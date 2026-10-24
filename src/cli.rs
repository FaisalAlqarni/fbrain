//! The command surface. Each subcommand calls into the library modules.

use crate::brain::{self, Brain};
use crate::config::{self, UserConfig};
use crate::doctor::{self, Level};
use crate::index::embed::{self, Embedder};
use crate::index::{text, Index};
use crate::links::Resolution;
use crate::lint;
use crate::note::{self, FmValue, Note, Status};
use crate::proposals::{self, Op, ProposalInput};
use crate::review::{self, Decision};
use crate::search::{self, Filter, Query};
use crate::serve;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::io::IsTerminal;
use std::path::PathBuf;

/// The brain was created but the model download failed; `main` exits with code 3.
#[derive(Debug)]
pub struct ModelNotDownloaded(pub String);

impl std::fmt::Display for ModelNotDownloaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "warning: model not downloaded: {}; run fbrain doctor --fix",
            self.0
        )
    }
}

impl std::error::Error for ModelNotDownloaded {}

/// A review or frontmatter edit was run by an agent or without a terminal; `main` exits with code 3.
#[derive(Debug)]
pub struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing non-interactive review: proposals are approved by Faisal"
        )
    }
}

impl std::error::Error for Refused {}

/// The agent running this command, if it says so: `FBRAIN_HARNESS` (exported by the harness
/// installs), else the marker Claude Code sets itself in the subprocesses of its Bash tool.
/// `CLAUDECODE` is not used: IDE extensions also set it in the user's own terminals.
/// <https://code.claude.com/docs/en/env-vars> (CLAUDE_CODE_CHILD_SESSION)
pub(crate) fn agent_name() -> Option<String> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    var("FBRAIN_HARNESS")
        .or_else(|| (var("CLAUDE_CODE_CHILD_SESSION")? == "1").then(|| "claude-code".into()))
}

/// Faisal at a terminal: stdin is a terminal and no agent marker is set. Every owner-only path
/// asks this one question. It is a convention, not a boundary: an agent with a shell can unset
/// its markers or pass `--yes-i-am-faisal`.
pub(crate) fn owner() -> bool {
    agent_name().is_none() && std::io::stdin().is_terminal()
}

/// What Faisal's own commands (`propose`, `onboard`, `interview`) may read: everything when
/// `owner()`, else what agents may read.
pub(crate) fn owner_surface() -> search::Surface {
    search::cli_surface(owner())
}

/// `--private` was passed by something that is not the owner. The message does not say whether
/// any note exists.
#[derive(Debug)]
pub struct PrivateRefused;

impl std::fmt::Display for PrivateRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing --private: only Faisal at a terminal may use it"
        )
    }
}

impl std::error::Error for PrivateRefused {}

/// `--private` asks for hidden notes; only the owner gets them.
fn private_surface(requested: bool) -> Result<search::Surface> {
    if requested && !owner() {
        return Err(PrivateRefused.into());
    }
    Ok(search::cli_surface(requested))
}

/// Only a person at a terminal changes the brain, unless `--yes-i-am-faisal` says otherwise.
fn guard(confirmed: bool) -> Result<()> {
    if confirmed || owner() {
        Ok(())
    } else {
        Err(Refused.into())
    }
}

