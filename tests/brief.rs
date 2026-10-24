mod common;

use common::Sandbox;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

fn brain(sb: &Sandbox) -> PathBuf {
    sb.fixture_brain("basic")
}

/// A directory that the fixture brain maps to project `alpha`.
fn map_alpha(sb: &Sandbox, brain: &Path, repos: &str) {
    let mut toml = fs::read_to_string(brain.join(".fbrain.toml")).unwrap();
    toml.push_str(&format!("\n[projects.alpha]\nrepos = [{repos}]\n"));
    fs::write(brain.join(".fbrain.toml"), toml).unwrap();
    let _ = sb;
}

fn mk(sb: &Sandbox, rel: &str) -> PathBuf {
    let d = sb.root.join(rel);
    fs::create_dir_all(&d).unwrap();
    d
}

fn run(sb: &Sandbox, brain: &Path, args: &[&str], stdin: Option<&str>) -> String {
    let mut c = sb.cmd();
    c.env("FBRAIN_BRAIN", brain).arg("brief").args(args);
    if let Some(s) = stdin {
        c.write_stdin(s);
    }
    let out = c.assert().success().get_output().clone();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim_end()
        .to_string()
}

fn mapped_setup() -> (Sandbox, PathBuf, PathBuf) {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let repo = mk(&sb, "repos/alpha-repo");
    map_alpha(&sb, &b, &format!("{:?}", repo.to_str().unwrap()));
    (sb, b, repo)
}

#[test]
fn mapped_cwd_gets_profile_project_brief_and_rules() {
    let (sb, b, repo) = mapped_setup();
    let out = run(
        &sb,
        &b,
        &["--harness", "opencode-v1", "--cwd", repo.to_str().unwrap()],
        None,
    );
    assert!(out.starts_with("## fbrain memory (brain: basic)"), "{out}");
    assert!(out.contains("Founder profile."), "{out}");
    assert!(out.contains("### Project: alpha"), "{out}");
    assert!(out.contains("Alpha brief."), "{out}");
    assert!(
        out.contains("Do not answer questions about Faisal's projects"),
        "{out}"
    );
}

#[test]
fn unmapped_cwd_lists_projects() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    fs::write(
        b.join("projects/alpha/BRIEF.md"),
        "---\ndescription: The alpha product\n---\n# Alpha\n",
    )
    .unwrap();
    let out = run(
        &sb,
        &b,
        &[
            "--harness",
            "opencode-v1",
            "--cwd",
            sb.root.to_str().unwrap(),
        ],
        None,
    );
    assert!(out.contains("### Projects"), "{out}");
    assert!(out.contains("- alpha: The alpha product"), "{out}");
    assert!(!out.contains("### Project:"), "{out}");
}

#[test]
fn trailing_star_matches_a_suffix_of_the_last_segment() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let cwd = mk(&sb, "work/adv-backend");
    let pat = format!("{:?}", format!("{}/work/adv-*", sb.root.display()));
    map_alpha(&sb, &b, &pat);
    let out = run(
        &sb,
        &b,
        &["--harness", "opencode-v1", "--cwd", cwd.to_str().unwrap()],
        None,
    );
    assert!(out.contains("### Project: alpha"), "{out}");
    let other = mk(&sb, "work/other");
    let out = run(
        &sb,
        &b,
        &["--harness", "opencode-v1", "--cwd", other.to_str().unwrap()],
        None,
    );
    assert!(out.contains("### Projects"), "{out}");
}

