//! Keyword search over the index: status and validity filtering, ranking, explanation.

use crate::index::chunk::Chunk;
use crate::index::embed::Embedder;
use crate::index::{text, ChunkScore, FileData, Index};
use crate::note::{Note, Status};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_BUDGET: usize = 4000;
/// Direct hits whose links are followed, and links added per hit.
const EXPAND_HITS: usize = 3;
const EXPAND_PER_HIT: usize = 3;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Filter {
    /// Include superseded, invalidated and expired notes.
    pub history: bool,
    /// `YYYY-MM-DD`; replaces today as the date `valid_to` is compared with.
    pub as_of: Option<String>,
    /// Keep only these statuses, on top of the rest of the filter.
    pub statuses: Option<Vec<Status>>,
}

/// Who receives the result. `Cli` and `Mcp` can both reach an agent, so neither gets a private
/// note. `Private` is Faisal asking for everything with `--private`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Surface {
    #[default]
    Cli,
    Mcp,
    Private,
}

/// The one chooser for the CLI commands that take `--private`.
pub fn cli_surface(include_private: bool) -> Surface {
    if include_private {
        Surface::Private
    } else {
        Surface::Cli
    }
}

/// The single exposure gate: false only for a private note (or one whose frontmatter did not
/// parse) unless the surface is `Private`. Search, expansion, `get`, conflict candidates, counts and the
/// session brief all decide through it.
pub fn expose(note: &Note, surface: Surface) -> bool {
    visible(note.is_private(), surface)
}

/// `expose` for an indexed note, which carries the flag `Note::is_private` computed.
pub fn exposed(file: &FileData, surface: Surface) -> bool {
    visible(file.private, surface)
}

fn visible(private: bool, surface: Surface) -> bool {
    !(private && surface != Surface::Private)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    pub text: String,
    pub k: usize,
    /// Estimated tokens of snippet plus title that the returned hits may add up to.
    pub budget_tokens: usize,
    pub filter: Filter,
    #[serde(default)]
    pub surface: Surface,
}

impl Query {
    pub fn new(text: &str, filter: Filter) -> Query {
        Query {
            text: text.into(),
            k: 10,
            budget_tokens: DEFAULT_BUDGET,
            filter,
            surface: Surface::Cli,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Explain {
    pub bm25_rank: Option<usize>,
    pub embed_rank: Option<usize>,
    pub fused: f32,
    pub matched_terms: Vec<String>,
    pub linked_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub rel_path: String,
    pub title: String,
    pub status: Status,
    /// Heading of the chunk that represents the note; empty for an unsplit note.
    pub heading_path: String,
    pub snippet: String,
    pub score: f32,
    pub explain: Explain,
}

fn passes(q: &Query, file: &FileData, today: &str) -> bool {
    let f = &q.filter;
    if !exposed(file, q.surface) {
        return false;
    }
    if let Some(s) = &f.statuses {
        if !s.contains(&file.status) {
            return false;
        }
    }
    if f.history {
        return true;
    }
    let cutoff = f.as_of.as_deref().unwrap_or(today);
    let expired = file
        .valid_to
        .as_deref()
        .is_some_and(|v| v.get(..10).unwrap_or(v) <= cutoff.get(..10).unwrap_or(cutoff));
    !matches!(file.status, Status::Superseded | Status::Invalidated) && !expired
}

type Best<'a> = (&'a str, f32, &'a Chunk);

/// The best chunk of each note that passes the filter, best note first.
fn rank_notes<'a>(
    index: &'a Index,
    scores: Vec<ChunkScore<'a>>,
    q: &Query,
    today: &str,
) -> Vec<Best<'a>> {
    let mut best: HashMap<&str, (f32, &Chunk)> = HashMap::new();
    for s in scores {
        let file = index.file(s.rel_path).expect("scored chunk has a file");
        if !passes(q, file, today) {
            continue;
        }
        let e = best.entry(s.rel_path).or_insert((s.score, s.chunk));
        if s.score > e.0 || (s.score == e.0 && s.chunk.ord < e.1.ord) {
            *e = (s.score, s.chunk);
        }
    }
    let mut ranked: Vec<_> = best.into_iter().map(|(p, (s, c))| (p, s, c)).collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    ranked
}

/// Reciprocal rank fusion (k 60) of two ranked path lists, plus a bonus for a note
/// ranked 1st (+0.05) or 2nd/3rd (+0.02) in either list, once per note.
fn fuse<'a>(bm25: &[&'a str], embed: &[&'a str]) -> Vec<(&'a str, f32)> {
    let mut scores: HashMap<&str, (f32, usize)> = HashMap::new();
    for list in [bm25, embed] {
        for (i, p) in list.iter().enumerate() {
            let e = scores.entry(p).or_insert((0.0, usize::MAX));
            e.0 += 1.0 / (60.0 + (i + 1) as f32);
            e.1 = e.1.min(i + 1);
        }
    }
    let mut out: Vec<_> = scores
        .into_iter()
        .map(|(p, (s, best))| {
            let bonus = match best {
                1 => 0.05,
                2 | 3 => 0.02,
                _ => 0.0,
            };
            (p, s + bonus)
        })
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    out
}

