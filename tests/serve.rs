//! The resident process (`fbrain serve`) as the CLI sees it. Every sandbox has its own
//! `FBRAIN_RUNTIME_DIR`, and dropping the sandbox stops the server it started.
mod common;

use common::{alive, Sandbox};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MODEL: &str = "google/embeddinggemma-300m";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// At most this many tests run servers at once: under more parallel load a server can miss the
/// client's 3 s start wait, and the test would measure the machine, not the behaviour.
const PARALLEL: usize = 4;

struct Slot;

impl Drop for Slot {
    fn drop(&mut self) {
        let (m, cv) = &SLOTS;
        *m.lock().unwrap_or_else(|p| p.into_inner()) -= 1;
        cv.notify_one();
    }
}

static SLOTS: (std::sync::Mutex<usize>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());

/// Held for the test's whole run; declared before the sandbox so it is released after it.
fn slot() -> Slot {
    let (m, cv) = &SLOTS;
    let mut n = m.lock().unwrap_or_else(|p| p.into_inner());
    while *n >= PARALLEL {
        n = cv.wait(n).unwrap_or_else(|p| p.into_inner());
    }
    *n += 1;
    Slot
}

fn brain(sb: &Sandbox) -> PathBuf {
    let dir = sb.fixture_brain("semantic");
    fs::write(dir.join(".fbrain.toml"), "name = \"semantic\"\n").unwrap();
    dir
}

fn install_model(sb: &Sandbox) {
    sb.cmd()
        .env(
            "FBRAIN_TEST_MODEL_DIR",
            std::env::var("FBRAIN_TEST_MODEL_DIR").unwrap(),
        )
        .env_remove("FBRAIN_OFFLINE")
        .args(["doctor", "--fix"])
        .assert()
        .success();
}

/// A brain with the model installed and no server running yet.
fn ready(sb: &Sandbox) -> PathBuf {
    let b = brain(sb);
    install_model(sb);
    assert_eq!(sb.server_pid(), None, "doctor must not start a server");
    b
}

fn fb(sb: &Sandbox, brain: &Path) -> assert_cmd::Command {
    let mut c = sb.cmd();
    c.arg("--brain").arg(brain);
    c
}

fn search(sb: &Sandbox, brain: &Path, q: &str) -> (Vec<Value>, String) {
    let out = fb(sb, brain)
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

fn doctor_serve_line(sb: &Sandbox, brain: &Path) -> String {
    let out = fb(sb, brain).arg("doctor").output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .find(|l| l.contains(": serve: "))
        .unwrap_or_default()
        .to_string()
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A caller that reads a search's output must not wait for the server that search started: on
/// Windows a child inherits every inheritable handle, the search's stdout and stderr included.
#[test]
fn a_search_that_starts_the_server_returns_while_the_server_keeps_running() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let mut child = sb
        .std_cmd()
        .arg("--brain")
        .arg(&b)
        .args(["search", "refund policy"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Read to end of file on a thread: a pipe the server inherited never ends, and the test must
    // fail instead of hanging until that server exits.
    let mut out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut out, &mut s);
        let _ = tx.send(s);
    });
    let text = rx
        .recv_timeout(Duration::from_secs(90))
        .expect("the search's output stayed open after it exited: the server holds the pipe");
    assert!(child.wait().unwrap().success());
    assert!(text.contains("refund"), "{text}");
    assert!(
        sb.server_pid().is_some_and(alive),
        "the server it started should keep running"
    );
}
#[test]
fn the_first_search_starts_a_server_that_doctor_reports() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let (hits, stderr) = search(&sb, &b, "opt out of marketing texts");
    assert!(!stderr.contains("semantic search off"), "{stderr}");
    let hit = hits
        .iter()
        .find(|h| h["rel_path"] == "projects/shop/sms-optout.md")
        .expect("note missing from hits");
    assert!(hit["explain"]["embed_rank"].is_number(), "{hit}");
    let line = doctor_serve_line(&sb, &b);
    assert!(line.starts_with("ok: serve: running pid "), "{line}");
    assert!(line.contains(VERSION), "{line}");
}

