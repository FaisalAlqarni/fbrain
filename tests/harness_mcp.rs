//! `fbrain install|uninstall <harness> --mcp` with every harness config dir inside the sandbox.
mod common;

use common::Sandbox;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

const FBRAIN: &str = env!("CARGO_BIN_EXE_fbrain");

fn stub_lines(sb: &Sandbox) -> Vec<String> {
    fs::read_to_string(sb.stub_log())
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn codex_config(sb: &Sandbox) -> PathBuf {
    sb.root.join("codex/config.toml")
}

fn opencode_json(sb: &Sandbox) -> PathBuf {
    sb.root.join("xdg-config/opencode/opencode.json")
}

fn toml_of(p: &PathBuf) -> toml::Value {
    toml::from_str(&fs::read_to_string(p).unwrap()).unwrap()
}

fn json_of(p: &PathBuf) -> Value {
    serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn claude_code_registers_through_the_claude_cli_and_never_writes_claude_json() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["install", "claude-code", "--mcp"])
        .assert()
        .success();
    let want = format!("claude mcp add --scope user fbrain -- {FBRAIN} mcp");
    assert!(stub_lines(&sb).contains(&want), "{:?}", stub_lines(&sb));
    assert!(!sb.root.join("home/.claude.json").exists());
    assert!(!sb.root.join("claude/.claude.json").exists());

    sb.cmd()
        .args(["uninstall", "claude-code"])
        .assert()
        .success();
    let log = stub_lines(&sb);
    assert!(
        log.contains(&"claude mcp remove --scope user fbrain".to_string()),
        "{log:?}"
    );
    assert!(!sb.root.join("home/.claude.json").exists());
}

#[test]
fn claude_code_install_twice_registers_once_and_without_mcp_adds_nothing() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["install", "claude-code", "--mcp"])
        .assert()
        .success();
    sb.cmd()
        .args(["install", "claude-code", "--mcp"])
        .assert()
        .success();
    let adds = |sb: &Sandbox| {
        stub_lines(sb)
            .iter()
            .filter(|l| l.starts_with("claude mcp add"))
            .count()
    };
    assert_eq!(adds(&sb), 1, "{:?}", stub_lines(&sb));

    let sb2 = Sandbox::new();
    sb2.cmd()
        .args(["install", "claude-code"])
        .assert()
        .success();
    assert!(stub_lines(&sb2).is_empty(), "{:?}", stub_lines(&sb2));
}

