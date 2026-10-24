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

fn links(sb: &Sandbox, brain: &PathBuf, note: &str) -> Value {
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(brain)
        .args(["--json", "get", note, "--links"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).unwrap()
}

fn outgoing(v: &Value) -> Vec<Value> {
    v["outgoing"].as_array().unwrap().clone()
}

fn resolved(v: &Value) -> Vec<String> {
    outgoing(v)
        .iter()
        .filter_map(|o| o["resolved"].as_str().map(String::from))
        .collect()
}

#[test]
fn alias_heading_block_and_embed_forms_resolve_to_one_target() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let v = links(&sb, &b, "src/pipes");
    assert_eq!(resolved(&v), vec!["Note.md"]);
}

#[test]
fn links_in_fenced_and_inline_code_are_not_extracted() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let v = links(&sb, &b, "src/code");
    assert!(outgoing(&v).is_empty(), "{v}");
}

#[test]
fn path_suffix_picks_the_matching_directory() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let v = links(&sb, &b, "src/suffix");
    assert_eq!(resolved(&v), vec!["a/sub/Pick.md"]);
    assert_eq!(outgoing(&v)[0]["ambiguous"], false);
}

#[test]
fn basename_match_ignores_case() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let v = links(&sb, &b, "src/lower");
    assert_eq!(resolved(&v), vec!["Note.md"]);
}

#[test]
fn alias_resolves_and_is_marked_via_alias() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let v = links(&sb, &b, "src/alias");
    let o = outgoing(&v);
    assert_eq!(o[0]["resolved"], "people/Mona.md");
    assert_eq!(o[0]["via_alias"], true);
    let plain = links(&sb, &b, "src/lower");
    assert_eq!(outgoing(&plain)[0]["via_alias"], false);
}

#[test]
fn duplicate_basename_takes_the_shortest_path_and_is_marked_ambiguous() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let v = links(&sb, &b, "src/dup");
    let o = outgoing(&v);
    assert_eq!(o[0]["resolved"], "x/Dup.md");
    assert_eq!(o[0]["ambiguous"], true);
}

#[test]
fn missing_target_has_null_resolved() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let v = links(&sb, &b, "src/dangling");
    let o = outgoing(&v);
    assert_eq!(o.len(), 1);
    assert_eq!(o[0]["target"], "Missing");
    assert!(o[0]["resolved"].is_null());
}

#[test]
fn backlinks_list_every_source_and_never_index_md() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let v = links(&sb, &b, "Target");
    let back: Vec<&str> = v["backlinks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(back, vec!["src/back1.md", "src/back2.md"]);
}

#[test]
fn links_flag_prints_sections_in_text_mode() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .args(["get", "Target", "--links"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("Backlinks"), "{s}");
    assert!(s.contains("src/back1.md"), "{s}");
}

#[test]
fn graph_of_three_thousand_notes_resolves_shared_basenames_and_suffixes() {
    let sb = Sandbox::new();
    let b = sb.root.join("big");
    fs::create_dir_all(b.join("s/t")).unwrap();
    fs::create_dir_all(b.join("u")).unwrap();
    fs::write(b.join(".fbrain.toml"), "name = \"big\"\n").unwrap();
    for p in ["s/Shared.md", "s/t/Shared.md", "u/Shared.md"] {
        fs::write(b.join(p), "# Shared\n").unwrap();
    }
    let n = 3000;
    for i in 0..n {
        let dir = b.join(format!("d{}", i % 50));
        fs::create_dir_all(&dir).unwrap();
        let mut body = format!("# N{i}\n\n");
        for j in 1..=8 {
            body += &format!("[[n{}]] ", (i + j) % n);
        }
        body += "[[Shared]] [[t/Shared]] [[Dangling]]\n";
        fs::write(dir.join(format!("n{i}.md")), body).unwrap();
    }
    let v = links(&sb, &b, "d0/n0");
    let o = outgoing(&v);
    assert_eq!(o.len(), 11);
    let find = |t: &str| o.iter().find(|x| x["target"] == t).unwrap().clone();
    assert_eq!(find("n1")["resolved"], "d1/n1.md");
    assert_eq!(find("Shared")["resolved"], "s/Shared.md");
    assert_eq!(find("Shared")["ambiguous"], true);
    assert_eq!(find("t/Shared")["resolved"], "s/t/Shared.md");
    assert!(find("Dangling")["resolved"].is_null());
    assert_eq!(v["backlinks"].as_array().unwrap().len(), 8);
}

#[test]
fn office_file_links_are_attachments_not_note_links() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    fs::write(
        b.join("src/office.md"),
        "# Office\n\n[[Deck.pptx]] [[Scope.docx]] [[Sheet.xlsx]] [[Old.doc]] [[Old.ppt]] [[Real]]\n",
    )
    .unwrap();
    let targets: Vec<String> = outgoing(&links(&sb, &b, "src/office"))
        .iter()
        .map(|o| o["target"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(targets, vec!["Real"]);
}
