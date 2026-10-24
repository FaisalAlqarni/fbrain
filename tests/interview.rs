mod common;

use common::Sandbox;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Std;

fn git(dir: &Path, args: &[&str], date: &str) {
    let out = Std::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "false")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Writes the files and commits them with the given date.
fn commit_files(dir: &Path, date: &str, files: &[(&str, &str)]) {
    for (rel, text) in files {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    git(dir, &["add", "-A"], date);
    git(dir, &["commit", "-q", "-m", "fixture"], date);
}

fn new_brain(sb: &Sandbox) -> PathBuf {
    let b = sb.root.join("brain");
    fs::create_dir_all(&b).unwrap();
    git(&b, &["init", "-q"], "2026-10-01T10:00:00Z");
    commit_files(
        &b,
        "2026-10-01T10:00:00Z",
        &[(".fbrain.toml", "name = \"t\"\n"), ("me/x.md", "# X\n")],
    );
    b
}

fn interview(sb: &Sandbox, b: &Path) -> assert_cmd::Command {
    let mut c = sb.cmd();
    c.env("FBRAIN_TEST_PROPOSAL_TS", "20261201T120000Z")
        .arg("--brain")
        .arg(b)
        .arg("interview");
    c
}

fn inbox(b: &Path) -> Vec<String> {
    let mut v: Vec<_> = fs::read_dir(b.join("inbox"))
        .map(|d| {
            d.map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn json(out: Vec<u8>) -> Value {
    serde_json::from_slice(&out).unwrap()
}

const SECTIONS: [&str; 9] = [
    "What it is",
    "Customers",
    "Modules",
    "Stack",
    "Environments",
    "Status",
    "Goals",
    "Risks",
    "Related",
];

const THIN: &str = "---\ndescription: Alpha\n---\n# alpha\n\n## What it is\n\nA thing.\n\n## Customers\n\n## Modules\n\napp, lib\n\n## Stack\n\nRuby\n\n## Environments\n\nHeroku\n\n## Status\n\nLive\n\n## Goals\n\nGrow\n\n## Risks\n\nNone\n\n## Related\n\nnone\n";

#[test]
fn a_new_project_gets_one_add_proposal_with_all_nine_sections() {
    let sb = Sandbox::new();
    let b = new_brain(&sb);
    let f = sb.root.join("a.toml");
    let mut t = String::from("[projects.beta]\n");
    for (k, v) in [
        ("what", "A new thing"),
        ("customers", "Cafes"),
        ("modules", "api"),
        ("stack", "Rust"),
        ("environments", "Fly"),
        ("status", "Early"),
        ("goals", "Launch"),
        ("risks", "Money"),
        ("related_repos", "none"),
    ] {
        t += &format!("{k} = \"{v}\"\n");
    }
    fs::write(&f, t).unwrap();
    interview(&sb, &b)
        .args(["beta", "--today", "2026-12-01", "--answers"])
        .arg(&f)
        .assert()
        .success();
    let files = inbox(&b);
    assert_eq!(files.len(), 1, "{files:?}");
    let p = &files[0];
    assert!(p.contains("op: add"), "{p}");
    assert!(p.contains("target: projects/beta/BRIEF.md"), "{p}");
    assert!(p.contains("proposed_by: interview"), "{p}");
    for (h, a) in SECTIONS.iter().zip([
        "A new thing",
        "Cafes",
        "api",
        "Rust",
        "Fly",
        "Early",
        "Launch",
        "Money",
        "none",
    ]) {
        assert!(p.contains(&format!("## {h}\n\n{a}\n")), "{h}: {p}");
    }
}

#[test]
fn a_thin_project_is_asked_only_its_empty_section_and_the_rest_stays_identical() {
    let sb = Sandbox::new();
    let b = new_brain(&sb);
    commit_files(
        &b,
        "2026-10-02T10:00:00Z",
        &[("projects/alpha/BRIEF.md", THIN)],
    );
    let out = interview(&sb, &b)
        .args(["alpha", "--print-questions", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json(out);
    let ids: Vec<_> = v["questions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["customers"]);

    let f = sb.root.join("a.toml");
    fs::write(&f, "[projects.alpha]\ncustomers = \"Cafes\"\n").unwrap();
    interview(&sb, &b)
        .args(["alpha", "--today", "2026-12-01", "--answers"])
        .arg(&f)
        .assert()
        .success();
    let files = inbox(&b);
    assert_eq!(files.len(), 1, "{files:?}");
    let p = &files[0];
    assert!(p.contains("op: update"), "{p}");
    assert!(p.contains("target: projects/alpha/BRIEF.md"), "{p}");
    let body = p.split("\n---\n").nth(1).unwrap();
    let want = THIN
        .split("\n---\n")
        .nth(1)
        .unwrap()
        .replace("## Customers\n\n", "## Customers\n\nCafes\n\n");
    assert_eq!(body, want);
}

/// Six notes, one per review kind, as of 2026-12-01.
fn review_brain(sb: &Sandbox) -> PathBuf {
    let b = new_brain(sb);
    commit_files(
        &b,
        "2026-10-01T10:00:00Z",
        &[("projects/alpha/status.md", "# Status\n\nOld.\n")],
    );
    commit_files(
        &b,
        "2026-11-01T10:00:00Z",
        &[(
            "projects/alpha/derived.md",
            "---\nderived_from: [\"[[projects/alpha/source]]\"]\n---\n# Derived\n",
        )],
    );
    commit_files(
        &b,
        "2026-11-20T10:00:00Z",
        &[
            ("projects/alpha/source.md", "# Source\n\nChanged.\n"),
            (
                "projects/alpha/stale.md",
                "---\nstatus: stale\n---\n# Stale\n",
            ),
            (
                "projects/alpha/old-fact.md",
                "---\nvalid_to: 2026-06-01\n---\n# Old fact\n\nThe price is 5.\n",
            ),
            (
                "projects/alpha/BRIEF.md",
                "# alpha\n\n## Open questions\n\n- [ ] Who signs consent?\n- [x] Done already\n",
            ),
            ("ME.md", "# Me\n\n## Goals\n\nShip v1\n"),
        ],
    );
    b
}

#[test]
fn review_lists_one_item_of_each_of_the_six_kinds() {
    let sb = Sandbox::new();
    let b = review_brain(&sb);
    let out = interview(&sb, &b)
        .args([
            "--review",
            "--today",
            "2026-12-01",
            "--print-items",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json(out);
    let items: Vec<_> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["kind"].as_str().unwrap(), i["id"].as_str().unwrap()))
        .collect();
    assert_eq!(
        items,
        [
            ("Stale", "Stale:projects/alpha/stale.md"),
            ("Expired", "Expired:projects/alpha/old-fact.md"),
            ("DerivedStale", "DerivedStale:projects/alpha/derived.md"),
            ("StatusOld", "StatusOld:projects/alpha/status.md"),
            ("OpenQuestion", "OpenQuestion:projects/alpha/BRIEF.md:1"),
            ("Goal", "Goal:ME.md"),
        ]
    );
}

fn answer_review(sb: &Sandbox, b: &Path, toml: &str) {
    let f = sb.root.join("r.toml");
    fs::write(&f, toml).unwrap();
    interview(sb, b)
        .args(["--review", "--today", "2026-12-01", "--answers"])
        .arg(&f)
        .assert()
        .success();
}

#[test]
fn review_answers_become_a_stale_proposal_and_a_checked_open_question() {
    let sb = Sandbox::new();
    let b = review_brain(&sb);
    answer_review(
        &sb,
        &b,
        "\"Expired:projects/alpha/old-fact.md\" = \"stale\"\n\"OpenQuestion:projects/alpha/BRIEF.md:1\" = \"answer Yes, approved 2026-10-23\"\n",
    );
    let files = inbox(&b);
    assert_eq!(files.len(), 2, "{files:?}");
    let stale = files
        .iter()
        .find(|p| p.contains("target: projects/alpha/old-fact.md"))
        .unwrap();
    assert!(stale.contains("op: update"), "{stale}");
    assert!(stale.contains("set_status: stale"), "{stale}");
    assert!(stale.contains("proposed_by: interview"), "{stale}");
    assert!(stale.ends_with("---\n"), "empty body: {stale}");
    let q = files
        .iter()
        .find(|p| p.contains("target: projects/alpha/BRIEF.md"))
        .unwrap();
    assert!(
        q.contains(
            "- [x] Who signs consent?\n  Answer: Yes, approved 2026-10-23\n- [x] Done already\n"
        ),
        "{q}"
    );
}

#[test]
fn keep_and_unanswered_items_produce_no_proposal() {
    let sb = Sandbox::new();
    let b = review_brain(&sb);
    answer_review(&sb, &b, "\"Stale:projects/alpha/stale.md\" = \"keep\"\n");
    assert!(inbox(&b).is_empty());
}

#[test]
fn approving_a_set_status_proposal_changes_only_the_status() {
    let sb = Sandbox::new();
    let b = review_brain(&sb);
    answer_review(
        &sb,
        &b,
        "\"Expired:projects/alpha/old-fact.md\" = \"stale\"\n",
    );
    let id = fs::read_dir(b.join("inbox"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".md")
        .to_string();
    git(&b, &["add", "-A"], "2026-12-01T10:00:00Z");
    git(
        &b,
        &["commit", "-q", "-m", "proposals"],
        "2026-12-01T10:00:00Z",
    );
    sb.cmd()
        .arg("--brain")
        .arg(&b)
        .args(["review", &id, "--approve", "--yes-i-am-faisal"])
        .assert()
        .success();
    let n = fs::read_to_string(b.join("projects/alpha/old-fact.md")).unwrap();
    assert!(n.contains("status: stale"), "{n}");
    assert!(n.contains("valid_to: 2026-06-01"), "{n}");
    assert!(n.ends_with("# Old fact\n\nThe price is 5.\n"), "{n}");
}

#[test]
fn a_set_status_outside_the_status_values_is_refused() {
    let sb = Sandbox::new();
    let b = review_brain(&sb);
    fs::create_dir_all(b.join("inbox")).unwrap();
    fs::write(
        b.join("inbox/20261201T120000Z-x.md"),
        "---\nop: update\ntarget: projects/alpha/old-fact.md\nproposed_by: interview\nsource: s\nrationale: r\nconflicts: []\nconflicts_protected: false\nset_status: bogus\n---\n",
    )
    .unwrap();
    git(&b, &["add", "-A"], "2026-12-01T10:00:00Z");
    git(
        &b,
        &["commit", "-q", "-m", "proposals"],
        "2026-12-01T10:00:00Z",
    );
    sb.cmd()
        .arg("--brain")
        .arg(&b)
        .args([
            "review",
            "20261201T120000Z-x",
            "--approve",
            "--yes-i-am-faisal",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("invalid set_status"));
    let n = fs::read_to_string(b.join("projects/alpha/old-fact.md")).unwrap();
    assert!(!n.contains("bogus"), "{n}");
}

#[test]
fn two_answers_on_one_note_make_one_proposal_that_applies_both() {
    let sb = Sandbox::new();
    let b = review_brain(&sb);
    commit_files(
        &b,
        "2026-11-21T10:00:00Z",
        &[(
            "projects/alpha/BRIEF.md",
            "# alpha\n\n## Open questions\n\n- [ ] Who signs consent?\n- [x] Done already\n- [ ] Which region?\n",
        )],
    );
    answer_review(
        &sb,
        &b,
        "\"OpenQuestion:projects/alpha/BRIEF.md:1\" = \"answer Yes\"\n\"OpenQuestion:projects/alpha/BRIEF.md:2\" = \"answer EU\"\n",
    );
    let files = inbox(&b);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(
        files[0].contains("OpenQuestion:projects/alpha/BRIEF.md:1"),
        "{}",
        files[0]
    );
    assert!(
        files[0].contains("OpenQuestion:projects/alpha/BRIEF.md:2"),
        "{}",
        files[0]
    );
    let id = fs::read_dir(b.join("inbox"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".md")
        .to_string();
    git(&b, &["add", "-A"], "2026-12-01T10:00:00Z");
    git(
        &b,
        &["commit", "-q", "-m", "proposals"],
        "2026-12-01T10:00:00Z",
    );
    sb.cmd()
        .arg("--brain")
        .arg(&b)
        .args(["review", &id, "--approve", "--yes-i-am-faisal"])
        .assert()
        .success();
    let n = fs::read_to_string(b.join("projects/alpha/BRIEF.md")).unwrap();
    assert!(
        n.ends_with("- [x] Who signs consent?\n  Answer: Yes\n- [x] Done already\n- [x] Which region?\n  Answer: EU\n"),
        "{n}"
    );
}
