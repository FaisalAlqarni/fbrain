//! Benchmark runner: recall, MRR, latency and memory of `fbrain search` on a labelled query set,
//! the same numbers for obsidian-mind's `om` and for `qmd`, and the embedding model's load cost.
//! Query files and reports of a real brain stay outside the fbrain repo.

use anyhow::{bail, Context, Result};
use clap::Parser;
use fbrain::brain::Brain;
use fbrain::config;
use fbrain::index::embed::{self, Embedder};
use fbrain::index::Index;
use fbrain::search::{self, Filter, Query};
use fbrain::serve;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

/// Warm timing: every query runs this many times after one untimed pass.
const WARM_RUNS: usize = 5;
const TOP: usize = 10;

#[derive(Parser)]
#[command(
    name = "fbrain-bench",
    about = "Benchmark fbrain search on a labelled query set"
)]
struct Args {
    /// The brain to search
    #[arg(long)]
    brain: PathBuf,
    /// Query file (`[[query]]` tables with id, text, lang, expect)
    #[arg(long)]
    queries: PathBuf,
    /// `model=<id>`. Repeat for several variants.
    #[arg(long = "variant")]
    variants: Vec<String>,
    /// Also run obsidian-mind's `om` MCP server (needs --om-script)
    #[arg(long)]
    baseline_om: bool,
    /// Allow fbrain variants to run without the embedding model (keyword-only numbers)
    #[arg(long)]
    keyword_only: bool,
    /// Also run `qmd --index <name> query`
    #[arg(long)]
    baseline_qmd: bool,
    /// Measure load time and peak memory of one `fbrain search` process with the model
    #[arg(long)]
    measure_model: bool,
    /// Write report.json here
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long)]
    min_recall: Option<f64>,
    #[arg(long)]
    max_warm_p95_ms: Option<f64>,
    #[arg(long)]
    max_cold_p95_ms: Option<f64>,
    #[arg(long)]
    max_rss_mb: Option<f64>,
    /// First `fbrain search` including the resident process start
    #[arg(long)]
    max_first_call_ms: Option<f64>,
    /// Warm search p95 through the resident process
    #[arg(long)]
    max_serve_warm_p95_ms: Option<f64>,
    /// Resident process RSS with the brain indexed
    #[arg(long)]
    max_server_rss_mb: Option<f64>,
    /// Peak RSS of each `fbrain search` client process (the same number as peak_rss_mb)
    #[arg(long)]
    max_client_rss_mb: Option<f64>,
    /// The `fbrain` binary used for cold runs; default: next to this binary
    #[arg(long)]
    fbrain_bin: Option<PathBuf>,
    /// Path of om-mcp.mjs
    #[arg(long)]
    om_script: Option<PathBuf>,
    #[arg(long, default_value = "obsidian-mind")]
    qmd_index: String,
}

#[derive(Deserialize)]
struct QueryFile {
    #[serde(rename = "query")]
    queries: Vec<Labelled>,
}

#[derive(Deserialize)]
struct Labelled {
    id: String,
    text: String,
    lang: String,
    /// Optional label for a per-kind recall split (paraphrase, multi-note, ...).
    #[serde(default)]
    kind: Option<String>,
    expect: Vec<String>,
}

fn load_queries(path: &Path) -> Result<Vec<Labelled>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let f: QueryFile =
        toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
    for q in &f.queries {
        if !["en", "ar", "mixed"].contains(&q.lang.as_str()) {
            bail!("query {}: lang must be en, ar or mixed", q.id);
        }
        if q.expect.is_empty() {
            bail!("query {}: expect is empty", q.id);
        }
    }
    if f.queries.is_empty() {
        bail!("{} has no queries", path.display());
    }
    Ok(f.queries)
}

#[derive(Default, Clone)]
struct Variant {
    name: String,
    model: Option<String>,
}

fn parse_variant(s: &str) -> Result<Variant> {
    let mut v = Variant {
        name: s.to_string(),
        ..Default::default()
    };
    for pair in s.split(',') {
        let (k, val) = pair
            .split_once('=')
            .with_context(|| format!("variant `{pair}` is not key=value"))?;
        match k {
            "model" => v.model = Some(val.to_string()),
            "stemming" | "folding" | "chunking" => bail!("variant key `{k}` is not supported yet"),
            _ => bail!("unknown variant key `{k}` (model)"),
        }
    }
    Ok(v)
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * sorted.len() as f64).ceil().max(1.0) as usize;
    Some(sorted[rank.min(sorted.len()) - 1])
}

