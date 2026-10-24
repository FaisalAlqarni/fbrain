//! `fbrain install|uninstall claude-code|codex` through the CLI, with every harness config
//! dir inside the sandbox.
mod common;

use common::Sandbox;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const FBRAIN: &str = env!("CARGO_BIN_EXE_fbrain");

fn settings(sb: &Sandbox) -> PathBuf {
    sb.root.join("claude/settings.json")
}

fn codex_hooks(sb: &Sandbox) -> PathBuf {
    sb.root.join("codex/hooks.json")
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
}

fn caveman() -> Value {
    json!({"matcher": "startup", "hooks": [{"type": "command", "command": "caveman-hook --start"}]})
}

fn seed_settings(sb: &Sandbox) -> String {
    let text = r#"{
    "permissions": {"allow": ["Bash(ls:*)"]},
    "hooks": {
        "SessionStart": [
            {"matcher": "startup", "hooks": [{"type": "command", "command": "caveman-hook --start"}]}
        ]
    },
    "model": "opus"
}
"#;
    fs::write(settings(sb), text).unwrap();
    text.to_string()
}

fn install(sb: &Sandbox, h: &str) -> String {
    let o = sb
        .cmd()
        .args(["install", h])
        .assert()
        .success()
        .get_output()
        .clone();
    String::from_utf8(o.stdout).unwrap()
}

/// Entries of `hooks.<event>` whose command runs `fbrain brief`.
fn fbrain_entries(v: &Value, event: &str) -> Vec<Value> {
    v["hooks"][event]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.to_string().contains("brief --harness"))
        .collect()
}

#[test]
fn install_adds_one_entry_per_event_and_keeps_everything_else() {
    let sb = Sandbox::new();
    seed_settings(&sb);
    let before = read_json(&settings(&sb));
    install(&sb, "claude-code");
    let after = read_json(&settings(&sb));
    assert_eq!(after["permissions"], before["permissions"]);
    assert_eq!(after["model"], before["model"]);
    assert_eq!(after["hooks"]["SessionStart"][0], caveman());
    assert_eq!(fbrain_entries(&after, "SessionStart").len(), 1);
    assert_eq!(fbrain_entries(&after, "SubagentStart").len(), 1);
    assert_eq!(
        after["hooks"]["SessionStart"][1]["matcher"],
        "startup|resume|clear|compact"
    );
    let rec = read_json(&sb.config_dir().join("installs.json"));
    let text = rec.to_string();
    assert!(text.contains("/hooks/SessionStart"), "{text}");
    assert!(text.contains("/hooks/SubagentStart"), "{text}");
    assert!(text.contains("claude-code"), "{text}");
}

