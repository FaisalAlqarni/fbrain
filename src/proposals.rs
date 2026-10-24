//! Proposals: agents and the user ask for a change by writing a file to `inbox/`. Nothing
//! outside `inbox/` is written here.

use crate::brain::Brain;
use crate::config;
use crate::index::{embed, text, Index};
use crate::note::{split_frontmatter, FmValue, Note};
use crate::search::{expose, exposed, Surface};
use crate::serve::EmbedKind;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

const INBOX: &str = "inbox";
const MAX_CONFLICTS: usize = 3;
const EMBED_CHARS: usize = 500;
const BAD_PATH: &str = "must be a relative path inside the brain";

/// The request was wrong, not the machine; `main` exits with code 2.
#[derive(Debug)]
pub struct Invalid(pub String);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Invalid {}

fn invalid<T>(msg: String) -> Result<T> {
    Err(Invalid(msg).into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Add,
    Update,
    Supersede,
    Invalidate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalInput {
    pub op: Op,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(skip_serializing, default)]
    pub body: String,
    pub proposed_by: String,
    pub source: String,
    pub rationale: String,
    /// Extra frontmatter keys of the proposal file (`map_repo`); review reads the ones it knows.
    #[serde(skip)]
    pub extra: Vec<(String, FmValue)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    #[serde(rename = "path")]
    pub rel_path: String,
    pub score: f32,
    pub protected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    pub id: String,
    pub rel_path: String,
    #[serde(flatten)]
    pub input: ProposalInput,
    pub conflicts: Vec<Conflict>,
    pub conflicts_protected: bool,
}

/// The proposal file's frontmatter; the body follows it.
#[derive(Serialize, Deserialize)]
struct FileFm {
    #[serde(flatten)]
    input: ProposalInput,
    conflicts: Vec<Conflict>,
    conflicts_protected: bool,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_norway::Value>,
}

const RESERVED: [&str; 9] = [
    "op",
    "target",
    "supersedes",
    "body",
    "proposed_by",
    "source",
    "rationale",
    "conflicts",
    "conflicts_protected",
];

fn to_yaml(v: &FmValue) -> serde_norway::Value {
    use serde_norway::Value;
    match v {
        FmValue::Str(s) => Value::String(s.clone()),
        FmValue::Bool(b) => Value::Bool(*b),
        FmValue::List(l) => Value::Sequence(l.iter().cloned().map(Value::String).collect()),
    }
}

/// The value kinds `FmValue` holds; any other YAML value in an extra key is dropped.
fn from_yaml(v: &serde_norway::Value) -> Option<FmValue> {
    use serde_norway::Value;
    match v {
        Value::String(s) => Some(FmValue::Str(s.clone())),
        Value::Bool(b) => Some(FmValue::Bool(*b)),
        Value::Sequence(l) => l
            .iter()
            .map(|x| x.as_str().map(String::from))
            .collect::<Option<_>>()
            .map(FmValue::List),
        _ => None,
    }
}

/// A path inside the brain as `a/b/c.md`: no absolute paths, drive letters, backslashes or `..`.
fn normalize(raw: &str, what: &str) -> Result<String> {
    let bad = || invalid(format!("invalid {what}: {BAD_PATH}"));
    if raw.starts_with('/') || raw.contains(['\\', '\0']) {
        return bad();
    }
    let mut parts = Vec::new();
    for p in raw.split('/').filter(|p| !p.is_empty() && *p != ".") {
        if p == ".." || p.contains(':') {
            return bad();
        }
        parts.push(p);
    }
    if parts.is_empty() {
        return bad();
    }
    if parts[0].eq_ignore_ascii_case(INBOX) {
        return invalid(format!("invalid {what}: inbox is reserved for proposals"));
    }
    if parts.iter().any(|p| p.starts_with('.')) || !parts[parts.len() - 1].ends_with(".md") {
        return invalid(format!("invalid {what}: must be a note (.md, not hidden)"));
    }
    Ok(parts.join("/"))
}

/// `normalize` plus the brain's `include` roots, the same set `Brain::notes` walks.
fn note_path(brain: &Brain, raw: &str, what: &str) -> Result<String> {
    let rel = normalize(raw, what)?;
    let roots = &brain.config.include;
    let in_roots = roots.is_empty()
        || roots
            .iter()
            .any(|r| rel.starts_with(&format!("{}/", r.trim_matches('/'))));
    if !in_roots {
        return invalid(format!("invalid {what}: {BAD_PATH}"));
    }
    Ok(rel)
}

/// A note path that is safe to read or write: the `normalize` and `include` rules plus a check
/// that no symlink on the way leads outside the brain or into `inbox/`.
pub(crate) fn note_target(brain: &Brain, raw: &str, what: &str) -> Result<String> {
    let rel = note_path(brain, raw, what)?;
    check_resolved(&brain.path.canonicalize()?, &rel, what)?;
    Ok(rel)
}

/// Rejects a path whose real location (through symlinks) is outside the brain or inside `inbox/`.
fn check_resolved(root: &Path, rel: &str, what: &str) -> Result<()> {
    let mut p = root.join(rel);
    let real = loop {
        // symlink_metadata sees a dangling link, which exists() would not.
        if p.symlink_metadata().is_ok() {
            match p.canonicalize() {
                Ok(c) => break c,
                Err(_) => return invalid(format!("invalid {what}: {BAD_PATH}")),
            }
        }
        if !p.pop() {
            return invalid(format!("invalid {what}: {BAD_PATH}"));
        }
    };
    match real.strip_prefix(root) {
        Err(_) => invalid(format!("invalid {what}: {BAD_PATH}")),
        Ok(r) if r.starts_with(INBOX) => {
            invalid(format!("invalid {what}: inbox is reserved for proposals"))
        }
        Ok(_) => Ok(()),
    }
}

/// Checks the input against the brain and returns it with `target` and `supersedes` normalised.
pub fn check(brain: &Brain, mut input: ProposalInput) -> Result<ProposalInput> {
    let root = brain.path.canonicalize()?;
    input.target = note_path(brain, &input.target, "target")?;
    check_resolved(&root, &input.target, "target")?;
    let exists = root.join(&input.target).is_file();
    match input.op {
        Op::Add if exists => {
            return invalid(format!(
                "target exists: {} (use --op update or supersede)",
                input.target
            ))
        }
        Op::Update | Op::Invalidate if !exists => {
            return invalid(format!("target not found: {}", input.target))
        }
        _ => {}
    }
    input.supersedes = match (input.op, input.supersedes.take()) {
        (Op::Supersede, None) => {
            return invalid("--supersedes is required for op supersede".into())
        }
        (_, None) => None,
        (op, Some(_)) if op != Op::Supersede => {
            return invalid("--supersedes is only valid for op supersede".into())
        }
        (_, Some(s)) => {
            let raw = s.trim().trim_start_matches("[[").trim_end_matches("]]");
            let rel = note_path(brain, raw, "supersedes")?;
            check_resolved(&root, &rel, "supersedes")?;
            if !root.join(&rel).is_file() {
                return invalid(format!("supersedes not found: {rel}"));
            }
            Some(rel)
        }
    };
    Ok(input)
}

/// UTC `yyyymmddThhmmssZ`; `FBRAIN_TEST_PROPOSAL_TS` replaces it so tests can pin the second.
fn timestamp() -> String {
    std::env::var("FBRAIN_TEST_PROPOSAL_TS").unwrap_or_else(|_| now_utc())
}

/// The current UTC time as `yyyymmddThhmmssZ`.
pub(crate) fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn slug(target: &str) -> String {
    let s: String = crate::note::stem(target)
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                '-'
            }
        })
        .collect();
    let s = s
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    match s.chars().take(60).collect::<String>() {
        s if s.is_empty() => "note".into(),
        s => s,
    }
}

