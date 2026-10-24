//! Wikilinks: extraction, Obsidian-style resolution and the backlink table.
//!
//! The extraction and resolution rules are ported from obsidian-mind's `wikilinks.ts` (MIT).

use crate::note::Note;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

const ASSET_EXTENSIONS: [&str; 21] = [
    "png",
    "jpg",
    "jpeg",
    "gif",
    "svg",
    "webp",
    "pdf",
    "canvas",
    "base",
    "excalidraw",
    "html",
    "htm",
    "mp3",
    "mp4",
    "mov",
    "zip",
    "docx",
    "pptx",
    "xlsx",
    "doc",
    "ppt",
];

const NONE: &[String] = &[];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawLink {
    pub target: String,
    pub alias: Option<String>,
    pub heading: Option<String>,
    pub block: Option<String>,
    pub embed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Resolved(String),
    /// Only a frontmatter alias matched; Obsidian itself would not resolve this.
    ViaAlias(String),
    /// Several files match; sorted shortest path first, then lexicographic. The first one wins.
    Ambiguous(Vec<String>),
    Missing,
}

/// Blanks fenced blocks (an unclosed fence runs to the end) and inline code spans, keeping newlines.
fn strip_code(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut fence: Option<(char, usize)> = None;
    for line in body.split_inclusive('\n') {
        let t = line.trim_start();
        let marker = t.chars().next().filter(|c| *c == '`' || *c == '~');
        let run = marker.map_or(0, |m| t.chars().take_while(|c| *c == m).count());
        match fence {
            Some((m, n)) => {
                if marker == Some(m) && run >= n && t[run..].trim().is_empty() {
                    fence = None;
                }
            }
            None if run >= 3 => fence = marker.map(|m| (m, run)),
            None => {
                out.push_str(&strip_inline_code(line));
                continue;
            }
        }
        out.extend(line.chars().map(|c| if c == '\n' { '\n' } else { ' ' }));
    }
    out
}

fn strip_inline_code(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = chars.clone();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '`' {
            i += 1;
            continue;
        }
        let n = chars[i..].iter().take_while(|c| **c == '`').count();
        let mut j = i + n;
        let mut close = None;
        while j < chars.len() {
            if chars[j] == '`' {
                let m = chars[j..].iter().take_while(|c| **c == '`').count();
                if m == n {
                    close = Some(j + m);
                    break;
                }
                j += m;
            } else {
                j += 1;
            }
        }
        match close {
            Some(end) => {
                out[i..end].fill(' ');
                i = end;
            }
            None => i += n,
        }
    }
    out.into_iter().collect()
}

/// The links of a note: its body's, plus those in frontmatter `superseded_by` and `derived_from`.
pub fn note_links(note: &Note) -> Vec<RawLink> {
    let mut out = extract(&note.body);
    for text in note.fm.superseded_by.iter().chain(&note.fm.derived_from) {
        out.extend(extract(text));
    }
    out
}

/// Wikilinks of a body, with code regions ignored. Placeholders, same-file anchors and asset
/// targets are dropped.
pub fn extract(body: &str) -> Vec<RawLink> {
    extract_spans(body).into_iter().map(|(_, l)| l).collect()
}

/// Like `extract`, with the byte range of each `[[...]]` in `body`.
pub fn extract_spans(body: &str) -> Vec<(std::ops::Range<usize>, RawLink)> {
    let text = strip_code(body);
    let mut found = Vec::new();
    let mut rest = text.as_str();
    while let Some(open) = rest.find("[[") {
        let start = text.len() - rest.len() + open;
        let embed = rest[..open].ends_with('!');
        let after = &rest[open + 2..];
        let Some(close) = after.find("]]") else { break };
        let inner = &after[..close];
        if inner.contains('\n') || inner.contains("[[") {
            rest = after;
            continue;
        }
        rest = &after[close + 2..];
        if let Some(l) = parse_inner(inner, embed) {
            found.push((start..start + inner.len() + 4, l));
        }
    }
    if found.is_empty() {
        return found;
    }
    // `strip_code` swaps characters one for one, so character positions in `text` and `body`
    // match; map the byte offsets of `text` back onto `body`.
    let theirs: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let ours: Vec<usize> = body
        .char_indices()
        .map(|(i, _)| i)
        .chain([body.len()])
        .collect();
    let at = |b: usize| {
        ours[theirs
            .binary_search(&b)
            .expect("a span starts on a character")]
    };
    found
        .into_iter()
        .map(|(r, l)| (at(r.start)..at(r.end), l))
        .collect()
}

