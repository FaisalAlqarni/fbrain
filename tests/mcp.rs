//! `fbrain mcp` over stdio: a test client speaks JSON-RPC to the real binary.
mod common;

use common::Sandbox;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

const TEST_MODEL: &str = "google/embeddinggemma-300m";

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next: u64,
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Mcp {
    fn spawn(sb: &Sandbox, brain: &Path, env: &[(&str, &str)]) -> Mcp {
        let mut cmd = sb.std_cmd();
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .arg("--brain")
            .arg(brain)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(v);
                }
            }
        });
        Mcp {
            child,
            stdin,
            rx,
            next: 1,
        }
    }

    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// The response to a request; notifications from the server are skipped.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let v = self
                .rx
                .recv_timeout(Duration::from_secs(60))
                .expect("no reply from fbrain mcp");
            if v["id"] == id {
                return v;
            }
        }
    }

    fn initialize(&mut self, client: &str) -> Value {
        let r = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": client, "version": "1.0"},
            }),
        );
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        r
    }

    /// `result` of a tool call, whatever its `isError`.
    fn call(&mut self, tool: &str, args: Value) -> Value {
        let r = self.request("tools/call", json!({"name": tool, "arguments": args}));
        assert!(r["error"].is_null(), "{r}");
        r["result"].clone()
    }

    /// The structured payload of a successful call.
    fn ok(&mut self, tool: &str, args: Value) -> Value {
        let r = self.call(tool, args);
        assert!(r["isError"] != true, "{r}");
        match r.get("structuredContent") {
            Some(v) if !v.is_null() => v.clone(),
            _ => serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap(),
        }
    }
}

fn reason(r: &Value) -> String {
    r["semantic_reason"].as_str().unwrap_or("").to_string()
}

fn brain(sb: &Sandbox) -> PathBuf {
    let dir = sb.fixture_brain("search");
    fs::write(dir.join(".fbrain.toml"), "name = \"search\"\n").unwrap();
    dir
}

fn paths(hits: &Value) -> Vec<String> {
    hits.as_array()
        .unwrap()
        .iter()
        .map(|h| h["rel_path"].as_str().unwrap().to_string())
        .collect()
}

fn cli_json(sb: &Sandbox, brain: &Path, args: &[&str]) -> Value {
    // `--private` is for Faisal at a terminal.
    #[cfg(unix)]
    if args.contains(&"--private") {
        let mut c = sb.std_cmd();
        c.arg("--brain").arg(brain).arg("--json").args(args);
        let o = common::owner_output(c);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        return serde_json::from_slice(&o.stdout).unwrap();
    }
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(brain)
        .arg("--json")
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).unwrap()
}

const PRIVATE_NOTE: &str = "---\nprivate: true\nstatus: active\n---\n# Salary record\n\nUsers give consent before we store a session. The opt-out path is a single toggle in settings, and consent records are kept for audit. Zebrafish payroll.\n";

fn add_private(b: &Path) {
    fs::write(b.join("projects/alpha/salary.md"), PRIVATE_NOTE).unwrap();
}

fn install_model(sb: &Sandbox) {
    sb.cmd()
        .env("FBRAIN_MODEL", TEST_MODEL)
        .env(
            "FBRAIN_TEST_MODEL_DIR",
            std::env::var("FBRAIN_TEST_MODEL_DIR").expect("set FBRAIN_TEST_MODEL_DIR"),
        )
        .env_remove("FBRAIN_OFFLINE")
        .args(["doctor", "--fix"])
        .assert()
        .success();
}

