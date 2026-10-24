//! Review: the only code that changes the brain's notes. A proposal read from `inbox/` is
//! untrusted (the file may be hand-edited or synced), so every decision re-checks its paths,
//! re-reads protection from the notes themselves and writes only fixed frontmatter keys.

use crate::brain::Brain;
use crate::note::{edit_frontmatter, split_frontmatter, FmValue, Note};
use crate::proposals::{self, Invalid, Op, Proposal};
use anyhow::{bail, Context, Result};
use std::io::{BufRead, Write};
use std::path::Path;

#[derive(Debug, Clone)]
pub enum Decision {
    Approve { force: bool },
    EditThenApprove { body: String, force: bool },
    MergeInto { rel_path: String, force: bool },
    SupersedeCandidate { rel_path: String, force: bool },
    Reject,
}

#[derive(Debug, Clone, Default)]
pub struct Applied {
    /// Sha of the commit that recorded the decision; `None` when nothing was committed.
    pub commit: Option<String>,
    /// Notes written, relative to the brain.
    pub changed: Vec<String>,
    /// Why nothing was committed when git is unavailable (the brain is not a repository).
    pub warning: Option<String>,
}

/// The decision changed the brain but did not finish (git failed, or the proposal file could not
/// be removed). `detail` says what was written, staged and removed.
#[derive(Debug)]
pub struct Incomplete {
    pub changed: Vec<String>,
    pub commit: Option<String>,
    pub detail: String,
}

impl std::fmt::Display for Incomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

impl std::error::Error for Incomplete {}

/// The proposal touches a protected note and `force` was not given; `main` exits with code 2.
#[derive(Debug)]
pub struct Protected(pub String);

impl std::fmt::Display for Protected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "proposal touches protected note {}; rerun with --force",
            escape(&self.0)
        )
    }
}

impl std::error::Error for Protected {}

/// Control characters made visible, so a proposal field cannot move the cursor or fake output.
pub fn escape(s: &str) -> String {
    escape_keeping(s, false)
}

/// Like `escape`, but keeps line breaks, for multi-line messages.
pub fn escape_text(s: &str) -> String {
    escape_keeping(s, true)
}

fn escape_keeping(s: &str, newlines: bool) -> String {
    s.chars()
        .flat_map(|c| match c {
            '\t' => vec![c],
            '\n' if newlines => vec![c],
            c if c.is_control() => format!("\\x{:02x}", c as u32).chars().collect(),
            c => vec![c],
        })
        .collect()
}

pub(crate) fn eol_of(text: &str) -> &'static str {
    match text.find('\n') {
        Some(i) if text[..i].ends_with('\r') => "\r\n",
        _ => "\n",
    }
}

pub(crate) fn with_eol(s: &str, eol: &str) -> String {
    let lf = s.replace("\r\n", "\n");
    if eol == "\r\n" {
        lf.replace('\n', "\r\n")
    } else {
        lf
    }
}

/// `(BOM and frontmatter block, rest)` of a note's raw text.
pub(crate) fn split_head(text: &str) -> (&str, &str) {
    let bom = if text.starts_with('\u{feff}') { 3 } else { 0 };
    let rest = &text[bom..];
    if rest.starts_with("---\n") || rest.starts_with("---\r\n") {
        let mut off = rest.find('\n').map_or(0, |i| i + 1);
        for l in rest[off..].split_inclusive('\n') {
            off += l.len();
            if l.trim_end() == "---" {
                return text.split_at(bom + off);
            }
        }
    }
    text.split_at(bom)
}

/// A proposal body without a BOM or a leading frontmatter block: the proposal cannot set flags.
fn clean_body(body: &str) -> String {
    let body = body.strip_prefix('\u{feff}').unwrap_or(body);
    match split_frontmatter(body) {
        (Some(_), rest) => {
            eprintln!("warning: dropped the frontmatter block at the top of the proposal body");
            rest
        }
        (None, rest) => rest,
    }
}

const CONFIG_FILE: &str = ".fbrain.toml";
/// The values `set_status` may carry: the `status` frontmatter values.
const STATUSES: [&str; 5] = ["active", "done", "stale", "superseded", "invalidated"];

/// A project name that is safe as a folder and a `.fbrain.toml` key.
fn valid_project_name(n: &str) -> bool {
    !n.is_empty()
        && n.chars().count() <= 64
        && !n.starts_with(['.', '-'])
        && n.chars().all(|c| c.is_alphanumeric() || "-_.".contains(c))
}

/// `<name>` of a `projects/<name>/BRIEF.md` target.
fn brief_project(target: &str) -> Option<&str> {
    target
        .strip_prefix("projects/")
        .and_then(|t| t.strip_suffix("/BRIEF.md"))
        .filter(|n| !n.contains('/'))
}

/// A `.fbrain.toml` edit: the text read when planning, and the new text.
struct ConfigChange {
    before: Option<String>,
    after: String,
}

