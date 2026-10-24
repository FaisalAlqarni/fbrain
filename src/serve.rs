//! The resident process (`fbrain serve`) and its client. One server per user holds the embedding
//! model and each brain's index. It talks newline-delimited JSON over a Unix socket in a 0700
//! directory, or a named pipe with a user-only security descriptor on Windows, and every
//! connection starts with a `Hello` carrying the per-start random token from a 0600 file.

use crate::brain::Brain;
use crate::config;
use crate::index::embed::{self, Embedder};
use crate::index::{Index, Stats, FORMAT_VERSION};
use crate::search::{self, Hit, Query};
use anyhow::{anyhow, bail, Context, Result};
use interprocess::local_socket::{prelude::*, ListenerOptions, Name, Stream};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering as Cmp;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// How long a client waits for a server it started to print its ready line.
const START_WAIT: Duration = Duration::from_secs(3);
/// Longest request or reply line in bytes.
const MAX_LINE: u64 = 8 << 20;
const MAX_QUERY_CHARS: usize = 10_000;
const MAX_K: usize = 1_000;
const MAX_BUDGET: usize = 1_000_000;
const MAX_EMBED_TEXTS: usize = 256;
/// A `Hello` and its answer must arrive within this time (the server bounds the read on Unix
/// only, where sockets have read timeouts; the client bounds it on every OS).
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest first line (the `Hello`) before the token is checked.
const MAX_HELLO: u64 = 4096;
/// Reply waits on Unix: a search, and a reindex or embed of a whole brain.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(120);
const REINDEX_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How long an exiting server lets requests in flight finish.
const DRAIN_MAX: Duration = Duration::from_secs(10 * 60);
const MAX_ACCEPT_ERRORS: u32 = 100;
const RETRY_PAUSE: Duration = Duration::from_millis(50);
const LOG_FILE: &str = "serve.log";
/// The log is started afresh past this size.
const MAX_LOG: u64 = 1 << 20;
/// A brain is re-read from disk before a search at most this often.
const REFRESH_EVERY: Duration = Duration::from_secs(1);

const SOCK: &str = "serve.sock";
const TOKEN: &str = "serve.token";
const PID: &str = "serve.pid";
/// Held by the running server for its whole life; a second server cannot take it.
const SERVER_LOCK: &str = "serve.lock";
/// Held by a client while it starts a server, so concurrent first calls start one.
const START_LOCK: &str = "start.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EmbedKind {
    Query,
    Document,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Request {
    Hello {
        version: String,
        model_id: String,
        cache_format: u32,
        token: String,
    },
    Search {
        brain: PathBuf,
        query: Query,
    },
    Reindex {
        brain: PathBuf,
        full: bool,
    },
    Embed {
        texts: Vec<String>,
        kind: EmbedKind,
    },
    Status,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ready,
    Hits(Vec<Hit>),
    Stats(ReindexStats),
    Vectors(Vec<Vec<f32>>),
    Status(ServerStatus),
    /// The client is newer than this server (an upgrade) or the model files changed; the server
    /// is exiting so the client can start a fresh one.
    VersionMismatch,
    /// The client is older than this server; the server keeps running. Carries its version.
    ClientOlder(String),
    /// The client wants another model; the server keeps running. Carries the model it serves.
    ModelMismatch(String),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReindexStats {
    #[serde(flatten)]
    pub stats: Stats,
    /// The model the index is embedded with; `None` for a keyword-only index.
    pub model: Option<String>,
    pub dim: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub pid: u32,
    pub version: String,
    pub model_id: String,
    pub rss_mb: u64,
    pub brains: Vec<PathBuf>,
    pub uptime_s: u64,
}

/// `FBRAIN_RUNTIME_DIR`, else `$XDG_RUNTIME_DIR/fbrain` on Linux, else `<cache_dir>/run`.
/// Created user-only (0700) on Unix; one owned by another user is an error.
pub fn runtime_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("FBRAIN_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => default_runtime_dir()?,
    };
    sys::make_private_dir(&dir)?;
    Ok(dir)
}

fn default_runtime_dir() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(x).join("fbrain"));
    }
    Ok(config::cache_dir()?.join("run"))
}

