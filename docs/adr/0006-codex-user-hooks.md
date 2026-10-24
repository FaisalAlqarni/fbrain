# 0006: Codex user hooks

Status: accepted (task 12).

## Decision

`fbrain install codex` writes into `$CODEX_HOME/hooks.json` (default `~/.codex/hooks.json`). No Codex
plugin is shipped and `config.toml` is not touched.

Shape, the same as Claude Code's `settings.json` hooks:

```json
{"hooks": {
  "SessionStart": [{"matcher": "startup|resume|clear|compact|fork",
                    "hooks": [{"type": "command", "command": "<abs path to fbrain> brief --harness codex"}]}],
  "SubagentStart": [{"hooks": [{"type": "command", "command": "<abs path to fbrain> brief --harness codex"}]}]
}}
```

Install adds one group per event, keeps every other key and hook, and records the two entries (file, JSON
pointer `/hooks/<event>`, command) in `installs.json`. Uninstall removes only those commands.

## Sources

- Codex hooks documentation, https://developers.openai.com/codex/hooks (redirects to
  https://learn.chatgpt.com/docs/hooks), read 2026-10-24. It lists `~/.codex/hooks.json` and
  `~/.codex/config.toml` as user-level hook locations next to plugin-bundled hooks, gives the
  `hooks.<Event>[{matcher, hooks:[{type, command}]}]` shape, needs no feature flag (hooks are on by
  default), and says Codex runs a new or changed non-managed hook only after it is trusted in `/hooks`.
  The page was read through a summariser; its `SubagentStart` entry has no example, so the matcher-less
  group for that event follows the fx plugin's own `hooks.json`.
- `/development/fx/hooks.json`: the same shape with `SessionStart` and `SubagentStart` groups, and the
  `startup|resume|clear|compact|fork` matcher.
- A working `~/.codex/hooks.json` on the development machine (another tool's hooks) uses the same shape.
  It was read only.
- `/development/fx/INSTALL.md` registers fx as a plugin; that route is not needed for user hooks.

## Trust

Codex skips a hook it has not trusted, and a changed hook needs trusting again, so install prints
`Codex: run /hooks in Codex and trust the fbrain hook (required after every install or change).` and
`fbrain doctor` prints a `warn: harness:codex` reminder whenever Codex is installed.
