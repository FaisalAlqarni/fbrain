//! Codex: SessionStart and SubagentStart hooks in `$CODEX_HOME/hooks.json` (ADR 0006).

use super::{install_hooks, write_with_backup, EntryKind, Harness, InstallRecord, WrittenEntry};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use toml_edit::{value, Array, DocumentMut, Item, Table};

pub const MCP_KEY: &str = "mcp_servers.fbrain";

/// The marker Codex's shell commands get through `shell_environment_policy.set`, so `fbrain`
/// knows an agent runs it. <https://learn.chatgpt.com/docs/config-file/config-reference>
pub const ENV_KEY: &str = "shell_environment_policy.set.FBRAIN_HARNESS";
const ENV_NAME: &str = "FBRAIN_HARNESS";
const ENV_VALUE: &str = "codex";

pub const TRUST_REMINDER: &str =
    "Codex: run /hooks in Codex and trust the fbrain hook (required after every install or change).";

/// `$CODEX_HOME/hooks.json`, default `~/.codex/hooks.json`.
pub fn hooks_path() -> Result<PathBuf> {
    let dir = match std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => crate::config::home_dir()
            .context("cannot find the home folder")?
            .join(".codex"),
    };
    Ok(dir.join("hooks.json"))
}

pub fn install(binary: &Path, old: Option<&InstallRecord>) -> Result<InstallRecord> {
    install_hooks(
        &hooks_path()?,
        Harness::Codex,
        binary,
        old,
        &[
            ("SessionStart", Some("startup|resume|clear|compact|fork")),
            ("SubagentStart", None),
        ],
    )
}

/// `$CODEX_HOME/config.toml`, default `~/.codex/config.toml`.
pub fn config_path() -> Result<PathBuf> {
    Ok(hooks_path()?.with_file_name("config.toml"))
}

fn load_config(path: &Path) -> Result<DocumentMut> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => bail!("cannot read {}: {e}; not modified", path.display()),
    };
    text.parse()
        .map_err(|e| anyhow::anyhow!("cannot parse {}: {e}; not modified", path.display()))
}

/// Whether `[mcp_servers.fbrain]` runs `<command> mcp`.
fn is_ours(t: Option<&Item>, command: &str) -> bool {
    let Some(t) = t else { return false };
    t.get("command").and_then(Item::as_str) == Some(command)
        && t.get("args")
            .and_then(Item::as_array)
            .is_some_and(|a| a.len() == 1 && a.get(0).and_then(|v| v.as_str()) == Some("mcp"))
}

fn server(doc: &DocumentMut) -> Option<&Item> {
    doc.get("mcp_servers")?.get("fbrain")
}

pub fn mcp_present(e: &WrittenEntry) -> bool {
    let Some(cmd) = &e.command else { return false };
    load_config(&e.path).is_ok_and(|d| is_ours(server(&d), cmd))
}

/// Everything `add_mcp` can refuse, checked without writing: a symlinked or unparseable
/// config, or a `[mcp_servers.fbrain]` that is neither ours nor recorded by an earlier install.
pub fn check_mcp(binary: &Path, recorded: bool) -> Result<()> {
    let path = config_path()?;
    super::refuse_symlink(&path)?;
    let doc = load_config(&path)?;
    let existing = server(&doc);
    if existing.is_some() && !is_ours(existing, &binary.to_string_lossy()) && !recorded {
        bail!(
            "{} already has [{MCP_KEY}], which fbrain did not write; remove it or edit it by hand",
            path.display()
        );
    }
    Ok(())
}

/// Adds `[mcp_servers.fbrain]` with comments and other tables untouched.
pub fn add_mcp(binary: &Path, recorded: bool) -> Result<WrittenEntry> {
    check_mcp(binary, recorded)?;
    let path = config_path()?;
    let command = binary.to_string_lossy().into_owned();
    let mut doc = load_config(&path)?;
    let existing = server(&doc);
    if !is_ours(existing, &command) {
        let servers = doc
            .entry("mcp_servers")
            .or_insert_with(|| {
                let mut t = Table::new();
                t.set_implicit(true);
                Item::Table(t)
            })
            .as_table_mut()
            .with_context(|| {
                format!(
                    "{}: mcp_servers is not a table; not modified",
                    path.display()
                )
            })?;
        let mut t = Table::new();
        t["command"] = value(&command);
        t["args"] = value(Array::from_iter(["mcp"]));
        servers.insert("fbrain", Item::Table(t));
        write_with_backup(&path, &doc.to_string())?;
    }
    Ok(WrittenEntry {
        path,
        kind: EntryKind::TomlKey(MCP_KEY.into()),
        command: Some(command),
    })
}