fn pct(mut xs: Vec<f64>, p: f64) -> Option<f64> {
    xs.sort_by(|a, b| a.total_cmp(b));
    percentile(&xs, p)
}

/// Quality numbers from the ranked note paths each query returned.
fn quality(qs: &[Labelled], ranked: &[Vec<String>]) -> Value {
    let rank_of = |q: &Labelled, r: &Vec<String>| {
        r.iter()
            .take(TOP)
            .position(|p| q.expect.contains(p))
            .map(|i| i + 1)
    };
    let ranks: Vec<Option<usize>> = qs.iter().zip(ranked).map(|(q, r)| rank_of(q, r)).collect();
    let recall = |sel: &dyn Fn(usize) -> bool| {
        let idx: Vec<usize> = (0..qs.len()).filter(|i| sel(*i)).collect();
        let hit = idx
            .iter()
            .filter(|i| ranks[**i].is_some_and(|r| r <= 5))
            .count();
        (!idx.is_empty()).then(|| hit as f64 / idx.len() as f64)
    };
    let mrr = ranks
        .iter()
        .map(|r| r.map_or(0.0, |r| 1.0 / r as f64))
        .sum::<f64>()
        / qs.len() as f64;
    let mut by_lang = BTreeMap::new();
    for lang in ["en", "ar", "mixed"] {
        if let Some(r) = recall(&|i| qs[i].lang == lang) {
            by_lang.insert(lang, r);
        }
    }
    let mut by_kind = BTreeMap::new();
    for kind in qs.iter().filter_map(|q| q.kind.as_deref()) {
        if let Some(r) = recall(&|i| qs[i].kind.as_deref() == Some(kind)) {
            by_kind.insert(kind, r);
        }
    }
    json!({
        "queries": qs.len(),
        "recall_at_5": recall(&|_| true),
        "mrr": mrr,
        "recall_by_lang": by_lang,
        "recall_by_kind": by_kind,
        "per_query": qs.iter().zip(&ranks).map(|(q, r)| json!({"id": q.id, "lang": q.lang, "rank": r})).collect::<Vec<_>>(),
    })
}

/// Runs `prog args` through this binary's `--rss-child` mode, which reports wall time and the
/// child's peak resident memory (unix only). Returns (wall ms, peak MB, exit ok).
fn measured(
    prog: &Path,
    args: &[OsString],
    envs: &[(String, String)],
) -> Result<(f64, Option<f64>, bool)> {
    let out = Command::new(std::env::current_exe()?)
        .arg("--rss-child")
        .arg(prog)
        .args(args)
        .envs(envs.iter().map(|(k, v)| (k, v)))
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let f: Vec<&str> = text.split_whitespace().collect();
    if f.len() != 3 {
        bail!(
            "rss helper failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let kb: f64 = f[1].parse()?;
    Ok((f[0].parse()?, (kb > 0.0).then(|| kb / 1024.0), f[2] == "1"))
}

fn rss_child(args: &[OsString]) -> ! {
    let t = Instant::now();
    let ok = Command::new(&args[0])
        .args(&args[1..])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let wall = t.elapsed().as_secs_f64() * 1000.0;
    println!("{wall} {} {}", children_peak_kb(), u8::from(ok));
    std::process::exit(0)
}

#[cfg(unix)]
fn children_peak_kb() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a valid, zeroed rusage the call fills in.
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) } != 0 {
        return 0.0; // reported as "not measured"
    }
    let max = ru.ru_maxrss as f64;
    if cfg!(target_os = "macos") {
        max / 1024.0
    } else {
        max
    }
}

#[cfg(not(unix))]
fn children_peak_kb() -> f64 {
    0.0
}

fn fbrain_bin(args: &Args) -> Result<PathBuf> {
    if let Some(p) = &args.fbrain_bin {
        return Ok(p.clone());
    }
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name(format!("fbrain{}", std::env::consts::EXE_SUFFIX)))
}

fn variant_env(v: &Variant) -> Vec<(String, String)> {
    let mut e = Vec::new();
    if let Some(m) = &v.model {
        e.push(("FBRAIN_MODEL".to_string(), m.clone()));
    }
    e
}

