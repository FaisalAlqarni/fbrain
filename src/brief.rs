//! The session payload an agent starts with, and the per-harness output format.
//! Reads files only: it never loads the model or contacts the resident process.

use crate::brain::Brain;
use crate::config;
use crate::harness::Harness;
use crate::note::Note;
use crate::proposals;
use crate::search::{expose, Surface};
use anyhow::Result;
use serde_json::json;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Cap on the payload, in characters. The global limit is 9,000.
pub const MAX_CHARS: usize = 8_800;
const RULES: &str = include_str!("harness/assets/rules.md");

/// The payload text; a failure becomes the visible "not loaded" notice.
pub fn render(brain: &Brain, _harness: Harness, cwd: &Path) -> String {
    try_render(brain, cwd).unwrap_or_else(|e| not_loaded(&e.to_string()))
}

pub fn not_loaded(reason: &str) -> String {
    format!("fbrain memory not loaded: {reason}. Run fbrain doctor.")
}

/// Hook JSON for `claude-code` and `codex`, the plain payload for the opencode plugins.
pub fn wrap(harness: Harness, payload: &str, hook_event: Option<&str>) -> String {
    match harness {
        Harness::ClaudeCode | Harness::Codex => json!({"hookSpecificOutput": {
            "hookEventName": hook_event.unwrap_or("SessionStart"),
            "additionalContext": payload,
        }})
        .to_string(),
        Harness::OpencodeV1 | Harness::OpencodeV2 => payload.to_string(),
    }
}

/// The whole command: never fails, so a broken brain never breaks a session.
pub fn run(brain_arg: Option<&str>, harness: &str, cwd_arg: Option<PathBuf>) -> String {
    let Some(h) = Harness::parse(harness) else {
        return not_loaded(&format!("unknown harness {harness:?}"));
    };
    if h == Harness::ClaudeCode {
        if let Err(e) = export_marker() {
            eprintln!(
                "fbrain: cannot mark this session as an agent session (CLAUDE_ENV_FILE): {e}"
            );
        }
    }
    let hook = read_hook_input();
    let event = hook
        .as_ref()
        .and_then(|v| v["hook_event_name"].as_str().map(str::to_string));
    let cwd = cwd_arg
        .or_else(|| {
            hook.as_ref()
                .and_then(|v| v["cwd"].as_str().map(PathBuf::from))
        })
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let payload = config::resolve_brain(brain_arg, &cwd)
        .and_then(|r| Brain::open(&r.path))
        .map(|b| render(&b, h, &cwd))
        .unwrap_or_else(|e| not_loaded(&e.to_string()));
    wrap(h, &payload, event.as_deref())
}

/// Claude Code runs the SessionStart hook with `CLAUDE_ENV_FILE`; the file's `export` lines run
/// before every later Bash command of the session, so `fbrain` there knows an agent runs it.
/// <https://code.claude.com/docs/en/hooks> (SessionStart, "Persist environment variables")
fn export_marker() -> std::io::Result<()> {
    let Some(file) = std::env::var_os("CLAUDE_ENV_FILE").filter(|f| !f.is_empty()) else {
        return Ok(());
    };
    let line = "export FBRAIN_HARNESS=claude-code";
    // Never write through a link: the file is named by an environment variable.
    if std::fs::symlink_metadata(&file).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::other("the env file is a symbolic link"));
    }
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if text.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    let lead = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut open = std::fs::OpenOptions::new();
    open.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut open, libc::O_NOFOLLOW);
    let mut f = open.open(file)?;
    std::io::Write::write_all(&mut f, format!("{lead}{line}\n").as_bytes())
}

/// The hook's stdin JSON. Read only when stdin is not a terminal, for at most 200 ms; anything
/// that does not arrive in time or does not parse is ignored.
fn read_hook_input() -> Option<serde_json::Value> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    // The reader may stay blocked on a pipe that never closes; process exit ends it.
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdin.lock().read_to_string(&mut s);
        let _ = tx.send(s);
    });
    let text = rx.recv_timeout(Duration::from_millis(200)).ok()?;
    serde_json::from_str(&text).ok()
}

