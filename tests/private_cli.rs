//! `private: true` means never served to agents. The CLI is the agents' other door, so it hides
//! private and unparseable notes unless Faisal passes `--private`.
#![cfg(unix)]
mod common;

use common::Sandbox;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

const SECRET: &str = "---\nprivate: true\nstatus: active\n---\n# Salary record\n\nZebrafish payroll consent figures.\n";

fn brain(sb: &Sandbox) -> PathBuf {
    let b = sb.root.join("brain");
    fs::create_dir_all(b.join("notes")).unwrap();
    fs::write(b.join(".fbrain.toml"), "name = \"t\"\n").unwrap();
    fs::write(b.join("notes/salary.md"), SECRET).unwrap();
    fs::write(
        b.join("notes/broken.md"),
        "---\nprivate: [oops\n---\n# Broken\n\nZebrafish payroll broken.\n",
    )
    .unwrap();
    fs::write(
        b.join("notes/open.md"),
        "---\nstatus: active\n---\n# Open\n\nZebrafish payroll is public. See [[notes/salary]].\n",
    )
    .unwrap();
    b
}

fn run(sb: &Sandbox, b: &PathBuf, args: &[&str]) -> (bool, String, String) {
    let mut c = sb.std_cmd();
    c.arg("--brain").arg(b).arg("--json").args(args);
    // `--private` is for Faisal at a terminal; everything else runs as an agent would.
    let o = if args.contains(&"--private") {
        common::owner_output(c)
    } else {
        c.output().unwrap()
    };
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

fn hit_paths(stdout: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(stdout).unwrap();
    v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["rel_path"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn cli_search_hides_private_and_unparseable_notes_unless_asked() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let (ok, out, _) = run(&sb, &b, &["search", "zebrafish payroll"]);
    assert!(ok);
    assert_eq!(hit_paths(&out), ["notes/open.md"], "{out}");
    let (ok, out, _) = run(&sb, &b, &["search", "--private", "zebrafish payroll"]);
    assert!(ok);
    let got = hit_paths(&out);
    assert!(got.contains(&"notes/salary.md".to_string()), "{out}");
    assert!(got.contains(&"notes/broken.md".to_string()), "{out}");
}

#[test]
fn cli_get_of_a_private_note_says_not_found_unless_asked() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let (ok, out, err) = run(&sb, &b, &["get", "notes/salary.md"]);
    assert!(!ok, "{out}");
    assert!(err.contains("note not found: notes/salary.md"), "{err}");
    assert!(!out.contains("Zebrafish"), "{out}");
    let (ok, out, _) = run(&sb, &b, &["get", "--private", "notes/salary.md"]);
    assert!(ok);
    assert!(out.contains("Salary record"), "{out}");
}

#[test]
fn cli_get_links_leaves_out_a_private_neighbour_unless_asked() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let (ok, out, _) = run(&sb, &b, &["get", "--links", "notes/open.md"]);
    assert!(ok);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["outgoing"].as_array().unwrap().len(), 0, "{out}");
    let (ok, out, _) = run(&sb, &b, &["get", "--links", "--private", "notes/open.md"]);
    assert!(ok);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["outgoing"][0]["resolved"], "notes/salary.md", "{out}");
}

#[test]
fn cli_propose_lists_no_private_conflict_candidate() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let mut c = sb.cmd();
    let out = c
        .arg("--brain")
        .arg(&b)
        .args([
            "--json",
            "propose",
            "--op",
            "add",
            "--target",
            "notes/dup.md",
        ])
        .args(["--rationale", "r", "--source", "s"])
        .write_stdin("Zebrafish payroll consent figures.\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(!out.contains("salary"), "{out}");
}

#[test]
fn interview_review_items_leave_out_private_notes() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    fs::write(
        b.join("notes/secret-stale.md"),
        "---\nprivate: true\nstatus: stale\n---\n# Secret stale\n",
    )
    .unwrap();
    fs::write(
        b.join("notes/public-stale.md"),
        "---\nstatus: stale\n---\n# Public stale\n",
    )
    .unwrap();
    let (ok, out, _) = run(
        &sb,
        &b,
        &[
            "interview",
            "--review",
            "--today",
            "2026-12-01",
            "--print-items",
        ],
    );
    assert!(ok, "{out}");
    assert!(out.contains("public-stale.md"), "{out}");
    assert!(!out.contains("secret-stale.md"), "{out}");
}

