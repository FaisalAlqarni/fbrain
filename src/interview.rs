//! Onboarding: scan the named repos, ask only what the scan and the existing notes did not
//! settle, and write the result as proposals. Nothing outside `inbox/` is written here.

use crate::brain::Brain;
use crate::note::{split_frontmatter, FmValue, Note, Status};
use crate::proposals::{self, Op, Proposal, ProposalInput};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

const ME_QUESTIONS: &str = include_str!("harness/assets/questions-me.toml");
const PROJECT_QUESTIONS: &str = include_str!("harness/assets/questions-project.toml");
/// Added to text that came from the scan and not from an answer.
const UNREVIEWED: &str = "(from scan, unreviewed)";
const STUB_MARKER: &str = "<!-- fbrain:init-stub -->";
const README_CHARS: usize = 1500;
const DOCS_MAX: usize = 30;
const LANGS_MAX: usize = 5;
const DIRS_MAX: usize = 8;
const WALK_MAX: usize = 20_000;
const LOG_MAX: &str = "100000";
const TOUCH_COMMITS: &str = "500";
const CONTEXT_NAMES: [&str; 6] = [
    "CONTEXT.md",
    "CLAUDE.md",
    "AGENTS.md",
    "GEMINI.md",
    ".cursorrules",
    ".github/copilot-instructions.md",
];
const README_NAMES: [&str; 5] = [
    "README.md",
    "README",
    "Readme.md",
    "readme.md",
    "README.rst",
];
/// Extensions that say nothing about the stack.
const NOT_CODE: [&str; 9] = [
    "md", "txt", "lock", "json", "svg", "png", "jpg", "jpeg", "gif",
];
/// `(heading, question id)` of ME.md, in order.
const ME_SECTIONS: [(&str, &str); 8] = [
    ("Role", "role"),
    ("Products", "products"),
    ("Stack", "stack"),
    ("Machines", "machines"),
    ("Working rules", "work_rules"),
    ("Goals", "goals"),
    ("Anti-goals", "anti_goals"),
    ("Language", "chat_language"),
];
/// `(heading, question id)` of BRIEF.md after the first section, in order.
const BRIEF_SECTIONS: [(&str, &str); 8] = [
    ("Customers", "customers"),
    ("Modules", "modules"),
    ("Stack", "stack"),
    ("Environments", "environments"),
    ("Status", "status"),
    ("Goals", "goals"),
    ("Risks", "risks"),
    ("Related", "related_repos"),
];

#[derive(Debug, Clone, Serialize)]
pub struct Scan {
    pub repo: PathBuf,
    pub name: String,
    /// Extension and file count, top 5.
    pub languages: Vec<(String, usize)>,
    /// First 1,500 characters of the README.
    pub readme_excerpt: Option<String>,
    pub context_files: Vec<String>,
    /// Top 30 paths under `docs/`.
    pub docs_files: Vec<String>,
    pub commits: usize,
    pub first_commit: Option<String>,
    pub last_commit: Option<String>,
    /// Top-level directories by number of commits that touched them, top 8.
    pub top_dirs: Vec<(String, usize)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill: Option<String>,
}

#[derive(Deserialize)]
struct QuestionFile {
    question: Vec<Question>,
}

fn load(raw: &str) -> Vec<Question> {
    toml::from_str::<QuestionFile>(raw)
        .expect("the question assets are valid")
        .question
}

/// Answers by question id. `me` answers ME.md; `projects` holds one table per repo name.
/// shortcut: ids repeat between the two sets (`stack`, `goals`), so project answers sit in
/// their own tables instead of one flat map; revisit if a third set appears.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Answers {
    #[serde(default)]
    pub projects: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(flatten)]
    pub me: BTreeMap<String, String>,
}

impl Answers {
    pub fn from_toml(text: &str) -> Result<Answers> {
        toml::from_str(text).context("answers file is not valid")
    }
}

/// A name that is safe as a folder and as a `.fbrain.toml` key.
fn project_name(repo: &Path) -> String {
    let raw = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s: String = s.trim_start_matches(['.', '-']).chars().take(64).collect();
    if s.is_empty() {
        "project".into()
    } else {
        s
    }
}

/// Runs git in `repo` without letting the repo's own config start programs or read a pager.
fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=false",
            "-c",
            "log.showSignature=false",
        ])
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn read_excerpt(repo: &Path) -> Option<String> {
    for name in README_NAMES {
        let p = repo.join(name);
        // A symlink could point outside the repo.
        if !std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let mut buf = Vec::new();
        std::fs::File::open(&p)
            .ok()?
            .take(16 * 1024)
            .read_to_end(&mut buf)
            .ok()?;
        let text = String::from_utf8_lossy(&buf);
        return Some(text.chars().take(README_CHARS).collect());
    }
    None
}

