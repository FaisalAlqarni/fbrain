mod common;

use common::Sandbox;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const TEST_MODEL: &str = "google/embeddinggemma-300m";

fn model_src() -> PathBuf {
    PathBuf::from(std::env::var("FBRAIN_TEST_MODEL_DIR").expect("set FBRAIN_TEST_MODEL_DIR"))
}

fn brain(sb: &Sandbox) -> PathBuf {
    let dir = sb.fixture_brain("semantic");
    fs::write(dir.join(".fbrain.toml"), "name = \"semantic\"\n").unwrap();
    dir
}

/// Copies the test model into the sandbox cache under `id`, as `doctor --fix` does.
fn install_model(sb: &Sandbox, id: &str) {
    sb.cmd()
        .env("FBRAIN_MODEL", id)
        .env("FBRAIN_TEST_MODEL_DIR", model_src())
        .env_remove("FBRAIN_OFFLINE")
        .args(["doctor", "--fix"])
        .assert()
        .success();
}

fn fb(sb: &Sandbox, brain: &Path, model: &str) -> assert_cmd::Command {
    let mut c = sb.cmd();
    c.env("FBRAIN_MODEL", model).arg("--brain").arg(brain);
    c
}

fn search(sb: &Sandbox, brain: &Path, model: &str, q: &str) -> (Vec<Value>, String) {
    let out = fb(sb, brain, model)
        .args(["--json", "search", q])
        .assert()
        .success()
        .get_output()
        .clone();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    (
        v["hits"].as_array().unwrap().clone(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn reindex_stats(sb: &Sandbox, brain: &Path, model: &str) -> String {
    let out = fb(sb, brain, model)
        .args(["reindex", "--stats"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

#[test]
fn meaning_match_with_no_shared_keyword_is_in_the_top_three() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    install_model(&sb, TEST_MODEL);
    let (hits, stderr) = search(&sb, &b, TEST_MODEL, "opt out of marketing texts");
    assert!(!stderr.contains("semantic search off"), "{stderr}");
    let pos = hits
        .iter()
        .position(|h| h["rel_path"] == "projects/shop/sms-optout.md")
        .expect("note missing from hits");
    assert!(pos < 3, "ranked {}: {hits:?}", pos + 1);
    assert!(hits[pos]["explain"]["bm25_rank"].is_null());
    assert!(hits[pos]["explain"]["embed_rank"].is_number());
}

#[test]
fn note_matching_both_channels_outranks_notes_matching_one() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    install_model(&sb, TEST_MODEL);
    let (hits, _) = search(&sb, &b, TEST_MODEL, "refund policy");
    assert_eq!(hits[0]["rel_path"], "projects/shop/refund-policy.md");
    assert!(hits[0]["explain"]["bm25_rank"].is_number());
    assert!(hits[0]["explain"]["embed_rank"].is_number());
    // The keyword-only mention of "refund" must not beat the note that matches both.
    let fused = |i: usize| hits[i]["explain"]["fused"].as_f64().unwrap();
    assert!(fused(0) > fused(1));
}

#[test]
fn missing_model_falls_back_to_keywords_and_says_so() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let (hits, stderr) = search(&sb, &b, TEST_MODEL, "refund");
    assert!(stderr.contains("semantic search off: model not downloaded, run fbrain doctor --fix"));
    assert_eq!(hits[0]["rel_path"], "projects/shop/refund-policy.md");
    assert!(hits.iter().all(|h| h["explain"]["embed_rank"].is_null()));
}

#[test]
fn search_with_a_present_model_works_offline() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    install_model(&sb, TEST_MODEL);
    let out = fb(&sb, &b, TEST_MODEL)
        .env("FBRAIN_OFFLINE", "1")
        .args(["--json", "search", "refund policy"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(!String::from_utf8_lossy(&out.stderr).contains("semantic search off"));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["hits"][0]["explain"]["embed_rank"].is_number());
}

#[test]
fn reindex_after_the_model_appears_embeds_every_chunk() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let before = reindex_stats(&sb, &b, TEST_MODEL);
    assert!(!before.contains("embedded"), "{before}");
    install_model(&sb, TEST_MODEL);
    let after = reindex_stats(&sb, &b, TEST_MODEL);
    assert!(after.contains("embedded 12"), "{after}");
    let again = reindex_stats(&sb, &b, TEST_MODEL);
    assert!(again.contains("embedded 0"), "{again}");
}

fn cached_index(sb: &Sandbox) -> PathBuf {
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.file_name().is_some_and(|n| n == "index.json") {
                out.push(p);
            }
        }
    }
    let mut found = Vec::new();
    walk(&sb.root.join("cache"), &mut found);
    assert_eq!(found.len(), 1, "{found:?}");
    found.remove(0)
}

