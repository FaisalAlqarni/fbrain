//! The agent harnesses fbrain supports.

pub mod claude_code;
pub mod codex;
pub mod opencode;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    ClaudeCode,
    Codex,
    OpencodeV1,
    OpencodeV2,
}

impl Harness {
    pub fn parse(s: &str) -> Option<Harness> {
        match s {
            "claude-code" => Some(Harness::ClaudeCode),
            "codex" => Some(Harness::Codex),
            "opencode-v1" => Some(Harness::OpencodeV1),
            "opencode-v2" => Some(Harness::OpencodeV2),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Codex => "codex",
            Harness::OpencodeV1 => "opencode-v1",
            Harness::OpencodeV2 => "opencode-v2",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// Points at the hook array (`/hooks/SessionStart`); the entry inside is found by its command.
    JsonPointer(String),
    TomlKey(String),
    File,
    MarkedBlock(String),
    /// An MCP server registered through the `claude` CLI; fbrain writes no file for it.
    McpCli,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrittenEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
    /// The hook command written, which identifies the entry inside the array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecord {
    pub harness: Harness,
    /// The fbrain executable the hooks call.
    pub binary: PathBuf,
    pub files: Vec<WrittenEntry>,
}

/// `mcp` also registers `fbrain mcp` with the harness. `agents_md` adds the marked block to
/// opencode's `AGENTS.md`. An install without `mcp` takes out the registration an earlier one made.
#[derive(Debug, Clone, Default)]
pub struct InstallOpts {
    pub mcp: bool,
    pub agents_md: bool,
}

const INSTALLS: &str = "installs.json";

pub fn records() -> Result<Vec<InstallRecord>> {
    let file = crate::config::user_config_dir()?.join(INSTALLS);
    match std::fs::read_to_string(&file) {
        Ok(t) => {
            serde_json::from_str(&t).with_context(|| format!("cannot parse {}", file.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", file.display())),
    }
}

fn save_records(all: &[InstallRecord]) -> Result<()> {
    let dir = crate::config::user_config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_string_pretty(all)? + "\n";
    crate::review::write_atomic(&dir, INSTALLS, text.as_bytes())
}

/// Writes the hooks for `h`, replacing any earlier install of it, and records what it wrote.
pub fn install(h: Harness, opts: &InstallOpts) -> Result<InstallRecord> {
    let binary = std::env::current_exe().context("cannot find the running fbrain executable")?;
    install_with(h, opts, &binary)
}

/// `install` for an explicit binary path (`doctor --fix` passes the recorded one).
pub fn install_with(h: Harness, opts: &InstallOpts, binary: &Path) -> Result<InstallRecord> {
    let mut all = records()?;
    let old = all.iter().find(|r| r.harness == h).cloned();
    if matches!(h, Harness::OpencodeV1 | Harness::OpencodeV2) {
        // Both majors share one plugin path, so installing one replaces the other's record.
        let other = if h == Harness::OpencodeV1 {
            Harness::OpencodeV2
        } else {
            Harness::OpencodeV1
        };
        let replaced = all.iter().find(|r| r.harness == other).cloned();
        let rec = opencode::install(h, binary, opts, old.as_ref().or(replaced.as_ref()))?;
        all.retain(|r| r.harness != h && r.harness != other);
        all.push(rec.clone());
        save_records(&all)?;
        return Ok(rec);
    }
    let target = match h {
        Harness::ClaudeCode => claude_code::settings_path()?,
        Harness::Codex => codex::hooks_path()?,
        _ => unreachable!("opencode returned above"),
    };
    let old_mcp = old.as_ref().and_then(|r| mcp_entry(r).cloned());
    // Everything that can refuse happens before the first write: a missing CLI, a Codex config
    // that does not parse, is a symlink or holds a table of that name that is not ours.
    let claude_cli = if h == Harness::ClaudeCode && opts.mcp {
        Some(claude_code::require_cli(binary)?)
    } else {
        None
    };
    if h == Harness::Codex {
        codex::check_env()?;
        if opts.mcp {
            codex::check_mcp(binary, old_mcp.is_some())?;
        }
    }
    // The registration is kept when it is the same server of the same binary and still there.
    let keep = match (h, &old_mcp) {
        (Harness::ClaudeCode, Some(_)) if opts.mcp => {
            old.as_ref().is_some_and(|r| r.binary == binary) && claude_code::mcp_present()
        }
        _ => false,
    };
    // Any other old registration goes first, so a failure here changes nothing.
    if let Some(e) = old_mcp
        .as_ref()
        .filter(|_| !keep && h == Harness::ClaudeCode)
    {
        remove_mcp(h, e)?;
    } else if let (Some(e), false) = (&old_mcp, opts.mcp) {
        remove_mcp(h, e)?;
    }
    // A config dir that moved since the last install: take the old hooks out of the old files.
    if let Some(old) = &old {
        for path in distinct_paths(old)
            .into_iter()
            .filter(|p| *p != target && p.exists())
        {
            let mine: Vec<WrittenEntry> = old
                .files
                .iter()
                .filter(|e| e.path == path)
                .cloned()
                .collect();
            edit_hooks(&path, &mine, &[])?;
        }
        for e in old.files.iter().filter(|e| {
            is_env(e) && e.path != codex::config_path().unwrap_or_default() && e.path.exists()
        }) {
            codex::remove_env(e)?;
        }
    }
    let mut rec = match h {
        Harness::ClaudeCode => claude_code::install(binary, old.as_ref())?,
        _ => codex::install(binary, old.as_ref())?,
    };
    if keep {
        rec.files.extend(old_mcp.clone());
    }
    if h == Harness::Codex {
        let wrote_before = old
            .as_ref()
            .is_some_and(|r| r.files.iter().any(|e| is_env(e) && e.command.is_some()));
        rec.files.push(codex::add_env(wrote_before)?);
    }
    // Recorded now, so a failing MCP step leaves hooks that uninstall can find.
    all.retain(|r| r.harness != h);
    all.push(rec.clone());
    save_records(&all)?;
    if !opts.mcp || keep {
        return Ok(rec);
    }
    let added = match h {
        Harness::ClaudeCode => {
            claude_code::add_mcp(claude_cli.as_deref().expect("found above"), binary)
        }
        _ => codex::add_mcp(binary, old_mcp.is_some()),
    };
    let entry = added.with_context(|| {
        format!(
            "the {} hooks are installed and recorded (fbrain uninstall {} removes them), but the MCP server was not registered",
            h.as_str(),
            h.as_str()
        )
    })?;
    rec.files.push(entry);
    all.retain(|r| r.harness != h);
    all.push(rec.clone());
    save_records(&all)?;
    Ok(rec)
}

/// Removes exactly the recorded entries of `h`, then its record.
pub fn uninstall(h: Harness) -> Result<()> {
    let mut all = records()?;
    let Some(rec) = all.iter().find(|r| r.harness == h).cloned() else {
        bail!("{} is not installed", h.as_str());
    };
    if matches!(h, Harness::OpencodeV1 | Harness::OpencodeV2) {
        opencode::remove(&rec.files)?;
    } else {
        // First, so a missing `claude` CLI stops the uninstall with the record still in place.
        if let Some(e) = mcp_entry(&rec) {
            remove_mcp(h, e)?;
        }
        for e in rec.files.iter().filter(|e| is_env(e)) {
            codex::remove_env(e)?;
        }
        for path in distinct_paths(&rec) {
            let mine: Vec<WrittenEntry> = rec
                .files
                .iter()
                .filter(|e| e.path == path)
                .cloned()
                .collect();
            edit_hooks(&path, &mine, &[])?;
        }
    }
    all.retain(|r| r.harness != h);
    save_records(&all)
}

/// The recorded MCP registration, if the install made one.
pub(crate) fn mcp_entry(rec: &InstallRecord) -> Option<&WrittenEntry> {
    rec.files.iter().find(|e| is_mcp(e))
}

pub(crate) fn is_mcp(e: &WrittenEntry) -> bool {
    match &e.kind {
        EntryKind::McpCli => true,
        EntryKind::TomlKey(k) => k == codex::MCP_KEY,
        EntryKind::JsonPointer(p) => p == opencode::MCP_POINTER,
        _ => false,
    }
}

fn remove_mcp(h: Harness, e: &WrittenEntry) -> Result<()> {
    match h {
        Harness::ClaudeCode => claude_code::remove_mcp(),
        Harness::Codex => codex::remove_mcp(e),
        _ => opencode::remove(std::slice::from_ref(e)),
    }
}

/// The recorded Codex shell marker in `config.toml`.
fn is_env(e: &WrittenEntry) -> bool {
    matches!(&e.kind, EntryKind::TomlKey(k) if k == codex::ENV_KEY)
}

/// Files that hold hooks; MCP registrations and the Codex marker are not among them.
fn distinct_paths(rec: &InstallRecord) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    for e in rec.files.iter().filter(|e| !is_mcp(e) && !is_env(e)) {
        if !v.contains(&e.path) {
            v.push(e.path.clone());
        }
    }
    v
}

/// The recorded entries that are no longer in their files, as `missing hooks.X entry` texts
/// (opencode: `missing <path>`).
pub fn missing(rec: &InstallRecord) -> Vec<String> {
    if matches!(rec.harness, Harness::OpencodeV1 | Harness::OpencodeV2) {
        return opencode::missing(rec);
    }
    let mut out = Vec::new();
    for e in &rec.files {
        if e.kind == EntryKind::McpCli && !claude_code::mcp_present() {
            out.push("missing the claude mcp server fbrain".to_string());
        }
        if matches!(&e.kind, EntryKind::TomlKey(k) if k == codex::MCP_KEY) && !codex::mcp_present(e)
        {
            out.push(format!(
                "missing {} in {}",
                codex::MCP_KEY,
                e.path.display()
            ));
        }
        if is_env(e) && !codex::env_present(e) {
            out.push(format!(
                "missing {} in {}",
                codex::ENV_KEY,
                e.path.display()
            ));
        }
        let (EntryKind::JsonPointer(ptr), Some(cmd)) = (&e.kind, &e.command) else {
            continue;
        };
        let name = ptr.trim_start_matches('/').replace('/', ".");
        let present = load(&e.path)
            .map(|doc| {
                doc.pointer(ptr)
                    .and_then(Value::as_array)
                    .is_some_and(|a| a.iter().any(|g| group_has(g, cmd)))
            })
            .unwrap_or(false);
        if !present {
            out.push(format!("missing {name} entry"));
        }
    }
    out
}

/// A hook event to write: its name, its optional matcher, and the command.
pub(crate) struct Add<'a> {
    pub event: &'a str,
    pub matcher: Option<&'a str>,
    pub command: &'a str,
}

/// The command line a hook runs: the binary by absolute path, quoted for the shell that runs it.
pub(crate) fn hook_command(binary: &Path, harness: Harness) -> String {
    format_command(&binary.to_string_lossy(), harness, cfg!(windows))
}

/// Claude Code runs hooks through `sh` (Git Bash on Windows, so forward slashes); Codex on Windows
/// runs PowerShell. The path is always single-quoted: nothing in it is expanded.
fn format_command(binary: &str, harness: Harness, windows: bool) -> String {
    let posix = |p: &str| format!("'{}'", p.replace('\'', "'\\''"));
    let path = match (harness, windows) {
        (Harness::Codex, true) => format!("& '{}'", binary.replace('\'', "''")),
        (_, true) => posix(&binary.replace('\\', "/")),
        (_, false) => posix(binary),
    };
    format!("{path} brief --harness {}", harness.as_str())
}

/// Installs `events` into the hooks file at `path`: one edit that first drops the entries `old`
/// recorded for that file and any entry with the same command, so a repeat install adds nothing.
pub(crate) fn install_hooks(
    path: &Path,
    harness: Harness,
    binary: &Path,
    old: Option<&InstallRecord>,
    events: &[(&str, Option<&str>)],
) -> Result<InstallRecord> {
    let command = hook_command(binary, harness);
    let adds: Vec<Add> = events
        .iter()
        .map(|(event, matcher)| Add {
            event,
            matcher: *matcher,
            command: &command,
        })
        .collect();
    let mut drop: Vec<WrittenEntry> = old.map(|r| r.files.clone()).unwrap_or_default();
    drop.retain(|e| e.path == path);
    edit_hooks(path, &drop, &adds)?;
    Ok(InstallRecord {
        harness,
        binary: binary.to_path_buf(),
        files: adds
            .iter()
            .map(|a| WrittenEntry {
                path: path.to_path_buf(),
                kind: EntryKind::JsonPointer(format!("/hooks/{}", a.event)),
                command: Some(a.command.to_string()),
            })
            .collect(),
    })
}

fn group_has(group: &Value, command: &str) -> bool {
    group["hooks"]
        .as_array()
        .is_some_and(|h| h.iter().any(|x| x["command"] == command))
}

fn load(path: &Path) -> Result<Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(e) => bail!("cannot read {}: {e}; not modified", path.display()),
    };
    let doc: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("cannot parse {}: {e}; not modified", path.display()))?;
    if !doc.is_object() {
        bail!(
            "cannot parse {}: top level is not an object; not modified",
            path.display()
        );
    }
    Ok(doc)
}