fn parse_inner(inner: &str, embed: bool) -> Option<RawLink> {
    let (main, alias) = match inner.split_once('|') {
        Some((m, a)) => (m.trim_end_matches('\\'), Some(a.trim().to_string())),
        None => (inner, None),
    };
    let (name, frag) = match main.split_once('#') {
        Some((n, f)) => (n, Some(f.trim())),
        None => (main, None),
    };
    let target = name.trim();
    let ext = target.rsplit_once('.').map(|(_, e)| e.to_lowercase());
    if target.is_empty()
        || target.contains("{{")
        || ext.is_some_and(|e| ASSET_EXTENSIONS.contains(&e.as_str()))
    {
        return None;
    }
    let (heading, block) = match frag {
        Some(f) => match f.strip_prefix('^') {
            Some(b) => (None, Some(b.to_string())),
            None => (Some(f.to_string()), None),
        },
        None => (None, None),
    };
    Some(RawLink {
        target: target.to_string(),
        alias,
        heading,
        block,
        embed,
    })
}

struct Entry {
    rel: String,
    /// Lowercase path without `.md`.
    key: String,
}

#[derive(Default)]
pub struct LinkGraph {
    entries: Vec<Entry>,
    raw: BTreeMap<String, Vec<RawLink>>,
    /// Entry indexes by the last path segment of `key`.
    by_base: HashMap<String, Vec<usize>>,
    /// Entry indexes by lowercase alias.
    by_alias: HashMap<String, Vec<usize>>,
    out: BTreeMap<String, Vec<String>>,
    back: BTreeMap<String, Vec<String>>,
}

fn key_of(path: &str) -> String {
    let p = path.trim_start_matches('/');
    let p = match p.len().checked_sub(3).and_then(|i| p.get(i..)) {
        Some(ext) if ext.eq_ignore_ascii_case(".md") => &p[..p.len() - 3],
        _ => p,
    };
    p.to_lowercase()
}