/// The edit that maps the repo named by a BRIEF proposal's `map_repo` to the project of its
/// `projects/<name>/BRIEF.md` target; `None` without `map_repo` or when the mapping is already
/// there. The key comes from an untrusted file: it must name an existing directory, it is stored
/// canonical, and it may not be the root, the home folder, the brain or a folder holding the
/// brain. A project that already maps other repos is not extended.
fn map_repo_config(brain: &Brain, p: &Proposal, target: &str) -> Result<Option<ConfigChange>> {
    let Some((_, value)) = p.input.extra.iter().find(|(k, _)| k == "map_repo") else {
        return Ok(None);
    };
    let bad = |why: &str| Invalid(format!("invalid map_repo: {why}"));
    let FmValue::Str(raw) = value else {
        return Err(bad("must be a path").into());
    };
    let name = brief_project(target)
        .filter(|n| valid_project_name(n))
        .ok_or_else(|| bad("the target must be projects/<name>/BRIEF.md"))?;
    if !Path::new(raw).is_absolute() {
        return Err(bad("must be an absolute path to an existing directory").into());
    }
    let repo = Path::new(raw)
        .canonicalize()
        .ok()
        .filter(|r| r.is_dir())
        .ok_or_else(|| bad("must be an absolute path to an existing directory"))?;
    let root = brain.path.canonicalize()?;
    let home = dirs::home_dir().and_then(|h| h.canonicalize().ok());
    if repo.parent().is_none() || home.as_ref() == Some(&repo) || root.starts_with(&repo) {
        return Err(bad(
            "refusing the filesystem root, the home folder, the brain or a folder that holds it",
        )
        .into());
    }
    let repo = repo.to_string_lossy();
    let repo = repo.strip_prefix(r"\\?\").unwrap_or(&repo).to_string();
    let before = read_note(brain, CONFIG_FILE)?;
    let mut doc: toml_edit::DocumentMut = before
        .clone()
        .unwrap_or_default()
        .parse()
        .map_err(|e| Invalid(format!("invalid {CONFIG_FILE}: {e}")))?;
    let not_table = || Invalid(format!("invalid {CONFIG_FILE}: [projects] must be tables"));
    let projects = doc
        .entry("projects")
        .or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            toml_edit::Item::Table(t)
        })
        .as_table_mut()
        .ok_or_else(not_table)?;
    let repos = projects
        .entry(name)
        .or_insert_with(toml_edit::table)
        .as_table_mut()
        .ok_or_else(not_table)?
        .entry("repos")
        .or_insert_with(|| toml_edit::value(toml_edit::Array::new()))
        .as_array_mut()
        .ok_or_else(not_table)?;
    if repos.iter().any(|r| r.as_str() == Some(repo.as_str())) {
        return Ok(None);
    }
    if !repos.is_empty() {
        return Err(bad(&format!("project {name} already maps other repos")).into());
    }
    repos.push(repo.as_str());
    Ok(Some(ConfigChange {
        before,
        after: doc.to_string(),
    }))
}

/// Fails when `.fbrain.toml` is not what planning read, so a hand edit is never overwritten.
fn ensure_config_unchanged(brain: &Brain, seen: &Option<String>) -> Result<()> {
    if read_note(brain, CONFIG_FILE)? != *seen {
        bail!("{CONFIG_FILE} changed since the proposal was planned; run the review again");
    }
    Ok(())
}

/// The frontmatter keys a proposal may seed on a new note: `description` (cut to 200
/// characters) and `projects` (at most 8 valid names, including the target's own).
fn seed_keys<'a>(p: &'a Proposal, target: &str) -> Result<Vec<(&'a str, FmValue)>> {
    let mut out = Vec::new();
    for (k, v) in &p.input.extra {
        match (k.as_str(), v) {
            ("description", FmValue::Str(d)) => {
                out.push(("description", FmValue::Str(d.chars().take(200).collect())));
            }
            ("projects", FmValue::List(l)) => {
                let own = brief_project(target).or_else(|| {
                    target
                        .strip_prefix("projects/")
                        .and_then(|t| t.split('/').next())
                });
                if l.len() > 8 || !l.iter().all(|n| valid_project_name(n)) {
                    return Err(
                        Invalid("invalid projects: at most 8 valid project names".into()).into(),
                    );
                }
                if own.is_none_or(|o| !l.iter().any(|n| n == o)) {
                    return Err(Invalid(
                        "invalid projects: must include the target's own project".into(),
                    )
                    .into());
                }
                out.push(("projects", v.clone()));
            }
            _ => {}
        }
    }
    Ok(out)
}

fn s(v: &str) -> FmValue {
    FmValue::Str(v.to_string())
}

struct Clock {
    today: String,
    approved_at: String,
}

fn clock() -> Clock {
    let t = proposals::now_utc();
    let today = format!("{}-{}-{}", &t[0..4], &t[4..6], &t[6..8]);
    Clock {
        approved_at: format!("{today}T{}:{}:{}Z", &t[9..11], &t[11..13], &t[13..15]),
        today,
    }
}

fn read_note(brain: &Brain, rel: &str) -> Result<Option<String>> {
    let file = brain.path.join(rel);
    match std::fs::read_to_string(&file) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", file.display())),
    }
}

