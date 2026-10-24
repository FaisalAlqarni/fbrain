//! `fbrain mcp`: a stdio MCP server with four tools, a thin client of the resident process.
//! It answers `initialize` and `tools/list` itself, without touching the resident process, and
//! never serves a note the MCP surface may not see (`search::expose`).

use crate::brain::Brain;
use crate::config;
use crate::index::{embed, Index};
use crate::proposals::{self, Op, ProposalInput};
use crate::search::{self, Filter, Hit, Query, Surface};
use crate::serve::{self, EmbedKind};
use anyhow::{bail, Context, Result};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, RoleServer, ServerHandler, ServiceExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Largest `k` a search may ask for.
const MAX_K: usize = 100;
/// How long a call waits for a resident process that is still starting, before it runs
/// keyword-only and says `starting`.
const START_GRACE: Duration = Duration::from_millis(250);
/// Consecutive timeouts that stop the client from calling the resident process for a while.
const BREAKER_AFTER: u32 = 3;
const BREAKER_PAUSE: Duration = Duration::from_secs(60);
/// After a failed start, the next call that wants the resident process tries again after this.
const START_RETRY: Duration = Duration::from_secs(10);
/// How long a call waits for the connection while another call holds it.
const LOCK_WAIT: Duration = Duration::from_secs(2);
/// Largest text arguments, in characters (the body in bytes).
const MAX_TEXT: usize = 2000;
const MAX_BODY: usize = 64 * 1024;
const MAX_CLIENT_NAME: usize = 64;

