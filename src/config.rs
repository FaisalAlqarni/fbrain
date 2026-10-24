//! User-level config: named brains, the default brain and repo-to-brain mapping.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct BrainRef {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct UserConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Minutes without a request before the resident process exits (default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serve_idle_minutes: Option<u64>,
    #[serde(default, rename = "brain")]
    pub brains: Vec<BrainEntry>,
    #[serde(default, rename = "map", skip_serializing_if = "Vec::is_empty")]
    pub maps: Vec<MapEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BrainEntry {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MapEntry {
    pub repo_prefix: PathBuf,
    pub brain: String,
}

pub fn user_config_dir() -> Result<PathBuf> {
    pick_dir(
        env_dir("FBRAIN_CONFIG_DIR"),
        dirs::config_dir(),
        "config",
        "FBRAIN_CONFIG_DIR",
    )
}

/// Embedding model id: env `FBRAIN_MODEL`, else the brain's `.fbrain.toml` `model`, else the default.
pub fn model_id(brain: Option<&crate::brain::Brain>) -> String {
    env_nonempty("FBRAIN_MODEL")
        .or_else(|| brain.and_then(|b| b.config.model.clone()))
        .unwrap_or_else(|| crate::index::embed::DEFAULT_MODEL.to_string())
}

/// Cosine similarity above which a proposal counts as a conflict with a note.
/// The default comes from the gate in ADR 0004.
pub fn conflict_threshold(brain: Option<&crate::brain::Brain>) -> f32 {
    brain
        .and_then(|b| b.config.conflict_threshold)
        .unwrap_or(DEFAULT_CONFLICT_THRESHOLD)
}

/// Share of a proposal's distinct terms a note must match to count as a conflict when no
/// embeddings are available.
pub fn conflict_overlap_min(brain: Option<&crate::brain::Brain>) -> f32 {
    brain
        .and_then(|b| b.config.conflict_overlap_min)
        .unwrap_or(0.6)
}

/// `[lint]` in `.fbrain.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LintConfig {
    /// Issues `lint --strict` accepts.
    #[serde(default)]
    pub allow: Vec<LintAllow>,
}

impl LintConfig {
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintAllow {
    pub rule: String,
    pub path: String,
}

/// Whether the brain's `[lint] allow` list covers this rule on this note.
pub fn lint_allows(brain: &crate::brain::Brain, rule: &str, rel_path: &str) -> bool {
    brain
        .config
        .lint
        .allow
        .iter()
        .any(|a| a.rule == rule && a.path == rel_path)
}

/// `[projects.<name>] repos = [...]` in `.fbrain.toml`, in file order.
#[derive(Debug, Clone, Default)]
pub struct ProjectMap(pub Vec<(String, Vec<String>)>);

#[derive(Serialize, Deserialize)]
struct ProjectEntry {
    #[serde(default)]
    repos: Vec<String>,
}

impl ProjectMap {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'de> Deserialize<'de> for ProjectMap {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = ProjectMap;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a table of projects")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut m: A,
            ) -> Result<ProjectMap, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = m.next_entry::<String, ProjectEntry>()? {
                    out.push((k, v.repos));
                }
                Ok(ProjectMap(out))
            }
        }
        d.deserialize_map(V)
    }
}

impl Serialize for ProjectMap {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, repos) in &self.0 {
            m.serialize_entry(
                k,
                &ProjectEntry {
                    repos: repos.clone(),
                },
            )?;
        }
        m.end()
    }
}

/// `/`-separated, with the Windows verbatim prefix dropped and case folded on Windows.
fn norm(p: &str) -> String {
    let s = p.replace('\\', "/");
    let s = s.strip_prefix("//?/").unwrap_or(&s).to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// `~` expanded and the existing part canonicalised; `dir` is the directory half of a glob.
fn canon(p: &str) -> String {
    let expanded = match (p.strip_prefix('~'), dirs::home_dir()) {
        (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            format!("{}{rest}", h.display())
        }
        _ => p.to_string(),
    };
    let path = PathBuf::from(&expanded);
    norm(&path.canonicalize().unwrap_or(path).to_string_lossy())
}

/// The first project, in file order, with a repo entry that is a path prefix of `cwd`.
/// A trailing `*` matches any suffix of the entry's last segment.
pub fn project_for(brain: &crate::brain::Brain, cwd: &Path) -> Option<String> {
    let cwd = norm(
        &cwd.canonicalize()
            .unwrap_or_else(|_| cwd.to_path_buf())
            .to_string_lossy(),
    );
    brain.config.projects.0.iter().find_map(|(name, repos)| {
        repos.iter().find_map(|repo| {
            let hit = match repo.strip_suffix('*') {
                Some(stem) => {
                    let (dir, last) = stem.rsplit_once(['/', '\\']).unwrap_or(("", stem));
                    let stem = format!("{}/{}", canon(dir).trim_end_matches('/'), norm(last));
                    cwd.starts_with(&stem)
                }
                None => {
                    let r = canon(repo);
                    let r = r.trim_end_matches('/');
                    cwd == r || cwd.starts_with(&format!("{r}/"))
                }
            };
            hit.then(|| name.clone())
        })
    })
}

pub const DEFAULT_SERVE_IDLE_MINUTES: u64 = 30;

/// Idle timeout of the resident process: env `FBRAIN_SERVE_IDLE_SECS` (tests), else the user
/// config's `serve_idle_minutes`, else 30 minutes.
pub fn serve_idle() -> Result<std::time::Duration> {
    if let Some(s) = env_nonempty("FBRAIN_SERVE_IDLE_SECS") {
        let secs: u64 = s
            .parse()
            .with_context(|| format!("FBRAIN_SERVE_IDLE_SECS is not a number: {s}"))?;
        return Ok(std::time::Duration::from_secs(secs));
    }
    let mins = UserConfig::load()?
        .serve_idle_minutes
        .unwrap_or(DEFAULT_SERVE_IDLE_MINUTES);
    Ok(std::time::Duration::from_secs(mins.saturating_mul(60)))
}

pub const DEFAULT_CONFLICT_THRESHOLD: f32 = 0.50;

/// Search token budget: the brain's `.fbrain.toml` `token_budget`, else 4000.
pub fn token_budget(brain: Option<&crate::brain::Brain>) -> usize {
    brain
        .and_then(|b| b.config.token_budget)
        .unwrap_or(crate::search::DEFAULT_BUDGET)
}

fn env_nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.is_empty())
}

