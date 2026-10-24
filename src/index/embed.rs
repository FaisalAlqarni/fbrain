//! Local embedding model: the pinned download, checksums, and encoding text with `ort`.

use anyhow::{anyhow, bail, ensure, Context, Result};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokenizers::Tokenizer;

pub const DEFAULT_MODEL: &str = "google/embeddinggemma-300m";
const SUPPORTED: &str = "google/embeddinggemma-300m, BAAI/bge-m3";
/// Longest input in tokens; the final special token is kept when a text is cut.
const MAX_TOKENS: usize = 512;
/// Texts per model run while indexing.
const BATCH: usize = 1;

/// One supported model: where it is downloaded from, the exact bytes expected, and how text is framed.
#[derive(Debug)]
pub struct ModelSpec {
    pub id: &'static str,
    pub revision: &'static str,
    /// (repo-relative path, sha256 hex). The `.onnx` file and `tokenizer.json` are required.
    pub files: &'static [(&'static str, &'static str)],
    pub dims: usize,
    pub query_prefix: &'static str,
    pub document_prefix: &'static str,
    /// Hugging Face repo the files are served from (not always the model's own id).
    pub repo: &'static str,
}

static GEMMA: ModelSpec = ModelSpec {
    id: "google/embeddinggemma-300m",
    repo: "onnx-community/embeddinggemma-300m-ONNX",
    revision: "5090578d9565bb06545b4552f76e6bc2c93e4a66",
    files: &[
        (
            "tokenizer.json",
            "4dda02faaf32bc91031dc8c88457ac272b00c1016cc679757d1c441b248b9c47",
        ),
        (
            "onnx/model_q4.onnx",
            "ad1dfee81a70f7944b9b9d1cc6e48075b832881cf33fab2f2b248be78f3f0043",
        ),
        (
            "onnx/model_q4.onnx_data",
            "599962c3143b040de2dd05e5975be3e9091dd067cacc6a8f7186e3203bab9e02",
        ),
    ],
    dims: 256,
    // Verbatim from the model card's prompt list: `query` for search and `title: none` for documents.
    query_prefix: "task: search result | query: ",
    document_prefix: "title: none | text: ",
};

// shortcut: bge-m3 is not Matryoshka-trained, so it keeps its full 1024 dimensions; ADR 0003 records this.
static BGE_M3: ModelSpec = ModelSpec {
    id: "BAAI/bge-m3",
    repo: "Xenova/bge-m3",
    revision: "4de13258303883538bd53b696b452bf8099f0858",
    files: &[
        (
            "tokenizer.json",
            "6710678b12670bc442b99edc952c4d996ae309a7020c1fa0096dd245c2faf790",
        ),
        (
            "onnx/model_int8.onnx",
            "a206e10e995aa2a833924bcd725ba5dd6c3425cd34bac3cf2b5677cd2a1c51d6",
        ),
    ],
    dims: 1024,
    query_prefix: "",
    document_prefix: "",
};

pub fn spec(model_id: &str) -> Result<&'static ModelSpec> {
    [&GEMMA, &BGE_M3]
        .into_iter()
        .find(|s| s.id == model_id)
        .ok_or_else(|| anyhow!("unknown model: {model_id} (supported: {SUPPORTED})"))
}

/// `<cache>/models/<owner>--<name>`.
pub fn model_dir(cache_dir: &Path, model_id: &str) -> PathBuf {
    cache_dir.join("models").join(model_id.replace('/', "--"))
}

/// Why a model dir cannot be used as it is; `doctor` matches on this instead of message text.
#[derive(Debug)]
pub enum ModelError {
    NotDownloaded(String),
    Incomplete(String),
    ChecksumMismatch(String),
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelError::NotDownloaded(id) => write!(f, "model not downloaded: {id}"),
            ModelError::Incomplete(file) => write!(f, "incomplete model: {file} is missing"),
            ModelError::ChecksumMismatch(file) => write!(f, "checksum mismatch: {file}"),
        }
    }
}

impl std::error::Error for ModelError {}

