mod common;

use common::Sandbox;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Std;

const GUARD: &str = "refusing non-interactive review: proposals are approved by Faisal";

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8(out.stdout).unwrap()
}

/// UTC today as `YYYY-MM-DD`.
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let z = secs.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The basic fixture as a committed git repo.
fn repo(sb: &Sandbox) -> PathBuf {
    let b = sb.fixture_brain("basic");
    git(&b, &["init", "-q"]);
    git(&b, &["add", "-A"]);
    git(&b, &["commit", "-q", "-m", "init"]);
    b
}

/// Files a proposal through the CLI, commits it, and returns its id.
fn propose(sb: &Sandbox, b: &Path, args: &[&str], body: &str) -> String {
    let id = propose_untracked(sb, b, args, body);
    git(b, &["add", "-A"]);
    git(b, &["commit", "-q", "-m", "proposal"]);
    id
}

fn propose_untracked(sb: &Sandbox, b: &Path, args: &[&str], body: &str) -> String {
    let out = sb
        .cmd()
        .env("FBRAIN_TEST_PROPOSAL_TS", "20261023T120000Z")
        .arg("--brain")
        .arg(b)
        .args(["--json", "propose", "--rationale", "r", "--source", "s"])
        .args(["--by", "codex"])
        .args(args)
        .write_stdin(body.to_string())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    v["id"].as_str().unwrap().to_string()
}

fn review(sb: &Sandbox, b: &Path, id: &str, args: &[&str]) -> assert_cmd::assert::Assert {
    sb.cmd()
        .arg("--brain")
        .arg(b)
        .args(["review", id])
        .args(args)
        .arg("--yes-i-am-faisal")
        .assert()
}

fn read(b: &Path, rel: &str) -> String {
    fs::read_to_string(b.join(rel)).unwrap()
}

#[test]
fn approve_add_creates_the_note_stamps_it_and_makes_one_commit() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "# New Title\n\nFresh zebra facts.\n",
    );
    review(&sb, &b, &id, &["--approve"]).success();
    let note = read(&b, "projects/alpha/new.md");
    let (fm, body) = note
        .strip_prefix("---\n")
        .unwrap()
        .split_once("---\n")
        .unwrap();
    assert_eq!(body, "# New Title\n\nFresh zebra facts.\n");
    assert!(fm.starts_with(&format!("status: active\nvalid_from: {}\n", today())));
    assert!(fm.contains("proposed_by: codex\n"));
    assert!(fm.contains("source: s\n"));
    let at = fm
        .lines()
        .find_map(|l| l.strip_prefix("approved_at: "))
        .unwrap();
    assert!(
        at.starts_with(&today()) && at.ends_with('Z') && at.len() == 20,
        "{at}"
    );
    assert!(!b.join(format!("inbox/{id}.md")).exists());
    assert_eq!(
        git(&b, &["log", "-1", "--format=%s"]).trim(),
        format!("fbrain: add projects/alpha/new.md ({id})")
    );
    let mut stat: Vec<String> = git(&b, &["show", "--name-only", "--format=", "HEAD"])
        .lines()
        .map(String::from)
        .collect();
    stat.sort();
    assert_eq!(
        stat,
        vec![
            format!("inbox/{id}.md"),
            "projects/alpha/new.md".to_string()
        ]
    );
    assert_eq!(git(&b, &["status", "--porcelain"]), "");
}

#[test]
fn a_second_review_of_the_same_id_is_not_found() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "x\n",
    );
    review(&sb, &b, &id, &["--approve"]).success();
    review(&sb, &b, &id, &["--approve"])
        .code(1)
        .stderr(format!("proposal not found: {id}\n"));
}

#[test]
fn self_approval_guard_needs_the_flag_when_a_harness_is_set_or_stdin_is_not_a_tty() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "x\n",
    );
    let bare = |harness: Option<&str>| {
        let mut c = sb.cmd();
        if let Some(h) = harness {
            c.env("FBRAIN_HARNESS", h);
        }
        c.arg("--brain").arg(&b).args(["review", &id, "--approve"]);
        c
    };
    for h in [Some("codex"), None] {
        bare(h).assert().code(3).stderr(format!("{GUARD}\n"));
        assert!(!b.join("projects/alpha/new.md").exists());
    }
    bare(Some("codex"))
        .arg("--yes-i-am-faisal")
        .assert()
        .success();
    assert!(b.join("projects/alpha/new.md").exists());
}

