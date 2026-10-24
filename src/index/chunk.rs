//! Splits a note into chunks: the whole body when short, else one per `##` section.

use crate::note::Note;
use serde::{Deserialize, Serialize};

const MAX_CHARS: usize = 2000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub rel_path: String,
    pub heading_path: String,
    pub text: String,
    pub ord: u32,
}

pub fn chunks(note: &Note) -> Vec<Chunk> {
    let body = note.body.trim();
    let sections = if body.chars().count() <= MAX_CHARS {
        vec![(String::new(), body.to_string())]
    } else {
        sections(body)
    };
    let mut out = Vec::new();
    for (heading, text) in sections {
        for piece in split_paragraphs(&text) {
            let prefix = if heading.is_empty() {
                note.title.clone()
            } else {
                format!("{} > {heading}", note.title)
            };
            out.push(Chunk {
                rel_path: note.rel_path.clone(),
                heading_path: heading.clone(),
                text: format!("{prefix}\n\n{piece}"),
                ord: out.len() as u32,
            });
        }
    }
    if out.is_empty() {
        out.push(Chunk {
            rel_path: note.rel_path.clone(),
            heading_path: String::new(),
            text: note.title.clone(),
            ord: 0,
        });
    }
    out
}

fn fence_toggles(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// `(heading, text)` pairs; text before the first `##` has an empty heading.
fn sections(body: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![(String::new(), String::new())];
    let mut in_fence = false;
    for line in body.lines() {
        if fence_toggles(line) {
            in_fence = !in_fence;
        }
        match line.strip_prefix("## ").filter(|_| !in_fence) {
            Some(h) => out.push((h.trim().to_string(), String::new())),
            None => {
                let cur = &mut out.last_mut().unwrap().1;
                cur.push_str(line);
                cur.push('\n');
            }
        }
    }
    out.retain(|(h, t)| !h.is_empty() || !t.trim().is_empty());
    out
}

/// Packs blank-line separated paragraphs up to MAX_CHARS; a fence is never split.
fn split_paragraphs(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.chars().count() <= MAX_CHARS {
        return vec![text.to_string()];
    }
    let mut paras: Vec<String> = vec![String::new()];
    let mut in_fence = false;
    for line in text.lines() {
        if fence_toggles(line) {
            in_fence = !in_fence;
        }
        if line.trim().is_empty() && !in_fence {
            if !paras.last().unwrap().is_empty() {
                paras.push(String::new());
            }
        } else {
            let cur = paras.last_mut().unwrap();
            cur.push_str(line);
            cur.push('\n');
        }
    }
    let mut out: Vec<String> = Vec::new();
    for p in paras.into_iter().filter(|p| !p.is_empty()) {
        match out.last_mut() {
            Some(last) if last.chars().count() + p.chars().count() < MAX_CHARS => {
                last.push('\n');
                last.push_str(&p);
            }
            _ => out.push(p),
        }
    }
    out.into_iter().map(|p| p.trim().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_note_is_one_chunk_prefixed_with_title() {
        let n = Note::parse("a.md", "# T\n\nbody\n");
        let c = chunks(&n);
        assert_eq!(c.len(), 1);
        assert!(c[0].text.starts_with("T\n"));
    }

    #[test]
    fn fenced_heading_does_not_split_and_oversized_section_splits_at_blank_lines() {
        let para = "word ".repeat(300);
        let body = format!("## A\n\n{para}\n\n```\n## fake\n```\n\n{para}\n\n## B\n\nend\n");
        let c = chunks(&Note::parse("a.md", &body));
        assert!(c.iter().all(|c| c.heading_path != "fake"));
        assert!(c.iter().filter(|c| c.heading_path == "A").count() >= 2);
        assert_eq!(c.last().unwrap().heading_path, "B");
        assert!(c.last().unwrap().text.starts_with("a > B"));
    }
}