/// The version this server reports; tests replace it with `FBRAIN_TEST_FAKE_VERSION`.
fn server_version() -> String {
    std::env::var("FBRAIN_TEST_FAKE_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| VERSION.to_string())
}

/// Orders two dotted versions by their numeric parts (`0.1.10` is after `0.1.9`); a part that
/// is not a number counts as 0, and a pre-release suffix is ignored.
fn compare_versions(a: &str, b: &str) -> Cmp {
    let parts = |v: &str| -> Vec<u64> {
        v.split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parts(a).cmp(&parts(b))
}

fn same_token(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// One line of at most `MAX_LINE` bytes; `None` at end of stream.
fn read_line(r: &mut impl BufRead) -> Result<Option<String>> {
    read_line_max(r, MAX_LINE)
}

fn read_line_max(r: &mut impl BufRead, max: u64) -> Result<Option<String>> {
    let mut buf = Vec::new();
    let n = r.by_ref().take(max + 1).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() as u64 > max {
        bail!("message larger than {max} bytes");
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| anyhow!("message is not UTF-8"))
}

fn write_msg(mut w: impl Write, msg: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    w.write_all(line.as_bytes())?;
    w.flush()?;
    Ok(())
}

/// Writes `text` through a temp file and a rename; the file is 0600 on Unix.
fn write_private(dir: &Path, name: &str, text: &str) -> Result<()> {
    let tmp = dir.join(format!(".{name}.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts
        .open(&tmp)
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    f.write_all(text.as_bytes())?;
    drop(f);
    fs::rename(&tmp, dir.join(name)).with_context(|| format!("cannot write {name}"))
}

fn new_token() -> Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| anyhow!("no randomness for the token: {e}"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Files only the running server owns; it removes them on exit. `serve.log` stays.
fn remove_server_files(rt: &Path) {
    for f in [SOCK, TOKEN, PID, SERVER_LOCK] {
        let _ = fs::remove_file(rt.join(f));
    }
}

/// A connection the client must not use: the endpoint is not our server, the server refused
/// our token, the runtime dir belongs to someone else, or the server refused this client (it is
/// older, or wants another model). Commands fail on it instead of falling back, because a quiet
/// downgrade would hide an impersonated endpoint or a stale session.
#[derive(Debug)]
pub struct Untrusted(pub String);

impl std::fmt::Display for Untrusted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Untrusted {}

/// Whether `e` (or a cause of it) is an [`Untrusted`] error.
pub fn is_untrusted(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Untrusted>())
}

static LOG: OnceLock<Mutex<fs::File>> = OnceLock::new();

/// Appends one line to `serve.log`; a server without a log keeps running.
fn log(msg: &str) {
    if let Some(f) = LOG.get() {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut f = f.lock().unwrap_or_else(|p| p.into_inner());
        // Messages carry client-supplied strings: one line each, no terminal codes.
        let msg = crate::review::escape(msg);
        let _ = writeln!(f, "{secs} pid {} {msg}", std::process::id());
    }
}

/// Opens `serve.log` (0600, append, emptied past `MAX_LOG`) and logs panics into it.
fn open_log(rt: &Path) -> Result<()> {
    let path = rt.join(LOG_FILE);
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = fs::remove_file(&path);
    }
    let mut opts = fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let f = opts
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let _ = LOG.set(Mutex::new(f));
    std::panic::set_hook(Box::new(|info| log(&format!("panic: {info}"))));
    Ok(())
}

/// The last `exit:` reason `pid` logged, if any.
fn logged_exit(rt: &Path, pid: u32) -> Option<String> {
    let text = fs::read_to_string(rt.join(LOG_FILE)).ok()?;
    let tag = format!(" pid {pid} exit: ");
    text.lines()
        .rev()
        .find_map(|l| l.split_once(&tag).map(|(_, r)| r.to_string()))
}

/// The last exit reason in the log and the log's path, for `doctor` when no server runs.
pub fn last_exit() -> Result<Option<(String, PathBuf)>> {
    let path = runtime_dir()?.join(LOG_FILE);
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(None);
    };
    let Some(last) = text.lines().last() else {
        return Ok(None);
    };
    let reason = match last.split_once(" exit: ") {
        Some((_, r)) => r.to_string(),
        None => format!("none logged, the server was killed or crashed (last line: {last})"),
    };
    Ok(Some((reason, path)))
}

/// Test-only request behaviour from `FBRAIN_TEST_SERVE_REQUEST`.
#[derive(Clone, Copy)]
enum TestHook {
    None,
    Fail,
    Stall(u64),
    PanicOnce,
}

fn test_hook() -> TestHook {
    match std::env::var("FBRAIN_TEST_SERVE_REQUEST").as_deref() {
        Ok("fail") => TestHook::Fail,
        Ok("panic-once") => TestHook::PanicOnce,
        Ok(s) => s
            .strip_prefix("stall:")
            .and_then(|ms| ms.parse().ok())
            .map_or(TestHook::None, TestHook::Stall),
        Err(_) => TestHook::None,
    }
}

struct Shared {
    rt: PathBuf,
    token: String,
    version: String,
    model_id: String,
    cache: PathBuf,
    /// Size and mtime of the model files when they were loaded.
    stamp: Option<String>,
    embedder: Embedder,
    brains: Mutex<BTreeMap<PathBuf, (Index, Instant)>>,
    started: Instant,
    /// Milliseconds after `started` of the last request.
    last: AtomicU64,
    busy: AtomicUsize,
    stopping: AtomicBool,
    hook: TestHook,
    panicked: AtomicBool,
}

/// Counts a request in flight; dropped on return or on a panic, so the count never sticks.
struct Busy<'a>(&'a Shared);

impl<'a> Busy<'a> {
    fn new(sh: &'a Shared) -> Busy<'a> {
        sh.busy.fetch_add(1, Ordering::SeqCst);
        sh.touch();
        Busy(sh)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.touch();
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Shared {
    fn touch(&self) {
        self.last
            .store(self.started.elapsed().as_millis() as u64, Ordering::SeqCst);
    }

    fn idle_for(&self) -> Duration {
        let now = self.started.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last.load(Ordering::SeqCst)))
    }

    /// The model files on disk are no longer the ones this server loaded.
    fn model_changed(&self) -> bool {
        embed::model_stamp(&self.cache, &self.model_id) != self.stamp
    }

    /// A panic in another request poisons the lock; the map is still whole (entries are only
    /// inserted after a successful load), so keep using it.
    fn brains(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, (Index, Instant)>> {
        self.brains.lock().unwrap_or_else(|p| {
            log("recovered the index lock after a panic");
            self.brains.clear_poison();
            p.into_inner()
        })
    }

    fn answer(&self, req: Request) -> Result<Response> {
        match self.hook {
            TestHook::Fail if !matches!(req, Request::Status) => bail!("test failure requested"),
            TestHook::Stall(ms) => std::thread::sleep(Duration::from_millis(ms)),
            _ => {}
        }
        match req {
            Request::Hello { .. } => bail!("Hello was already sent"),
            Request::Search { brain, query } => {
                check_query(&query)?;
                let brain = open_brain(&brain)?;
                let mut brains = self.brains();
                if matches!(self.hook, TestHook::PanicOnce)
                    && !self.panicked.swap(true, Ordering::SeqCst)
                {
                    panic!("test panic requested");
                }
                let fresh = brains
                    .get(&brain.path)
                    .is_some_and(|(_, t)| t.elapsed() < REFRESH_EVERY);
                if !fresh {
                    let ix = self.load(&brain, false)?;
                    brains.insert(brain.path.clone(), (ix, Instant::now()));
                }
                let (ix, _) = &brains[&brain.path];
                Ok(Response::Hits(search::search(
                    ix,
                    &query,
                    Some(&self.embedder),
                )?))
            }
            Request::Reindex { brain, full } => {
                let brain = open_brain(&brain)?;
                let ix = self.load(&brain, full)?;
                let stats = ReindexStats {
                    stats: ix.stats,
                    model: Some(self.model_id.clone()),
                    dim: ix.dim(),
                };
                self.brains().insert(brain.path, (ix, Instant::now()));
                Ok(Response::Stats(stats))
            }
            Request::Embed { texts, kind } => {
                if texts.len() > MAX_EMBED_TEXTS {
                    bail!("at most {MAX_EMBED_TEXTS} texts per request");
                }
                Ok(Response::Vectors(match kind {
                    EmbedKind::Query => self.embedder.embed_queries(&texts)?,
                    EmbedKind::Document => self.embedder.embed_documents(&texts)?,
                }))
            }
            Request::Status => Ok(Response::Status(ServerStatus {
                pid: std::process::id(),
                version: self.version.clone(),
                model_id: self.model_id.clone(),
                rss_mb: sys::rss_mb(),
                brains: self.brains().keys().cloned().collect(),
                uptime_s: self.started.elapsed().as_secs(),
            })),
        }
    }

    fn load(&self, brain: &Brain, full: bool) -> Result<Index> {
        Index::load_or_build(
            brain,
            &self.cache,
            full,
            Some((self.model_id.as_str(), &self.embedder)),
        )
    }
}

fn check_query(q: &Query) -> Result<()> {
    if q.text.chars().count() > MAX_QUERY_CHARS {
        bail!("query longer than {MAX_QUERY_CHARS} characters");
    }
    if q.k > MAX_K || q.budget_tokens > MAX_BUDGET {
        bail!("k must be at most {MAX_K} and the budget at most {MAX_BUDGET}");
    }
    Ok(())
}

/// Brains are named by absolute path and keyed by their canonical path.
fn open_brain(path: &Path) -> Result<Brain> {
    if !path.is_absolute() {
        bail!("brain path must be absolute: {}", path.display());
    }
    let canon = path
        .canonicalize()
        .with_context(|| format!("cannot open brain {}", path.display()))?;
    Brain::open(&canon)
}

#[derive(Debug)]
struct AlreadyRunning;

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("another fbrain serve is running")
    }
}

impl std::error::Error for AlreadyRunning {}

/// Runs the server until it has been idle for `idle`, gets SIGTERM, or meets a client of another
/// version. Prints `ready <pid>` once it accepts connections, `busy` when another server holds
/// the lock, or `error: <reason>` when it cannot start.
pub fn run(idle: Duration) -> Result<()> {
    let mut out = std::io::stdout();
    let (listener, shared) = match start() {
        Ok(s) => s,
        Err(e) if e.is::<AlreadyRunning>() => {
            let _ = writeln!(out, "busy");
            return Ok(());
        }
        Err(e) => {
            log(&format!("exit: start failed: {e:#}"));
            let _ = writeln!(out, "error: {e:#}");
            return Err(e);
        }
    };
    log(&format!(
        "start: version {}, model {}",
        shared.version, shared.model_id
    ));
    // The client reads one line and closes the pipe, so later writes are ignored.
    let _ = writeln!(out, "ready {}", std::process::id());
    let _ = out.flush();
    let watch = shared.clone();
    let tick = (idle / 4).clamp(Duration::from_millis(10), Duration::from_millis(250));
    std::thread::spawn(move || loop {
        std::thread::sleep(tick);
        if watch.busy.load(Ordering::SeqCst) == 0 && watch.idle_for() >= idle {
            stop(&watch, "idle");
        } else if sys::stop_requested() {
            stop(&watch, "signal");
        } else if [TOKEN, PID].iter().any(|f| !watch.rt.join(f).exists()) {
            stop(&watch, "serve.token or serve.pid was removed");
        }
    });
    let mut errors = 0;
    for conn in listener.incoming() {
        match conn {
            Ok(s) => {
                errors = 0;
                let sh = shared.clone();
                std::thread::spawn(move || handle(s, &sh));
            }
            Err(e) => {
                errors += 1;
                log(&format!("accept failed: {e}"));
                if errors >= MAX_ACCEPT_ERRORS {
                    stop(
                        &shared,
                        &format!("accept failed {errors} times in a row: {e}"),
                    );
                }
                // Out of descriptors or similar: back off instead of spinning.
                std::thread::sleep(Duration::from_millis(10 * errors.min(100) as u64));
            }
        }
    }
    stop(&shared, "the listener closed");
    Ok(())
}

/// Logs why, stops new connections, lets requests in flight finish (up to `DRAIN_MAX`), removes
/// the server's files and exits. Later callers only wait for that.
fn stop(sh: &Shared, reason: &str) {
    if sh.stopping.swap(true, Ordering::SeqCst) {
        return;
    }
    log(&format!("exit: {reason}"));
    #[cfg(unix)]
    let _ = fs::remove_file(sh.rt.join(SOCK));
    let deadline = Instant::now() + DRAIN_MAX;
    while sh.busy.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    // A reply written just before is flushed; give the client a moment to read it.
    std::thread::sleep(Duration::from_millis(20));
    remove_server_files(&sh.rt);
    std::process::exit(0)
}

fn start() -> Result<(interprocess::local_socket::Listener, Arc<Shared>)> {
    if let Some(ms) = std::env::var("FBRAIN_TEST_SERVE_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
    let rt = runtime_dir()?;
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(rt.join(SERVER_LOCK))
        .context("cannot open the server lock")?;
    // Held until the process exits; exit does not unwind, so the lock is never released early.
    let lock: &'static mut fd_lock::RwLock<fs::File> =
        Box::leak(Box::new(fd_lock::RwLock::new(lock_file)));
    let Ok(guard) = lock.try_write() else {
        return Err(AlreadyRunning.into());
    };
    std::mem::forget(guard);
    let started = (|| {
        open_log(&rt)?;
        if std::env::var_os("FBRAIN_TEST_SERVE_FAIL").is_some_and(|v| v == "1") {
            bail!("FBRAIN_TEST_SERVE_FAIL is set");
        }
        if std::env::var_os("FBRAIN_TEST_SERVE_FAIL").is_some_and(|v| v == "die") {
            // Dies without a ready or error line, as a crash would.
            eprintln!("test: serve is dying");
            std::process::exit(3);
        }
        write_private(&rt, PID, &std::process::id().to_string())?;
        let cache = config::cache_dir()?;
        let model_id = config::model_id(None);
        let embedder = embed::load_if_present(&cache, &model_id)?
            .ok_or_else(|| anyhow!("model not downloaded: {model_id}, run fbrain doctor --fix"))?;
        let stamp = embed::model_stamp(&cache, &model_id);
        let token = new_token()?;
        write_private(&rt, TOKEN, &token)?;
        // We hold the server lock, so a socket file left here belongs to a dead server.
        #[cfg(unix)]
        let _ = fs::remove_file(rt.join(SOCK));
        let listener =
            sys::listener(ListenerOptions::new().name(endpoint(&rt)?)).context("cannot listen")?;
        sys::on_stop(&rt);
        let shared = Arc::new(Shared {
            rt: rt.clone(),
            token,
            version: server_version(),
            model_id,
            cache,
            stamp,
            embedder,
            brains: Mutex::new(BTreeMap::new()),
            started: Instant::now(),
            last: AtomicU64::new(0),
            busy: AtomicUsize::new(0),
            stopping: AtomicBool::new(false),
            hook: test_hook(),
            panicked: AtomicBool::new(false),
        });
        Ok((listener, shared))
    })();
    if started.is_err() {
        remove_server_files(&rt);
    }
    started
}

fn handle(stream: Stream, sh: &Shared) {
    if sh.stopping.load(Ordering::SeqCst) {
        return;
    }
    let _ = stream.set_recv_timeout(Some(HELLO_TIMEOUT));
    let mut r = BufReader::new(&stream);
    let w = &stream;
    // The Hello is small; a longer first line is refused before any token check.
    let hello = match read_line_max(&mut r, MAX_HELLO) {
        Ok(Some(l)) => serde_json::from_str::<Request>(&l).ok(),
        _ => None,
    };
    let Some(Request::Hello {
        version,
        model_id,
        cache_format,
        token,
    }) = hello
    else {
        let _ = write_msg(w, &Response::Error("unauthorized".into()));
        return;
    };
    if !same_token(&token, &sh.token) {
        log("refused a connection with a wrong token");
        let _ = write_msg(w, &Response::Error("unauthorized".into()));
        return;
    }
    let changed = sh.model_changed();
    let order = compare_versions(&version, &sh.version).then(cache_format.cmp(&FORMAT_VERSION));
    if changed || order == Cmp::Greater {
        // A newer client means an upgrade: this server is stale and gives way.
        let _ = write_msg(w, &Response::VersionMismatch);
        stop(
            sh,
            &format!(
                "version mismatch: client {version}, cache format {cache_format}{}",
                if changed {
                    ", model files changed on disk"
                } else {
                    ""
                }
            ),
        );
        return;
    }
    // The server stays up for these: the other session, not this server, has to change.
    if order == Cmp::Less {
        log(&format!(
            "refused an older client: {version}, cache format {cache_format}"
        ));
        // Name what is older: the version, or only the cache format.
        let what = if compare_versions(&version, &sh.version) == Cmp::Less {
            sh.version.clone()
        } else {
            format!("cache format {FORMAT_VERSION}")
        };
        let _ = write_msg(w, &Response::ClientOlder(what));
        return;
    }
    if model_id != sh.model_id {
        log(&format!("refused a client that wants model {model_id}"));
        let _ = write_msg(w, &Response::ModelMismatch(sh.model_id.clone()));
        return;
    }
    // Counts as activity, so the idle watcher does not exit between this and the first request.
    sh.touch();
    if write_msg(w, &Response::Ready).is_err() {
        return;
    }
    let _ = stream.set_recv_timeout(None);
    loop {
        let req = match read_line(&mut r) {
            Ok(Some(l)) => {
                serde_json::from_str::<Request>(&l).map_err(|e| anyhow!("invalid request: {e}"))
            }
            Ok(None) => return,
            Err(e) => Err(e),
        };
        let req = match req {
            Ok(q) => q,
            Err(e) => {
                let _ = write_msg(w, &Response::Error(format!("{e:#}")));
                return;
            }
        };
        // An exiting server takes no new work; the client reconnects to the next server.
        if sh.stopping.load(Ordering::SeqCst) {
            return;
        }
        let resp = {
            let _busy = Busy::new(sh);
            sh.answer(req)
                .unwrap_or_else(|e| Response::Error(format!("{e:#}")))
        };
        if write_msg(w, &resp).is_err() {
            return;
        }
    }
}

#[cfg(unix)]
fn endpoint(rt: &Path) -> Result<Name<'static>> {
    use interprocess::local_socket::GenericFilePath;
    Ok(rt.join(SOCK).to_fs_name::<GenericFilePath>()?)
}

#[cfg(windows)]
fn endpoint(rt: &Path) -> Result<Name<'static>> {
    use interprocess::local_socket::GenericNamespaced;
    Ok(sys::pipe_name(rt).to_ns_name::<GenericNamespaced>()?)
}

enum Attempt {
    Connected(Client),
    /// A server of another version or model, or one whose model files changed; it is exiting.
    Mismatch,
    /// No usable server, and why.
    Absent(String),
}

/// A call got no reply within its time limit. A long-lived client counts these in a row.
#[derive(Debug)]
pub struct CallTimedOut(pub String);

impl std::fmt::Display for CallTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CallTimedOut {}

pub fn is_timeout(e: &anyhow::Error) -> bool {
    e.downcast_ref::<CallTimedOut>().is_some()
}

/// The server understood the request and refused it (a rejected query, a corrupt index). The
/// same request fails the same way again, so a long-lived client reports it instead of retrying.
#[derive(Debug)]
pub struct ServerRejected(pub String);

impl std::fmt::Display for ServerRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ServerRejected {}

pub fn is_rejected(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ServerRejected>().is_some()
}

/// What a bounded read of one reply line gave.
enum Reply {
    Line(String),
    /// The server closed the connection (or reset it) before a whole line.
    Closed,
    TimedOut,
    Failed(anyhow::Error),
}

/// Reads one reply line on a helper thread and waits at most `timeout`, on every OS (named pipes
/// have no read timeout). The connection comes back with a line; after a timeout it stays with
/// the helper thread, which drops it once the server answers or goes away.
fn read_reply(
    mut conn: BufReader<Stream>,
    timeout: Duration,
) -> (Option<BufReader<Stream>>, Reply) {
    let closer = sys::closer(conn.get_ref());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = read_line(&mut conn);
        let _ = tx.send((conn, r));
    });
    match rx.recv_timeout(timeout) {
        Ok((conn, Ok(Some(line)))) => (Some(conn), Reply::Line(line)),
        Ok((_, Ok(None))) => (None, Reply::Closed),
        Ok((_, Err(e))) => match e.downcast_ref::<std::io::Error>().map(|io| io.kind()) {
            Some(
                std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe,
            ) => (None, Reply::Closed),
            _ => (None, Reply::Failed(e)),
        },
        Err(_) => {
            // Wakes the helper thread's blocked read, so it ends instead of waiting for a
            // server that may never answer.
            closer();
            (None, Reply::TimedOut)
        }
    }
}

