//! `lint --fix` rewrites notes, so it is guarded like review and commits what it writes. A git
//! failure other than "not a git repository" stops every writer before its first write.
mod common;

use common::Sandbox;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Std;

const FIXABLE: &str = "---\nstatus: superseded\n---\n# Old\n\nGone.\n";

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Std::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap()
}

fn brain(sb: &Sandbox) -> PathBuf {
    let b = sb.root.join("brain");
    fs::create_dir_all(&b).unwrap();
    git(&b, &["init", "-q"]);
    fs::write(b.join(".fbrain.toml"), "name = \"t\"\n").unwrap();
    fs::write(b.join("old.md"), FIXABLE).unwrap();
    git(&b, &["add", "-A"]);
    git(&b, &["commit", "-q", "-m", "fbrain: seed"]);
    b
}

/// Makes `git rev-parse` fail for a reason other than "not a git repository".
fn break_git(b: &Path) {
    let cfg = b.join(".git/config");
    let mut t = fs::read_to_string(&cfg).unwrap();
    t.push_str("[extensions]\n\tbogus = true\n");
    t = t.replace("repositoryformatversion = 0", "repositoryformatversion = 1");
    fs::write(cfg, t).unwrap();
}

#[test]
fn lint_fix_is_refused_for_an_agent_and_plain_lint_is_not() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let out = sb
        .cmd()
        .env("FBRAIN_HARNESS", "claude-code")
        .arg("--brain")
        .arg(&b)
        .args(["lint", "--fix"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert_eq!(fs::read_to_string(b.join("old.md")).unwrap(), FIXABLE);
    assert!(!b.join("INDEX.md").exists());
    sb.cmd()
        .env("FBRAIN_HARNESS", "claude-code")
        .arg("--brain")
        .arg(&b)
        .arg("lint")
        .assert()
        .success();
}

#[test]
fn lint_fix_with_the_override_commits_what_it_writes() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    sb.cmd()
        .env("FBRAIN_HARNESS", "claude-code")
        .arg("--brain")
        .arg(&b)
        .args(["lint", "--fix", "--yes-i-am-faisal"])
        .assert()
        .success();
    assert!(fs::read_to_string(b.join("old.md"))
        .unwrap()
        .contains("valid_to"));
    assert!(b.join("INDEX.md").exists());
    let subject = git(&b, &["log", "-1", "--format=%s"]);
    assert!(subject.starts_with("fbrain: lint --fix"), "{subject}");
    assert_eq!(git(&b, &["status", "--porcelain"]), "", "left uncommitted");
}

#[test]
fn a_git_failure_that_is_not_a_missing_repo_stops_lint_fix_before_it_writes() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    break_git(&b);
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .args(["lint", "--fix", "--yes-i-am-faisal"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("bogus"), "git's own message is missing: {err}");
    assert_eq!(fs::read_to_string(b.join("old.md")).unwrap(), FIXABLE);
    assert!(!b.join("INDEX.md").exists());
}

#[test]
fn a_git_failure_that_is_not_a_missing_repo_stops_fm_set_before_it_writes() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    break_git(&b);
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .args(["fm-set", "--yes-i-am-faisal", "old", "protected=true"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("bogus"));
    assert_eq!(fs::read_to_string(b.join("old.md")).unwrap(), FIXABLE);
}

#[test]
fn a_git_failure_that_is_not_a_missing_repo_stops_review_before_it_writes() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    sb.cmd()
        .arg("--brain")
        .arg(&b)
        .args(["propose", "--op", "add", "--target", "new.md"])
        .args(["--rationale", "r", "--source", "s"])
        .write_stdin("# New\n\nText.\n")
        .assert()
        .success();
    break_git(&b);
    let inbox: Vec<_> = fs::read_dir(b.join("inbox")).unwrap().flatten().collect();
    let id = inbox[0]
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".md")
        .to_string();
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .args(["review", &id, "--approve", "--yes-i-am-faisal"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("bogus"));
    assert!(!b.join("new.md").exists(), "wrote before the git check");
    assert!(inbox[0].path().exists(), "the proposal was removed");
}

#[test]
fn a_brain_outside_git_still_gets_its_writes_with_a_warning() {
    let sb = Sandbox::new();
    let b = sb.root.join("plain");
    fs::create_dir_all(&b).unwrap();
    fs::write(b.join(".fbrain.toml"), "name = \"t\"\n").unwrap();
    fs::write(b.join("old.md"), FIXABLE).unwrap();
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .args(["lint", "--fix", "--yes-i-am-faisal"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(fs::read_to_string(b.join("old.md"))
        .unwrap()
        .contains("valid_to"));
}

#[test]
fn lint_fix_refuses_a_target_with_uncommitted_changes_and_writes_nothing() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let edited = format!("{FIXABLE}Owner edit, not committed yet.\n");
    fs::write(b.join("old.md"), &edited).unwrap();
    let head = git(&b, &["rev-parse", "HEAD"]);
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .args(["lint", "--fix", "--yes-i-am-faisal"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("old.md") && err.contains("uncommitted"),
        "{err}"
    );
    assert_eq!(fs::read_to_string(b.join("old.md")).unwrap(), edited);
    assert!(!b.join("INDEX.md").exists());
    assert_eq!(git(&b, &["rev-parse", "HEAD"]), head, "nothing committed");
}