#[test]
fn eight_concurrent_first_searches_start_exactly_one_server() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let children: Vec<_> = (0..8)
        .map(|_| {
            sb.std_cmd()
                .arg("--brain")
                .arg(&b)
                .args(["--json", "search", "refund policy"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for c in children {
        let o = c.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(o.status.success(), "{err}");
        assert!(!err.contains("semantic search off"), "{err}");
    }
    let pid = sb.server_pid().expect("no pid file");
    assert!(alive(pid));
    let entries: Vec<String> = fs::read_dir(sb.runtime_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.starts_with("serve.pid"))
            .count(),
        1,
        "{entries:?}"
    );
    #[cfg(unix)]
    assert_eq!(
        entries.iter().filter(|e| e.ends_with(".sock")).count(),
        1,
        "{entries:?}"
    );
    #[cfg(target_os = "linux")]
    assert_eq!(linux_servers_for(&sb.runtime_dir()), vec![pid]);
}

/// Live `fbrain serve` processes whose environment names this runtime dir.
#[cfg(target_os = "linux")]
fn linux_servers_for(runtime: &Path) -> Vec<u32> {
    let want = format!("FBRAIN_RUNTIME_DIR={}", runtime.display());
    let mut out = Vec::new();
    for e in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let cmd = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let env = fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        let is_serve = cmd.split(|b| *b == 0).any(|a| a == b"serve");
        let ours = env.split(|b| *b == 0).any(|kv| kv == want.as_bytes());
        if is_serve && ours && alive(pid) {
            out.push(pid);
        }
    }
    out
}

#[cfg(unix)]
#[test]
fn a_killed_server_leaves_a_stale_socket_that_the_next_search_replaces() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    search(&sb, &b, "refund policy");
    let old = sb.server_pid().unwrap();
    std::process::Command::new("kill")
        .args(["-9", &old.to_string()])
        .status()
        .unwrap();
    wait_until("the killed server to go", 5, || !alive(old));
    assert!(sb.runtime_dir().join("serve.sock").exists());
    let (hits, stderr) = search(&sb, &b, "opt out of marketing texts");
    assert!(!stderr.contains("semantic search off"), "{stderr}");
    assert!(!hits.is_empty());
    let line = doctor_serve_line(&sb, &b);
    assert!(line.starts_with("ok: serve: running pid "), "{line}");
    assert!(!line.contains(&format!("pid {old},")), "{line}");
}

#[test]
fn a_server_with_another_version_is_replaced_on_the_next_search() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let mut old = sb
        .std_cmd()
        .arg("serve")
        .env("FBRAIN_TEST_FAKE_VERSION", "0.0.0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(old.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    assert!(first.starts_with("ready"), "{first}");
    assert_eq!(sb.server_pid(), Some(old.id()));
    let (_, stderr) = search(&sb, &b, "refund policy");
    assert!(!stderr.contains("semantic search off"), "{stderr}");
    wait_until("the old server to exit", 5, || {
        old.try_wait().unwrap().is_some()
    });
    let line = doctor_serve_line(&sb, &b);
    assert!(line.starts_with("ok: serve: running pid "), "{line}");
    assert!(line.contains(&format!("version {VERSION}")), "{line}");
    assert_ne!(sb.server_pid(), Some(old.id()));
}