/// Markdown notes of the fixture that the index covers.
fn note_count(b: &Path) -> usize {
    fn walk(d: &Path, n: &mut usize) {
        for e in fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if p.is_dir() {
                walk(&p, n);
            } else if name.ends_with(".md") && name != "INDEX.md" {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(b, &mut n);
    n
}

#[test]
fn tools_list_has_exactly_the_four_tools_with_their_hints() {
    let sb = Sandbox::new();
    let mut c = Mcp::spawn(&sb, &brain(&sb), &[]);
    let init = c.initialize("test-client");
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "{init}"
    );
    let list = c.request("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let mut names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["get", "health", "propose", "search"]);
    let hint = |name: &str, key: &str| {
        tools.iter().find(|t| t["name"] == name).unwrap()["annotations"][key].clone()
    };
    assert_eq!(hint("search", "readOnlyHint"), true);
    assert_eq!(hint("get", "readOnlyHint"), true);
    assert_eq!(hint("health", "readOnlyHint"), true);
    assert_eq!(hint("propose", "readOnlyHint"), false);
    assert_eq!(hint("propose", "destructiveHint"), false);
    let search = tools.iter().find(|t| t["name"] == "search").unwrap();
    assert_eq!(search["inputSchema"]["required"], json!(["query"]));
    assert!(search["inputSchema"]["properties"]["as_of"].is_object());
}

#[test]
fn search_returns_the_consent_note_first_in_the_order_of_the_cli() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let mut c = Mcp::spawn(&sb, &b, &[]);
    c.initialize("test-client");
    let r = c.ok("search", json!({"query": "consent opt-out"}));
    let got = paths(&r["hits"]);
    assert_eq!(got[0], "projects/alpha/consent.md");
    assert_eq!(r["semantic"], false);
    assert!(reason(&r).contains("model not downloaded"), "{r}");
    let cli = cli_json(&sb, &b, &["search", "consent opt-out"]);
    assert_eq!(got, paths(&cli["hits"]));
}

#[cfg(unix)]
#[test]
fn a_private_note_is_absent_from_mcp_search_and_get_but_not_from_the_private_cli() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    add_private(&b);
    let mut c = Mcp::spawn(&sb, &b, &[]);
    c.initialize("test-client");
    let r = c.ok("search", json!({"query": "zebrafish payroll consent"}));
    assert!(!paths(&r["hits"]).contains(&"projects/alpha/salary.md".to_string()));
    let cli = cli_json(
        &sb,
        &b,
        &["search", "--private", "zebrafish payroll consent"],
    );
    assert_eq!(paths(&cli["hits"])[0], "projects/alpha/salary.md");

    let g = c.call("get", json!({"note": "projects/alpha/salary.md"}));
    assert_eq!(g["isError"], true, "{g}");
    assert_eq!(
        g["content"][0]["text"],
        "note not found: projects/alpha/salary.md"
    );
    let cli_get = cli_json(&sb, &b, &["get", "--private", "projects/alpha/salary.md"]);
    assert_eq!(cli_get["title"], "Salary record");
}

#[test]
fn a_note_with_broken_frontmatter_is_treated_as_private() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    fs::write(
        b.join("projects/alpha/broken.md"),
        "---\nprivate: [oops\n---\n# Broken\n\nZebrafish payroll.\n",
    )
    .unwrap();
    let mut c = Mcp::spawn(&sb, &b, &[]);
    c.initialize("test-client");
    let r = c.ok("search", json!({"query": "zebrafish payroll"}));
    assert!(paths(&r["hits"]).is_empty(), "{r}");
    let g = c.call("get", json!({"note": "projects/alpha/broken.md"}));
    assert_eq!(g["isError"], true);
}

#[test]
fn get_returns_the_note_with_links_when_asked() {
    let sb = Sandbox::new();
    let b = sb.fixture_brain("links");
    fs::write(b.join(".fbrain.toml"), "name = \"links\"\n").unwrap();
    fs::write(
        b.join("topic/Secret.md"),
        "---\nprivate: true\n---\n# Secret\n\nSee [[A]].\n",
    )
    .unwrap();
    let a = fs::read_to_string(b.join("topic/A.md")).unwrap();
    fs::write(b.join("topic/A.md"), format!("{a}\nAlso [[Secret]].\n")).unwrap();
    let mut c = Mcp::spawn(&sb, &b, &[]);
    c.initialize("test-client");
    let r = c.ok("get", json!({"note": "topic/A.md", "links": true}));
    assert_eq!(r["rel_path"], "topic/A.md");
    assert_eq!(r["status"], "active");
    assert!(r["body"].as_str().unwrap().contains("[["));
    assert!(r["outgoing"].is_array());
    let links = format!("{} {}", r["outgoing"], r["backlinks"]);
    assert!(!links.contains("Secret"), "private note leaked: {links}");
    assert!(links.contains("topic/B.md"), "{links}");
    let plain = c.ok("get", json!({"note": "topic/A.md"}));
    assert!(plain.get("outgoing").is_none_or(Value::is_null));
}

