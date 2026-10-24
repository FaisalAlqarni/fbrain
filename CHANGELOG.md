# Changelog

## 0.1.0 (unreleased)

First release.

- Brains: `init` (new or `--adopt` an existing folder), `brains`, `get`, `lint`, `reindex`.
- Search: BM25 plus EmbeddingGemma-300M embeddings (CPU, local), validity windows, one-hop link expansion, token-budget packing, explanations. `BAAI/bge-m3` is selectable.
- Resident process (`fbrain serve`, hidden): one per user, starts on demand, exits when idle. Search falls back to keywords with a notice when it is unavailable.
- Writes: `propose` writes to `inbox/` with conflict candidates; `inbox` and `review` apply, edit, merge, supersede or reject, one git commit per proposal.
- Interviews: `onboard` and `interview` produce proposals.
- Agents: `install` and `uninstall` for Claude Code, Codex and opencode; `brief` session payload; `mcp` stdio server; `doctor`.
- Releases: cargo-dist archives with shell and PowerShell installers for Linux x64 and arm64, macOS arm64 and Windows x64 and arm64. No Intel macOS build.
