# fbrain

fbrain keeps a second brain as plain markdown notes in a git repo, searches it (keywords plus a local embedding model), and gives coding agents the relevant notes at session start. Agents can read the brain and propose changes. Only you approve them.

It runs on your machine. After a one-time model download it uses no network, no cloud service and no LLM. It does not search code; use a code search tool for that.

## Install

Release archives and installers are built by GitHub Actions when a `v*` tag is pushed. The commands below work once a release exists at `https://github.com/FaisalAlqarni/fbrain/releases`.

Linux and macOS:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/FaisalAlqarni/fbrain/releases/latest/download/fbrain-installer.sh | sh
```

Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/FaisalAlqarni/fbrain/releases/latest/download/fbrain-installer.ps1 | iex"
```

The one-line installers download from the GitHub release, so they work only while the repository is public. For a private repository, authenticate and download the archive instead, for example `gh release download --repo FaisalAlqarni/fbrain --pattern 'fbrain-*-<target>.*'`, then unpack it and put `fbrain` on your PATH.

Or build it yourself with a stable Rust toolchain:

```sh
git clone https://github.com/FaisalAlqarni/fbrain
cd fbrain
cargo install --path . --bin fbrain
```

The installers put `fbrain` in `~/.cargo/bin` (or `$CARGO_HOME/bin`). Check with `fbrain --version`.

### Platforms

Release builds exist for:

- Linux x64 and arm64, glibc 2.39 or newer (Ubuntu 24.04 or newer)
- macOS on Apple Silicon
- Windows 11, x64 and arm64 (a recent build)

CI builds and tests Linux x64, macOS arm64 and Windows x64. The Linux arm64 and Windows arm64 builds are produced but not tested in CI.

### Intel macOS

There is no Intel macOS release because the `ort` crate has no prebuilt ONNX Runtime for it. You can build from source, but this procedure has not been executed end to end. The outline:

1. Build ONNX Runtime from source for x86_64 macOS, following the ONNX Runtime build instructions, as a shared or static library.
2. Tell `ort` where it is with `ORT_LIB_LOCATION=<path to the build output>` and `ORT_PREFER_DYNAMIC_LINK=1` if you built a shared library.
3. In `Cargo.toml`, turn off the `download-binaries` and `copy-dylibs` features of the `ort` dependency (keep the others), for example `ort = { version = "=2.0.0-rc.15", default-features = false, features = ["std", "ndarray", "tracing", "api-31"] }`. The `load-dynamic` feature, which loads the library at run time, is the other route.
4. Run `cargo install --path . --bin fbrain`.

If a step differs on your machine, the `ort` linking documentation is the authority.

## First run

```sh
fbrain init ~/brain
```

This creates a git repo with the standard layout (`ME.md`, `me/`, `setup/`, `projects/`, `inbox/`, `.fbrain.toml`), registers it as the default brain, and downloads the embedding model (about 190 MB) once. Pass `--no-model` to skip the download; search then uses keywords only. `fbrain doctor --fix` downloads the model later.

`init` runs `git init` but makes no commit. Make the first commit yourself.

To use a folder of notes you already have (an Obsidian vault, for example) without moving or editing anything:

```sh
fbrain init --adopt ~/vault
fbrain init --adopt ~/vault --include notes --include projects   # only walk these folders
```

To fill in your profile and project briefs from your repos:

```sh
fbrain onboard --repo ~/code/app1 --repo ~/code/app2
```

`onboard` scans the repos (git log, README, docs), asks only what the scan could not answer, and writes `ME.md` and one `BRIEF.md` per repo as proposals in `inbox/`. Review them as described below. `fbrain interview <project>` runs a fixed question set for one project, and `fbrain interview --review` walks stale notes and open questions.

## Daily use

```sh
fbrain search "how do we deploy"        # ranked notes with snippets and why they matched
fbrain get projects/app1/BRIEF          # print one note
fbrain get projects/app1/BRIEF --links  # with outgoing links and backlinks
fbrain lint                             # check the brain; --fix applies the mechanical fixes and commits them (terminal only, like review)
fbrain brief --harness claude-code      # print the session payload a hook would inject
```

Search returns current facts by default. `--history` includes superseded and expired notes, `--as-of 2026-01-01` judges validity at a date, and `--json` prints JSON.