#[cfg(unix)]
#[test]
fn search_expansion_never_adds_a_private_neighbour() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let consent = fs::read_to_string(b.join("projects/alpha/consent.md")).unwrap();
    fs::write(
        b.join("projects/alpha/consent.md"),
        format!("{consent}\nSee [[salary]].\n"),
    )
    .unwrap();
    // Shares no word with the query, so it can only arrive by expansion.
    fs::write(
        b.join("projects/alpha/salary.md"),
        "---\nprivate: true\n---\n# Payroll\n\nZebrafish figures.\n",
    )
    .unwrap();
    let mut c = Mcp::spawn(&sb, &b, &[]);
    c.initialize("test-client");
    let r = c.ok("search", json!({"query": "consent opt-out"}));
    assert!(
        !paths(&r["hits"]).contains(&"projects/alpha/salary.md".to_string()),
        "{r}"
    );
    let cli = cli_json(&sb, &b, &["search", "--private", "consent opt-out"]);
    assert!(
        paths(&cli["hits"]).contains(&"projects/alpha/salary.md".to_string()),
        "{cli}"
    );
}

#[test]
fn propose_records_the_client_name_as_proposed_by() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let mut c = Mcp::spawn(&sb, &b, &[]);
    c.initialize("claude-code");
    let args = json!({
        "op": "add", "target": "projects/alpha/new.md", "body": "# New\n\nA fresh note.\n",
        "rationale": "r", "source": "session 1",
    });
    let r = c.ok("propose", args.clone());
    let rel = r["rel_path"].as_str().unwrap();
    assert!(rel.starts_with("inbox/"), "{r}");
    assert!(r["conflicts"].is_array());
    assert_eq!(r["conflicts_protected"], false);
    assert_eq!(r["semantic"], false);
    assert!(reason(&r).contains("model not downloaded"), "{r}");
    let text = fs::read_to_string(b.join(rel)).unwrap();
    assert!(text.contains("proposed_by: claude-code\n"), "{text}");
    assert!(!b.join("projects/alpha/new.md").exists());
    // No client name given: `mcp`.
    let sb2 = Sandbox::new();
    let b2 = brain(&sb2);
    let mut c2 = Mcp::spawn(&sb2, &b2, &[]);
    c2.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "", "version": "1"}}),
    );
    c2.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let r2 = c2.ok("propose", args);
    let text2 = fs::read_to_string(b2.join(r2["rel_path"].as_str().unwrap())).unwrap();
    assert!(text2.contains("proposed_by: mcp\n"), "{text2}");
}

#[test]
fn propose_conflicts_leave_out_private_notes_on_both_surfaces() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    add_private(&b);
    let body = "Users give consent before we store a session. The opt-out path is a single toggle in settings, and consent records are kept for audit. Zebrafish payroll.\n";
    let mut c = Mcp::spawn(&sb, &b, &[]);
    c.initialize("test-client");
    let r = c.ok(
        "propose",
        json!({"op": "add", "target": "projects/alpha/dup.md", "body": body,
               "rationale": "r", "source": "s"}),
    );
    let listed: Vec<_> = r["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap().to_string())
        .collect();
    assert!(
        listed.contains(&"projects/alpha/consent.md".to_string()),
        "{r}"
    );
    assert!(!listed.iter().any(|p| p.contains("salary")), "{r}");

    let mut cli = sb.cmd();
    let out = cli
        .arg("--brain")
        .arg(&b)
        .args([
            "--json",
            "propose",
            "--op",
            "add",
            "--target",
            "projects/alpha/dup2.md",
        ])
        .args(["--rationale", "r", "--source", "s"])
        .write_stdin(body)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let cli_out: Value = serde_json::from_slice(&out).unwrap();
    assert!(!cli_out.to_string().contains("salary"), "{cli_out}");
}

