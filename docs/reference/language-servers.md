# Language-server diagnostics

When the agent writes a file, RapidLM can ask the language server for that file's type what it makes of the new
content, and hand the problems to the model in the same tool result. A type error or a missing import is found in
the step that made it, not a build later.

It is **opt-in**. In `.rapidlm/settings.json`:

```json
{ "lsp": true }
```

turns on the built-in servers whose programs are on `PATH`:

| Server | Program | Files |
|---|---|---|
| `rust` | `rust-analyzer` | `.rs` |
| `typescript` | `typescript-language-server --stdio` | `.ts .tsx .js .jsx .mjs .cjs` |
| `pyright` | `pyright-langserver --stdio` | `.py` |
| `go` | `gopls` | `.go` |
| `clangd` | `clangd` | `.c .cc .cpp .cxx .h .hh .hpp` |

An object configures more:

```json
{
  "lsp": {
    "defaults": true,
    "servers": {
      "rust": { "command": ["rust-analyzer"], "extensions": ["rs"] },
      "svelte": { "command": ["svelteserver", "--stdio"], "extensions": ["svelte"],
                  "language_id": "svelte", "initialization_options": {} },
      "pyright": false
    },
    "wait_ms": 3000,
    "min_severity": "error"
  }
}
```

| Key | Meaning |
|---|---|
| `defaults` | include the built-in servers found on `PATH` (default `true`) |
| `servers.<name>` | a server of your own, or a replacement for a built-in of the same name; `false` removes a built-in. `extensions` is required |
| `wait_ms` | how long to wait for a server's answer after a write (default 3000, at most 30000) |
| `min_severity` | `error` (default), `warning`, `information` or `hint`: the least serious problem reported |

## What the model sees

```
wrote 482 bytes to src/lib.rs
lsp (rust): 2 problem(s) in src/lib.rs (1 error(s)); fix them or say why they are expected
  src/lib.rs:12:5 error [E0308] mismatched types
  src/lib.rs:30:1 warning unused variable: `x`
```

A file the server finds clean is reported as `no problems reported`, so silence means the server said nothing in
time, not that all is well. At most 10 problems are listed, each as its first line, stripped of control characters.

## Behaviour

- A server is started the first time a file of its type is written, kept for the session (and shared by its
  subagents), and stopped, with the helpers it started, when the session ends. Servers are not restarted every turn.
- Everything is advisory. A missing, slow, crashed or silent server costs a bounded wait and never fails a write.
  A server that cannot be started is reported once, then left alone for a minute. After two writes in a row with no
  answer, a server is probed with a short wait instead of the full one until it answers again.
- The file is sent after any formatter has run, so the server sees what is on disk. A save notification follows the
  change, because servers that run a checker on save (rust-analyzer's `cargo check`) report type errors only then.
- A server that publishes an empty or partial set and a fuller one a moment later is waited on briefly, so the
  last answer wins.
- Requests a server makes of the client (configuration, progress, capability registration) are answered with empty
  results so it never stalls.
- Servers run only in a trusted project, as the program named in the settings with your permissions, the same
  trust as hooks and formatters. A managed policy that permits only its own hooks also stops them.

Tools cannot write `.rapidlm/settings.json` or `.claude/settings.json`, so the model cannot add a server of its own.
