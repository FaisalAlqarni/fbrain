//! opencode 1.x and 2.x: a plugin file in the auto-loaded `plugins` folder, plus an optional
//! marked block in the global `AGENTS.md` for when plugins are off. `opencode.json` is never
//! touched for the plugin: registering the file there as well would load it twice. The one key
//! fbrain adds there is `mcp.fbrain`, and only with `--mcp`.

use super::{
    load, refuse_symlink, write_with_backup, EntryKind, Harness, InstallOpts, InstallRecord,
    WrittenEntry,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const MCP_POINTER: &str = "/mcp/fbrain";
const START: &str = "<!-- fbrain:start -->";
const END: &str = "<!-- fbrain:end -->";
const BLOCK_ID: &str = "fbrain";
/// Present in every plugin we write; an existing file without it is not ours and gets backed up.
const PLUGIN_MARK: &str = "// fbrain plugin for opencode";

const V1: &str = include_str!("assets/fbrain-opencode-v1.js");
const V2: &str = include_str!("assets/fbrain-opencode-v2.js");
const RULES: &str = include_str!("assets/rules.md");
const BLOCK_LINE: &str = include_str!("assets/agents-md-block.md");

/// `$XDG_CONFIG_HOME/opencode`, else `~/.config/opencode`.
pub fn config_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => dirs::home_dir()
            .context("cannot find the home folder")?
            .join(".config"),
    };
    Ok(base.join("opencode"))
}

pub fn plugin_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("plugins").join("fbrain.js"))
}

fn agents_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("AGENTS.md"))
}

/// The plugin source with the binary path embedded as a JSON string literal (a valid JS string).
fn plugin_source(h: Harness, binary: &Path) -> String {
    let template = if h == Harness::OpencodeV1 { V1 } else { V2 };
    let literal = serde_json::to_string(&binary.to_string_lossy()).expect("a string serializes");
    template.replace("__FBRAIN_BIN__", &literal)
}

/// The rules text plus the line that tells the agent to run `fbrain brief`.
fn block_body(h: Harness) -> String {
    let line = BLOCK_LINE.trim_end().replace("opencode-v1", h.as_str());
    format!("{}\n\n{line}", RULES.trim_end())
}

/// Writes the plugin (and, with `agents_md`, the block), replacing what `old` recorded.
pub fn install(
    h: Harness,
    binary: &Path,
    opts: &InstallOpts,
    old: Option<&InstallRecord>,
) -> Result<InstallRecord> {
    let agents_md = opts.agents_md;
    let plugin = plugin_path()?;
    let agents = agents_path()?;
    // Everything that can refuse happens before the first write.
    refuse_symlink(&plugin)?;
    let agents_text = if agents_md {
        refuse_symlink(&agents)?;
        Some(
            with_block(&read_or_empty(&agents)?, &block_body(h))
                .with_context(|| agents.display().to_string())?,
        )
    } else {
        None
    };
    let json_file = json_path()?;
    let old_mcp = old.and_then(|r| super::mcp_entry(r)).cloned();
    // A config dir that moved since the last install: clean the old one. The other major shares
    // our plugin path, which is overwritten below, so only its other entries are removed here.
    if let Some(old) = old {
        let stale: Vec<WrittenEntry> = old
            .files
            .iter()
            .filter(|e| e.path != plugin && e.path != agents && e.path != json_file)
            .cloned()
            .collect();
        remove(&stale)?;
    }
    // An install without --mcp takes out the registration an earlier one made.
    if let (false, Some(e)) = (opts.mcp, &old_mcp) {
        remove(std::slice::from_ref(e))?;
    }
    // Check the JSON before anything is written, so a bad file leaves no half install.
    let mcp_doc = if opts.mcp {
        refuse_symlink(&json_file)?;
        let doc = load(&json_file)?;
        check_mcp(&doc, &json_file, binary, old_mcp.is_some())?;
        Some(doc)
    } else {
        None
    };
    if !agents_md {
        if let Some(old) = old {
            let had: Vec<WrittenEntry> = old
                .files
                .iter()
                .filter(|e| matches!(e.kind, EntryKind::MarkedBlock(_)) && e.path == agents)
                .cloned()
                .collect();
            remove(&had)?;
        }
    }

    let source = plugin_source(h, binary);
    let foreign = std::fs::read_to_string(&plugin).is_ok_and(|t| !t.contains(PLUGIN_MARK));
    if foreign {
        write_with_backup(&plugin, &source)?;
    } else {
        let dir = plugin.parent().context("plugin path has no folder")?;
        std::fs::create_dir_all(dir)?;
        crate::review::write_atomic(dir, "fbrain.js", source.as_bytes())?;
    }
    let mut files = vec![WrittenEntry {
        path: plugin,
        kind: EntryKind::File,
        command: None,
    }];
    if let Some(text) = agents_text {
        write_with_backup(&agents, &text)?;
        files.push(WrittenEntry {
            path: agents,
            kind: EntryKind::MarkedBlock(BLOCK_ID.into()),
            command: None,
        });
    }
    if let Some(doc) = mcp_doc {
        files.push(add_mcp(&json_file, doc, binary)?);
    }
    Ok(InstallRecord {
        harness: h,
        binary: binary.to_path_buf(),
        files,
    })
}