#[test]
fn propose_cannot_target_a_private_note() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    add_private(&b);
    let mut c = Mcp::spawn(&sb, &b, &[]);
    c.initialize("test-client");
    let r = c.call(
        "propose",
        json!({"op": "update", "target": "projects/alpha/salary.md", "body": "x\n",
               "rationale": "r", "source": "s"}),
    );
    assert_eq!(r["isError"], true, "{r}");
    assert!(!r.to_string().contains("Salary"), "{r}");
    assert_eq!(fs::read_dir(b.join("inbox")).map_or(0, Iterator::count), 0);
}

#[test]
fn health_counts_the_notes_without_the_private_ones() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let mut c = Mcp::spawn(&sb, &b, &[]);
    c.initialize("test-client");
    let h = c.ok("health", json!({}));
    assert_eq!(h["notes"], note_count(&b), "{h}");
    assert!(h["search_ms"].is_number(), "{h}");
    assert_eq!(h["search_kind"], "keyword", "{h}");
    assert_eq!(h["semantic"], false, "{h}");
    assert!(reason(&h).contains("model not downloaded"), "{h}");
    assert_eq!(h["breaker"]["state"], "closed", "{h}");
    // No cache existed before this first call: unknown, not "fresh".
    assert!(h["index_age_s"].is_null(), "{h}");
    let again = c.ok("health", json!({}));
    assert!(again["index_age_s"].is_number(), "{again}");
    assert_eq!(h["model"], "missing");
    assert_eq!(h["server"], "unavailable");
    assert!(h["brain"].as_str().unwrap().contains("search"));

    add_private(&b);
    std::thread::sleep(Duration::from_millis(1100));
    let h2 = c.ok("health", json!({}));
    assert_eq!(h2["notes"], note_count(&b) - 1, "{h2}");
}

#[test]
fn a_note_added_on_disk_shows_in_the_next_search() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let mut c = Mcp::spawn(&sb, &b, &[]);
    c.initialize("test-client");
    let q = json!({"query": "axolotl"});
    assert!(paths(&c.ok("search", q.clone())["hits"]).is_empty());
    std::thread::sleep(Duration::from_millis(1100));
    fs::write(
        b.join("projects/alpha/pets.md"),
        "# Pets\n\nThe axolotl tank.\n",
    )
    .unwrap();
    let r = c.ok("search", q);
    assert_eq!(paths(&r["hits"]), ["projects/alpha/pets.md"]);
}

#[test]
fn initialize_answers_before_the_resident_process_is_ready_and_search_says_starting() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    install_model(&sb);
    let start = Instant::now();
    let mut c = Mcp::spawn(
        &sb,
        &b,
        &[
            ("FBRAIN_MODEL", TEST_MODEL),
            ("FBRAIN_TEST_SERVE_DELAY_MS", "2000"),
        ],
    );
    let init = c.initialize("test-client");
    assert!(init["result"].is_object());
    let took = start.elapsed();
    assert!(
        took < Duration::from_millis(500),
        "initialize took {took:?}"
    );
    let r = c.ok("search", json!({"query": "consent opt-out"}));
    assert!(
        start.elapsed() < Duration::from_millis(1900),
        "search waited for the server"
    );
    assert_eq!(r["semantic"], false, "{r}");
    assert!(reason(&r).contains("starting"), "{r}");
    assert_eq!(paths(&r["hits"])[0], "projects/alpha/consent.md");
    let h = c.ok("health", json!({}));
    assert_eq!(h["server"], "starting", "{h}");

    // Once the resident process is up, the same search is semantic.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let r = c.ok("search", json!({"query": "consent opt-out"}));
        if r["semantic"] == true {
            break;
        }
        assert!(Instant::now() < deadline, "never semantic: {r}");
        std::thread::sleep(Duration::from_millis(250));
    }
    let p = c.ok(
        "propose",
        json!({"op": "add", "target": "projects/alpha/fresh.md", "body": "# Fresh\n\nQuokka.\n",
               "rationale": "r", "source": "s"}),
    );
    assert_eq!(p["semantic"], true, "{p}");
}

