# 0003: Embedding model

Status: accepted (task 06). Supersedes 0002.

## Decision

Semantic search uses EmbeddingGemma-300M, the q4 ONNX export from `onnx-community/embeddinggemma-300m-ONNX`
at revision `5090578d9565bb06545b4552f76e6bc2c93e4a66`, run on CPU through `ort` (`=2.0.0-rc.15`, 4 intra-op
threads, arena and memory pattern off) with `tokenizers`. Files are `tokenizer.json`, `onnx/model_q4.onnx` and
`onnx/model_q4.onnx_data` (about 207 MiB). Each file is checked against a SHA-256 constant in the source after
download and by `fbrain doctor`.

- Prefixes, from the model card: queries `task: search result | query: `, documents `title: none | text: `
  followed by the chunk text (which already starts with `title > heading path`).
- Vectors come from the model's `sentence_embedding` output, are cut to 256 dimensions (Matryoshka) and
  re-normalised. Spike 05b measured 0.97 v2 recall@5 at 256 dimensions against 0.95 at 768.
- Inputs are cut at 512 tokens, keeping the final special token.
- `BAAI/bge-m3` is selectable through `model`: `Xenova/bge-m3`, `onnx/model_int8.onnx` plus `tokenizer.json`, revision
  `4de13258303883538bd53b696b452bf8099f0858`, no prefixes, first-token (CLS) output, 1024 dimensions. bge-m3 is not
  trained for Matryoshka truncation, so it keeps its full size.
- The index manifest records the model id, the revision with both prefixes, the dimension and the format
  version; a change in any of them re-embeds everything.
- Removed: `model2vec-rs`, the potion models, `Dtype`, `model_dtype`, `FBRAIN_MODEL_DTYPE`.
- ONNX Runtime allows one environment per process, so all embedders in a process share one.

## Gate results (v2 labels, 60 queries, 55 notes, Ryzen 9 9950X3D)

Thresholds for this task: hybrid recall@5 >= 0.90, Arabic recall@5 >= 0.85, recall@5 >= 0.62 (`om` on v2),
in-process warm search p95 < 50 ms. All pass. First-call and resident-process memory gates belong to task 07.

| Measure | ext4 copy | /mnt/d copy |
|---|---|---|
| Hybrid recall@5 | 0.95 | 0.95 |
| Hybrid MRR | 0.755 | 0.755 |
| Arabic recall@5 | 0.95 | 0.95 |
| English / mixed recall@5 | 0.93 / 1.00 | 0.93 / 1.00 |
| Per kind: paraphrase / arabic / mixed / multi-note | 0.95 / 0.95 / 1.00 / 0.90 | same |
| Embedding channel only: recall@5 / MRR | 0.95 / 0.758 | 0.95 / 0.758 |
| Warm search p50 / p95 (in process, query embedding included) | 27.6 / 33.0 ms | 27.6 / 32.7 ms |
| Model load (`Embedder::load`) | 925 ms | 2521 ms |
| Peak RSS of one `fbrain search` process | 516 MB | 516 MB |
| Cold `fbrain search` p95 (fresh process, not a gate here) | 1254 ms | 2708 ms |

Reports are in each bench copy's `.fbrain-bench/` (`report-v2-ext4`, `report-v2-ntfs`), not in this repo.

On the v1 labels (50 queries, ext4): hybrid recall@5 1.00, MRR 0.905, embedding only 0.88 / 0.755.

`BAAI/bge-m3` on v2 (ext4): hybrid recall@5 0.95, MRR 0.756, Arabic 0.90, embedding only 0.933 / 0.752,
warm p95 32.2 ms, peak RSS 879 MB, cold p95 1785 ms.

Fusion did not change recall@5 or MRR for the default model on these labels (embedding channel alone: 0.95 and 0.758). The fused ranking is kept as designed; these labels cannot show whether the keyword channel helps elsewhere.

## Consequences

- The default model is published under Google's Gemma terms. fbrain downloads it on the user's machine and does
  not bundle it; the README states this and names bge-m3 (MIT) as the alternative.
- Loading takes about 0.9 s on ext4 and 2.5 s on NTFS-through-WSL, which is why task 07 moves it into one resident process.
