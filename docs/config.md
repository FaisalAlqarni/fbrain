# Configuration

## `.fbrain.toml`

| Key | Default | Meaning |
|---|---|---|
| `name` | folder name | Brain name. |
| `include` | all folders | Folders walked for notes. |
| `model` | `google/embeddinggemma-300m` | Embedding model: `google/embeddinggemma-300m` or `BAAI/bge-m3`. Any other id is an error. |
| `token_budget` | `4000` | Token budget for packing search results. |
| `conflict_threshold` | `0.50` | Cosine similarity above which a proposal counts as a conflict with a note (ADR 0004). |
| `conflict_overlap_min` | `0.6` | Without embeddings: the share of a proposal's distinct terms that a note must contain to count as a conflict. |

### Project mapping

`fbrain brief` maps the working directory to a project:

```toml
[projects.app1]
repos = ["~/code/app1", "~/work/app1-*"]
```

A project matches when the canonicalised working directory starts with one of its `repos` entries as a path prefix. `~` is expanded. A trailing `*` matches any suffix of the entry's last segment. The first project in file order that matches wins. On Windows the comparison ignores case and treats `\` as `/`. With no match, the brief lists the projects instead.

`FBRAIN_MODEL` overrides `model` for one process.

## Model

`fbrain init` (unless `--no-model` is passed) and `fbrain doctor --fix` download the model once into
`<cache dir>/models/<owner>--<name>`. The default is the q4 ONNX export of EmbeddingGemma-300M from
`onnx-community/embeddinggemma-300m-ONNX` (about 190 MB); `BAAI/bge-m3` is the int8 ONNX export from
`Xenova/bge-m3` (about 570 MB). Each is fetched at one pinned revision, and every file must match its
SHA-256 sum in the source before the temp folder is renamed into place. `fbrain doctor` re-checks the sums
and prints `fail: model: checksum mismatch: <file>` for a changed file; `fbrain doctor --fix` fetches it again.
After the download fbrain never uses the network.

EmbeddingGemma is published under Google's Gemma terms. fbrain does not bundle it; your machine downloads it.

Vectors are truncated to 256 dimensions and re-normalised for EmbeddingGemma (it is trained for this),
and kept at 1024 for bge-m3. Queries and documents get the model card's task prefixes.

Without the model, `fbrain search` still works on keywords and prints
`semantic search off: model not downloaded, run fbrain doctor --fix` on stderr.

Changing the model id, its revision, its prompt prefixes or its dimension rebuilds the whole index and
re-embeds every chunk on the next command.

## Environment

| Variable | Effect |
|---|---|
| `FBRAIN_OFFLINE=1` | Every download attempt fails with `offline: network disabled (FBRAIN_OFFLINE=1)`. Tests and CI use it to prove no network use. |
| `FBRAIN_MODEL` | Model id, above `.fbrain.toml`. |
| `FBRAIN_TEST_MODEL_DIR` | A folder holding the default model's files (`tokenizer.json`, `onnx/model_q4.onnx`, `onnx/model_q4.onnx_data`). Replaces Hugging Face as the download source. Tests that need a model read it and fail when it is unset. |
| `FBRAIN_TEST_START_WAIT_MS` | How long a client waits for a resident process it started to become ready (default 3000). CI raises it because many tests load the model at once. |

## Test model

Tests use `onnx-community/embeddinggemma-300m-ONNX` at revision `5090578d9565bb06545b4552f76e6bc2c93e4a66`.
CI downloads it, checks the sums and caches it under that revision, then exports `FBRAIN_TEST_MODEL_DIR`.

## Build notes

`ort` is pinned to `=2.0.0-rc.15` and downloads a prebuilt ONNX Runtime at build time. Tokenisation uses
`tokenizers` with the `onig` regex backend. HTTPS uses `ureq` with rustls.