fn top_n(counts: BTreeMap<String, usize>, n: usize) -> Vec<(String, usize)> {
    let mut v: Vec<_> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

/// Reads the repo's files and `git log`; runs nothing from the repo.
pub fn scan_repo(path: &Path) -> Result<Scan> {
    let repo = path
        .canonicalize()
        .with_context(|| format!("cannot read repo {}", path.display()))?;
    if !repo.is_dir() {
        bail!("not a folder: {}", path.display());
    }
    let mut exts: BTreeMap<String, usize> = BTreeMap::new();
    let mut docs = Vec::new();
    let walk = ignore::WalkBuilder::new(&repo)
        .require_git(false)
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some("node_modules" | "target" | "vendor" | "dist" | "build")
            )
        })
        .build();
    for e in walk.flatten().take(WALK_MAX) {
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = e.path().strip_prefix(&repo) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.starts_with("docs/") {
            docs.push(rel.clone());
        }
        if let Some(ext) = e.path().extension().and_then(|x| x.to_str()) {
            let ext = ext.to_lowercase();
            if !NOT_CODE.contains(&ext.as_str()) {
                *exts.entry(ext).or_default() += 1;
            }
        }
    }
    docs.sort();
    docs.truncate(DOCS_MAX);
    let context_files = CONTEXT_NAMES
        .iter()
        .filter(|n| std::fs::symlink_metadata(repo.join(n)).is_ok_and(|m| m.is_file()))
        .map(|n| n.to_string())
        .collect();
    let commits = git(&repo, &["rev-list", "--count", "HEAD"])
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let dates = git(&repo, &["log", "-n", LOG_MAX, "--format=%cs"]).unwrap_or_default();
    let mut dates = dates.lines();
    let last_commit = dates.next().map(String::from);
    let first_commit = dates.next_back().map(String::from).or(last_commit.clone());
    let mut touched: BTreeMap<String, usize> = BTreeMap::new();
    let log = git(
        &repo,
        &["log", "-n", TOUCH_COMMITS, "--name-only", "--format=%x01"],
    )
    .unwrap_or_default();
    for commit in log.split('\x01') {
        let dirs: std::collections::BTreeSet<&str> = commit
            .lines()
            .filter_map(|l| l.split_once('/').map(|(d, _)| d))
            .filter(|d| !d.is_empty())
            .collect();
        for d in dirs {
            *touched.entry(d.to_string()).or_default() += 1;
        }
    }
    Ok(Scan {
        name: project_name(&repo),
        languages: top_n(exts, LANGS_MAX),
        readme_excerpt: read_excerpt(&repo),
        context_files,
        docs_files: docs,
        commits,
        first_commit,
        last_commit,
        top_dirs: top_n(touched, DIRS_MAX),
        repo,
    })
}

pub fn scan_all(repos: &[PathBuf]) -> Result<Vec<Scan>> {
    if repos.is_empty() {
        bail!("give at least one --repo PATH");
    }
    let scans = repos
        .iter()
        .map(|r| scan_repo(r))
        .collect::<Result<Vec<_>>>()?;
    for (i, s) in scans.iter().enumerate() {
        if scans[..i].iter().any(|o| o.name == s.name) {
            bail!("two repos share the name {}: rename one folder", s.name);
        }
    }
    Ok(scans)
}

fn lang_name(ext: &str) -> &str {
    match ext {
        "rs" => "Rust",
        "rb" => "Ruby",
        "py" => "Python",
        "js" | "mjs" | "cjs" => "JavaScript",
        "ts" | "tsx" => "TypeScript",
        "go" => "Go",
        "java" => "Java",
        "kt" => "Kotlin",
        "swift" => "Swift",
        "c" | "h" => "C",
        "cpp" | "cc" | "hpp" => "C++",
        "cs" => "C#",
        "php" => "PHP",
        "ex" | "exs" => "Elixir",
        "erb" => "ERB",
        other => other,
    }
}

fn stack_of(scans: &[Scan]) -> Option<String> {
    let mut names: Vec<&str> = Vec::new();
    for (ext, _) in scans.iter().flat_map(|s| s.languages.iter()) {
        let n = lang_name(ext);
        if !names.contains(&n) {
            names.push(n);
        }
    }
    (!names.is_empty()).then(|| names.join(", "))
}

