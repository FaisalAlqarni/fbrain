//! The resident process stays under its 800 MB gate while it embeds a brain of long notes.
//! Linux only: the peak is the server's `VmHWM`.
#![cfg(target_os = "linux")]
mod common;

use common::Sandbox;
use std::fs;

const GATE_MB: u64 = 800;

/// Notes long enough to fill the model's 512-token window, each with its own vocabulary.
fn long_brain(sb: &Sandbox) -> std::path::PathBuf {
    let b = sb.root.join("brain");
    fs::create_dir_all(b.join("me")).unwrap();
    fs::write(b.join(".fbrain.toml"), "name = \"mem\"\n").unwrap();
    for n in 0..60 {
        let mut body = format!("# Long note {n}\n\n");
        for p in 0..(3 + (n * 5) % 12) {
            body.push_str(&format!(
                "Paragraph {p} of note {n} discusses topic{n}x{p} with item{} and detail{} in the \
                 quarterly review, covering budget{} owner{} and the follow up for team{}. ",
                n * 7 + p,
                n * 13 + p,
                p * 3 + n,
                n + p * 5,
                (n * p) % 11
            ));
            for _ in 0..(n * p) % 9 {
                body.push_str("The text goes on about refunds, consent, onboarding and invoices. ");
            }
            body.push_str("\n\n");
        }
        fs::write(b.join(format!("me/long-{n}.md")), body).unwrap();
    }
    b
}

fn peak_mb(pid: u32) -> u64 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM");
    kb / 1024
}

#[test]
fn the_first_embed_of_long_notes_keeps_the_resident_process_under_the_gate() {
    let sb = Sandbox::new();
    let b = long_brain(&sb);
    sb.cmd()
        .env(
            "FBRAIN_TEST_MODEL_DIR",
            std::env::var("FBRAIN_TEST_MODEL_DIR").unwrap(),
        )
        .env_remove("FBRAIN_OFFLINE")
        .args(["doctor", "--fix"])
        .assert()
        .success();
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .args(["search", "refunds consent"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("semantic search off"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pid = sb.server_pid().expect("the search started the server");
    let peak = peak_mb(pid);
    assert!(
        peak < GATE_MB,
        "resident process peaked at {peak} MB (gate {GATE_MB} MB)"
    );
}
