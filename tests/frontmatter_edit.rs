mod common;

use common::Sandbox;
use fbrain::note::{edit_frontmatter, FmValue};
use std::fs;
use std::path::PathBuf;

fn brain_with(sb: &Sandbox, rel: &str, bytes: &[u8]) -> PathBuf {
    let dir = sb.fixture_brain("basic");
    fs::write(dir.join(rel), bytes).unwrap();
    dir
}

fn fm_set(sb: &Sandbox, brain: &PathBuf, note: &str, kv: &str) {
    sb.cmd()
        .arg("--brain")
        .arg(brain)
        .args(["fm-set", "--yes-i-am-faisal", note, kv])
        .assert()
        .success();
}

const RICH: &str = "---\n# keep this comment\nstatus: active\nowner: x\naliases: [a, b]\nprojects:\n  - one\n  - two\n---\n# Title\n\nBody --- stays.\n";

#[test]
fn setting_a_key_changes_only_its_line() {
    let sb = Sandbox::new();
    let b = brain_with(&sb, "projects/alpha/rich.md", RICH.as_bytes());
    fm_set(&sb, &b, "projects/alpha/rich.md", "status=done");
    let got = fs::read_to_string(b.join("projects/alpha/rich.md")).unwrap();
    assert_eq!(got, RICH.replace("status: active", "status: done"));
}

#[test]
fn replacing_a_block_list_replaces_its_continuation_lines() {
    let out = edit_frontmatter(
        RICH,
        &[("projects", FmValue::List(vec!["x".into(), "y z".into()]))],
    );
    assert_eq!(
        out,
        RICH.replace("projects:\n  - one\n  - two\n", "projects: [x, y z]\n")
    );
}

#[test]
fn a_new_key_is_appended_before_the_closing_fence() {
    let sb = Sandbox::new();
    let b = brain_with(&sb, "projects/alpha/rich.md", RICH.as_bytes());
    fm_set(&sb, &b, "projects/alpha/rich.md", "valid_to=2026-10-23");
    let got = fs::read_to_string(b.join("projects/alpha/rich.md")).unwrap();
    assert_eq!(
        got,
        RICH.replace("  - two\n---\n", "  - two\nvalid_to: 2026-10-23\n---\n")
    );
}

#[test]
fn a_file_without_frontmatter_gains_a_block_and_keeps_its_body() {
    let sb = Sandbox::new();
    let b = brain_with(&sb, "projects/alpha/plain.md", b"# Plain\n\nText\n");
    fm_set(&sb, &b, "projects/alpha/plain.md", "status=done");
    let got = fs::read_to_string(b.join("projects/alpha/plain.md")).unwrap();
    assert_eq!(got, "---\nstatus: done\n---\n# Plain\n\nText\n");
}

#[test]
fn a_crlf_file_keeps_every_other_byte_and_the_new_line_is_crlf() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    let before = fs::read(b.join("projects/alpha/crlf.md")).unwrap();
    fm_set(&sb, &b, "projects/alpha/crlf.md", "status=stale");
    let after = fs::read(b.join("projects/alpha/crlf.md")).unwrap();
    let want = String::from_utf8(before.clone())
        .unwrap()
        .replace("status: done", "status: stale");
    assert_eq!(String::from_utf8(after).unwrap(), want);

    fm_set(&sb, &b, "projects/alpha/crlf.md", "valid_to=2026-10-23");
    let after = fs::read_to_string(b.join("projects/alpha/crlf.md")).unwrap();
    assert_eq!(
        after,
        want.replace(
            "status: stale\r\n",
            "status: stale\r\nvalid_to: 2026-10-23\r\n"
        )
    );
}

#[test]
fn a_crlf_file_without_frontmatter_gains_a_crlf_block() {
    let out = edit_frontmatter(
        "# T\r\nbody\r\n",
        &[("status", FmValue::Str("done".into()))],
    );
    assert_eq!(out, "---\r\nstatus: done\r\n---\r\n# T\r\nbody\r\n");
}

#[test]
fn values_that_yaml_would_misread_are_quoted() {
    let out = edit_frontmatter(
        "---\na: 1\n---\n",
        &[
            ("b", FmValue::Str("yes".into())),
            ("c", FmValue::Str("[[projects/x/new]]".into())),
            ("d", FmValue::Bool(true)),
        ],
    );
    assert_eq!(
        out,
        "---\na: 1\nb: \"yes\"\nc: \"[[projects/x/new]]\"\nd: true\n---\n"
    );
}

#[test]
fn fm_set_is_hidden_from_help() {
    let sb = Sandbox::new();
    let out = sb
        .cmd()
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(!String::from_utf8(out).unwrap().contains("fm-set"));
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn a_planted_temp_symlink_is_never_written_through() {
        let sb = Sandbox::new();
        let b = sb.fixture_brain("basic");
        let victim = sb.root.join("victim.txt");
        fs::write(&victim, "keep me\n").unwrap();
        symlink(&victim, b.join("projects/alpha/status.md.fbrain-tmp")).unwrap();
        fm_set(&sb, &b, "projects/alpha/status.md", "status=stale");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep me\n");
        let got = fs::read_to_string(b.join("projects/alpha/status.md")).unwrap();
        assert!(got.contains("status: stale\n"));
    }

    #[test]
    fn fm_set_refuses_a_symlinked_note() {
        let sb = Sandbox::new();
        let b = sb.fixture_brain("basic");
        let real = sb.root.join("real.md");
        fs::write(&real, "---\nstatus: a\n---\n").unwrap();
        symlink(&real, b.join("projects/alpha/linked.md")).unwrap();
        sb.cmd()
            .arg("--brain")
            .arg(&b)
            .args([
                "fm-set",
                "--yes-i-am-faisal",
                "projects/alpha/linked.md",
                "status=b",
            ])
            .assert()
            .failure();
        assert_eq!(fs::read_to_string(&real).unwrap(), "---\nstatus: a\n---\n");
    }

    #[test]
    fn fm_set_keeps_the_file_mode() {
        let sb = Sandbox::new();
        let b = sb.fixture_brain("basic");
        let f = b.join("projects/alpha/status.md");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
        fm_set(&sb, &b, "projects/alpha/status.md", "status=stale");
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