fn try_render(brain: &Brain, cwd: &Path) -> Result<String> {
    let name = if brain.config.name.is_empty() {
        brain
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        brain.config.name.clone()
    };
    let me = read_body(brain, "ME.md");
    let (project_head, mut brief, brief_path) = match config::project_for(brain, cwd) {
        Some(p) => {
            let rel = format!("projects/{p}/BRIEF.md");
            let body = read_body(brain, &rel).unwrap_or_else(|| "No BRIEF.md yet.".into());
            (format!("### Project: {p}"), Some(body), rel)
        }
        None => (String::new(), None, String::new()),
    };
    let list = if brief.is_none() {
        project_list(brain)
    } else {
        String::new()
    };
    let (pending, skipped) = proposals::list_with_skipped(brain)?;
    let n = pending.len() + skipped.len();
    let mut me = me;

    let assemble = |me: &Option<String>, brief: &Option<String>| {
        let mut parts = vec![format!("## fbrain memory (brain: {name})")];
        parts.extend(me.clone());
        if let Some(b) = brief {
            parts.push(format!("{project_head}\n\n{b}"));
        } else {
            parts.push(list.clone());
        }
        parts.push(RULES.trim().to_string());
        if n > 0 {
            parts.push(format!("Pending proposals: {n} (run fbrain review)"));
        }
        parts.join("\n\n")
    };
    let over = |s: &str| s.chars().count().saturating_sub(MAX_CHARS);
    // The brief is cut first, then ME.md, each to the room the rest leaves.
    let excess = over(&assemble(&me, &brief));
    if excess > 0 {
        if let Some(b) = brief.as_mut() {
            cut(b, excess, &brief_path);
        }
    }
    let excess = over(&assemble(&me, &brief));
    if excess > 0 {
        if let Some(m) = me.as_mut() {
            cut(m, excess, "ME.md");
        }
    }
    Ok(assemble(&me, &brief))
}

/// Shortens `body` by at least `excess` characters and appends the marker, so the result is
/// no longer than `body` was.
fn cut(body: &mut String, excess: usize, rel_path: &str) {
    let marker = format!("\n\n[truncated: run fbrain get {rel_path}]");
    let keep = body
        .chars()
        .count()
        .saturating_sub(excess + marker.chars().count());
    *body = body.chars().take(keep).collect::<String>() + &marker;
}

/// The brief reaches agents like MCP does, so a private note is replaced by a bare notice.
fn withheld() -> String {
    "(private note withheld)".into()
}

fn read_body(brain: &Brain, rel: &str) -> Option<String> {
    let text = std::fs::read_to_string(brain.path.join(rel)).ok()?;
    let note = Note::parse(rel, &text);
    if !expose(&note, Surface::Mcp) {
        return Some(withheld());
    }
    Some(note.body.trim().to_string())
}

/// `### Projects` with one line per folder under `projects/`.
fn project_list(brain: &Brain) -> String {
    let mut names: Vec<String> = std::fs::read_dir(brain.path.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    let mut out = String::from("### Projects");
    for n in names {
        let rel = format!("projects/{n}/BRIEF.md");
        let desc = std::fs::read_to_string(brain.path.join(&rel))
            .ok()
            .map(|t| Note::parse(&rel, &t))
            .and_then(|note| {
                if !expose(&note, Surface::Mcp) {
                    return Some(withheld());
                }
                note.fm.description.clone().or_else(|| {
                    note.body
                        .lines()
                        .map(|l| l.trim().trim_start_matches('#').trim())
                        .find(|l| !l.is_empty())
                        .map(str::to_string)
                })
            })
            .unwrap_or_else(|| "no BRIEF.md".into());
        out.push_str(&format!("\n- {n}: {desc}"));
    }
    out
}