pub fn json_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("opencode.json"))
}

fn mcp_value(binary: &Path) -> Value {
    json!({"type": "local", "command": [binary.to_string_lossy(), "mcp"]})
}

/// An `mcp.fbrain` that fbrain did not write (unrecorded and different) is left alone.
fn check_mcp(doc: &Value, path: &Path, binary: &Path, recorded: bool) -> Result<()> {
    if doc
        .pointer(MCP_POINTER)
        .is_some_and(|v| *v != mcp_value(binary))
        && !recorded
    {
        bail!(
            "{} already has mcp.fbrain, which fbrain did not write; remove it or edit it by hand",
            path.display()
        );
    }
    Ok(())
}

/// Adds `mcp.fbrain`, unless it is already there as written.
fn add_mcp(path: &Path, mut doc: Value, binary: &Path) -> Result<WrittenEntry> {
    let command = binary.to_string_lossy().into_owned();
    let want = mcp_value(binary);
    if doc.pointer(MCP_POINTER) != Some(&want) {
        let mcp = doc
            .as_object_mut()
            .expect("checked in load")
            .entry("mcp")
            .or_insert_with(|| json!({}));
        let Some(mcp) = mcp.as_object_mut() else {
            bail!(
                "cannot parse {}: mcp is not an object; not modified",
                path.display()
            );
        };
        mcp.insert("fbrain".into(), want);
        write_with_backup(path, &(serde_json::to_string_pretty(&doc)? + "\n"))?;
    }
    Ok(WrittenEntry {
        path: path.to_path_buf(),
        kind: EntryKind::JsonPointer(MCP_POINTER.into()),
        command: Some(command),
    })
}

/// Removes `mcp.fbrain` (and `mcp` when that leaves it empty) if it still runs what was written.
fn remove_mcp(e: &WrittenEntry) -> Result<()> {
    if !e.path.exists() {
        return Ok(());
    }
    let mut doc = load(&e.path)?;
    let ours = doc.pointer(MCP_POINTER).is_some_and(|v| {
        v["command"][0].as_str() == e.command.as_deref() && v["command"][1] == "mcp"
    });
    if !ours {
        if doc.pointer(MCP_POINTER).is_some() {
            eprintln!(
                "left mcp.fbrain in {}: it was changed since install",
                e.path.display()
            );
        }
        return Ok(());
    }
    let mcp = doc["mcp"].as_object_mut().expect("pointer found it");
    mcp.shift_remove("fbrain");
    if mcp.is_empty() {
        doc.as_object_mut()
            .expect("checked in load")
            .shift_remove("mcp");
    }
    write_with_backup(&e.path, &(serde_json::to_string_pretty(&doc)? + "\n"))
}