fn run_fbrain(args: &Args, qs: &[Labelled], v: &Variant) -> Result<Value> {
    let brain = Brain::open(&args.brain)?;
    let cache = config::cache_dir()?;
    let id = v
        .model
        .clone()
        .unwrap_or_else(|| config::model_id(Some(&brain)));
    let embedder = embed::load_if_present(&cache, &id)?;
    if embedder.is_none() && !args.keyword_only {
        bail!(
            "variant {} ran without the embedding model ({id} is not downloaded); run fbrain doctor --fix, or pass --keyword-only to measure keywords on purpose",
            v.name
        );
    }
    let index = Index::load_or_build(
        &brain,
        &cache,
        false,
        embedder.as_ref().map(|e| (id.as_str(), e)),
    )?;
    let run = |text: &str| -> Result<Vec<String>> {
        let q = Query::new(text, Filter::default());
        Ok(search::search(&index, &q, embedder.as_ref())?
            .into_iter()
            .filter(|h| h.explain.linked_from.is_none())
            .map(|h| h.rel_path)
            .collect())
    };
    let mut ranked = Vec::new();
    for q in qs {
        ranked.push(run(&q.text)?);
    }
    let mut warm = Vec::new();
    for q in qs {
        for _ in 0..WARM_RUNS {
            let t = Instant::now();
            run(&q.text)?;
            warm.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let bin = fbrain_bin(args)?;
    let envs = variant_env(v);
    let (served, cold_runs) = match &embedder {
        Some(_) => {
            if serve::status_if_running(&id)?.is_some() {
                bail!(
                    "a resident process is already running in {}; set FBRAIN_RUNTIME_DIR to an unused dir so the first call includes its start",
                    serve::runtime_dir()?.display()
                );
            }
            let served = run_serve(args, qs, &bin, &envs, &id);
            let cold = cold_searches(args, qs, &bin, &envs);
            // No server ran before, so the one running now is the one this run started.
            stop_server(&id)?;
            (Some(served?), cold)
        }
        None => (None, cold_searches(args, qs, &bin, &envs)),
    };
    let (cold, rss) = cold_runs?;
    let mut r = quality(qs, &ranked);
    // The embedding channel alone, to show what fusion adds.
    r["embedding_only"] = match &embedder {
        Some(e) => {
            let mut only = Vec::new();
            for q in qs {
                let qv = e.embed_queries(std::slice::from_ref(&q.text))?.remove(0);
                only.push(
                    embed::cosine_top(&index, &qv, TOP, None)?
                        .into_iter()
                        .map(|t| t.0)
                        .collect(),
                );
            }
            quality(qs, &only)
        }
        None => Value::Null,
    };
    r["name"] = json!(v.name);
    r["kind"] = json!("fbrain");
    r["semantic"] = json!(embedder.is_some());
    r["warm_p50_ms"] = json!(pct(warm.clone(), 0.5));
    r["warm_p95_ms"] = json!(pct(warm, 0.95));
    r["cold_p95_ms"] = json!(pct(cold, 0.95));
    r["peak_rss_mb"] = json!(rss);
    r["serve"] = served.unwrap_or(Value::Null);
    Ok(r)
}

/// Wall time and peak RSS of one `fbrain search` process per query.
fn cold_searches(
    args: &Args,
    qs: &[Labelled],
    bin: &Path,
    envs: &[(String, String)],
) -> Result<(Vec<f64>, Option<f64>)> {
    let (mut cold, mut rss) = (Vec::new(), None::<f64>);
    for q in qs {
        let (ms, mb, ok) = measured(bin, &search_argv(args, &q.text), envs)?;
        if !ok {
            bail!("`fbrain search` failed for query {}", q.id);
        }
        cold.push(ms);
        rss = mb.map_or(rss, |m| Some(rss.map_or(m, |r| r.max(m))));
    }
    Ok((cold, rss))
}

/// The first search including the resident process start, then the same queries as the
/// in-process run through the server.
fn run_serve(
    args: &Args,
    qs: &[Labelled],
    bin: &Path,
    envs: &[(String, String)],
    id: &str,
) -> Result<Value> {
    let (first_ms, _, ok) = measured(bin, &search_argv(args, &qs[0].text), envs)?;
    if !ok {
        bail!("the first `fbrain search` failed");
    }
    let mut client = serve::Client::connect_or_start_with(bin, id)?;
    let brain = args.brain.canonicalize()?;
    let mut run = |text: &str| -> Result<Vec<String>> {
        Ok(client
            .search(&brain, &Query::new(text, Filter::default()))?
            .into_iter()
            .filter(|h| h.explain.linked_from.is_none())
            .map(|h| h.rel_path)
            .collect())
    };
    let mut ranked = Vec::new();
    for q in qs {
        ranked.push(run(&q.text)?);
    }
    let mut warm = Vec::new();
    for q in qs {
        for _ in 0..WARM_RUNS {
            let t = Instant::now();
            run(&q.text)?;
            warm.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let status = client.status()?;
    let qual = quality(qs, &ranked);
    Ok(json!({
        "first_call_ms": first_ms,
        "warm_p50_ms": pct(warm.clone(), 0.5),
        "warm_p95_ms": pct(warm, 0.95),
        "server_rss_mb": status.rss_mb,
        "recall_at_5": qual["recall_at_5"],
        "recall_by_lang": qual["recall_by_lang"],
    }))
}

/// Stops the running server, if any, and waits until it no longer answers.
fn stop_server(id: &str) -> Result<()> {
    let Some(pid) = serve::status_if_running(id)?.map(|s| s.pid) else {
        return Ok(());
    };
    #[cfg(unix)]
    // SAFETY: kill with a pid the server itself reported; no memory is involved.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(windows)]
    Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output()?;
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while serve::status_if_running(id)?.is_some() {
        if Instant::now() > deadline {
            bail!("the resident process {pid} did not stop");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

fn search_argv(args: &Args, text: &str) -> Vec<OsString> {
    vec![
        "--brain".into(),
        args.brain.clone().into_os_string(),
        "--json".into(),
        "search".into(),
        text.into(),
    ]
}

fn run_qmd(args: &Args, qs: &[Labelled]) -> Result<Value> {
    let (mut ranked, mut times, mut rss) = (Vec::new(), Vec::new(), None::<f64>);
    for q in qs {
        let argv: Vec<OsString> = [
            "--index",
            &args.qmd_index,
            "query",
            &q.text,
            "--json",
            "-n",
            "10",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        // The helper discards stdout, so run once more through a plain process for the paths.
        let out = Command::new("qmd")
            .args(&argv)
            .output()
            .context("cannot run qmd")?;
        let v: Value = serde_json::from_slice(&out.stdout).context("qmd output is not JSON")?;
        ranked.push(dedupe(v.as_array().into_iter().flatten().filter_map(|h| {
            let f = h["file"].as_str()?.strip_prefix("qmd://")?;
            let f = f.split_once('/')?.1;
            Some(f.split('?').next()?.to_string())
        })));
        let (ms, mb, _) = measured(Path::new("qmd"), &argv, &[])?;
        times.push(ms);
        rss = mb.map_or(rss, |m| Some(rss.map_or(m, |r| r.max(m))));
    }
    let mut r = quality(qs, &ranked);
    r["name"] = json!("qmd query");
    r["kind"] = json!("qmd");
    r["warm_p50_ms"] = Value::Null;
    r["warm_p95_ms"] = Value::Null;
    r["cold_p95_ms"] = json!(pct(times, 0.95));
    r["peak_rss_mb"] = json!(rss);
    Ok(r)
}

fn dedupe(it: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = Vec::new();
    for p in it {
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    seen
}

/// Paths from om's `[n] <vault>/<path>:<line>  (score N%)` result lines.
fn om_paths(text: &str) -> Vec<String> {
    dedupe(text.lines().filter_map(|l| {
        let rest = l.strip_prefix('[')?.split_once("] ")?.1;
        let head = rest.split("  (score").next()?;
        let path = head.rsplit_once(':')?.0;
        Some(path.split_once('/')?.1.to_string())
    }))
}

fn run_om(args: &Args, qs: &[Labelled]) -> Result<Value> {
    let script = args
        .om_script
        .clone()
        .context("--baseline-om needs --om-script")?;
    let mut child = Command::new("node")
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot start node")?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().context("no stdout")?);
    let mut next_id = 0;
    let mut rpc = |method: &str, params: Value| -> Result<Value> {
        next_id += 1;
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params})
        )?;
        stdin.flush()?;
        loop {
            let mut line = String::new();
            if stdout.read_line(&mut line)? == 0 {
                bail!("om server closed its output");
            }
            let v: Value = serde_json::from_str(&line)?;
            if v["id"] == next_id {
                return Ok(v);
            }
        }
    };
    rpc(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "fbrain-bench", "version": "0"}}),
    )?;
    let mut ask = |text: &str| -> Result<(Vec<String>, f64)> {
        let t = Instant::now();
        let r = rpc(
            "tools/call",
            json!({"name": "search", "arguments": {"query": text}}),
        )?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let body = r["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        Ok((om_paths(body), ms))
    };
    // The first call loads om's own model; it is reported apart from the warm numbers.
    let first = ask(&qs[0].text)?.1;
    let (mut ranked, mut times) = (Vec::new(), Vec::new());
    for q in qs {
        let (p, ms) = ask(&q.text)?;
        ranked.push(p);
        times.push(ms);
    }
    let _ = child.kill();
    let mut r = quality(qs, &ranked);
    r["name"] = json!("om search");
    r["kind"] = json!("om");
    r["first_call_ms"] = json!(first);
    r["warm_p50_ms"] = json!(pct(times.clone(), 0.5));
    r["warm_p95_ms"] = json!(pct(times, 0.95));
    r["cold_p95_ms"] = Value::Null;
    r["peak_rss_mb"] = Value::Null;
    Ok(r)
}

/// Load time and peak memory of one `fbrain search` process with the configured model.
fn measure_model(args: &Args, qs: &[Labelled]) -> Result<Value> {
    let brain = Brain::open(&args.brain)?;
    let cache = config::cache_dir()?;
    let id = config::model_id(Some(&brain));
    let spec = embed::spec(&id)?;
    let dir = embed::ensure_model(&id, &cache, true)?;
    let t = Instant::now();
    let e = Embedder::load(&dir, spec)?;
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    drop(e);
    let mut bytes = 0;
    for (f, _) in spec.files {
        bytes += std::fs::metadata(dir.join(f))?.len();
    }
    let argv = [
        "--brain".into(),
        args.brain.clone().into_os_string(),
        "--json".into(),
        "search".into(),
        qs[0].text.clone().into(),
    ];
    let bin = fbrain_bin(args)?;
    // The first process builds the index; the second is the one reported.
    measured(&bin, &argv, &[])?;
    let (wall, rss, ok) = measured(&bin, &argv, &[])?;
    if !ok {
        bail!("`fbrain search` failed with model {id}");
    }
    Ok(
        json!([{"model": id, "stored_mb": bytes as f64 / 1048576.0, "load_ms": load_ms, "search_ms": wall, "peak_rss_mb": rss}]),
    )
}

fn fmt(v: &Value, digits: usize) -> String {
    v.as_f64().map_or("n/a".into(), |f| format!("{f:.digits$}"))
}

fn print_variant(r: &Value) {
    println!("variant {}", r["name"].as_str().unwrap_or_default());
    println!("  queries {}", r["queries"]);
    if r["kind"] == "fbrain" {
        println!("  semantic {}", r["semantic"]);
    }
    println!("  recall_at_5 {}", fmt(&r["recall_at_5"], 2));
    println!("  mrr {}", fmt(&r["mrr"], 2));
    println!("  warm_p50_ms {}", fmt(&r["warm_p50_ms"], 2));
    println!("  warm_p95_ms {}", fmt(&r["warm_p95_ms"], 2));
    println!("  cold_p95_ms {}", fmt(&r["cold_p95_ms"], 1));
    println!("  peak_rss_mb {}", fmt(&r["peak_rss_mb"], 1));
    if r["serve"].is_object() {
        let s = &r["serve"];
        println!("  serve.first_call_ms {}", fmt(&s["first_call_ms"], 0));
        println!("  serve.warm_p50_ms {}", fmt(&s["warm_p50_ms"], 2));
        println!("  serve.warm_p95_ms {}", fmt(&s["warm_p95_ms"], 2));
        println!("  serve.server_rss_mb {}", fmt(&s["server_rss_mb"], 0));
        println!("  serve.recall_at_5 {}", fmt(&s["recall_at_5"], 2));
    }
    if let Some(m) = r["recall_by_lang"].as_object() {
        for (lang, v) in m {
            println!("  recall_at_5[{lang}] {}", fmt(v, 2));
        }
    }
}

fn check_thresholds(args: &Args, variants: &[Value]) -> Result<()> {
    let limits = [
        ("recall_at_5", args.min_recall, true),
        ("warm_p95_ms", args.max_warm_p95_ms, false),
        ("cold_p95_ms", args.max_cold_p95_ms, false),
        ("peak_rss_mb", args.max_rss_mb, false),
        ("peak_rss_mb", args.max_client_rss_mb, false),
        ("serve.first_call_ms", args.max_first_call_ms, false),
        ("serve.warm_p95_ms", args.max_serve_warm_p95_ms, false),
        ("serve.server_rss_mb", args.max_server_rss_mb, false),
    ];
    for v in variants.iter().filter(|v| v["kind"] == "fbrain") {
        for (metric, limit, is_min) in limits {
            let Some(limit) = limit else { continue };
            let found = metric.split('.').fold(v, |at, key| &at[key]);
            let Some(val) = found.as_f64() else {
                bail!("threshold failed: {metric} was not measured, so limit {limit:.2} cannot be checked");
            };
            if (is_min && val < limit) || (!is_min && val > limit) {
                bail!(
                    "threshold failed: {metric} {val:.2} {} {limit:.2}",
                    if is_min { "<" } else { ">" }
                );
            }
        }
    }
    Ok(())
}

fn run(args: Args) -> Result<()> {
    let qs = load_queries(&args.queries)?;
    let mut specs: Vec<Variant> = args
        .variants
        .iter()
        .map(|s| parse_variant(s))
        .collect::<Result<_>>()?;
    if specs.is_empty() {
        specs.push(Variant {
            name: "default".into(),
            ..Default::default()
        });
    }
    let mut variants = Vec::new();
    for v in &specs {
        variants.push(run_fbrain(&args, &qs, v)?);
    }
    if args.baseline_om {
        variants.push(run_om(&args, &qs)?);
    }
    if args.baseline_qmd {
        variants.push(run_qmd(&args, &qs)?);
    }
    let model = if args.measure_model {
        Some(measure_model(&args, &qs)?)
    } else {
        None
    };
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let report = json!({"load_average": load.trim(), "variants": variants, "model": model});
    for v in &variants {
        print_variant(v);
    }
    if let Some(rows) = &model {
        for r in rows.as_array().into_iter().flatten() {
            println!(
                "model {}: stored_mb {} load_ms {} search_ms {} peak_rss_mb {}",
                r["model"].as_str().unwrap_or_default(),
                fmt(&r["stored_mb"], 1),
                fmt(&r["load_ms"], 0),
                fmt(&r["search_ms"], 0),
                fmt(&r["peak_rss_mb"], 1)
            );
        }
    }
    if let Some(dir) = &args.out {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
    }
    check_thresholds(&args, &variants)
}

fn main() {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    if argv.first().is_some_and(|a| a == "--rss-child") && argv.len() > 1 {
        rss_child(&argv[1..]);
    }
    if let Err(e) = run(Args::parse()) {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(extra: &[&str]) -> Args {
        let mut v = vec!["fbrain-bench", "--brain", "b", "--queries", "q"];
        v.extend(extra);
        Args::parse_from(v)
    }

    #[test]
    fn a_metric_that_was_not_measured_fails_its_active_limit() {
        let v = json!({"kind": "fbrain", "recall_at_5": 1.0, "peak_rss_mb": null});
        let e = check_thresholds(&args(&["--max-rss-mb", "100"]), std::slice::from_ref(&v))
            .unwrap_err();
        assert!(
            e.to_string().contains("peak_rss_mb was not measured"),
            "{e}"
        );
        assert!(check_thresholds(&args(&[]), std::slice::from_ref(&v)).is_ok());
    }

    #[test]
    fn the_resident_process_limits_check_the_serve_numbers() {
        let v = json!({"kind": "fbrain", "peak_rss_mb": 20.0, "serve": {
            "first_call_ms": 1500.0, "warm_p95_ms": 40.0, "server_rss_mb": 600.0}});
        let ok = args(&[
            "--max-first-call-ms",
            "2000",
            "--max-serve-warm-p95-ms",
            "50",
            "--max-server-rss-mb",
            "800",
            "--max-client-rss-mb",
            "50",
        ]);
        assert!(check_thresholds(&ok, std::slice::from_ref(&v)).is_ok());
        let e = check_thresholds(
            &args(&["--max-first-call-ms", "1000"]),
            std::slice::from_ref(&v),
        )
        .unwrap_err();
        assert!(
            e.to_string()
                .contains("serve.first_call_ms 1500.00 > 1000.00"),
            "{e}"
        );
        let e = check_thresholds(
            &args(&["--max-client-rss-mb", "10"]),
            std::slice::from_ref(&v),
        )
        .unwrap_err();
        assert!(e.to_string().contains("peak_rss_mb 20.00 > 10.00"), "{e}");
    }
}