#[derive(Parser)]
#[command(name = "fbrain", version, about = "Markdown brain for coding agents")]
pub struct Cli {
    /// Brain name or path (overrides FBRAIN_BRAIN, repo mapping and the default)
    #[arg(long, global = true)]
    pub brain: Option<String>,
    /// Print JSON
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a brain, or register an existing folder with --adopt
    Init {
        /// Folder for the new brain
        #[arg(required_unless_present = "adopt", conflicts_with = "adopt")]
        path: Option<PathBuf>,
        /// Register an existing folder without moving or editing its files
        #[arg(long, value_name = "PATH")]
        adopt: Option<PathBuf>,
        /// Brain name (defaults to the folder name)
        #[arg(long)]
        name: Option<String>,
        /// With --adopt: only walk notes under this folder (repeatable)
        #[arg(long, requires = "adopt")]
        include: Vec<String>,
        /// Skip the embedding model download
        #[arg(long)]
        no_model: bool,
        /// Replace the path of an already registered brain name
        #[arg(long)]
        force: bool,
    },
    /// List registered brains, or print the active one with --which
    Brains {
        #[arg(long)]
        which: bool,
    },
    /// Print a note by name or relative path
    Get {
        note: String,
        /// Also show the note's outgoing links and backlinks
        #[arg(long)]
        links: bool,
        /// Include private notes (owner only)
        #[arg(long, hide = true)]
        private: bool,
    },
    /// Search notes by keyword (BM25), current facts only unless told otherwise
    Search {
        query: String,
        /// Maximum number of notes
        #[arg(short, default_value_t = 10)]
        k: usize,
        /// Include superseded, invalidated and expired notes
        #[arg(long)]
        history: bool,
        /// Judge validity as of this date (YYYY-MM-DD) instead of today
        #[arg(long, value_name = "DATE")]
        as_of: Option<String>,
        /// Keep only these statuses (repeatable); superseded and invalidated also need --history
        #[arg(long, value_parser = parse_status)]
        status: Vec<Status>,
        /// With --json: also print the chunks of every hit note
        #[arg(long)]
        chunks: bool,
        /// Token budget for the results (default: token_budget in .fbrain.toml, else 4000)
        #[arg(long)]
        budget: Option<usize>,
        /// Include private notes (owner only)
        #[arg(long, hide = true)]
        private: bool,
    },
    /// Add fbrain's session hooks to a harness (claude-code, codex, opencode-v1 or opencode-v2)
    Install {
        /// claude-code, codex, opencode-v1 or opencode-v2
        harness: String,
        /// opencode only: also add a marked fbrain block to the global AGENTS.md (for when plugins are off)
        #[arg(long)]
        agents_md: bool,
        /// Also register `fbrain mcp` as an MCP server with the harness
        #[arg(long)]
        mcp: bool,
    },
    /// Serve search, get, propose and health to an agent over MCP on stdin and stdout
    Mcp,
    /// Remove exactly what install wrote for a harness
    Uninstall {
        /// claude-code, codex, opencode-v1 or opencode-v2
        harness: String,
    },
    /// Check the model, index, hooks and a timed search; --fix repairs what it can
    Doctor {
        #[arg(long)]
        fix: bool,
    },
    /// Refresh the search index
    Reindex {
        /// Re-read every note
        #[arg(long)]
        full: bool,
        /// Print how many notes changed, stayed and were removed
        #[arg(long)]
        stats: bool,
    },
    /// Propose a change: writes a file to inbox/ and lists notes that may conflict; the body is read from stdin
    Propose {
        #[arg(long, value_enum)]
        op: Op,
        /// Note path inside the brain (the new path for add)
        #[arg(long)]
        target: String,
        /// With --op supersede: the note being replaced
        #[arg(long)]
        supersedes: Option<String>,
        #[arg(long)]
        rationale: String,
        #[arg(long)]
        source: String,
        /// Who proposes (default: the agent named by $FBRAIN_HARNESS or its own marker, else cli)
        #[arg(long = "by")]
        by: Option<String>,
    },
    /// Scan repos, ask what the scan could not answer, and write ME.md and one BRIEF.md per repo as proposals
    Onboard {
        /// A repo to scan (repeatable)
        #[arg(long = "repo", value_name = "PATH")]
        repos: Vec<PathBuf>,
        /// Answers as TOML (ME answers at the top, `[projects.<name>]` tables per repo); without it, ask in the terminal
        #[arg(long, value_name = "FILE")]
        answers: Option<PathBuf>,
        /// Print the scan and the questions that are still open, then stop (use with --json)
        #[arg(long)]
        print_questions: bool,
    },
    /// Interview about one project, or with --review about what has gone stale; answers become proposals
    #[command(group(clap::ArgGroup::new("mode").required(true).args(["project", "review"])))]
    Interview {
        /// The project to interview; a missing brief is proposed, an existing one gets its empty sections filled
        project: Option<String>,
        /// Walk stale notes, expired facts, changed sources, old status notes, open questions and goals
        #[arg(long, conflicts_with_all = ["repo", "print_questions"])]
        review: bool,
        /// The project's repo, scanned for prefills and mapped to the project (default: the mapped repo)
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
        /// Answers as TOML: `[projects.<name>]` with the question ids; with --review, `"<item id>" = "<action>"` (keep, stale, update TEXT, supersede TEXT, answer TEXT); without it, ask in the terminal
        #[arg(long, value_name = "FILE")]
        answers: Option<PathBuf>,
        /// Print the open questions, then stop (use with --json)
        #[arg(long)]
        print_questions: bool,
        /// With --review: print the items, then stop (use with --json)
        #[arg(long, requires = "review")]
        print_items: bool,
        /// The date to measure age against and to stamp the proposal source (default: today)
        #[arg(long, value_name = "YYYY-MM-DD")]
        today: Option<String>,
    },
    /// List pending proposals, oldest first
    Inbox,
    /// Review pending proposals, oldest first; with an id and one decision flag, decide that one
    #[command(group(clap::ArgGroup::new("decision").args(["approve", "edit", "merge_into", "supersede_candidate", "reject"])))]
    Review {
        /// The proposal to decide (see `fbrain inbox`); without it, review interactively
        #[arg(requires = "decision")]
        id: Option<String>,
        /// Apply the proposal
        #[arg(long)]
        approve: bool,
        /// Edit the body in $VISUAL or $EDITOR, then apply it
        #[arg(long)]
        edit: bool,
        /// Append the proposal body to this note instead of writing its target
        #[arg(long, value_name = "NOTE")]
        merge_into: Option<String>,
        /// Apply the proposal, then mark this note superseded by its target
        #[arg(long, value_name = "NOTE")]
        supersede_candidate: Option<String>,
        /// Delete the proposal without applying it
        #[arg(long)]
        reject: bool,
        /// Allow a proposal that touches a protected note
        #[arg(long)]
        force: bool,
        /// Run without a terminal or from an agent session; only Faisal passes this
        #[arg(long, hide = true)]
        yes_i_am_faisal: bool,
    },
    /// Print the session payload for a harness hook; always exits 0
    Brief {
        /// claude-code, codex, opencode-v1 or opencode-v2
        #[arg(long)]
        harness: String,
        /// Working directory to map to a project (default: the hook's stdin `cwd`, else the current directory)
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Check the brain's health; --fix applies the mechanical fixes (L008, L010, L011)
    Lint {
        /// Add a missing valid_to, regenerate INDEX.md, rewrite alias-only links to path form
        #[arg(long)]
        fix: bool,
        /// Exit 1 when issues outside the `[lint] allow` list remain
        #[arg(long)]
        strict: bool,
        /// With --fix: run without a terminal or from an agent session; only Faisal passes this
        #[arg(long, hide = true, requires = "fix")]
        yes_i_am_faisal: bool,
        /// Report findings about private notes too (owner only)
        #[arg(long, hide = true)]
        private: bool,
    },
    /// Set frontmatter keys of a note without touching anything else
    #[command(hide = true)]
    FmSet {
        /// Run without a terminal or from an agent session
        #[arg(long)]
        yes_i_am_faisal: bool,
        note: String,
        /// key=value; true and false are booleans, [a, b] is a list
        #[arg(required = true)]
        pairs: Vec<String>,
    },
    /// The resident process that holds the model; started by the commands that need it
    #[command(hide = true)]
    Serve,
}

fn parse_status(s: &str) -> Result<Status, String> {
    serde_json::from_value(s.into())
        .map_err(|_| format!("unknown status `{s}` (active, done, stale, superseded, invalidated)"))
}

/// Loads the model when it is downloaded and builds the index with it; a missing model
/// gives a keyword-only index and a notice on stderr.
fn open_index(brain: &Brain, full: bool) -> Result<(Index, Option<Embedder>)> {
    let cache = config::cache_dir()?;
    let id = config::model_id(Some(brain));
    let embedder = embed::load_if_present(&cache, &id)?;
    if embedder.is_none() {
        eprintln!("semantic search off: model not downloaded, run fbrain doctor --fix");
    }
    let index = Index::load_or_build(
        brain,
        &cache,
        full,
        embedder.as_ref().map(|e| (id.as_str(), e)),
    )?;
    Ok((index, embedder))
}

/// The resident process for this brain's model, or why semantic search is off. An endpoint
/// that is not ours, or that refuses our token, fails the command instead.
fn semantic(brain: &Brain) -> Result<std::result::Result<serve::Client, String>> {
    let id = config::model_id(Some(brain));
    if !embed::is_downloaded(&config::cache_dir()?, &id)? {
        return Ok(Err("model not downloaded, run fbrain doctor --fix".into()));
    }
    match serve::Client::connect_or_start(&id) {
        Ok(c) => Ok(Ok(c)),
        Err(e) if serve::is_untrusted(&e) => Err(e),
        Err(e) => Ok(Err(format!("resident process unavailable: {e:#}"))),
    }
}

/// Runs `f` on the resident process; a failed call becomes the reason semantic search is off,
/// except an untrusted endpoint, which fails the command.
pub(crate) fn through_server<T>(
    brain: &Brain,
    f: impl FnOnce(&mut serve::Client) -> Result<T>,
) -> Result<std::result::Result<T, String>> {
    match semantic(brain)? {
        Ok(mut c) => match f(&mut c) {
            Ok(v) => Ok(Ok(v)),
            Err(e) if serve::is_untrusted(&e) => Err(e),
            Err(e) => Ok(Err(format!("resident process failed: {e:#}"))),
        },
        Err(why) => Ok(Err(why)),
    }
}

/// The index `propose` matches against. The resident process refreshes the embedded cache;
/// without it the index is keyword-only, and the notice is printed.
pub(crate) fn propose_index(brain: &Brain) -> Result<Index> {
    let cache = config::cache_dir()?;
    let embedded = match through_server(brain, |c| c.reindex(&brain.path, false))? {
        Ok(_) => Index::load_embedded(brain, &cache, &config::model_id(Some(brain)))?,
        Err(_) => None,
    };
    match embedded {
        Some(i) => Ok(i),
        None => Index::load_or_build(brain, &cache, false, None),
    }
}

/// Keyword-only index built in this process, after the notice saying why.
fn keyword_index(brain: &Brain, full: bool, why: &str) -> Result<Index> {
    eprintln!("semantic search off: {why}");
    Index::load_or_build(brain, &config::cache_dir()?, full, None)
}

/// Every chunk of the hit notes, read from the notes themselves.
fn hit_chunks(brain: &Brain, hits: &[search::Hit]) -> Result<Vec<serde_json::Value>> {
    let mut out = Vec::new();
    for h in hits {
        let file = brain.path.join(&h.rel_path);
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        for c in crate::index::chunk::chunks(&Note::parse(&h.rel_path, &text)) {
            out.push(json!({"rel_path": c.rel_path, "heading_path": c.heading_path, "ord": c.ord}));
        }
    }
    Ok(out)
}

/// Outgoing links (one entry per distinct target text) and backlinks of a note.
pub(crate) fn link_info(
    index: &Index,
    rel: &str,
    surface: search::Surface,
) -> (Vec<serde_json::Value>, Vec<String>) {
    let graph = index.graph();
    let shown = |p: &str| index.file(p).is_none_or(|f| search::exposed(f, surface));
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for l in graph.links(rel) {
        if seen.contains(&l.target.as_str()) {
            continue;
        }
        seen.push(&l.target);
        let (resolved, via_alias, ambiguous) = match graph.resolve(&l.target, rel) {
            Resolution::Resolved(p) => (Some(p), false, false),
            Resolution::ViaAlias(p) => (Some(p), true, false),
            Resolution::Ambiguous(c) => (c.into_iter().next(), false, true),
            Resolution::Missing => (None, false, false),
        };
        // A link to a note the surface may not see is left out, target text and all.
        if resolved.as_deref().is_some_and(|p| !shown(p)) {
            continue;
        }
        out.push(json!({
            "target": l.target,
            "resolved": resolved,
            "via_alias": via_alias,
            "ambiguous": ambiguous,
        }));
    }
    let back = graph.backlinks(rel).iter().filter(|b| shown(b)).cloned();
    (out, back.collect())
}

fn parse_harness(s: &str) -> Result<crate::harness::Harness> {
    crate::harness::Harness::parse(s).with_context(|| {
        format!("unknown harness {s:?}: use claude-code, codex, opencode-v1 or opencode-v2")
    })
}

fn open_brain(cli_brain: Option<&str>) -> Result<Brain> {
    let cwd = std::env::current_dir()?;
    Brain::open(&config::resolve_brain(cli_brain, &cwd)?.path)
}

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init {
            path,
            adopt,
            name,
            include,
            no_model,
            force,
        } => {
            let target = adopt.clone().or(path).context("init needs a path")?;
            let name = name.unwrap_or_else(|| {
                target
                    .canonicalize()
                    .unwrap_or_else(|_| target.clone())
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
            if adopt.is_some() {
                brain::adopt(&target, &name, &include, force)?;
            } else {
                brain::init(&target, &name, force)?;
            }
            if !no_model {
                let b = Brain::open(&target)?;
                let id = config::model_id(Some(&b));
                if let Err(e) = embed::ensure_model(&id, &config::cache_dir()?, true) {
                    return Err(ModelNotDownloaded(format!("{e:#}")).into());
                }
            }
            Ok(())
        }
        Command::Doctor { fix } => {
            let checks = doctor::run(fix, cli.brain.as_deref())?;
            for c in &checks {
                println!("{}", c.line());
            }
            let failed: Vec<_> = checks
                .iter()
                .filter(|c| c.level == Level::Fail)
                .map(|c| format!("{}: {}", c.name, c.detail))
                .collect();
            if !failed.is_empty() {
                bail!("{}", failed.join("; "));
            }
            Ok(())
        }
        Command::Install {
            harness,
            agents_md,
            mcp,
        } => {
            use crate::harness::{EntryKind, Harness};
            let h = parse_harness(&harness)?;
            if agents_md && !matches!(h, Harness::OpencodeV1 | Harness::OpencodeV2) {
                bail!("--agents-md applies to opencode-v1 and opencode-v2 only");
            }
            let opts = crate::harness::InstallOpts { agents_md, mcp };
            let rec = crate::harness::install(h, &opts)?;
            let mut files: Vec<_> = rec.files.iter().collect();
            files.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);
            for f in files {
                let path = f.path.display();
                match &f.kind {
                    EntryKind::File => println!("installed {} plugin in {path}", h.as_str()),
                    EntryKind::MarkedBlock(_) => {
                        println!("installed the fbrain block for {} in {path}", h.as_str())
                    }
                    EntryKind::McpCli => {
                        println!("registered the fbrain MCP server with the claude CLI")
                    }
                    k if crate::harness::is_mcp(f) => {
                        println!("registered the fbrain MCP server ({k:?}) in {path}")
                    }
                    _ => println!("installed {} hooks in {path}", h.as_str()),
                }
            }
            if h == crate::harness::Harness::Codex {
                println!("{}", crate::harness::codex::TRUST_REMINDER);
            }
            Ok(())
        }
        Command::Uninstall { harness } => crate::harness::uninstall(parse_harness(&harness)?),
        Command::Brains { which } => {
            if which {
                let cwd = std::env::current_dir()?;
                println!(
                    "{}",
                    config::resolve_brain(cli.brain.as_deref(), &cwd)?.name
                );
                return Ok(());
            }
            let cfg = UserConfig::load()?;
            if cli.json {
                println!(
                    "{}",
                    json!({ "default": cfg.default, "brains": cfg.brains })
                );
            } else {
                for b in &cfg.brains {
                    let tag = if cfg.default.as_deref() == Some(&b.name) {
                        " (default)"
                    } else {
                        ""
                    };
                    println!("{}{tag}  {}", b.name, b.path.display());
                }
            }
            Ok(())
        }
        Command::Brief { harness, cwd } => {
            println!("{}", crate::brief::run(cli.brain.as_deref(), &harness, cwd));
            Ok(())
        }
        Command::Lint {
            fix,
            strict,
            yes_i_am_faisal,
            private,
        } => {
            // A fix run passed the guard, so it sees everything it rewrites.
            let all = if fix {
                guard(yes_i_am_faisal)?;
                true
            } else {
                private_surface(private)? == search::Surface::Private
            };
            let cwd = std::env::current_dir()?;
            let brain = Brain::open(&config::resolve_brain(cli.brain.as_deref(), &cwd)?.path)?;
            let found = lint::run(&brain, fix, all)?;
            let allowed = |i: &lint::Issue| config::lint_allows(&brain, i.rule, &i.rel_path);
            if cli.json {
                let all: Vec<_> = found
                    .iter()
                    .map(|i| {
                        let mut v = serde_json::to_value(i).expect("an issue serializes");
                        v["allowed"] = json!(allowed(i));
                        v
                    })
                    .collect();
                println!("{}", json!(all));
            } else {
                for i in &found {
                    let tag = if allowed(i) { " (allowed)" } else { "" };
                    println!(
                        "{} {}: {}{tag}",
                        i.rule,
                        review::escape(&i.rel_path),
                        review::escape(&i.detail)
                    );
                }
            }
            let open = found.iter().filter(|i| !allowed(i)).count();
            if strict && open > 0 {
                bail!("{open} lint issue(s) not in the allowlist");
            }
            Ok(())
        }
        Command::Get {
            note,
            links,
            private,
        } => {
            let surface = private_surface(private)?;
            let cwd = std::env::current_dir()?;
            let brain_ref = config::resolve_brain(cli.brain.as_deref(), &cwd)?;
            let brain = Brain::open(&brain_ref.path)?;
            let Some(found) = brain.find(&note)? else {
                bail!("note not found: {note}");
            };
            let text = std::fs::read_to_string(&found.abs_path)
                .with_context(|| format!("cannot read {}", found.abs_path.display()))?;
            let n = Note::parse(&found.rel_path, &text);
            if !search::expose(&n, surface) {
                bail!("note not found: {note}");
            }
            let link_info = if links {
                let (index, _) = open_index(&brain, false)?;
                Some(link_info(&index, &n.rel_path, surface))
            } else {
                None
            };
            if cli.json {
                let mut v = serde_json::to_value(&n)?;
                v["status"] = serde_json::to_value(n.fm.status)?;
                if let Some((out, back)) = &link_info {
                    v["outgoing"] = json!(out);
                    v["backlinks"] = json!(back);
                }
                println!("{v}");
            } else {
                if let Some(err) = &n.frontmatter_error {
                    eprintln!("frontmatter error in {}: {err}", n.rel_path);
                }
                print!("{}", n.body);
                if let Some((out, back)) = &link_info {
                    println!("\nOutgoing links:");
                    for o in out {
                        let to = o["resolved"].as_str().unwrap_or("(missing)");
                        let flags = match (o["via_alias"] == true, o["ambiguous"] == true) {
                            (true, _) => " [via alias]",
                            (_, true) => " [ambiguous]",
                            _ => "",
                        };
                        println!(
                            "  [[{}]] -> {to}{flags}",
                            o["target"].as_str().unwrap_or("")
                        );
                    }
                    println!("Backlinks:");
                    for b in back {
                        println!("  {b}");
                    }
                }
            }
            Ok(())
        }
        Command::Search {
            query,
            k,
            history,
            as_of,
            status,
            chunks,
            budget,
            private,
        } => {
            let surface = private_surface(private)?;
            let mut brain = open_brain(cli.brain.as_deref())?;
            brain.path = brain.path.canonicalize().unwrap_or(brain.path);
            if text::tokenize(&query).is_empty() {
                eprintln!("no searchable terms in query");
            }
            let filter = Filter {
                history,
                as_of,
                statuses: (!status.is_empty()).then_some(status),
            };
            let mut q = Query::new(&query, filter);
            q.surface = surface;
            q.k = k;
            q.budget_tokens = budget.unwrap_or_else(|| config::token_budget(Some(&brain)));
            let (hits, off) = match through_server(&brain, |c| c.search(&brain.path, &q))? {
                Ok(h) => (h, None),
                Err(why) => (
                    search::search(&keyword_index(&brain, false, &why)?, &q, None)?,
                    Some(why),
                ),
            };
            if cli.json {
                let mut v = json!({ "hits": hits, "semantic": off.is_none() });
                if let Some(why) = &off {
                    v["semantic_reason"] = json!(why);
                }
                if chunks {
                    v["chunks"] = json!(hit_chunks(&brain, &hits)?);
                }
                println!("{v}");
            } else {
                for (i, h) in hits.iter().enumerate() {
                    println!("{}. {}  ({}, {:?})", i + 1, h.title, h.rel_path, h.status);
                    println!("   {}", h.snippet);
                    println!(
                        "   score {:.2}, bm25 rank {}, terms: {}",
                        h.score,
                        h.explain.bm25_rank.unwrap_or(0),
                        h.explain.matched_terms.join(", ")
                    );
                }
            }
            Ok(())
        }
        Command::Reindex { full, stats } => {
            let mut brain = open_brain(cli.brain.as_deref())?;
            brain.path = brain.path.canonicalize().unwrap_or(brain.path);
            let r = match through_server(&brain, |c| c.reindex(&brain.path, full))? {
                Ok(r) => r,
                Err(why) => serve::ReindexStats {
                    stats: keyword_index(&brain, full, &why)?.stats,
                    model: None,
                    dim: 0,
                },
            };
            let (s, embedded) = (r.stats, r.model.is_some());
            if stats && cli.json {
                println!(
                    "{}",
                    json!({
                        "changed": s.changed,
                        "unchanged": s.unchanged,
                        "removed": s.removed,
                        "embedded": s.embedded,
                        "model": r.model,
                        "dim": r.dim,
                    })
                );
            } else if stats {
                print!(
                    "changed {}, unchanged {}, removed {}",
                    s.changed, s.unchanged, s.removed
                );
                if embedded {
                    print!(", embedded {}", s.embedded);
                }
                println!();
            } else {
                println!("indexed {} notes", s.changed + s.unchanged);
            }
            Ok(())
        }
        Command::Propose {
            op,
            target,
            supersedes,
            rationale,
            source,
            by,
        } => {
            let mut brain = open_brain(cli.brain.as_deref())?;
            brain.path = brain.path.canonicalize().unwrap_or(brain.path);
            let mut body = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut body)
                .context("cannot read the proposal body from stdin")?;
            let input = proposals::check(
                &brain,
                ProposalInput {
                    op,
                    target,
                    supersedes,
                    body,
                    proposed_by: by.or_else(agent_name).unwrap_or_else(|| "cli".into()),
                    source,
                    rationale,
                    extra: Vec::new(),
                },
            )?;
            let index = propose_index(&brain)?;
            let p = proposals::propose(&brain, &index, input, owner_surface())?;
            if cli.json {
                println!("{}", serde_json::to_string(&p)?);
            } else {
                println!("proposed {}", p.rel_path);
                for c in &p.conflicts {
                    println!(
                        "  may conflict: {} (score {:.2}, protected {})",
                        c.rel_path, c.score, c.protected
                    );
                }
            }
            Ok(())
        }
        Command::Onboard {
            repos,
            answers,
            print_questions,
        } => {
            use crate::interview;
            let brain = open_brain(cli.brain.as_deref())?;
            let scans = interview::scan_all(&repos)?;
            let me = interview::questions_me(&scans, &brain);
            let per: Vec<_> = scans
                .iter()
                .map(|s| (s.name.clone(), interview::open_questions_project(&brain, s)))
                .collect();
            if print_questions {
                if cli.json {
                    let projects: Vec<_> = scans
                        .iter()
                        .zip(&per)
                        .map(|(s, (name, qs))| json!({"name": name, "scan": s, "questions": qs}))
                        .collect();
                    println!("{}", json!({"me": me, "projects": projects}));
                } else {
                    for q in &me {
                        println!("me.{}: {}", q.id, q.text);
                    }
                    for (name, qs) in &per {
                        for q in qs {
                            println!("{name}.{}: {}", q.id, q.text);
                        }
                    }
                }
                return Ok(());
            }
            let answers = match answers {
                Some(f) => interview::Answers::from_toml(
                    &std::fs::read_to_string(&f)
                        .with_context(|| format!("cannot read {}", f.display()))?,
                )?,
                None => interview::collect(&me, &per, &mut std::io::stdin().lock())?,
            };
            let made = interview::onboard_scans(&brain, &scans, &answers)?;
            if cli.json {
                println!("{}", serde_json::to_string(&made)?);
            } else {
                for p in &made {
                    println!("proposed {}", p.rel_path);
                }
            }
            Ok(())
        }
        Command::Interview {
            project,
            review,
            repo,
            answers,
            print_questions,
            print_items,
            today,
        } => {
            use crate::interview;
            let brain = open_brain(cli.brain.as_deref())?;
            let today = today.unwrap_or_else(interview::today);
            let read = |f: &PathBuf| {
                std::fs::read_to_string(f).with_context(|| format!("cannot read {}", f.display()))
            };
            let made = if review {
                let items = interview::review_items(&brain, &today)?;
                if print_items {
                    if cli.json {
                        println!("{}", serde_json::to_string(&items)?);
                    } else {
                        for i in &items {
                            println!("{}: {}", i.id, i.detail);
                        }
                    }
                    return Ok(());
                }
                let given = match answers {
                    Some(f) => interview::review_answers_from_toml(&read(&f)?)?,
                    None => interview::collect_review(&items, &mut std::io::stdin().lock())?,
                };
                interview::review_proposals(&brain, &items, &given, &today)?
            } else {
                let project = project.unwrap_or_default();
                let (scan, qs) = interview::project_questions(&brain, &project, repo.as_deref())?;
                if print_questions {
                    if cli.json {
                        println!(
                            "{}",
                            json!({"project": project, "scan": scan, "questions": qs})
                        );
                    } else {
                        for q in &qs {
                            println!("{}: {}", q.id, q.text);
                        }
                    }
                    return Ok(());
                }
                let given = match answers {
                    Some(f) => interview::Answers::from_toml(&read(&f)?)?,
                    None => interview::collect(
                        &[],
                        &[(project.clone(), qs)],
                        &mut std::io::stdin().lock(),
                    )?,
                };
                interview::interview_project(&brain, &project, repo.as_deref(), &given, &today)?
            };
            if cli.json {
                println!("{}", serde_json::to_string(&made)?);
            } else {
                for p in &made {
                    println!("proposed {}", p.rel_path);
                }
            }
            Ok(())
        }
        Command::Inbox => {
            let brain = open_brain(cli.brain.as_deref())?;
            let list = proposals::list(&brain)?;
            if cli.json {
                println!("{}", serde_json::to_string(&list)?);
            } else {
                for p in &list {
                    println!(
                        "{}  {} {}  ({} conflicts)",
                        review::escape(&p.id),
                        serde_json::to_value(p.input.op)?.as_str().unwrap_or(""),
                        review::escape(&p.input.target),
                        p.conflicts.len()
                    );
                }
            }
            Ok(())
        }
        Command::Review {
            id,
            approve,
            edit,
            merge_into,
            supersede_candidate,
            reject,
            force,
            yes_i_am_faisal,
        } => {
            guard(yes_i_am_faisal)?;
            let mut brain = open_brain(cli.brain.as_deref())?;
            brain.path = brain.path.canonicalize().unwrap_or(brain.path);
            let Some(id) = id else {
                return review::interactive(&brain);
            };
            let note_arg = |n: String| {
                if n.ends_with(".md") {
                    n
                } else {
                    format!("{n}.md")
                }
            };
            let decision = if edit {
                let Some(p) = proposals::get(&brain, &id)? else {
                    bail!("proposal not found: {id}");
                };
                let body = review::edit_body(&p.input.body)?;
                Decision::EditThenApprove { body, force }
            } else if let Some(n) = merge_into {
                Decision::MergeInto {
                    rel_path: note_arg(n),
                    force,
                }
            } else if let Some(n) = supersede_candidate {
                Decision::SupersedeCandidate {
                    rel_path: note_arg(n),
                    force,
                }
            } else if reject {
                Decision::Reject
            } else {
                debug_assert!(approve);
                Decision::Approve { force }
            };
            let applied = match review::apply(&brain, &id, decision) {
                Ok(a) => a,
                Err(e) => {
                    if let (true, Some(i)) = (cli.json, e.downcast_ref::<review::Incomplete>()) {
                        println!(
                            "{}",
                            json!({"error": i.detail, "commit": i.commit, "changed": i.changed})
                        );
                    }
                    return Err(e);
                }
            };
            if cli.json {
                println!(
                    "{}",
                    json!({"commit": applied.commit, "changed": applied.changed, "warning": applied.warning})
                );
            } else {
                match &applied.commit {
                    Some(sha) => println!(
                        "applied {} ({})",
                        review::escape(&id),
                        &sha[..sha.len().min(7)]
                    ),
                    None => println!("applied {}", review::escape(&id)),
                }
            }
            Ok(())
        }
        Command::FmSet {
            yes_i_am_faisal,
            note: name,
            pairs,
        } => {
            guard(yes_i_am_faisal)?;
            let brain = open_brain(cli.brain.as_deref())?;
            brain.git_unavailable()?;
            let Some(found) = brain.find(&name)? else {
                bail!("note not found: {name}");
            };
            let set: Vec<(String, FmValue)> = pairs
                .iter()
                .map(|p| {
                    let (k, v) = p
                        .split_once('=')
                        .with_context(|| format!("expected key=value: {p}"))?;
                    let v = match v {
                        "true" => FmValue::Bool(true),
                        "false" => FmValue::Bool(false),
                        _ => match v.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
                            Some(l) => FmValue::List(
                                l.split(',')
                                    .map(|s| s.trim().to_string())
                                    .filter(|s| !s.is_empty())
                                    .collect(),
                            ),
                            None => FmValue::Str(v.to_string()),
                        },
                    };
                    Ok((k.to_string(), v))
                })
                .collect::<Result<_>>()?;
            let meta = std::fs::symlink_metadata(&found.abs_path)?;
            if meta.file_type().is_symlink() {
                bail!("refusing to edit a symlink: {}", found.rel_path);
            }
            let text = std::fs::read_to_string(&found.abs_path)
                .with_context(|| format!("cannot read {}", found.abs_path.display()))?;
            let refs: Vec<(&str, FmValue)> =
                set.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
            let mut rnd = [0u8; 8];
            getrandom::fill(&mut rnd).map_err(|e| anyhow::anyhow!("no random bytes: {e}"))?;
            let tmp = found.abs_path.with_extension(format!(
                "md.{}.fbrain-tmp",
                rnd.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ));
            // create_new never follows a planted link or reuses a file.
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .with_context(|| format!("cannot create {}", tmp.display()))?;
            let written =
                std::io::Write::write_all(&mut f, note::edit_frontmatter(&text, &refs).as_bytes())
                    .and_then(|_| {
                        crate::review::best_effort_perms(f.set_permissions(meta.permissions()))
                    })
                    .and_then(|_| std::fs::rename(&tmp, &found.abs_path));
            if let Err(e) = written {
                let _ = std::fs::remove_file(&tmp);
                return Err(e)
                    .with_context(|| format!("cannot write {}", found.abs_path.display()));
            }
            Ok(())
        }
        Command::Mcp => crate::mcp::run(cli.brain.as_deref()),
        Command::Serve => serve::run(config::serve_idle()?),
    }
}