/// Removes the `remove` entries' commands and adds `add`, in one parse, one backup and one
/// atomic write. A file that does not parse is never written.
fn edit_hooks(path: &Path, remove: &[WrittenEntry], add: &[Add]) -> Result<()> {
    // A linked settings file (dotfiles) is edited where it lives, not replaced by a plain file.
    let path = &std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut doc = load(path)?;
    let bad = |what: &str| anyhow::anyhow!("cannot parse {}: {what}; not modified", path.display());
    let mut targets: Vec<(String, String)> = Vec::new();
    for e in remove {
        if let (EntryKind::JsonPointer(p), Some(c)) = (&e.kind, &e.command) {
            if let Some(ev) = p.strip_prefix("/hooks/") {
                targets.push((ev.to_string(), c.clone()));
            }
        }
    }
    targets.extend(
        add.iter()
            .map(|a| (a.event.to_string(), a.command.to_string())),
    );
    for (event, command) in &targets {
        let Some(hooks) = doc.get_mut("hooks") else {
            continue;
        };
        let hooks = hooks
            .as_object_mut()
            .ok_or_else(|| bad("hooks is not an object"))?;
        let Some(arr) = hooks.get_mut(event) else {
            continue;
        };
        let arr = arr
            .as_array_mut()
            .ok_or_else(|| bad(&format!("hooks.{event} is not an array")))?;
        let mut touched = false;
        let mut i = 0;
        while i < arr.len() {
            if let Some(h) = arr[i].get_mut("hooks").and_then(Value::as_array_mut) {
                let n = h.len();
                h.retain(|x| x["command"] != command.as_str());
                if h.len() != n {
                    touched = true;
                    if h.is_empty() {
                        arr.remove(i);
                        continue;
                    }
                }
            }
            i += 1;
        }
        if touched && arr.is_empty() {
            hooks.remove(event);
        }
        if touched && hooks.is_empty() {
            doc.as_object_mut()
                .expect("checked in load")
                .remove("hooks");
        }
    }
    for a in add {
        let root = doc.as_object_mut().expect("checked in load");
        let hooks = root.entry("hooks").or_insert_with(|| json!({}));
        let hooks = hooks
            .as_object_mut()
            .ok_or_else(|| bad("hooks is not an object"))?;
        let arr = hooks.entry(a.event).or_insert_with(|| json!([]));
        let arr = arr
            .as_array_mut()
            .ok_or_else(|| bad(&format!("hooks.{} is not an array", a.event)))?;
        let mut group = json!({});
        if let Some(m) = a.matcher {
            group["matcher"] = json!(m);
        }
        group["hooks"] = json!([{"type": "command", "command": a.command}]);
        arr.push(group);
    }
    write_with_backup(path, &(serde_json::to_string_pretty(&doc)? + "\n"))
}

