mod common;

use common::Sandbox;
use predicates::prelude::PredicateBooleanExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn search_brain(sb: &Sandbox) -> PathBuf {
    let dir = sb.fixture_brain("search");
    fs::write(dir.join(".fbrain.toml"), "name = \"search\"\n").unwrap();
    dir
}

fn snapshot_outside_inbox(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            if e.path() == dir.join("inbox") {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path(), fs::read(e.path()).unwrap());
            }
        }
    }
    out
}

fn propose(sb: &Sandbox, brain: &Path, args: &[&str], body: &str) -> assert_cmd::assert::Assert {
    let mut c = sb.cmd();
    c.env("FBRAIN_TEST_PROPOSAL_TS", "20261023T120000Z")
        .arg("--brain")
        .arg(brain)
        .arg("propose")
        .args(args)
        .write_stdin(body.to_string());
    c.assert()
}

fn inbox_files(brain: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(brain.join("inbox"))
        .map(|d| {
            d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn inbox_json(sb: &Sandbox, brain: &Path) -> Vec<Value> {
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(brain)
        .args(["--json", "inbox"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice::<Value>(&out)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

const RESTATED: &str = "Users give consent before we store a session. The opt-out path is a single toggle in settings, and consent records are kept for audit.";

#[test]
fn propose_add_writes_one_inbox_file_and_changes_nothing_else() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    let before = snapshot_outside_inbox(&b);
    propose(
        &sb,
        &b,
        &[
            "--op",
            "add",
            "--target",
            "projects/alpha/new.md",
            "--rationale",
            "r",
            "--source",
            "s",
            "--by",
            "codex",
        ],
        "# New\n\nSomething entirely fresh about zebras.\n",
    )
    .success();
    assert_eq!(before, snapshot_outside_inbox(&b));
    assert_eq!(inbox_files(&b), vec!["20261023T120000Z-new.md"]);
    let text = fs::read_to_string(b.join("inbox/20261023T120000Z-new.md")).unwrap();
    for line in [
        "op: add\n",
        "target: projects/alpha/new.md\n",
        "proposed_by: codex\n",
        "source: s\n",
        "rationale: r\n",
        "conflicts_protected: false\n",
    ] {
        assert!(text.contains(line), "{line:?} missing in {text}");
    }
    assert!(text.ends_with("---\n# New\n\nSomething entirely fresh about zebras.\n"));
}

#[test]
fn by_defaults_to_the_harness_variable_then_cli() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    let args = [
        "--op",
        "add",
        "--target",
        "projects/alpha/a.md",
        "--rationale",
        "r",
        "--source",
        "s",
    ];
    propose(&sb, &b, &args, "x one\n").success();
    let mut c = sb.cmd();
    c.env("FBRAIN_HARNESS", "opencode")
        .env("FBRAIN_TEST_PROPOSAL_TS", "20261023T120001Z")
        .arg("--brain")
        .arg(&b)
        .arg("propose")
        .args(args)
        .write_stdin("x two\n");
    c.assert().success();
    let p = inbox_json(&sb, &b);
    assert_eq!(p[0]["proposed_by"], "cli");
    assert_eq!(p[1]["proposed_by"], "opencode");
}

#[test]
fn restating_a_note_lists_it_as_a_conflict_and_unrelated_text_lists_none() {
    let sb = Sandbox::new();
    let b = search_brain(&sb);
    let args = [
        "--op",
        "add",
        "--target",
        "projects/alpha/consent2.md",
        "--rationale",
        "r",
        "--source",
        "s",
    ];
    propose(&sb, &b, &args, RESTATED)
        .success()
        .stderr(predicates::str::contains(
            "semantic search off: model not downloaded",
        ));
    // A keyword search for this text has a rank-1 hit for the unrelated proposal too.
    propose(
        &sb,
        &b,
        &args,
        "Billing invoices quarterly archive zebra sandwich.",
    )
    .success();
    let p = inbox_json(&sb, &b);
    assert_eq!(p[0]["conflicts"][0]["path"], "projects/alpha/consent.md");
    assert_eq!(p[0]["conflicts"][0]["protected"], false);
    assert!(p[0]["conflicts"].as_array().unwrap().len() <= 3);
    assert_eq!(p[1]["conflicts"], serde_json::json!([]));
    let hit = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .args([
            "--json",
            "search",
            "Billing invoices quarterly archive zebra sandwich",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let hits: Value = serde_json::from_slice(&hit).unwrap();
    assert!(!hits["hits"].as_array().unwrap().is_empty());
}

#[test]
fn the_target_itself_is_never_its_own_conflict() {
    let sb = Sandbox::new();
    let b = search_brain(&sb);
    propose(
        &sb,
        &b,
        &[
            "--op",
            "update",
            "--target",
            "projects/alpha/consent.md",
            "--rationale",
            "r",
            "--source",
            "s",
        ],
        RESTATED,
    )
    .success();
    let p = inbox_json(&sb, &b);
    assert!(p[0]["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["path"] != "projects/alpha/consent.md"));
}

#[test]
fn two_proposals_in_one_second_get_distinct_ids() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    let args = [
        "--op",
        "add",
        "--target",
        "projects/alpha/new.md",
        "--rationale",
        "r",
        "--source",
        "s",
    ];
    propose(&sb, &b, &args, "first\n").success();
    propose(&sb, &b, &args, "second\n").success();
    assert_eq!(
        inbox_files(&b),
        vec!["20261023T120000Z-new-2.md", "20261023T120000Z-new.md"]
    );
    let first = fs::read_to_string(b.join("inbox/20261023T120000Z-new.md")).unwrap();
    assert!(first.ends_with("first\n"));
}

#[test]
fn updating_a_protected_note_marks_the_proposal_protected() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    fs::write(
        b.join("projects/alpha/locked.md"),
        "---\nprotected: true\n---\n# Locked\n",
    )
    .unwrap();
    propose(
        &sb,
        &b,
        &[
            "--op",
            "update",
            "--target",
            "projects/alpha/locked.md",
            "--rationale",
            "r",
            "--source",
            "s",
        ],
        "new text\n",
    )
    .success();
    let text = fs::read_to_string(b.join("inbox/20261023T120000Z-locked.md")).unwrap();
    assert!(text.contains("conflicts_protected: true\n"), "{text}");
}

#[test]
fn invalid_proposals_exit_2_with_the_exact_message() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    let cases: [(&[&str], &str); 6] = [
        (
            &["--op", "add", "--target", "../outside.md"],
            "invalid target: must be a relative path inside the brain",
        ),
        (
            &["--op", "add", "--target", "/abs.md"],
            "invalid target: must be a relative path inside the brain",
        ),
        (
            &["--op", "add", "--target", "inbox/x.md"],
            "invalid target: inbox is reserved for proposals",
        ),
        (
            &["--op", "add", "--target", "projects/alpha/status.md"],
            "target exists: projects/alpha/status.md (use --op update or supersede)",
        ),
        (
            &["--op", "update", "--target", "projects/alpha/none.md"],
            "target not found: projects/alpha/none.md",
        ),
        (
            &["--op", "supersede", "--target", "projects/alpha/new.md"],
            "--supersedes is required for op supersede",
        ),
    ];
    for (args, msg) in cases {
        let out = propose(
            &sb,
            &b,
            &[args, &["--rationale", "r", "--source", "s"]].concat(),
            "x\n",
        )
        .code(2)
        .get_output()
        .stderr
        .clone();
        assert_eq!(String::from_utf8(out).unwrap(), format!("{msg}\n"));
    }
    assert!(inbox_files(&b).is_empty());
}

#[cfg(unix)]
#[test]
fn a_symlink_that_leaves_the_brain_is_rejected() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    let outside = sb.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, b.join("projects/link")).unwrap();
    for op in ["add", "update"] {
        propose(
            &sb,
            &b,
            &[
                "--op",
                op,
                "--target",
                "projects/link/x.md",
                "--rationale",
                "r",
                "--source",
                "s",
            ],
            "x\n",
        )
        .code(2)
        .stderr("invalid target: must be a relative path inside the brain\n");
    }
}

#[test]
fn supersedes_is_validated_like_a_target() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    let base = [
        "--op",
        "supersede",
        "--target",
        "projects/alpha/new.md",
        "--rationale",
        "r",
        "--source",
        "s",
    ];
    propose(
        &sb,
        &b,
        &[&base[..], &["--supersedes", "../x.md"]].concat(),
        "x\n",
    )
    .code(2)
    .stderr("invalid supersedes: must be a relative path inside the brain\n");
    propose(
        &sb,
        &b,
        &[&base[..], &["--supersedes", "projects/alpha/none.md"]].concat(),
        "x\n",
    )
    .code(2)
    .stderr("supersedes not found: projects/alpha/none.md\n");
    propose(
        &sb,
        &b,
        &[&base[..], &["--supersedes", "projects/alpha/status.md"]].concat(),
        "x\n",
    )
    .success();
    let p = inbox_json(&sb, &b);
    assert_eq!(p[0]["supersedes"], "projects/alpha/status.md");
}

#[test]
fn inbox_lists_oldest_first_with_id_op_target_conflicts() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    for (ts, target) in [
        ("20261023T120005Z", "projects/alpha/b.md"),
        ("20261023T120001Z", "projects/alpha/a.md"),
    ] {
        let mut c = sb.cmd();
        c.env("FBRAIN_TEST_PROPOSAL_TS", ts)
            .arg("--brain")
            .arg(&b)
            .args([
                "propose",
                "--op",
                "add",
                "--target",
                target,
                "--rationale",
                "r",
                "--source",
                "s",
            ])
            .write_stdin("x\n");
        c.assert().success();
    }
    let p = inbox_json(&sb, &b);
    let ids: Vec<_> = p.iter().map(|x| x["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["20261023T120001Z-a", "20261023T120005Z-b"]);
    assert_eq!(p[0]["op"], "add");
    assert_eq!(p[0]["target"], "projects/alpha/a.md");
    assert!(p[0]["conflicts"].is_array());
}

#[test]
fn an_unreadable_proposal_is_skipped_with_a_warning() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    fs::create_dir_all(b.join("inbox")).unwrap();
    fs::write(b.join("inbox/junk.md"), "no frontmatter\n").unwrap();
    sb.cmd()
        .arg("--brain")
        .arg(&b)
        .args(["--json", "inbox"])
        .assert()
        .success()
        .stdout("[]\n")
        .stderr(predicates::str::contains("skipping inbox/junk.md"));
}

#[test]
fn with_the_model_a_paraphrase_conflicts_by_cosine_and_unrelated_text_does_not() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("semantic");
    fs::write(b.join(".fbrain.toml"), "name = \"semantic\"\n").unwrap();
    sb.cmd()
        .env(
            "FBRAIN_TEST_MODEL_DIR",
            std::env::var("FBRAIN_TEST_MODEL_DIR").expect("set FBRAIN_TEST_MODEL_DIR"),
        )
        .env_remove("FBRAIN_OFFLINE")
        .args(["doctor", "--fix"])
        .assert()
        .success();
    let args = [
        "--op",
        "add",
        "--target",
        "projects/shop/returns.md",
        "--rationale",
        "r",
        "--source",
        "s",
    ];
    propose(&sb, &b, &args, "# Returns\n\nA customer who cancels a yearly plan within a month gets all of the money returned.\n")
        .success()
        .stderr(predicates::str::contains("semantic search off").not());
    propose(
        &sb,
        &b,
        &args,
        "# Notes\n\nTwo cats sleep on the warm roof tiles all afternoon.\n",
    )
    .success();
    let p = inbox_json(&sb, &b);
    let paths: Vec<&str> = p[0]["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"projects/shop/refund-policy.md")
            || paths.contains(&"projects/shop/money-back.md"),
        "{paths:?}"
    );
    assert!(p[0]["conflicts"][0]["score"].as_f64().unwrap() >= 0.5);
    assert_eq!(p[1]["conflicts"], serde_json::json!([]));
}

#[test]
fn superseding_a_protected_note_marks_the_proposal_protected() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    fs::write(
        b.join("projects/alpha/locked.md"),
        "---\nprotected: true\n---\n# Locked\n",
    )
    .unwrap();
    fs::write(
        b.join("projects/alpha/broken.md"),
        "---\nprotected: [oops\n---\n# Broken\n",
    )
    .unwrap();
    // A note whose frontmatter does not parse is hidden from the CLI like a private one.
    for (old, ok) in [
        ("projects/alpha/locked.md", true),
        ("projects/alpha/broken.md", false),
    ] {
        propose(
            &sb,
            &b,
            &[
                "--op",
                "supersede",
                "--target",
                "projects/alpha/new.md",
                "--supersedes",
                old,
                "--rationale",
                "r",
                "--source",
                "s",
            ],
            "unrelated zebra text\n",
        )
        .code(if ok { 0 } else { 2 });
    }
    let p = inbox_json(&sb, &b);
    assert_eq!(p.len(), 1);
    assert!(p.iter().all(|x| x["conflicts_protected"] == true));
}