/// The first prose paragraph of a README: no headings, badges, tables, code or HTML.
fn first_paragraph(readme: &str) -> Option<String> {
    readme
        .replace("\r\n", "\n")
        .split("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty() && !p.starts_with(['#', '[', '!', '<', '|', '`', '-', '=', '>']))
        .map(|p| {
            p.lines()
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(300)
                .collect()
        })
}

/// The `## <heading>` sections of a note body, with the text before the first one as the head.
fn sections(body: &str) -> (String, Vec<(String, String)>) {
    let mut head = String::new();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut fenced = false;
    for line in body.replace("\r\n", "\n").lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        match line.strip_prefix("## ").filter(|_| !fenced) {
            Some(h) => out.push((h.trim().to_string(), String::new())),
            None => {
                let buf = out.last_mut().map_or(&mut head, |s| &mut s.1);
                buf.push_str(line);
                buf.push('\n');
            }
        }
    }
    for s in &mut out {
        s.1 = s.1.trim().to_string();
    }
    (head.trim().to_string(), out)
}

fn existing_me(brain: &Brain) -> Option<String> {
    let text = std::fs::read_to_string(brain.path.join("ME.md")).ok()?;
    Some(split_frontmatter(&text).1)
}

fn is_stub(body: &str) -> bool {
    body.contains(STUB_MARKER)
}

/// The profile questions the scan and ME.md leave open: a section that already has text is not
/// asked again.
pub fn questions_me(scans: &[Scan], brain: &Brain) -> Vec<Question> {
    let have = existing_me(brain)
        .map(|b| sections(&b).1)
        .unwrap_or_default();
    let settled = |id: &str| {
        ME_SECTIONS
            .iter()
            .find(|(_, i)| *i == id)
            .is_some_and(|(h, _)| have.iter().any(|(n, t)| n == h && !t.is_empty()))
    };
    load(ME_QUESTIONS)
        .into_iter()
        .filter(|q| !settled(&q.id))
        .map(|mut q| {
            q.prefill = match q.id.as_str() {
                "products" => Some(
                    scans
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                )
                .filter(|s| !s.is_empty()),
                "stack" => stack_of(scans),
                _ => None,
            };
            q
        })
        .collect()
}

/// The project questions, with a prefill where the scan suggests an answer.
pub fn questions_project(scan: &Scan) -> Vec<Question> {
    load(PROJECT_QUESTIONS)
        .into_iter()
        .map(|mut q| {
            q.prefill = match q.id.as_str() {
                "what" => scan.readme_excerpt.as_deref().and_then(first_paragraph),
                "modules" => Some(
                    scan.top_dirs
                        .iter()
                        .map(|(d, _)| d.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                )
                .filter(|s| !s.is_empty()),
                "stack" => stack_of(std::slice::from_ref(scan)),
                "status" => scan
                    .last_commit
                    .as_ref()
                    .map(|last| format!("{} commits, last on {last}", scan.commits)),
                _ => None,
            };
            q
        })
        .collect()
}

/// Asks every question in the terminal: the prompt goes to stderr with the prefill as the
/// default, and an empty line (or the end of input) accepts it.
pub fn collect(
    me: &[Question],
    projects: &[(String, Vec<Question>)],
    input: &mut dyn BufRead,
) -> Result<Answers> {
    let mut answers = Answers::default();
    let mut ask = |q: &Question| -> Result<Option<String>> {
        match &q.prefill {
            Some(p) => eprint!("{} [{p}]: ", q.text),
            None => eprint!("{}: ", q.text),
        }
        let mut line = String::new();
        input.read_line(&mut line)?;
        let line = line.trim().to_string();
        Ok((!line.is_empty()).then_some(line))
    };
    for q in me {
        if let Some(a) = ask(q)? {
            answers.me.insert(q.id.clone(), a);
        }
    }
    for (name, qs) in projects {
        for q in qs {
            if let Some(a) = ask(q)? {
                answers
                    .projects
                    .entry(name.clone())
                    .or_default()
                    .insert(q.id.clone(), a);
            }
        }
    }
    Ok(answers)
}

/// The answer to `q`, else its prefill marked as taken from the scan; the flag says which.
fn resolve_flagged(
    q: &Question,
    given: Option<&BTreeMap<String, String>>,
) -> Option<(String, bool)> {
    given
        .and_then(|g| g.get(&q.id))
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .map(|a| (a, false))
        .or_else(|| {
            q.prefill
                .clone()
                .map(|p| (format!("{p} {UNREVIEWED}"), true))
        })
}

fn resolve(q: &Question, given: Option<&BTreeMap<String, String>>) -> Option<String> {
    resolve_flagged(q, given).map(|r| r.0)
}

/// `body` with each `(heading, text)` added: a heading that exists with no text gets the text,
/// one that is missing is appended, and every other byte stays as it was.
fn fill_sections(body: &str, adds: &[(&str, String)]) -> String {
    let have = sections(body).1;
    let mut out = body.to_string();
    for (heading, text) in adds {
        match have.iter().find(|(h, _)| h == heading) {
            Some((_, t)) if !t.is_empty() => {}
            Some(_) => {
                let line = format!("## {heading}");
                let mut at = 0;
                let mut done = false;
                for l in out.split_inclusive('\n') {
                    if l.trim_end() == line {
                        out.replace_range(
                            at..at + l.trim_end().len(),
                            &format!("{line}\n\n{text}"),
                        );
                        done = true;
                        break;
                    }
                    at += l.len();
                }
                if !done {
                    out.push_str(&format!("\n## {heading}\n\n{text}\n"));
                }
            }
            None => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&format!("\n## {heading}\n\n{text}\n"));
            }
        }
    }
    out
}