#[test]
fn the_flag_is_in_no_help_text() {
    let sb = Sandbox::new();
    let help = |args: &[&str]| {
        String::from_utf8(
            sb.cmd()
                .args(args)
                .arg("--help")
                .assert()
                .success()
                .get_output()
                .stdout
                .clone(),
        )
        .unwrap()
    };
    assert!(!help(&["review"]).contains("yes-i-am-faisal"));
    assert!(!help(&[]).contains("yes-i-am-faisal"));
}

const OLD: &str =
    "---\nstatus: active\ntitle: Old Title\n---\n# Old Title\n\nThe oldonlyterm fact.\n";

fn with_old(sb: &Sandbox, front: &str) -> PathBuf {
    let b = repo(sb);
    let text = OLD.replace("title: Old Title\n", &format!("title: Old Title\n{front}"));
    fs::write(b.join("projects/alpha/old.md"), text).unwrap();
    git(&b, &["add", "-A"]);
    git(&b, &["commit", "-q", "-m", "old"]);
    b
}

fn head_subject(b: &Path) -> String {
    git(b, &["log", "-1", "--format=%s"]).trim().to_string()
}

fn strip_stamp(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.starts_with("approved_at: ") {
                "approved_at: T"
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[test]
fn approve_update_replaces_the_body_and_keeps_existing_frontmatter() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "update", "--target", "projects/alpha/status.md"],
        "# Alpha status\n\nReplaced.\n",
    );
    review(&sb, &b, &id, &["--approve"]).success();
    assert_eq!(
        strip_stamp(&read(&b, "projects/alpha/status.md")),
        "---\nstatus: done\nvalid_from: 2026-10-23\nowner: faisal\nproposed_by: codex\nsource: s\napproved_at: T\n---\n# Alpha status\n\nReplaced.\n"
    );
    assert_eq!(
        head_subject(&b),
        format!("fbrain: update projects/alpha/status.md ({id})")
    );
}

#[test]
fn approve_update_keeps_crlf_line_endings() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "update", "--target", "projects/alpha/crlf.md"],
        "line one\nline two\n",
    );
    review(&sb, &b, &id, &["--approve"]).success();
    let got = fs::read(b.join("projects/alpha/crlf.md")).unwrap();
    let got = String::from_utf8(got).unwrap();
    assert!(got.contains("\r\n"));
    assert!(
        !got.replace("\r\n", "").contains('\n'),
        "bare LF in {got:?}"
    );
    assert!(got.ends_with("line one\r\nline two\r\n"));
}