#[test]
fn an_idle_server_exits_and_removes_its_files() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let out = fb(&sb, &b)
        .env("FBRAIN_SERVE_IDLE_SECS", "2")
        .args(["search", "refund policy"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(!String::from_utf8_lossy(&out.stderr).contains("semantic search off"));
    let pid = sb.server_pid().unwrap();
    std::thread::sleep(Duration::from_secs(4));
    assert!(!alive(pid), "server {pid} still running");
    let left: Vec<String> = fs::read_dir(sb.runtime_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["serve.log"], "only the log stays");
    let line = doctor_serve_line(&sb, &b);
    assert!(line.contains("not running"), "{line}");
    assert!(line.contains("last exit: idle"), "{line}");
    assert!(line.contains("serve.log"), "{line}");
}

#[test]
fn a_server_that_cannot_start_leaves_keyword_search_with_a_notice() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let out = fb(&sb, &b)
        .env("FBRAIN_TEST_SERVE_FAIL", "1")
        .args(["--json", "search", "refund policy"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("semantic search off: resident process unavailable:"),
        "{stderr}"
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let hits = v["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    assert!(hits[0]["explain"]["bm25_rank"].is_number());
    assert!(hits.iter().all(|h| h["explain"]["embed_rank"].is_null()));
}

#[test]
fn a_damaged_model_falls_back_to_keywords_naming_the_cause() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let tok = sb
        .root
        .join("cache/models/google--embeddinggemma-300m/tokenizer.json");
    let mut bytes = fs::read(&tok).unwrap();
    bytes[10] ^= 1;
    fs::write(&tok, bytes).unwrap();
    let out = fb(&sb, &b)
        .args(["--json", "search", "refund policy"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("semantic search off: "), "{stderr}");
    assert!(
        stderr.contains("checksum mismatch: tokenizer.json"),
        "{stderr}"
    );
    assert!(stderr.contains("fbrain doctor --fix"), "{stderr}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(!v["hits"].as_array().unwrap().is_empty());
}

#[test]
fn a_model_changed_under_a_running_server_is_not_used() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    search(&sb, &b, "refund policy");
    let tok = sb
        .root
        .join("cache/models/google--embeddinggemma-300m/tokenizer.json");
    let mut bytes = fs::read(&tok).unwrap();
    bytes[10] ^= 1;
    fs::write(&tok, bytes).unwrap();
    let (hits, stderr) = search(&sb, &b, "refund policy");
    assert!(
        stderr.contains("checksum mismatch: tokenizer.json"),
        "{stderr}"
    );
    assert!(hits.iter().all(|h| h["explain"]["embed_rank"].is_null()));
}

/// The endpoint a raw client connects to: the socket file, or the named pipe for this runtime dir.
#[cfg(unix)]
fn raw_connect(sb: &Sandbox) -> (Box<dyn std::io::Read>, Box<dyn Write>) {
    let s = std::os::unix::net::UnixStream::connect(sb.runtime_dir().join("serve.sock")).unwrap();
    (Box::new(s.try_clone().unwrap()), Box::new(s))
}

#[cfg(windows)]
fn raw_connect(sb: &Sandbox) -> (Box<dyn std::io::Read>, Box<dyn Write>) {
    let name: String = sb
        .runtime_dir()
        .display()
        .to_string()
        .chars()
        .map(|c| if c == '\\' || c == ':' { '_' } else { c })
        .collect();
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!(r"\\.\pipe\fbrain-test-{name}"))
        .unwrap();
    (Box::new(f.try_clone().unwrap()), Box::new(f))
}

#[test]
fn a_hello_with_a_wrong_token_is_refused_and_closed() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    search(&sb, &b, "refund policy");
    let (r, mut w) = raw_connect(&sb);
    let hello = serde_json::json!({"Hello": {
        "version": VERSION, "model_id": MODEL, "cache_format": 3, "token": "wrong"}});
    writeln!(w, "{hello}").unwrap();
    let mut r = BufReader::new(r);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), r#"{"Error":"unauthorized"}"#);
    let mut rest = String::new();
    assert_eq!(r.read_line(&mut rest).unwrap_or(0), 0, "still open: {rest}");
}

#[cfg(unix)]
#[test]
fn the_runtime_dir_and_token_file_are_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    search(&sb, &b, "refund policy");
    let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(sb.runtime_dir()), 0o700);
    assert_eq!(mode(sb.runtime_dir().join("serve.token")), 0o600);
    assert_eq!(mode(sb.runtime_dir().join("serve.log")), 0o600);
}

#[test]
fn get_never_starts_a_server() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    fb(&sb, &b)
        .args(["get", "refund-policy"])
        .assert()
        .success();
    assert_eq!(sb.server_pid(), None);
    assert!(!sb.runtime_dir().join("serve.sock").exists());
}

