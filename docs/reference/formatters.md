# Formatters

A project can have files normalised as the agent writes them. Configure formatters in
`.rapidlm/settings.json` (or the compatible settings file):

```json
{
  "formatters": [
    { "command": ["rustfmt", "--edition", "2024"], "globs": ["*.rs"] },
    { "name": "prettier", "command": ["prettier", "--stdin-filepath", "{path}"],
      "globs": ["*.ts", "*.tsx", "*.json"], "timeout_ms": 15000 }
  ]
}
```

| Field | Meaning |
|---|---|
| `command` | argv; the first word is the program. No shell is involved. `{path}` is replaced by the file's project-relative path |
| `globs` | required; which files the formatter claims. A pattern without `/` matches the file name at any depth (`*.rs` covers `src/a.rs`); one with `/` matches the whole relative path |
| `name` | shown to the model; defaults to the program name |
| `timeout_ms` | default 10 000, at most 120 000 |

The first formatter whose globs match a file is the only one that runs. The command runs in the project root.

## How it works

The file's content goes to the formatter's standard input and its standard output is what gets written, **before**
the file touches the disk. The write stays atomic, the change record in the ledger is of the final bytes, and a
formatter can never leave a half-formatted file. A formatter must therefore read standard input and write standard
output (`rustfmt`, `prettier --stdin-filepath`, `black -`, `ruff format -`, `gofmt`, `shfmt`, `clang-format
--assume-filename` all do).

When a formatter changes the content, the tool result tells the model that the file on disk differs from what it
sent and to read it again before editing it, so later patches are not made against text that no longer exists.

A formatter is a convenience, not a gate. If it fails, times out, cannot be started, or returns nothing for a
non-empty file, the file is written exactly as sent and the result says why, including the end of the
formatter's error output. A formatter that rejects the input has usually found a syntax error, which is useful
for the model to see.

Subagents write through the same formatters. Entries that are malformed (no `globs`, an empty command) are dropped
and reported once when the settings are loaded.

Formatters run only in a trusted project, as the program named in the settings with your permissions, the same
trust as hooks.
