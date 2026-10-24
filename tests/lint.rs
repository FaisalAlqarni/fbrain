mod common;

use common::Sandbox;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Std;

const SEED: &str = "2026-01-01T00:00:00+00:00";
const MANUAL: &str = "2026-01-10T00:00:00+00:00";
const SOURCE_EDIT: &str = "2026-02-01T00:00:00+00:00";

fn git_at(dir: &Path, date: &str, args: &[&str]) -> String {
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
    String::from_utf8(out.stdout).unwrap()
}

fn commit_all(dir: &Path, date: &str, subject: &str) {
    git_at(dir, date, &["add", "-A"]);
    git_at(dir, date, &["commit", "-q", "-m", subject]);
}

fn read(b: &Path, rel: &str) -> String {
    fs::read_to_string(b.join(rel)).unwrap()
}

fn write(b: &Path, rel: &str, text: &str) {
    let p = b.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

/// The lint fixture as a git repo with a fixed history: everything seeded by `fbrain:`, then a
/// manual edit of the protected note and a later edit of the source of the derived summary.
fn lint_repo(sb: &Sandbox) -> PathBuf {
    let b = sb.fixture_brain("lint");
    git_at(&b, SEED, &["init", "-q"]);
    commit_all(&b, SEED, "fbrain: seed");
    let p = "projects/alpha/decision.md";
    write(&b, p, &(read(&b, p) + "Edited by hand.\n"));
    commit_all(&b, MANUAL, "manual edit");
    let s = "projects/alpha/source-doc.md";
    write(&b, s, &(read(&b, s) + "Changed later.\n"));
    commit_all(&b, SOURCE_EDIT, "fbrain: update source");
    b
}

/// The basic fixture as a committed repo (every commit subject starts with `fbrain:`).
fn basic_repo(sb: &Sandbox) -> PathBuf {
    let b = sb.fixture_brain("basic");
    git_at(&b, SEED, &["init", "-q"]);
    commit_all(&b, SEED, "fbrain: seed");
    b
}

fn lint(sb: &Sandbox, b: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    sb.cmd()
        .arg("--brain")
        .arg(b)
        .arg("lint")
        .args(args)
        .assert()
}

/// `(rule, rel_path, detail)` of the issues `lint --json` prints, allowed ones included.
fn issues(sb: &Sandbox, b: &Path, args: &[&str]) -> Vec<(String, String, String)> {
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(b)
        .arg("--json")
        .arg("lint")
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["rule"].as_str().unwrap().to_string(),
                i["rel_path"].as_str().unwrap().to_string(),
                i["detail"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn rules(found: &[(String, String, String)]) -> Vec<&str> {
    found.iter().map(|i| i.0.as_str()).collect()
}

/// Every `.md` file in the brain by rel path, with its text.
fn snapshot(b: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![b.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "md") {
                let rel = p
                    .strip_prefix(b)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, fs::read_to_string(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

fn body_of(text: &str) -> &str {
    match text
        .strip_prefix("---\n")
        .and_then(|r| r.split_once("\n---\n"))
    {
        Some((_, body)) => body,
        None => text,
    }
}

#[test]
fn the_lint_fixture_yields_one_issue_per_rule() {
    let sb = Sandbox::new();
    let b = lint_repo(&sb);
    let found = issues(&sb, &b, &[]);
    let got: Vec<(&str, &str)> = found.iter().map(|i| (i.0.as_str(), i.1.as_str())).collect();
    assert_eq!(
        got,
        vec![
            ("L001", "projects/alpha/broken.md"),
            ("L002", "projects/alpha/ambig.md"),
            ("L003", "projects/alpha/orphan.md"),
            ("L004", "projects/alpha/old-a.md"),
            ("L005", "projects/alpha/old-b.md"),
            ("L006", "projects/alpha/summary.md"),
            ("L007", "projects/alpha/decision.md"),
            ("L008", "projects/alpha/old-c.md"),
            ("L009", "projects/alpha/badfm.md"),
            ("L010", "INDEX.md"),
            ("L011", "projects/alpha/aliaslink.md"),
        ]
    );
    let l002 = &found[1].2;
    assert!(
        l002.contains("projects/alpha/Dup.md") && l002.contains("projects/alpha/more/Dup.md"),
        "{l002}"
    );
}

#[test]
fn text_output_is_rule_path_detail_per_line() {
    let sb = Sandbox::new();
    let b = lint_repo(&sb);
    let out = lint(&sb, &b, &[]).success().get_output().stdout.clone();
    let text = String::from_utf8(out).unwrap();
    let line = text
        .lines()
        .find(|l| l.starts_with("L003 "))
        .expect("an L003 line");
    assert!(
        line.starts_with("L003 projects/alpha/orphan.md: "),
        "{line}"
    );
}

#[test]
fn an_uncommitted_edit_to_a_protected_note_is_one_l007() {
    let sb = Sandbox::new();
    let b = basic_repo(&sb);
    write(
        &b,
        "projects/alpha/rule.md",
        "---\nprotected: true\n---\n# Rule\n\nSee [[ME]].\n",
    );
    commit_all(&b, MANUAL, "fbrain: add rule");
    let clean = issues(&sb, &b, &[]);
    assert!(!rules(&clean).contains(&"L007"), "{clean:?}");
    write(
        &b,
        "projects/alpha/rule.md",
        "---\nprotected: true\n---\n# Rule\n\nSee [[ME]]. Edited in Obsidian.\n",
    );
    let found = issues(&sb, &b, &[]);
    let l007: Vec<_> = found.iter().filter(|i| i.0 == "L007").collect();
    assert_eq!(l007.len(), 1, "{found:?}");
    assert_eq!(l007[0].1, "projects/alpha/rule.md");
    assert_eq!(l007[0].2, "uncommitted edit to protected note");
}

#[test]
fn strict_honours_the_allowlist() {
    let sb = Sandbox::new();
    let b = lint_repo(&sb);
    // Fix what can be fixed and allow every remaining issue except the orphan.
    lint(&sb, &b, &["--fix", "--yes-i-am-faisal"]).success();
    let remaining = issues(&sb, &b, &[]);
    let base = fs::read_to_string(b.join(".fbrain.toml")).unwrap();
    let with_allow = |skip: &str| {
        let mut toml = format!("{base}\n[lint]\nallow = [\n");
        for (rule, path, _) in remaining.iter().filter(|i| i.0 != skip) {
            toml.push_str(&format!("  {{ rule = \"{rule}\", path = \"{path}\" }},\n"));
        }
        toml + "]\n"
    };
    fs::write(b.join(".fbrain.toml"), with_allow("L003")).unwrap();
    lint(&sb, &b, &["--strict"]).code(1);
    fs::write(b.join(".fbrain.toml"), with_allow("")).unwrap();
    let out = lint(&sb, &b, &["--strict"])
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let line = text.lines().find(|l| l.starts_with("L003 ")).unwrap();
    assert!(line.ends_with(" (allowed)"), "{line}");
}

#[test]
fn strict_exits_0_when_nothing_is_reported() {
    let sb = Sandbox::new();
    let b = sb.root.join("clean");
    write(&b, ".fbrain.toml", "name = \"clean\"\n");
    write(&b, "ME.md", "# ME\n");
    write(&b, "projects/a/BRIEF.md", "# A\n");
    lint(&sb, &b, &["--fix", "--yes-i-am-faisal"]).success();
    lint(&sb, &b, &["--strict"]).success();
    write(&b, "projects/a/lonely.md", "# Lonely\n");
    lint(&sb, &b, &["--strict"]).code(1);
}

#[test]
fn fix_regenerates_index_and_only_index_orphan_detection_is_unchanged() {
    let sb = Sandbox::new();
    let b = lint_repo(&sb);
    lint(&sb, &b, &["--fix", "--yes-i-am-faisal"]).success();
    assert!(read(&b, "INDEX.md")
        .starts_with("<!-- generated by fbrain lint --fix; do not edit -->\n# Index\n"));
    let found = issues(&sb, &b, &[]);
    let l003: Vec<_> = found.iter().filter(|i| i.0 == "L003").collect();
    assert_eq!(l003.len(), 1, "{found:?}");
    assert_eq!(l003[0].1, "projects/alpha/orphan.md");
}

#[test]
fn fix_applies_l008_l010_and_l011_and_nothing_else() {
    let sb = Sandbox::new();
    let b = lint_repo(&sb);
    let before = snapshot(&b);
    lint(&sb, &b, &["--fix", "--yes-i-am-faisal"]).success();
    let after = snapshot(&b);

    let changed: Vec<&str> = after
        .iter()
        .filter(|a| before.iter().find(|x| x.0 == a.0) != Some(a))
        .map(|a| a.0.as_str())
        .collect();
    assert_eq!(
        changed,
        vec![
            "INDEX.md",
            "projects/alpha/aliaslink.md",
            "projects/alpha/old-c.md"
        ]
    );
    assert_eq!(after.len(), before.len());

    assert_eq!(
        read(&b, "projects/alpha/old-c.md"),
        "---\nstatus: invalidated\nvalid_to: 2026-01-01\n---\n# Old C\n\nSee [[projects/alpha/BRIEF|Alpha]].\n"
    );
    assert_eq!(
        read(&b, "projects/alpha/aliaslink.md"),
        "# Alias link\n\nنص [[me/working-style|wstyle]] and [[me/working-style|x]] and `[[wstyle]]` in code.\n"
    );
    assert!(read(&b, "INDEX.md").contains("[[projects/alpha/BRIEF|alpha brief]]"));

    let found = issues(&sb, &b, &[]);
    for gone in ["L008", "L010", "L011"] {
        assert!(
            !rules(&found).contains(&gone),
            "{gone} still reported: {found:?}"
        );
    }
    assert_eq!(found.len(), 8, "{found:?}");
    assert_eq!(snapshot(&b), after, "a second lint must not write");
}

#[test]
fn fix_edits_a_body_only_to_rewrite_the_alias_link() {
    let sb = Sandbox::new();
    let b = lint_repo(&sb);
    let before = snapshot(&b);
    lint(&sb, &b, &["--fix", "--yes-i-am-faisal"]).success();
    for (rel, text) in snapshot(&b) {
        if rel == "INDEX.md" {
            continue;
        }
        let old = &before.iter().find(|x| x.0 == rel).unwrap().1;
        let same = body_of(old) == body_of(&text);
        assert_eq!(same, rel != "projects/alpha/aliaslink.md", "{rel}");
    }
}

#[test]
fn fixes_keep_crlf_line_endings() {
    let sb = Sandbox::new();
    let b = basic_repo(&sb);
    write(
        &b,
        "projects/alpha/old.md",
        "---\r\nstatus: invalidated\r\n---\r\n# Old\r\n\r\nSee [[wstyle]].\r\n",
    );
    commit_all(&b, MANUAL, "fbrain: add old");
    lint(&sb, &b, &["--fix", "--yes-i-am-faisal"]).success();
    assert_eq!(
        read(&b, "projects/alpha/old.md"),
        "---\r\nstatus: invalidated\r\nvalid_to: 2026-01-10\r\n---\r\n# Old\r\n\r\nSee [[me/working-style|wstyle]].\r\n"
    );
}

#[test]
fn index_for_the_basic_fixture_matches_the_golden_file() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    lint(&sb, &b, &["--fix", "--yes-i-am-faisal"]).success();
    let golden = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden/basic-INDEX.md"),
    )
    .unwrap();
    assert_eq!(read(&b, "INDEX.md"), golden);
}

#[test]
fn fix_leaves_protected_notes_alone() {
    let sb = Sandbox::new();
    let b = basic_repo(&sb);
    let text = "---\nprotected: true\nstatus: invalidated\n---\n# Rule\n\nSee [[wstyle]].\n";
    write(&b, "projects/alpha/rule.md", text);
    commit_all(&b, MANUAL, "fbrain: add rule");
    lint(&sb, &b, &["--fix", "--yes-i-am-faisal"]).success();
    assert_eq!(read(&b, "projects/alpha/rule.md"), text);
    let found = issues(&sb, &b, &[]);
    assert!(found
        .iter()
        .any(|i| i.0 == "L008" && i.1 == "projects/alpha/rule.md"));
    assert!(found
        .iter()
        .any(|i| i.0 == "L011" && i.1 == "projects/alpha/rule.md"));
}