pub struct Client {
    /// `None` after a call timed out: the helper thread still holds the connection.
    conn: Option<BufReader<Stream>>,
    rt: PathBuf,
    model_id: String,
    pid: u32,
    /// The `fbrain` binary that starts a server when none answers.
    exe: PathBuf,
}

/// How long a call may wait for its reply: a search refreshes changed notes first, a reindex
/// or a batch embed may re-embed a whole brain.
fn call_timeout(req: &Request) -> Duration {
    if let Some(ms) = std::env::var("FBRAIN_TEST_CALL_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    match req {
        Request::Search { .. } | Request::Status | Request::Embed { .. } => SEARCH_TIMEOUT,
        _ => REINDEX_TIMEOUT,
    }
}

impl Client {
    /// Connects to the user's server, starting one when none answers. A server of another
    /// version or model exits on the handshake and a matching one is started.
    // The model id is a parameter: it comes from the brain's config, which only the caller knows.
    pub fn connect_or_start(model_id: &str) -> Result<Client> {
        let exe = std::env::current_exe().context("cannot find the fbrain binary")?;
        Client::connect_or_start_with(&exe, model_id)
    }

    /// The same, starting the server with `exe`: a caller that is not itself `fbrain` (the
    /// bench) names the binary that has the `serve` command.
    pub fn connect_or_start_with(exe: &Path, model_id: &str) -> Result<Client> {
        let rt = runtime_dir()?;
        let deadline = Instant::now() + START_WAIT * 2;
        loop {
            let why = match try_connect(&rt, model_id)? {
                Attempt::Connected(c) => {
                    return Ok(Client {
                        exe: exe.to_path_buf(),
                        ..c
                    })
                }
                Attempt::Mismatch => {
                    wait_gone(&rt, deadline);
                    "the running server was of another version or model".to_string()
                }
                Attempt::Absent(reason) => {
                    start_server(&rt, exe, model_id)?;
                    reason
                }
            };
            if Instant::now() > deadline {
                bail!(
                    "no server answered within {} s; last: {why}",
                    (START_WAIT * 2).as_secs()
                );
            }
            std::thread::sleep(RETRY_PAUSE);
        }
    }