#[test]
fn claude_code_wraps_the_same_payload_in_hook_json() {
    let (sb, b, repo) = mapped_setup();
    let cwd = repo.to_str().unwrap();
    let plain = run(&sb, &b, &["--harness", "opencode-v1", "--cwd", cwd], None);
    for h in ["claude-code", "codex"] {
        let out = run(
            &sb,
            &b,
            &["--harness", h, "--cwd", cwd],
            Some(r#"{"hook_event_name":"SubagentStart"}"#),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SubagentStart");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], plain.as_str());
    }
    let out = run(&sb, &b, &["--harness", "claude-code", "--cwd", cwd], None);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
}

#[test]
fn a_long_brief_is_truncated_to_the_cap() {
    let (sb, b, repo) = mapped_setup();
    fs::write(b.join("projects/alpha/BRIEF.md"), "x".repeat(20_000)).unwrap();
    let out = run(
        &sb,
        &b,
        &["--harness", "opencode-v1", "--cwd", repo.to_str().unwrap()],
        None,
    );
    assert!(out.chars().count() <= 8_800, "{}", out.chars().count());
    assert!(out.contains("[truncated: run fbrain get projects/alpha/BRIEF.md]"));
    assert!(out.contains("Do not approve or reject proposals"));
}

#[test]
fn size_is_counted_in_characters_not_bytes() {
    let (sb, b, repo) = mapped_setup();
    let cwd = repo.to_str().unwrap();
    fs::write(b.join("projects/alpha/BRIEF.md"), "ع".repeat(12_000)).unwrap();
    let out = run(&sb, &b, &["--harness", "opencode-v1", "--cwd", cwd], None);
    assert!(out.chars().count() <= 8_800, "{}", out.chars().count());
    assert!(out.contains("[truncated: run fbrain get projects/alpha/BRIEF.md]"));
    fs::write(b.join("projects/alpha/BRIEF.md"), "ع".repeat(6_000)).unwrap();
    let out = run(&sb, &b, &["--harness", "opencode-v1", "--cwd", cwd], None);
    assert!(!out.contains("[truncated"), "a 6,000 character brief fits");
}

#[test]
fn a_long_me_is_truncated_after_the_brief() {
    let (sb, b, repo) = mapped_setup();
    fs::write(b.join("ME.md"), "y".repeat(20_000)).unwrap();
    fs::write(b.join("projects/alpha/BRIEF.md"), "z".repeat(20_000)).unwrap();
    let out = run(
        &sb,
        &b,
        &["--harness", "opencode-v1", "--cwd", repo.to_str().unwrap()],
        None,
    );
    assert!(out.chars().count() <= 8_800, "{}", out.chars().count());
    assert!(out.contains("[truncated: run fbrain get projects/alpha/BRIEF.md]"));
    assert!(out.contains("[truncated: run fbrain get ME.md]"));
}

#[test]
fn a_broken_brain_prints_a_notice_and_exits_zero() {
    let sb = Sandbox::new();
    let bad = mk(&sb, "not-a-brain");
    let want = format!(
        "fbrain memory not loaded: not a brain: {} has no .fbrain.toml. Run fbrain doctor.",
        bad.display()
    );
    assert_eq!(run(&sb, &bad, &["--harness", "opencode-v2"], None), want);
    let out = run(&sb, &bad, &["--harness", "claude-code"], None);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["additionalContext"], want.as_str());
}

#[test]
fn pending_proposals_are_counted() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    fs::create_dir_all(b.join("inbox")).unwrap();
    for n in ["a", "b"] {
        fs::write(b.join(format!("inbox/{n}.md")), "x").unwrap();
    }
    let out = run(&sb, &b, &["--harness", "opencode-v1"], None);
    assert!(
        out.contains("Pending proposals: 2 (run fbrain review)"),
        "{out}"
    );
    let sb2 = Sandbox::new();
    let out = run(&sb2, &brain(&sb2), &["--harness", "opencode-v1"], None);
    assert!(!out.contains("Pending proposals"), "{out}");
}

#[test]
fn output_has_no_em_or_en_dashes_and_no_review_override() {
    let (sb, b, repo) = mapped_setup();
    let out = run(
        &sb,
        &b,
        &["--harness", "opencode-v1", "--cwd", repo.to_str().unwrap()],
        None,
    );
    assert!(!out.contains('\u{2013}') && !out.contains('\u{2014}'));
    assert!(out.contains("Do not approve or reject proposals"));
    assert!(!out.contains("--yes-i-am-faisal"));
    assert!(!out.contains("fm-set"));
}

#[test]
fn cwd_comes_from_stdin_and_the_flag_overrides_it() {
    let (sb, b, repo) = mapped_setup();
    let other = mk(&sb, "elsewhere");
    let stdin = format!(
        r#"{{"hook_event_name":"SessionStart","cwd":{:?}}}"#,
        repo.to_str().unwrap()
    );
    let mut c = sb.cmd();
    c.env("FBRAIN_BRAIN", &b)
        .current_dir(&other)
        .args(["brief", "--harness", "claude-code"])
        .write_stdin(stdin.clone());
    let out = String::from_utf8(c.assert().success().get_output().stdout.clone()).unwrap();
    assert!(out.contains("### Project: alpha"), "{out}");
    let mut c = sb.cmd();
    c.env("FBRAIN_BRAIN", &b)
        .current_dir(&other)
        .args([
            "brief",
            "--harness",
            "claude-code",
            "--cwd",
            other.to_str().unwrap(),
        ])
        .write_stdin(stdin);
    let out = String::from_utf8(c.assert().success().get_output().stdout.clone()).unwrap();
    assert!(out.contains("### Projects"), "{out}");
}

#[test]
fn an_open_stdin_that_never_closes_does_not_hang() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let mut cmd = sb.std_cmd();
    cmd.env("FBRAIN_BRAIN", &b)
        .args(["brief", "--harness", "claude-code"]);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut held = child.stdin.take().unwrap();
    let t = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break Some(s);
        }
        if t.elapsed() > Duration::from_secs(2) {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let _ = held.flush();
    assert!(status.expect("brief hung on an open stdin").success());
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("### Projects"), "{text}");
}

#[test]
fn brief_starts_no_resident_process() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    run(&sb, &b, &["--harness", "opencode-v1"], None);
    assert!(sb.server_pid().is_none());
    let left = fs::read_dir(sb.runtime_dir())
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(left, 0, "no socket, token or pid file");
}