/// Fails when `path` is a symbolic link: replacing it would break the user's link setup.
pub(crate) fn refuse_symlink(path: &Path) -> Result<()> {
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        bail!(
            "{} is a symbolic link; fbrain will not replace it, so it is not modified",
            path.display()
        );
    }
    Ok(())
}

/// Keeps the existing file as `<file>.fbrain-bak` (or `.fbrain-bak.1`, `.2`, ... when that name is
/// taken by different content, so no earlier backup is lost), then writes atomically. A write
/// that changes nothing makes no backup. A symbolic link is refused.
fn write_with_backup(path: &Path, text: &str) -> Result<()> {
    refuse_symlink(path)?;
    let dir = path.parent().context("config file has no folder")?;
    let name = path
        .file_name()
        .context("config file has no name")?
        .to_string_lossy();
    std::fs::create_dir_all(dir)?;
    if let Ok(current) = std::fs::read(path) {
        if current == text.as_bytes() {
            return Ok(());
        }
        let mut n = 0u32;
        loop {
            let bak = dir.join(match n {
                0 => format!("{name}.fbrain-bak"),
                n => format!("{name}.fbrain-bak.{n}"),
            });
            match std::fs::read(&bak) {
                Ok(b) if b == current => break,
                Ok(_) => n += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::copy(path, &bak)
                        .with_context(|| format!("cannot back up {}", path.display()))?;
                    break;
                }
                Err(e) => return Err(e).with_context(|| format!("cannot read {}", bak.display())),
            }
        }
    }
    crate::review::write_atomic(dir, &name, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NASTY: &str = "a b$x`y`;z'q";

    #[test]
    fn claude_code_quotes_like_posix_sh_with_forward_slashes_on_windows() {
        assert_eq!(
            format_command(
                r"C:\Users\a b\$x`y`;z'q\fbrain.exe",
                Harness::ClaudeCode,
                true
            ),
            r#"'C:/Users/a b/$x`y`;z'\''q/fbrain.exe' brief --harness claude-code"#
        );
        assert_eq!(
            format_command("/opt/a b/fbrain", Harness::ClaudeCode, false),
            "'/opt/a b/fbrain' brief --harness claude-code"
        );
    }

    #[test]
    fn codex_on_windows_uses_the_powershell_call_operator_with_doubled_quotes() {
        assert_eq!(
            format_command(r"C:\Users\a b\$x`y`;z'q\fbrain.exe", Harness::Codex, true),
            r"& 'C:\Users\a b\$x`y`;z''q\fbrain.exe' brief --harness codex"
        );
    }

    #[test]
    fn codex_on_unix_uses_posix_single_quotes() {
        assert_eq!(
            format_command("/opt/a'b/fbrain", Harness::Codex, false),
            r"'/opt/a'\''b/fbrain' brief --harness codex"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_unix_command_runs_a_binary_whose_path_has_shell_characters() {
        use std::os::unix::fs::PermissionsExt;
        for h in [Harness::ClaudeCode, Harness::Codex] {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join(NASTY);
            std::fs::create_dir_all(&dir).unwrap();
            let bin = dir.join("fbrain");
            std::fs::write(&bin, "#!/bin/sh\necho \"$@\"\n").unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            let line = format_command(&bin.to_string_lossy(), h, false);
            let out = std::process::Command::new("sh")
                .args(["-c", &line])
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim(),
                format!("brief --harness {}", h.as_str()),
                "{line}"
            );
        }
    }
}