fn base_of(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

impl LinkGraph {
    pub fn build(notes: &[Note]) -> LinkGraph {
        LinkGraph::from_parts(
            notes
                .iter()
                .map(|n| (n.rel_path.as_str(), n.fm.aliases.clone(), note_links(n))),
        )
    }

    /// `(rel_path, aliases, links)` per note. The root `INDEX.md` is left out of the graph.
    pub fn from_parts<'a>(
        parts: impl Iterator<Item = (&'a str, Vec<String>, Vec<RawLink>)>,
    ) -> LinkGraph {
        let mut g = LinkGraph::default();
        for (rel, aliases, links) in parts {
            if rel == "INDEX.md" {
                continue;
            }
            let i = g.entries.len();
            let key = key_of(rel);
            g.by_base
                .entry(base_of(&key).to_string())
                .or_default()
                .push(i);
            for a in &aliases {
                g.by_alias
                    .entry(a.trim().to_lowercase())
                    .or_default()
                    .push(i);
            }
            g.entries.push(Entry {
                rel: rel.to_string(),
                key,
            });
            g.raw.insert(rel.to_string(), links);
        }
        for (rel, links) in &g.raw {
            let mut seen: Vec<String> = Vec::new();
            for l in links {
                let hit = match g.resolve(&l.target, rel) {
                    Resolution::Resolved(p) | Resolution::ViaAlias(p) => p,
                    Resolution::Ambiguous(c) => c[0].clone(),
                    Resolution::Missing => continue,
                };
                if hit != *rel && !seen.contains(&hit) {
                    seen.push(hit);
                }
            }
            for t in &seen {
                g.back.entry(t.clone()).or_default().push(rel.clone());
            }
            g.out.insert(rel.clone(), seen);
        }
        g
    }

    pub fn resolve(&self, target: &str, from_rel: &str) -> Resolution {
        let t = target.trim();
        let rel_form = t.starts_with("./") || t.starts_with("../");
        let key = if rel_form {
            let dir = from_rel.rsplit_once('/').map_or("", |(d, _)| d);
            match normalize(&format!("{dir}/{t}")) {
                Some(p) => key_of(&p),
                None => return Resolution::Missing,
            }
        } else {
            key_of(t)
        };
        let candidates = self
            .by_base
            .get(base_of(&key))
            .map_or(&[][..], Vec::as_slice);
        let mut found: Vec<&str> = candidates
            .iter()
            .map(|&i| &self.entries[i])
            .filter(|e| {
                if rel_form {
                    e.key == key
                } else if key.contains('/') {
                    e.key == key
                        || (e.key.ends_with(&key)
                            && e.key[..e.key.len() - key.len()].ends_with('/'))
                } else {
                    true
                }
            })
            .map(|e| e.rel.as_str())
            .collect();
        if found.is_empty() && !rel_form && !key.contains('/') {
            found = self
                .by_alias
                .get(&key)
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .map(|&i| self.entries[i].rel.as_str())
                .collect();
            sort_ties(&mut found);
            return match found.first() {
                Some(p) => Resolution::ViaAlias(p.to_string()),
                None => Resolution::Missing,
            };
        }
        sort_ties(&mut found);
        match found.len() {
            0 => Resolution::Missing,
            1 => Resolution::Resolved(found[0].to_string()),
            _ => Resolution::Ambiguous(found.into_iter().map(String::from).collect()),
        }
    }

    /// Notes this note links to, resolved, without duplicates, self-links or missing targets.
    pub fn outgoing(&self, rel: &str) -> &[String] {
        self.out.get(rel).map_or(NONE, Vec::as_slice)
    }

    /// Notes that link to this note, in path order.
    pub fn backlinks(&self, rel: &str) -> &[String] {
        self.back.get(rel).map_or(NONE, Vec::as_slice)
    }

    /// The links found in this note's body, in source order.
    pub fn links(&self, rel: &str) -> &[RawLink] {
        self.raw.get(rel).map_or(&[], Vec::as_slice)
    }
}

/// Shortest path first, then lexicographic.
fn sort_ties(v: &mut [&str]) {
    v.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
}

/// Collapses `.` and `..`; `None` when the path climbs out of the brain.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(body: &str) -> Vec<String> {
        extract(body).into_iter().map(|l| l.target).collect()
    }

    fn graph(files: &[(&str, &str)]) -> LinkGraph {
        let notes: Vec<Note> = files.iter().map(|(p, t)| Note::parse(p, t)).collect();
        LinkGraph::build(&notes)
    }

    #[test]
    fn suffixes_are_split_off_the_target() {
        let l = extract("[[A|shown]] [[B#Head]] [[C#^blk]] ![[D]]");
        assert_eq!(l[0].alias.as_deref(), Some("shown"));
        assert_eq!(l[1].heading.as_deref(), Some("Head"));
        assert_eq!(l[2].block.as_deref(), Some("blk"));
        assert!(l[3].embed && !l[0].embed);
        assert_eq!(
            targets("[[A|shown]] [[B#Head]] [[C#^blk]] ![[D]]"),
            ["A", "B", "C", "D"]
        );
    }

    #[test]
    fn table_escaped_pipe_splits_the_alias() {
        let l = extract(r"| [[A\|shown]] |");
        assert_eq!(l[0].target, "A");
        assert_eq!(l[0].alias.as_deref(), Some("shown"));
    }

    #[test]
    fn code_placeholders_assets_and_anchors_are_ignored() {
        let body = "`[[In]]` and ``[[Two]]``\n```rust\n[[Fenced]]\n```\n~~~\n[[Tilde]]\n~~~\n\
                    [[{{title}}]] [[pic.png]] [[#Same]] [[Real.md]] [[Open";
        assert_eq!(targets(body), ["Real.md"]);
        assert_eq!(targets("```\n[[Unclosed]]\n"), Vec::<String>::new());
    }

    #[test]
    fn resolution_follows_obsidian_rules() {
        let g = graph(&[
            ("Note.md", ""),
            ("a/sub/Pick.md", ""),
            ("b/Pick.md", ""),
            ("a/rel.md", ""),
            ("x/Dup.md", ""),
            ("x/y/Dup.md", ""),
            ("p/Mona.md", "---\naliases: [Consent]\n---\n"),
        ]);
        let r = |t: &str, from: &str| g.resolve(t, from);
        assert_eq!(r("note", "z.md"), Resolution::Resolved("Note.md".into()));
        assert_eq!(r("Note.md", "z.md"), Resolution::Resolved("Note.md".into()));
        assert_eq!(
            r("sub/Pick", "z.md"),
            Resolution::Resolved("a/sub/Pick.md".into())
        );
        assert_eq!(
            r("./sub/Pick", "a/rel.md"),
            Resolution::Resolved("a/sub/Pick.md".into())
        );
        assert_eq!(
            r("../Note", "a/rel.md"),
            Resolution::Resolved("Note.md".into())
        );
        assert_eq!(
            r("Dup", "z.md"),
            Resolution::Ambiguous(vec!["x/Dup.md".into(), "x/y/Dup.md".into()])
        );
        assert_eq!(
            r("consent", "z.md"),
            Resolution::ViaAlias("p/Mona.md".into())
        );
        assert_eq!(r("Nope", "z.md"), Resolution::Missing);
    }

    #[test]
    fn outgoing_and_backlinks_skip_index_md_and_duplicates() {
        let g = graph(&[
            ("INDEX.md", "[[T]] [[S]]"),
            ("S.md", "[[T]] [[T|again]] [[Gone]] [[S]]"),
            ("T.md", ""),
        ]);
        assert_eq!(g.outgoing("S.md"), ["T.md"]);
        assert_eq!(g.backlinks("T.md"), ["S.md"]);
        assert!(g.outgoing("INDEX.md").is_empty());
        assert_eq!(g.resolve("INDEX", "S.md"), Resolution::Missing);
    }
}