#[test]
fn reindex_goes_through_the_server() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let stats = |sb: &Sandbox| {
        let o = fb(sb, &b).args(["reindex", "--stats"]).assert().success();
        String::from_utf8(o.get_output().stdout.clone()).unwrap()
    };
    let first = stats(&sb);
    assert!(first.contains("embedded 12"), "{first}");
    assert!(
        sb.server_pid().is_some_and(alive),
        "reindex did not start a server"
    );
    let second = stats(&sb);
    assert!(second.starts_with("changed 0,"), "{second}");
}

#[test]
fn search_chunks_through_the_server_lists_every_chunk_of_the_hits() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let out = fb(&sb, &b)
        .args(["--json", "search", "--chunks", "refund policy"])
        .assert()
        .success()
        .get_output()
        .clone();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let chunks = v["chunks"].as_array().unwrap();
    assert!(chunks
        .iter()
        .any(|c| c["rel_path"] == "projects/shop/refund-policy.md"));
}

/// Waits until the server in this sandbox is gone (it was started with a short idle timeout).
fn wait_server_gone(sb: &Sandbox) {
    let pid = sb.server_pid();
    wait_until("the server to exit", 10, || {
        pid.is_none_or(|p| !alive(p)) && sb.server_pid().is_none()
    });
}

#[test]
fn a_keyword_fallback_leaves_the_embedded_cache_alone() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let o = fb(&sb, &b)
        .env("FBRAIN_SERVE_IDLE_SECS", "1")
        .args(["reindex", "--stats"])
        .assert()
        .success();
    assert!(String::from_utf8_lossy(&o.get_output().stdout).contains("embedded 12"));
    wait_server_gone(&sb);
    let o = fb(&sb, &b)
        .env("FBRAIN_TEST_SERVE_FAIL", "1")
        .args(["search", "refund policy"])
        .assert()
        .success();
    assert!(String::from_utf8_lossy(&o.get_output().stderr).contains("semantic search off"));
    let o = fb(&sb, &b).args(["reindex", "--stats"]).assert().success();
    let out = String::from_utf8_lossy(&o.get_output().stdout).into_owned();
    assert!(out.starts_with("changed 0,"), "{out}");
    assert!(out.contains("embedded 0"), "{out}");
}

fn json_search(cmd: &mut assert_cmd::Command, q: &str) -> (Value, String) {
    let out = cmd
        .args(["--json", "search", q])
        .assert()
        .success()
        .get_output()
        .clone();
    (
        serde_json::from_slice(&out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn json_search_says_whether_semantic_search_was_on() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let (v, _) = json_search(&mut fb(&sb, &b), "refund policy");
    assert_eq!(v["semantic"], true, "{v}");
    assert!(v.get("semantic_reason").is_none(), "{v}");
    let sb2 = Sandbox::new();
    let b2 = ready(&sb2);
    let (v, _) = json_search(
        fb(&sb2, &b2).env("FBRAIN_TEST_SERVE_FAIL", "1"),
        "refund policy",
    );
    assert_eq!(v["semantic"], false, "{v}");
    assert!(
        v["semantic_reason"]
            .as_str()
            .unwrap()
            .contains("FBRAIN_TEST_SERVE_FAIL"),
        "{v}"
    );
}

#[test]
fn a_failed_call_on_a_connected_server_falls_back_to_keywords() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let (v, stderr) = json_search(
        fb(&sb, &b).env("FBRAIN_TEST_SERVE_REQUEST", "fail"),
        "refund policy",
    );
    assert!(stderr.contains("semantic search off: "), "{stderr}");
    assert!(stderr.contains("test failure requested"), "{stderr}");
    assert_eq!(v["semantic"], false, "{v}");
    assert!(!v["hits"].as_array().unwrap().is_empty());
}

#[test]
fn a_call_that_gets_no_reply_times_out_naming_the_server_pid() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let (v, stderr) = json_search(
        fb(&sb, &b)
            .env("FBRAIN_TEST_SERVE_REQUEST", "stall:5000")
            .env("FBRAIN_TEST_CALL_TIMEOUT_MS", "500"),
        "refund policy",
    );
    let pid = sb.server_pid().unwrap();
    assert!(stderr.contains(&format!("pid {pid}")), "{stderr}");
    assert!(stderr.contains("no reply"), "{stderr}");
    assert_eq!(v["semantic"], false, "{v}");
}

