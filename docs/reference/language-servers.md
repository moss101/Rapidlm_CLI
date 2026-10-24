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
  Starting one holds up only writes of its own language.
- Everything is advisory. A missing, slow, crashed or silent server costs a bounded wait and never fails a write.
  A server that cannot be started, or that exits, is reported once, then left alone for a minute. A server that
  dies before answering `initialize` fails the start at once rather than after the 20 s start timeout. After two
  writes in a row with no answer, a server is probed with a short wait instead of the full one until it answers
  again.
- Messages to a server are queued to a thread of its own, so a server that stops reading its input (busy indexing,
  stuck) cannot block a write, and ending the session cannot hang on it.
- The file is sent after any formatter has run, so the server sees what is on disk, followed by a save
  notification, because servers that run a checker on save (rust-analyzer's `cargo check`) report type errors only
  then. The file is **closed again** once the answer is in. It is on disk exactly as sent, and an open buffer would
  otherwise outlive a later change made some other way (a shell command, `/undo`, your editor) and make the next
  check of another file wrong.
- A file's URI is built from the project's resolved root and the cleaned relative path, never from the spelling the
  model used (`./a.rs`, `src//a.rs`), and compared with the server's after decoding escapes, so servers that escape
  `@`, `(`, `[` or a drive colon differently still match.
- A server that publishes an empty or partial set and a fuller one a moment later is waited on briefly, so the last
  answer wins. The wait is measured from that file's own last answer.
- Requests a server makes of the client are answered in the shape the protocol asks for (empty configuration,
  `applied: false` for edits) so it never stalls.
- Tools rooted elsewhere than the project, such as a subagent working in its own worktree, get no diagnostics: the
  server watches the project, and a draft from another tree would be checked against the wrong files.

## Trust

Servers run only in a trusted project, as the program named in the settings with your permissions, the same trust
as hooks and formatters. A managed policy that permits only its own hooks also stops them. Tools cannot write
`.rapidlm/settings.json` or `.claude/settings.json`, so the model cannot add a server of its own.

A server is given a **scrubbed environment**: `PATH`, `HOME`, locale, the toolchain families (`CARGO_*`, `RUSTUP_*`,
`RUST_*`, `NODE_*`, `GO*`, `VIRTUAL_ENV`, `JAVA_HOME`, ...) and `XDG_*`, and nothing whose name contains `TOKEN`,
`SECRET`, `PASSWORD` or `API_KEY`. Provider keys do not reach it.

Be aware of what a server does with a project. rust-analyzer's check on save runs `cargo check`, which executes
`build.rs` scripts and procedural macros; tsserver loads the project's plugins. Files the model writes can include
those, and they then run under the server, outside the shell tool's approval. If that matters for a project, leave
`lsp` off for it.

## Known limits

- A server that publishes without a `version` cannot be told apart from a publish for an earlier write that arrived
  late; the result can then describe the previous content of the file.
- Windows `.cmd` shims (npm-installed servers) are started through `cmd /C`; a server that detaches from its
  process group (`setsid`) is not ended with the group.
