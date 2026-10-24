//! The per-brain index: chunks, BM25 and an incremental on-disk cache.

pub mod bm25;
pub mod chunk;
pub mod embed;
pub mod text;

use crate::brain::Brain;
use crate::links::{LinkGraph, RawLink};
use crate::note::{Note, Status};
use anyhow::{bail, Context, Result};
use chunk::Chunk;
use embed::Embedder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Cache layout version; the resident process handshake compares it.
pub const FORMAT_VERSION: u32 = 5;
/// Manifest values when no embedding model is available.
const NO_MODEL: &str = "none";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub changed: usize,
    pub unchanged: usize,
    pub removed: usize,
    /// Chunks embedded in this run; 0 when no model is available.
    pub embedded: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileData {
    pub mtime_ns: i64,
    pub size: u64,
    pub title: String,
    pub status: Status,
    /// `private: true`, or frontmatter that did not parse (the flags are then unknown).
    pub private: bool,
    pub valid_to: Option<String>,
    pub chunks: Vec<Chunk>,
    /// `chunks.len() * dim` floats, empty when the file was indexed without a model.
    #[serde(default)]
    pub vecs: Vec<f32>,
    pub aliases: Vec<String>,
    pub links: Vec<RawLink>,
}

#[derive(Serialize, Deserialize)]
struct Cached {
    version: u32,
    model: String,
    dim: String,
    /// Model revision and prompt prefixes: a change in any of them changes every vector.
    format: String,
    files: BTreeMap<String, FileData>,
}

pub struct Index {
    pub stats: Stats,
    files: BTreeMap<String, FileData>,
    flat: Vec<(String, usize)>,
    bm25: bm25::Bm25,
    /// One vector per entry of `flat`, `dim` floats each; empty without a model.
    vectors: Vec<f32>,
    dim: usize,
    graph: LinkGraph,
}

pub struct ChunkScore<'a> {
    pub rel_path: &'a str,
    pub chunk: &'a Chunk,
    pub score: f32,
}

impl Index {
    /// `model` is the id and loaded embedder; `None` builds a keyword-only index.
    pub fn load_or_build(
        brain: &Brain,
        cache_dir: &Path,
        full: bool,
        model: Option<(&str, &Embedder)>,
    ) -> Result<Index> {
        let dim_n = model.map_or(0, |(_, e)| e.dim());
        let (model_id, dim, format) = model.map_or(
            (
                NO_MODEL.to_string(),
                NO_MODEL.to_string(),
                NO_MODEL.to_string(),
            ),
            |(id, e)| (id.to_string(), e.dim().to_string(), e.format()),
        );
        let dir = cache_dir.join(brain_key(&brain.path)?);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("index.lock"))?;
        let mut lock = fd_lock::RwLock::new(lock_file);
        let _guard = lock.write().context("cannot lock the index cache")?;

        // A keyword-only index has its own file, so a fallback never replaces the embedded one.
        let cache_file = dir.join(if model.is_some() {
            "index.json"
        } else {
            "index-keyword.json"
        });
        let old = if full {
            None
        } else {
            read_cache(&cache_file, &model_id, &dim, &format)
        };
        let mut old_files = old.map(|c| c.files).unwrap_or_default();

