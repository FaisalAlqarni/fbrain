mod common;

use common::Sandbox;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Std;

const STUB: &str = "# Me\n\n<!-- fbrain:init-stub -->\n";

fn git(dir: &Path, args: &[&str]) {
    let out = Std::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
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

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn commit_all(dir: &Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
}

/// A Ruby repo with a README, a CONTEXT.md, a docs folder and 5 commits.
fn ruby_repo(sb: &Sandbox, name: &str) -> PathBuf {
    let r = sb.root.join(name);
    fs::create_dir_all(&r).unwrap();
    git(&r, &["init", "-q"]);
    write(
        &r,
        "README.md",
        "# Shop\n\n[![ci](https://x/y.svg)](https://x)\n\nShop sells handmade candles to cafes.\nIt also runs the invoices.\n\n## Install\n\nrun bundle.\n",
    );
    write(&r, "CONTEXT.md", "# Context\n\nTerms.\n");
    write(&r, "docs/guide.md", "# Guide\n");
    write(&r, "docs/deploy.md", "# Deploy\n");
    commit_all(&r, "one");
    write(&r, "app/models/order.rb", "class Order; end\n");
    write(&r, "app/models/item.rb", "class Item; end\n");
    commit_all(&r, "two");
    write(&r, "app/controllers/orders.rb", "class Orders; end\n");
    commit_all(&r, "three");
    write(&r, "lib/tax.rb", "module Tax; end\n");
    commit_all(&r, "four");
    write(&r, "lib/tax.rb", "module Tax; VERSION = 1; end\n");
    commit_all(&r, "five");
    r
}

fn rust_repo(sb: &Sandbox, name: &str) -> PathBuf {
    let r = sb.root.join(name);
    fs::create_dir_all(&r).unwrap();
    git(&r, &["init", "-q"]);
    write(&r, "Cargo.toml", "[package]\nname = \"tool\"\n");
    write(&r, "src/main.rs", "fn main() {}\n");
    write(&r, "src/lib.rs", "pub fn f() {}\n");
    commit_all(&r, "init");
    r
}

/// The basic fixture as a committed git repo whose ME.md is `me`.
fn brain(sb: &Sandbox, me: &str) -> PathBuf {
    brain_at(sb, me, "brain")
}

fn brain_at(sb: &Sandbox, me: &str, dest: &str) -> PathBuf {
    let b = sb.root.join(dest);
    fs::rename(sb.fixture_brain("basic"), &b).unwrap();
    fs::write(b.join("ME.md"), me).unwrap();
    git(&b, &["init", "-q"]);
    commit_all(&b, "init");
    b
}

fn onboard(sb: &Sandbox, b: &Path) -> assert_cmd::Command {
    let mut c = sb.cmd();
    c.env("FBRAIN_TEST_PROPOSAL_TS", "20261023T120000Z")
        .arg("--brain")
        .arg(b)
        .arg("onboard");
    c
}

fn questions(sb: &Sandbox, b: &Path, repo: &Path) -> Value {
    let out = onboard(sb, b)
        .arg("--repo")
        .arg(repo)
        .args(["--print-questions", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).unwrap()
}

fn ids(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|q| q["id"].as_str().unwrap().to_string())
        .collect()
}

fn answers_toml(a: &Path, b: &Path) -> String {
    let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
    let mut s = String::from(
        "role = \"Founder\"\nproducts = \"Shop and Tool\"\nstack = \"Ruby, Rust\"\nmachines = \"WSL laptop\"\nwork_rules = \"Tests first\"\ngoals = \"Ship v1\"\nanti_goals = \"No cloud\"\nchat_language = \"English\"\n",
    );
    for p in [a, b] {
        s += &format!("\n[projects.{}]\n", name(p));
        for (k, v) in [
            ("what", "A thing"),
            ("customers", "Cafes"),
            ("modules", "app, lib"),
            ("stack", "Ruby"),
            ("environments", "Heroku"),
            ("status", "Live"),
            ("goals", "Grow"),
            ("risks", "None known"),
            ("related_repos", "none"),
        ] {
            s += &format!("{k} = \"{v}\"\n");
        }
    }
    s
}

fn inbox(b: &Path) -> Vec<(String, String)> {
    let mut v: Vec<_> = fs::read_dir(b.join("inbox"))
        .map(|d| {
            d.map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    fs::read_to_string(e.path()).unwrap(),
                )
            })
            .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn snapshot_outside_inbox(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            if e.path() == dir.join("inbox") || e.file_name() == ".git" {
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

#[test]
fn print_questions_json_carries_the_scan_and_a_prefill() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    let r = ruby_repo(&sb, "shop");
    let v = questions(&sb, &b, &r);
    let p = &v["projects"][0];
    assert_eq!(p["scan"]["languages"][0][0], "rb");
    assert_eq!(p["scan"]["languages"][0][1], 4);
    assert_eq!(p["scan"]["commits"], 5);
    assert!(p["scan"]["context_files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f == "CONTEXT.md"));
    let what = p["questions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["id"] == "what")
        .unwrap();
    assert_eq!(
        what["prefill"],
        "Shop sells handmade candles to cafes. It also runs the invoices."
    );
    assert_eq!(
        ids(&v["me"]),
        [
            "role",
            "products",
            "stack",
            "machines",
            "work_rules",
            "goals",
            "anti_goals",
            "chat_language"
        ]
    );
}

#[test]
fn a_question_settled_by_an_existing_note_is_not_asked() {
    let sb = Sandbox::new();
    let b = brain(
        &sb,
        "# Me\n\n## Role\n\nFounder of two shops.\n\n## Goals\n\n\n",
    );
    let r = ruby_repo(&sb, "shop");
    let v = questions(&sb, &b, &r);
    let me = ids(&v["me"]);
    assert!(!me.contains(&"role".to_string()), "{me:?}");
    assert!(
        me.contains(&"goals".to_string()),
        "empty section still asked"
    );
}

#[test]
fn onboard_writes_one_me_and_one_brief_proposal_per_repo_and_nothing_else() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    let (a, c) = (ruby_repo(&sb, "shop"), rust_repo(&sb, "tool"));
    let before = snapshot_outside_inbox(&b);
    let f = sb.root.join("answers.toml");
    fs::write(&f, answers_toml(&a, &c)).unwrap();
    onboard(&sb, &b)
        .arg("--repo")
        .arg(&a)
        .arg("--repo")
        .arg(&c)
        .arg("--answers")
        .arg(&f)
        .assert()
        .success();
    let files = inbox(&b);
    assert_eq!(files.len(), 3, "{files:?}");
    for (_, t) in &files {
        assert!(t.contains("proposed_by: onboard"), "{t}");
        assert!(t.contains("source: onboard scan "), "{t}");
    }
    let all: String = files.iter().map(|f| f.1.clone()).collect();
    assert!(all.contains("target: ME.md"));
    assert!(all.contains("op: update"));
    assert!(all.contains("target: projects/shop/BRIEF.md"));
    assert!(all.contains("target: projects/tool/BRIEF.md"));
    assert!(all.contains("## Role\n\nFounder"));
    assert!(all.contains("## Customers\n\nCafes"));
    assert!(all.contains("## Scan"));
    assert_eq!(snapshot_outside_inbox(&b), before);
}

#[test]
fn approving_the_brief_maps_the_repo_so_the_session_brief_finds_it() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    let (a, c) = (ruby_repo(&sb, "shop"), rust_repo(&sb, "tool"));
    let f = sb.root.join("answers.toml");
    fs::write(&f, answers_toml(&a, &c)).unwrap();
    onboard(&sb, &b)
        .args(["--repo"])
        .arg(&a)
        .arg("--repo")
        .arg(&c)
        .arg("--answers")
        .arg(&f)
        .assert()
        .success();
    commit_all(&b, "proposals");
    let shown = canon(&a);
    let (id, text) = inbox(&b)
        .into_iter()
        .find(|(_, t)| t.contains("target: projects/shop/BRIEF.md"))
        .unwrap();
    assert!(text.contains(&format!("map_repo: {shown}")), "{text}");
    let id = id.trim_end_matches(".md").to_string();
    sb.cmd()
        .arg("--brain")
        .arg(&b)
        .args(["review", &id, "--approve", "--yes-i-am-faisal"])
        .assert()
        .success();
    let brief = fs::read_to_string(b.join("projects/shop/BRIEF.md")).unwrap();
    assert!(brief.contains("description: A thing"), "{brief}");
    assert!(brief.contains("projects: [shop]"), "{brief}");
    let files = Std::new("git")
        .current_dir(&b)
        .args(["show", "--name-only", "--format=", "HEAD"])
        .output()
        .unwrap()
        .stdout;
    assert!(String::from_utf8_lossy(&files).contains(".fbrain.toml"));
    let cfg: toml::Table = fs::read_to_string(b.join(".fbrain.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(cfg["projects"]["shop"]["repos"][0].as_str().unwrap(), shown);
    let out = sb
        .cmd()
        .current_dir(&a)
        .arg("--brain")
        .arg(&b)
        .args(["brief", "--harness", "opencode-v1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("### Project: shop"));
}

#[test]
fn review_refuses_a_map_repo_that_is_not_an_existing_absolute_directory() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    write(
        &b,
        "inbox/20261023T120000Z-brief.md",
        "---\nop: add\ntarget: projects/x/BRIEF.md\nproposed_by: onboard\nsource: s\nrationale: r\nconflicts: []\nconflicts_protected: false\nmap_repo: relative/dir\n---\n# X\n",
    );
    commit_all(&b, "p");
    sb.cmd()
        .arg("--brain")
        .arg(&b)
        .args([
            "review",
            "20261023T120000Z-brief",
            "--approve",
            "--yes-i-am-faisal",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("map_repo"));
    assert!(!b.join("projects/x/BRIEF.md").exists());
}

#[test]
fn piped_answers_give_the_same_proposals_as_an_answers_file() {
    let sb = Sandbox::new();
    let a = ruby_repo(&sb, "shop");
    let (b1, b2) = (brain_at(&sb, STUB, "brain1"), brain_at(&sb, STUB, "brain2"));
    let mut file = String::new();
    let mut lines = String::new();
    for (k, v) in [
        ("role", "Founder"),
        ("products", "Shop"),
        ("stack", "Ruby"),
        ("machines", "WSL"),
        ("work_rules", "Tests"),
        ("goals", "Ship"),
        ("anti_goals", "Cloud"),
        ("chat_language", "English"),
    ] {
        file += &format!("{k} = \"{v}\"\n");
        lines += &format!("{v}\n");
    }
    file += "\n[projects.shop]\n";
    for (k, v) in [
        ("what", "A thing"),
        ("customers", "Cafes"),
        ("modules", "app"),
        ("stack", "Ruby"),
        ("environments", "Heroku"),
        ("status", "Live"),
        ("goals", "Grow"),
        ("risks", "None"),
        ("related_repos", "none"),
    ] {
        file += &format!("{k} = \"{v}\"\n");
        lines += &format!("{v}\n");
    }
    let f = sb.root.join("a.toml");
    fs::write(&f, file).unwrap();
    onboard(&sb, &b1)
        .arg("--repo")
        .arg(&a)
        .arg("--answers")
        .arg(&f)
        .assert()
        .success();
    onboard(&sb, &b2)
        .arg("--repo")
        .arg(&a)
        .write_stdin(lines)
        .assert()
        .success();
    assert_eq!(inbox(&b1), inbox(&b2));
    assert_eq!(inbox(&b1).len(), 2);
}

// ---- fix round 1 ----

fn run_answers(
    sb: &Sandbox,
    b: &Path,
    repos: &[&Path],
    answers: &str,
) -> assert_cmd::assert::Assert {
    let f = sb.root.join(format!("answers-{}.toml", repos.len()));
    fs::write(&f, answers).unwrap();
    let mut c = onboard(sb, b);
    for r in repos {
        c.arg("--repo").arg(r);
    }
    c.arg("--answers").arg(&f).assert()
}

fn proposal_with(b: &Path, needle: &str) -> (String, String) {
    inbox(b)
        .into_iter()
        .find(|(_, t)| t.contains(needle))
        .map(|(n, t)| (n.trim_end_matches(".md").to_string(), t))
        .unwrap_or_else(|| panic!("no proposal with {needle}"))
}

fn approve(sb: &Sandbox, b: &Path, id: &str) -> assert_cmd::assert::Assert {
    commit_all(b, "proposals");
    sb.cmd()
        .arg("--brain")
        .arg(b)
        .args(["review", id, "--approve", "--yes-i-am-faisal"])
        .assert()
}

const OLD_ME: &str =
    "# Me\n\nintro text\n\n## Role\n\nFounder of two shops.\n\n## Goals\n\n\n## Notes\n\nkeep me\n";

#[test]
fn onboard_over_an_me_with_content_fills_only_its_empty_sections() {
    let sb = Sandbox::new();
    let b = brain(&sb, OLD_ME);
    let r = ruby_repo(&sb, "shop");
    run_answers(
        &sb,
        &b,
        &[&r],
        "role = \"Ignored\"\ngoals = \"Ship v1\"\nmachines = \"WSL\"\n",
    )
    .success();
    let (id, t) = proposal_with(&b, "target: ME.md");
    assert!(t.contains("op: update"), "{t}");
    assert!(
        t.contains("intro text\n\n## Role\n\nFounder of two shops.\n\n## Goals\n\nShip v1\n\n\n## Notes\n\nkeep me\n"),
        "{t}"
    );
    assert!(!t.contains("Ignored"), "{t}");
    assert!(t.contains("## Machines\n\nWSL\n"), "{t}");
    approve(&sb, &b, &id).success();
    assert!(fs::read_to_string(b.join("ME.md"))
        .unwrap()
        .contains("## Goals\n\nShip v1"));
}

#[test]
fn onboard_over_the_old_init_stub_proposes_an_update() {
    let sb = Sandbox::new();
    let b = brain(&sb, "# ME\n");
    let r = ruby_repo(&sb, "shop");
    run_answers(&sb, &b, &[&r], "role = \"Founder\"\n").success();
    let (_, t) = proposal_with(&b, "target: ME.md");
    assert!(t.contains("op: update"), "{t}");
    assert!(t.contains("# ME\n"), "{t}");
    assert!(t.contains("## Role\n\nFounder\n"), "{t}");
}

const FIRST: &str = "[projects.shop]\nwhat = \"A thing\"\ncustomers = \"Cafes\"\n";

#[test]
fn rerun_merges_into_the_approved_brief_and_remaps_the_repo() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    let r = ruby_repo(&sb, "shop");
    run_answers(&sb, &b, &[&r], FIRST).success();
    let (id, _) = proposal_with(&b, "target: projects/shop/BRIEF.md");
    approve(&sb, &b, &id).success();
    // The mapping is gone (for example the user removed it); a rerun can restore it.
    fs::write(b.join(".fbrain.toml"), "name = \"basic\"\n").unwrap();
    commit_all(&b, "unmap");
    let q = questions(&sb, &b, &r);
    let open = ids(&q["projects"][0]["questions"]);
    assert!(!open.contains(&"what".to_string()), "{open:?}");
    assert!(!open.contains(&"customers".to_string()), "{open:?}");
    assert!(open.contains(&"risks".to_string()), "{open:?}");
    run_answers(&sb, &b, &[&r], "[projects.shop]\nrisks = \"Cash\"\n").success();
    let (id, t) = proposal_with(&b, "## Risks");
    assert!(t.contains("op: update"), "{t}");
    assert!(t.contains("map_repo:"), "{t}");
    assert!(t.contains("## Customers\n\nCafes\n"), "{t}");
    assert!(t.contains("## Risks\n\nCash\n"), "{t}");
    approve(&sb, &b, &id).success();
    let cfg = fs::read_to_string(b.join(".fbrain.toml")).unwrap();
    assert!(cfg.contains("[projects.shop]"), "{cfg}");
    let brief = fs::read_to_string(b.join("projects/shop/BRIEF.md")).unwrap();
    assert!(brief.contains("## Customers\n\nCafes"), "{brief}");
    assert!(brief.contains("## Risks\n\nCash"), "{brief}");
}

#[test]
fn text_taken_from_the_scan_is_marked_unreviewed() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    let r = ruby_repo(&sb, "shop");
    run_answers(
        &sb,
        &b,
        &[&r],
        "[projects.shop]\nstack = \"Ruby on Rails\"\n",
    )
    .success();
    let (_, t) = proposal_with(&b, "target: projects/shop/BRIEF.md");
    let label = "(from scan, unreviewed)";
    assert!(
        t.contains(&format!(
            "Shop sells handmade candles to cafes. It also runs the invoices. {label}"
        )),
        "{t}"
    );
    assert!(t.contains("## Stack\n\nRuby on Rails\n"), "{t}");
    let desc = t.lines().find(|l| l.starts_with("description:")).unwrap();
    assert!(desc.contains(label), "{desc}");
    let (_, me) = proposal_with(&b, "target: ME.md");
    assert!(me.contains(&format!("Ruby {label}")), "{me}");
}

fn hand_proposal(b: &Path, target: &str, extra: &str) -> String {
    let id = "20261023T120000Z-hand".to_string();
    write(
        b,
        &format!("inbox/{id}.md"),
        &format!("---\nop: add\ntarget: {target}\nproposed_by: onboard\nsource: s\nrationale: r\nconflicts: []\nconflicts_protected: false\n{extra}---\n# X\n"),
    );
    id
}

fn approve_fails_with(sb: &Sandbox, b: &Path, id: &str, msg: &str) {
    approve(sb, b, id)
        .failure()
        .stderr(predicates::str::contains(msg.to_string()));
}

#[test]
fn map_repo_is_stored_canonical() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    let r = ruby_repo(&sb, "shop");
    let odd = format!("{}/../shop", r.display());
    let id = hand_proposal(
        &b,
        "projects/shop/BRIEF.md",
        &format!("map_repo: '{odd}'\n"),
    );
    approve(&sb, &b, &id).success();
    let cfg: toml::Table = fs::read_to_string(b.join(".fbrain.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        cfg["projects"]["shop"]["repos"][0].as_str().unwrap(),
        canon(&r)
    );
}

#[test]
fn map_repo_refuses_root_home_the_brain_and_its_ancestors() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    let cases = [
        "/".to_string(),
        sb.root.join("home").display().to_string(),
        b.display().to_string(),
        sb.root.display().to_string(),
    ];
    for c in cases {
        let id = hand_proposal(&b, "projects/shop/BRIEF.md", &format!("map_repo: '{c}'\n"));
        approve_fails_with(&sb, &b, &id, "map_repo");
        assert!(!b.join("projects/shop").exists(), "{c}");
        fs::remove_file(b.join(format!("inbox/{id}.md"))).unwrap();
    }
}

#[test]
fn map_repo_cannot_add_a_second_repo_to_a_project_that_maps_another() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    let (r, other) = (ruby_repo(&sb, "shop"), rust_repo(&sb, "tool"));
    fs::write(
        b.join(".fbrain.toml"),
        format!(
            "name = \"basic\"\n\n[projects.shop]\nrepos = ['{}']\n",
            canon(&other)
        ),
    )
    .unwrap();
    let id = hand_proposal(
        &b,
        "projects/shop/BRIEF.md",
        &format!("map_repo: '{}'\n", r.display()),
    );
    approve_fails_with(&sb, &b, &id, "already maps");
    fs::remove_file(b.join(format!("inbox/{id}.md"))).unwrap();
    // The same path again is accepted.
    let id = hand_proposal(
        &b,
        "projects/shop/BRIEF.md",
        &format!("map_repo: '{}'\n", other.display()),
    );
    approve(&sb, &b, &id).success();
}

#[test]
fn review_caps_description_and_projects() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    let r = ruby_repo(&sb, "shop");
    let long = "d".repeat(300);
    let id = hand_proposal(
        &b,
        "projects/shop/BRIEF.md",
        &format!(
            "description: {long}\nprojects: [shop]\nmap_repo: '{}'\n",
            r.display()
        ),
    );
    approve(&sb, &b, &id).success();
    let brief = fs::read_to_string(b.join("projects/shop/BRIEF.md")).unwrap();
    let d = brief
        .lines()
        .find(|l| l.starts_with("description:"))
        .unwrap();
    assert_eq!(
        d.trim_start_matches("description: ")
            .trim_matches('"')
            .len(),
        200,
        "{d}"
    );
    for bad in [
        "[other]",
        "[shop, a, b, c, d, e, f, g, h]",
        "[\"bad name\"]",
    ] {
        let id = hand_proposal(&b, "projects/neo/BRIEF.md", &format!("projects: {bad}\n"));
        approve_fails_with(&sb, &b, &id, "projects");
        assert!(!b.join("projects/neo").exists(), "{bad}");
        fs::remove_file(b.join(format!("inbox/{id}.md"))).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn a_failed_approve_leaves_no_new_directory_behind() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    let r = ruby_repo(&sb, "shop");
    let real = sb.root.join("real.toml");
    fs::write(&real, "name = \"basic\"\n").unwrap();
    fs::remove_file(b.join(".fbrain.toml")).unwrap();
    std::os::unix::fs::symlink(&real, b.join(".fbrain.toml")).unwrap();
    let id = hand_proposal(
        &b,
        "projects/shop/BRIEF.md",
        &format!("map_repo: '{}'\n", r.display()),
    );
    approve(&sb, &b, &id).failure();
    assert!(!b.join("projects/shop").exists());
}

// ---- fix round 2 ----

#[cfg(unix)]
#[test]
fn approving_into_a_brain_reached_through_a_symlink_creates_the_project_folder() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    let r = ruby_repo(&sb, "shop");
    let link = sb.root.join("brain-link");
    std::os::unix::fs::symlink(&b, &link).unwrap();
    let id = hand_proposal(
        &b,
        "projects/shop/BRIEF.md",
        &format!("map_repo: '{}'\n", r.display()),
    );
    commit_all(&b, "proposals");
    sb.cmd()
        .arg("--brain")
        .arg(&link)
        .args(["review", &id, "--approve", "--yes-i-am-faisal"])
        .assert()
        .success();
    assert!(b.join("projects/shop/BRIEF.md").is_file());
}

#[cfg(unix)]
#[test]
fn a_symlinked_folder_below_the_brain_root_is_still_refused() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    std::os::unix::fs::symlink(b.join("projects/alpha"), b.join("projects/alias")).unwrap();
    let id = hand_proposal(&b, "projects/alias/deep/BRIEF.md", "");
    approve(&sb, &b, &id).failure();
    assert!(!b.join("projects/alpha/deep").exists());
}

#[cfg(unix)]
#[test]
fn lint_fix_writes_into_a_brain_reached_through_a_symlink() {
    let sb = Sandbox::new();
    let b = brain(&sb, STUB);
    write(
        &b,
        "projects/alpha/old.md",
        "---\nstatus: invalidated\n---\n# Old\n",
    );
    commit_all(&b, "old");
    let link = sb.root.join("brain-link");
    std::os::unix::fs::symlink(&b, &link).unwrap();
    sb.cmd()
        .arg("--brain")
        .arg(&link)
        .args(["lint", "--fix", "--yes-i-am-faisal"])
        .assert()
        .success();
    assert!(fs::read_to_string(b.join("projects/alpha/old.md"))
        .unwrap()
        .contains("valid_to:"));
}

/// The canonical path as fbrain stores and shows it: without Windows' `\\?\` prefix.
fn canon(p: &Path) -> String {
    let c = p.canonicalize().unwrap().display().to_string();
    c.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(c)
}