    /// One request and its reply. A connection the server closed (it restarted, crashed or
    /// was stopped) is opened again once and the request sent again.
    fn call(&mut self, req: &Request) -> Result<Response> {
        match self.call_once(req)? {
            Some(r) => Ok(r),
            None => {
                let reason = logged_exit(&self.rt, self.pid)
                    .unwrap_or_else(|| "it stopped without logging why (killed or crashed)".into());
                eprintln!(
                    "note: the resident process (pid {}) restarted: {reason}; sending the request again",
                    self.pid
                );
                *self = Client::connect_or_start_with(&self.exe, &self.model_id)?;
                self.call_once(req)?.with_context(|| {
                    format!(
                        "the resident process (pid {}) closed the connection",
                        self.pid
                    )
                })
            }
        }
    }

    /// `None` when the connection was closed before a reply.
    fn call_once(&mut self, req: &Request) -> Result<Option<Response>> {
        let timeout = call_timeout(req);
        let Some(conn) = self.conn.take() else {
            bail!(
                "the connection to the resident process (pid {}) was given up after a timeout",
                self.pid
            );
        };
        if write_msg(conn.get_ref(), req).is_err() {
            return Ok(None);
        }
        let (conn, reply) = read_reply(conn, timeout);
        self.conn = conn;
        let line = match reply {
            Reply::Line(l) => l,
            Reply::Closed => return Ok(None),
            Reply::TimedOut => {
                return Err(CallTimedOut(format!(
                    "no reply from the resident process (pid {}) within {} ms",
                    self.pid,
                    timeout.as_millis()
                ))
                .into())
            }
            Reply::Failed(e) => return Err(e),
        };
        match serde_json::from_str(&line).context("invalid reply from the resident process")? {
            Response::Error(e) => Err(ServerRejected(e).into()),
            r => Ok(Some(r)),
        }
    }

