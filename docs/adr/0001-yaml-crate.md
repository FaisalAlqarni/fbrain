# 0001: YAML crate for reading frontmatter

Status: accepted (task 01)

## Decision

fbrain reads note frontmatter with `serde_norway` 0.9. It is used only to read. Writing frontmatter is done as text edits (task 07), so no YAML serialiser is involved.

## Why

`serde_yaml` is deprecated. The two candidates were `serde_yml` and `serde_norway`.

- `serde_norway` continues the `serde_yaml` 0.9 API (a fork at cafkafk/serde-yaml, version 0.9.42 at the time of writing). The API is the one `serde_yaml` users already know, and it is on a 0.9 line rather than a 0.0.x line.
- `serde_yml` is at 0.0.13 on crates.io, a pre-release numbering with no stability signal. I did not run it against the fixtures.

`serde_norway` is the one the task 01 tests exercise. Through the `fbrain get --json` tests it:

- parses lists (`aliases: [wstyle]`) and unquoted dates (`valid_from: 2026-10-23`) as strings;
- keeps unknown keys (`owner: faisal`) in `Frontmatter.extra`;
- reports malformed YAML (`status: [unclosed`) as an error that `Note::parse` turns into `frontmatter_error`.

## Limits

- I compared the two crates by their published metadata, not by running both. If `serde_norway` stops receiving releases, the swap is local to `src/note.rs`.
- A numeric value in a string field (`valid_from: 2026`) fails to parse and shows up as `frontmatter_error`. Quote such values.