#[test]
fn supersede_marks_the_old_note_with_a_path_link_and_hides_it_from_search() {
    let sb = Sandbox::new();
    let b = with_old(&sb, "");
    let id = propose(
        &sb,
        &b,
        &[
            "--op",
            "supersede",
            "--target",
            "projects/alpha/new.md",
            "--supersedes",
            "projects/alpha/old.md",
        ],
        "# New Title\n\nThe replacement fact.\n",
    );
    review(&sb, &b, &id, &["--approve"]).success();
    let old = read(&b, "projects/alpha/old.md");
    assert_eq!(
        old,
        format!(
            "---\nstatus: superseded\ntitle: Old Title\nvalid_to: {}\nsuperseded_by: \"[[projects/alpha/new]]\"\n---\n# Old Title\n\nThe oldonlyterm fact.\n",
            today()
        )
    );
    assert!(read(&b, "projects/alpha/new.md").contains("status: active\n"));
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .args(["--json", "get", "projects/alpha/old.md", "--links"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["outgoing"][0]["resolved"], "projects/alpha/new.md");
    let search = |extra: &[&str]| -> Vec<serde_json::Value> {
        let out = sb
            .cmd()
            .arg("--brain")
            .arg(&b)
            .args(["--json", "search", "oldonlyterm"])
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()["hits"]
            .as_array()
            .unwrap()
            .clone()
    };
    assert!(search(&[]).is_empty());
    let hist = search(&["--history"]);
    assert_eq!(hist[0]["rel_path"], "projects/alpha/old.md");
    assert_eq!(hist[0]["status"], "superseded");
    assert_eq!(
        head_subject(&b),
        format!("fbrain: supersede projects/alpha/new.md ({id})")
    );
}

#[test]
fn supersede_candidate_applies_an_add_and_supersedes_the_named_note() {
    let sb = Sandbox::new();
    let b = with_old(&sb, "");
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "# New\n\nFresh.\n",
    );
    review(
        &sb,
        &b,
        &id,
        &["--supersede-candidate", "projects/alpha/old.md"],
    )
    .success();
    assert!(read(&b, "projects/alpha/new.md").contains("status: active\n"));
    let old = read(&b, "projects/alpha/old.md");
    assert!(old.contains("status: superseded\n"));
    assert!(old.contains("superseded_by: \"[[projects/alpha/new]]\"\n"));
    assert_eq!(git(&b, &["status", "--porcelain"]), "");
}

#[test]
fn invalidate_sets_status_and_valid_to_and_leaves_default_search() {
    let sb = Sandbox::new();
    let b = with_old(&sb, "");
    let id = propose(
        &sb,
        &b,
        &["--op", "invalidate", "--target", "projects/alpha/old.md"],
        "",
    );
    review(&sb, &b, &id, &["--approve"]).success();
    let old = read(&b, "projects/alpha/old.md");
    assert!(old.contains("status: invalidated\n"));
    assert!(old.contains(&format!("valid_to: {}\n", today())));
    assert!(old.ends_with("---\n# Old Title\n\nThe oldonlyterm fact.\n"));
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .args(["--json", "search", "oldonlyterm"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(v["hits"].as_array().unwrap().is_empty());
}

#[test]
fn merge_into_appends_a_dated_section_and_leaves_the_proposal_target_alone() {
    let sb = Sandbox::new();
    let b = with_old(&sb, "");
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "Merged text here.\n",
    );
    review(&sb, &b, &id, &["--merge-into", "projects/alpha/old.md"]).success();
    let old = read(&b, "projects/alpha/old.md");
    assert!(
        old.ends_with(&format!(
            "The oldonlyterm fact.\n\n## Merged {}\n\nMerged text here.\n",
            today()
        )),
        "{old}"
    );
    assert!(old.contains("proposed_by: codex\n") && old.contains("approved_at: "));
    assert!(!b.join("projects/alpha/new.md").exists());
    assert_eq!(
        head_subject(&b),
        format!("fbrain: merge projects/alpha/old.md ({id})")
    );
}

#[test]
fn reject_deletes_the_proposal_and_commits_when_it_was_tracked() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "x\n",
    );
    review(&sb, &b, &id, &["--reject"]).success();
    assert!(!b.join(format!("inbox/{id}.md")).exists());
    assert!(!b.join("projects/alpha/new.md").exists());
    assert_eq!(
        head_subject(&b),
        format!("fbrain: reject projects/alpha/new.md ({id})")
    );
    assert_eq!(git(&b, &["status", "--porcelain"]), "");
}

#[test]
fn reject_of_an_untracked_proposal_only_deletes_it() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose_untracked(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "x\n",
    );
    review(&sb, &b, &id, &["--reject"]).success();
    assert!(!b.join(format!("inbox/{id}.md")).exists());
    assert_eq!(head_subject(&b), "init");
}

#[test]
fn a_protected_note_needs_force() {
    let sb = Sandbox::new();
    let b = with_old(&sb, "protected: true\n");
    let id = propose(
        &sb,
        &b,
        &["--op", "update", "--target", "projects/alpha/old.md"],
        "changed\n",
    );
    let before = read(&b, "projects/alpha/old.md");
    review(&sb, &b, &id, &["--approve"])
        .code(2)
        .stderr("proposal touches protected note projects/alpha/old.md; rerun with --force\n");
    assert_eq!(read(&b, "projects/alpha/old.md"), before);
    assert!(b.join(format!("inbox/{id}.md")).exists());
    assert_eq!(git(&b, &["status", "--porcelain"]), "");
    review(&sb, &b, &id, &["--approve", "--force"]).success();
    assert!(read(&b, "projects/alpha/old.md").contains("changed\n"));
}