#[derive(Deserialize, JsonSchema)]
struct SearchParams {
    /// What to look for, in English or Arabic.
    query: String,
    /// How many notes to return (default 10, at most 100).
    k: Option<usize>,
    /// Include superseded, invalidated and expired notes.
    history: Option<bool>,
    /// Judge validity as of this date (YYYY-MM-DD) instead of today.
    as_of: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GetParams {
    /// A note's path inside the brain, or its name.
    note: String,
    /// Also return the note's outgoing links and backlinks.
    links: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct ProposeParams {
    /// What the proposal does to the target.
    op: Op,
    /// Note path inside the brain (the new path for `add`).
    target: String,
    /// The proposed content: the full note for `add`, the new content for `update`.
    body: String,
    /// One line on why.
    rationale: String,
    /// Where this came from: a session id, a file or a URL.
    source: String,
    /// With `supersede`: the note being replaced.
    supersedes: Option<String>,
}

impl JsonSchema for Op {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Op".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "enum": ["add", "update", "supersede", "invalidate"]})
    }
}

/// Why the resident process is not being used.
#[derive(Clone, Copy, PartialEq)]
enum OffKind {
    /// The last start failed; the next call after `until` tries again.
    StartFailed,
    /// Repeated timeouts: the circuit breaker is open.
    Breaker,
    /// An endpoint that is not ours: every call fails until `until`, then tries again.
    Untrusted,
}

/// What the connection to the resident process is doing.
enum Kind {
    Idle,
    Starting,
    Ready,
    /// Not used until `until`; `why` is the last failure.
    Off {
        why: String,
        until: Instant,
        kind: OffKind,
    },
}

/// The MCP server's one connection to the resident process. A client that failed or timed out
/// is dropped and replaced, so a call never reuses a connection in an unknown state, and
/// repeated timeouts stop the calls for a while instead of piling up helper threads.
struct Resident {
    kind: Mutex<Kind>,
    changed: Condvar,
    /// Held for the whole call, so calls take turns.
    client: Mutex<Option<serve::Client>>,
    model: Mutex<String>,
    timeouts: AtomicU32,
}

/// What a call to the resident process came to. A failure that must not be hidden behind
/// keyword hits (an untrusted endpoint, a request the server rejected) is an `Err` instead.
enum Outcome<T> {
    Done(T),
    /// Not used, and why: still starting, busy, off, or a call that failed in transit.
    Off(String),
}

fn remaining(until: Instant) -> u64 {
    until.saturating_duration_since(Instant::now()).as_secs() + 1
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Resident {
    fn new() -> Resident {
        Resident {
            kind: Mutex::new(Kind::Idle),
            changed: Condvar::new(),
            client: Mutex::new(None),
            model: Mutex::new(String::new()),
            timeouts: AtomicU32::new(0),
        }
    }

    fn set(&self, k: Kind) {
        *lock(&self.kind) = k;
        self.changed.notify_all();
    }

    /// Starts connecting on a helper thread, so no tool call waits for a slow start.
    fn start(self: &Arc<Self>, model_id: &str) {
        *lock(&self.kind) = Kind::Starting;
        let me = Arc::clone(self);
        let id = model_id.to_string();
        std::thread::spawn(move || match serve::Client::connect_or_start(&id) {
            Ok(c) => {
                *lock(&me.client) = Some(c);
                me.set(Kind::Ready);
            }
            Err(e) => {
                eprintln!("fbrain mcp: the resident process did not start: {e:#}");
                let untrusted = serve::is_untrusted(&e);
                me.set(Kind::Off {
                    why: format!("resident process unavailable: {e:#}"),
                    until: Instant::now()
                        + if untrusted {
                            BREAKER_PAUSE
                        } else {
                            START_RETRY
                        },
                    kind: if untrusted {
                        OffKind::Untrusted
                    } else {
                        OffKind::StartFailed
                    },
                });
            }
        });
    }

    /// Runs `f` on the connection. With `may_start`, an absent resident process is started.
    /// `Err` is for what must not be answered with keyword hits: an endpoint that is not ours
    /// and a request the server rejected.
    fn with<T>(
        self: &Arc<Self>,
        model_id: &str,
        may_start: bool,
        f: impl FnOnce(&mut serve::Client) -> Result<T>,
    ) -> Result<Outcome<T>> {
        if *lock(&self.model) != model_id {
            // Another brain uses another model: that needs another connection.
            *lock(&self.client) = None;
            *lock(&self.model) = model_id.to_string();
            *lock(&self.kind) = Kind::Idle;
        }
        let mut kind = lock(&self.kind);
        if matches!(&*kind, Kind::Off { until, .. } if Instant::now() >= *until) {
            *kind = Kind::Idle;
        }
        if matches!(&*kind, Kind::Idle) {
            if !may_start {
                return Ok(Outcome::Off("the resident process was not started".into()));
            }
            drop(kind);
            self.start(model_id);
            kind = lock(&self.kind);
        }
        if matches!(&*kind, Kind::Starting) {
            kind = self
                .changed
                .wait_timeout_while(kind, START_GRACE, |k| matches!(k, Kind::Starting))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        match &*kind {
            Kind::Starting => return Ok(Outcome::Off("resident process starting".into())),
            Kind::Off {
                why,
                until,
                kind: OffKind::Untrusted,
            } => {
                let _ = until;
                bail!("{why}")
            }
            Kind::Off { why, until, .. } => {
                return Ok(Outcome::Off(format!(
                    "{why}; retrying in {} s",
                    remaining(*until)
                )))
            }
            Kind::Idle => return Ok(Outcome::Off("the resident process went away".into())),
            Kind::Ready => {}
        }
        drop(kind);
        // Calls take turns, but a call does not queue behind a hung one.
        let deadline = Instant::now() + LOCK_WAIT;
        let mut slot = loop {
            match self.client.try_lock() {
                Ok(g) => break g,
                Err(std::sync::TryLockError::Poisoned(p)) => break p.into_inner(),
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => {
                    return Ok(Outcome::Off(
                        "resident process busy with another call".into(),
                    ))
                }
            }
        };
        let Some(client) = slot.as_mut() else {
            self.set(Kind::Idle);
            return Ok(Outcome::Off(
                "the resident process connection was replaced".into(),
            ));
        };
        match f(client) {
            Ok(v) => {
                self.timeouts.store(0, Ordering::SeqCst);
                Ok(Outcome::Done(v))
            }
            // The server answered, so the connection is fine and the request is the problem.
            Err(e) if serve::is_rejected(&e) => Err(e),
            Err(e) => {
                // Whatever else went wrong, this connection is not used again.
                *slot = None;
                drop(slot);
                let why = format!("{e:#}");
                eprintln!("fbrain mcp: resident process call failed: {why}");
                if serve::is_untrusted(&e) {
                    self.set(Kind::Idle);
                    return Err(e);
                }
                if serve::is_timeout(&e) {
                    if self.timeouts.fetch_add(1, Ordering::SeqCst) + 1 >= BREAKER_AFTER {
                        self.timeouts.store(0, Ordering::SeqCst);
                        self.set(Kind::Off {
                            why: format!("resident process stopped answering: {why}"),
                            until: Instant::now() + BREAKER_PAUSE,
                            kind: OffKind::Breaker,
                        });
                        return Ok(Outcome::Off(format!(
                            "resident process stopped answering: {why}; retrying in {} s",
                            BREAKER_PAUSE.as_secs()
                        )));
                    }
                } else {
                    self.timeouts.store(0, Ordering::SeqCst);
                }
                self.set(Kind::Idle);
                Ok(Outcome::Off(format!("resident process call failed: {why}")))
            }
        }
    }

    fn label(&self) -> Option<&'static str> {
        match &*lock(&self.kind) {
            Kind::Ready => Some("running"),
            Kind::Starting => Some("starting"),
            _ => None,
        }
    }

    /// The breaker as `health` shows it.
    fn breaker(&self) -> Value {
        match &*lock(&self.kind) {
            Kind::Off {
                why,
                until,
                kind: OffKind::Breaker,
            } => json!({"state": "open", "until_s": remaining(*until), "reason": why}),
            _ => json!({"state": "closed"}),
        }
    }
}

#[derive(Clone)]
struct Fbrain {
    brain_arg: Option<String>,
    resident: Arc<Resident>,
    tool_router: ToolRouter<Fbrain>,
}

fn text_error(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

/// A finished tool: its JSON, or the error text for the agent.
fn finish(r: Result<Value>) -> CallToolResult {
    match r {
        Ok(v) => CallToolResult::structured(v),
        Err(e) => text_error(format!("{e:#}")),
    }
}

async fn blocking(f: impl FnOnce() -> Result<Value> + Send + 'static) -> CallToolResult {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => finish(r),
        Err(e) => text_error(format!("the tool stopped unexpectedly: {e}")),
    }
}

#[tool_router]
impl Fbrain {
    /// Search the brain's notes. Returns hits with a snippet and why each matched, and whether
    /// meaning-based search was on.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn search(&self, Parameters(p): Parameters<SearchParams>) -> CallToolResult {
        let me = self.clone();
        blocking(move || me.do_search(p)).await
    }

    /// Read one note by path or name.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn get(&self, Parameters(p): Parameters<GetParams>) -> CallToolResult {
        let me = self.clone();
        blocking(move || me.do_get(p)).await
    }