#[cfg(target_os = "linux")]
/// Threads of `pid` blocked in a read on a Unix socket.
fn socket_readers(pid: u32) -> usize {
    fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .flatten()
        .filter(|t| {
            fs::read_to_string(t.path().join("wchan"))
                .is_ok_and(|w| w.starts_with("unix_stream_read"))
        })
        .count()
}

#[cfg(target_os = "linux")]
#[test]
fn a_hung_resident_process_opens_the_breaker_and_leaks_no_threads() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    install_model(&sb);
    let mut server = ready_server(&sb, &[("FBRAIN_TEST_SERVE_REQUEST", "stall:60000")]);
    let mut c = Mcp::spawn(
        &sb,
        &b,
        &[
            ("FBRAIN_MODEL", TEST_MODEL),
            ("FBRAIN_TEST_CALL_TIMEOUT_MS", "300"),
        ],
    );
    c.initialize("test-client");
    let q = json!({"query": "consent opt-out"});
    // Wait until the resident process is connected: its stalled answer then times out.
    let deadline = Instant::now() + Duration::from_secs(30);
    while reason(&c.ok("search", q.clone())).contains("starting") {
        assert!(
            Instant::now() < deadline,
            "the resident process never came up"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    for _ in 0..8 {
        let r = c.ok("search", q.clone());
        assert_eq!(r["semantic"], false, "{r}");
        assert_eq!(paths(&r["hits"])[0], "projects/alpha/consent.md");
    }
    // A leaked helper stays blocked reading the socket. The total thread count is no measure:
    // tokio's blocking pool grows and keeps idle threads for a while on a busy machine.
    let settle = Instant::now() + Duration::from_secs(2);
    let mut stuck = socket_readers(c.child.id());
    while stuck > 0 && Instant::now() < settle {
        std::thread::sleep(Duration::from_millis(50));
        stuck = socket_readers(c.child.id());
    }
    assert_eq!(
        stuck, 0,
        "threads still blocked reading the resident process socket"
    );
    // Breaker open: the next search does not wait for the timeout.
    let t = Instant::now();
    let r = c.ok("search", q);
    assert_eq!(r["semantic"], false);
    assert!(reason(&r).contains("retrying in"), "{r}");
    assert!(
        t.elapsed() < Duration::from_millis(250),
        "waited {:?}",
        t.elapsed()
    );
    let h = c.ok("health", json!({}));
    assert_eq!(h["breaker"]["state"], "open", "{h}");
    assert!(h["breaker"]["until_s"].is_number(), "{h}");
    let _ = server.kill();
    let _ = server.wait();
}

#[test]
fn the_session_brief_withholds_a_private_profile() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    fs::write(
        b.join("ME.md"),
        "---\nprivate: true\n---\n# Me\n\nMy salary is zebrafish.\n",
    )
    .unwrap();
    let out = sb
        .cmd()
        .arg("--brain")
        .arg(&b)
        .args(["brief", "--harness", "opencode-v1", "--cwd"])
        .arg(&b)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains("zebrafish"), "{text}");
    assert!(text.contains("(private note withheld)"), "{text}");
}

/// Errors a tool call came back with, as text.
fn error_text(r: &Value) -> String {
    assert_eq!(r["isError"], true, "{r}");
    r["content"][0]["text"].as_str().unwrap().to_string()
}

// Only Linux reports the peer pid that exposes the lie; macOS relies on the 0700 runtime dir.
#[cfg(target_os = "linux")]
#[test]
fn an_endpoint_that_is_not_the_resident_process_is_an_error_not_keyword_hits() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    install_model(&sb);
    // Start a real server, then make its pid file lie.
    let mut server = ready_server(&sb, &[]);
    fs::write(sb.runtime_dir().join("serve.pid"), "1").unwrap();
    let mut c = Mcp::spawn(&sb, &b, &[("FBRAIN_MODEL", TEST_MODEL)]);
    c.initialize("test-client");
    let r = c.call("search", json!({"query": "consent opt-out"}));
    assert!(error_text(&r).contains("not by the fbrain server"), "{r}");
    let _ = server.kill();
    let _ = server.wait();
}