pub fn cache_dir() -> Result<PathBuf> {
    pick_dir(
        env_dir("FBRAIN_CACHE_DIR"),
        dirs::cache_dir(),
        "cache",
        "FBRAIN_CACHE_DIR",
    )
}

/// The env override wins; with no override and no OS dir, fail instead of writing into the cwd.
fn pick_dir(env: Option<PathBuf>, os: Option<PathBuf>, what: &str, var: &str) -> Result<PathBuf> {
    match (env, os) {
        (Some(d), _) => Ok(d),
        (None, Some(d)) => Ok(d.join("fbrain")),
        (None, None) => bail!("cannot find the {what} directory; set {var}"),
    }
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn config_file() -> Result<PathBuf> {
    Ok(user_config_dir()?.join("config.toml"))
}

impl UserConfig {
    pub fn load() -> Result<Self> {
        let file = config_file()?;
        match std::fs::read_to_string(&file) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("invalid user config {}", file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", file.display())),
        }
    }

    /// Writes through a temp file and a rename so a crash never leaves a half-written config.
    pub fn save(&self) -> Result<()> {
        let file = config_file()?;
        std::fs::create_dir_all(user_config_dir()?)?;
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &file).with_context(|| format!("cannot write {}", file.display()))
    }

    /// Fails when `name` is taken and `force` is not set. Call before touching the filesystem.
    pub fn check_name_free(&self, name: &str, force: bool) -> Result<()> {
        if !force && self.brains.iter().any(|b| b.name == name) {
            bail!("brain name already registered: {name} (use --force to replace its path)");
        }
        Ok(())
    }

    /// Adds a brain; the first one becomes the default. An existing name needs `force`.
    pub fn register(&mut self, name: &str, path: &Path, force: bool) -> Result<()> {
        match self.brains.iter_mut().find(|b| b.name == name) {
            Some(_) if !force => {
                bail!("brain name already registered: {name} (use --force to replace its path)")
            }
            Some(b) => b.path = path.to_path_buf(),
            None => self.brains.push(BrainEntry {
                name: name.into(),
                path: path.to_path_buf(),
            }),
        }
        if self.default.is_none() {
            self.default = Some(name.into());
        }
        Ok(())
    }

    /// A registered name, else the string taken as a path.
    fn lookup(&self, name_or_path: &str) -> BrainRef {
        match self.brains.iter().find(|b| b.name == name_or_path) {
            Some(b) => BrainRef {
                name: b.name.clone(),
                path: b.path.clone(),
            },
            None => {
                let path = PathBuf::from(name_or_path);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                BrainRef { name, path }
            }
        }
    }
}

pub const NO_BRAIN: &str = "no brain configured";

/// Precedence: explicit, env `FBRAIN_BRAIN`, first matching `[[map]]` for `cwd`, then `default`.
pub fn resolve_brain(explicit: Option<&str>, cwd: &Path) -> Result<BrainRef> {
    let cfg = UserConfig::load()?;
    if let Some(e) = explicit {
        return Ok(cfg.lookup(e));
    }
    if let Some(e) = std::env::var("FBRAIN_BRAIN").ok().filter(|v| !v.is_empty()) {
        return Ok(cfg.lookup(&e));
    }
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    for m in &cfg.maps {
        let prefix = m
            .repo_prefix
            .canonicalize()
            .unwrap_or_else(|_| m.repo_prefix.clone());
        if cwd.starts_with(&prefix) {
            return Ok(cfg.lookup(&m.brain));
        }
    }
    match &cfg.default {
        Some(d) => Ok(cfg.lookup(d)),
        None => bail!("{NO_BRAIN}: run `fbrain init <path>` or pass --brain"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_env_and_os_dir_is_an_error() {
        let err = pick_dir(None, None, "config", "FBRAIN_CONFIG_DIR").unwrap_err();
        assert!(err.to_string().contains("FBRAIN_CONFIG_DIR"), "{err}");
    }

    #[test]
    fn serve_idle_minutes_is_read_from_the_user_config_and_optional() {
        let c: UserConfig = toml::from_str("serve_idle_minutes = 5\n").unwrap();
        assert_eq!(c.serve_idle_minutes, Some(5));
        let c: UserConfig = toml::from_str("").unwrap();
        assert_eq!(c.serve_idle_minutes, None);
        assert!(!toml::to_string(&c).unwrap().contains("serve_idle"));
    }

    #[test]
    fn env_dir_wins_over_os_dir() {
        let got = pick_dir(Some("/env".into()), Some("/os".into()), "config", "X").unwrap();
        assert_eq!(got, PathBuf::from("/env"));
    }
}
