# Changelog

## 0.1.1 (2026-10-24)

Fixes for macOS and Windows.

- macOS: the resident process now starts. It failed with "cannot listen: unsupported" because a socket's file mode cannot be set there, so search stayed keyword-only. The socket's 0700 folder and the token keep other users out.
- Windows: a server started by `fbrain search` no longer inherits the search's stdout and stderr. A program reading that output (an agent's shell tool, for example) waited until the server's idle exit, up to 30 minutes.
- Windows: Claude Code and opencode config is found under `USERPROFILE`, where those tools look.
- Windows: brain paths are stored and shown as `C:\...`, without the `\\?\` prefix.
- macOS: a refused permission change reported as `ENOTSUP` is skipped like other unsupported ones, instead of failing a review.
- Release builds: Windows links the dynamic MSVC runtime (install the Visual C++ Redistributable), macOS targets 13.4 and links clang's runtime.

## 0.1.0 (2026-10-24)

First release.

- Brains: `init` (new or `--adopt` an existing folder), `brains`, `get`, `lint`, `reindex`.
- Search: BM25 plus EmbeddingGemma-300M embeddings (CPU, local), validity windows, one-hop link expansion, token-budget packing, explanations. `BAAI/bge-m3` is selectable.
- Resident process (`fbrain serve`, hidden): one per user, starts on demand, exits when idle. Search falls back to keywords with a notice when it is unavailable.
- Writes: `propose` writes to `inbox/` with conflict candidates; `inbox` and `review` apply, edit, merge, supersede or reject, one git commit per proposal.
- Interviews: `onboard` and `interview` produce proposals.
- Agents: `install` and `uninstall` for Claude Code, Codex and opencode; `brief` session payload; `mcp` stdio server; `doctor`.
- Releases: cargo-dist archives with shell and PowerShell installers for Linux x64 and arm64, macOS arm64 and Windows x64 and arm64. No Intel macOS build.