    pub fn search(&mut self, brain: &Path, q: &Query) -> Result<Vec<Hit>> {
        let req = Request::Search {
            brain: brain.to_path_buf(),
            query: q.clone(),
        };
        match self.call(&req)? {
            Response::Hits(h) => Ok(h),
            r => bail!("unexpected reply to search: {r:?}"),
        }
    }

    pub fn reindex(&mut self, brain: &Path, full: bool) -> Result<ReindexStats> {
        let req = Request::Reindex {
            brain: brain.to_path_buf(),
            full,
        };
        match self.call(&req)? {
            Response::Stats(s) => Ok(s),
            r => bail!("unexpected reply to reindex: {r:?}"),
        }
    }

    pub fn embed(&mut self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        let req = Request::Embed {
            texts: texts.to_vec(),
            kind,
        };
        match self.call(&req)? {
            Response::Vectors(v) => Ok(v),
            r => bail!("unexpected reply to embed: {r:?}"),
        }
    }

    /// No retry: a status call must never start a server (doctor only asks).
    pub fn status(&mut self) -> Result<ServerStatus> {
        let reply = self.call_once(&Request::Status)?.with_context(|| {
            format!(
                "the resident process (pid {}) closed the connection",
                self.pid
            )
        })?;
        match reply {
            Response::Status(s) => Ok(s),
            r => bail!("unexpected reply to status: {r:?}"),
        }
    }
}

/// The running server's status, or `None` when none answers. Never starts one.
// A server of another version or model exits on this handshake once its requests finish.
pub fn status_if_running(model_id: &str) -> Result<Option<ServerStatus>> {
    match try_connect(&runtime_dir()?, model_id)? {
        Attempt::Connected(mut c) => c.status().map(Some),
        Attempt::Absent(_) => Ok(None),
        Attempt::Mismatch => bail!(
            "a server of another version or model was running; it exits once its requests finish and the next search starts a matching one"
        ),
    }
}

/// A client for the running server, or `None` when none answers. Never starts one.
pub fn connect_if_running(model_id: &str) -> Result<Option<Client>> {
    match try_connect(&runtime_dir()?, model_id)? {
        Attempt::Connected(c) => Ok(Some(c)),
        Attempt::Absent(_) => Ok(None),
        Attempt::Mismatch => bail!(
            "a server of another version or model was running; it exits once its requests finish and the next search starts a matching one"
        ),
    }
}

fn try_connect(rt: &Path, model_id: &str) -> Result<Attempt> {
    let stream = match sys::connect(rt) {
        Ok(s) => s,
        Err(e) => return Ok(Attempt::Absent(format!("cannot connect: {e}"))),
    };
    let read = |f: &str| fs::read_to_string(rt.join(f));
    let (token, pid) = match (read(TOKEN), read(PID)) {
        (Ok(t), Ok(p)) => (t, p),
        // The server checks for these files and exits when one is gone; wait for that.
        (t, _) => {
            let gone = if t.is_err() { TOKEN } else { PID };
            return Ok(Attempt::Absent(format!(
                "the endpoint answers but {gone} is missing; that server is exiting"
            )));
        }
    };
    let pid = pid.trim();
    sys::check_peer(&stream, pid).map_err(|e| anyhow::Error::new(Untrusted(format!("{e:#}"))))?;
    let conn = BufReader::new(stream);
    let hello = Request::Hello {
        version: VERSION.into(),
        model_id: model_id.into(),
        cache_format: FORMAT_VERSION,
        token: token.trim().into(),
    };
    let closed = || Attempt::Absent("the server closed the connection during the handshake".into());
    if write_msg(conn.get_ref(), &hello).is_err() {
        return Ok(closed());
    }
    let (conn, reply) = read_reply(conn, HELLO_TIMEOUT);
    let line = match reply {
        Reply::Line(l) => l,
        Reply::Closed => return Ok(closed()),
        Reply::TimedOut => {
            return Ok(Attempt::Absent(format!(
                "no answer to the handshake from pid {pid} within {} s",
                HELLO_TIMEOUT.as_secs()
            )))
        }
        Reply::Failed(e) => {
            return Ok(Attempt::Absent(format!(
                "no answer to the handshake: {e:#}"
            )))
        }
    };
    match serde_json::from_str(&line).context("invalid reply from the resident process")? {
        Response::Ready => Ok(Attempt::Connected(Client {
            conn: Some(conn.context("the handshake reply came without its connection")?),
            rt: rt.to_path_buf(),
            model_id: model_id.into(),
            pid: pid.parse().unwrap_or(0),
            exe: PathBuf::new(),
        })),
        Response::VersionMismatch => Ok(Attempt::Mismatch),
        Response::ClientOlder(server) => Err(Untrusted(format!(
            "this fbrain ({VERSION}) is older than the running server ({server}); restart this session so it runs the newer binary"
        ))
        .into()),
        Response::ModelMismatch(server) => Err(Untrusted(format!(
            "the running server serves model {server} but this brain uses {model_id}; one server per user serves one model, so use the same model in every brain"
        ))
        .into()),
        Response::Error(e) => Err(Untrusted(format!(
            "the resident process (pid {pid}) refused the connection: {e}"
        ))
        .into()),
        r => bail!("unexpected reply to hello: {r:?}"),
    }
}

/// Waits until the exiting server's endpoint stops answering.
fn wait_gone(rt: &Path, deadline: Instant) {
    while Instant::now() < deadline {
        match sys::connect(rt) {
            Ok(_) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return,
        }
    }
}