/// Removes exactly the recorded files and blocks. Anything already gone is fine. A recorded file
/// is deleted only if it is still a `plugins/fbrain.js` that carries our mark; any other file is
/// kept with a warning.
pub fn remove(entries: &[WrittenEntry]) -> Result<()> {
    for e in entries {
        match &e.kind {
            EntryKind::File => remove_plugin(&e.path)?,
            EntryKind::JsonPointer(p) if p == MCP_POINTER => remove_mcp(e)?,
            EntryKind::MarkedBlock(_) => {
                let text = read_or_empty(&e.path)?;
                let stripped =
                    without_block(&text).with_context(|| e.path.display().to_string())?;
                if stripped != text {
                    write_with_backup(&e.path, &stripped)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn remove_plugin(path: &Path) -> Result<()> {
    let keep = |why: &str| eprintln!("warning: not removing {}: {why}", path.display());
    let ours_by_name = path.file_name().is_some_and(|n| n == "fbrain.js")
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == "plugins");
    if !ours_by_name {
        keep("it is not a plugins/fbrain.js file");
        return Ok(());
    }
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        Ok(m) if m.file_type().is_symlink() => {
            keep("it is a symbolic link");
            return Ok(());
        }
        Ok(_) => {}
    }
    if !std::fs::read_to_string(path).is_ok_and(|t| t.contains(PLUGIN_MARK)) {
        keep("it is no longer the plugin fbrain wrote");
        return Ok(());
    }
    std::fs::remove_file(path).with_context(|| format!("cannot remove {}", path.display()))
}

/// `missing <path>` for each recorded file or block that is no longer there (or a block that no
/// longer parses as exactly one).
pub fn missing(rec: &InstallRecord) -> Vec<String> {
    rec.files
        .iter()
        .filter_map(|e| {
            let p = e.path.display();
            match &e.kind {
                EntryKind::File if !e.path.exists() => Some(format!("missing {p}")),
                // A plugin from before the marker existed: reinstalling adds it.
                EntryKind::File => std::fs::read_to_string(&e.path)
                    .is_ok_and(|t| !t.contains("FBRAIN_HARNESS"))
                    .then(|| format!("{p} lacks the agent marker")),
                EntryKind::MarkedBlock(_) => {
                    match has_block(&read_or_empty(&e.path).unwrap_or_default()) {
                        Ok(true) => None,
                        Ok(false) => Some(format!("missing {p}")),
                        Err(err) => Some(format!("{p}: {err}")),
                    }
                }
                EntryKind::JsonPointer(p) if p == MCP_POINTER => (!load(&e.path)
                    .is_ok_and(|d| d.pointer(MCP_POINTER).is_some()))
                .then(|| format!("missing mcp.fbrain in {}", e.path.display())),
                _ => None,
            }
        })
        .collect()
}

/// The major version `opencode --version` reports, if it runs and prints one.
pub fn reported_major() -> Option<(u32, String)> {
    let out = std::process::Command::new("opencode")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let digits: String = text
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some((digits.parse().ok()?, text))
}

fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Where the one block sits: byte offsets of its first marker line and the end of its last, or
/// `None` when there is none. Markers count only as whole lines. Anything but zero or one
/// well-formed block is an error, so no caller edits a file it cannot read exactly.
fn find_block(text: &str) -> Result<Option<(usize, usize)>> {
    let (mut starts, mut ends) = (Vec::new(), Vec::new());
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        match line.trim_end_matches(['\r', '\n']) {
            START => starts.push((at, at + line.len())),
            END => ends.push((at, at + line.len())),
            _ => {}
        }
        at += line.len();
    }
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([s], [e]) if s.0 < e.0 => Ok(Some((s.0, e.1))),
        _ => anyhow::bail!(
            "expected zero or one block between {START} and {END} on lines of their own, found {} start and {} end markers in a wrong order or number; fix the file by hand, it is not modified",
            starts.len(),
            ends.len()
        ),
    }
}

fn has_block(text: &str) -> Result<bool> {
    Ok(find_block(text)?.is_some())
}

/// `text` with exactly one marked block at the end: an earlier block is replaced, not repeated.
/// A text that lacks its final newline gets one before the block and none after it, which is how
/// `without_block` knows to take that newline out again.
fn with_block(text: &str, body: &str) -> Result<String> {
    let mut out = without_block(text)?;
    let unterminated = !out.is_empty() && !out.ends_with('\n');
    if unterminated {
        out.push('\n');
    }
    out.push_str(&format!("{START}\n{body}\n{END}"));
    if !unterminated {
        out.push('\n');
    }
    Ok(out)
}

/// `text` without its block, byte for byte as it was before `with_block`.
fn without_block(text: &str) -> Result<String> {
    let Some((s, e)) = find_block(text)? else {
        return Ok(text.to_string());
    };
    let mut head = &text[..s];
    if e == text.len() && !text.ends_with('\n') {
        // The block ends the file with no newline: install added one in front of it.
        head = head.strip_suffix('\n').unwrap_or(head);
    }
    Ok(format!("{head}{}", &text[e..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binary_path_is_embedded_as_an_escaped_string_literal() {
        let src = plugin_source(Harness::OpencodeV1, Path::new("/opt/a\"b'c\\d\nx/fbrain"));
        assert!(
            src.contains(r#"const FBRAIN = "/opt/a\"b'c\\d\nx/fbrain";"#),
            "{src}"
        );
        assert!(!src.contains("__FBRAIN_BIN__"));
    }

    #[test]
    fn a_block_goes_after_the_existing_text_once_and_comes_out_byte_for_byte() {
        for original in ["", "# Mine\n\nText.\n", "# Mine", "a\n\n", "a\r\nb\r\n"] {
            let once = with_block(original, "body").unwrap();
            assert!(
                once.starts_with(original.trim_end_matches('\n')),
                "{once:?}"
            );
            assert_eq!(once.matches(START).count(), 1, "{once:?}");
            assert_eq!(with_block(&once, "body").unwrap(), once, "{original:?}");
            assert_eq!(without_block(&once).unwrap(), original, "{original:?}");
        }
    }

    #[test]
    fn text_after_the_block_survives_its_removal() {
        let t = "a\n<!-- fbrain:start -->\nbody\n<!-- fbrain:end -->\nlater\n";
        assert_eq!(without_block(t).unwrap(), "a\nlater\n");
    }

    #[test]
    fn markers_inside_a_line_are_prose_not_markers() {
        let t = "Use <!-- fbrain:start --> and <!-- fbrain:end --> markers.\n";
        assert!(!has_block(t).unwrap());
        assert_eq!(without_block(t).unwrap(), t);
        let with = with_block(t, "body").unwrap();
        assert_eq!(without_block(&with).unwrap(), t);
    }

    #[test]
    fn a_marker_structure_that_is_not_zero_or_one_block_is_refused() {
        let end_first = "<!-- fbrain:end -->\nx\n<!-- fbrain:start -->\n";
        let two = format!("{0}{0}", "<!-- fbrain:start -->\nb\n<!-- fbrain:end -->\n");
        let nested = "<!-- fbrain:start -->\n<!-- fbrain:start -->\nb\n<!-- fbrain:end -->\n";
        let lone = "<!-- fbrain:start -->\nb\n";
        for t in [end_first, two.as_str(), nested, lone] {
            assert!(has_block(t).is_err(), "{t:?}");
            assert!(with_block(t, "b").is_err(), "{t:?}");
            assert!(without_block(t).is_err(), "{t:?}");
        }
    }

    #[test]
    fn the_block_names_the_harness_and_embeds_the_rules_once() {
        let b = block_body(Harness::OpencodeV2);
        assert!(b.contains("fbrain brief --harness opencode-v2 and read its output."));
        assert_eq!(b.matches("### fbrain rules").count(), 1);
    }
}
