use predicates::prelude::PredicateStrExt;
mod common;

use common::Sandbox;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn init(sb: &Sandbox, name: &str) -> PathBuf {
    let p = sb.root.join(name);
    sb.cmd()
        .args(["init"])
        .arg(&p)
        .args(["--name", name, "--no-model"])
        .assert()
        .success();
    p
}

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path(), fs::read(e.path()).unwrap());
            }
        }
    }
    out
}

/// `--private` is for Faisal at a terminal; a note with broken frontmatter needs it.
#[cfg(unix)]
fn get_json_private(sb: &Sandbox, brain: &Path, note: &str) -> Value {
    let mut c = sb.std_cmd();
    c.args(["get", note, "--private", "--json", "--brain"])
        .arg(brain);
    let out = common::owner_output(c);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn get_json(sb: &Sandbox, brain: &Path, note: &str) -> Value {
    let out = sb
        .cmd()
        .args(["get", note, "--json", "--brain"])
        .arg(brain)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn init_creates_layout_git_and_default_registration() {
    let sb = Sandbox::new();
    let b = init(&sb, "b");
    for d in ["me", "setup", "projects", "inbox", ".git"] {
        assert!(b.join(d).is_dir(), "{d}");
    }
    assert!(b.join("ME.md").is_file());
    assert!(fs::read_to_string(b.join(".fbrain.toml"))
        .unwrap()
        .contains("name = \"b\""));
    sb.cmd()
        .arg("brains")
        .assert()
        .success()
        .stdout(predicates_contains("b (default)"));
}

fn predicates_contains(
    s: &str,
) -> predicates::str::Utf8Predicate<predicates::str::ContainsPredicate> {
    predicates::str::contains(s.to_string()).from_utf8()
}

#[test]
fn init_refuses_existing_brain() {
    let sb = Sandbox::new();
    let b = init(&sb, "b");
    sb.cmd()
        .arg("init")
        .arg(&b)
        .args(["--name", "b", "--no-model"])
        .assert()
        .code(1)
        .stderr(predicates_contains(&format!(
            "brain already exists: {}",
            b.display()
        )));
}

#[test]
fn brains_lists_names_default_and_paths() {
    let sb = Sandbox::new();
    let a = init(&sb, "a");
    let b = init(&sb, "b");
    let out = sb.cmd().arg("brains").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(&format!("a (default)  {}", a.display())),
        "{text}"
    );
    assert!(text.contains(&format!("b  {}", b.display())), "{text}");
    assert!(!text.contains("b (default)"), "{text}");
}

#[test]
fn get_resolves_by_stem_and_by_rel_path() {
    let sb = Sandbox::new();
    let f = sb.fixture_brain("basic");
    for q in ["notes with spaces", "projects/alpha/notes with spaces.md"] {
        sb.cmd()
            .args(["get", q, "--brain"])
            .arg(&f)
            .assert()
            .success()
            .stdout(predicates_contains("Body of the spaced note."));
    }
}

#[test]
fn get_missing_note_fails() {
    let sb = Sandbox::new();
    let f = sb.fixture_brain("basic");
    sb.cmd()
        .args(["get", "missing", "--brain"])
        .arg(&f)
        .assert()
        .code(1)
        .stderr(predicates_contains("note not found: missing"));
}

#[test]
fn get_json_reports_status_title_and_rel_path() {
    let sb = Sandbox::new();
    let f = sb.fixture_brain("basic");
    let v = get_json(&sb, &f, "status");
    assert_eq!(v["rel_path"], "projects/alpha/status.md");
    assert_eq!(v["status"], "done");
    assert_eq!(v["title"], "Alpha status");
    assert!(v["frontmatter_error"].is_null());
    assert_eq!(v["frontmatter"]["valid_from"], "2026-10-23");
    assert_eq!(v["frontmatter"]["owner"], "faisal");
}

#[cfg(unix)]
#[test]
fn invalid_frontmatter_is_reported_not_fatal() {
    let sb = Sandbox::new();
    let f = sb.fixture_brain("basic");
    fs::write(
        f.join("bad.md"),
        "---\nstatus: [unclosed\n---\n# Bad\n\ntext\n",
    )
    .unwrap();
    let v = get_json_private(&sb, &f, "bad");
    assert!(v["frontmatter_error"].is_string());
    assert_eq!(v["status"], "active");
}

#[test]
fn brain_selection_follows_precedence() {
    let sb = Sandbox::new();
    init(&sb, "a");
    let b = init(&sb, "b");
    let repos = sb.root.join("repos/x");
    fs::create_dir_all(&repos).unwrap();
    let cfg = sb.config_dir().join("config.toml");
    let mut text = fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!(
        "\n[[map]]\nrepo_prefix = {:?}\nbrain = \"b\"\n",
        sb.root.join("repos").to_str().unwrap()
    ));
    fs::write(&cfg, text).unwrap();

    let which = |cwd: &Path, env: Option<&str>| {
        let mut c = sb.cmd();
        c.current_dir(cwd).args(["brains", "--which"]);
        if let Some(e) = env {
            c.env("FBRAIN_BRAIN", e);
        }
        String::from_utf8(c.output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string()
    };
    assert_eq!(which(&repos, None), "b");
    assert_eq!(which(&sb.root, None), "a");
    assert_eq!(which(&repos, Some("a")), "a");
    let explicit = sb
        .cmd()
        .current_dir(&sb.root)
        .args(["--brain"])
        .arg(&b)
        .args(["brains", "--which"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(explicit.stdout).unwrap().trim(), "b");
}

#[test]
fn brain_path_without_config_is_rejected() {
    let sb = Sandbox::new();
    let d = sb.root.join("plain");
    fs::create_dir_all(&d).unwrap();
    sb.cmd()
        .args(["get", "x", "--brain"])
        .arg(&d)
        .assert()
        .code(1)
        .stderr(predicates_contains(&format!(
            "not a brain: {} has no .fbrain.toml",
            d.display()
        )));
}

#[test]
fn adopt_registers_without_touching_existing_files() {
    let sb = Sandbox::new();
    let e = sb.root.join("existing");
    fs::create_dir_all(e.join("work")).unwrap();
    fs::write(e.join("work/a.md"), "# A\n").unwrap();
    fs::write(e.join("README.md"), "keep me\r\n").unwrap();
    let before = snapshot(&e);
    sb.cmd()
        .arg("init")
        .arg("--adopt")
        .arg(&e)
        .args(["--name", "e", "--no-model"])
        .assert()
        .success();
    let after = snapshot(&e);
    for (p, bytes) in &before {
        assert_eq!(after.get(p), Some(bytes), "{}", p.display());
    }
    assert!(e.join(".fbrain.toml").is_file());
    assert!(e.join("inbox").is_dir());
    sb.cmd()
        .arg("brains")
        .assert()
        .success()
        .stdout(predicates_contains("e (default)"));
}

#[test]
fn adopt_keeps_existing_brain_config_byte_identical() {
    let sb = Sandbox::new();
    let f = sb.fixture_brain("basic");
    let before = fs::read(f.join(".fbrain.toml")).unwrap();
    sb.cmd()
        .arg("init")
        .arg("--adopt")
        .arg(&f)
        .args(["--name", "basic", "--no-model"])
        .assert()
        .success();
    assert_eq!(fs::read(f.join(".fbrain.toml")).unwrap(), before);
    sb.cmd()
        .arg("brains")
        .assert()
        .success()
        .stdout(predicates_contains("basic"));
}

#[test]
fn adopt_include_limits_note_walk() {
    let sb = Sandbox::new();
    let e = sb.root.join("vault");
    fs::create_dir_all(e.join("work")).unwrap();
    fs::create_dir_all(e.join("other")).unwrap();
    fs::write(e.join("work/a.md"), "# A\n").unwrap();
    fs::write(e.join("other/b.md"), "# B\n").unwrap();
    sb.cmd()
        .arg("init")
        .arg("--adopt")
        .arg(&e)
        .args([
            "--name",
            "v",
            "--include",
            "work/",
            "--include",
            "brain/",
            "--no-model",
        ])
        .assert()
        .success();
    sb.cmd()
        .args(["get", "a", "--brain", "v"])
        .assert()
        .success();
    sb.cmd()
        .args(["get", "b", "--brain", "v"])
        .assert()
        .code(1)
        .stderr(predicates_contains("note not found: b"));
}

#[test]
fn hidden_folders_are_not_walked() {
    let sb = Sandbox::new();
    let f = sb.fixture_brain("basic");
    sb.cmd()
        .args(["get", "x", "--brain"])
        .arg(&f)
        .assert()
        .code(1)
        .stderr(predicates_contains("note not found: x"));
}

#[test]
fn crlf_note_parses_frontmatter_and_drops_carriage_returns() {
    let sb = Sandbox::new();
    let f = sb.fixture_brain("basic");
    let v = get_json(&sb, &f, "crlf");
    assert_eq!(v["status"], "done");
    assert_eq!(v["title"], "Windows note");
    assert!(!v["body"].as_str().unwrap().contains('\r'));
    assert!(v["body"].as_str().unwrap().contains("Line two."));
}

#[test]
fn init_with_taken_name_changes_nothing_and_force_recovers() {
    let sb = Sandbox::new();
    let first = init(&sb, "a");
    let second = sb.root.join("second");
    sb.cmd()
        .arg("init")
        .arg(&second)
        .args(["--name", "a", "--no-model"])
        .assert()
        .code(1)
        .stderr(predicates_contains("brain name already registered: a"));
    assert!(!second.exists(), "failed init must not create the folder");
    sb.cmd()
        .arg("init")
        .arg(&second)
        .args(["--name", "a", "--no-model", "--force"])
        .assert()
        .success();
    let out = sb.cmd().arg("brains").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(&format!("a (default)  {}", second.display())),
        "{text}"
    );
    assert!(!text.contains(&first.display().to_string()), "{text}");
}

#[test]
fn adopt_with_taken_name_changes_nothing() {
    let sb = Sandbox::new();
    init(&sb, "a");
    let e = sb.root.join("existing");
    fs::create_dir_all(&e).unwrap();
    sb.cmd()
        .arg("init")
        .arg("--adopt")
        .arg(&e)
        .args(["--name", "a"])
        .assert()
        .code(1)
        .stderr(predicates_contains("brain name already registered: a"));
    assert!(!e.join(".fbrain.toml").exists());
    assert!(!e.join("inbox").exists());
}

#[cfg(unix)]
#[test]
fn get_text_mode_reports_frontmatter_error_on_stderr() {
    let sb = Sandbox::new();
    let f = sb.fixture_brain("basic");
    fs::write(
        f.join("bad.md"),
        "---\nstatus: [unclosed\n---\n# Bad\n\ntext\n",
    )
    .unwrap();
    let mut c = sb.std_cmd();
    c.args(["get", "bad", "--private", "--brain"]).arg(&f);
    let o = common::owner_output(c);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("text"));
    assert!(String::from_utf8_lossy(&o.stderr).contains("frontmatter error in bad.md"));
}
