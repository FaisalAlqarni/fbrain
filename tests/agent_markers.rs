//! How the guard and `proposed_by` learn that an agent, not Faisal, runs a command.
//! Claude Code: `brief` (run by its SessionStart hook) exports FBRAIN_HARNESS through
//! CLAUDE_ENV_FILE, and CLAUDE_CODE_CHILD_SESSION is a marker Claude Code sets itself.
//! Codex: `shell_environment_policy.set`. opencode: the plugins' shell environment hooks.
mod common;

use common::Sandbox;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn brain(sb: &Sandbox) -> PathBuf {
    let b = sb.root.join("brain");
    fs::create_dir_all(&b).unwrap();
    std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&b)
        .status()
        .unwrap();
    fs::write(b.join(".fbrain.toml"), "name = \"t\"\n").unwrap();
    b
}

#[test]
fn the_claude_brief_exports_the_marker_to_the_session_env_file_once() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let env = sb.root.join("claude-env");
    for _ in 0..2 {
        sb.cmd()
            .env("CLAUDE_ENV_FILE", &env)
            .arg("--brain")
            .arg(&b)
            .args(["brief", "--harness", "claude-code", "--cwd"])
            .arg(&b)
            .assert()
            .success();
    }
    assert_eq!(
        fs::read_to_string(&env).unwrap(),
        "export FBRAIN_HARNESS=claude-code\n"
    );
    let other = sb.root.join("codex-env");
    sb.cmd()
        .env("CLAUDE_ENV_FILE", &other)
        .arg("--brain")
        .arg(&b)
        .args(["brief", "--harness", "codex", "--cwd"])
        .arg(&b)
        .assert()
        .success();
    assert!(!other.exists(), "only the claude-code brief writes it");
}

/// Runs `fbrain` with stdin, stdout and stderr on a pseudo terminal; returns the exit code.
#[cfg(unix)]
fn on_a_terminal(mut cmd: std::process::Command) -> Option<i32> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::Stdio;
    let (mut m, mut s) = (0, 0);
    let rc = unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0);
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
    cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    let mut child = cmd.spawn().unwrap();
    drop((cmd, slave));
    let code = child.wait().unwrap().code();
    drop(master);
    code
}

#[cfg(unix)]
fn proposal_id(sb: &Sandbox, b: &std::path::Path) -> String {
    sb.cmd()
        .arg("--brain")
        .arg(b)
        .args(["propose", "--op", "add", "--target", "new.md"])
        .args(["--rationale", "r", "--source", "s"])
        .write_stdin("x\n")
        .assert()
        .success();
    let e = fs::read_dir(b.join("inbox"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    e.file_name()
        .to_string_lossy()
        .trim_end_matches(".md")
        .to_string()
}

#[cfg(unix)]
fn approve_on_a_terminal(sb: &Sandbox, b: &std::path::Path, env: &[(&str, &str)]) -> Option<i32> {
    let id = proposal_id(sb, b);
    let mut cmd = sb.std_cmd();
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.arg("--brain").arg(b).args(["review", &id, "--approve"]);
    on_a_terminal(cmd)
}

#[cfg(unix)]
#[test]
fn a_claude_code_child_session_is_refused_even_on_a_terminal() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let code = approve_on_a_terminal(&sb, &b, &[("CLAUDE_CODE_CHILD_SESSION", "1")]);
    assert_eq!(code, Some(3));
    assert!(!b.join("new.md").exists());
}

#[cfg(unix)]
#[test]
fn a_terminal_with_no_agent_marker_may_approve() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    assert_eq!(approve_on_a_terminal(&sb, &b, &[]), Some(0));
    assert!(b.join("new.md").exists());
}

#[cfg(unix)]
#[test]
fn claudecode_alone_does_not_refuse_because_ide_terminals_set_it_too() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    assert_eq!(
        approve_on_a_terminal(&sb, &b, &[("CLAUDECODE", "1")]),
        Some(0)
    );
}

fn proposed_by(sb: &Sandbox, b: &std::path::Path, env: &[(&str, &str)]) -> String {
    let mut c = sb.cmd();
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c
        .arg("--brain")
        .arg(b)
        .args(["--json", "propose", "--op", "add", "--target", "n.md"])
        .args(["--rationale", "r", "--source", "s"])
        .write_stdin("x\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    v["proposed_by"].as_str().unwrap().to_string()
}

#[test]
fn propose_names_claude_code_from_its_own_marker_and_prefers_fbrain_harness() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    assert_eq!(proposed_by(&sb, &b, &[]), "cli");
    assert_eq!(
        proposed_by(&sb, &b, &[("CLAUDE_CODE_CHILD_SESSION", "1")]),
        "claude-code"
    );
    assert_eq!(
        proposed_by(
            &sb,
            &b,
            &[
                ("CLAUDE_CODE_CHILD_SESSION", "1"),
                ("FBRAIN_HARNESS", "codex")
            ]
        ),
        "codex"
    );
}