fn sha256_hex(file: &Path) -> Result<String> {
    let read = |e: std::io::Error| {
        anyhow::Error::new(e).context(format!("cannot read {}", file.display()))
    };
    let mut f = std::fs::File::open(file).map_err(read)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(read)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

fn any_present(dir: &Path, spec: &ModelSpec) -> bool {
    spec.files.iter().any(|(f, _)| dir.join(f).exists())
}

fn all_present(dir: &Path, spec: &ModelSpec) -> bool {
    spec.files
        .iter()
        .all(|(f, _)| std::fs::metadata(dir.join(f)).is_ok_and(|m| m.is_file() && m.len() > 0))
}

/// Every file of `spec` exists in `dir` with the pinned SHA-256.
fn verify(dir: &Path, spec: &ModelSpec) -> Result<()> {
    for (f, sum) in spec.files {
        if !dir.join(f).is_file() {
            return Err(ModelError::Incomplete(f.to_string()).into());
        }
        if sha256_hex(&dir.join(f))? != *sum {
            return Err(ModelError::ChecksumMismatch(f.to_string()).into());
        }
    }
    Ok(())
}

const STAMP: &str = ".verified";

/// Size and mtime of every spec file, one line each.
fn stamp_text(dir: &Path, spec: &ModelSpec) -> Option<String> {
    let mut s = String::new();
    for (f, _) in spec.files {
        let m = std::fs::metadata(dir.join(f)).ok()?;
        let ns = m
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        s += &format!("{f} {} {ns}\n", m.len());
    }
    Some(s)
}

/// Records that the files passed a full checksum. A failed write only costs a re-hash next time.
fn write_stamp(dir: &Path, spec: &ModelSpec) {
    if let Some(s) = stamp_text(dir, spec) {
        let _ = std::fs::write(dir.join(STAMP), s);
    }
}

/// Full checksums on first use, then a size-and-mtime stamp. The stamp catches truncation,
/// bit rot and an ordinary overwrite; it does not stop a local attacker who restores the mtime
/// (`doctor` always hashes in full).
fn verify_cached(dir: &Path, spec: &ModelSpec) -> Result<()> {
    let stamp = std::fs::read_to_string(dir.join(STAMP)).ok();
    if stamp.is_some() && stamp == stamp_text(dir, spec) {
        return Ok(());
    }
    verify(dir, spec)?;
    write_stamp(dir, spec);
    Ok(())
}

const MAX_FILE_BYTES: u64 = 1 << 30;
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(600);

/// Copies at most `max` bytes; a longer stream is an error, not a truncated file.
fn copy_capped(from: impl std::io::Read, to: &mut impl std::io::Write, max: u64) -> Result<()> {
    let n = std::io::copy(&mut from.take(max + 1), to)?;
    if n > max {
        bail!("file is larger than {max} bytes");
    }
    Ok(())
}

/// Returns the verified local model dir. Downloads only when `allow_download` is set and the
/// network is not disabled; the files land in a temp dir, are checked against the pinned
/// SHA-256 sums, and only then renamed into place.
pub fn ensure_model(model_id: &str, cache_dir: &Path, allow_download: bool) -> Result<PathBuf> {
    let spec = spec(model_id)?;
    let dir = model_dir(cache_dir, model_id);
    let state = verify(&dir, spec);
    if state.is_ok() {
        write_stamp(&dir, spec);
        return Ok(dir);
    }
    if !allow_download {
        if !any_present(&dir, spec) {
            return Err(ModelError::NotDownloaded(model_id.to_string()).into());
        }
        return state.map(|()| dir);
    }
    if std::env::var_os("FBRAIN_OFFLINE").is_some_and(|v| v == "1") {
        bail!("offline: network disabled (FBRAIN_OFFLINE=1)");
    }
    let parent = dir.parent().expect("model dir has a parent");
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".download-{}-{}",
        std::process::id(),
        model_id.replace('/', "--")
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let fetched = fetch_all(spec, &tmp).and_then(|()| verify(&tmp, spec));
    let result = fetched.and_then(|()| {
        // `dir` is absent, incomplete or corrupt here; rename cannot replace a non-empty dir.
        let _ = std::fs::remove_dir_all(&dir);
        match std::fs::rename(&tmp, &dir) {
            Ok(()) => Ok(()),
            // Another process finished first.
            Err(_) if verify(&dir, spec).is_ok() => Ok(()),
            Err(e) => Err(e).with_context(|| format!("cannot move model into {}", dir.display())),
        }
    });
    let _ = std::fs::remove_dir_all(&tmp);
    result?;
    write_stamp(&dir, spec);
    Ok(dir)
}

/// HTTPS only, including after a redirect, with a short redirect chain.
fn download_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .https_only(true)
        .redirects(3)
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .timeout(DOWNLOAD_DEADLINE)
        .build()
}

/// `FBRAIN_TEST_MODEL_DIR` replaces Hugging Face as the source so tests never download.
fn fetch_all(spec: &ModelSpec, dest: &Path) -> Result<()> {
    let agent = download_agent();
    for (f, _) in spec.files {
        let to = dest.join(f);
        std::fs::create_dir_all(to.parent().expect("file has a parent"))?;
        if let Some(src) = std::env::var_os("FBRAIN_TEST_MODEL_DIR") {
            std::fs::copy(Path::new(&src).join(f), &to)
                .with_context(|| format!("cannot copy {f} from FBRAIN_TEST_MODEL_DIR"))?;
            continue;
        }
        let url = format!(
            "https://huggingface.co/{}/resolve/{}/{f}",
            spec.repo, spec.revision
        );
        let resp = agent
            .get(&url)
            .call()
            .map_err(|e| anyhow!("download failed: {url}: {e}"))?;
        let mut out = std::fs::File::create(&to)?;
        copy_capped(resp.into_reader(), &mut out, MAX_FILE_BYTES)
            .with_context(|| format!("download interrupted: {url}"))?;
    }
    Ok(())
}