Notes are markdown with optional frontmatter (`status`, `valid_from`, `valid_to`, `superseded_by`, `protected`, `private`, `projects`, `aliases`, `source`). A newer fact does not overwrite an older one: the older note gets `status: superseded` and a link to the new one. Edit notes in any editor; fbrain refreshes changed files before each search.

Settings live in `.fbrain.toml` in the brain; the keys are listed in [docs/config.md](docs/config.md).

### Review proposals

Agents and the `onboard` and `interview` commands never write to the notes. They write a file to `inbox/` with `fbrain propose` (the MCP `propose` tool does the same), after listing notes that may conflict or duplicate it.

```sh
fbrain inbox            # pending proposals, oldest first
fbrain review           # walk them: approve, edit, merge, supersede or reject
```

Approving a proposal applies it and makes one git commit for that proposal, without touching anything else you had staged. Rejecting deletes the proposal file. A proposal that touches a `protected` note needs `--force`.

`review` refuses to run without a terminal, or when `FBRAIN_HARNESS` is set, unless `--yes-i-am-faisal` is passed. This is a convention that keeps an agent from approving its own proposals by accident. fbrain does not stop an agent that has shell access and chooses to pass the flag. The real boundary is how you sandbox your agents.

The guard knows an agent is running through a marker, `FBRAIN_HARNESS`, that each install arranges as far as the harness allows:

- Claude Code: the SessionStart hook (`fbrain brief`) appends `export FBRAIN_HARNESS=claude-code` to `CLAUDE_ENV_FILE`, which Claude Code runs before every later Bash command of that session ([hooks](https://code.claude.com/docs/en/hooks)). Claude Code also sets `CLAUDE_CODE_CHILD_SESSION=1` in its Bash tool subprocesses ([variables](https://code.claude.com/docs/en/env-vars)), and fbrain honours that too. `CLAUDECODE=1` is not used: IDE extensions set it in your own terminals as well. Not covered: PowerShell commands on Windows without Git Bash never read `CLAUDE_ENV_FILE`.
- Codex: install adds `FBRAIN_HARNESS = "codex"` under `[shell_environment_policy.set]` in `config.toml` ([reference](https://learn.chatgpt.com/docs/config-file/config-reference)); uninstall removes it. Not covered: Codex documents no marker of its own, and a `shell_environment_policy` that excludes the variable (an `include` filter) hides it.
- opencode: the v1 plugin's `shell.env` hook ([plugins](https://opencode.ai/docs/plugins.md)) and the v2 plugin's `ctx.shell.hook("create.before")` ([v2 plugins](https://opencode.ai/v2/docs/build/plugins), beta) set it for every shell command. Not covered: opencode documents no marker of its own, and the `--agents-md` fallback without plugins sets nothing.

Without a marker, only the terminal check protects review. `fbrain doctor` reports a missing Codex marker or opencode plugin; the Claude Code marker depends on the SessionStart hook that doctor already checks.

### The resident process

Search needs the embedding model in memory. The first command that needs it starts `fbrain serve` in the background; one runs per user. It exits after 30 idle minutes (`serve_idle_minutes` in the user config changes this) and talks over a socket or named pipe that only your user can open. If it cannot start, `search` falls back to keywords and prints a notice.

Expect about 500 to 650 MB of memory while it runs with a few clients. The one-time full re-embed after a model or index format change embeds one text at a time and peaked at 630 MB on a 55-note and a 275-note brain (it peaked at 900 to 1,080 MB with the earlier batch of four). It takes a few minutes on a large brain; a `search` that waits longer than 120 s falls back to keywords for that call. `fbrain doctor` shows its pid, version, model and memory. `get`, `lint`, `brief`, `init` and `install` never start it. A client with a newer version makes the old process exit and start a new one (an upgrade). An older client, for example a long-lived `fbrain mcp` from before an upgrade, is refused with an error that says to restart that session, and the process keeps running. One process serves one model per user: a brain that uses another model gets an error naming both models, and the process keeps running. CPU only; no GPU is used.

## Agents

```sh
fbrain install claude-code           # also: codex, opencode-v1, opencode-v2
fbrain install claude-code --mcp     # also register `fbrain mcp` as an MCP server
fbrain uninstall claude-code         # removes exactly what install wrote
```

Each harness gets one hook that runs `fbrain brief` by absolute path at session start (and for subagents where the harness supports it). The payload is the profile from `ME.md`, the project brief for the current directory, and a few rules for the agent (search before answering from memory, propose instead of editing, never approve). It stays under 9,000 characters.

- Claude Code: hooks in `settings.json`; with `--mcp`, registered through the `claude` CLI.
- Codex: hooks in `hooks.json`. Codex asks you to trust a changed hook: run `/hooks` in Codex after every install.
- opencode: a plugin file in opencode's plugins folder; `--agents-md` adds a fallback block to the global `AGENTS.md`.

Before the first change to a harness config file, fbrain copies it to `<file>.fbrain-bak`.

`fbrain doctor` checks the binary path, the model and its checksums, the resident process, the brain, index freshness, each harness's wiring, and runs one timed search. `fbrain doctor --fix` repairs what it can: it downloads a missing model and re-installs a removed hook, using the binary path recorded at install time.

## Multiple brains

```sh
fbrain init ~/work-brain --name work
fbrain brains            # list; --which prints the active one
fbrain search --brain work "release checklist"
```

The active brain is chosen by `--brain`, then `FBRAIN_BRAIN`, then the first `[[map]]` entry in the user config whose path prefix contains the current directory, then the default. In a brain's `.fbrain.toml`, `[projects.<name>] repos = [...]` maps repo folders to a project so `brief` picks the right project brief.

## Git sync

A brain is an ordinary git repo, and fbrain only commits to it (one commit per approved proposal). Syncing is plain git:

```sh
cd ~/brain
git remote add origin <your private repo>
git push -u origin HEAD
git pull        # on another machine
```

Keep brain repos private. The search index lives in the OS cache directory and is rebuilt from the notes, so it is never synced.

On WSL, a brain on a Windows drive (`/mnt/<drive>`) without uid mapping can make git refuse the repo as having "dubious ownership". Run `git config --global --add safe.directory <path>`. fbrain's writes there skip permission changes the mount refuses.

## Privacy

- Everything stays on your machine. fbrain downloads the model once from Hugging Face and makes no other network request. `FBRAIN_OFFLINE=1` blocks even that.
- Notes with `private: true` in their frontmatter are never shown to agents: the MCP server, the session brief, `INDEX.md` and the CLI (`search`, `get`, `lint`, and the conflict lists of `propose`, `onboard` and `interview`) all leave them out, and the MCP counts skip them. A note whose frontmatter cannot be parsed is treated as private and protected until you fix it; plain `fbrain lint` still names it, as `frontmatter does not parse`, and reports nothing else about hidden notes. You see everything with `--private` on `fbrain search`, `fbrain get` and `fbrain lint`, and `propose`, `onboard` and `interview` read private notes without a flag. Both work only for you: stdin must be a terminal and no agent marker (`FBRAIN_HARNESS`, Claude Code's own marker) may be set, otherwise `--private` exits with code 3. This is a convention, as it is for `review`: fbrain does not stop an agent that has shell access and removes the markers. After upgrading, run `fbrain lint --fix` once so an older `INDEX.md` loses private titles.
- Writes never follow a symbolic link at the target on Linux and macOS: review and `lint --fix` open the parent folder first and write relative to it. On Windows the write checks the path and then renames, with no folder handle, so a symlink or junction swapped in between the check and the rename is not caught, and `is_symlink` does not see junctions at all. This matters only if another process can write inside your brain while you review; keep the brain in a folder only your user can write to.
- The resident process listens on a user-only socket or pipe and requires a token stored in a file only your user can read.

## Model licence

The default model is `google/embeddinggemma-300m`, used through the ONNX export `onnx-community/embeddinggemma-300m-ONNX`. It is published under Google's Gemma terms. fbrain does not bundle it. Your machine downloads it on `fbrain init` or `fbrain doctor --fix`, checked against pinned SHA-256 sums, and using it means accepting the Gemma terms.

To use `BAAI/bge-m3` (MIT licence, about 570 MB) instead, set it in the brain's `.fbrain.toml`:

```toml
model = "BAAI/bge-m3"
```

then run `fbrain doctor --fix` to download it. The index is rebuilt for the new model on the next command.

## Development

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test
```

Tests that need the embedding model read `FBRAIN_TEST_MODEL_DIR` (see [docs/config.md](docs/config.md)). Design decisions are in [docs/adr](docs/adr). Release builds use [cargo-dist](https://github.com/axodotdev/cargo-dist); pushing a `v*` tag runs `.github/workflows/release.yml`.

## Licence

fbrain is released under the MIT licence, see [LICENSE](LICENSE). The default embedding model has its own terms, described above.