/// Hybrid search when `embedder` is given and the index has vectors, else keywords only.
pub fn search(index: &Index, q: &Query, embedder: Option<&Embedder>) -> Result<Vec<Hit>> {
    let mut terms = text::tokenize(&q.text);
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let today = today();
    let cap = q.k * 5;
    let mut bm25: Vec<_> = rank_notes(index, index.scores(&terms), q, &today)
        .into_iter()
        .filter(|b| b.1 > 0.0)
        .collect();
    bm25.truncate(cap);
    let mut embed = match embedder {
        Some(e) => {
            let qv = e.embed_queries(std::slice::from_ref(&q.text))?.remove(0);
            rank_notes(index, index.embed_scores(&qv)?, q, &today)
        }
        None => Vec::new(),
    };
    embed.truncate(cap);

    let rank_of = |list: &[Best], p: &str| list.iter().position(|b| b.0 == p).map(|i| i + 1);
    let rows: Vec<(&str, f32)> = if embed.is_empty() {
        bm25.iter().map(|b| (b.0, b.1)).collect()
    } else {
        let bp: Vec<&str> = bm25.iter().map(|b| b.0).collect();
        let ep: Vec<&str> = embed.iter().map(|b| b.0).collect();
        fuse(&bp, &ep)
    };
    let mut hits: Vec<Hit> = rows
        .into_iter()
        .take(q.k)
        .map(|(rel, score)| {
            let chunk = bm25
                .iter()
                .chain(&embed)
                .find(|b| b.0 == rel)
                .map(|b| b.2)
                .expect("fused note came from a channel");
            let mut hit = make_hit(index, rel, chunk, &terms, score);
            hit.explain.bm25_rank = rank_of(&bm25, rel);
            hit.explain.embed_rank = rank_of(&embed, rel);
            hit
        })
        .collect();
    let linked = expand(index, &hits, q, &terms, &today);
    hits.extend(linked);
    Ok(pack(hits, q.budget_tokens))
}

fn make_hit(index: &Index, rel: &str, chunk: &Chunk, terms: &[String], score: f32) -> Hit {
    let file = index.file(rel).expect("ranked note has a file");
    let in_chunk = text::tokenize(&chunk.text);
    Hit {
        rel_path: rel.to_string(),
        title: file.title.clone(),
        status: file.status,
        heading_path: chunk.heading_path.clone(),
        snippet: snippet(&chunk.text, terms),
        score,
        explain: Explain {
            bm25_rank: None,
            embed_rank: None,
            fused: score,
            matched_terms: terms
                .iter()
                .filter(|t| in_chunk.contains(t))
                .cloned()
                .collect(),
            linked_from: None,
        },
    }
}