#[test]
fn a_panic_in_one_request_is_logged_and_the_server_keeps_serving() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    fb(&sb, &b)
        .env("FBRAIN_TEST_SERVE_REQUEST", "panic-once")
        .args(["search", "refund policy"])
        .assert()
        .success();
    let pid = sb.server_pid().unwrap();
    let (v, stderr) = json_search(&mut fb(&sb, &b), "opt out of marketing texts");
    assert_eq!(v["semantic"], true, "{v} {stderr}");
    assert_eq!(sb.server_pid(), Some(pid), "the server was replaced");
    let log = fs::read_to_string(sb.runtime_dir().join("serve.log")).unwrap();
    assert!(log.contains("panic"), "{log}");
}

#[test]
fn a_live_server_whose_token_file_was_removed_is_replaced() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    // A cold start, the same work as the replacement plus the first embed of the brain.
    let t = Instant::now();
    search(&sb, &b, "refund policy");
    let cold = t.elapsed();
    let old = sb.server_pid().unwrap();
    fs::remove_file(sb.runtime_dir().join("serve.token")).unwrap();
    let t = Instant::now();
    let (hits, stderr) = search(&sb, &b, "opt out of marketing texts");
    assert!(!stderr.contains("semantic search off"), "{stderr}");
    // Replacing must not wait out a timeout (the handshake's is 5 s) on top of a start.
    let took = t.elapsed();
    assert!(
        took < cold + Duration::from_secs(4),
        "{took:?} after a {cold:?} cold start"
    );
    assert!(hits.iter().any(|h| h["explain"]["embed_rank"].is_number()));
    assert_ne!(sb.server_pid(), Some(old));
    let log = fs::read_to_string(sb.runtime_dir().join("serve.log")).unwrap();
    assert!(log.contains("serve.token"), "{log}");
}

#[cfg(target_os = "linux")]
#[test]
fn an_endpoint_whose_pid_is_not_the_servers_fails_the_command() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    search(&sb, &b, "refund policy");
    let real = sb.server_pid().unwrap();
    fs::write(sb.runtime_dir().join("serve.pid"), "1").unwrap();
    fb(&sb, &b)
        .args(["search", "refund policy"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("not by the fbrain server"));
    assert!(alive(real), "the real server must keep running");
}

/// A brain big enough that a full reindex takes a few seconds.
fn big_brain(sb: &Sandbox) -> PathBuf {
    let b = brain(sb);
    for i in 0..150 {
        fs::write(
            b.join(format!("me/bulk-{i}.md")),
            format!(
                "# Bulk note {i}\n\nNote number {i} about topic {} and item {}.\n",
                i % 7,
                i * 3
            ),
        )
        .unwrap();
    }
    b
}