        let mut stats = Stats::default();
        let mut files = BTreeMap::new();
        for r in brain.notes()? {
            if r.rel_path == "INDEX.md" {
                continue;
            }
            let mtime_ns = r.mtime_ns as i64;
            match old_files.remove(&r.rel_path) {
                // A file whose vectors do not match its chunks is re-embedded like a changed file.
                Some(f)
                    if f.mtime_ns == mtime_ns
                        && f.size == r.size
                        && f.vecs.len() == f.chunks.len() * dim_n =>
                {
                    stats.unchanged += 1;
                    files.insert(r.rel_path, f);
                }
                _ => {
                    let text = std::fs::read_to_string(&r.abs_path)
                        .with_context(|| format!("cannot read {}", r.abs_path.display()))?;
                    let note = Note::parse(&r.rel_path, &text);
                    let chunks = chunk::chunks(&note);
                    let vecs = match model {
                        Some((_, e)) => {
                            let texts: Vec<String> =
                                chunks.iter().map(|c| c.text.clone()).collect();
                            let vecs = e.embed_documents(&texts)?;
                            if vecs.len() != texts.len() || vecs.iter().any(|v| v.len() != dim_n) {
                                bail!(
                                    "embedding model returned malformed vectors for {}",
                                    r.rel_path
                                );
                            }
                            stats.embedded += texts.len();
                            vecs.concat()
                        }
                        None => Vec::new(),
                    };
                    stats.changed += 1;
                    files.insert(
                        r.rel_path,
                        FileData {
                            mtime_ns,
                            size: r.size,
                            title: note.title.clone(),
                            status: note.fm.status,
                            private: note.is_private(),
                            valid_to: note.fm.valid_to.clone(),
                            chunks,
                            vecs,
                            aliases: note.fm.aliases.clone(),
                            links: crate::links::note_links(&note),
                        },
                    );
                }
            }
        }
        stats.removed = old_files.len();

        if stats.changed > 0 || stats.removed > 0 || !cache_file.exists() {
            write_cache(&dir, &cache_file, &files, &model_id, &dim, &format)?;
        }
        Ok(Index::from_files(
            files,
            stats,
            model.map_or(0, |(_, e)| e.dim()),
        ))
    }

    /// The embedded cache the resident process keeps for `model_id`, read without a model of our
    /// own; `None` when it is absent or was built for another model or format version. It is as
    /// fresh as the resident process's last refresh.
    pub fn load_embedded(brain: &Brain, cache_dir: &Path, model_id: &str) -> Result<Option<Index>> {
        let file = cache_dir.join(brain_key(&brain.path)?).join("index.json");
        let Some(c) = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Cached>(&b).ok())
            .filter(|c| c.version == FORMAT_VERSION && c.model == model_id)
        else {
            return Ok(None);
        };
        let dim: usize = c
            .dim
            .parse()
            .with_context(|| format!("bad dim in {}", file.display()))?;
        let index = Index::from_files(c.files, Stats::default(), dim);
        index.check_vectors()?;
        Ok(Some(index))
    }

    fn from_files(files: BTreeMap<String, FileData>, stats: Stats, dim: usize) -> Index {
        let mut flat = Vec::new();
        let mut docs = Vec::new();
        let mut vectors = Vec::new();
        for (rel, f) in &files {
            vectors.extend_from_slice(&f.vecs);
            for (i, c) in f.chunks.iter().enumerate() {
                flat.push((rel.clone(), i));
                docs.push(text::tokenize(&c.text));
            }
        }
        let graph = LinkGraph::from_parts(
            files
                .iter()
                .map(|(rel, f)| (rel.as_str(), f.aliases.clone(), f.links.clone())),
        );
        Index {
            stats,
            graph,
            bm25: bm25::Bm25::build(&docs),
            files,
            flat,
            vectors,
            dim,
        }
    }

    /// Embedding dimension; 0 for a keyword-only index.
    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn graph(&self) -> &LinkGraph {
        &self.graph
    }

    /// Seconds since this brain's newest index cache was written; `None` when there is none.
    pub fn cache_age_s(brain: &Brain, cache_dir: &Path) -> Option<u64> {
        let dir = cache_dir.join(brain_key(&brain.path).ok()?);
        ["index.json", "index-keyword.json"]
            .iter()
            .filter_map(|f| std::fs::metadata(dir.join(f)).ok()?.modified().ok())
            .max()
            .map(|t| t.elapsed().map_or(0, |d| d.as_secs()))
    }

    /// Every indexed note with its path.
    pub fn files(&self) -> impl Iterator<Item = (&String, &FileData)> {
        self.files.iter()
    }

    pub fn file(&self, rel_path: &str) -> Option<&FileData> {
        self.files.get(rel_path)
    }

    /// Every note's vectors must cover exactly its chunks.
    pub fn check_vectors(&self) -> Result<()> {
        if self.dim == 0 {
            return Ok(());
        }
        for (rel, f) in &self.files {
            if f.vecs.len() != f.chunks.len() * self.dim {
                bail!("vectors of {rel} do not match its chunks, run fbrain reindex --full");
            }
        }
        Ok(())
    }

    /// Dot product of `query` (normalised) with every chunk vector, i.e. the cosine.
    /// Empty only for a keyword-only index; inconsistent vectors are an error.
    pub fn embed_scores(&self, query: &[f32]) -> Result<Vec<ChunkScore<'_>>> {
        if self.dim == 0 {
            return Ok(Vec::new());
        }
        self.check_vectors()?;
        if query.len() != self.dim {
            bail!(
                "query vector has {} floats, the index has {}",
                query.len(),
                self.dim
            );
        }
        Ok(self
            .vectors
            .chunks_exact(self.dim)
            .zip(&self.flat)
            .map(|(v, (rel, i))| ChunkScore {
                rel_path: rel,
                chunk: &self.files[rel].chunks[*i],
                score: v.iter().zip(query).map(|(a, b)| a * b).sum(),
            })
            .collect())
    }

    /// BM25 score of every chunk that matches at least one term.
    pub fn scores(&self, terms: &[String]) -> Vec<ChunkScore<'_>> {
        self.bm25
            .scores(terms)
            .into_iter()
            .map(|(id, score)| {
                let (rel, i) = &self.flat[id];
                ChunkScore {
                    rel_path: rel,
                    chunk: &self.files[rel].chunks[*i],
                    score,
                }
            })
            .collect()
    }
}