/// Takes the start lock and, unless a server answers by then, starts one and waits for it.
fn start_server(rt: &Path, exe: &Path, model_id: &str) -> Result<()> {
    let path = rt.join(START_LOCK);
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .context("cannot open the start lock")?;
    let mut lock = fd_lock::RwLock::new(f);
    let _guard = lock.write().context("cannot take the start lock")?;
    if sys::connect(rt).is_ok() {
        return Ok(());
    }
    let result = spawn_server(rt, exe, model_id);
    // Removed while held: a client that opened it meanwhile re-checks the endpoint, and the
    // server lock stops a second server even if two clients spawn at once.
    let _ = fs::remove_file(&path);
    result
}

fn spawn_server(rt: &Path, exe: &Path, model_id: &str) -> Result<()> {
    let (stderr, log_from) = start_stderr(rt);
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve")
        .env("FBRAIN_MODEL", model_id)
        .current_dir(rt)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(stderr);
    sys::detach(&mut cmd);
    let mut child = cmd.spawn().context("cannot start fbrain serve")?;
    let stdout = child.stdout.take().context("no stdout from fbrain serve")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let r = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = tx.send(r);
    });
    let line = match rx.recv_timeout(START_WAIT) {
        Ok(r) => r.context("cannot read the ready line of fbrain serve")?,
        Err(_) => {
            // Not killed: a slow load (a busy machine) finishes and the retry connects to it.
            // Something must reap it if it dies later, so a long-lived client leaves no zombie.
            let pid = child.id();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            bail!(
                "fbrain serve was not ready within {} s{}",
                START_WAIT.as_secs(),
                exit_detail(rt, pid, log_from)
            )
        }
    };
    let line = line.trim();
    if line.starts_with("ready") {
        return Ok(());
    }
    if line == "busy" {
        // Another server holds the lock and is still loading; the caller connects again.
        std::thread::sleep(Duration::from_millis(50));
        return Ok(());
    }
    let _ = child.wait();
    match line.strip_prefix("error: ") {
        Some(e) => bail!("{e}"),
        None => bail!(
            "fbrain serve exited before it was ready{}",
            exit_detail(rt, child.id(), log_from)
        ),
    }
}

/// How much of the start-up stderr and log an error quotes.
const DETAIL_BYTES: usize = 2048;

/// A starting server's stderr is appended to `serve.log`, so a crash before the log opens still
/// leaves its message in the one file. Returns the log's size before this start, or nothing.
fn start_stderr(rt: &Path) -> (std::process::Stdio, u64) {
    let path = rt.join(LOG_FILE);
    let before = fs::metadata(&path).map_or(0, |m| m.len());
    let mut o = fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    match o.open(&path) {
        Ok(f) => (f.into(), before),
        Err(_) => (std::process::Stdio::null(), before),
    }
}

fn tail(text: &str) -> &str {
    let mut from = text.len().saturating_sub(DETAIL_BYTES);
    while !text.is_char_boundary(from) {
        from += 1;
    }
    text[from..].trim()
}

/// `: <logged exit reason>; log: <what this start added to serve.log>` for a server that died
/// at start, or nothing.
fn exit_detail(rt: &Path, pid: u32, log_from: u64) -> String {
    let mut out = String::new();
    if let Some(r) = logged_exit(rt, pid) {
        out.push_str(&format!(": {r}"));
    }
    if let Ok(all) = fs::read(rt.join(LOG_FILE)) {
        let from = if (log_from as usize) <= all.len() {
            log_from as usize
        } else {
            0
        };
        let added = String::from_utf8_lossy(&all[from..]);
        let e = tail(&added);
        if !e.is_empty() {
            out.push_str(&format!("; log: {e}"));
        }
    }
    out
}

#[cfg(unix)]
mod sys {
    use anyhow::{bail, Context, Result};
    use interprocess::local_socket::{prelude::*, Listener, ListenerOptions, Stream};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub fn make_private_dir(dir: &Path) -> Result<()> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let m = std::fs::symlink_metadata(dir)?;
        if !m.is_dir() {
            bail!("runtime dir {} is not a directory", dir.display());
        }
        // SAFETY: geteuid has no preconditions.
        if m.uid() != unsafe { libc::geteuid() } {
            return Err(super::Untrusted(format!(
                "runtime dir {} belongs to another user",
                dir.display()
            ))
            .into());
        }
        if m.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    pub fn connect(rt: &Path) -> Result<Stream> {
        Ok(Stream::connect(super::endpoint(rt)?)?)
    }

    /// Shuts the connection down in both directions when called. It holds its own descriptor
    /// (a duplicate), so it cannot hit a descriptor number reused after the stream was dropped.
    pub fn closer(s: &Stream) -> Box<dyn FnOnce() + Send> {
        use std::os::fd::{AsFd, AsRawFd};
        let Stream::UdSocket(u) = s;
        match u.as_fd().try_clone_to_owned() {
            Ok(fd) => Box::new(move || {
                // SAFETY: `fd` is a socket descriptor this closure owns.
                unsafe { libc::shutdown(fd.as_raw_fd(), libc::SHUT_RDWR) };
            }),
            Err(_) => Box::new(|| {}),
        }
    }

    pub fn listener(opts: ListenerOptions<'_>) -> std::io::Result<Listener> {
        use interprocess::os::unix::local_socket::ListenerOptionsExt;
        opts.reclaim_name(false).mode(0o600).create_sync()
    }

    /// The 0700 directory already keeps other users out; where the OS reports the peer pid
    /// (Linux, not macOS), it must also be the pid the server wrote.
    pub fn check_peer(s: &Stream, pid: &str) -> Result<()> {
        if let Some(p) = s.peer_creds().ok().and_then(|c| c.pid()) {
            if p.to_string() != pid {
                bail!("the resident process endpoint is held by pid {p}, not by the fbrain server ({pid})");
            }
        }
        Ok(())
    }

    pub fn detach(cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe; it gives the server its own session, so the
        // terminal's signals and hangup do not reach it.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    static STOP: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_signal(_: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }

    /// SIGTERM and SIGINT set a flag that the idle watcher acts on.
    pub fn on_stop(_rt: &Path) {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe {
            libc::signal(libc::SIGTERM, handler);
            libc::signal(libc::SIGINT, handler);
        }
    }

    pub fn stop_requested() -> bool {
        STOP.load(Ordering::SeqCst)
    }

    #[cfg(target_os = "linux")]
    pub fn rss_mb() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("VmRSS:"))
                    .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            })
            .map_or(0, |kb| kb / 1024)
    }

    /// macOS reports the peak, not the current, resident size here.
    #[cfg(not(target_os = "linux"))]
    pub fn rss_mb() -> u64 {
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: `ru` is a valid, zeroed rusage the call fills in.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
            return 0;
        }
        ru.ru_maxrss as u64 / (1 << 20)
    }
}