fn spawn_full_reindex(sb: &Sandbox, b: &Path) -> std::process::Child {
    sb.std_cmd()
        .arg("--brain")
        .arg(b)
        .args(["reindex", "--full", "--stats"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn a_version_mismatch_waits_for_a_running_reindex() {
    let _slot = slot();
    let sb = Sandbox::new();
    install_model(&sb);
    let b = big_brain(&sb);
    search(&sb, &b, "refund policy");
    let child = spawn_full_reindex(&sb, &b);
    std::thread::sleep(Duration::from_millis(1500));
    let token = fs::read_to_string(sb.runtime_dir().join("serve.token")).unwrap();
    let (r, mut w) = raw_connect(&sb);
    let hello = serde_json::json!({"Hello": {
        "version": "99.0.0", "model_id": MODEL, "cache_format": 3, "token": token.trim()}});
    writeln!(w, "{hello}").unwrap();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).unwrap();
    assert_eq!(line.trim(), r#""VersionMismatch""#);
    let o = child.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{err}");
    assert!(out.starts_with("changed 162,"), "{out} {err}");
    assert!(!err.contains("semantic search off"), "{err}");
    let log = fs::read_to_string(sb.runtime_dir().join("serve.log")).unwrap();
    assert!(log.contains("exit: version mismatch"), "{log}");
}

#[cfg(unix)]
#[test]
fn a_server_killed_during_a_reindex_is_restarted_and_the_reindex_retried() {
    let _slot = slot();
    let sb = Sandbox::new();
    install_model(&sb);
    let b = big_brain(&sb);
    search(&sb, &b, "refund policy");
    let old = sb.server_pid().unwrap();
    let child = spawn_full_reindex(&sb, &b);
    std::thread::sleep(Duration::from_millis(1500));
    std::process::Command::new("kill")
        .args(["-9", &old.to_string()])
        .status()
        .unwrap();
    let o = child.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{err}");
    assert!(out.starts_with("changed "), "{out} {err}");
    assert!(err.contains("restarted"), "{err}");
    assert!(sb.server_pid().is_some_and(|p| p != old));
}

#[cfg(unix)]
#[test]
fn doctor_never_starts_a_server_even_when_the_status_call_is_cut_off() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    // Every request, the status one included, waits 15 s before its answer.
    fb(&sb, &b)
        .env("FBRAIN_TEST_SERVE_REQUEST", "stall:15000")
        .env("FBRAIN_TEST_CALL_TIMEOUT_MS", "100")
        .args(["search", "refund policy"])
        .assert()
        .success();
    let old = sb.server_pid().unwrap();
    let doctor = sb
        .std_cmd()
        .arg("--brain")
        .arg(&b)
        .arg("doctor")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Killed while doctor's status call waits on it: on Linux once doctor holds a socket, which
    // a slow machine reaches late; elsewhere after a delay doctor's own checks fit in.
    #[cfg(target_os = "linux")]
    wait_until("doctor to connect to the server", 60, || {
        fs::read_dir(format!("/proc/{}/fd", doctor.id()))
            .into_iter()
            .flatten()
            .flatten()
            .any(|f| {
                fs::read_link(f.path()).is_ok_and(|l| l.to_string_lossy().starts_with("socket:"))
            })
    });
    #[cfg(not(target_os = "linux"))]
    std::thread::sleep(Duration::from_secs(6));
    // Past the handshake and into the 15 s status call, with room for a busy machine.
    std::thread::sleep(Duration::from_secs(3));
    std::process::Command::new("kill")
        .args(["-9", &old.to_string()])
        .status()
        .unwrap();
    let out = doctor.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("warn: serve: "), "{stdout}");
    wait_until("the killed server to go", 5, || !alive(old));
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(sb.server_pid(), Some(old), "doctor started a new server");
    #[cfg(target_os = "linux")]
    assert!(linux_servers_for(&sb.runtime_dir()).is_empty());
}

#[test]
fn a_server_that_dies_silently_has_its_stderr_in_the_notice() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let out = fb(&sb, &b)
        .env("FBRAIN_TEST_SERVE_FAIL", "die")
        .args(["--json", "search", "refund policy"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("test: serve is dying"), "{stderr}");
}

/// One Hello on a fresh connection; returns the server's first reply line.
fn hello_reply(sb: &Sandbox, version: &str, model: &str, format: u32) -> String {
    let token = fs::read_to_string(sb.runtime_dir().join("serve.token")).unwrap();
    let (r, mut w) = raw_connect(sb);
    let hello = serde_json::json!({"Hello": {
        "version": version, "model_id": model, "cache_format": format, "token": token.trim()}});
    writeln!(w, "{hello}").unwrap();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).unwrap();
    line.trim().to_string()
}