pub(crate) fn note_at(brain: &Brain, rel: &str) -> Result<Note> {
    let file = brain.path.join(rel);
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("cannot read {}", file.display()))?;
    Ok(Note::parse(rel, &text))
}

fn conflict(brain: &Brain, rel: String, score: f32) -> Result<Conflict> {
    Ok(Conflict {
        protected: note_at(brain, &rel)?.is_protected(),
        score: (score * 10000.0).round() / 10000.0,
        rel_path: rel,
    })
}

fn is_candidate(rel: &str, target: &str) -> bool {
    rel != target && !rel.starts_with("inbox/")
}

/// Embeds the proposal text: the vector, or why semantic matching is off.
pub type EmbedText<'a> = &'a mut dyn FnMut(&str) -> Result<std::result::Result<Vec<f32>, String>>;

/// The CLI's embedder: the resident process, started when needed.
fn embed_via_server(
    brain: &Brain,
) -> impl FnMut(&str) -> Result<std::result::Result<Vec<f32>, String>> + '_ {
    move |text| {
        Ok(crate::cli::through_server(brain, |c| {
            c.embed(&[text.to_string()], EmbedKind::Document)
        })?
        .map(|mut v| v.remove(0)))
    }
}

/// Notes that restate the proposal: raw cosine at or above `conflict_threshold` through the
/// resident process, else BM25 candidates filtered by `conflict_overlap_min` with the notice.
/// Notes the surface may not see are never candidates.
fn conflicts(
    brain: &Brain,
    index: &Index,
    target: &str,
    body: &str,
    surface: Surface,
    embed_text: EmbedText,
) -> Result<Vec<Conflict>> {
    let snippet: String = body.chars().take(EMBED_CHARS).collect();
    let visible = |rel: &str| index.file(rel).is_some_and(|f| exposed(f, surface));
    let why = match embed_text(&snippet)? {
        Ok(vec) if index.dim() > 0 => {
            let top = embed::cosine_top(index, &vec, usize::MAX, Some(target))?;
            let min = config::conflict_threshold(Some(brain));
            return top
                .into_iter()
                .filter(|(rel, s)| is_candidate(rel, target) && visible(rel) && *s >= min)
                .take(MAX_CONFLICTS)
                .map(|(rel, s)| conflict(brain, rel, s))
                .collect();
        }
        Ok(_) => "index has no embeddings, run fbrain reindex".to_string(),
        Err(why) => why,
    };
    eprintln!("semantic search off: {why}");
    keyword_conflicts(brain, index, target, &snippet, surface)
}