#[cfg(windows)]
mod sys {
    use anyhow::{bail, Context, Result};
    use interprocess::local_socket::{prelude::*, Listener, ListenerOptions, Stream};
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetNamedSecurityInfoW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, ACL, DACL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// Administrators own folders an elevated process creates; they can read the user's files anyway.
    const ADMINISTRATORS: &str = "S-1-5-32-544";

    fn wide(s: &str) -> Result<widestring::U16CString> {
        widestring::U16CString::from_str(s).map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// The SID string of `sid`.
    unsafe fn sid_string(sid: PSID) -> Result<String> {
        let mut s: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut s) == 0 {
            bail!("cannot format a SID: {}", std::io::Error::last_os_error());
        }
        let n = (0..).take_while(|&i| *s.add(i) != 0).count();
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
        LocalFree(s.cast());
        Ok(out)
    }

    /// A security descriptor parsed from SDDL, freed on drop, with its DACL. A descriptor
    /// without one is an error: a null DACL handed to `SetNamedSecurityInfoW` allows everyone.
    #[derive(Debug)]
    pub struct Sddl {
        sd: PSECURITY_DESCRIPTOR,
        pub dacl: *mut ACL,
    }

    impl Drop for Sddl {
        fn drop(&mut self) {
            // SAFETY: `sd` came from ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe { LocalFree(self.sd.cast()) };
        }
    }