#[test]
fn a_client_with_another_model_is_refused_and_the_server_keeps_running() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    search(&sb, &b, "refund policy");
    let pid = sb.server_pid().unwrap();
    let line = hello_reply(&sb, VERSION, "other/model", fbrain::index::FORMAT_VERSION);
    assert_eq!(line, format!(r#"{{"ModelMismatch":"{MODEL}"}}"#));
    std::thread::sleep(Duration::from_millis(500));
    assert!(alive(pid), "a model mismatch must not stop the server");
    let (_, stderr) = search(&sb, &b, "refund policy");
    assert!(!stderr.contains("semantic search off"), "{stderr}");
    assert_eq!(sb.server_pid(), Some(pid));
}

#[test]
fn an_older_client_is_refused_and_the_server_keeps_running() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    search(&sb, &b, "refund policy");
    let pid = sb.server_pid().unwrap();
    let line = hello_reply(&sb, "0.0.0", MODEL, fbrain::index::FORMAT_VERSION);
    assert_eq!(line, format!(r#"{{"ClientOlder":"{VERSION}"}}"#));
    let line = hello_reply(&sb, VERSION, MODEL, fbrain::index::FORMAT_VERSION - 1);
    assert_eq!(
        line,
        format!(
            r#"{{"ClientOlder":"cache format {}"}}"#,
            fbrain::index::FORMAT_VERSION
        )
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(alive(pid), "an older client must not stop the server");
}

#[test]
fn a_newer_cache_format_stops_the_server_for_the_upgrade() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    search(&sb, &b, "refund policy");
    let pid = sb.server_pid().unwrap();
    let line = hello_reply(&sb, VERSION, MODEL, fbrain::index::FORMAT_VERSION + 1);
    assert_eq!(line, r#""VersionMismatch""#);
    wait_until("the server to exit", 5, || !alive(pid));
}

#[test]
fn the_cli_fails_visibly_when_the_running_server_is_newer_than_it() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    let mut newer = sb
        .std_cmd()
        .arg("serve")
        .env("FBRAIN_TEST_FAKE_VERSION", "99.0.0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(newer.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    assert!(first.starts_with("ready"), "{first}");
    let o = fb(&sb, &b)
        .args(["search", "refund policy"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        !o.status.success(),
        "silent fallback: {}",
        String::from_utf8_lossy(&o.stdout)
    );
    assert!(
        err.contains("older than the running server") && err.contains("restart"),
        "{err}"
    );
    assert!(err.contains("99.0.0") && err.contains(VERSION), "{err}");
    assert!(
        newer.try_wait().unwrap().is_none(),
        "the newer server was stopped"
    );
    let _ = newer.kill();
    let _ = newer.wait();
}

#[test]
fn client_supplied_strings_cannot_forge_log_lines() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    search(&sb, &b, "refund policy");
    let line = hello_reply(
        &sb,
        "0.0.0\n1 pid 5 exit: forged",
        "other/model\u{1b}[2J",
        fbrain::index::FORMAT_VERSION,
    );
    assert!(line.contains("ClientOlder"), "{line}");
    let log = fs::read_to_string(sb.runtime_dir().join("serve.log")).unwrap();
    let forged: Vec<_> = log.lines().filter(|l| l.contains("forged")).collect();
    assert_eq!(forged.len(), 1, "{log}");
    assert!(forged[0].contains("refused an older client"), "{log}");
    assert!(!log.contains('\u{1b}'), "{log}");
}

#[cfg(unix)]
#[test]
fn private_notes_stay_hidden_from_semantic_search_through_the_resident_process() {
    let _slot = slot();
    let sb = Sandbox::new();
    let b = ready(&sb);
    fs::write(
        b.join("me/payroll.md"),
        "---\nprivate: true\n---\n# Payroll\n\nAnnual plans can be refunded in full within 30 days of purchase.\n",
    )
    .unwrap();
    let (hits, stderr) = search(&sb, &b, "refund policy");
    assert!(!stderr.contains("semantic search off"), "{stderr}");
    assert!(hits.iter().any(|h| h["explain"]["embed_rank"].is_number()));
    assert!(
        hits.iter().all(|h| h["rel_path"] != "me/payroll.md"),
        "{hits:?}"
    );
    let mut c = sb.std_cmd();
    c.arg("--brain")
        .arg(&b)
        .args(["--json", "search", "--private", "refund policy"]);
    let o = common::owner_output(c);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["semantic"], true);
    assert!(v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .any(|h| h["rel_path"] == "me/payroll.md"));
}
