//! Claude Code: SessionStart and SubagentStart hooks in `settings.json`.

use super::{install_hooks, EntryKind, Harness, InstallRecord, WrittenEntry};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// `$CLAUDE_CONFIG_DIR/settings.json` when the variable is set, else `~/.claude/settings.json`.
pub fn settings_path() -> Result<PathBuf> {
    let dir = match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => crate::config::home_dir()
            .context("cannot find the home folder")?
            .join(".claude"),
    };
    Ok(dir.join("settings.json"))
}

pub fn install(binary: &Path, old: Option<&InstallRecord>) -> Result<InstallRecord> {
    install_hooks(
        &settings_path()?,
        Harness::ClaudeCode,
        binary,
        old,
        &[
            ("SessionStart", Some("startup|resume|clear|compact")),
            ("SubagentStart", None),
        ],
    )
}

/// The `claude` executable on PATH. On Windows the npm install is `claude.cmd`.
fn find_cli() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["claude.exe", "claude.cmd", "claude"]
    } else {
        &["claude"]
    };
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

/// For `install --mcp`: the CLI, or the error that names the command to run by hand.
pub fn require_cli(binary: &Path) -> Result<PathBuf> {
    find_cli().with_context(|| {
        format!(
            "claude CLI not found; install Claude Code or register manually: claude mcp add --scope user fbrain -- {} mcp",
            binary.display()
        )
    })
}

fn run_cli(cli: &Path, args: &[&Path]) -> Result<std::process::Output> {
    std::process::Command::new(cli)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("cannot run {}", cli.display()))
}

fn failed(args: &[&Path], out: &std::process::Output) -> anyhow::Error {
    anyhow::anyhow!(
        "claude {} failed ({}): {}",
        args[..2]
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" "),
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// Whether the `claude` CLI lists a server named `fbrain`.
pub fn mcp_present() -> bool {
    let p = Path::new;
    find_cli().is_some_and(|cli| {
        run_cli(&cli, &[p("mcp"), p("get"), p("fbrain")]).is_ok_and(|o| o.status.success())
    })
}

/// Registers the server through the `claude` CLI, which owns `~/.claude.json`; fbrain never
/// writes that file because Claude Code rewrites it while it runs.
pub fn add_mcp(cli: &Path, binary: &Path) -> Result<WrittenEntry> {
    let p = Path::new;
    let args = [
        p("mcp"),
        p("add"),
        p("--scope"),
        p("user"),
        p("fbrain"),
        p("--"),
        binary,
        p("mcp"),
    ];
    let out = run_cli(cli, &args)?;
    if !out.status.success() {
        return Err(failed(&args, &out));
    }
    Ok(WrittenEntry {
        path: settings_path()?,
        kind: EntryKind::McpCli,
        command: Some(binary.to_string_lossy().into_owned()),
    })
}

pub fn remove_mcp() -> Result<()> {
    let cli = find_cli().context(
        "claude CLI not found; remove the server yourself: claude mcp remove --scope user fbrain",
    )?;
    let p = Path::new;
    let args = [p("mcp"), p("remove"), p("--scope"), p("user"), p("fbrain")];
    let out = run_cli(&cli, &args)?;
    // A server that is already gone is what the caller wants; any other failure is not.
    let said = String::from_utf8_lossy(&out.stderr).to_lowercase();
    if out.status.success() || said.contains("no mcp server found") {
        return Ok(());
    }
    Err(failed(&args, &out))
}
