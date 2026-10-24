//! The embedder through its library interface, with the real default model from FBRAIN_TEST_MODEL_DIR.

mod common;

use common::Sandbox;
use fbrain::brain::Brain;
use fbrain::index::embed::{self, Embedder};
use fbrain::index::Index;
use std::path::PathBuf;

const DEFAULT: &str = "google/embeddinggemma-300m";

fn embedder() -> Embedder {
    let dir =
        PathBuf::from(std::env::var("FBRAIN_TEST_MODEL_DIR").expect("set FBRAIN_TEST_MODEL_DIR"));
    Embedder::load(&dir, embed::spec(DEFAULT).unwrap()).unwrap()
}

fn s(t: &str) -> Vec<String> {
    vec![t.to_string()]
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
fn unknown_model_ids_are_rejected_with_the_supported_list() {
    let e = embed::spec("x/y").unwrap_err();
    assert_eq!(
        e.to_string(),
        "unknown model: x/y (supported: google/embeddinggemma-300m, BAAI/bge-m3)"
    );
}

#[test]
fn the_default_spec_pins_the_revision_and_256_dimensions() {
    let spec = embed::spec(DEFAULT).unwrap();
    assert_eq!(spec.dims, 256);
    assert_eq!(spec.revision, "5090578d9565bb06545b4552f76e6bc2c93e4a66");
    assert_eq!(spec.query_prefix, "task: search result | query: ");
    assert!(spec.files.iter().any(|f| f.0 == "tokenizer.json"));
    assert!(spec.files.iter().all(|f| f.1.len() == 64));
}

#[test]
fn vectors_have_256_dimensions_and_unit_length() {
    let e = embedder();
    assert_eq!(e.dim(), 256);
    let q = e.embed_queries(&s("opt out of marketing texts")).unwrap();
    let d = e.embed_documents(&s("Customers can unsubscribe")).unwrap();
    for v in q.iter().chain(&d) {
        assert_eq!(v.len(), 256);
        assert!((dot(v, v) - 1.0).abs() < 1e-3);
    }
}

#[test]
fn queries_and_documents_get_different_prefixes() {
    let e = embedder();
    let t = s("refund policy");
    let q = e.embed_queries(&t).unwrap().remove(0);
    let d = e.embed_documents(&t).unwrap().remove(0);
    assert!(dot(&q, &d) < 0.999);
}

#[test]
fn an_arabic_query_is_closer_to_its_english_match_than_to_an_unrelated_note() {
    let e = embedder();
    let q = e
        .embed_queries(&s("كيف يلغي العميل الاشتراك في الرسائل الترويجية"))
        .unwrap()
        .remove(0);
    let docs = e
        .embed_documents(&[
            "Customers can unsubscribe from promotional SMS".to_string(),
            "Nightly backups run at 02:00 and are kept for 30 days".to_string(),
        ])
        .unwrap();
    assert!(dot(&q, &docs[0]) > dot(&q, &docs[1]));
}

#[test]
fn cosine_top_ranks_notes_by_their_best_chunk_and_honours_exclude() {
    let sb = Sandbox::new();
    let dir = sb.fixture_brain("semantic");
    std::fs::write(dir.join(".fbrain.toml"), "name = \"semantic\"\n").unwrap();
    let brain = Brain::open(&dir).unwrap();
    let e = embedder();
    let ix =
        Index::load_or_build(&brain, &sb.root.join("cache"), false, Some((DEFAULT, &e))).unwrap();
    let q = e
        .embed_queries(&s("opt out of marketing texts"))
        .unwrap()
        .remove(0);

    let top = embed::cosine_top(&ix, &q, 3, None).unwrap();
    assert_eq!(top.len(), 3);
    assert_eq!(top[0].0, "projects/shop/sms-optout.md");
    assert!(top.windows(2).all(|w| w[0].1 >= w[1].1));
    let names: std::collections::HashSet<_> = top.iter().map(|t| &t.0).collect();
    assert_eq!(names.len(), 3);

    let without = embed::cosine_top(&ix, &q, 3, Some("projects/shop/sms-optout.md")).unwrap();
    assert!(without.iter().all(|t| t.0 != "projects/shop/sms-optout.md"));
    assert_eq!(without.len(), 3);
}

#[test]
fn two_embedders_can_be_alive_in_one_process() {
    let (a, b) = (embedder(), embedder());
    let t = s("refund policy");
    assert_eq!(a.embed_queries(&t).unwrap(), b.embed_queries(&t).unwrap());
}

#[test]
fn cosine_top_names_the_cause_when_the_index_has_no_usable_vectors() {
    let sb = Sandbox::new();
    let dir = sb.fixture_brain("semantic");
    std::fs::write(dir.join(".fbrain.toml"), "name = \"semantic\"\n").unwrap();
    let brain = Brain::open(&dir).unwrap();
    let e = embedder();
    let q = e.embed_queries(&s("refund policy")).unwrap().remove(0);

    let keyword_only =
        Index::load_or_build(&brain, &sb.root.join("cache-kw"), false, None).unwrap();
    let err = embed::cosine_top(&keyword_only, &q, 3, None).unwrap_err();
    assert!(err.to_string().contains("no embeddings"), "{err}");

    let ix =
        Index::load_or_build(&brain, &sb.root.join("cache"), false, Some((DEFAULT, &e))).unwrap();
    let err = embed::cosine_top(&ix, &q[..10], 3, None).unwrap_err();
    assert!(
        err.to_string().contains("query vector has 10 floats"),
        "{err}"
    );
}
