//! A brain: a folder of markdown notes with a `.fbrain.toml`.

use crate::config::UserConfig;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

const CONFIG_FILE: &str = ".fbrain.toml";
const STANDARD_DIRS: [&str; 4] = ["me", "setup", "projects", "inbox"];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrainConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Embedding model id; see `config::model_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Cosine above which a proposal counts as a conflict; see `config::conflict_threshold`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_threshold: Option<f32>,
    /// Share of a proposal's distinct terms a note must match to count as a keyword-only conflict;
    /// see `config::conflict_overlap_min`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_overlap_min: Option<f32>,
    /// Token budget for packing search results; see `config::token_budget`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<usize>,
    /// Project to repo-path mapping used by `fbrain brief`; see `config::project_for`.
    #[serde(default, skip_serializing_if = "crate::config::ProjectMap::is_empty")]
    pub projects: crate::config::ProjectMap,
    #[serde(default, skip_serializing_if = "crate::config::LintConfig::is_empty")]
    pub lint: crate::config::LintConfig,
}

#[derive(Debug, Clone)]
pub struct NoteRef {
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub mtime_ns: i128,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub sha: String,
    /// Committer date, strict ISO 8601.
    pub date: String,
    pub subject: String,
}

#[derive(Debug)]
pub struct Brain {
    pub path: PathBuf,
    pub config: BrainConfig,
}

impl Brain {
    pub fn open(path: &Path) -> Result<Brain> {
        let file = path.join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("not a brain: {} has no {CONFIG_FILE}", path.display())
            }
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", file.display())),
        };
        let config =
            toml::from_str(&text).with_context(|| format!("invalid {}", file.display()))?;
        Ok(Brain {
            path: path.to_path_buf(),
            config,
        })
    }

    pub fn notes(&self) -> Result<Vec<NoteRef>> {
        let roots: Vec<String> = self
            .config
            .include
            .iter()
            .map(|r| r.trim_matches('/').to_string())
            .collect();
        let walker = ignore::WalkBuilder::new(&self.path)
            .hidden(true)
            .git_global(false)
            .git_exclude(false)
            .require_git(false)
            .build();
        let mut notes = Vec::new();
        for entry in walker {
            let entry = entry?;
            if !entry.file_type().is_some_and(|t| t.is_file())
                || entry.path().extension().is_none_or(|e| e != "md")
            {
                continue;
            }
            let rel = entry.path().strip_prefix(&self.path)?;
            let rel_path = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let hidden = rel_path.split('/').any(|s| s.starts_with('.'));
            let in_inbox = rel_path.starts_with("inbox/");
            let included =
                roots.is_empty() || roots.iter().any(|r| rel_path.starts_with(&format!("{r}/")));
            if hidden || in_inbox || !included {
                continue;
            }
            let meta = entry.metadata()?;
            let mtime_ns = match meta.modified()?.duration_since(UNIX_EPOCH) {
                Ok(d) => d.as_nanos() as i128,
                Err(e) => -(e.duration().as_nanos() as i128),
            };
            notes.push(NoteRef {
                rel_path,
                abs_path: entry.path().to_path_buf(),
                mtime_ns,
                size: meta.len(),
            });
        }
        notes.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        Ok(notes)
    }

    /// Runs `git` in the brain with paths taken literally; stdout on success, else the reason.
    fn git(&self, args: &[&str], paths: &[String]) -> Result<String> {
        let mut cmd = Command::new("git");
        cmd.arg("--literal-pathspecs")
            .args(args)
            .current_dir(&self.path);
        if !paths.is_empty() {
            cmd.arg("--").args(paths);
        }
        let out = cmd.output().context("cannot run git")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("git {} failed: {}", args[0], err.trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `Some(why)` only when the folder is not in a git work tree. Any other git failure (dubious
    /// ownership, a corrupt repository, git missing) is an error, so no writer starts a job it
    /// cannot commit.
    pub fn git_unavailable(&self) -> Result<Option<String>> {
        let out = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .env("LC_ALL", "C")
            .current_dir(&self.path)
            .output()
            .context("cannot run git")?;
        if out.status.success() {
            return Ok(None);
        }
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("not a git repository") {
            return Ok(Some(format!("not a git repository ({})", err.trim())));
        }
        bail!("git rev-parse failed: {}", err.trim())
    }

    /// Whether git tracks `rel`. A git failure is an error, not "untracked".
    pub fn git_tracked(&self, rel: &str) -> Result<bool> {
        Ok(!self.git(&["ls-files"], &[rel.into()])?.trim().is_empty())
    }

    /// Whether the work tree or index differs from HEAD for `rel` (an untracked file counts).
    pub fn git_dirty(&self, rel: &str) -> Result<bool> {
        Ok(!self
            .git(&["status", "--porcelain"], &[rel.into()])?
            .trim()
            .is_empty())
    }

    /// Puts a tracked file back in the index and the work tree as HEAD has it.
    pub fn git_restore(&self, rel: &str) -> Result<()> {
        self.git(&["checkout", "-q", "HEAD"], &[rel.into()])
            .map(drop)
    }

    pub fn git_add(&self, rels: &[String]) -> Result<()> {
        self.git(&["add"], rels).map(drop)
    }

    /// Deletes a tracked file and stages the deletion.
    pub fn git_rm(&self, rel: &str) -> Result<()> {
        self.git(&["rm", "-q"], &[rel.into()]).map(drop)
    }

    /// Commits only `rels`, so anything else already staged stays staged. Returns the new sha.
    pub fn git_commit(&self, subject: &str, rels: &[String]) -> Result<String> {
        self.git(&["commit", "-q", "--only", "-m", subject], rels)?;
        Ok(self.git(&["rev-parse", "HEAD"], &[])?.trim().to_string())
    }

    /// The latest commit that touched `rel`; `None` when it has none.
    pub fn git_last_commit(&self, rel: &str) -> Result<Option<CommitInfo>> {
        let out = match self.git(&["log", "-1", "--format=%H%x1f%cI%x1f%s"], &[rel.into()]) {
            Err(e) if e.to_string().contains("does not have any commits") => return Ok(None),
            r => r?,
        };
        let mut f = out.trim_end().splitn(3, '\u{1f}');
        Ok(match (f.next(), f.next(), f.next()) {
            (Some(sha), Some(date), Some(subject)) if !sha.is_empty() => Some(CommitInfo {
                sha: sha.into(),
                date: date.into(),
                subject: subject.into(),
            }),
            _ => None,
        })
    }

    /// Finds a note by rel path (with or without `.md`), else by case-insensitive file stem.
    /// Stem ties go to the shortest path, then lexicographic order.
    pub fn find(&self, query: &str) -> Result<Option<NoteRef>> {
        let notes = self.notes()?;
        let q = query.trim_start_matches("./");
        let q_md = if q.ends_with(".md") {
            q.to_string()
        } else {
            format!("{q}.md")
        };
        if let Some(n) = notes.iter().find(|n| n.rel_path == q_md) {
            return Ok(Some(n.clone()));
        }
        let want = crate::note::stem(&q_md).to_lowercase();
        Ok(notes
            .into_iter()
            .filter(|n| crate::note::stem(&n.rel_path).to_lowercase() == want)
            .min_by(|a, b| (a.rel_path.len(), &a.rel_path).cmp(&(b.rel_path.len(), &b.rel_path))))
    }
}

fn make_standard_dirs(path: &Path) -> Result<()> {
    for d in STANDARD_DIRS {
        std::fs::create_dir_all(path.join(d))?;
    }
    Ok(())
}

fn write_config(path: &Path, name: &str, include: &[String]) -> Result<()> {
    let cfg = BrainConfig {
        name: name.into(),
        include: include.to_vec(),
        model: None,
        conflict_threshold: None,
        conflict_overlap_min: None,
        token_budget: None,
        projects: Default::default(),
        lint: Default::default(),
    };
    std::fs::write(path.join(CONFIG_FILE), toml::to_string(&cfg)?)?;
    Ok(())
}

fn register(user: &mut UserConfig, path: &Path, name: &str, force: bool) -> Result<()> {
    let path = crate::config::plain_path(&path.canonicalize()?);
    user.register(name, Path::new(&path), force)?;
    user.save()
}

fn inside_git_repo(path: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn init(path: &Path, name: &str, force: bool) -> Result<()> {
    if path.join(CONFIG_FILE).exists() {
        bail!("brain already exists: {}", path.display());
    }
    // Fail on a name clash before touching the filesystem, so a retry is never blocked.
    let mut user = UserConfig::load()?;
    user.check_name_free(name, force)?;
    std::fs::create_dir_all(path)?;
    make_standard_dirs(path)?;
    let me = path.join("ME.md");
    if !me.exists() {
        std::fs::write(&me, "# Me\n\n<!-- fbrain:init-stub -->\n")?;
    }
    if !inside_git_repo(path) {
        let status = Command::new("git")
            .arg("init")
            .current_dir(path)
            .stdout(Stdio::null())
            .status()
            .context("cannot run git")?;
        if !status.success() {
            bail!("git init failed in {}", path.display());
        }
    }
    // The config file marks the brain as existing, so it is written after every step that can fail.
    write_config(path, name, &[])?;
    if let Err(e) = register(&mut user, path, name, force) {
        let _ = std::fs::remove_file(path.join(CONFIG_FILE));
        return Err(e);
    }
    Ok(())
}

pub fn adopt(path: &Path, name: &str, include: &[String], force: bool) -> Result<()> {
    if !path.is_dir() {
        bail!("not a folder: {}", path.display());
    }
    let mut user = UserConfig::load()?;
    user.check_name_free(name, force)?;
    if !path.join(CONFIG_FILE).exists() {
        write_config(path, name, include)?;
    }
    make_standard_dirs(path)?;
    register(&mut user, path, name, force)
}
