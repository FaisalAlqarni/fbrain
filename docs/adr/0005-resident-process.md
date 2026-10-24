# 0005: Resident process

Status: accepted (task 07).

## Decision

One `fbrain serve` process per user holds the embedding model and each brain's index. `search` and
`reindex` reach it through `serve::Client`; `get`, `init` and `doctor` never start it (`doctor` only asks
for its status).

- Endpoint: a Unix domain socket `serve.sock` in the runtime dir (`FBRAIN_RUNTIME_DIR`, else
  `$XDG_RUNTIME_DIR/fbrain` on Linux, else `<cache dir>/run`). The dir is created 0700 and refused when
  another user owns it. On Windows: the named pipe `\\.\pipe\fbrain-<user>` with the DACL
  `D:P(A;;GA;;;<user SID>)`, and the files in `<cache dir>\run`.
- Crate: `interprocess` 2.4 for both, through its local socket API. It sets the socket mode on Unix and
  takes a security descriptor on Windows, so one code path serves both; the std Unix socket plus a separate
  Windows pipe crate would have meant two. Windows SID lookup uses `windows-sys` directly.
- Protocol: newline-delimited JSON, one request and one reply per line, at most 8 MiB a line. The first
  line must be `Hello` with the token from `serve.token` (32 random bytes, 0600, new on every start);
  anything else gets `{"Error":"unauthorized"}` and the connection is closed. Unknown fields are refused;
  queries are limited to 10,000 characters, `k` 1,000 and a budget of 1,000,000; `Embed` to 256 texts.
- Before it sends the token, the client checks that the pid behind the endpoint is the one in `serve.pid`
  (Linux and Windows report it; macOS does not, and there the 0700 dir is the guard). On Windows it also
  checks that the pipe server's process runs as the same user (its token's SID), and opens the pipe with
  `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` through its own `CreateFileW` (interprocess does not
  expose the flags), so a squatting server can identify but not impersonate the client. The Windows
  runtime dir gets a protected DACL for the user alone (inherited by the token, pid and log files); a
  dir owned by another user (other than Administrators) is refused.
- An endpoint that fails these checks, a refused token, or a runtime dir owned by another user fails the
  command (exit 1). Every other failure falls back to keyword-only.
- Start: a client that cannot connect takes `start.lock`, connects again, and only then spawns
  `fbrain serve` by its absolute `current_exe()` path, detached (`setsid` on Unix, `DETACHED_PROCESS` on
  Windows), with stdin and stderr null and stdout a pipe it reads one ready line from, for up to 3 s.
  The server holds `serve.lock` for its whole life, so a second server that races past the start lock
  prints `busy` and exits.
- Handshake: a `Hello` whose version, model id or cache format differs from the server's, or that arrives
  after the model files changed on disk (size or mtime), gets `VersionMismatch`; the server then exits and
  the client starts a matching one. A changed model file therefore reaches the start-up checksum check.
- Exit: after `serve_idle_minutes` (user config, default 30) without a request, on SIGTERM or SIGINT, on a
  version mismatch, when `serve.token` or `serve.pid` is removed, or after 100 accept errors in a row.
  Every exit logs its reason, removes the socket (no new clients), lets requests in flight finish (up to
  10 minutes), then removes `serve.sock`, `serve.token`, `serve.pid` and `serve.lock`. On Windows a server
  with a console also cleans up on console close. The client removes `start.lock` after a start.
- Log: `serve.log` (0600) in the runtime dir holds starts, exits with their reason, panics (a panic hook),
  refused tokens and accept errors. It is started afresh past 1 MiB. `doctor` prints the last exit reason
  and the log path when no server runs. A panic in one request ends that connection only; the index lock
  it poisoned is recovered.
- Client: a pause of 50 ms between connect attempts, and the last reason in the final error. A reply
  timeout of 120 s for a search and 30 minutes for a reindex or embed, naming the server pid (Unix; named
  pipes have no timeouts). A connection the server closed is opened again once and the request resent,
  with a note naming the logged exit reason.
- Fallback: when the client cannot connect or start a server, a call to it fails, or the model is missing
  or fails its checks, `search` and `reindex` run keyword-only in the client and print
  `semantic search off: <reason>` (`resident process unavailable: <cause>` for a server that did not
  come up, `resident process failed: <cause>` for a failed call), and exit 0. `search --json` adds
  `"semantic": false` and `"semantic_reason"`. The keyword-only index lives in its own cache file
  (`index-keyword.json`), so a fallback never replaces the embedded `index.json`.
- Tokenisation uses `tokenizers` with the pure-Rust `fancy-regex` backend instead of `onig` (C), so the
  Windows MSVC build does not need the Oniguruma C library.

## Gate

Ryzen 9 9950X3D, release build, v2 labels (60 queries), EmbeddingGemma-300M q4, 2026-10-24. A fresh runtime
dir per run, so the first call includes the server start. Warm numbers are 5 timed runs per query after one
untimed pass, through `serve::Client`. Client RSS is the peak of the `fbrain search` processes.

| Metric | Threshold | ext4 | `/mnt/d` (NTFS via WSL) |
|---|---|---|---|
| First call, including server start | < 2000 ms on ext4 | 1182 ms | 2466 ms |
| Warm search p50 through the server | | 27.3 ms | 30.5 ms |
| Warm search p95 through the server | < 50 ms | 33.0 ms | 41.8 ms |
| Server RSS, brain indexed | < 800 MB | 498 MB | 490 MB |
| Client process peak RSS | < 50 MB | 11.7 MB | 11.7 MB |
| CLI search p95, server warm | | 35.2 ms | 52.8 ms |
| Recall@5 through the server (in-process) | ≥ 0.90 | 0.95 (0.95) | 0.95 (0.95) |
| Arabic recall@5 | ≥ 0.85 | 0.95 | 0.95 |

Re-run on ext4 after the fix round (tokenizer on `fancy-regex`, `reindex --full` first, so every vector
was made with it): first call 1171 ms, warm p50 27.0 ms, warm p95 33.3 ms through the server, server RSS
499 MB, client 11.9 MB, recall@5 0.95 (Arabic 0.95, MRR 0.76), the same as before the switch.

Every threshold passes on ext4. On `/mnt/d` the first call is over 2 s; the design sets that threshold for
ext4 only and asks for the `/mnt/d` number to be reported. Recall equals the task 06 numbers.

## Limits

- The first call on `/mnt/d` (2.5 s) is within 0.6 s of the client's 3 s start wait; a slower disk or a
  busy machine would push the first call to keyword-only with the notice.
- The server takes its cache dir and model id from the environment of the client that started it;
  `FBRAIN_CACHE_DIR` is not part of the handshake.
- `doctor` speaks the handshake too, so it makes a server of another version exit, after that server's
  requests in flight finish.
- The Windows code was type-checked for `x86_64-pc-windows-msvc`, not run; CI runs it on `windows-latest`.
  A full-crate check here stops at `ring` (C under `ureq`'s rustls), which needs an MSVC toolchain.
