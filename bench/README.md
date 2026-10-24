# Benchmark runner

`fbrain-bench` measures `fbrain search` on a labelled query set: recall at 5, MRR, warm and cold
latency, peak memory, and recall per language. It can run obsidian-mind's `om` and `qmd` on the same
queries and measure the embedding model's load time and memory.

```
cargo run --release -p fbrain-bench -- --brain <brain> --queries <file> \
  [--variant model=<id>]... [--baseline-om --om-script <om-mcp.mjs>] \
  [--baseline-qmd [--qmd-index <name>]] [--measure-model] [--out <dir>] \
  [--min-recall F] [--max-warm-p95-ms N] [--max-cold-p95-ms N] [--max-rss-mb N]
```

Build `fbrain` in the same profile first (`cargo build --release -p fbrain`); cold runs start the `fbrain`
binary that sits next to `fbrain-bench`, or `--fbrain-bin`. A missed limit prints
`threshold failed: <metric> <value> <op> <limit>` and exits 1. Limits apply to fbrain variants only.

Metrics:

- `recall_at_5`: share of queries with an expected note among the first 5 notes returned.
- `mrr`: mean of 1 / rank of the best expected note within the first 10, 0 when absent.
- `warm_p50_ms`, `warm_p95_ms`: in-process search time, 5 runs per query after one untimed pass.
- `cold_p95_ms`: wall time of a fresh `fbrain search` process per query.
- `peak_rss_mb`: largest peak resident memory of those processes (unix only).

`--out <dir>` also writes `report.json`.

## Query file

```toml
[[query]]
id = "q01"
text = "consent policy"
lang = "en"            # en | ar | mixed
expect = ["projects/alpha/consent.md"]   # note paths relative to the brain, best first
```

`bench/fixture-queries.toml` holds five queries for `tests/fixtures/brains/search`; `bench_smoke`
runs them. Query sets for a real brain contain its notes, so they and their reports stay out of this
repo: keep them in the brain's `.fbrain-bench/` folder or in `bench/private/` (gitignored).

## Write-path timing

Wall time of one `fbrain` process on the `basic` fixture brain (a fresh git repo per run), 10 runs
each, debug build, model not downloaded so `propose` ran keyword-only. Measured at task 09; report
only, no limit applies.

| Command | p50 | p95 |
|---|---|---|
| `propose` (add) | 6 ms | 7 ms |
| `review <id> --approve --yes-i-am-faisal` | 19 ms | 21 ms |

## MCP server timing and memory

`fbrain mcp` searches (a release build) over the task 05 ext4 bench copy (198 notes, EmbeddingGemma
q4, index cache already built), 50 labelled queries times 3 passes after one warm pass, wall time of
one `tools/call` for `search` as the client sees it, on the Ryzen 9 9950X3D. Measured at task 14;
report only, the limits are the 50 MB per client and 800 MB resident process rows of the design.

| Case | p50 | p95 |
|---|---|---|
| 1 client | 22 ms | 26 ms |
| 3 clients searching at once (450 calls) | 64 ms | 80 ms |

Peak resident memory (`VmHWM`) with 3 `fbrain mcp` clients sharing one resident process: clients
19 MB, 14 MB and 14 MB; resident process 640 MB (447 MB resident at the end).

The first run after the cache format version changed re-embedded the whole copy inside the first
`search`, and the resident process peaked at 932 MB there (910 MB at the end). That is above the
800 MB row; it was a one-time rebuild after the index format change in this task, not the steady
state. The first `search` of that run
waited about 55 s for the rebuild.