#[test]
fn outside_a_git_repo_it_applies_and_warns() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("basic");
    let id = propose_untracked(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "x\n",
    );
    review(&sb, &b, &id, &["--approve"])
        .success()
        .stderr(predicates::str::starts_with("warning: not committed:"));
    assert!(b.join("projects/alpha/new.md").exists());
    assert!(!b.join(format!("inbox/{id}.md")).exists());
}

#[test]
fn staged_changes_of_other_files_stay_staged_and_out_of_the_commit() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "x\n",
    );
    fs::write(b.join("scratch.md"), "mine\n").unwrap();
    git(&b, &["add", "scratch.md"]);
    review(&sb, &b, &id, &["--approve"]).success();
    assert_eq!(
        git(&b, &["diff", "--cached", "--name-only"]).trim(),
        "scratch.md"
    );
    assert!(!git(&b, &["show", "--name-only", "--format=", "HEAD"]).contains("scratch.md"));
}

#[test]
fn a_hand_edited_proposal_cannot_write_outside_the_notes() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    fs::create_dir_all(b.join("inbox")).unwrap();
    for (n, target) in [
        "../escape.md",
        "inbox/x.md",
        ".hidden/x.md",
        "projects/alpha/x.txt",
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("hand-{n}");
        fs::write(
            b.join(format!("inbox/{id}.md")),
            format!("---\nop: add\ntarget: {target}\nproposed_by: x\nsource: s\nrationale: r\nconflicts: []\nconflicts_protected: false\n---\nbody\n"),
        )
        .unwrap();
        review(&sb, &b, &id, &["--approve"]).code(2);
        assert!(b.join(format!("inbox/{id}.md")).exists());
    }
    assert!(!sb.root.join("escape.md").exists());
    assert!(!b.join("inbox/x.md").exists() && !b.join("projects/alpha/x.txt").exists());
}

#[cfg(unix)]
#[test]
fn a_symlink_in_the_target_path_is_refused() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let out = sb.root.join("outside");
    fs::create_dir(&out).unwrap();
    fs::create_dir_all(b.join("inbox")).unwrap();
    std::os::unix::fs::symlink(&out, b.join("projects/link")).unwrap();
    fs::write(
        b.join("inbox/hand.md"),
        "---\nop: add\ntarget: projects/link/x.md\nproposed_by: x\nsource: s\nrationale: r\nconflicts: []\nconflicts_protected: false\n---\nbody\n",
    )
    .unwrap();
    review(&sb, &b, "hand", &["--approve"]).code(2);
    assert!(!out.join("x.md").exists());
}

#[test]
fn frontmatter_in_a_proposal_body_is_dropped_and_cannot_set_flags() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "---\nprotected: true\nstatus: done\n---\nreal body\n",
    );
    review(&sb, &b, &id, &["--approve"]).success();
    let note = read(&b, "projects/alpha/new.md");
    assert!(
        !note.contains("protected") && note.contains("status: active\n"),
        "{note}"
    );
    assert!(note.ends_with("---\nreal body\n"));
}

#[cfg(unix)]
#[test]
fn edit_then_approve_applies_the_body_the_editor_leaves() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "original\n",
    );
    let ed = sb.root.join("ed.sh");
    fs::write(&ed, "#!/bin/sh\nprintf edited > \"$1\"\n").unwrap();
    fs::set_permissions(&ed, fs::Permissions::from_mode(0o755)).unwrap();
    sb.cmd()
        .env("EDITOR", &ed)
        .env_remove("VISUAL")
        .arg("--brain")
        .arg(&b)
        .args(["review", &id, "--edit", "--yes-i-am-faisal"])
        .assert()
        .success();
    assert!(read(&b, "projects/alpha/new.md").ends_with("---\nedited"));
}

