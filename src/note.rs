//! Note parsing: frontmatter plus body. Parsing never fails; bad YAML is reported in the result.

use serde::{Deserialize, Serialize};
use serde_norway::Value as YamlValue;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Active,
    Done,
    Stale,
    Superseded,
    Invalidated,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Frontmatter {
    pub status: Status,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub superseded_by: Option<String>,
    pub protected: bool,
    pub private: bool,
    pub projects: Vec<String>,
    pub aliases: Vec<String>,
    pub source: Option<String>,
    pub proposed_by: Option<String>,
    pub approved_at: Option<String>,
    pub derived_from: Vec<String>,
    pub description: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, YamlValue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub rel_path: String,
    pub title: String,
    #[serde(rename = "frontmatter")]
    pub fm: Frontmatter,
    pub body: String,
    pub frontmatter_error: Option<String>,
}

impl Note {
    /// A note whose frontmatter failed to parse is treated as private: its real flags are unknown.
    pub fn is_private(&self) -> bool {
        self.fm.private || self.frontmatter_error.is_some()
    }

    /// A note whose frontmatter failed to parse is treated as protected: its real flags are unknown.
    pub fn is_protected(&self) -> bool {
        self.fm.protected || self.frontmatter_error.is_some()
    }

    pub fn parse(rel_path: &str, text: &str) -> Note {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let (yaml, body) = split_frontmatter(text);
        let (fm, frontmatter_error) = match yaml {
            None => (Frontmatter::default(), None),
            Some(y) if y.trim().is_empty() => (Frontmatter::default(), None),
            Some(y) => match serde_norway::from_str::<Frontmatter>(&y) {
                Ok(fm) => (fm, None),
                Err(e) => (Frontmatter::default(), Some(e.to_string())),
            },
        };
        let title = fm
            .extra
            .get("title")
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .or_else(|| {
                body.lines()
                    .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
            })
            .unwrap_or_else(|| stem(rel_path));
        Note {
            rel_path: rel_path.into(),
            title,
            fm,
            body,
            frontmatter_error,
        }
    }
}

pub fn stem(rel_path: &str) -> String {
    let file = rel_path.rsplit('/').next().unwrap_or(rel_path);
    file.strip_suffix(".md").unwrap_or(file).to_string()
}

/// Returns the YAML text (if a fenced block opens the file) and the body, both with LF endings.
pub(crate) fn split_frontmatter(text: &str) -> (Option<String>, String) {
    let lf = text.replace("\r\n", "\n");
    let Some(rest) = lf.strip_prefix("---\n") else {
        return (None, lf);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return (
                Some(rest[..offset].to_string()),
                rest[offset + line.len()..].to_string(),
            );
        }
        offset += line.len();
    }
    (None, lf)
}

/// A value `edit_frontmatter` can write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FmValue {
    Str(String),
    Bool(bool),
    List(Vec<String>),
}

/// A scalar YAML reads back as the same string: plain when it is plainly safe, else double-quoted.
fn scalar(s: &str) -> String {
    let plain = s.chars().next().is_some_and(char::is_alphanumeric)
        && !s.ends_with([' ', ':'])
        && !s.contains(": ")
        && !s.contains(" #")
        && s.chars()
            .all(|c| c.is_alphanumeric() || "-_./@+: ".contains(c))
        && !matches!(
            s.to_lowercase().as_str(),
            "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "y" | "n"
        );
    if plain {
        s.to_string()
    } else {
        serde_json::to_string(s).expect("a string always serializes")
    }
}

fn render(key: &str, v: &FmValue) -> String {
    let val = match v {
        FmValue::Str(s) => scalar(s),
        FmValue::Bool(b) => b.to_string(),
        FmValue::List(l) => format!(
            "[{}]",
            l.iter().map(|s| scalar(s)).collect::<Vec<_>>().join(", ")
        ),
    };
    format!("{key}: {val}")
}

/// Sets top-level frontmatter keys without touching any other byte: an existing key's line (and
/// its indented or `- ` continuation lines) is replaced in place, a new key is appended before the
/// closing `---`, and a file with no frontmatter gains a block at the top. New lines use the
/// file's own line ending.
pub fn edit_frontmatter(text: &str, set: &[(&str, FmValue)]) -> String {
    let mut out = text.to_string();
    for (key, value) in set {
        out = set_key(&out, key, &render(key, value));
    }
    out
}

fn set_key(text: &str, key: &str, line: &str) -> String {
    let bom = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let body = &text[bom..];
    let eol = match body.find('\n') {
        Some(i) if body[..i].ends_with('\r') => "\r\n",
        _ => "\n",
    };
    let opens = body.starts_with("---\n") || body.starts_with("---\r\n");
    let mut lines = Vec::new(); // (start, text with its line ending) of the lines inside the block
    let mut close = None;
    if opens {
        let mut off = bom + body.find('\n').expect("opening fence has a newline") + 1;
        for l in text[off..].split_inclusive('\n') {
            if l.trim_end() == "---" {
                close = Some(off);
                break;
            }
            lines.push((off, l));
            off += l.len();
        }
    }
    let Some(close) = close else {
        return format!(
            "{}---{eol}{line}{eol}---{eol}{}",
            &text[..bom],
            &text[bom..]
        );
    };
    let is_key = |l: &str| l.strip_prefix(key).is_some_and(|r| r.starts_with(':'));
    let Some(first) = lines.iter().position(|(_, l)| is_key(l)) else {
        return format!("{}{line}{eol}{}", &text[..close], &text[close..]);
    };
    let mut end = first + 1;
    while end < lines.len() && matches!(lines[end].1.chars().next(), Some(' ' | '\t' | '-')) {
        end += 1;
    }
    let to = lines.get(end).map_or(close, |l| l.0);
    format!("{}{line}{eol}{}", &text[..lines[first].0], &text[to..])
}