fn temp_name(name: &str) -> Result<String> {
    let mut rnd = [0u8; 8];
    getrandom::fill(&mut rnd).map_err(|e| anyhow::anyhow!("no random bytes: {e}"))?;
    Ok(format!(
        "{name}.{}.fbrain-tmp",
        rnd.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

/// Runs `rename` until it works, retrying 5 times over 1 s: Obsidian on Windows may hold the file.
fn rename_with_retry(mut rename: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    let mut res = rename();
    for _ in 0..5 {
        if res.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        res = rename();
    }
    res
}

/// Creates the missing directories above `rel` and returns the ones it made. On Unix no step
/// follows a symlink. Residual risk off Unix: the path was checked just before, and a folder
/// swapped for a link in between is not detected (no directory-descriptor API in std).
#[cfg(unix)]
fn make_parents(root: &Path, rel: &str) -> std::io::Result<Vec<String>> {
    nofollow::mkdir_all(root, rel.rsplit_once('/').map_or("", |(d, _)| d))
}

#[cfg(not(unix))]
fn make_parents(root: &Path, rel: &str) -> std::io::Result<Vec<String>> {
    let (mut made, mut path) = (Vec::new(), String::new());
    for comp in rel
        .rsplit_once('/')
        .map_or("", |(d, _)| d)
        .split('/')
        .filter(|c| !c.is_empty())
    {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(comp);
        if !root.join(&path).exists() {
            std::fs::create_dir(root.join(&path))?;
            made.push(path.clone());
        }
    }
    Ok(made)
}

#[cfg(unix)]
fn remove_dir(root: &Path, rel: &str) -> std::io::Result<()> {
    nofollow::rmdir(root, rel)
}

#[cfg(not(unix))]
fn remove_dir(root: &Path, rel: &str) -> std::io::Result<()> {
    std::fs::remove_dir(root.join(rel))
}

/// Directory-relative file operations that never follow a symlink, so a folder swapped for a link
/// between the path check and the write cannot send the write outside the brain.
#[cfg(unix)]
mod nofollow {
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn check(r: i32) -> io::Result<i32> {
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(r)
        }
    }

    /// The directory that holds `rel`, opened one component at a time with `O_NOFOLLOW`, and the
    /// file name.
    pub fn open_parent(root: &Path, rel: &str) -> io::Result<(OwnedFd, String)> {
        let (dirs, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        let root_c = CString::new(root.as_os_str().as_bytes())?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        // SAFETY: the pointers are valid C strings for the duration of each call, and a
        // successful call returns a descriptor this function owns.
        let mut fd = unsafe { OwnedFd::from_raw_fd(check(libc::open(root_c.as_ptr(), flags))?) };
        for comp in dirs.split('/').filter(|c| !c.is_empty()) {
            let c = CString::new(comp)?;
            let next = check(unsafe {
                libc::openat(fd.as_raw_fd(), c.as_ptr(), flags | libc::O_NOFOLLOW)
            })?;
            fd = unsafe { OwnedFd::from_raw_fd(next) };
        }
        Ok((fd, name.to_string()))
    }

    /// Creates the missing directories of `dirs` below `root`, one component at a time and never
    /// through a symlink. Returns the directories it created, outermost first.
    pub fn mkdir_all(root: &Path, dirs: &str) -> io::Result<Vec<String>> {
        // The root is the brain's configured path, which may itself be reached through a link
        // (a brain folder that is a symlink); it is resolved once. Every component below it is
        // opened with O_NOFOLLOW.
        let root = root.canonicalize()?;
        let root_c = CString::new(root.as_os_str().as_bytes())?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        // SAFETY: as in `open_parent`.
        let mut fd = unsafe { OwnedFd::from_raw_fd(check(libc::open(root_c.as_ptr(), flags))?) };
        let flags = flags | libc::O_NOFOLLOW;
        let (mut made, mut path) = (Vec::new(), String::new());
        for comp in dirs.split('/').filter(|c| !c.is_empty()) {
            let c = CString::new(comp)?;
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(comp);
            let mut next = unsafe { libc::openat(fd.as_raw_fd(), c.as_ptr(), flags) };
            if next < 0 && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
                check(unsafe { libc::mkdirat(fd.as_raw_fd(), c.as_ptr(), 0o777) })?;
                made.push(path.clone());
                next = unsafe { libc::openat(fd.as_raw_fd(), c.as_ptr(), flags) };
            }
            fd = unsafe { OwnedFd::from_raw_fd(check(next)?) };
        }
        Ok(made)
    }

    /// Removes an empty directory below `root`, through no symlink.
    pub fn rmdir(root: &Path, rel: &str) -> io::Result<()> {
        let (dir, name) = open_parent(root, rel)?;
        let c = CString::new(name)?;
        check(unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR) }).map(drop)
    }

    /// The permission bits of an existing regular file; an existing symlink is an error.
    pub fn existing_mode(dir: &OwnedFd, name: &str) -> io::Result<Option<u32>> {
        let c = CString::new(name)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                c.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if r < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(e)
            };
        }
        match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => Ok(Some(st.st_mode as u32 & 0o7777)),
            libc::S_IFLNK => Err(io::Error::new(io::ErrorKind::InvalidInput, "is a symlink")),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "is not a regular file",
            )),
        }
    }

    pub fn create_new(dir: &OwnedFd, name: &str, mode: u32) -> io::Result<File> {
        let c = CString::new(name)?;
        let flags =
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let fd = check(unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags, mode) })?;
        Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn rename(dir: &OwnedFd, from: &str, to: &str) -> io::Result<()> {
        let (f, t) = (CString::new(from)?, CString::new(to)?);
        let fd = dir.as_raw_fd();
        check(unsafe { libc::renameat(fd, f.as_ptr(), fd, t.as_ptr()) }).map(drop)
    }

    pub fn unlink(dir: &OwnedFd, name: &str) -> io::Result<()> {
        let c = CString::new(name)?;
        check(unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) }).map(drop)
    }
}

/// Mode preservation is best effort: filesystems such as WSL drvfs answer EPERM or ENOTSUP to a
/// permission change. Only those refusals are skipped; any other error still fails the write.
pub(crate) fn best_effort_perms(res: std::io::Result<()>) -> std::io::Result<()> {
    use std::io::ErrorKind::{PermissionDenied, Unsupported};
    match res {
        Err(e) if matches!(e.kind(), PermissionDenied | Unsupported) => Ok(()),
        // macOS reports ENOTSUP (45), which std does not map to Unsupported.
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => Ok(()),
        other => other,
    }
}