#[test]
fn install_twice_leaves_one_entry_per_event() {
    let sb = Sandbox::new();
    seed_settings(&sb);
    install(&sb, "claude-code");
    install(&sb, "claude-code");
    let after = read_json(&settings(&sb));
    assert_eq!(fbrain_entries(&after, "SessionStart").len(), 1);
    assert_eq!(fbrain_entries(&after, "SubagentStart").len(), 1);
    assert_eq!(after["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
}

#[test]
fn the_command_starts_with_the_binary_path_and_the_original_is_backed_up() {
    let sb = Sandbox::new();
    let original = seed_settings(&sb);
    install(&sb, "claude-code");
    let after = read_json(&settings(&sb));
    let cmd = fbrain_entries(&after, "SessionStart")[0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        cmd.starts_with(&format!("'{FBRAIN}' ")) && cmd.ends_with(" brief --harness claude-code"),
        "{cmd}"
    );
    let bak = fs::read(sb.root.join("claude/settings.json.fbrain-bak")).unwrap();
    assert_eq!(bak, original.as_bytes());
    // The backup is kept: a second install does not overwrite it.
    install(&sb, "claude-code");
    let bak = fs::read(sb.root.join("claude/settings.json.fbrain-bak")).unwrap();
    assert_eq!(bak, original.as_bytes());
}

#[test]
fn uninstall_removes_only_the_recorded_entries() {
    let sb = Sandbox::new();
    seed_settings(&sb);
    let before = read_json(&settings(&sb));
    install(&sb, "claude-code");
    sb.cmd()
        .args(["uninstall", "claude-code"])
        .assert()
        .success();
    assert_eq!(read_json(&settings(&sb)), before);
    let rec = fs::read_to_string(sb.config_dir().join("installs.json")).unwrap();
    assert!(!rec.contains("claude-code"), "{rec}");
}

#[test]
fn a_settings_file_that_does_not_parse_is_left_alone() {
    let sb = Sandbox::new();
    fs::write(settings(&sb), "{ not json").unwrap();
    let o = sb
        .cmd()
        .args(["install", "claude-code"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("cannot parse "), "{err}");
    assert!(err.contains("; not modified"), "{err}");
    assert_eq!(fs::read_to_string(settings(&sb)).unwrap(), "{ not json");
}

#[test]
fn codex_install_writes_hooks_json_prints_the_trust_reminder_and_uninstall_removes_it() {
    let sb = Sandbox::new();
    let out = install(&sb, "codex");
    assert!(
        out.contains(
            "Codex: run /hooks in Codex and trust the fbrain hook (required after every install or change)."
        ),
        "{out}"
    );
    let v = read_json(&codex_hooks(&sb));
    for ev in ["SessionStart", "SubagentStart"] {
        let e = fbrain_entries(&v, ev);
        assert_eq!(e.len(), 1, "{ev}");
        let cmd = e[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.ends_with(" brief --harness codex"), "{cmd}");
    }
    sb.cmd().args(["uninstall", "codex"]).assert().success();
    let v = read_json(&codex_hooks(&sb));
    assert!(fbrain_entries(&v, "SessionStart").is_empty());
    assert!(fbrain_entries(&v, "SubagentStart").is_empty());
}

#[cfg(unix)]
#[test]
fn the_installed_command_prints_the_brief_for_the_mapped_project() {
    let sb = Sandbox::new();
    let brain = sb.fixture_brain("basic");
    let repo = sb.root.join("repos/alpha-repo");
    fs::create_dir_all(&repo).unwrap();
    let mut toml = fs::read_to_string(brain.join(".fbrain.toml")).unwrap();
    toml.push_str(&format!(
        "\n[projects.alpha]\nrepos = [{:?}]\n",
        repo.to_str().unwrap()
    ));
    fs::write(brain.join(".fbrain.toml"), toml).unwrap();
    install(&sb, "claude-code");
    let v = read_json(&settings(&sb));
    let cmd = fbrain_entries(&v, "SessionStart")[0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    let mut child = sb
        .shell_cmd(&cmd)
        .env("FBRAIN_BRAIN", &brain)
        .current_dir(&repo)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"hook_event_name":"SessionStart"}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ctx = json["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(ctx.contains("### Project: alpha"), "{ctx}");
}

#[test]
fn a_moved_config_dir_leaves_no_hook_behind_in_the_old_one() {
    let sb = Sandbox::new();
    let (a, b) = (sb.root.join("claude-a"), sb.root.join("claude-b"));
    for d in [&a, &b] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(a.join("settings.json"), r#"{"model":"opus"}"#).unwrap();
    for d in [&a, &b] {
        sb.cmd()
            .env("CLAUDE_CONFIG_DIR", d)
            .args(["install", "claude-code"])
            .assert()
            .success();
    }
    let old = read_json(&a.join("settings.json"));
    assert!(fbrain_entries(&old, "SessionStart").is_empty(), "{old}");
    assert!(fbrain_entries(&old, "SubagentStart").is_empty(), "{old}");
    assert_eq!(old["model"], "opus");
    assert_eq!(
        fbrain_entries(&read_json(&b.join("settings.json")), "SessionStart").len(),
        1
    );
    sb.cmd()
        .env("CLAUDE_CONFIG_DIR", &b)
        .args(["uninstall", "claude-code"])
        .assert()
        .success();
    let b = read_json(&b.join("settings.json"));
    assert!(fbrain_entries(&b, "SessionStart").is_empty(), "{b}");
}
