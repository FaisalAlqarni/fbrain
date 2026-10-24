mod common;

use common::Sandbox;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn brain(sb: &Sandbox) -> PathBuf {
    let dir = sb.fixture_brain("search");
    fs::write(dir.join(".fbrain.toml"), "name = \"search\"\n").unwrap();
    dir
}

fn run(sb: &Sandbox, brain: &Path, args: &[&str]) -> (String, String) {
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(brain)
        .args(args)
        .assert()
        .success()
        .get_output()
        .clone();
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn hits(sb: &Sandbox, brain: &Path, args: &[&str]) -> Vec<Value> {
    let mut a = vec!["--json", "search"];
    a.extend_from_slice(args);
    let (out, _) = run(sb, brain, &a);
    let v: Value = serde_json::from_str(&out).unwrap();
    v["hits"].as_array().unwrap().clone()
}

fn paths(h: &[Value]) -> Vec<&str> {
    h.iter().map(|x| x["rel_path"].as_str().unwrap()).collect()
}

#[test]
fn consent_query_ranks_consent_note_first_with_explanation() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let h = hits(&sb, &b, &["consent opt-out"]);
    assert_eq!(h[0]["rel_path"], "projects/alpha/consent.md");
    assert_eq!(h[0]["explain"]["bm25_rank"], 1);
    let terms = h[0]["explain"]["matched_terms"].as_array().unwrap();
    assert!(terms.iter().any(|t| t == "consent"), "{terms:?}");
}

#[test]
fn arabic_diacritics_are_ignored() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let h = hits(&sb, &b, &["المُوافَقة"]);
    assert_eq!(h[0]["rel_path"], "projects/beta/ar-consent.md");
}

#[test]
fn arabic_alef_forms_match() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let h = hits(&sb, &b, &["اشعار"]);
    assert_eq!(h[0]["rel_path"], "projects/beta/ar-notice.md");
}

#[test]
fn arabic_tatweel_is_ignored() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let h = hits(&sb, &b, &["رســالة"]);
    assert_eq!(h[0]["rel_path"], "projects/beta/ar-message.md");
}

#[test]
fn superseded_note_needs_history() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    assert!(hits(&sb, &b, &["pelican"]).is_empty());
    let h = hits(&sb, &b, &["--history", "pelican"]);
    assert_eq!(h[0]["rel_path"], "projects/alpha/legacy-quota.md");
    assert_eq!(h[0]["status"], "superseded");
}

#[test]
fn invalidated_note_needs_history() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    assert!(hits(&sb, &b, &["walrus"]).is_empty());
    assert_eq!(
        hits(&sb, &b, &["--history", "walrus"])[0]["status"],
        "invalidated"
    );
}

#[test]
fn expired_active_note_needs_as_of_before_valid_to() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    assert!(hits(&sb, &b, &["heron"]).is_empty());
    let h = hits(&sb, &b, &["--as-of", "2019-06-01", "heron"]);
    assert_eq!(h[0]["rel_path"], "projects/alpha/expired.md");
}

#[test]
fn status_flag_restricts_to_listed_statuses() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let all = hits(&sb, &b, &["ledger"]);
    assert_eq!(all.len(), 2);
    let h = hits(&sb, &b, &["--status", "done", "ledger"]);
    assert_eq!(paths(&h), ["projects/alpha/billing-done.md"]);
}

#[test]
fn stopword_only_query_returns_nothing_and_says_so() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let (out, err) = run(&sb, &b, &["--json", "search", "the and of"]);
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["hits"],
        serde_json::json!([])
    );
    assert!(err.contains("no searchable terms in query"), "{err}");
}

#[test]
fn long_note_hit_comes_from_the_matching_section() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let (out, _) = run(&sb, &b, &["--json", "search", "--chunks", "quokka"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    let h = &v["hits"][0];
    assert_eq!(h["rel_path"], "projects/alpha/handbook.md");
    assert!(h["heading_path"].as_str().unwrap().contains("Rollout"));
    assert!(h["snippet"].as_str().unwrap().contains("quokka"));
    assert!(h["snippet"].as_str().unwrap().chars().count() <= 240);
    let chunks = v["chunks"].as_array().unwrap();
    let headings: Vec<&str> = chunks
        .iter()
        .map(|c| c["heading_path"].as_str().unwrap())
        .collect();
    assert_eq!(headings, ["", "Background", "Decisions", "Rollout"]);
}

#[test]
fn reindex_reprocesses_only_changed_files() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    hits(&sb, &b, &["consent"]);
    let f = b.join("me/sync.md");
    fs::write(&f, "# Weekly sync\n\nMonday planning only.\n").unwrap();
    let (out, _) = run(&sb, &b, &["reindex", "--stats"]);
    assert_eq!(out.trim(), "changed 1, unchanged 11, removed 0");
    fs::remove_file(&f).unwrap();
    let (out, _) = run(&sb, &b, &["reindex", "--stats"]);
    assert_eq!(out.trim(), "changed 0, unchanged 11, removed 1");
}

#[test]
fn reindex_full_reprocesses_everything() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    hits(&sb, &b, &["consent"]);
    let (out, _) = run(&sb, &b, &["reindex", "--full", "--stats"]);
    assert!(out.starts_with("changed 12,"), "{out}");
}

#[test]
fn same_named_brains_at_different_paths_keep_separate_caches() {
    let sb = Sandbox::new();
    let a = brain(&sb);
    let other = sb.root.join("other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join(".fbrain.toml"), "name = \"search\"\n").unwrap();
    fs::write(other.join("solo.md"), "# Solo\n\nxylophone only here\n").unwrap();
    assert!(hits(&sb, &a, &["xylophone"]).is_empty());
    assert_eq!(paths(&hits(&sb, &other, &["xylophone"])), ["solo.md"]);
    assert!(hits(&sb, &other, &["consent"]).is_empty());
    assert!(hits(&sb, &a, &["xylophone"]).is_empty());
}

#[test]
fn concurrent_cold_searches_agree_and_leave_a_complete_cache() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let children: Vec<_> = (0..4)
        .map(|_| {
            let mut c = std::process::Command::new(sb.cmd().get_program());
            for (k, v) in sb.cmd().get_envs() {
                if let Some(v) = v {
                    c.env(k, v);
                } else {
                    c.env_remove(k);
                }
            }
            c.current_dir(&sb.root)
                .args(["--json", "--brain"])
                .arg(&b)
                .args(["search", "consent"])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let tops: Vec<String> = children
        .into_iter()
        .map(|c| {
            let o = c.wait_with_output().unwrap();
            assert!(o.status.success());
            let v: Value = serde_json::from_slice(&o.stdout).unwrap();
            v["hits"][0]["rel_path"].as_str().unwrap().to_string()
        })
        .collect();
    assert!(tops.iter().all(|t| t == &tops[0]));
    let (out, _) = run(&sb, &b, &["reindex", "--stats"]);
    assert!(out.starts_with("changed 0,"), "{out}");
}

#[test]
fn index_md_is_never_a_hit() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let h = hits(
        &sb,
        &b,
        &["--history", "-k", "50", "index consent ledger quokka"],
    );
    assert!(!paths(&h).contains(&"INDEX.md"), "{:?}", paths(&h));
    assert!(!h.is_empty());
}
