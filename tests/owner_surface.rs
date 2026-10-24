//! `--private` and the owner paths belong to Faisal at a terminal: stdin is a terminal and no
//! agent marker is set. This is a convention, not a boundary.
#![cfg(unix)]
mod common;

use common::{owner_output, Sandbox};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

const SECRET: &str = "---\nprivate: true\nstatus: active\n---\n# Salary record\n\nZebrafish payroll consent figures.\n";

fn brain(sb: &Sandbox) -> PathBuf {
    let b = sb.root.join("brain");
    fs::create_dir_all(b.join("notes")).unwrap();
    fs::write(b.join(".fbrain.toml"), "name = \"t\"\n").unwrap();
    fs::write(b.join("notes/salary.md"), SECRET).unwrap();
    b
}

fn cmd(sb: &Sandbox, b: &Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Command {
    let mut c = sb.std_cmd();
    for (k, v) in env {
        c.env(k, v);
    }
    c.arg("--brain").arg(b).args(args);
    c
}

/// stdin is a pipe: not a terminal, so not the owner.
fn piped(sb: &Sandbox, b: &Path, args: &[&str]) -> Output {
    cmd(sb, b, args, &[])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn private_flag_is_refused_with_exit_3_for_a_pipe_and_for_each_agent_marker() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    for args in [
        &["search", "--private", "zebrafish"][..],
        &["get", "--private", "notes/salary.md"][..],
        &["lint", "--private"][..],
    ] {
        let o = piped(&sb, &b, args);
        assert_eq!(o.status.code(), Some(3), "{args:?}: {}", text(&o));
        assert!(!text(&o).contains("Zebrafish") && !text(&o).contains("Salary"));
        for env in [
            ("FBRAIN_HARNESS", "codex"),
            ("CLAUDE_CODE_CHILD_SESSION", "1"),
        ] {
            let o = owner_output(cmd(&sb, &b, args, &[env]));
            assert_eq!(o.status.code(), Some(3), "{args:?} {env:?}: {}", text(&o));
            assert!(!text(&o).contains("Zebrafish") && !text(&o).contains("Salary"));
        }
    }
}

#[test]
fn the_refusal_does_not_say_whether_the_note_exists() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let real = text(&piped(&sb, &b, &["get", "--private", "notes/salary.md"]));
    let fake = text(&piped(&sb, &b, &["get", "--private", "notes/nothing.md"]));
    assert_eq!(real.replace("salary", "X"), fake.replace("nothing", "X"));
    assert!(!real.contains("not found"), "{real}");
}

#[test]
fn the_owner_on_a_terminal_can_use_private_flag() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let o = owner_output(cmd(&sb, &b, &["get", "--private", "notes/salary.md"], &[]));
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("Zebrafish"));
    let o = owner_output(cmd(&sb, &b, &["search", "--private", "zebrafish"], &[]));
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("notes/salary.md"));
}

#[test]
fn private_flag_is_in_no_help_text() {
    let sb = Sandbox::new();
    for sub in ["search", "get", "lint"] {
        let o = sb
            .cmd()
            .args([sub, "--help"])
            .assert()
            .success()
            .get_output()
            .clone();
        assert!(!text(&o).contains("--private"), "{sub}: {}", text(&o));
    }
}

/// A private ME.md, as the session brief test uses.
fn private_me_brain(sb: &Sandbox) -> (PathBuf, PathBuf, PathBuf) {
    let b = sb.root.join("brain");
    fs::create_dir_all(&b).unwrap();
    fs::write(b.join(".fbrain.toml"), "name = \"t\"\n").unwrap();
    fs::write(
        b.join("ME.md"),
        "---\nprivate: true\n---\n# Me\n\n## Role\n\nSecret role.\n",
    )
    .unwrap();
    let repo = sb.root.join("shop");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("README.md"), "# Shop\n\nShop sells candles.\n").unwrap();
    let answers = sb.root.join("answers.toml");
    fs::write(&answers, "role = \"Founder\"\ngoals = \"Ship v1\"\n").unwrap();
    (b, repo, answers)
}

fn onboard(sb: &Sandbox, b: &Path, repo: &Path, answers: &Path, env: &[(&str, &str)]) -> Output {
    let mut c = cmd(sb, b, &["onboard", "--repo"], env);
    c.arg(repo).arg("--answers").arg(answers);
    owner_output(c)
}

fn inbox_has_me(b: &Path) -> bool {
    fs::read_dir(b.join("inbox")).is_ok_and(|d| {
        d.flatten()
            .any(|e| fs::read_to_string(e.path()).is_ok_and(|t| t.contains("target: ME.md")))
    })
}

#[test]
fn onboard_over_a_private_me_works_for_the_owner_and_refuses_an_agent() {
    let sb = Sandbox::new();
    let (b, repo, answers) = private_me_brain(&sb);
    let o = onboard(&sb, &b, &repo, &answers, &[("FBRAIN_HARNESS", "codex")]);
    assert!(!o.status.success(), "{}", text(&o));
    assert!(!inbox_has_me(&b));
    let o = onboard(&sb, &b, &repo, &answers, &[]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(inbox_has_me(&b));
}