#[test]
fn supersedes_with_another_op_is_rejected() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    for op in ["add", "update", "invalidate"] {
        let target = if op == "add" {
            "projects/alpha/new.md"
        } else {
            "projects/alpha/status.md"
        };
        propose(
            &sb,
            &b,
            &[
                "--op",
                op,
                "--target",
                target,
                "--supersedes",
                "projects/alpha/BRIEF.md",
                "--rationale",
                "r",
                "--source",
                "s",
            ],
            "x\n",
        )
        .code(2)
        .stderr("--supersedes is only valid for op supersede\n");
    }
    assert!(inbox_files(&b).is_empty());
}

#[test]
fn targets_must_be_visible_markdown_notes() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    fs::create_dir_all(b.join(".git/hooks")).unwrap();
    fs::write(b.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
    fs::create_dir_all(b.join("scripts")).unwrap();
    fs::write(b.join("scripts/x.sh"), "#!/bin/sh\n").unwrap();
    fs::write(b.join("projects/alpha/.secret.md"), "s\n").unwrap();
    for (op, t) in [
        ("update", ".git/hooks/pre-commit"),
        ("update", ".fbrain.toml"),
        ("update", "scripts/x.sh"),
        ("update", "projects/alpha/.secret.md"),
        ("add", "projects/a/.new.md"),
        ("add", "projects/alpha/new.txt"),
    ] {
        propose(
            &sb,
            &b,
            &[
                "--op",
                op,
                "--target",
                t,
                "--rationale",
                "r",
                "--source",
                "s",
            ],
            "x\n",
        )
        .code(2)
        .stderr("invalid target: must be a note (.md, not hidden)\n");
    }
    propose(
        &sb,
        &b,
        &[
            "--op",
            "supersede",
            "--target",
            "projects/alpha/new.md",
            "--supersedes",
            ".fbrain.toml",
            "--rationale",
            "r",
            "--source",
            "s",
        ],
        "x\n",
    )
    .code(2)
    .stderr("invalid supersedes: must be a note (.md, not hidden)\n");
    assert!(inbox_files(&b).is_empty());
}

#[test]
fn targets_outside_the_include_roots_are_rejected() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    fs::write(
        b.join(".fbrain.toml"),
        "name = \"basic\"\ninclude = [\"projects\"]\n",
    )
    .unwrap();
    propose(
        &sb,
        &b,
        &[
            "--op",
            "add",
            "--target",
            "me/new.md",
            "--rationale",
            "r",
            "--source",
            "s",
        ],
        "x\n",
    )
    .code(2)
    .stderr("invalid target: must be a relative path inside the brain\n");
    propose(
        &sb,
        &b,
        &[
            "--op",
            "add",
            "--target",
            "projects/alpha/new.md",
            "--rationale",
            "r",
            "--source",
            "s",
        ],
        "x\n",
    )
    .success();
}