fn check_answers(scans: &[Scan], answers: &Answers) -> Result<()> {
    let known = |raw: &str| load(raw).into_iter().map(|q| q.id).collect::<Vec<_>>();
    let (me, project) = (known(ME_QUESTIONS), known(PROJECT_QUESTIONS));
    if let Some(k) = answers.me.keys().find(|k| !me.contains(k)) {
        bail!("unknown answer id: {k}");
    }
    for (name, given) in &answers.projects {
        if !scans.iter().any(|s| &s.name == name) {
            bail!("answers for unknown project: {name}");
        }
        if let Some(k) = given.keys().find(|k| !project.contains(k)) {
            bail!("unknown answer id for {name}: {k}");
        }
    }
    Ok(())
}

pub fn today() -> String {
    let t = proposals::now_utc();
    format!("{}-{}-{}", &t[0..4], &t[4..6], &t[6..8])
}

fn me_body(brain: &Brain, scans: &[Scan], answers: &Answers) -> Option<(Op, String)> {
    let old = existing_me(brain);
    let qs = questions_me(scans, brain);
    let new_text = |id: &str| {
        qs.iter()
            .find(|q| q.id == id)
            .and_then(|q| resolve(q, Some(&answers.me)))
    };
    let Some(old) = old else {
        let mut out = "# Me\n".to_string();
        for (heading, id) in ME_SECTIONS {
            if let Some(t) = new_text(id) {
                out.push_str(&format!("\n## {heading}\n\n{t}\n"));
            }
        }
        return Some((Op::Add, out));
    };
    if is_stub(&old) {
        let adds: Vec<_> = ME_SECTIONS
            .iter()
            .filter_map(|(h, id)| Some((*h, new_text(id)?)))
            .collect();
        return Some((Op::Update, fill_sections("# Me\n", &adds)));
    }
    let adds: Vec<_> = ME_SECTIONS
        .iter()
        .filter_map(|(h, id)| Some((*h, new_text(id)?)))
        .collect();
    let new = fill_sections(&old, &adds);
    (new != old).then_some((Op::Update, new))
}

fn project_heading(id: &str) -> &'static str {
    BRIEF_SECTIONS
        .iter()
        .find(|(_, i)| *i == id)
        .map_or("What it is", |(h, _)| h)
}

/// The project questions that the scan and the project's existing BRIEF.md leave open: a
/// section that already has text is not asked again.
pub fn open_questions_project(brain: &Brain, scan: &Scan) -> Vec<Question> {
    open_among(brain, &scan.name, questions_project(scan))
}

fn open_among(brain: &Brain, name: &str, all: Vec<Question>) -> Vec<Question> {
    let target = format!("projects/{name}/BRIEF.md");
    let have = std::fs::read_to_string(brain.path.join(target))
        .map(|t| sections(&split_frontmatter(&t).1).1)
        .unwrap_or_default();
    all.into_iter()
        .filter(|q| {
            let h = project_heading(&q.id);
            !have.iter().any(|(n, t)| n == h && !t.is_empty())
        })
        .collect()
}