#[test]
fn the_generated_index_lists_no_private_or_unparseable_note() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    fs::create_dir_all(b.join("projects/alpha")).unwrap();
    fs::create_dir_all(b.join("me")).unwrap();
    fs::write(b.join("projects/alpha/salary.md"), SECRET).unwrap();
    fs::write(
        b.join("me/broken.md"),
        "---\nprivate: [oops\n---\n# Broken profile\n",
    )
    .unwrap();
    fs::write(b.join("projects/alpha/open.md"), "# Open fact\n").unwrap();
    sb.cmd()
        .arg("--brain")
        .arg(&b)
        .args(["lint", "--fix", "--yes-i-am-faisal"])
        .assert()
        .success();
    let index = fs::read_to_string(b.join("INDEX.md")).unwrap();
    assert!(index.contains("Open fact"), "{index}");
    assert!(!index.contains("Salary"), "{index}");
    assert!(!index.contains("Broken"), "{index}");
}

/// A brain where every kind of lint finding touches a hidden note.
fn lint_brain(sb: &Sandbox) -> PathBuf {
    let b = sb.root.join("lintbrain");
    for d in ["a", "b", "notes"] {
        fs::create_dir_all(b.join(d)).unwrap();
    }
    fs::write(b.join(".fbrain.toml"), "name = \"t\"\n").unwrap();
    // Private note with its own findings and an alias a public note links through.
    fs::write(
        b.join("secret.md"),
        "---\nprivate: true\naliases: [Vault]\n---\n# Secret\n\nSee [[nowhere-secret]].\n",
    )
    .unwrap();
    fs::write(b.join("a/dup.md"), "---\nprivate: true\n---\n# Dup\n").unwrap();
    fs::write(b.join("b/dup.md"), "# Dup\n").unwrap();
    fs::write(
        b.join("notes/pub.md"),
        "# Pub\n\nSee [[dup]] and [[Vault]] and [[missing-public]].\n",
    )
    .unwrap();
    fs::write(b.join("broken.md"), "---\nprivate: [oops\n---\n# Broken\n").unwrap();
    b
}

fn lint_json(sb: &Sandbox, b: &PathBuf, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["lint"];
    all.extend(args);
    let (ok, out, err) = run(sb, b, &all);
    assert!(ok, "{err}");
    serde_json::from_str::<Value>(&out)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn plain_lint_reports_nothing_about_hidden_notes() {
    let sb = Sandbox::new();
    let b = lint_brain(&sb);
    let issues = lint_json(&sb, &b, &[]);
    let all = serde_json::to_string(&issues).unwrap();
    for leak in ["secret.md", "a/dup.md", "Vault", "nowhere-secret", "oops"] {
        assert!(!all.contains(leak), "{leak} leaked: {all}");
    }
    // The public note's own finding stays.
    assert!(
        issues
            .iter()
            .any(|i| i["rule"] == "L001" && i["rel_path"] == "notes/pub.md"),
        "{all}"
    );
    // An unparseable note is still reported, with no parser text.
    let l009: Vec<_> = issues.iter().filter(|i| i["rule"] == "L009").collect();
    assert_eq!(l009.len(), 1, "{all}");
    assert_eq!(l009[0]["detail"], "frontmatter does not parse");
}

#[test]
fn the_owner_sees_every_lint_finding_with_private() {
    let sb = Sandbox::new();
    let b = lint_brain(&sb);
    let issues = lint_json(&sb, &b, &["--private"]);
    let all = serde_json::to_string(&issues).unwrap();
    assert!(
        all.contains("secret.md") && all.contains("a/dup.md"),
        "{all}"
    );
    let l009 = issues.iter().find(|i| i["rule"] == "L009").unwrap();
    assert_ne!(l009["detail"], "frontmatter does not parse");
}