fn codex_config(sb: &Sandbox) -> PathBuf {
    sb.root.join("codex/config.toml")
}

#[test]
fn codex_install_sets_the_marker_for_its_shell_commands_and_uninstall_removes_only_it() {
    let sb = Sandbox::new();
    let seed = "# mine\nmodel = \"x\"\n\n[shell_environment_policy]\ninherit = \"core\"\n";
    fs::write(codex_config(&sb), seed).unwrap();
    sb.cmd().args(["install", "codex"]).assert().success();
    let v: toml::Value = fs::read_to_string(codex_config(&sb))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        v["shell_environment_policy"]["set"]["FBRAIN_HARNESS"].as_str(),
        Some("codex")
    );
    assert_eq!(
        v["shell_environment_policy"]["inherit"].as_str(),
        Some("core")
    );
    sb.cmd().args(["install", "codex"]).assert().success();
    sb.cmd().args(["uninstall", "codex"]).assert().success();
    assert_eq!(fs::read_to_string(codex_config(&sb)).unwrap(), seed);
}

#[test]
fn codex_install_leaves_no_config_behind_after_uninstall_when_there_was_none() {
    let sb = Sandbox::new();
    sb.cmd().args(["install", "codex"]).assert().success();
    sb.cmd().args(["uninstall", "codex"]).assert().success();
    let left = fs::read_to_string(codex_config(&sb)).unwrap_or_default();
    assert!(left.trim().is_empty(), "{left}");
}

#[test]
fn doctor_reports_a_codex_marker_that_was_removed() {
    let sb = Sandbox::new();
    sb.cmd().args(["install", "codex"]).assert().success();
    fs::write(codex_config(&sb), "").unwrap();
    let o = sb.cmd().arg("doctor").output().unwrap();
    let text =
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
    assert!(
        text.contains("fail: harness:codex: missing shell_environment_policy.set.FBRAIN_HARNESS"),
        "{text}"
    );
}