#[cfg(unix)]
#[test]
fn a_failed_write_reports_the_path_leaves_no_temp_file_and_commits_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    let b = repo(&sb);
    fs::create_dir(b.join("projects/ro")).unwrap();
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/ro/new.md"],
        "x\n",
    );
    fs::set_permissions(b.join("projects/ro"), fs::Permissions::from_mode(0o555)).unwrap();
    let out = review(&sb, &b, &id, &["--approve"])
        .code(1)
        .get_output()
        .stderr
        .clone();
    fs::set_permissions(b.join("projects/ro"), fs::Permissions::from_mode(0o755)).unwrap();
    let err = String::from_utf8(out).unwrap();
    assert!(
        err.starts_with("could not write projects/ro/new.md: "),
        "{err}"
    );
    assert_eq!(fs::read_dir(b.join("projects/ro")).unwrap().count(), 0);
    assert_eq!(head_subject(&b), "proposal");
    assert!(b.join(format!("inbox/{id}.md")).exists());
}

#[cfg(unix)]
#[test]
fn interactive_review_through_a_terminal_approves_on_a() {
    use std::io::{Read, Write};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::Stdio;
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "x\n",
    );
    let (mut m, mut s) = (0, 0);
    let rc = unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0);
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
    let mut cmd = sb.std_cmd();
    cmd.arg("--brain").arg(&b).arg("review");
    cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    let mut child = cmd.spawn().unwrap();
    drop((cmd, slave));
    let mut master = std::fs::File::from(master);
    master.write_all(b"a\n").unwrap();
    let mut screen = Vec::new();
    let mut buf = [0u8; 4096];
    // Reading ends with EIO once the child has closed the terminal.
    while let Ok(n) = master.read(&mut buf) {
        if n == 0 {
            break;
        }
        screen.extend_from_slice(&buf[..n]);
    }
    assert!(
        child.wait().unwrap().success(),
        "{}",
        String::from_utf8_lossy(&screen)
    );
    let screen = String::from_utf8_lossy(&screen);
    assert!(
        screen.contains("[a]pprove [e]dit [m]erge [r]eject [s]kip [q]uit"),
        "{screen}"
    );
    assert!(b.join("projects/alpha/new.md").exists());
    assert!(!b.join(format!("inbox/{id}.md")).exists());
}

#[test]
fn control_characters_in_proposal_fields_are_shown_escaped() {
    assert_eq!(
        fbrain::review::escape("a\u{1b}[2Jb\r\nc\td"),
        "a\\x1b[2Jb\\x0d\\x0ac\td"
    );
}

// ---- fix round 1 ----

/// Writes a proposal file by hand, as a synced or edited inbox file would look.
fn hand(
    b: &Path,
    id: &str,
    op: &str,
    target: &str,
    extra: &str,
    stored_protected: bool,
    body: &str,
) {
    fs::create_dir_all(b.join("inbox")).unwrap();
    fs::write(
        b.join(format!("inbox/{id}.md")),
        format!("---\nop: {op}\ntarget: {target}\n{extra}proposed_by: x\nsource: s\nrationale: r\nconflicts: []\nconflicts_protected: {stored_protected}\n---\n{body}"),
    )
    .unwrap();
}

#[cfg(unix)]
fn failing_hook(b: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let hook = b.join(".git/hooks/pre-commit");
    fs::write(&hook, "#!/bin/sh\necho hook-says-no >&2\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_failing_commit_is_an_error_that_names_what_was_written() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "x\n",
    );
    failing_hook(&b);
    let out = review(&sb, &b, &id, &["--approve"])
        .code(1)
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("hook-says-no"), "{err}");
    assert!(
        err.contains("not committed") && err.contains("projects/alpha/new.md"),
        "{err}"
    );
    assert!(b.join("projects/alpha/new.md").exists());
    assert!(
        b.join(format!("inbox/{id}.md")).exists(),
        "the proposal stays until the commit worked"
    );
}