/// For each of the top direct hits, up to 3 notes it links to (then notes linking to it) that pass
/// the filter and are not in `hits` yet, shown by their first chunk.
fn expand(index: &Index, hits: &[Hit], q: &Query, terms: &[String], today: &str) -> Vec<Hit> {
    let graph = index.graph();
    let mut added: Vec<Hit> = Vec::new();
    for parent in hits.iter().take(EXPAND_HITS) {
        let candidates = graph
            .outgoing(&parent.rel_path)
            .iter()
            .chain(graph.backlinks(&parent.rel_path));
        let mut taken = 0;
        for rel in candidates {
            if taken == EXPAND_PER_HIT {
                break;
            }
            let seen = |h: &Hit| h.rel_path == *rel;
            let Some(file) = index.file(rel) else {
                continue;
            };
            if hits.iter().any(seen) || added.iter().any(seen) || !passes(q, file, today) {
                continue;
            }
            let Some(chunk) = file.chunks.first() else {
                continue;
            };
            let mut hit = make_hit(index, rel, chunk, terms, 0.0);
            hit.explain.linked_from = Some(parent.rel_path.clone());
            added.push(hit);
            taken += 1;
        }
    }
    added
}

fn is_arabic(c: char) -> bool {
    matches!(c, '\u{600}'..='\u{6FF}' | '\u{750}'..='\u{77F}' | '\u{8A0}'..='\u{8FF}' | '\u{FB50}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFF}')
}

/// Arabic-script characters count 1 token per 2, everything else 1 per 4, each rounded up.
fn estimate_tokens(text: &str) -> usize {
    let arabic = text.chars().filter(|c| is_arabic(*c)).count();
    let other = text.chars().count() - arabic;
    arabic.div_ceil(2) + other.div_ceil(4)
}

/// Keeps hits in order while snippet plus title fit the budget; a hit that does not fit is
/// skipped, and the first hit is always kept.
fn pack(hits: Vec<Hit>, budget: usize) -> Vec<Hit> {
    let mut used = 0;
    hits.into_iter()
        .enumerate()
        .filter(|(i, h)| {
            let cost = estimate_tokens(&h.snippet) + estimate_tokens(&h.title);
            let keep = *i == 0 || used + cost <= budget;
            if keep {
                used += cost;
            }
            keep
        })
        .map(|(_, h)| h)
        .collect()
}

/// Up to 240 chars of the original text, starting about 80 chars before the first matched word.
fn snippet(text: &str, terms: &[String]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let is_word = |c: char| c.is_alphanumeric() || text::is_stripped(c);
    let mut first = 0;
    let mut i = 0;
    while i < chars.len() {
        if !is_word(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_word(chars[i]) {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        if terms.contains(&text::normalize(&word)) {
            first = start;
            break;
        }
    }
    let from = first.saturating_sub(80);
    let s: String = chars[from..].iter().take(240).collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Today in UTC as `YYYY-MM-DD`.
fn today() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86400) as i64;
    // Howard Hinnant's civil-from-days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_adds_rank_scores_and_a_bonus_for_the_top_three() {
        let r = |rank: f32| 1.0 / (60.0 + rank);
        let out = fuse(&["a", "b", "c", "d"], &["c", "x", "a"]);
        let get = |p: &str| out.iter().find(|o| o.0 == p).unwrap().1;
        assert!((get("a") - (r(1.0) + r(3.0) + 0.05)).abs() < 1e-6);
        assert!((get("c") - (r(3.0) + r(1.0) + 0.05)).abs() < 1e-6);
        assert!((get("b") - (r(2.0) + 0.02)).abs() < 1e-6);
        assert!((get("x") - (r(2.0) + 0.02)).abs() < 1e-6);
        assert!((get("d") - r(4.0)).abs() < 1e-6);
    }

    #[test]
    fn arabic_counts_half_a_token_per_character_and_other_text_a_quarter() {
        assert_eq!(estimate_tokens(&"ك".repeat(200)), 100);
        assert_eq!(estimate_tokens(&"a".repeat(200)), 50);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn snippet_is_centred_on_the_first_matched_word_and_capped() {
        let text = format!("{} needle {}", "a ".repeat(100), "b ".repeat(200));
        let s = snippet(&text, &["needle".into()]);
        assert!(s.contains("needle"));
        assert!(s.chars().count() <= 240);
    }
}