/// Writes `bytes` to a temp file beside the note and renames it into place. On Unix every step
/// goes through a directory descriptor without following symlinks.
#[cfg(unix)]
pub(crate) fn write_atomic(root: &Path, rel: &str, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let fail = |e: &dyn std::fmt::Display| anyhow::anyhow!("could not write {rel}: {e}");
    let (dir, name) = nofollow::open_parent(root, rel).map_err(|e| fail(&e))?;
    let tmp = temp_name(&name).map_err(|e| fail(&e))?;
    let mode = nofollow::existing_mode(&dir, &name)
        .map_err(|e| fail(&e))?
        .unwrap_or(0o644);
    let mut f = nofollow::create_new(&dir, &tmp, mode).map_err(|e| fail(&e))?;
    let written = f
        .write_all(bytes)
        .and_then(|_| f.sync_all())
        .and_then(|_| best_effort_perms(f.set_permissions(std::fs::Permissions::from_mode(mode))));
    drop(f);
    let res = written.and_then(|_| rename_with_retry(|| nofollow::rename(&dir, &tmp, &name)));
    if let Err(e) = res {
        let _ = nofollow::unlink(&dir, &tmp);
        return Err(fail(&e));
    }
    Ok(())
}

/// Residual risk off Unix: the path is checked before the write, and a folder swapped for a link
/// between that check and the rename is not detected (no directory-descriptor API in std).
#[cfg(not(unix))]
pub(crate) fn write_atomic(root: &Path, rel: &str, bytes: &[u8]) -> Result<()> {
    let fail = |e: &dyn std::fmt::Display| anyhow::anyhow!("could not write {rel}: {e}");
    let file = root.join(rel);
    let name = file
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let tmp = file.with_file_name(temp_name(&name).map_err(|e| fail(&e))?);
    // create_new never follows a planted link or reuses a file.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| fail(&e))?;
    let mut res = f.write_all(bytes).and_then(|_| f.sync_all());
    if res.is_ok() {
        if let Ok(meta) = std::fs::metadata(&file) {
            res = best_effort_perms(f.set_permissions(meta.permissions()));
        }
    }
    drop(f);
    let res = res.and_then(|_| rename_with_retry(|| std::fs::rename(&tmp, &file)));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(fail(&e));
    }
    Ok(())
}

#[cfg(unix)]
fn remove_note(root: &Path, rel: &str) -> std::io::Result<()> {
    let (dir, name) = nofollow::open_parent(root, rel)?;
    nofollow::unlink(&dir, &name)
}

#[cfg(not(unix))]
fn remove_note(root: &Path, rel: &str) -> std::io::Result<()> {
    std::fs::remove_file(root.join(rel))
}

/// Writes every note, or none: an earlier write is put back when a later one fails. A note that
/// cannot be put back is named in the error.
pub(crate) fn write_all(brain: &Brain, writes: &[(String, String)]) -> Result<()> {
    let mut done: Vec<(String, Option<String>)> = Vec::new();
    let mut made: Vec<String> = Vec::new();
    for (rel, text) in writes {
        let result = (|| {
            // Checked again right before the write: the tree may have changed since planning.
            // The brain config is the one fixed non-note path review writes.
            if rel != CONFIG_FILE {
                proposals::note_target(brain, rel, "target")?;
            }
            let file = brain.path.join(rel);
            made.extend(
                make_parents(&brain.path, rel)
                    .with_context(|| format!("cannot create the folder of {rel}"))?,
            );
            if std::fs::symlink_metadata(&file).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(Invalid(format!("invalid target: {rel} is a symlink")).into());
            }
            let before = read_note(brain, rel)?;
            write_atomic(&brain.path, rel, text.as_bytes())?;
            done.push((rel.clone(), before));
            Ok(())
        })();
        if let Err(e) = result {
            let mut stuck = Vec::new();
            for (rel, before) in done.into_iter().rev() {
                let put_back = match before {
                    Some(t) => write_atomic(&brain.path, &rel, t.as_bytes()),
                    None => remove_note(&brain.path, &rel).map_err(Into::into),
                };
                if let Err(re) = put_back {
                    stuck.push(format!("{rel} ({re:#})"));
                }
            }
            for dir in made.iter().rev() {
                // Left in place when something else was put in it meanwhile.
                let _ = remove_dir(&brain.path, dir);
            }
            if stuck.is_empty() {
                return Err(e);
            }
            return Err(anyhow::anyhow!(
                "{e:#}; could not restore, left changed: {}",
                stuck.join(", ")
            ));
        }
    }
    Ok(())
}

fn stamps<'a>(p: &'a Proposal, c: &'a Clock) -> Vec<(&'static str, FmValue)> {
    vec![
        ("proposed_by", s(&p.input.proposed_by)),
        ("source", s(&p.input.source)),
        ("approved_at", s(&c.approved_at)),
    ]
}