#[cfg(unix)]
#[test]
fn a_failing_commit_in_json_mode_carries_an_error_field() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "x\n",
    );
    failing_hook(&b);
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .args(["--json", "review", &id, "--approve", "--yes-i-am-faisal"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(v["error"].as_str().unwrap().contains("hook-says-no"));
    assert_eq!(v["changed"][0], "projects/alpha/new.md");
}

#[cfg(unix)]
#[test]
fn a_failing_commit_on_reject_keeps_the_proposal() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "x\n",
    );
    failing_hook(&b);
    review(&sb, &b, &id, &["--reject"]).code(1);
    assert!(b.join(format!("inbox/{id}.md")).exists());
    assert_eq!(git(&b, &["status", "--porcelain"]), "");
}

#[test]
fn a_git_error_while_checking_tracking_is_not_taken_for_untracked() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "x\n",
    );
    fs::write(b.join(".git/index"), "garbage").unwrap();
    review(&sb, &b, &id, &["--reject"]).code(1);
    assert!(b.join(format!("inbox/{id}.md")).exists());
}

#[cfg(unix)]
#[test]
fn a_failed_second_write_puts_the_first_back() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    let b = with_old(&sb, "");
    fs::create_dir(b.join("projects/beta")).unwrap();
    git(&b, &["mv", "projects/alpha/old.md", "projects/beta/old.md"]);
    git(&b, &["commit", "-q", "-m", "mv"]);
    let id = propose(
        &sb,
        &b,
        &[
            "--op",
            "supersede",
            "--target",
            "projects/alpha/new.md",
            "--supersedes",
            "projects/beta/old.md",
        ],
        "x\n",
    );
    fs::set_permissions(b.join("projects/beta"), fs::Permissions::from_mode(0o555)).unwrap();
    let err = review(&sb, &b, &id, &["--approve"])
        .code(1)
        .get_output()
        .stderr
        .clone();
    fs::set_permissions(b.join("projects/beta"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(String::from_utf8(err)
        .unwrap()
        .starts_with("could not write projects/beta/old.md: "));
    assert!(!b.join("projects/alpha/new.md").exists());
    assert!(b.join(format!("inbox/{id}.md")).exists());
}

#[cfg(unix)]
#[test]
fn a_proposal_that_cannot_be_removed_reports_that_it_was_applied() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose_untracked(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "x\n",
    );
    fs::set_permissions(b.join("inbox"), fs::Permissions::from_mode(0o555)).unwrap();
    let out = review(&sb, &b, &id, &["--approve"])
        .code(1)
        .get_output()
        .stderr
        .clone();
    fs::set_permissions(b.join("inbox"), fs::Permissions::from_mode(0o755)).unwrap();
    let err = String::from_utf8(out).unwrap();
    assert!(
        err.contains("applied") && err.contains("projects/alpha/new.md"),
        "{err}"
    );
    assert!(b.join("projects/alpha/new.md").exists());
}

#[test]
fn a_malformed_inbox_file_can_be_rejected_and_is_counted_in_the_inbox() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    fs::create_dir_all(b.join("inbox")).unwrap();
    fs::write(b.join("inbox/broken.md"), "no frontmatter at all\n").unwrap();
    sb.cmd()
        .arg("--brain")
        .arg(&b)
        .arg("inbox")
        .assert()
        .success()
        .stderr(predicates::str::contains("1 malformed inbox file"));
    review(&sb, &b, "broken", &["--reject"]).success();
    assert!(!b.join("inbox/broken.md").exists());
}