    /// Propose a change. This writes one file to the brain's inbox and lists notes that may
    /// conflict; only the user approves it.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        open_world_hint = false
    ))]
    async fn propose(
        &self,
        Parameters(p): Parameters<ProposeParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let by = ctx
            .peer
            .peer_info()
            .map(|i| {
                i.client_info
                    .name
                    .trim()
                    .chars()
                    .take(MAX_CLIENT_NAME)
                    .collect::<String>()
            })
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "mcp".into());
        let me = self.clone();
        blocking(move || me.do_propose(p, by)).await
    }

    /// Report the brain, note count, resident process, model, index age and a timed search.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn health(&self) -> CallToolResult {
        let me = self.clone();
        blocking(move || me.do_health()).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Fbrain {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Search and read the user's notes, decisions and memory with search and get. \
                 Record a lesson, decision or status change with propose; it goes to an inbox \
                 and only the user approves it. Do not use this for code search.",
        )
    }
}

impl Fbrain {
    fn brain(&self) -> Result<(Brain, String)> {
        let cwd = std::env::current_dir()?;
        let r = config::resolve_brain(self.brain_arg.as_deref(), &cwd)?;
        let mut brain = Brain::open(&r.path)?;
        brain.path = brain.path.canonicalize().unwrap_or(brain.path);
        Ok((brain, r.name))
    }

    /// The index built in this process without a model. It re-reads only changed files, so
    /// a note added or approved a moment ago is in it.
    fn keyword_index(&self, brain: &Brain) -> Result<Index> {
        Index::load_or_build(brain, &config::cache_dir()?, false, None)
    }

    /// Runs on the resident process when the model is there, else says why not.
    fn relay<T>(
        &self,
        brain: &Brain,
        may_start: bool,
        f: impl FnOnce(&mut serve::Client) -> Result<T>,
    ) -> Result<Outcome<T>> {
        let id = config::model_id(Some(brain));
        if !embed::is_downloaded(&config::cache_dir()?, &id)? {
            return Ok(Outcome::Off(
                "model not downloaded, run fbrain doctor --fix".into(),
            ));
        }
        self.resident.with(&id, may_start, f)
    }