/// The new text of the proposal's target: a new note, or the existing one with its body replaced.
fn target_text(brain: &Brain, p: &Proposal, rel: &str, body: &str, c: &Clock) -> Result<String> {
    Ok(match read_note(brain, rel)? {
        None => {
            // Two proposal keys may seed a new note's frontmatter; the proposal body cannot.
            let mut set = seed_keys(p, rel)?;
            set.extend([("status", s("active")), ("valid_from", s(&c.today))]);
            set.extend(stamps(p, c));
            edit_frontmatter(&with_eol(body, "\n"), &set)
        }
        Some(old) => {
            let status = match p.input.extra.iter().find(|(k, _)| k == "set_status") {
                Some((_, FmValue::Str(v))) if STATUSES.contains(&v.as_str()) => Some(v),
                Some(_) => {
                    return Err(Invalid(format!(
                        "invalid set_status: use one of {}",
                        STATUSES.join(", ")
                    ))
                    .into())
                }
                None => None,
            };
            // An empty body with set_status changes the status only.
            let text = if status.is_some() && body.trim().is_empty() {
                old
            } else {
                let (head, _) = split_head(&old);
                format!("{head}{}", with_eol(body, eol_of(&old)))
            };
            let mut set = stamps(p, c);
            set.extend(status.map(|v| ("status", s(v))));
            edit_frontmatter(&text, &set)
        }
    })
}

fn superseded_text(old: &str, new_rel: &str, c: &Clock) -> String {
    let link = format!("[[{}]]", new_rel.strip_suffix(".md").unwrap_or(new_rel));
    edit_frontmatter(
        old,
        &[
            ("status", s("superseded")),
            ("valid_to", s(&c.today)),
            ("superseded_by", s(&link)),
        ],
    )
}

fn merged_text(old: &str, p: &Proposal, body: &str, c: &Clock) -> String {
    let eol = eol_of(old);
    let body = body.trim_end_matches(['\n', '\r']);
    let section = with_eol(&format!("\n## Merged {}\n\n{body}\n", c.today), eol);
    let mut text = old.to_string();
    if !has_merged(&text, body, eol) {
        if !text.ends_with('\n') {
            text.push_str(eol);
        }
        text.push_str(&section);
    }
    edit_frontmatter(&text, &stamps(p, c))
}

/// Whether the note already ends with a `## Merged <date>` section holding exactly this body,
/// whatever its date: a retry after a failed commit finds the section it appended last, even
/// when midnight passed. An earlier section that merely starts with the same text does not count.
fn has_merged(text: &str, body: &str, eol: &str) -> bool {
    let Some(i) = text.rfind(&format!("{eol}## Merged ")) else {
        return false;
    };
    text[i + eol.len()..]
        .split_once(eol)
        .is_some_and(|(_, after)| after == with_eol(&format!("\n{body}\n"), eol))
}

/// What a decision does: notes to write, the commit subject, and the notes whose protection counts.
struct Plan {
    verb: String,
    label: String,
    writes: Vec<(String, String)>,
    guard: Vec<String>,
    /// `.fbrain.toml` as planning read it, when the plan edits it.
    config_seen: Option<Option<String>>,
}

fn plan(brain: &Brain, p: &Proposal, decision: &Decision, c: &Clock) -> Result<(Plan, bool)> {
    let mut input = p.input.clone();
    let op_name = |op: Op| {
        serde_json::to_value(op)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
    };
    let (force, merge_into) = match decision {
        Decision::Approve { force } => (*force, None),
        Decision::EditThenApprove { body, force } => {
            input.body.clone_from(body);
            (*force, None)
        }
        Decision::SupersedeCandidate { rel_path, force } => {
            if !matches!(input.op, Op::Add | Op::Update | Op::Supersede) {
                return Err(Invalid(
                    "--supersede-candidate needs an add, update or supersede proposal".into(),
                )
                .into());
            }
            input.op = Op::Supersede;
            input.supersedes = Some(rel_path.clone());
            (*force, None)
        }
        Decision::MergeInto { rel_path, force } => (*force, Some(rel_path.as_str())),
        Decision::Reject => unreachable!("reject has no plan"),
    };
    let body = clean_body(&input.body);
    let mut guard = Vec::new();
    let plan = if let Some(raw) = merge_into {
        let target = proposals::note_target(brain, &input.target, "target")?;
        let rel = proposals::note_target(brain, raw, "merge target")?;
        let Some(old) = read_note(brain, &rel)? else {
            return Err(Invalid(format!("merge target not found: {rel}")).into());
        };
        guard.push(rel.clone());
        guard.push(target);
        Plan {
            verb: "merge".into(),
            label: rel.clone(),
            writes: vec![(rel, merged_text(&old, p, &body, c))],
            guard,
            config_seen: None,
        }
    } else {
        let input = proposals::check(brain, input)?;
        let target = input.target.clone();
        if input.op == Op::Invalidate {
            let old = read_note(brain, &target)?.context("target vanished")?;
            let text = edit_frontmatter(
                &old,
                &[("status", s("invalidated")), ("valid_to", s(&c.today))],
            );
            guard.push(target.clone());
            Plan {
                verb: "invalidate".into(),
                label: target.clone(),
                writes: vec![(target, text)],
                guard,
                config_seen: None,
            }
        } else {
            let mut writes = vec![(target.clone(), target_text(brain, p, &target, &body, c)?)];
            let mut config_seen = None;
            if matches!(input.op, Op::Add | Op::Update) {
                if let Some(ch) = map_repo_config(brain, p, &target)? {
                    config_seen = Some(ch.before);
                    writes.push((CONFIG_FILE.to_string(), ch.after));
                }
            }
            if read_note(brain, &target)?.is_some() {
                guard.push(target.clone());
            }
            if let Some(old_rel) = &input.supersedes {
                if *old_rel == target {
                    return Err(Invalid("supersedes must differ from target".into()).into());
                }
                let old = read_note(brain, old_rel)?.context("supersedes vanished")?;
                writes.push((old_rel.clone(), superseded_text(&old, &target, c)));
                guard.push(old_rel.clone());
            }
            Plan {
                verb: op_name(input.op).unwrap_or_default(),
                label: target,
                writes,
                guard,
                config_seen,
            }
        }
    };
    Ok((plan, force))
}