#[test]
fn cached_vectors_that_do_not_match_their_chunks_are_re_embedded() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    install_model(&sb, TEST_MODEL);
    reindex_stats(&sb, &b, TEST_MODEL);
    let file = cached_index(&sb);
    let mut v: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    v["files"]["projects/shop/refund-policy.md"]["vecs"] = serde_json::json!([]);
    fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();

    let out = reindex_stats(&sb, &b, TEST_MODEL);
    assert!(out.contains("embedded 1"), "{out}");
    let (hits, _) = search(&sb, &b, TEST_MODEL, "refund policy");
    assert!(hits[0]["explain"]["embed_rank"].is_number());
}

#[test]
fn a_changed_manifest_model_id_re_embeds_every_chunk() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    install_model(&sb, TEST_MODEL);
    assert!(reindex_stats(&sb, &b, TEST_MODEL).contains("embedded 12"));
    let file = cached_index(&sb);
    let mut v: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    v["model"] = "google/embeddinggemma-300m@old".into();
    fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();
    let out = reindex_stats(&sb, &b, TEST_MODEL);
    assert!(out.contains("embedded 12"), "{out}");
}

#[test]
fn a_cross_lingual_query_finds_the_english_note_in_the_top_three() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    install_model(&sb, TEST_MODEL);
    let (hits, _) = search(
        &sb,
        &b,
        TEST_MODEL,
        "كيف يلغي العميل الاشتراك في الرسائل الترويجية",
    );
    let pos = hits
        .iter()
        .position(|h| h["rel_path"] == "projects/shop/sms-optout.md")
        .expect("note missing from hits");
    assert!(pos < 3, "ranked {}: {hits:?}", pos + 1);
}

#[test]
fn reindex_stats_json_reports_the_model_and_dimension() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    install_model(&sb, TEST_MODEL);
    let out = fb(&sb, &b, TEST_MODEL)
        .args(["--json", "reindex", "--stats"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["dim"], 256);
    assert_eq!(v["model"], TEST_MODEL);
    assert_eq!(v["embedded"], 12);
}

#[test]
fn an_unknown_model_id_is_an_error_naming_the_supported_ones() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    fs::write(
        b.join(".fbrain.toml"),
        "name = \"semantic\"\nmodel = \"x/y\"\n",
    )
    .unwrap();
    sb.cmd()
        .arg("--brain")
        .arg(&b)
        .args(["search", "q"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains(
            "unknown model: x/y (supported: google/embeddinggemma-300m, BAAI/bge-m3)",
        ));
}

#[test]
fn the_alternative_model_absent_from_the_cache_falls_back_to_keywords() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    fs::write(
        b.join(".fbrain.toml"),
        "name = \"semantic\"\nmodel = \"BAAI/bge-m3\"\n",
    )
    .unwrap();
    sb.cmd()
        .arg("--brain")
        .arg(&b)
        .args(["search", "refund"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "semantic search off: model not downloaded, run fbrain doctor --fix",
        ));
}