#[test]
fn claude_code_without_the_claude_cli_exits_1_with_the_manual_command() {
    let sb = Sandbox::new();
    fs::remove_file(sb.stub("claude")).unwrap();
    // Only the sandbox bin (no claude) and the system dirs `sh` lives in.
    let out = sb
        .cmd()
        .env("PATH", sb.root.join("bin"))
        .args(["install", "claude-code", "--mcp"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let err = String::from_utf8(out.stderr).unwrap();
    let want = format!(
        "claude CLI not found; install Claude Code or register manually: claude mcp add --scope user fbrain -- {FBRAIN} mcp"
    );
    assert!(err.contains(&want), "{err}");
    assert!(!sb.root.join("claude/settings.json").exists());
}

#[test]
fn codex_gets_one_mcp_table_and_uninstall_takes_out_only_that() {
    let sb = Sandbox::new();
    let original = "model = \"o3\"\n# keep me\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n";
    fs::write(codex_config(&sb), original).unwrap();
    let before = toml_of(&codex_config(&sb));

    sb.cmd()
        .args(["install", "codex", "--mcp"])
        .assert()
        .success();
    let after = toml_of(&codex_config(&sb));
    let server = &after["mcp_servers"]["fbrain"];
    assert_eq!(server["command"].as_str(), Some(FBRAIN));
    assert_eq!(
        server["args"].as_array().unwrap(),
        &vec![toml::Value::from("mcp")]
    );
    let mut rest = after.clone();
    rest.as_table_mut().unwrap().remove("mcp_servers");
    // The install also sets the agent marker for Codex's shell commands.
    rest.as_table_mut()
        .unwrap()
        .remove("shell_environment_policy");
    assert_eq!(rest, before);
    assert_eq!(
        fs::read_to_string(sb.root.join("codex/config.toml.fbrain-bak")).unwrap(),
        original
    );
    assert!(fs::read_to_string(codex_config(&sb))
        .unwrap()
        .contains("# keep me"));

    // A second install leaves one table.
    sb.cmd()
        .args(["install", "codex", "--mcp"])
        .assert()
        .success();
    assert_eq!(toml_of(&codex_config(&sb)), after);

    sb.cmd().args(["uninstall", "codex"]).assert().success();
    assert_eq!(toml_of(&codex_config(&sb)), before);
}

#[cfg(unix)]
/// A `claude` that remembers whether `fbrain` is registered, like the real CLI. `add_fails`
/// makes `mcp add` exit 1; `remove_error` is what `mcp remove` says when it fails.
fn stateful_claude(sb: &Sandbox, add_fails: bool) {
    use std::os::unix::fs::PermissionsExt;
    let p = sb.root.join("bin/claude");
    let add = if add_fails {
        "echo boom >&2; exit 1"
    } else {
        "touch \"$S\""
    };
    fs::write(
        &p,
        format!(
            "#!/bin/sh\necho \"claude $@\" >> \"$FBRAIN_STUB_LOG\"\nS=\"$(dirname \"$0\")/claude.state\"\n\
             case \"$2\" in\n add) {add};;\n remove) if [ -f \"$S\" ]; then rm \"$S\"; else \
             echo 'No MCP server found with name: fbrain' >&2; exit 1; fi;;\n \
             get) [ -f \"$S\" ] || exit 1;;\nesac\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
fn claude_hooks_are_fbrain(sb: &Sandbox) -> bool {
    fs::read_to_string(sb.root.join("claude/settings.json"))
        .is_ok_and(|t| t.contains("brief --harness"))
}

#[cfg(unix)]
#[test]
fn a_failed_claude_add_leaves_the_hooks_recorded_and_uninstall_removes_them() {
    let sb = Sandbox::new();
    stateful_claude(&sb, true);
    let out = sb
        .cmd()
        .args(["install", "claude-code", "--mcp"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("boom") && err.contains("hooks"), "{err}");
    assert!(claude_hooks_are_fbrain(&sb));
    sb.cmd()
        .args(["uninstall", "claude-code"])
        .assert()
        .success();
    assert!(!claude_hooks_are_fbrain(&sb));
}

#[cfg(unix)]
#[test]
fn uninstall_does_not_fail_when_the_claude_server_is_already_gone() {
    let sb = Sandbox::new();
    stateful_claude(&sb, false);
    sb.cmd()
        .args(["install", "claude-code", "--mcp"])
        .assert()
        .success();
    fs::remove_file(sb.root.join("bin/claude.state")).unwrap();
    sb.cmd()
        .args(["uninstall", "claude-code"])
        .assert()
        .success();
    assert!(!claude_hooks_are_fbrain(&sb));
}

#[cfg(unix)]
#[test]
fn uninstall_still_fails_when_claude_remove_fails_for_another_reason() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    sb.cmd()
        .args(["install", "claude-code", "--mcp"])
        .assert()
        .success();
    let p = sb.root.join("bin/claude");
    fs::write(&p, "#!/bin/sh\necho 'permission denied' >&2\nexit 1\n").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    sb.cmd()
        .args(["uninstall", "claude-code"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("permission denied"));
    assert!(claude_hooks_are_fbrain(&sb));
}

#[cfg(unix)]
#[test]
fn doctor_reports_a_missing_claude_server_and_fix_adds_it_again() {
    let sb = Sandbox::new();
    stateful_claude(&sb, false);
    sb.cmd()
        .args(["install", "claude-code", "--mcp"])
        .assert()
        .success();
    fs::remove_file(sb.root.join("bin/claude.state")).unwrap();
    let out = sb
        .cmd()
        .args(["doctor"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("harness:claude-code") && text.contains("mcp"),
        "{text}"
    );
    // Exit 1: the sandbox is offline, so the model check fails.
    sb.cmd().args(["doctor", "--fix"]).assert().code(1);
    assert!(sb.root.join("bin/claude.state").exists());
    let adds = stub_lines(&sb)
        .iter()
        .filter(|l| l.starts_with("claude mcp add"))
        .count();
    assert_eq!(adds, 2, "{:?}", stub_lines(&sb));
}

#[test]
fn a_refused_codex_config_leaves_no_hooks_behind() {
    let sb = Sandbox::new();
    fs::write(
        codex_config(&sb),
        "[mcp_servers.fbrain]\ncommand = \"other\"\n",
    )
    .unwrap();
    sb.cmd()
        .args(["install", "codex", "--mcp"])
        .assert()
        .failure();
    assert!(!sb.root.join("codex/hooks.json").exists());
    fs::write(codex_config(&sb), "not = [valid").unwrap();
    sb.cmd()
        .args(["install", "codex", "--mcp"])
        .assert()
        .failure();
    assert!(!sb.root.join("codex/hooks.json").exists());
}

#[test]
fn codex_refuses_to_replace_an_fbrain_server_it_did_not_write() {
    let sb = Sandbox::new();
    let mine = "[mcp_servers.fbrain]\ncommand = \"other\"\n";
    fs::write(codex_config(&sb), mine).unwrap();
    let out = sb
        .cmd()
        .args(["install", "codex", "--mcp"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(String::from_utf8(out.stderr)
        .unwrap()
        .contains("mcp_servers.fbrain"));
    assert_eq!(fs::read_to_string(codex_config(&sb)).unwrap(), mine);
}

#[test]
fn opencode_gets_only_the_mcp_fbrain_key_and_uninstall_restores_the_file() {
    for h in ["opencode-v1", "opencode-v2"] {
        let sb = Sandbox::new();
        let dir = sb.root.join("xdg-config/opencode");
        fs::create_dir_all(&dir).unwrap();
        let original =
            json!({"model": "x", "mcp": {"other": {"type": "remote", "url": "https://e"}}});
        fs::write(opencode_json(&sb), original.to_string()).unwrap();

        sb.cmd().args(["install", h, "--mcp"]).assert().success();
        let after = json_of(&opencode_json(&sb));
        assert_eq!(
            after["mcp"]["fbrain"],
            json!({"type": "local", "command": [FBRAIN, "mcp"]})
        );
        let mut rest = after.clone();
        rest["mcp"].as_object_mut().unwrap().remove("fbrain");
        assert_eq!(rest, original, "{h}");
        assert_eq!(
            json_of(&dir.join("opencode.json.fbrain-bak")),
            original,
            "{h}"
        );

        sb.cmd().args(["install", h, "--mcp"]).assert().success();
        assert_eq!(json_of(&opencode_json(&sb)), after, "{h}");

        sb.cmd().args(["uninstall", h]).assert().success();
        assert_eq!(json_of(&opencode_json(&sb)), original, "{h}");
    }
}

#[test]
fn opencode_install_without_mcp_never_touches_opencode_json() {
    let sb = Sandbox::new();
    sb.cmd().args(["install", "opencode-v1"]).assert().success();
    assert!(!opencode_json(&sb).exists());
}

#[test]
fn opencode_refuses_to_replace_an_fbrain_server_it_did_not_write() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.root.join("xdg-config/opencode")).unwrap();
    let mine = json!({"mcp": {"fbrain": {"type": "local", "command": ["other"]}}});
    fs::write(opencode_json(&sb), mine.to_string()).unwrap();
    sb.cmd()
        .args(["install", "opencode-v1", "--mcp"])
        .assert()
        .failure();
    assert_eq!(json_of(&opencode_json(&sb)), mine);
    assert!(!sb
        .root
        .join("xdg-config/opencode/plugins/fbrain.js")
        .exists());
}

#[test]
fn doctor_fix_puts_back_a_removed_mcp_table_and_keeps_the_registration() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["install", "codex", "--mcp"])
        .assert()
        .success();
    let with = toml_of(&codex_config(&sb));
    fs::write(codex_config(&sb), "model = \"o3\"\n").unwrap();
    sb.cmd().args(["doctor"]).assert().failure();
    // The exit code is 1 here: the sandbox is offline, so the model check fails.
    sb.cmd().args(["doctor", "--fix"]).assert().code(1);
    let fixed = toml_of(&codex_config(&sb));
    assert_eq!(fixed["mcp_servers"], with["mcp_servers"]);
    assert_eq!(fixed["model"].as_str(), Some("o3"));
}