fn scan_section(scan: &Scan) -> String {
    let list = |v: &[(String, usize)]| {
        v.iter()
            .map(|(k, n)| format!("{k} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "- Commits: {}{}\n- Top directories: {}\n- Languages: {}",
        scan.commits,
        match (&scan.first_commit, &scan.last_commit) {
            (Some(a), Some(b)) => format!(" ({a} to {b})"),
            _ => String::new(),
        },
        list(&scan.top_dirs),
        list(&scan.languages),
    )
}

/// The `description` of a brief: the first line of the "What it is" answer, at most 200
/// characters, keeping the scan label when the text came from the scan.
fn description_of(name: &str, what: Option<&(String, bool)>) -> String {
    match what {
        Some((w, true)) => {
            let first = w.lines().next().unwrap_or_default();
            let keep = 200 - UNREVIEWED.len() - 1;
            let text = first.strip_suffix(UNREVIEWED).unwrap_or(first).trim_end();
            format!(
                "{} {UNREVIEWED}",
                text.chars().take(keep).collect::<String>()
            )
        }
        Some((w, false)) => w
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect(),
        None => name.to_string(),
    }
}

/// The BRIEF for a repo: `(description, op, body)`. A BRIEF that exists is extended, never
/// rewritten; `None` when there is nothing to add to it.
fn brief_body(brain: &Brain, scan: &Scan, answers: &Answers) -> Option<(String, Op, String)> {
    let given = answers.projects.get(&scan.name);
    let qs = open_questions_project(brain, scan);
    let get = |id: &str| {
        qs.iter()
            .find(|q| q.id == id)
            .and_then(|q| resolve_flagged(q, given))
    };
    let what = get("what");
    let description = description_of(&scan.name, what.as_ref());
    let mut adds: Vec<(&str, String)> = Vec::new();
    if let Some((w, _)) = what {
        adds.push(("What it is", w));
    }
    for (heading, id) in BRIEF_SECTIONS {
        if let Some((t, _)) = get(id) {
            adds.push((heading, t));
        }
    }
    adds.push(("Scan", scan_section(scan)));
    let target = brain.path.join(format!("projects/{}/BRIEF.md", scan.name));
    match std::fs::read_to_string(target) {
        Ok(old) => {
            let old = split_frontmatter(&old).1;
            let new = fill_sections(&old, &adds);
            (new != old).then_some((description, Op::Update, new))
        }
        Err(_) => Some((
            description,
            Op::Add,
            fill_sections(&format!("# {}\n", scan.name), &adds),
        )),
    }
}

/// Writes the profile and one brief per repo as proposals; the brain tree is not touched.
pub fn onboard(brain: &Brain, repos: &[PathBuf], answers: &Answers) -> Result<Vec<Proposal>> {
    let scans = scan_all(repos)?;
    onboard_scans(brain, &scans, answers)
}

pub fn onboard_scans(brain: &Brain, scans: &[Scan], answers: &Answers) -> Result<Vec<Proposal>> {
    check_answers(scans, answers)?;
    let source = format!("onboard scan {}", today());
    let mut inputs = Vec::new();
    if let Some((op, body)) = me_body(brain, scans, answers) {
        inputs.push(ProposalInput {
            op,
            target: "ME.md".into(),
            supersedes: None,
            body,
            proposed_by: "onboard".into(),
            source: source.clone(),
            rationale: "profile drafted from the repo scan and the interview".into(),
            extra: Vec::new(),
        });
    }
    for scan in scans {
        let target = format!("projects/{}/BRIEF.md", scan.name);
        let Some((description, op, body)) = brief_body(brain, scan, answers) else {
            continue;
        };
        inputs.push(ProposalInput {
            op,
            target,
            supersedes: None,
            body,
            proposed_by: "onboard".into(),
            source: source.clone(),
            rationale: format!("project brief drafted from the scan of {}", scan.name),
            extra: vec![
                ("description".into(), FmValue::Str(description)),
                ("projects".into(), FmValue::List(vec![scan.name.clone()])),
                (
                    "map_repo".into(),
                    FmValue::Str(crate::config::plain_path(&scan.repo)),
                ),
            ],
        });
    }
    submit(brain, inputs)
}

/// Checks every input first, so a bad one writes nothing, then proposes them all.
fn submit(brain: &Brain, inputs: Vec<ProposalInput>) -> Result<Vec<Proposal>> {
    let inputs = inputs
        .into_iter()
        .map(|i| proposals::check(brain, i))
        .collect::<Result<Vec<_>>>()?;
    let index = crate::cli::propose_index(brain)?;
    inputs
        .into_iter()
        .map(|i| proposals::propose(brain, &index, i, crate::cli::owner_surface()))
        .collect()
}

/// A body with LF endings, from a note's raw text without its frontmatter. Review puts the
/// note's own line endings back when it applies the proposal.
fn body_of(raw: &str) -> String {
    crate::review::split_head(raw).1.replace("\r\n", "\n")
}

fn check_project(project: &str) -> Result<()> {
    if project.is_empty() || project.starts_with('.') || project.contains(['/', '\\']) {
        bail!("invalid project name: {project}");
    }
    Ok(())
}

/// The repo to scan: `--repo`, else the first existing folder mapped to the project.
fn repo_for(brain: &Brain, project: &str, repo: Option<&Path>) -> Option<PathBuf> {
    repo.map(Path::to_path_buf).or_else(|| {
        brain
            .config
            .projects
            .0
            .iter()
            .filter(|(n, _)| n == project)
            .flat_map(|(_, repos)| repos)
            .map(PathBuf::from)
            .find(|p| p.is_dir())
    })
}

/// The scan of the project's repo, if there is one, and the questions still open: all of them
/// for a project without a brief, else those whose BRIEF.md section is empty or missing.
pub fn project_questions(
    brain: &Brain,
    project: &str,
    repo: Option<&Path>,
) -> Result<(Option<Scan>, Vec<Question>)> {
    check_project(project)?;
    let scan = repo_for(brain, project, repo)
        .map(|r| scan_repo(&r))
        .transpose()?;
    let all = match &scan {
        Some(s) => questions_project(s),
        None => load(PROJECT_QUESTIONS),
    };
    Ok((scan, open_among(brain, project, all)))
}

/// A brief for a new project, or the empty sections of an existing one, as one proposal.
/// `today` only dates the proposal's source.
pub fn interview_project(
    brain: &Brain,
    project: &str,
    repo: Option<&Path>,
    answers: &Answers,
    today: &str,
) -> Result<Vec<Proposal>> {
    let (scan, qs) = project_questions(brain, project, repo)?;
    if let Some(n) = answers.projects.keys().find(|n| *n != project) {
        bail!("answers for unknown project: {n}");
    }
    let given = answers.projects.get(project);
    let known: Vec<_> = load(PROJECT_QUESTIONS).into_iter().map(|q| q.id).collect();
    if let Some(k) = given.and_then(|g| g.keys().find(|k| !known.contains(k))) {
        bail!("unknown answer id for {project}: {k}");
    }
    let get = |id: &str| {
        qs.iter()
            .find(|q| q.id == id)
            .and_then(|q| resolve_flagged(q, given))
    };
    let what = get("what");
    let mut adds: Vec<(&str, String)> = Vec::new();
    if let Some((w, _)) = &what {
        adds.push(("What it is", w.clone()));
    }
    for (heading, id) in BRIEF_SECTIONS {
        if let Some((t, _)) = get(id) {
            adds.push((heading, t));
        }
    }
    let target = format!("projects/{project}/BRIEF.md");
    let mut input = ProposalInput {
        op: Op::Add,
        target: target.clone(),
        supersedes: None,
        body: String::new(),
        proposed_by: "interview".into(),
        source: format!("interview {today}"),
        rationale: format!("project interview for {project}"),
        extra: Vec::new(),
    };
    match std::fs::read_to_string(brain.path.join(&target)) {
        Err(_) => {
            if let Some(s) = &scan {
                adds.push(("Scan", scan_section(s)));
            }
            input.body = fill_sections(&format!("# {project}\n"), &adds);
            input.extra = vec![
                (
                    "description".into(),
                    FmValue::Str(description_of(project, what.as_ref())),
                ),
                ("projects".into(), FmValue::List(vec![project.to_string()])),
            ];
        }
        Ok(raw) => {
            let old = body_of(&raw);
            let new = fill_sections(&old, &adds);
            if new == old {
                return Ok(Vec::new());
            }
            input.op = Op::Update;
            input.body = new;
        }
    }
    if let Some(s) = &scan {
        input.extra.push((
            "map_repo".into(),
            FmValue::Str(crate::config::plain_path(&s.repo)),
        ));
    }
    submit(brain, vec![input])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ReviewKind {
    Stale,
    Expired,
    DerivedStale,
    StatusOld,
    OpenQuestion,
    Goal,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewItem {
    /// `<kind>:<rel_path>`, plus `:<n>` for the n-th open question of a note (n from 1).
    pub id: String,
    pub kind: ReviewKind,
    pub rel_path: String,
    pub detail: String,
}

const OLD_DAYS: i64 = 30;

fn item(kind: ReviewKind, rel: &str, n: Option<usize>, detail: String) -> ReviewItem {
    let id = match n {
        Some(n) => format!("{kind:?}:{rel}:{n}"),
        None => format!("{kind:?}:{rel}"),
    };
    ReviewItem {
        id,
        kind,
        rel_path: rel.to_string(),
        detail,
    }
}

/// The unchecked `- [ ]` items under a heading named `Open questions`: `(line index, text)`.
/// `body` has LF endings.
fn open_questions(body: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let (mut level, mut fenced) = (None::<usize>, false);
    for (i, line) in body.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        if fenced {
            continue;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
            if line[hashes..].trim().eq_ignore_ascii_case("open questions") {
                level = Some(hashes);
            } else if level.is_some_and(|l| hashes <= l) {
                level = None;
            }
        } else if let (Some(_), Some(t)) = (level, line.trim_start().strip_prefix("- [ ]")) {
            out.push((i, t.trim().to_string()));
        }
    }
    out
}

/// What needs Faisal's attention as of `today` (`YYYY-MM-DD`), in the order of `ReviewKind`.
pub fn review_items(brain: &Brain, today: &str) -> Result<Vec<ReviewItem>> {
    let now = crate::lint::epoch(&format!("{today}T00:00:00Z"))
        .with_context(|| format!("not a date (YYYY-MM-DD): {today}"))?;
    let mut notes = Vec::new();
    for r in brain.notes()? {
        let raw = std::fs::read_to_string(&r.abs_path)
            .with_context(|| format!("cannot read {}", r.abs_path.display()))?;
        let n = Note::parse(&r.rel_path, &raw);
        if crate::search::expose(&n, crate::cli::owner_surface()) {
            notes.push(n);
        }
    }
    let mut out = Vec::new();
    for n in notes.iter().filter(|n| n.fm.status == Status::Stale) {
        out.push(item(
            ReviewKind::Stale,
            &n.rel_path,
            None,
            "status: stale".into(),
        ));
    }
    for n in &notes {
        if let (Status::Active, Some(v)) = (n.fm.status, n.fm.valid_to.as_deref()) {
            if v.get(..10).unwrap_or(v) < today {
                out.push(item(
                    ReviewKind::Expired,
                    &n.rel_path,
                    None,
                    format!("valid_to {v} has passed"),
                ));
            }
        }
    }
    for i in crate::lint::run(brain, false, crate::cli::owner())?
        .into_iter()
        .filter(|i| i.rule == "L006" && notes.iter().any(|n| n.rel_path == i.rel_path))
    {
        out.push(item(ReviewKind::DerivedStale, &i.rel_path, None, i.detail));
    }
    if brain.git_unavailable()?.is_none() {
        for n in &notes {
            let parts: Vec<_> = n.rel_path.split('/').collect();
            if !(parts.len() == 3
                && parts[0] == "projects"
                && matches!(parts[2], "status.md" | "BRIEF.md"))
            {
                continue;
            }
            let Some(c) = brain.git_last_commit(&n.rel_path)? else {
                continue;
            };
            let then = crate::lint::epoch(&c.date)
                .with_context(|| format!("unreadable commit date {}", c.date))?;
            if (now - then) / 86_400 > OLD_DAYS {
                out.push(item(
                    ReviewKind::StatusOld,
                    &n.rel_path,
                    None,
                    format!("last commit {}", c.date.get(..10).unwrap_or(&c.date)),
                ));
            }
        }
    }
    for n in &notes {
        for (k, (_, text)) in open_questions(&n.body.replace("\r\n", "\n"))
            .into_iter()
            .enumerate()
        {
            out.push(item(
                ReviewKind::OpenQuestion,
                &n.rel_path,
                Some(k + 1),
                text,
            ));
        }
    }
    if let Some(me) = notes.iter().find(|n| n.rel_path == "ME.md") {
        if let Some((_, goals)) = sections(&me.body).1.into_iter().find(|(h, _)| h == "Goals") {
            out.push(item(ReviewKind::Goal, "ME.md", None, goals));
        }
    }
    Ok(out)
}

/// Asks about every item in the terminal; an empty line keeps it.
pub fn collect_review(
    items: &[ReviewItem],
    input: &mut dyn BufRead,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for it in items {
        eprint!(
            "{:?} {}: {}\n  keep | stale | update <text> | supersede <text>{} [keep]: ",
            it.kind,
            it.rel_path,
            it.detail,
            if it.kind == ReviewKind::OpenQuestion {
                " | answer <text>"
            } else {
                ""
            }
        );
        let mut line = String::new();
        input.read_line(&mut line)?;
        if !line.trim().is_empty() {
            out.insert(it.id.clone(), line.trim().to_string());
        }
    }
    Ok(out)
}

/// Answers by item id: `keep`, `stale`, `update <text>`, `supersede <text>`, `answer <text>`.
pub fn review_answers_from_toml(text: &str) -> Result<BTreeMap<String, String>> {
    toml::from_str(text).context("answers file is not valid")
}

/// One answered review item: the item, its action word and the text after it.
type Act<'a> = (&'a ReviewItem, String, &'a str);

/// Checks the open question at `line` of `body` and puts the answer under it.
fn answer_line(body: &str, line: usize, text: &str) -> String {
    let text = text.replace('\n', " ");
    let mut out = String::new();
    for (i, l) in body.split_inclusive('\n').enumerate() {
        if i != line {
            out.push_str(l);
            continue;
        }
        let indent = &l[..l.len() - l.trim_start().len()];
        out.push_str(&l.replacen("- [ ]", "- [x]", 1));
        if !l.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{indent}  Answer: {text}\n"));
    }
    out
}

/// Turns the answers into proposals, one per note: several answers on one note are applied to
/// one body together, so approving the proposal cannot undo another. An item without an answer,
/// or answered `keep`, gets none.
pub fn review_proposals(
    brain: &Brain,
    items: &[ReviewItem],
    answers: &BTreeMap<String, String>,
    today: &str,
) -> Result<Vec<Proposal>> {
    if let Some(k) = answers.keys().find(|k| !items.iter().any(|i| &i.id == *k)) {
        bail!("unknown review item: {k}");
    }
    // (item, action word, text) per note, in item order.
    let mut by_note: Vec<(&str, Vec<Act>)> = Vec::new();
    for it in items {
        let Some(answer) = answers.get(&it.id) else {
            continue;
        };
        let answer = answer.trim();
        let (word, text) = answer
            .split_once(char::is_whitespace)
            .map_or((answer, ""), |(w, t)| (w, t.trim()));
        let word = word.to_lowercase();
        if matches!(word.as_str(), "update" | "supersede" | "answer") && text.is_empty() {
            bail!("{}: {word} needs text", it.id);
        }
        if !matches!(
            word.as_str(),
            "keep" | "stale" | "update" | "supersede" | "answer"
        ) {
            bail!("{}: unknown action {word}", it.id);
        }
        if word == "answer" && it.kind != ReviewKind::OpenQuestion {
            bail!("{}: only an open question can be answered", it.id);
        }
        if word == "keep" {
            continue;
        }
        match by_note.iter_mut().find(|(r, _)| *r == it.rel_path) {
            Some((_, v)) => v.push((it, word, text)),
            None => by_note.push((&it.rel_path, vec![(it, word, text)])),
        }
    }
    let mut inputs = Vec::new();
    for (rel, acts) in by_note {
        let raw = std::fs::read_to_string(brain.path.join(rel))
            .with_context(|| format!("cannot read {rel}"))?;
        let ids = acts.iter().map(|a| a.0.id.as_str()).collect::<Vec<_>>();
        let mut input = ProposalInput {
            op: Op::Update,
            target: rel.to_string(),
            supersedes: None,
            body: String::new(),
            proposed_by: "interview".into(),
            source: format!("interview review {today}"),
            rationale: format!("periodic review items: {}", ids.join(", ")),
            extra: Vec::new(),
        };
        if let Some((_, _, text)) = acts.iter().find(|a| a.1 == "supersede") {
            if acts.len() > 1 {
                bail!("{rel}: supersede cannot be combined with other answers on the same note");
            }
            let title = Note::parse(rel, &raw).title;
            input.op = Op::Supersede;
            input.supersedes = Some(rel.to_string());
            input.target = format!("{}-{today}.md", rel.trim_end_matches(".md"));
            input.body = format!("# {title}\n\n{text}\n");
            inputs.push(input);
            continue;
        }
        let mut body = body_of(&raw);
        let mut edited = false;
        // Later questions first: answering one adds a line, which would shift the ones below.
        let mut qs: Vec<_> = acts.iter().filter(|a| a.1 == "answer").collect();
        qs.sort_by_key(|a| {
            std::cmp::Reverse(
                a.0.id
                    .rsplit(':')
                    .next()
                    .and_then(|n| n.parse::<usize>().ok()),
            )
        });
        let open = open_questions(&body);
        let mut lines = Vec::new();
        for (it, _, text) in qs {
            let n: usize = it
                .id
                .rsplit(':')
                .next()
                .and_then(|n| n.parse().ok())
                .context("open question without a number")?;
            let (line, _) = open
                .get(n.wrapping_sub(1))
                .with_context(|| format!("{}: no such open question", it.id))?;
            lines.push((*line, *text));
        }
        for (line, text) in lines {
            body = answer_line(&body, line, text);
            edited = true;
        }
        for (_, _, text) in acts.iter().filter(|a| a.1 == "update") {
            body = format!("{}\n\n## Update {today}\n\n{text}\n", body.trim_end());
            edited = true;
        }
        if edited {
            input.body = body;
        }
        if acts.iter().any(|a| a.1 == "stale") {
            input
                .extra
                .push(("set_status".into(), FmValue::Str("stale".into())));
        }
        inputs.push(input);
    }
    submit(brain, inputs)
}