#[cfg(unix)]
#[test]
fn render_shows_a_failed_read_of_the_target() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    let b = repo(&sb);
    let id = propose(
        &sb,
        &b,
        &["--op", "update", "--target", "projects/alpha/status.md"],
        "new\n",
    );
    fs::set_permissions(
        b.join("projects/alpha/status.md"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let brain = fbrain::brain::Brain::open(&b).unwrap();
    let p = fbrain::proposals::get(&brain, &id).unwrap().unwrap();
    let shown = fbrain::review::render(&brain, &p);
    fs::set_permissions(
        b.join("projects/alpha/status.md"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(shown.contains("cannot read the current target"), "{shown}");
}

#[test]
fn a_supersedes_outside_the_notes_is_refused_at_apply_time() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    fs::create_dir(b.join(".hidden")).unwrap();
    fs::write(b.join(".hidden/secret.md"), "keep\n").unwrap();
    hand(
        &b,
        "h1",
        "supersede",
        "projects/alpha/new.md",
        "supersedes: .hidden/secret.md\n",
        false,
        "x\n",
    );
    review(&sb, &b, "h1", &["--approve"]).code(2);
    assert_eq!(read(&b, ".hidden/secret.md"), "keep\n");
    assert!(!b.join("projects/alpha/new.md").exists());
}

#[test]
fn a_supersedes_on_a_non_supersede_op_is_refused_at_apply_time() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    hand(
        &b,
        "h1",
        "add",
        "projects/alpha/new.md",
        "supersedes: projects/alpha/status.md\n",
        false,
        "x\n",
    );
    review(&sb, &b, "h1", &["--approve"]).code(2);
    assert!(!b.join("projects/alpha/new.md").exists());
}

#[test]
fn a_merge_target_outside_the_notes_is_refused() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    fs::create_dir(b.join(".hidden")).unwrap();
    fs::write(b.join(".hidden/secret.md"), "keep\n").unwrap();
    hand(&b, "h1", "add", "projects/alpha/new.md", "", false, "x\n");
    for bad in [".hidden/secret.md", "../outside.md", "inbox/h1.md"] {
        review(&sb, &b, "h1", &["--merge-into", bad]).code(2);
    }
    assert_eq!(read(&b, ".hidden/secret.md"), "keep\n");
    assert!(b.join("inbox/h1.md").exists());
}

#[test]
fn the_protected_flag_stored_in_the_inbox_file_is_not_trusted() {
    let sb = Sandbox::new();
    let b = with_old(&sb, "protected: true\n");
    hand(
        &b,
        "h1",
        "update",
        "projects/alpha/old.md",
        "",
        false,
        "changed\n",
    );
    review(&sb, &b, "h1", &["--approve"]).code(2);
    assert!(!read(&b, "projects/alpha/old.md").contains("changed"));
    hand(
        &b,
        "h2",
        "update",
        "projects/alpha/status.md",
        "",
        true,
        "changed\n",
    );
    review(&sb, &b, "h2", &["--approve"]).success();
}

#[test]
fn a_note_with_unparseable_frontmatter_counts_as_protected() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    fs::write(
        b.join("projects/alpha/broken.md"),
        "---\nstatus: [unclosed\n---\nold\n",
    )
    .unwrap();
    hand(
        &b,
        "h1",
        "update",
        "projects/alpha/broken.md",
        "",
        false,
        "new\n",
    );
    review(&sb, &b, "h1", &["--approve"]).code(2);
    review(&sb, &b, "h1", &["--approve", "--force"]).success();
}

#[cfg(unix)]
#[test]
fn a_note_that_is_a_symlink_is_never_replaced() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    std::os::unix::fs::symlink(
        b.join("projects/alpha/status.md"),
        b.join("projects/alpha/link.md"),
    )
    .unwrap();
    hand(
        &b,
        "h1",
        "update",
        "projects/alpha/link.md",
        "",
        false,
        "changed\n",
    );
    review(&sb, &b, "h1", &["--approve"]).code(2);
    assert!(fs::symlink_metadata(b.join("projects/alpha/link.md"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!read(&b, "projects/alpha/status.md").contains("changed"));
}

#[cfg(unix)]
#[test]
fn a_symlinked_folder_on_the_way_is_not_written_through() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    std::os::unix::fs::symlink(b.join("projects/alpha"), b.join("projects/linkdir")).unwrap();
    hand(&b, "h1", "add", "projects/linkdir/new.md", "", false, "x\n");
    review(&sb, &b, "h1", &["--approve"]).code(1);
    assert!(!b.join("projects/alpha/new.md").exists());
}

