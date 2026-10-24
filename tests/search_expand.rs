mod common;

use common::Sandbox;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn brain(sb: &Sandbox) -> PathBuf {
    let dir = sb.fixture_brain("links");
    fs::write(dir.join(".fbrain.toml"), "name = \"links\"\n").unwrap();
    dir
}

fn hits(sb: &Sandbox, brain: &PathBuf, args: &[&str]) -> Vec<Value> {
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(brain)
        .args(["--json", "search"])
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    v["hits"].as_array().unwrap().clone()
}

fn paths(h: &[Value]) -> Vec<&str> {
    h.iter().map(|x| x["rel_path"].as_str().unwrap()).collect()
}

#[test]
fn linked_notes_follow_the_direct_hit_labelled_and_skip_superseded() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let h = hits(&sb, &b, &["zebrafish"]);
    assert_eq!(
        paths(&h),
        vec!["topic/A.md", "topic/B.md", "topic/C.md", "topic/E.md"]
    );
    assert!(h[0]["explain"]["linked_from"].is_null());
    for linked in &h[1..] {
        assert_eq!(linked["explain"]["linked_from"], "topic/A.md");
    }
}

#[test]
fn small_budget_keeps_only_the_first_of_three_long_hits() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    assert_eq!(hits(&sb, &b, &["packing", "--budget", "50"]).len(), 1);
    assert_eq!(hits(&sb, &b, &["packing"]).len(), 3);
}

#[test]
fn arabic_text_costs_one_token_per_two_characters() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let word = "كتاب".repeat(45);
    // Each hit is 200 Arabic characters (about 100 tokens): 120 fits one, 210 fits two.
    assert_eq!(hits(&sb, &b, &[&word, "--budget", "120"]).len(), 1);
    assert_eq!(hits(&sb, &b, &[&word, "--budget", "210"]).len(), 2);
}

#[test]
fn brain_config_token_budget_applies_and_flag_overrides_it() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    fs::write(
        b.join(".fbrain.toml"),
        "name = \"links\"\ntoken_budget = 50\n",
    )
    .unwrap();
    assert_eq!(hits(&sb, &b, &["packing"]).len(), 1);
    assert_eq!(hits(&sb, &b, &["packing", "--budget", "4000"]).len(), 3);
}

#[test]
fn index_md_is_never_a_hit() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    for q in ["zebrafish", "Index", "Target"] {
        let h = hits(&sb, &b, &[q]);
        assert!(!paths(&h).contains(&"INDEX.md"), "{q}: {h:?}");
    }
}