    pub fn dacl_from_sddl(sddl: &str) -> Result<Sddl> {
        let text = wide(sddl)?;
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: NUL-terminated input; the descriptor is owned by the returned `Sddl`.
        unsafe {
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            ) == 0
            {
                bail!(
                    "cannot build the security descriptor {sddl}: {}",
                    std::io::Error::last_os_error()
                );
            }
            let mut out = Sddl {
                sd,
                dacl: std::ptr::null_mut(),
            };
            let (mut present, mut defaulted) = (0, 0);
            if GetSecurityDescriptorDacl(sd, &mut present, &mut out.dacl, &mut defaulted) == 0 {
                bail!(
                    "cannot read the DACL of {sddl}: {}",
                    std::io::Error::last_os_error()
                );
            }
            if present == 0 || out.dacl.is_null() {
                bail!("security descriptor {sddl} has no DACL");
            }
            Ok(out)
        }
    }

    /// Creates the dir, refuses one owned by another user, and gives it a protected DACL that
    /// allows only this user; files made inside (token, pid, log) inherit it.
    pub fn make_private_dir(dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let me = user_sid()?;
        let path = wide(&dir.display().to_string())?;
        // SAFETY: Win32 security calls on a path we own; every returned descriptor is freed.
        unsafe {
            let mut owner: PSID = std::ptr::null_mut();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            );
            if rc != 0 {
                bail!("cannot read the owner of {}: error {rc}", dir.display());
            }
            let owner_sid = sid_string(owner);
            LocalFree(sd.cast());
            let owner_sid = owner_sid?;
            if owner_sid != me && owner_sid != ADMINISTRATORS {
                return Err(super::Untrusted(format!(
                    "runtime dir {} belongs to another user ({owner_sid})",
                    dir.display()
                ))
                .into());
            }
            let want = dacl_from_sddl(&format!("D:P(A;OICI;FA;;;{me})"))?;
            let rc = SetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                want.dacl,
                std::ptr::null(),
            );
            if rc != 0 {
                bail!("cannot make {} private: error {rc}", dir.display());
            }
        }
        Ok(())
    }

    /// `fbrain-<user>`, or one pipe per directory when `FBRAIN_RUNTIME_DIR` is set (tests).
    pub fn pipe_name(rt: &Path) -> String {
        if std::env::var_os("FBRAIN_RUNTIME_DIR").is_some_and(|v| !v.is_empty()) {
            let dir: String = rt
                .display()
                .to_string()
                .chars()
                .map(|c| if c == '\\' || c == ':' { '_' } else { c })
                .collect();
            return format!("fbrain-test-{dir}");
        }
        let user: String = std::env::var("USERNAME")
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        format!("fbrain-{user}")
    }

    /// The SID string of the user running this process.
    pub fn user_sid() -> Result<String> {
        // SAFETY: the pseudo-handle of this process needs no closing.
        unsafe { process_sid(GetCurrentProcess()) }
    }

    /// The SID string of the user that runs process `pid`.
    fn pid_sid(pid: u32) -> Result<String> {
        // SAFETY: the process handle is closed after its token is read.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                bail!(
                    "cannot open process {pid}: {}",
                    std::io::Error::last_os_error()
                );
            }
            let sid = process_sid(h);
            CloseHandle(h);
            sid
        }
    }

    /// SAFETY: `process` must be a valid process handle with query access.
    unsafe fn process_sid(process: HANDLE) -> Result<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            bail!(
                "cannot open the process token: {}",
                std::io::Error::last_os_error()
            );
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
        CloseHandle(token);
        if ok == 0 {
            bail!(
                "cannot read the token user: {}",
                std::io::Error::last_os_error()
            );
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        sid_string(user.User.Sid)
    }

    /// A protected DACL with one entry: full access for the current user, nobody else.
    pub fn user_only_sddl() -> Result<String> {
        Ok(format!("D:P(A;;GA;;;{})", user_sid()?))
    }

    pub fn listener(opts: ListenerOptions<'_>) -> std::io::Result<Listener> {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        let sddl = user_only_sddl().map_err(std::io::Error::other)?;
        let wide = widestring::U16CString::from_str(&sddl).map_err(std::io::Error::other)?;
        let sd = SecurityDescriptor::deserialize(&wide)?;
        opts.reclaim_name(false)
            .security_descriptor(sd)
            .create_sync()
    }

    /// Another user may create a pipe of the same name first; only the process whose pid is
    /// in the user-only pid file gets the token.
    pub fn check_peer(s: &Stream, pid: &str) -> Result<()> {
        let p = s
            .peer_creds()
            .ok()
            .and_then(|c| c.pid())
            .context("cannot read the pid behind the resident process pipe")?;
        if p.to_string() != pid {
            bail!("the resident process pipe is held by pid {p}, not by the fbrain server ({pid})");
        }
        let owner = pid_sid(p)?;
        if owner != user_sid()? {
            bail!("the resident process pipe is held by another user ({owner}), not by the fbrain server");
        }
        Ok(())
    }

    /// Opens the pipe at identification level, so the server can check who we are but cannot
    /// act as us. Overlapped, as interprocess expects of its pipe handles.
    /// Named pipes have no portable shutdown from another thread here: a timed-out call leaves
    /// its helper thread until the server answers or exits (the MCP breaker bounds how many).
    pub fn closer(_: &Stream) -> Box<dyn FnOnce() + Send> {
        Box::new(|| {})
    }

    pub fn connect(rt: &Path) -> Result<Stream> {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
        };
        let path = wide(&format!(r"\\.\pipe\{}", pipe_name(rt)))?;
        // SAFETY: a plain CreateFileW on a NUL-terminated path; the handle is owned at once.
        let handle = unsafe {
            let h = CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error().into());
            }
            OwnedHandle::from_raw_handle(h)
        };
        let np = interprocess::os::windows::named_pipe::local_socket::Stream::try_from(handle)
            .map_err(|e| anyhow::anyhow!("cannot use the pipe handle: {e}"))?;
        Ok(np.into())
    }

    pub fn detach(cmd: &mut std::process::Command) {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    static RUNTIME: OnceLock<PathBuf> = OnceLock::new();

    unsafe extern "system" fn on_console(_ctrl: u32) -> windows_sys::core::BOOL {
        if let Some(rt) = RUNTIME.get() {
            super::remove_server_files(rt);
        }
        0
    }

    /// A server started from a console removes its files when the console closes or gets
    /// Ctrl+C; one started detached has no console.
    pub fn on_stop(rt: &Path) {
        let _ = RUNTIME.set(rt.to_path_buf());
        // SAFETY: registers a handler with the signature the API expects.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_console), 1);
        }
    }

    pub fn stop_requested() -> bool {
        false
    }

    pub fn rss_mb() -> u64 {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        // SAFETY: `c` is a zeroed counters struct of the size passed.
        unsafe {
            let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) == 0 {
                return 0;
            }
            c.WorkingSetSize as u64 / (1 << 20)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_descriptor_without_a_dacl_is_refused_not_applied_as_allow_all() {
            // Owner only, no D: part: GetSecurityDescriptorDacl reports no DACL present.
            let e = dacl_from_sddl("O:BA").unwrap_err();
            assert!(e.to_string().contains("no DACL"), "{e}");
            let sid = user_sid().unwrap();
            let sd = dacl_from_sddl(&format!("D:P(A;OICI;FA;;;{sid})")).unwrap();
            assert!(!sd.dacl.is_null());
        }

        #[test]
        fn a_runtime_dir_this_user_created_is_accepted_and_made_private() {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("run");
            make_private_dir(&dir).unwrap();
            make_private_dir(&dir).unwrap();
            assert!(dir.is_dir());
        }

        #[test]
        fn the_pipe_descriptor_names_only_the_current_user() {
            let sddl = user_only_sddl().unwrap();
            assert!(sddl.starts_with("D:P(A;;GA;;;S-1-5-"), "{sddl}");
            assert_eq!(sddl.matches('(').count(), 1, "{sddl}");
            let wide = widestring::U16CString::from_str(&sddl).unwrap();
            SecurityDescriptor::deserialize(&wide).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_embed_call_times_out_like_a_search_not_like_a_reindex() {
        let embed = Request::Embed {
            texts: Vec::new(),
            kind: EmbedKind::Document,
        };
        let reindex = Request::Reindex {
            brain: PathBuf::new(),
            full: false,
        };
        assert_eq!(call_timeout(&embed), SEARCH_TIMEOUT);
        assert_eq!(call_timeout(&reindex), REINDEX_TIMEOUT);
    }

    #[test]
    fn tokens_compare_equal_only_when_identical() {
        assert!(same_token("abc", "abc"));
        assert!(!same_token("abc", "abd"));
        assert!(!same_token("abc", "ab"));
        assert!(!same_token("", "a"));
    }

    #[test]
    fn a_line_over_the_limit_is_an_error_not_a_truncated_request() {
        let big = vec![b'x'; MAX_LINE as usize + 1];
        let e = read_line(&mut &big[..]).unwrap_err();
        assert!(e.to_string().contains("larger than"), "{e}");
        assert_eq!(
            read_line(&mut &b"{}\n"[..]).unwrap().as_deref(),
            Some("{}\n")
        );
        assert_eq!(read_line(&mut &b""[..]).unwrap(), None);
    }

    #[test]
    fn a_request_with_an_unknown_field_is_rejected() {
        let ok = r#"{"Reindex":{"brain":"/b","full":false}}"#;
        assert!(serde_json::from_str::<Request>(ok).is_ok());
        let extra = r#"{"Reindex":{"brain":"/b","full":false,"x":1}}"#;
        assert!(serde_json::from_str::<Request>(extra).is_err());
    }

    #[test]
    fn queries_over_the_limits_are_refused() {
        let mut q = Query::new("x", Default::default());
        assert!(check_query(&q).is_ok());
        q.k = MAX_K + 1;
        assert!(check_query(&q).is_err());
        let long = Query::new(&"a".repeat(MAX_QUERY_CHARS + 1), Default::default());
        assert!(check_query(&long).is_err());
    }

    /// A listener on a fresh endpoint: a socket file on Unix, a uniquely named pipe on Windows.
    fn test_listener() -> (
        interprocess::local_socket::Listener,
        Name<'static>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let name = {
            use interprocess::local_socket::GenericFilePath;
            dir.path()
                .join("t.sock")
                .to_fs_name::<GenericFilePath>()
                .unwrap()
        };
        #[cfg(windows)]
        let name = {
            use interprocess::local_socket::GenericNamespaced;
            let n = dir
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            format!("fbrain-unit-{}-{n}", std::process::id())
                .to_ns_name::<GenericNamespaced>()
                .unwrap()
        };
        let l = ListenerOptions::new()
            .name(name.clone())
            .create_sync()
            .unwrap();
        (l, name, dir)
    }

    #[test]
    fn a_reply_that_never_comes_times_out_instead_of_blocking() {
        let (listener, name, _dir) = test_listener();
        let server = std::thread::spawn(move || {
            let s = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(2));
            drop(s);
        });
        let conn = BufReader::new(Stream::connect(name).unwrap());
        let t = Instant::now();
        let (back, reply) = read_reply(conn, Duration::from_millis(200));
        assert!(matches!(reply, Reply::TimedOut));
        assert!(back.is_none());
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        server.join().unwrap();
    }

    #[test]
    fn a_reply_line_comes_back_with_the_connection() {
        let (listener, name, _dir) = test_listener();
        let server = std::thread::spawn(move || {
            let s = listener.accept().unwrap();
            write_msg(&s, &Response::Ready).unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        let conn = BufReader::new(Stream::connect(name).unwrap());
        let (back, reply) = read_reply(conn, Duration::from_secs(5));
        assert!(matches!(reply, Reply::Line(ref l) if l.trim() == r#""Ready""#));
        assert!(back.is_some());
        server.join().unwrap();
    }

    #[test]
    fn a_closed_connection_reads_as_closed() {
        let (listener, name, _dir) = test_listener();
        let server = std::thread::spawn(move || drop(listener.accept().unwrap()));
        let conn = BufReader::new(Stream::connect(name).unwrap());
        server.join().unwrap();
        let (_, reply) = read_reply(conn, Duration::from_secs(5));
        assert!(matches!(reply, Reply::Closed));
    }

    #[test]
    fn a_relative_brain_path_is_refused() {
        assert!(open_brain(Path::new("relative/brain")).is_err());
    }
}
