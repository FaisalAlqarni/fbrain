mod common;

use common::Sandbox;
use std::path::PathBuf;

const TEST_MODEL: &str = "google/embeddinggemma-300m";

fn model_src() -> PathBuf {
    PathBuf::from(std::env::var("FBRAIN_TEST_MODEL_DIR").expect("set FBRAIN_TEST_MODEL_DIR"))
}

fn out(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn doctor_without_a_model_fails_the_model_check() {
    let sb = Sandbox::new();
    let o = sb.cmd().arg("doctor").assert().code(1).get_output().clone();
    assert!(
        out(&o).contains(
            "fail: model: not downloaded: google/embeddinggemma-300m; run fbrain doctor --fix"
        ),
        "{}",
        out(&o)
    );
}

#[test]
fn doctor_fix_copies_the_model_into_the_cache() {
    let sb = Sandbox::new();
    let o = sb
        .cmd()
        .env("FBRAIN_MODEL", TEST_MODEL)
        .env("FBRAIN_TEST_MODEL_DIR", model_src())
        .env_remove("FBRAIN_OFFLINE")
        .args(["doctor", "--fix"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(out(&o).contains("ok: model: "), "{}", out(&o));
    sb.cmd()
        .env("FBRAIN_MODEL", TEST_MODEL)
        .arg("doctor")
        .assert()
        .success();
}

#[test]
fn doctor_fix_offline_refuses_to_download() {
    let sb = Sandbox::new();
    let o = sb
        .cmd()
        .env("FBRAIN_MODEL", TEST_MODEL)
        .env("FBRAIN_OFFLINE", "1")
        .args(["doctor", "--fix"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let all = format!("{}{}", out(&o), String::from_utf8_lossy(&o.stderr));
    assert!(
        all.contains("offline: network disabled (FBRAIN_OFFLINE=1)"),
        "{all}"
    );
}

#[test]
fn init_downloads_the_model_unless_told_not_to() {
    let sb = Sandbox::new();
    let cmd = |name: &str, extra: &[&str]| {
        let mut c = sb.cmd();
        c.env("FBRAIN_MODEL", TEST_MODEL)
            .env("FBRAIN_TEST_MODEL_DIR", model_src())
            .env_remove("FBRAIN_OFFLINE")
            .arg("init")
            .arg(sb.root.join(name))
            .args(["--name", name])
            .args(extra);
        c
    };
    cmd("skip", &["--no-model"]).assert().success();
    sb.cmd()
        .env("FBRAIN_MODEL", TEST_MODEL)
        .arg("doctor")
        .assert()
        .code(1);
    cmd("c", &[]).assert().success();
    sb.cmd()
        .env("FBRAIN_MODEL", TEST_MODEL)
        .arg("doctor")
        .assert()
        .success();
}

fn model_in_cache(sb: &Sandbox) -> PathBuf {
    sb.root.join("cache/models/google--embeddinggemma-300m")
}

fn doctor(sb: &Sandbox, fix: bool) -> assert_cmd::Command {
    let mut c = sb.cmd();
    c.env("FBRAIN_MODEL", TEST_MODEL)
        .env("FBRAIN_TEST_MODEL_DIR", model_src())
        .env_remove("FBRAIN_OFFLINE")
        .arg("doctor");
    if fix {
        c.arg("--fix");
    }
    c
}

#[test]
fn doctor_fix_replaces_a_model_that_will_not_load() {
    let sb = Sandbox::new();
    let dir = model_in_cache(&sb);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(dir.join("onnx")).unwrap();
    for f in [
        "tokenizer.json",
        "onnx/model_q4.onnx",
        "onnx/model_q4.onnx_data",
    ] {
        std::fs::write(dir.join(f), "junk").unwrap();
    }
    let o = doctor(&sb, false).assert().code(1).get_output().clone();
    assert!(out(&o).contains("fail: model:"), "{}", out(&o));
    doctor(&sb, true).assert().success();
    doctor(&sb, false).assert().success();
}

#[test]
fn doctor_fix_completes_a_partial_model_dir() {
    let sb = Sandbox::new();
    let dir = model_in_cache(&sb);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tokenizer.json"), "{}").unwrap();
    doctor(&sb, true).assert().success();
    doctor(&sb, false).assert().success();
}

#[test]
fn doctor_reports_why_the_brain_cannot_be_used() {
    let sb = Sandbox::new();
    let o = sb
        .cmd()
        .env("FBRAIN_MODEL", TEST_MODEL)
        .args(["--brain"])
        .arg(sb.root.join("nope"))
        .arg("doctor")
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(out(&o).contains("fail: index: not a brain"), "{}", out(&o));
}

#[test]
fn init_with_a_failed_download_exits_3_and_keeps_the_brain() {
    let sb = Sandbox::new();
    let o = sb
        .cmd()
        .env("FBRAIN_OFFLINE", "1")
        .arg("init")
        .arg(sb.root.join("d"))
        .args(["--name", "d"])
        .assert()
        .code(3)
        .get_output()
        .clone();
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(
        err.contains("warning: model not downloaded: offline: network disabled (FBRAIN_OFFLINE=1); run fbrain doctor --fix"),
        "{err}"
    );
    assert!(sb.root.join("d/.fbrain.toml").exists());
}

#[test]
fn init_adopt_reports_an_unreadable_brain_config() {
    let sb = Sandbox::new();
    let dir = sb.root.join("e");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".fbrain.toml"), "model = [").unwrap();
    let o = sb
        .cmd()
        .arg("init")
        .arg("--adopt")
        .arg(&dir)
        .args(["--name", "e"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(String::from_utf8_lossy(&o.stderr).contains("invalid"));
}

#[test]
fn doctor_detects_a_changed_byte_and_fix_restores_the_file() {
    let sb = Sandbox::new();
    doctor(&sb, true).assert().success();
    let tok = model_in_cache(&sb).join("tokenizer.json");
    let mut bytes = std::fs::read(&tok).unwrap();
    bytes[10] ^= 1;
    std::fs::write(&tok, bytes).unwrap();

    let o = doctor(&sb, false).assert().code(1).get_output().clone();
    assert!(
        out(&o).contains("fail: model: checksum mismatch: tokenizer.json; run fbrain doctor --fix"),
        "{}",
        out(&o)
    );
    let o = doctor(&sb, true).assert().success().get_output().clone();
    assert!(
        out(&o).contains("ok: model: google/embeddinggemma-300m"),
        "{}",
        out(&o)
    );
}

#[test]
fn doctor_on_a_partial_model_dir_says_what_is_missing_and_how_to_fix_it() {
    let sb = Sandbox::new();
    let dir = model_in_cache(&sb);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        model_src().join("tokenizer.json"),
        dir.join("tokenizer.json"),
    )
    .unwrap();
    let o = doctor(&sb, false).assert().code(1).get_output().clone();
    assert!(
        out(&o).contains(
            "fail: model: incomplete model: onnx/model_q4.onnx is missing; run fbrain doctor --fix"
        ),
        "{}",
        out(&o)
    );
}