/// Notes the decision touches or that the proposal lists as conflicts, which are protected now.
/// The flag stored in the inbox file is not used: the file is not trusted.
fn first_protected(brain: &Brain, p: &Proposal, guard: &[String]) -> Result<Option<String>> {
    let listed = p
        .conflicts
        .iter()
        .filter_map(|c| proposals::note_target(brain, &c.rel_path, "conflict").ok());
    for rel in guard.iter().cloned().chain(listed) {
        if read_note(brain, &rel)?.is_some() && proposals::note_at(brain, &rel)?.is_protected() {
            return Ok(Some(rel));
        }
    }
    Ok(None)
}

/// Applies one decision to one proposal: writes, commits, then deletes the proposal file.
pub fn apply(brain: &Brain, proposal_id: &str, decision: Decision) -> Result<Applied> {
    // Before the first write: a git failure must not leave notes changed and uncommitted.
    let git_off = brain.git_unavailable()?;
    let p = match proposals::get(brain, proposal_id) {
        Ok(Some(p)) => p,
        Ok(None) => bail!("proposal not found: {}", escape(proposal_id)),
        // An inbox file that cannot be read as a proposal can still be rejected.
        Err(e)
            if matches!(decision, Decision::Reject)
                && proposals::inbox_file_exists(brain, proposal_id) =>
        {
            eprintln!(
                "warning: rejecting an unreadable proposal: {}",
                escape_text(&format!("{e:#}"))
            );
            unreadable(proposal_id)
        }
        Err(e) => return Err(e),
    };
    let (verb, label, writes) = if matches!(decision, Decision::Reject) {
        ("reject".to_string(), escape(&p.input.target), Vec::new())
    } else {
        let c = clock();
        let (plan, force) = plan(brain, &p, &decision, &c)?;
        if !force {
            if let Some(rel) = first_protected(brain, &p, &plan.guard)? {
                return Err(Protected(rel).into());
            }
        }
        if let Some(seen) = &plan.config_seen {
            ensure_config_unchanged(brain, seen)?;
        }
        write_all(brain, &plan.writes)?;
        (plan.verb, escape(&plan.label), plan.writes)
    };
    let changed: Vec<String> = writes.into_iter().map(|(rel, _)| rel).collect();
    record(
        brain,
        &p,
        &format!("fbrain: {verb} {label} ({})", p.id),
        &changed,
        git_off,
    )
}

fn unreadable(id: &str) -> Proposal {
    Proposal {
        id: id.to_string(),
        rel_path: format!("inbox/{id}.md"),
        input: proposals::ProposalInput {
            op: Op::Add,
            target: "(unreadable proposal)".into(),
            supersedes: None,
            body: String::new(),
            proposed_by: String::new(),
            source: String::new(),
            rationale: String::new(),
            extra: Vec::new(),
        },
        conflicts: Vec::new(),
        conflicts_protected: false,
    }
}

/// Commits what changed together with the proposal's deletion, then removes the proposal file.
/// A brain that is not a git repository still gets the changes; the commit is skipped with a
/// warning. Any other git failure is an error that says what was written and staged, and the
/// proposal file stays.
fn record(
    brain: &Brain,
    p: &Proposal,
    subject: &str,
    changed: &[String],
    git_off: Option<String>,
) -> Result<Applied> {
    let written = if changed.is_empty() {
        "no note was changed".to_string()
    } else {
        format!("notes written: {}", changed.join(", "))
    };
    let incomplete = |detail: String, commit: Option<String>| Incomplete {
        changed: changed.to_vec(),
        commit,
        detail,
    };
    let remove_proposal = |commit: Option<String>| -> Result<()> {
        match std::fs::remove_file(brain.path.join(&p.rel_path)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                let done = commit
                    .as_ref()
                    .map_or(String::new(), |c| format!(", commit {c}"));
                Err(incomplete(
                    format!(
                        "decision applied but {} could not be removed: {e}; {written}{done}",
                        p.rel_path
                    ),
                    commit,
                )
                .into())
            }
        }
    };
    let mut warning = None;
    let mut commit = None;
    if let Some(why) = git_off {
        if !changed.is_empty() {
            eprintln!("warning: not committed: {why}");
            warning = Some(format!("not committed: {why}"));
        }
        remove_proposal(None)?;
    } else {
        let git_failed = |e: anyhow::Error, tail: String| {
            incomplete(
                format!("not committed: {e:#}; {written}; {} {tail}", p.rel_path),
                None,
            )
        };
        let tracked = brain
            .git_tracked(&p.rel_path)
            .map_err(|e| git_failed(e, "was kept".into()))?;
        if !changed.is_empty() {
            brain
                .git_add(changed)
                .map_err(|e| git_failed(e, "was kept; the notes may be staged".into()))?;
        }
        let mut paths = changed.to_vec();
        if tracked {
            brain
                .git_rm(&p.rel_path)
                .map_err(|e| git_failed(e, "was kept; the notes are staged".into()))?;
            paths.push(p.rel_path.clone());
        }
        if !paths.is_empty() {
            match brain.git_commit(subject, &paths) {
                Ok(sha) => commit = Some(sha),
                Err(e) => {
                    let tail = match tracked.then(|| brain.git_restore(&p.rel_path)) {
                        Some(Ok(())) | None => "was kept; the notes are staged".to_string(),
                        Some(Err(r)) => format!(
                            "is deleted and the deletion is staged (restoring it failed: {r:#})"
                        ),
                    };
                    return Err(git_failed(e, tail).into());
                }
            }
        }
        remove_proposal(commit.clone())?;
    }
    Ok(Applied {
        commit,
        changed: changed.to_vec(),
        warning,
    })
}