/// A resident process that has loaded the model, so a test does not race the client's 3 s start wait
/// on a slow machine.
#[cfg(target_os = "linux")]
fn ready_server(sb: &Sandbox, env: &[(&str, &str)]) -> Child {
    let mut server = sb
        .std_cmd()
        .arg("serve")
        .env("FBRAIN_MODEL", TEST_MODEL)
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    assert!(first.starts_with("ready"), "{first}");
    server
}

#[cfg(unix)]
#[test]
fn a_server_newer_than_the_client_is_an_error_not_keyword_hits_and_keeps_running() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    install_model(&sb);
    let mut server = sb
        .std_cmd()
        .arg("serve")
        .env("FBRAIN_MODEL", TEST_MODEL)
        .env("FBRAIN_TEST_FAKE_VERSION", "99.0.0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    assert!(first.starts_with("ready"), "{first}");
    let mut c = Mcp::spawn(&sb, &b, &[("FBRAIN_MODEL", TEST_MODEL)]);
    c.initialize("test-client");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let r = c.call("search", json!({"query": "consent opt-out"}));
        if r["isError"] == true {
            assert!(
                error_text(&r).contains("older than the running server"),
                "{r}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "never refused: {r}");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        server.try_wait().unwrap().is_none(),
        "the newer server was stopped"
    );
    let _ = server.kill();
    let _ = server.wait();
}

#[test]
fn a_request_the_resident_process_rejects_is_an_error_not_keyword_hits() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    install_model(&sb);
    let mut c = Mcp::spawn(
        &sb,
        &b,
        &[
            ("FBRAIN_MODEL", TEST_MODEL),
            ("FBRAIN_TEST_SERVE_REQUEST", "fail"),
        ],
    );
    c.initialize("test-client");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let r = c.call("search", json!({"query": "consent opt-out"}));
        if r["isError"] == true {
            assert!(error_text(&r).contains("test failure requested"), "{r}");
            break;
        }
        assert!(Instant::now() < deadline, "never rejected: {r}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn oversized_arguments_are_refused_naming_the_field_and_the_limit() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let mut c = Mcp::spawn(&sb, &b, &[]);
    c.initialize("test-client");
    let t = error_text(&c.call("search", json!({"query": "a".repeat(2001)})));
    assert!(t.contains("query") && t.contains("2000"), "{t}");
    let t = error_text(&c.call("search", json!({"query": "ok", "as_of": "yesterday"})));
    assert!(t.contains("as_of") && t.contains("YYYY-MM-DD"), "{t}");
    let t = error_text(&c.call("get", json!({"note": "a".repeat(2001)})));
    assert!(t.contains("note") && t.contains("2000"), "{t}");
    let base = json!({"op": "add", "target": "projects/alpha/n.md", "body": "x",
                      "rationale": "r", "source": "s"});
    let with = |k: &str, v: String| {
        let mut a = base.clone();
        a[k] = json!(v);
        a
    };
    let t = error_text(&c.call("propose", with("body", "a".repeat(65537))));
    assert!(t.contains("body") && t.contains("65536"), "{t}");
    let t = error_text(&c.call("propose", with("rationale", "a".repeat(2001))));
    assert!(t.contains("rationale") && t.contains("2000"), "{t}");
    let t = error_text(&c.call("propose", with("source", "a".repeat(2001))));
    assert!(t.contains("source") && t.contains("2000"), "{t}");
    assert_eq!(fs::read_dir(b.join("inbox")).map_or(0, Iterator::count), 0);
}

#[test]
fn a_long_client_name_is_cut_before_it_reaches_the_proposal() {
    let sb = Sandbox::new();
    let b = brain(&sb);
    let mut c = Mcp::spawn(&sb, &b, &[]);
    c.initialize(&"n".repeat(500));
    let r = c.ok(
        "propose",
        json!({"op": "add", "target": "projects/alpha/n.md", "body": "x\n",
               "rationale": "r", "source": "s"}),
    );
    let text = fs::read_to_string(b.join(r["rel_path"].as_str().unwrap())).unwrap();
    let line = text
        .lines()
        .find(|l| l.starts_with("proposed_by:"))
        .unwrap();
    assert_eq!(line.len(), "proposed_by: ".len() + 64, "{line}");
}
