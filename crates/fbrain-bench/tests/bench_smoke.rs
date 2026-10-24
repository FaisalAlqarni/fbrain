#[path = "../../../tests/common/mod.rs"]
mod common;

use common::Sandbox;

trait BenchCmd {
    fn bench_cmd(&self) -> assert_cmd::Command;
}

impl BenchCmd for Sandbox {
    /// The same sandbox for the `fbrain-bench` runner (and the `fbrain` processes it starts).
    fn bench_cmd(&self) -> assert_cmd::Command {
        assert_cmd::Command::from_std(self.sandboxed(std::process::Command::new(env!(
            "CARGO_BIN_EXE_fbrain-bench"
        ))))
    }
}
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn brain(sb: &Sandbox) -> PathBuf {
    let dir = sb.fixture_brain("search");
    fs::write(dir.join(".fbrain.toml"), "name = \"search\"\n").unwrap();
    dir
}

fn fixture_queries() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/fixture-queries.toml")
}

#[test]
fn report_has_every_metric_and_exits_zero() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let out = sb.root.join("out");
    let assert = sb
        .bench_cmd()
        .arg("--brain")
        .arg(&b)
        .arg("--queries")
        .arg(fixture_queries())
        .arg("--out")
        .arg(&out)
        .arg("--keyword-only")
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("semantic false"), "{stdout}");
    for field in [
        "recall_at_5",
        "mrr",
        "warm_p50_ms",
        "warm_p95_ms",
        "cold_p95_ms",
        "peak_rss_mb",
        "recall_at_5[en]",
        "recall_at_5[ar]",
        "recall_at_5[mixed]",
    ] {
        assert!(stdout.contains(field), "{field} missing from:\n{stdout}");
    }
    let report: Value =
        serde_json::from_slice(&fs::read(out.join("report.json")).unwrap()).unwrap();
    let v = &report["variants"][0];
    assert_eq!(v["recall_at_5"], 1.0, "{v}");
    assert_eq!(v["mrr"], 1.0, "{v}");
    assert_eq!(v["queries"], 5);
    for f in ["warm_p50_ms", "warm_p95_ms", "cold_p95_ms"] {
        assert!(v[f].as_f64().unwrap() >= 0.0, "{f}: {v}");
    }
    assert_eq!(v["recall_by_lang"]["ar"], 1.0);
}

#[test]
fn recall_below_min_recall_exits_one_naming_the_metric() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    // Two of five queries expect the right note; the other three expect an unrelated one.
    let wrong = "me/sync.md";
    let q = |id: &str, text: &str, expect: &str| {
        format!(
            "[[query]]\nid = \"{id}\"\ntext = \"{text}\"\nlang = \"en\"\nexpect = [\"{expect}\"]\n"
        )
    };
    let queries = [
        q("a", "consent opt-out", "projects/alpha/consent.md"),
        q("b", "Rust SQLite HTTP layer", "projects/beta/stack.md"),
        q("c", "consent opt-out", wrong),
        q("d", "Rust SQLite HTTP layer", wrong),
        q("e", "rollout checklist", wrong),
    ]
    .join("\n");
    let file = sb.root.join("q.toml");
    fs::write(&file, queries).unwrap();
    sb.bench_cmd()
        .arg("--brain")
        .arg(&b)
        .arg("--queries")
        .arg(&file)
        .args(["--min-recall", "0.8", "--keyword-only"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains(
            "threshold failed: recall_at_5 0.40 < 0.80",
        ));
}

#[test]
fn unknown_variant_key_is_an_error() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    sb.bench_cmd()
        .arg("--brain")
        .arg(&b)
        .arg("--queries")
        .arg(fixture_queries())
        .args(["--variant", "colour=red", "--keyword-only"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("unknown variant key `colour`"));
}

#[test]
fn report_splits_recall_by_kind_and_scores_the_embedding_channel_alone() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    sb.cmd()
        .env(
            "FBRAIN_TEST_MODEL_DIR",
            std::env::var("FBRAIN_TEST_MODEL_DIR").unwrap(),
        )
        .env_remove("FBRAIN_OFFLINE")
        .args(["doctor", "--fix"])
        .assert()
        .success();
    let q = |id: &str, text: &str, kind: &str, expect: &str| {
        format!(
            "[[query]]\nid = \"{id}\"\ntext = \"{text}\"\nlang = \"en\"\nkind = \"{kind}\"\nexpect = [\"{expect}\"]\n"
        )
    };
    let queries = [
        q(
            "a",
            "consent opt-out",
            "paraphrase",
            "projects/alpha/consent.md",
        ),
        q(
            "b",
            "Rust SQLite HTTP layer",
            "multi",
            "projects/beta/stack.md",
        ),
    ]
    .join("\n");
    let file = sb.root.join("q.toml");
    fs::write(&file, queries).unwrap();
    let out = sb.root.join("out");
    sb.bench_cmd()
        .arg("--brain")
        .arg(&b)
        .arg("--queries")
        .arg(&file)
        .arg("--out")
        .arg(&out)
        .assert()
        .success();
    let report: Value =
        serde_json::from_slice(&fs::read(out.join("report.json")).unwrap()).unwrap();
    let v = &report["variants"][0];
    assert_eq!(v["recall_by_kind"]["paraphrase"], 1.0, "{v}");
    assert_eq!(v["recall_by_kind"]["multi"], 1.0, "{v}");
    assert!(v["embedding_only"]["recall_at_5"].is_number(), "{v}");
    assert!(v["embedding_only"]["mrr"].is_number(), "{v}");
    let s = &v["serve"];
    for f in [
        "first_call_ms",
        "warm_p50_ms",
        "warm_p95_ms",
        "server_rss_mb",
        "recall_at_5",
    ] {
        assert!(s[f].as_f64().is_some_and(|x| x > 0.0), "serve.{f}: {v}");
    }
    assert_eq!(s["recall_at_5"], v["recall_at_5"], "{v}");
    assert_eq!(sb.server_pid(), None, "the bench left its server running");
}

/// A server still loading when the bench connects must be waited for, not replaced by a
/// `fbrain-bench serve` (the bench's own binary has no `serve` command).
#[test]
fn a_semantic_run_waits_for_a_server_that_is_still_loading() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    sb.cmd()
        .env(
            "FBRAIN_TEST_MODEL_DIR",
            std::env::var("FBRAIN_TEST_MODEL_DIR").unwrap(),
        )
        .env_remove("FBRAIN_OFFLINE")
        .args(["doctor", "--fix"])
        .assert()
        .success();
    let file = sb.root.join("q.toml");
    fs::write(
        &file,
        "[[query]]\nid = \"a\"\ntext = \"consent opt-out\"\nlang = \"en\"\nexpect = [\"projects/alpha/consent.md\"]\n",
    )
    .unwrap();
    let out = sb.root.join("out");
    let assert = sb
        .bench_cmd()
        .env("FBRAIN_TEST_SERVE_DELAY_MS", "2000")
        .arg("--brain")
        .arg(&b)
        .arg("--queries")
        .arg(&file)
        .arg("--out")
        .arg(&out)
        .assert();
    let err = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(!err.contains("unexpected argument 'serve'"), "{err}");
    assert.success();
    assert_eq!(sb.server_pid(), None, "the bench left its server running");
}

#[test]
fn a_semantic_run_without_the_model_fails_instead_of_measuring_keywords() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    sb.bench_cmd()
        .arg("--brain")
        .arg(&b)
        .arg("--queries")
        .arg(fixture_queries())
        .assert()
        .code(1)
        .stderr(predicates::str::contains("ran without the embedding model"));
}