    /// Hits, and why they are keyword-only (`None` when the resident process answered).
    fn search_hits(
        &self,
        brain: &Brain,
        q: &Query,
        may_start: bool,
    ) -> Result<(Vec<Hit>, Option<String>)> {
        let why = match self.relay(brain, may_start, |c| c.search(&brain.path, q))? {
            Outcome::Done(h) => return Ok((h, None)),
            Outcome::Off(w) => w,
        };
        eprintln!("fbrain mcp: semantic search off: {why}");
        let index = self.keyword_index(brain)?;
        Ok((search::search(&index, q, None)?, Some(why)))
    }

    fn do_search(&self, p: SearchParams) -> Result<Value> {
        check_len("query", &p.query, MAX_TEXT)?;
        if let Some(d) = &p.as_of {
            let ok = d.len() == 10
                && d.bytes().enumerate().all(|(i, b)| {
                    if i == 4 || i == 7 {
                        b == b'-'
                    } else {
                        b.is_ascii_digit()
                    }
                });
            if !ok {
                bail!("as_of must be YYYY-MM-DD");
            }
        }
        let (brain, _) = self.brain()?;
        let mut q = Query::new(
            &p.query,
            Filter {
                history: p.history.unwrap_or(false),
                as_of: p.as_of,
                statuses: None,
            },
        );
        q.k = p.k.unwrap_or(10).clamp(1, MAX_K);
        q.budget_tokens = config::token_budget(Some(&brain));
        q.surface = Surface::Mcp;
        let (hits, off) = self.search_hits(&brain, &q, true)?;
        Ok(with_semantic(json!({"hits": hits}), off))
    }

    fn do_get(&self, p: GetParams) -> Result<Value> {
        check_len("note", &p.note, MAX_TEXT)?;
        let (brain, _) = self.brain()?;
        let missing = || anyhow::anyhow!("note not found: {}", p.note);
        let found = brain.find(&p.note)?.ok_or_else(missing)?;
        let text = std::fs::read_to_string(&found.abs_path)
            .with_context(|| format!("cannot read {}", found.rel_path))?;
        let n = crate::note::Note::parse(&found.rel_path, &text);
        if !search::expose(&n, Surface::Mcp) {
            return Err(missing());
        }
        let mut v = json!({
            "rel_path": n.rel_path,
            "title": n.title,
            "status": n.fm.status,
            "frontmatter": n.fm,
            "body": n.body,
        });
        if p.links.unwrap_or(false) {
            let index = self.keyword_index(&brain)?;
            let (out, back) = crate::cli::link_info(&index, &n.rel_path, Surface::Mcp);
            v["outgoing"] = json!(out);
            v["backlinks"] = json!(back);
        }
        Ok(v)
    }

    fn do_propose(&self, p: ProposeParams, by: String) -> Result<Value> {
        check_len("target", &p.target, MAX_TEXT)?;
        check_len("rationale", &p.rationale, MAX_TEXT)?;
        check_len("source", &p.source, MAX_TEXT)?;
        if let Some(s) = &p.supersedes {
            check_len("supersedes", s, MAX_TEXT)?;
        }
        if p.body.len() > MAX_BODY {
            bail!("body is longer than {MAX_BODY} bytes");
        }
        let (brain, _) = self.brain()?;
        let input = ProposalInput {
            op: p.op,
            target: p.target,
            supersedes: p.supersedes,
            body: p.body,
            proposed_by: by,
            source: p.source,
            rationale: p.rationale,
            extra: Vec::new(),
        };
        let input = proposals::check(&brain, input)?;
        // The resident process refreshes the embedded cache; this process only reads it.
        // Whatever keeps the check keyword-only is recorded, never dropped.
        let off: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
        let note_off = |why: String| {
            off.borrow_mut().get_or_insert(why);
        };
        let id = config::model_id(Some(&brain));
        let embedded = match self.relay(&brain, true, |c| c.reindex(&brain.path, false))? {
            Outcome::Done(_) => {
                let i = Index::load_embedded(&brain, &config::cache_dir()?, &id)?;
                if i.is_none() {
                    note_off("the embedded index is not available".into());
                }
                i
            }
            Outcome::Off(why) => {
                note_off(why);
                None
            }
        };
        let index = match embedded {
            Some(i) => i,
            None => self.keyword_index(&brain)?,
        };
        let mut embed_text = |text: &str| {
            Ok(
                match self.relay(&brain, true, |c| {
                    c.embed(&[text.to_string()], EmbedKind::Document)
                })? {
                    Outcome::Done(mut v) => Ok(v.remove(0)),
                    Outcome::Off(why) => {
                        note_off(why.clone());
                        Err(why)
                    }
                },
            )
        };
        let p = proposals::propose_with(&brain, &index, input, Surface::Mcp, &mut embed_text)?;
        Ok(with_semantic(
            json!({
                "id": p.id,
                "rel_path": p.rel_path,
                "conflicts": p.conflicts,
                "conflicts_protected": p.conflicts_protected,
            }),
            off.into_inner(),
        ))
    }