fn keyword_conflicts(
    brain: &Brain,
    index: &Index,
    target: &str,
    snippet: &str,
    surface: Surface,
) -> Result<Vec<Conflict>> {
    let terms: BTreeSet<String> = text::tokenize(snippet).into_iter().collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let list: Vec<String> = terms.iter().cloned().collect();
    let mut best: BTreeMap<&str, f32> = BTreeMap::new();
    for s in index.scores(&list) {
        let e = best.entry(s.rel_path).or_insert(f32::MIN);
        *e = e.max(s.score);
    }
    let mut ranked: Vec<_> = best
        .into_iter()
        .filter(|(r, _)| {
            is_candidate(r, target) && index.file(r).is_some_and(|f| exposed(f, surface))
        })
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let min = config::conflict_overlap_min(Some(brain));
    let mut out = Vec::new();
    for (rel, _) in ranked.into_iter().take(MAX_CONFLICTS) {
        let Some(f) = index.file(rel) else { continue };
        let have: BTreeSet<String> = f
            .chunks
            .iter()
            .flat_map(|c| text::tokenize(&c.text))
            .collect();
        let overlap =
            terms.iter().filter(|t| have.contains(*t)).count() as f32 / terms.len() as f32;
        if overlap >= min {
            out.push(conflict(brain, rel.to_string(), overlap)?);
        }
    }
    Ok(out)
}

pub fn propose(
    brain: &Brain,
    index: &Index,
    input: ProposalInput,
    surface: Surface,
) -> Result<Proposal> {
    propose_with(brain, index, input, surface, &mut embed_via_server(brain))
}

