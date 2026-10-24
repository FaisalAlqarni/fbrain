# fbrain

Rust CLI (binary, crate and command: `fbrain`). A brain is a git repo of markdown notes; fbrain reads them, searches them and proposes changes.

## Module layout

- `src/main.rs`: parses the CLI and exits 1 with the error text on failure.
- `src/cli.rs`: the command surface; every command calls into the modules below.
- `src/config.rs`: user config (named brains, default, `[[map]]` repo prefixes), brain selection, config and cache dirs.
- `src/brain.rs`: `init`, `adopt`, `Brain::open`, note walking and lookup.
- `src/note.rs`: frontmatter and body parsing (LF and CRLF).
- `src/serve.rs`: the resident process (`fbrain serve`, hidden) and its client: endpoint per OS, protocol, start lock, idle exit, version handshake. `search` and `reindex` go through `serve::Client`; `get`, `init`, `doctor` (status only) never start it. ADR 0005.
- `src/proposals.rs`: `propose`, `list`, `get`; the target and supersedes checks. `src/review.rs`: `apply` (the only code that writes notes), the interactive loop, atomic writes, git commits through `brain.rs` helpers.
- `src/mcp.rs`: `fbrain mcp`, a stdio MCP server (search, get, propose, health) on `rmcp`; a thin client of the resident process. `search::expose` is the one gate that keeps `private` notes from MCP and from the session brief.
- Later tasks add `interview`.
- `tests/common/mod.rs`: the only `Command` builder for tests. `tests/fixtures/brains/basic/` is the fixture brain.
- `docs/adr/`: decisions.

## Commands

Use the rustup toolchain: `export PATH="$HOME/.cargo/bin:$PATH"`.

- One test file: `cargo test --test <name>`
- Scope and all: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
- Setup: `cargo fetch`

## Test rules

Every test builds its `Command` through `tests/common` (`Sandbox::cmd()`, or `Sandbox::std_cmd()` to spawn and keep a process). It points `FBRAIN_CONFIG_DIR`, `FBRAIN_CACHE_DIR`, `FBRAIN_RUNTIME_DIR`, `HOME`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `XDG_*`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME` at temp dirs, puts stub `claude`, `codex` and `opencode` first on `PATH`, and sets git identity with signing off. Never build a `Command` another way. Dropping a `Sandbox` stops the resident process named by its own `run/serve.pid` (after checking the pid is that sandbox's server), so a test never leaves a server running and never touches the user's real one.

## Env vars

- `FBRAIN_CONFIG_DIR`, `FBRAIN_CACHE_DIR`: override the OS config and cache dirs.
- `FBRAIN_BRAIN`: brain name or path, below `--brain` and above the repo mapping.
- `FBRAIN_TEST_MODEL_DIR`: local model files for tests (used from task 03 on).
- `FBRAIN_RUNTIME_DIR`: the resident process's directory (socket, token, pid and lock files). The bench gate sets it to an unused dir so the first call includes the server start.

Test-only (documented here, not for users):

- `FBRAIN_SERVE_IDLE_SECS`: idle timeout of the resident process in seconds, above `serve_idle_minutes`.
- `FBRAIN_TEST_SERVE_DELAY_MS`: the server sleeps this long before it starts (so before its ready line).
- `FBRAIN_TEST_SERVE_FAIL=1`: the server exits with an error before its ready line.
- `FBRAIN_TEST_FAKE_VERSION`: the version the server reports in the handshake and in `doctor`.
- `FBRAIN_TEST_SERVE_REQUEST`: `fail` (every request but `Status` answers an error), `stall:<ms>` (the server waits before each answer) or `panic-once` (the first search panics).
- `FBRAIN_TEST_CALL_TIMEOUT_MS`: replaces the client's reply timeout.

The server logs starts, exits with their reason, panics, refused tokens and accept errors to `<runtime dir>/serve.log` (0600); `doctor` prints the last exit reason and the log path when no server runs.