/// Takes out the recorded table, unless it no longer runs what was written (the user changed it).
pub fn remove_mcp(e: &WrittenEntry) -> Result<()> {
    let mut doc = load_config(&e.path)?;
    let Some(cmd) = &e.command else { return Ok(()) };
    let Some(servers) = doc.get_mut("mcp_servers").and_then(Item::as_table_mut) else {
        return Ok(());
    };
    if !is_ours(servers.get("fbrain"), cmd) {
        if servers.contains_key("fbrain") {
            eprintln!(
                "left [{MCP_KEY}] in {}: it was changed since install",
                e.path.display()
            );
        }
        return Ok(());
    }
    servers.remove("fbrain");
    if servers.is_empty() {
        doc.remove("mcp_servers");
    }
    write_with_backup(&e.path, &doc.to_string())
}

fn marker(doc: &DocumentMut) -> Option<&Item> {
    doc.get("shell_environment_policy")?
        .get("set")?
        .get(ENV_NAME)
}

/// What `add_env` can refuse, checked without writing: a symlinked or unparseable config, a
/// `shell_environment_policy` or `set` that is not a table, or a marker the user set to something
/// else.
pub fn check_env() -> Result<()> {
    let path = config_path()?;
    super::refuse_symlink(&path)?;
    let doc = load_config(&path)?;
    let not_table =
        |what: &str| anyhow::anyhow!("{}: {what} is not a table; not modified", path.display());
    if let Some(p) = doc.get("shell_environment_policy") {
        let p = p
            .as_table_like()
            .ok_or_else(|| not_table("shell_environment_policy"))?;
        if let Some(set) = p.get("set") {
            set.as_table_like()
                .ok_or_else(|| not_table("shell_environment_policy.set"))?;
        }
    }
    if let Some(v) = marker(&doc).filter(|v| v.as_str() != Some(ENV_VALUE)) {
        bail!(
            "{} already sets {ENV_KEY} to {}, which fbrain did not write; remove it or edit it by hand",
            path.display(),
            v.to_string().trim()
        );
    }
    Ok(())
}

pub fn env_present(e: &WrittenEntry) -> bool {
    load_config(&e.path).is_ok_and(|d| marker(&d).and_then(Item::as_str) == Some(ENV_VALUE))
}

/// Adds `FBRAIN_HARNESS = "codex"` under `[shell_environment_policy.set]`, other keys untouched.
/// The entry's `command` is `Some` only when fbrain wrote the key (now or in `wrote_before`'s
/// install), so uninstall leaves one the user had.
pub fn add_env(wrote_before: bool) -> Result<WrittenEntry> {
    check_env()?;
    let path = config_path()?;
    let mut doc = load_config(&path)?;
    let ours = marker(&doc).is_none() || wrote_before;
    if marker(&doc).is_none() {
        let table = || {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        };
        let policy = doc
            .as_table_mut()
            .entry("shell_environment_policy")
            .or_insert_with(table)
            .as_table_like_mut()
            .expect("check_env saw a table");
        let set = policy
            .entry("set")
            .or_insert_with(table)
            .as_table_like_mut()
            .expect("check_env saw a table");
        set.insert(ENV_NAME, value(ENV_VALUE));
        write_with_backup(&path, &doc.to_string())?;
    }
    Ok(WrittenEntry {
        path,
        kind: EntryKind::TomlKey(ENV_KEY.into()),
        command: ours.then(|| ENV_VALUE.to_string()),
    })
}

/// Takes the marker out, unless the user changed its value; empty tables it leaves are dropped.
pub fn remove_env(e: &WrittenEntry) -> Result<()> {
    if e.command.is_none() {
        return Ok(());
    }
    let mut doc = load_config(&e.path)?;
    if marker(&doc).and_then(Item::as_str) != Some(ENV_VALUE) {
        return Ok(());
    }
    let policy = doc
        .get_mut("shell_environment_policy")
        .and_then(Item::as_table_like_mut)
        .expect("the marker is under it");
    let set = policy
        .get_mut("set")
        .and_then(Item::as_table_like_mut)
        .expect("the marker is under it");
    set.remove(ENV_NAME);
    if set.is_empty() {
        policy.remove("set");
    }
    if policy.is_empty() {
        doc.remove("shell_environment_policy");
    }
    write_with_backup(&e.path, &doc.to_string())
}