/// Every file of the model is in the cache (not checked against its sums).
pub fn is_downloaded(cache_dir: &Path, model_id: &str) -> Result<bool> {
    Ok(all_present(
        &model_dir(cache_dir, model_id),
        spec(model_id)?,
    ))
}

/// Size and mtime of the model files; a change means the files on disk are not the ones loaded.
pub fn model_stamp(cache_dir: &Path, model_id: &str) -> Option<String> {
    stamp_text(&model_dir(cache_dir, model_id), spec(model_id).ok()?)
}

/// The embedder when the model is in the cache, `None` when it is not downloaded.
/// A model that is present but damaged or unloadable is an error.
pub fn load_if_present(cache_dir: &Path, model_id: &str) -> Result<Option<Embedder>> {
    let spec = spec(model_id)?;
    let dir = model_dir(cache_dir, model_id);
    if !all_present(&dir, spec) {
        return Ok(None);
    }
    verify_cached(&dir, spec).context("model files are damaged, run fbrain doctor --fix")?;
    Embedder::load(&dir, spec).map(Some)
}

/// ONNX Runtime allows one environment per process; every embedder shares it.
fn environment() -> ort::Result<ort::environment::Environment> {
    static ENV: Mutex<Option<ort::environment::Environment>> = Mutex::new(None);
    let mut slot = ENV.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_none() {
        *slot = Some(ort::init().build()?);
    }
    Ok(slot.clone().expect("set above"))
}

pub struct Embedder {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    spec: &'static ModelSpec,
    inputs: Vec<String>,
    output: String,
}

impl Embedder {
    pub fn load(model_dir: &Path, spec: &'static ModelSpec) -> Result<Embedder> {
        let onnx = spec
            .files
            .iter()
            .find(|f| f.0.ends_with(".onnx"))
            .expect("every spec lists its .onnx file")
            .0;
        let err =
            |e: &dyn std::fmt::Display| anyhow!("cannot load model {}: {e}", model_dir.display());
        let env = environment().map_err(|e| anyhow!("ONNX Runtime failed to start: {e}"))?;
        // Arena and memory pattern off: they only grow resident memory for a one-text-at-a-time workload.
        let session = Session::builder(&env)
            .map_err(|e| err(&e))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| err(&e))?
            .with_intra_threads(4)
            .map_err(|e| err(&e))?
            .with_execution_providers([ort::ep::CPU::default().with_arena_allocator(false).build()])
            .map_err(|e| err(&e))?
            .with_memory_pattern(false)
            .map_err(|e| err(&e))?
            .commit_from_file(model_dir.join(onnx))
            .map_err(|e| err(&e))?;
        let inputs = session
            .inputs()
            .iter()
            .map(|i| i.name().to_string())
            .collect();
        let outs: Vec<String> = session
            .outputs()
            .iter()
            .map(|o| o.name().to_string())
            .collect();
        let output = outs
            .iter()
            .find(|o| *o == "sentence_embedding")
            .or_else(|| outs.iter().find(|o| *o == "last_hidden_state"))
            .or(outs.first())
            .cloned()
            .context("model has no outputs")?;
        let mut tokenizer = Tokenizer::from_file(model_dir.join("tokenizer.json"))
            .map_err(|e| anyhow!("cannot load tokenizer: {e}"))?;
        tokenizer
            .with_truncation(None)
            .map_err(|e| anyhow!("{e}"))?;
        tokenizer.with_padding(None);
        let e = Embedder {
            session: Mutex::new(session),
            tokenizer,
            spec,
            inputs,
            output,
        };
        let probe = e.embed(&["x".to_string()], "")?;
        ensure!(
            probe[0].len() == spec.dims,
            "model produced {} dimensions, expected {}",
            probe[0].len(),
            spec.dims
        );
        Ok(e)
    }

    pub fn dim(&self) -> usize {
        self.spec.dims
    }

    /// What the index manifest records besides the model id: revision and prompt prefixes.
    pub fn format(&self) -> String {
        let s = self.spec;
        format!("{}|{}|{}", s.revision, s.query_prefix, s.document_prefix)
    }

    /// One L2-normalised vector per text, with the spec's query prefix.
    pub fn embed_queries(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed(texts, self.spec.query_prefix)
    }

    /// One L2-normalised vector per text, with the spec's document prefix.
    pub fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed(texts, self.spec.document_prefix)
    }

    fn embed(&self, texts: &[String], prefix: &str) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH) {
            out.extend(self.run(batch, prefix)?);
        }
        Ok(out)
    }

    fn run(&self, texts: &[String], prefix: &str) -> Result<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        let enc = self
            .tokenizer
            .encode_batch(prefixed, true)
            .map_err(|e| anyhow!("tokenizer: {e}"))?;
        let b = texts.len();
        let len = enc
            .iter()
            .map(|e| e.get_ids().len().min(MAX_TOKENS))
            .max()
            .unwrap_or(1)
            .max(1);
        let (mut ids, mut mask) = (vec![0i64; b * len], vec![0i64; b * len]);
        for (i, e) in enc.iter().enumerate() {
            let src = e.get_ids();
            let n = src.len().min(MAX_TOKENS);
            for j in 0..n {
                let id = if n < src.len() && j == n - 1 {
                    src[src.len() - 1]
                } else {
                    src[j]
                };
                ids[i * len + j] = id as i64;
                mask[i * len + j] = 1;
            }
        }
        let mut feeds: Vec<(String, ort::session::SessionInputValue)> = Vec::new();
        for name in &self.inputs {
            let data = match name.as_str() {
                "input_ids" => ids.clone(),
                "attention_mask" => mask.clone(),
                "token_type_ids" => vec![0; b * len],
                "position_ids" => (0..b * len).map(|k| (k % len) as i64).collect(),
                other => bail!("model has an unknown input: {other}"),
            };
            feeds.push((name.clone(), Tensor::from_array(([b, len], data))?.into()));
        }
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow!("embedding session lock poisoned"))?;
        let outputs = session.run(feeds)?;
        let (shape, data) = outputs[self.output.as_str()].try_extract_tensor::<f32>()?;
        let d = *shape.last().context("empty output shape")? as usize;
        // Sentence output is [b, d]; token output is [b, len, d] and the first token is the CLS summary.
        let stride = if shape.len() == 2 {
            d
        } else {
            shape[1] as usize * d
        };
        ensure!(
            d >= self.spec.dims && data.len() >= b * stride,
            "unexpected model output shape {shape:?}"
        );
        Ok((0..b)
            .map(|i| {
                let mut v = data[i * stride..i * stride + self.spec.dims].to_vec();
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
                v.iter_mut().for_each(|x| *x /= norm);
                v
            })
            .collect())
    }
}

