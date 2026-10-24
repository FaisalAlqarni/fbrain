//! `fbrain doctor`: model, index and resident process checks.

use crate::brain::Brain;
use crate::config;
use crate::index::embed::{self, Embedder, ModelError};
use crate::index::Index;
use crate::search::{Filter, Query};
use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
}

impl Check {
    fn new(name: &str, level: Level, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            level,
            detail: detail.into(),
        }
    }

    pub fn line(&self) -> String {
        let tag = match self.level {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
        };
        format!("{tag}: {}: {}", self.name, self.detail)
    }
}

/// Checks the model and the index of the active brain (`brain` is the `--brain` value).
/// With `fix`, downloads a missing or unloadable model and re-embeds the index.
pub fn run(fix: bool, brain: Option<&str>) -> Result<Vec<Check>> {
    let cache = config::cache_dir()?;
    let cwd = std::env::current_dir()?;
    let opened = match config::resolve_brain(brain, &cwd) {
        Ok(r) => Some(Brain::open(&r.path)),
        Err(e) if e.to_string().starts_with(config::NO_BRAIN) => None,
        Err(e) => Some(Err(e)),
    };
    let active = opened.as_ref().and_then(|o| o.as_ref().ok());
    let id = config::model_id(active);

    let installs = crate::harness::records()?;
    let mut checks = binary_checks(&installs);
    checks.push(match &opened {
        None => Check::new("brain", Level::Warn, "no active brain"),
        Some(Err(e)) => Check::new("brain", Level::Fail, format!("{e:#}")),
        Some(Ok(b)) => Check::new("brain", Level::Ok, b.path.display().to_string()),
    });
    let loaded = embed::ensure_model(&id, &cache, fix)
        .map_err(|e| model_problem(&e, &id, fix))
        .and_then(|dir| {
            let spec = embed::spec(&id).map_err(|e| format!("{e:#}"))?;
            Embedder::load(&dir, spec)
                .map(|e| (dir, e))
                .map_err(|e| load_problem(&e))
        });
    let embedder = match loaded {
        Ok((dir, e)) => {
            checks.push(Check::new(
                "model",
                Level::Ok,
                format!("{id} ({})", dir.display()),
            ));
            Some(e)
        }
        Err(msg) => {
            checks.push(Check::new("model", Level::Fail, msg));
            None
        }
    };

    checks.push(match &opened {
        None => Check::new("index", Level::Warn, "no active brain"),
        Some(Err(e)) => Check::new("index", Level::Fail, format!("{e:#}")),
        Some(Ok(b)) => index_check(b, &cache, &id, embedder.as_ref()),
    });
    checks.push(serve_check(&id));
    checks.push(search_check(active, &id));
    for rec in &installs {
        checks.extend(harness_checks(rec, fix));
    }
    Ok(checks)
}

/// The binary each install recorded (or this one when nothing is installed) exists and runs.
fn binary_checks(installs: &[crate::harness::InstallRecord]) -> Vec<Check> {
    let mut bins: Vec<std::path::PathBuf> = installs.iter().map(|r| r.binary.clone()).collect();
    if bins.is_empty() {
        bins.extend(std::env::current_exe());
    }
    bins.sort();
    bins.dedup();
    bins.iter()
        .map(|b| {
            if !b.exists() {
                return Check::new("binary", Level::Fail, format!("{} not found", b.display()));
            }
            match std::process::Command::new(b).arg("--version").output() {
                Ok(o) if o.status.success() => Check::new(
                    "binary",
                    Level::Ok,
                    format!(
                        "{} ({})",
                        b.display(),
                        String::from_utf8_lossy(&o.stdout).trim()
                    ),
                ),
                Ok(o) => Check::new(
                    "binary",
                    Level::Fail,
                    format!("{} --version exited with {}", b.display(), o.status),
                ),
                Err(e) => Check::new(
                    "binary",
                    Level::Fail,
                    format!("{} does not run: {e}", b.display()),
                ),
            }
        })
        .collect()
}

/// One real search, timed. It runs only against a resident process that is already up: doctor
/// never starts one.
fn search_check(brain: Option<&Brain>, id: &str) -> Check {
    let Some(brain) = brain else {
        return Check::new("search", Level::Warn, "skipped, no active brain");
    };
    let mut client = match crate::serve::connect_if_running(id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Check::new(
                "search",
                Level::Ok,
                "skipped, no resident process is running",
            )
        }
        Err(e) => return Check::new("search", Level::Warn, format!("skipped, {e:#}")),
    };
    let path = brain
        .path
        .canonicalize()
        .unwrap_or_else(|_| brain.path.clone());
    let q = Query::new("fbrain", Filter::default());
    let start = std::time::Instant::now();
    match client.search(&path, &q) {
        Ok(_) => Check::new(
            "search",
            Level::Ok,
            format!("{} ms", start.elapsed().as_millis()),
        ),
        Err(e) => Check::new("search", Level::Fail, format!("{e:#}")),
    }
}