/// `propose` with the caller's way to embed the body, for a client that manages its own
/// connection to the resident process.
pub fn propose_with(
    brain: &Brain,
    index: &Index,
    input: ProposalInput,
    surface: Surface,
    embed_text: EmbedText,
) -> Result<Proposal> {
    // Before `check`, whose "target exists" answer would confirm a note the surface may not see.
    for (raw, what) in [
        (Some(input.target.as_str()), "target"),
        (input.supersedes.as_deref(), "supersedes"),
    ] {
        let Some(raw) = raw else { continue };
        let raw = raw.trim().trim_start_matches("[[").trim_end_matches("]]");
        let Ok(rel) = note_path(brain, raw, what) else {
            continue;
        };
        if brain.path.join(&rel).is_file() && !expose(&note_at(brain, &rel)?, surface) {
            return invalid(format!("{what} not found: {rel}"));
        }
    }
    let input = check(brain, input)?;
    let conflicts = conflicts(
        brain,
        index,
        &input.target,
        &input.body,
        surface,
        embed_text,
    )?;
    let target_protected = {
        let f = brain.path.join(&input.target);
        f.is_file() && note_at(brain, &input.target)?.is_protected()
    };
    let superseded_protected = match &input.supersedes {
        Some(rel) => note_at(brain, rel)?.is_protected(),
        None => false,
    };
    let conflicts_protected =
        target_protected || superseded_protected || conflicts.iter().any(|c| c.protected);

    let root = brain.path.canonicalize()?;
    let dir = brain.path.join(INBOX);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    if !dir.canonicalize()?.starts_with(&root) {
        return invalid(format!("invalid inbox: {BAD_PATH}"));
    }
    if let Some((k, _)) = input
        .extra
        .iter()
        .find(|(k, _)| RESERVED.contains(&k.as_str()))
    {
        return invalid(format!("invalid extra key: {k} is a proposal field"));
    }
    let extra = input
        .extra
        .iter()
        .map(|(k, v)| (k.clone(), to_yaml(v)))
        .collect();
    let fm = FileFm {
        input,
        conflicts,
        conflicts_protected,
        extra,
    };
    let content = format!(
        "---\n{}---\n{}",
        serde_norway::to_string(&fm)?,
        fm.input.body
    );
    let base = format!("{}-{}", timestamp(), slug(&fm.input.target));
    for n in 1.. {
        let id = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        let file = dir.join(format!("{id}.md"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file)
        {
            Ok(mut f) => {
                f.write_all(content.as_bytes())
                    .with_context(|| format!("cannot write {}", file.display()))?;
                return Ok(Proposal {
                    rel_path: format!("{INBOX}/{id}.md"),
                    id,
                    input: fm.input,
                    conflicts: fm.conflicts,
                    conflicts_protected,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", file.display())),
        }
    }
    unreachable!("the loop only ends by returning")
}

/// An inbox file that is not a proposal: `(file name, reason)`.
pub type Skipped = (String, String);

/// Pending proposals, oldest first, and the inbox files that could not be read as proposals
/// (a bad name or bad content), as `(file name, reason)`. Nothing is printed.
pub fn list_with_skipped(brain: &Brain) -> Result<(Vec<Proposal>, Vec<Skipped>)> {
    let dir = brain.path.join(INBOX);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    };
    let (mut out, mut skipped) = (Vec::new(), Vec::new());
    for e in entries {
        let path = e?.path();
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".md").map(str::to_string) else {
            continue;
        };
        if id.chars().any(char::is_control) {
            skipped.push((name, "the file name has control characters".into()));
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                skipped.push((name, format!("cannot read: {e}")));
                continue;
            }
        };
        match parse(id, &raw) {
            Ok(p) => out.push(p),
            Err(e) => skipped.push((name, e)),
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    skipped.sort();
    Ok((out, skipped))
}

/// Prints one line per skipped inbox file and a count; the names are escaped.
pub fn report_skipped(skipped: &[Skipped]) {
    if skipped.is_empty() {
        return;
    }
    for (name, why) in skipped {
        eprintln!(
            "skipping {INBOX}/{}: {}",
            crate::review::escape(name),
            crate::review::escape(why)
        );
    }
    eprintln!(
        "{} malformed inbox file(s) skipped; fbrain review <id> --reject deletes one",
        skipped.len()
    );
}

/// Pending proposals, oldest first. A file that is not a proposal is reported and skipped.
pub fn list(brain: &Brain) -> Result<Vec<Proposal>> {
    let (out, skipped) = list_with_skipped(brain)?;
    report_skipped(&skipped);
    Ok(out)
}

/// Whether `inbox/<id>.md` exists as a file, readable as a proposal or not.
pub fn inbox_file_exists(brain: &Brain, id: &str) -> bool {
    valid_id(id) && brain.path.join(INBOX).join(format!("{id}.md")).is_file()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && !id.contains(['/', '\\', ':'])
        && !id.chars().any(char::is_control)
}

fn parse(id: String, raw: &str) -> std::result::Result<Proposal, String> {
    let (yaml, body) = split_frontmatter(raw);
    let mut fm: FileFm = serde_norway::from_str(&yaml.ok_or("no proposal frontmatter")?)
        .map_err(|e| e.to_string())?;
    fm.input.body = body;
    fm.input.extra = fm
        .extra
        .iter()
        .filter_map(|(k, v)| Some((k.clone(), from_yaml(v)?)))
        .collect();
    Ok(Proposal {
        rel_path: format!("{INBOX}/{id}.md"),
        id,
        input: fm.input,
        conflicts: fm.conflicts,
        conflicts_protected: fm.conflicts_protected,
    })
}

/// One pending proposal by id; `None` when there is no such file. The id is a bare file name.
pub fn get(brain: &Brain, id: &str) -> Result<Option<Proposal>> {
    if !valid_id(id) {
        return Ok(None);
    }
    let path = brain.path.join(INBOX).join(format!("{id}.md"));
    match std::fs::read_to_string(&path) {
        Ok(raw) => parse(id.to_string(), &raw)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("invalid proposal {id}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}