/// FNV-1a over the canonical path: stable across Rust versions, unlike `DefaultHasher`.
fn brain_key(path: &Path) -> Result<String> {
    let canon = path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    let h = canon
        .to_string_lossy()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ b as u64).wrapping_mul(0x100000001b3)
        });
    Ok(format!("{h:016x}"))
}

/// A missing, unparseable or mismatched cache is rebuilt from the notes, so it reads as absent.
fn read_cache(file: &Path, model: &str, dim: &str, format: &str) -> Option<Cached> {
    let c: Cached = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    (c.version == FORMAT_VERSION && c.model == model && c.dim == dim && c.format == format)
        .then_some(c)
}

fn write_cache(
    dir: &Path,
    file: &Path,
    files: &BTreeMap<String, FileData>,
    model: &str,
    dim: &str,
    format: &str,
) -> Result<()> {
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("index.json");
    let tmp: PathBuf = dir.join(format!("{name}.{}.tmp", std::process::id()));
    let data = serde_json::to_vec(&Cached {
        version: FORMAT_VERSION,
        model: model.into(),
        dim: dim.into(),
        format: format.into(),
        files: files.clone(),
    })?;
    std::fs::write(&tmp, data).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, file).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(chunks: usize, floats: usize) -> FileData {
        FileData {
            mtime_ns: 0,
            size: 0,
            title: "t".into(),
            status: Status::Active,
            private: false,
            valid_to: None,
            chunks: (0..chunks)
                .map(|i| Chunk {
                    rel_path: "a.md".into(),
                    heading_path: String::new(),
                    ord: i as u32,
                    text: format!("word{i}"),
                })
                .collect(),
            vecs: vec![0.5; floats],
            aliases: Vec::new(),
            links: Vec::new(),
        }
    }

    fn index(floats: usize) -> Index {
        let files = BTreeMap::from([("a.md".to_string(), file(2, floats))]);
        Index::from_files(files, Stats::default(), 3)
    }

    #[test]
    fn vectors_that_do_not_cover_every_chunk_are_an_error_not_an_empty_result() {
        assert!(index(6).check_vectors().is_ok());
        assert!(index(3).check_vectors().is_err());
        assert!(index(3).embed_scores(&[1.0, 0.0, 0.0]).is_err());
        assert_eq!(index(6).embed_scores(&[1.0, 0.0, 0.0]).unwrap().len(), 2);
    }
}