/// Commits `changed` (and nothing else already staged) for a writer that has no proposal, such
/// as `lint --fix`. The caller has already ruled out a brain outside git.
pub(crate) fn commit_changes(brain: &Brain, subject: &str, changed: &[String]) -> Result<String> {
    let done = || format!("notes written: {}", changed.join(", "));
    brain
        .git_add(changed)
        .with_context(|| format!("not committed; {}; the notes may be staged", done()))?;
    brain
        .git_commit(subject, changed)
        .with_context(|| format!("not committed; {}; the notes are staged", done()))
}

/// Opens `$VISUAL` or `$EDITOR` on a temp copy of `body` and returns what the editor left.
pub fn edit_body(body: &str) -> Result<String> {
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .context("no editor: set VISUAL or EDITOR")?;
    let mut rnd = [0u8; 8];
    getrandom::fill(&mut rnd).map_err(|e| anyhow::anyhow!("no random bytes: {e}"))?;
    let tmp = std::env::temp_dir().join(format!(
        "fbrain-edit-{}.md",
        rnd.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| f.write_all(body.as_bytes()))
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    let mut parts = editor.split_whitespace();
    let status = std::process::Command::new(parts.next().unwrap_or_default())
        .args(parts)
        .arg(&tmp)
        .status();
    let edited = std::fs::read_to_string(&tmp);
    let _ = std::fs::remove_file(&tmp);
    let status = status.with_context(|| format!("cannot run the editor `{editor}`"))?;
    if !status.success() {
        bail!("the editor `{editor}` failed: {status}");
    }
    edited.context("cannot read the edited text")
}

/// Line diff of `old` to `new` (longest common subsequence), `+` and `-` prefixed.
fn diff(old: &str, new: &str) -> Vec<String> {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            out.push(format!("  {}", escape(a[i])));
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            out.push(format!("+ {}", escape(b[j])));
            j += 1;
        } else {
            out.push(format!("- {}", escape(a[i])));
            i += 1;
        }
    }
    out
}

/// A proposal as the reviewer sees it: its fields, the diff against the target, the candidates.
pub fn render(brain: &Brain, p: &Proposal) -> String {
    let op = serde_json::to_value(p.input.op)
        .ok()
        .and_then(|v| v.as_str().map(String::from));
    let mut out = format!(
        "== {}\n{} {}  (by {}, source {})\nrationale: {}\n",
        escape(&p.id),
        op.unwrap_or_default(),
        escape(&p.input.target),
        escape(&p.input.proposed_by),
        escape(&p.input.source),
        escape(&p.input.rationale),
    );
    if let Some(old) = &p.input.supersedes {
        out.push_str(&format!("supersedes: {}\n", escape(old)));
    }
    for (n, c) in p.conflicts.iter().enumerate() {
        out.push_str(&format!(
            "candidate {}: {} (score {:.2}{})\n",
            n + 1,
            escape(&c.rel_path),
            c.score,
            if c.protected { ", protected" } else { "" }
        ));
    }
    let current = match proposals::note_target(brain, &p.input.target, "target") {
        Err(e) => Err(format!("the target path is refused: {e:#}")),
        Ok(rel) => match read_note(brain, &rel) {
            Ok(text) => Ok(text.map(|t| Note::parse(&rel, &t).body)),
            Err(e) => Err(format!("cannot read the current target: {e:#}")),
        },
    };
    if p.input.op == Op::Invalidate {
        out.push_str("sets status invalidated on the target\n");
    } else {
        let old = match &current {
            Ok(old) => old.clone().unwrap_or_default(),
            Err(why) => {
                out.push_str(&format!(
                    "{} (no diff; the proposed text follows)\n",
                    escape_text(why)
                ));
                String::new()
            }
        };
        for l in diff(&old, &clean_body(&p.input.body)) {
            out.push_str(&l);
            out.push('\n');
        }
        if matches!(p.input.op, Op::Add | Op::Update) {
            match map_repo_config(brain, p, &p.input.target) {
                Ok(Some(ch)) => {
                    out.push_str(&format!("changes {CONFIG_FILE}:\n"));
                    for l in diff(&ch.before.unwrap_or_default(), &ch.after) {
                        out.push_str(&l);
                        out.push('\n');
                    }
                }
                Ok(None) => {}
                Err(e) => out.push_str(&format!(
                    "map_repo is refused: {}\n",
                    escape_text(&format!("{e:#}"))
                )),
            }
        }
    }
    out
}