/// `harness:<name>`: every recorded entry is still in its file. With `fix`, a broken install is
/// written again. Codex also always gets the trust reminder.
fn harness_checks(rec: &crate::harness::InstallRecord, fix: bool) -> Vec<Check> {
    let name = format!("harness:{}", rec.harness.as_str());
    let mut missing = crate::harness::missing(rec);
    let mut note = String::new();
    let mut fix_note_off = false;
    if fix && !missing.is_empty() {
        // Only the binary recorded at install is written into a hook; never this process's.
        if !rec.binary.exists() {
            missing.push(format!("run fbrain install {}", rec.harness.as_str()));
            fix_note_off = true;
        } else {
            let opts = crate::harness::InstallOpts {
                agents_md: rec
                    .files
                    .iter()
                    .any(|e| matches!(e.kind, crate::harness::EntryKind::MarkedBlock(_))),
                mcp: crate::harness::mcp_entry(rec).is_some(),
            };
            match crate::harness::install_with(rec.harness, &opts, &rec.binary) {
                Ok(fresh) => {
                    let mut files: Vec<String> = fresh
                        .files
                        .iter()
                        .map(|e| e.path.display().to_string())
                        .collect();
                    files.dedup();
                    note = format!(
                        " (restored: {}; changed {})",
                        missing.join("; "),
                        files.join(", ")
                    );
                    missing = crate::harness::missing(&fresh);
                }
                Err(e) => missing.push(format!("{e:#}")),
            }
        }
    }
    let mut out = vec![if missing.is_empty() {
        Check::new(
            &name,
            Level::Ok,
            format!("{} entries present{note}", rec.files.len()),
        )
    } else {
        Check::new(
            &name,
            Level::Fail,
            format!(
                "{}{}",
                missing.join("; "),
                if fix || fix_note_off { "" } else { FIX_HINT }
            ),
        )
    }];
    if rec.harness == crate::harness::Harness::Codex {
        out.push(Check::new(
            &name,
            Level::Warn,
            "run /hooks in Codex and trust the fbrain hook after any install or change",
        ));
    }
    let want = match rec.harness {
        crate::harness::Harness::OpencodeV1 => Some(1),
        crate::harness::Harness::OpencodeV2 => Some(2),
        _ => None,
    };
    if let (Some(want), Some((got, text))) = (want, crate::harness::opencode::reported_major()) {
        if got != want {
            out.push(Check::new(
                &name,
                Level::Warn,
                format!(
                    "opencode reports version {text}, but {} is installed; run fbrain install opencode-v{got}",
                    rec.harness.as_str()
                ),
            ));
        }
    }
    out
}

/// The resident process as it reports itself; doctor never starts one.
fn serve_check(id: &str) -> Check {
    match crate::serve::status_if_running(id) {
        Ok(Some(s)) => Check::new(
            "serve",
            Level::Ok,
            format!(
                "running pid {}, version {}, model {}, rss {} MB, {} brains, up {} s",
                s.pid,
                s.version,
                s.model_id,
                s.rss_mb,
                s.brains.len(),
                s.uptime_s
            ),
        ),
        Ok(None) => {
            let mut detail = String::from("not running (the next search starts it)");
            if let Ok(Some((reason, log))) = crate::serve::last_exit() {
                detail += &format!("; last exit: {reason}; log {}", log.display());
            }
            Check::new("serve", Level::Ok, detail)
        }
        Err(e) => Check::new("serve", Level::Warn, format!("{e:#}")),
    }
}

const FIX_HINT: &str = "; run fbrain doctor --fix";

/// A model that is missing or whose files differ from the pinned sums; `--fix` repairs these.
fn model_problem(e: &anyhow::Error, id: &str, fix: bool) -> String {
    let hint = if fix { "" } else { FIX_HINT };
    match e.downcast_ref::<ModelError>() {
        Some(ModelError::NotDownloaded(_)) => format!("not downloaded: {id}{hint}"),
        Some(m) => format!("{m}{hint}"),
        None => format!("{e:#}"),
    }
}

/// The files match their sums but the runtime could not use them, so downloading again changes nothing.
fn load_problem(e: &anyhow::Error) -> String {
    format!("{e:#}; checksums match, so fbrain doctor --fix will not help (likely the ONNX Runtime library or its start-up, not the model files)")
}

fn index_check(
    brain: &Brain,
    cache: &std::path::Path,
    id: &str,
    embedder: Option<&Embedder>,
) -> Check {
    match Index::load_or_build(brain, cache, false, embedder.map(|e| (id, e))) {
        Ok(ix) => {
            if let Err(e) = ix.check_vectors() {
                return Check::new("index", Level::Fail, format!("{e:#}"));
            }
            let n = ix.stats.changed + ix.stats.unchanged;
            if embedder.is_some() {
                Check::new("index", Level::Ok, format!("{n} notes, embeddings on"))
            } else {
                Check::new("index", Level::Warn, format!("{n} notes, keywords only"))
            }
        }
        Err(e) => Check::new("index", Level::Fail, format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_load_failure_with_correct_checksums_says_fix_will_not_help_and_chains_the_cause() {
        let e = anyhow::anyhow!("libonnxruntime refused to start").context("cannot load model x");
        let line = load_problem(&e);
        assert!(
            line.contains("cannot load model x: libonnxruntime refused to start"),
            "{line}"
        );
        assert!(
            line.contains("checksums match, so fbrain doctor --fix will not help"),
            "{line}"
        );
    }
}