    fn do_health(&self) -> Result<Value> {
        let (brain, name) = self.brain()?;
        let cache = config::cache_dir()?;
        let id = config::model_id(Some(&brain));
        // Before the index below is refreshed, so a brain with no cache reads as unknown.
        let age = Index::cache_age_s(&brain, &cache);
        let mut q = Query::new("fbrain", Filter::default());
        q.surface = Surface::Mcp;
        let start = Instant::now();
        let (_, off) = self.search_hits(&brain, &q, false)?;
        let search_ms = start.elapsed().as_millis() as u64;
        let index = self.keyword_index(&brain)?;
        let notes = index
            .files()
            .filter(|(_, f)| search::exposed(f, Surface::Mcp))
            .count();
        let mut server_error = None;
        let server = match self.resident.label() {
            Some(l) => l,
            None => match serve::status_if_running(&id) {
                Ok(Some(_)) => "running",
                Ok(None) => "unavailable",
                Err(e) => {
                    server_error = Some(format!("{e:#}"));
                    "unavailable"
                }
            },
        };
        let mut v = with_semantic(
            json!({
                "brain": name,
                "notes": notes,
                "server": server,
                "model": if embed::is_downloaded(&cache, &id)? { "loaded" } else { "missing" },
                "index_age_s": age,
                "search_ms": search_ms,
                "search_kind": if off.is_none() { "semantic" } else { "keyword" },
                "breaker": self.resident.breaker(),
            }),
            off,
        );
        if let Some(e) = server_error {
            v["server_error"] = json!(e);
        }
        Ok(v)
    }
}

/// `semantic: true`, or `semantic: false` with the reason the result is keyword-only.
fn with_semantic(mut v: Value, off: Option<String>) -> Value {
    v["semantic"] = json!(off.is_none());
    if let Some(why) = off {
        v["semantic_reason"] = json!(why);
    }
    v
}

/// Tool arguments are cut off before any work, so a client cannot make a huge write or query.
fn check_len(field: &str, v: &str, max: usize) -> Result<()> {
    if v.chars().count() > max {
        bail!("{field} is longer than {max} characters");
    }
    Ok(())
}

/// Serves MCP on stdin and stdout until the client closes them.
pub fn run(brain_arg: Option<&str>) -> Result<()> {
    let server = Fbrain {
        brain_arg: brain_arg.map(str::to_string),
        resident: Arc::new(Resident::new()),
        tool_router: Fbrain::tool_router(),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .context("cannot start the async runtime")?;
    let done: Result<()> = rt.block_on(async {
        let running = server
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| anyhow::anyhow!("cannot start the MCP server: {e}"))?;
        running
            .waiting()
            .await
            .map_err(|e| anyhow::anyhow!("the MCP server stopped: {e}"))?;
        Ok(())
    });
    // Helper threads may still be blocked on the resident process; do not wait for them.
    rt.shutdown_background();
    if let Err(e) = &done {
        bail!("{e:#}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_does_not_queue_behind_a_call_that_holds_the_connection() {
        let r = Arc::new(Resident::new());
        *lock(&r.model) = "m".into();
        *lock(&r.kind) = Kind::Ready;
        let held = Arc::clone(&r);
        let (tx, rx) = std::sync::mpsc::channel();
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let t = std::thread::spawn(move || {
            let _slot = lock(&held.client);
            tx.send(()).unwrap();
            let _ = wait.recv();
        });
        rx.recv().unwrap();
        let start = Instant::now();
        let out = r.with("m", true, |_| Ok(1)).unwrap();
        let waited = start.elapsed();
        done.send(()).unwrap();
        t.join().unwrap();
        assert!(matches!(&out, Outcome::Off(w) if w.contains("busy")));
        assert!(waited >= LOCK_WAIT && waited < LOCK_WAIT * 2, "{waited:?}");
    }
}