#[test]
fn proposal_fields_are_escaped_in_the_inbox_and_in_errors() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    hand(&b, "h1", "add", "\"projects/\\e[2Jx.md\"", "", false, "x\n");
    hand(
        &b,
        "h2",
        "update",
        "\"projects/\\e[2Jmissing.md\"",
        "",
        false,
        "x\n",
    );
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .arg("inbox")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(!out.contains(&0x1b), "{out:?}");
    assert!(String::from_utf8(out).unwrap().contains("\\x1b"));
    let err = review(&sb, &b, "h2", &["--approve"])
        .code(2)
        .get_output()
        .stderr
        .clone();
    assert!(!err.contains(&0x1b), "{err:?}");
    assert!(String::from_utf8(err).unwrap().contains("\\x1b"));
}

#[cfg(unix)]
#[test]
fn an_inbox_file_name_with_a_control_character_is_skipped_visibly() {
    let sb = Sandbox::new();
    let b = repo(&sb);
    fs::create_dir_all(b.join("inbox")).unwrap();
    fs::write(b.join("inbox/bad\u{1b}[2J.md"), "---\nop: add\n---\n").unwrap();
    let err = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .arg("inbox")
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    assert!(!err.contains(&0x1b), "{err:?}");
    assert!(String::from_utf8(err)
        .unwrap()
        .contains("malformed inbox file"));
}

#[cfg(unix)]
#[test]
fn retrying_a_merge_after_a_failed_commit_does_not_append_a_second_section() {
    let sb = Sandbox::new();
    let b = with_old(&sb, "");
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "Merged text here.\n",
    );
    failing_hook(&b);
    review(&sb, &b, &id, &["--merge-into", "projects/alpha/old.md"]).code(1);
    assert_eq!(
        read(&b, "projects/alpha/old.md")
            .matches("## Merged")
            .count(),
        1
    );
    fs::remove_file(b.join(".git/hooks/pre-commit")).unwrap();
    review(&sb, &b, &id, &["--merge-into", "projects/alpha/old.md"]).success();
    let old = read(&b, "projects/alpha/old.md");
    assert_eq!(old.matches("## Merged").count(), 1, "{old}");
    assert!(!b.join(format!("inbox/{id}.md")).exists());
}

#[cfg(unix)]
#[test]
fn a_merge_retried_on_another_day_still_finds_its_section() {
    let sb = Sandbox::new();
    let b = with_old(&sb, "");
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "Merged text here.\n",
    );
    failing_hook(&b);
    review(&sb, &b, &id, &["--merge-into", "projects/alpha/old.md"]).code(1);
    // Midnight passed between the attempts: the section on disk carries yesterday's date.
    let old = read(&b, "projects/alpha/old.md");
    fs::write(
        b.join("projects/alpha/old.md"),
        old.replace(&format!("## Merged {}", today()), "## Merged 2000-01-01"),
    )
    .unwrap();
    fs::remove_file(b.join(".git/hooks/pre-commit")).unwrap();
    review(&sb, &b, &id, &["--merge-into", "projects/alpha/old.md"]).success();
    let old = read(&b, "projects/alpha/old.md");
    assert_eq!(old.matches("Merged text here.").count(), 1, "{old}");
}

#[cfg(unix)]
#[test]
fn a_merge_whose_text_starts_an_earlier_merged_section_is_still_appended() {
    let sb = Sandbox::new();
    let b = with_old(&sb, "");
    let path = b.join("projects/alpha/old.md");
    let mut old = fs::read_to_string(&path).unwrap();
    old.push_str("\n## Merged 2000-01-01\n\nUses tabs.\nNever spaces.\n");
    fs::write(&path, old).unwrap();
    git(&b, &["commit", "-qam", "earlier merge"]);
    let id = propose(
        &sb,
        &b,
        &["--op", "add", "--target", "projects/alpha/new.md"],
        "Uses tabs.\n",
    );
    review(&sb, &b, &id, &["--merge-into", "projects/alpha/old.md"]).success();
    let old = read(&b, "projects/alpha/old.md");
    assert_eq!(old.matches("## Merged").count(), 2, "{old}");
    assert!(old.ends_with("\n\nUses tabs.\n"), "{old}");
}
