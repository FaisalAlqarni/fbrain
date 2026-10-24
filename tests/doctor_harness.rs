//! `fbrain doctor` binary, harness and search checks.
mod common;

use common::Sandbox;
use std::fs;
use std::path::PathBuf;

const MODEL: &str = "google/embeddinggemma-300m";

fn text(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn settings(sb: &Sandbox) -> PathBuf {
    sb.root.join("claude").join("settings.json")
}

/// Downloads (copies) the test model so a later `doctor` has no model failure of its own.
fn install_model(sb: &Sandbox) {
    sb.cmd()
        .env("FBRAIN_MODEL", MODEL)
        .env(
            "FBRAIN_TEST_MODEL_DIR",
            std::env::var("FBRAIN_TEST_MODEL_DIR").unwrap(),
        )
        .env_remove("FBRAIN_OFFLINE")
        .args(["doctor", "--fix"])
        .assert()
        .success();
}

/// Installs claude-code through a private copy of the binary and returns the copy's path. A
/// freshly written executable can fail to start with "text file busy" while a parallel test
/// forks, so the start is retried.
fn install_via_copy(sb: &Sandbox) -> PathBuf {
    let copy = sb.root.join("fbrain-copy");
    fs::copy(env!("CARGO_BIN_EXE_fbrain"), &copy).unwrap();
    for _ in 0..50 {
        match sb
            .std_cmd_for(&copy)
            .args(["install", "claude-code"])
            .status()
        {
            Ok(s) => {
                assert!(s.success());
                return copy;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
    panic!("could not start {}", copy.display());
}

#[test]
fn doctor_fails_when_the_recorded_binary_is_gone() {
    let sb = Sandbox::new();
    let copy = install_via_copy(&sb);
    fs::remove_file(&copy).unwrap();
    let o = sb.cmd().arg("doctor").assert().code(1).get_output().clone();
    assert!(
        text(&o).contains(&format!("fail: binary: {} not found", copy.display())),
        "{}",
        text(&o)
    );
}

#[test]
fn doctor_always_reminds_about_codex_trust() {
    let sb = Sandbox::new();
    sb.cmd().args(["install", "codex"]).assert().success();
    let o = sb.cmd().arg("doctor").output().unwrap();
    assert!(
        text(&o).contains(
            "warn: harness:codex: run /hooks in Codex and trust the fbrain hook after any install or change"
        ),
        "{}",
        text(&o)
    );
}

#[test]
fn doctor_reports_a_deleted_entry_and_fix_restores_it() {
    let sb = Sandbox::new();
    install_model(&sb);
    sb.cmd().args(["install", "claude-code"]).assert().success();
    sb.cmd()
        .env("FBRAIN_MODEL", MODEL)
        .arg("doctor")
        .assert()
        .success();
    let mut v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(settings(&sb)).unwrap()).unwrap();
    v["hooks"].as_object_mut().unwrap().remove("SessionStart");
    fs::write(settings(&sb), v.to_string()).unwrap();
    let o = sb
        .cmd()
        .env("FBRAIN_MODEL", MODEL)
        .arg("doctor")
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(
        text(&o).contains("fail: harness:claude-code: missing hooks.SessionStart entry"),
        "{}",
        text(&o)
    );
    sb.cmd()
        .env("FBRAIN_MODEL", MODEL)
        .args(["doctor", "--fix"])
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(settings(&sb)).unwrap()).unwrap();
    assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    sb.cmd()
        .env("FBRAIN_MODEL", MODEL)
        .arg("doctor")
        .assert()
        .success();
}

#[test]
fn doctor_times_a_real_search_once_the_resident_process_runs() {
    let sb = Sandbox::new();
    let brain = sb.fixture_brain("semantic");
    fs::write(brain.join(".fbrain.toml"), "name = \"semantic\"\n").unwrap();
    install_model(&sb);
    let skipped = sb
        .cmd()
        .arg("--brain")
        .arg(&brain)
        .arg("doctor")
        .output()
        .unwrap();
    assert!(
        text(&skipped).contains("ok: search: skipped, no resident process is running"),
        "{}",
        text(&skipped)
    );
    let searched = sb
        .cmd()
        .arg("--brain")
        .arg(&brain)
        .args(["search", "refund policy"])
        .assert()
        .success()
        .get_output()
        .clone();
    let o = sb
        .cmd()
        .arg("--brain")
        .arg(&brain)
        .arg("doctor")
        .assert()
        .success()
        .get_output()
        .clone();
    let line = text(&o)
        .lines()
        .find(|l| l.starts_with("ok: search: "))
        .unwrap_or_default()
        .to_string();
    let n = line
        .strip_prefix("ok: search: ")
        .and_then(|r| r.strip_suffix(" ms"))
        .unwrap_or_default();
    assert!(
        n.parse::<u64>().is_ok(),
        "{line}\nsearch stderr: {}\nserve.log: {}",
        String::from_utf8_lossy(&searched.stderr),
        fs::read_to_string(sb.runtime_dir().join("serve.log")).unwrap_or_default()
    );
}

#[test]
fn doctor_fix_reinstalls_with_the_recorded_binary_not_the_running_one() {
    let sb = Sandbox::new();
    let copy = install_via_copy(&sb);
    let mut v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(settings(&sb)).unwrap()).unwrap();
    v["hooks"].as_object_mut().unwrap().remove("SessionStart");
    fs::write(settings(&sb), v.to_string()).unwrap();
    let o = sb.cmd().args(["doctor", "--fix"]).output().unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(settings(&sb)).unwrap()).unwrap();
    let cmd = v["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    // The Claude Code hook runs in a POSIX shell, so Windows paths are written with `/`.
    let shown = copy.display().to_string().replace('\\', "/");
    assert!(cmd.starts_with(&format!("'{shown}' ")), "{cmd}");
    assert!(
        text(&o).contains(&settings(&sb).display().to_string()),
        "{}",
        text(&o)
    );
}

#[test]
fn doctor_fix_does_not_reinstall_when_the_recorded_binary_is_gone() {
    let sb = Sandbox::new();
    let copy = install_via_copy(&sb);
    fs::remove_file(&copy).unwrap();
    let mut v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(settings(&sb)).unwrap()).unwrap();
    v["hooks"].as_object_mut().unwrap().remove("SessionStart");
    fs::write(settings(&sb), v.to_string()).unwrap();
    let o = sb
        .cmd()
        .args(["doctor", "--fix"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(
        text(&o).contains("fail: harness:claude-code: ")
            && text(&o).contains("run fbrain install claude-code"),
        "{}",
        text(&o)
    );
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(settings(&sb)).unwrap()).unwrap();
    assert!(v["hooks"].get("SessionStart").is_none());
}