#[test]
fn a_codex_marker_the_user_set_to_another_value_is_refused_not_overwritten() {
    let sb = Sandbox::new();
    let seed = "[shell_environment_policy.set]\nFBRAIN_HARNESS = \"mine\"\n";
    fs::write(codex_config(&sb), seed).unwrap();
    sb.cmd().args(["install", "codex"]).assert().failure();
    assert_eq!(fs::read_to_string(codex_config(&sb)).unwrap(), seed);
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Installs an opencode plugin and drives its shell environment hook under node.
fn shell_env(sb: &Sandbox, harness: &str) -> Value {
    assert!(
        node_available(),
        "node not found: install node to run plugin tests"
    );
    sb.cmd().args(["install", harness]).assert().success();
    let plugin = sb.root.join("xdg-config/opencode/plugins/fbrain.js");
    let dir = sb.root.join("node");
    fs::create_dir_all(&dir).unwrap();
    fs::copy(&plugin, dir.join("plugin.mjs")).unwrap();
    fs::write(
        dir.join("driver.mjs"),
        r#"
import { pathToFileURL } from "node:url";
const [, , file, harness] = process.argv;
const m = await import(pathToFileURL(file).href);
const env = {};
if (harness === "opencode-v1") {
  const p = await m.fbrain({ directory: "/x" });
  await p["shell.env"]({ cwd: "/x" }, { env });
} else {
  let handler;
  await m.default.setup({
    location: { directory: "/x" },
    session: { hook: async () => {} },
    shell: { hook: async (name, fn) => { if (name === "create.before") handler = fn; } },
  });
  handler({ command: "ls", cwd: "/x", timeout: 1, shell: "sh", env });
}
console.log(JSON.stringify(env));
"#,
    )
    .unwrap();
    let o = std::process::Command::new("node")
        .arg(dir.join("driver.mjs"))
        .arg(dir.join("plugin.mjs"))
        .arg(harness)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

#[test]
fn the_opencode_v1_plugin_exports_the_marker_to_shell_commands() {
    let sb = Sandbox::new();
    assert_eq!(shell_env(&sb, "opencode-v1")["FBRAIN_HARNESS"], "opencode");
}

#[test]
fn the_opencode_v2_plugin_exports_the_marker_to_shell_commands() {
    let sb = Sandbox::new();
    assert_eq!(shell_env(&sb, "opencode-v2")["FBRAIN_HARNESS"], "opencode");
}

#[test]
fn doctor_reports_an_opencode_plugin_that_predates_the_marker() {
    let sb = Sandbox::new();
    sb.cmd().args(["install", "opencode-v1"]).assert().success();
    let plugin = sb.root.join("xdg-config/opencode/plugins/fbrain.js");
    let old = fs::read_to_string(&plugin)
        .unwrap()
        .replace("FBRAIN_HARNESS", "NOT_THE_MARKER");
    fs::write(&plugin, old).unwrap();
    let o = sb.cmd().arg("doctor").output().unwrap();
    let text =
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
    assert!(text.contains("lacks the agent marker"), "{text}");
}

fn brief_with_env_file(sb: &Sandbox, env: &std::path::Path) -> std::process::Output {
    let b = brain(sb);
    sb.cmd()
        .env("CLAUDE_ENV_FILE", env)
        .arg("--brain")
        .arg(&b)
        .args(["brief", "--harness", "claude-code", "--cwd"])
        .arg(&b)
        .output()
        .unwrap()
}

const LINE: &str = "export FBRAIN_HARNESS=claude-code\n";

#[test]
fn the_marker_goes_on_its_own_line_after_an_unterminated_last_line() {
    let sb = Sandbox::new();
    let env = sb.root.join("env");
    fs::write(&env, "export A=1").unwrap();
    assert!(brief_with_env_file(&sb, &env).status.success());
    assert_eq!(
        fs::read_to_string(&env).unwrap(),
        format!("export A=1\n{LINE}")
    );
}

#[test]
fn a_commented_or_longer_line_does_not_count_as_the_marker() {
    let sb = Sandbox::new();
    let env = sb.root.join("env");
    let seed = "# export FBRAIN_HARNESS=claude-code\nexport FBRAIN_HARNESS=claude-code-x\n";
    fs::write(&env, seed).unwrap();
    assert!(brief_with_env_file(&sb, &env).status.success());
    assert_eq!(fs::read_to_string(&env).unwrap(), format!("{seed}{LINE}"));
}

#[cfg(unix)]
#[test]
fn a_symlinked_env_file_is_not_written_through() {
    let sb = Sandbox::new();
    let target = sb.root.join("target");
    fs::write(&target, "keep\n").unwrap();
    let env = sb.root.join("env");
    std::os::unix::fs::symlink(&target, &env).unwrap();
    let o = brief_with_env_file(&sb, &env);
    assert!(o.status.success());
    assert_eq!(fs::read_to_string(&target).unwrap(), "keep\n");
    assert!(String::from_utf8_lossy(&o.stderr).contains("cannot mark"));
}

#[test]
fn an_unwritable_env_file_leaves_the_brief_working_with_a_note_on_stderr() {
    let sb = Sandbox::new();
    let env = sb.root.join("no-such-dir/env");
    let o = brief_with_env_file(&sb, &env);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("cannot mark"));
    assert!(String::from_utf8_lossy(&o.stdout).contains("additionalContext"));
}

#[test]
fn a_non_table_shell_environment_policy_stops_install_before_any_write() {
    let sb = Sandbox::new();
    let seed = "shell_environment_policy = \"core\"\n";
    fs::write(codex_config(&sb), seed).unwrap();
    for args in [
        &["install", "codex"][..],
        &["install", "codex", "--mcp"][..],
    ] {
        sb.cmd().args(args).assert().failure();
        assert_eq!(fs::read_to_string(codex_config(&sb)).unwrap(), seed);
        assert!(!sb.root.join("codex/hooks.json").exists(), "hooks written");
        assert!(
            !sb.config_dir().join("installs.json").exists(),
            "orphan record"
        );
    }
}

#[test]
fn uninstall_leaves_a_marker_the_user_had_before_install() {
    let sb = Sandbox::new();
    let seed = "[shell_environment_policy.set]\nFBRAIN_HARNESS = \"codex\"\n";
    fs::write(codex_config(&sb), seed).unwrap();
    sb.cmd().args(["install", "codex"]).assert().success();
    sb.cmd().args(["install", "codex"]).assert().success();
    sb.cmd().args(["uninstall", "codex"]).assert().success();
    assert_eq!(fs::read_to_string(codex_config(&sb)).unwrap(), seed);
}