fn ask(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

/// Walks the pending proposals, oldest first, and applies the reviewer's choices.
pub fn interactive(brain: &Brain) -> Result<()> {
    let (list, skipped) = proposals::list_with_skipped(brain)?;
    proposals::report_skipped(&skipped);
    if list.is_empty() {
        println!("no pending proposals");
    }
    'proposals: for p in list {
        print!("{}", render(brain, &p));
        loop {
            let Some(answer) = ask("[a]pprove [e]dit [m]erge [r]eject [s]kip [q]uit: ") else {
                return Ok(());
            };
            let decision = match answer.chars().next().map(|c| c.to_ascii_lowercase()) {
                Some('a') => Decision::Approve { force: false },
                Some('e') => match edit_body(&p.input.body) {
                    Ok(body) => Decision::EditThenApprove { body, force: false },
                    Err(e) => {
                        eprintln!("{}", escape_text(&format!("{e:#}")));
                        continue;
                    }
                },
                Some('m') => {
                    let pick =
                        ask("merge into candidate number or note path: ").unwrap_or_default();
                    let rel_path = match pick.parse::<usize>() {
                        Ok(n) if n >= 1 && n <= p.conflicts.len() => {
                            p.conflicts[n - 1].rel_path.clone()
                        }
                        _ if !pick.is_empty() => pick,
                        _ => continue,
                    };
                    Decision::MergeInto {
                        rel_path,
                        force: false,
                    }
                }
                Some('r') => Decision::Reject,
                Some('s') => continue 'proposals,
                Some('q') => return Ok(()),
                _ => continue,
            };
            let mut result = apply(brain, &p.id, decision.clone());
            if let Err(e) = &result {
                if e.is::<Protected>() {
                    let yes = ask(&format!(
                        "{}. Force it? [y/N]: ",
                        escape_text(&e.to_string())
                    ))
                    .is_some_and(|a| a.eq_ignore_ascii_case("y"));
                    if !yes {
                        continue;
                    }
                    result = apply(brain, &p.id, forced(decision));
                }
            }
            match result {
                Ok(a) => {
                    println!("applied {}", escape(&p.id));
                    if let Some(sha) = a.commit {
                        println!("committed {}", &sha[..sha.len().min(7)]);
                    }
                    continue 'proposals;
                }
                Err(e) => {
                    eprintln!("{}", escape_text(&format!("{e:#}")));
                    // Its notes were written: offering it again would apply it twice.
                    if e.is::<Incomplete>() {
                        continue 'proposals;
                    }
                }
            }
        }
    }
    Ok(())
}

fn forced(d: Decision) -> Decision {
    match d {
        Decision::Approve { .. } => Decision::Approve { force: true },
        Decision::EditThenApprove { body, .. } => Decision::EditThenApprove { body, force: true },
        Decision::MergeInto { rel_path, .. } => Decision::MergeInto {
            rel_path,
            force: true,
        },
        Decision::SupersedeCandidate { rel_path, .. } => Decision::SupersedeCandidate {
            rel_path,
            force: true,
        },
        Decision::Reject => Decision::Reject,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brain_with(config: &str) -> (tempfile::TempDir, Brain) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), config).unwrap();
        let brain = Brain::open(dir.path()).unwrap();
        (dir, brain)
    }

    fn brief_proposal(repo: &Path) -> Proposal {
        Proposal {
            id: "p".into(),
            rel_path: "inbox/p.md".into(),
            input: proposals::ProposalInput {
                op: Op::Add,
                target: "projects/shop/BRIEF.md".into(),
                supersedes: None,
                body: "# Shop\n".into(),
                proposed_by: "onboard".into(),
                source: "s".into(),
                rationale: "r".into(),
                extra: vec![("map_repo".into(), FmValue::Str(repo.display().to_string()))],
            },
            conflicts: Vec::new(),
            conflicts_protected: false,
        }
    }

    #[test]
    fn the_preview_shows_the_config_change_a_map_repo_would_make() {
        let (_d, brain) = brain_with("name = \"t\"\n");
        let repo = tempfile::tempdir().unwrap();
        let out = render(&brain, &brief_proposal(repo.path()));
        assert!(out.contains(".fbrain.toml"), "{out}");
        assert!(out.contains("[projects.shop]"), "{out}");
        assert!(
            out.contains(&repo.path().canonicalize().unwrap().display().to_string()),
            "{out}"
        );
    }

    #[test]
    fn a_config_edited_after_planning_is_refused() {
        let (d, brain) = brain_with("name = \"t\"\n");
        let seen = Some(std::fs::read_to_string(d.path().join(CONFIG_FILE)).unwrap());
        assert!(ensure_config_unchanged(&brain, &seen).is_ok());
        std::fs::write(d.path().join(CONFIG_FILE), "name = \"edited\"\n").unwrap();
        let e = ensure_config_unchanged(&brain, &seen).unwrap_err();
        assert!(format!("{e:#}").contains("changed since"), "{e:#}");
    }
}

#[cfg(all(test, unix))]
mod perm_tests {
    use super::best_effort_perms;
    use std::io::Error;

    #[test]
    fn refused_or_unsupported_permission_changes_are_skipped() {
        for raw in [libc::EPERM, libc::EACCES, libc::ENOTSUP, libc::EOPNOTSUPP] {
            assert!(
                best_effort_perms(Err(Error::from_raw_os_error(raw))).is_ok(),
                "{raw}"
            );
        }
        assert!(best_effort_perms(Ok(())).is_ok());
    }

    #[test]
    fn other_permission_step_errors_still_fail() {
        for raw in [libc::EIO, libc::ENOSPC, libc::EROFS, libc::EBADF] {
            assert!(
                best_effort_perms(Err(Error::from_raw_os_error(raw))).is_err(),
                "{raw}"
            );
        }
        assert!(best_effort_perms(Err(Error::other("x"))).is_err());
    }
}