/// Notes by their best chunk's cosine with `query` (normalised), highest first, at most `k`.
/// `exclude` drops one note path. An index without usable vectors is an error naming the cause.
pub fn cosine_top(
    index: &crate::index::Index,
    query: &[f32],
    k: usize,
    exclude: Option<&str>,
) -> Result<Vec<(String, f32)>> {
    if index.dim() == 0 {
        bail!("index has no embeddings (built without a model); run fbrain doctor --fix, then fbrain reindex");
    }
    let scores = index.embed_scores(query)?;
    let mut best: std::collections::BTreeMap<&str, f32> = std::collections::BTreeMap::new();
    for s in scores.iter().filter(|s| Some(s.rel_path) != exclude) {
        let e = best.entry(s.rel_path).or_insert(f32::MIN);
        *e = e.max(s.score);
    }
    let mut out: Vec<(String, f32)> = best.into_iter().map(|(p, s)| (p.to_string(), s)).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(k);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_model_without_download_is_an_error_naming_the_id() {
        let tmp = tempfile::tempdir().unwrap();
        let e = ensure_model(DEFAULT_MODEL, tmp.path(), false).unwrap_err();
        assert_eq!(
            e.to_string(),
            "model not downloaded: google/embeddinggemma-300m"
        );
    }

    #[test]
    fn the_download_agent_refuses_plain_http() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || drop(listener.accept()));
        let e = download_agent()
            .get(&format!("http://127.0.0.1:{port}/x"))
            .call()
            .unwrap_err();
        assert_eq!(e.kind(), ureq::ErrorKind::InsecureRequestHttpsOnly, "{e}");
    }

    #[test]
    fn a_checksum_read_error_names_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let e = sha256_hex(&tmp.path().join("gone.bin")).unwrap_err();
        assert!(format!("{e:#}").contains("gone.bin"), "{e:#}");
    }

    #[test]
    fn copy_capped_fails_on_a_stream_over_the_cap() {
        let mut out = Vec::new();
        assert!(copy_capped(&[7u8; 10][..], &mut out, 10).is_ok());
        let mut out = Vec::new();
        assert!(copy_capped(&[7u8; 11][..], &mut out, 10).is_err());
    }

    #[test]
    fn only_the_two_supported_ids_can_name_a_cache_dir() {
        let tmp = tempfile::tempdir().unwrap();
        for id in ["../x", "a/../b", "a", "a/b/c", "/b"] {
            assert!(ensure_model(id, tmp.path(), false).is_err(), "{id}");
        }
    }
}
