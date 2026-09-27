# `[ui.status_line]` — the TUI's status row

The status row at the bottom of the interactive TUI shows the built-in items, a
command's output, or nothing. It is set in `[ui.status_line]` of the user's
`config.toml` (in the RapidLM home) or of the project's `.rapidlm/config.toml`;
the user's wins when both set one.

```toml
[ui.status_line]
type = "command"            # builtin | command | disabled (default: builtin)
command = "my-status-script" # one line; required when type = "command"
refresh_interval = 30        # seconds, 1 to 86400 (default: 60)
items = ["model", "context", "cost"]  # builtin only; default: every item
```

Built-in items: `model`, `sandbox`, `policy`, `context`, `goal`, `agents`,
`cost`, `connectivity` — shown in that order, whichever `items` names.

## A command status line

- The command runs through `sh -c`, **inside the sandbox** (the same tier
  `shell_exec` uses), with the project root as its working directory and a
  5-second timeout. Where this host has no sandbox tier the row says the command
  is unavailable; it is never run unsandboxed.
- It receives a JSON `rapidlm.status_payload` v1 document on stdin
  (`crates/protocol/src/status.rs`; fixture
  `crates/protocol/tests/fixtures/status/v1/status_payload.json`):
  `session_id`, `turn_id`, `model`, `effort`, `context.{used_tokens,
  limit_tokens, used_percent}`, `cost.{usd_micros, basis}`, `goal.{id, state}`,
  `worktree`, `workspace.{cwd, repo}`, `trigger` (`state` | `refresh_interval`).
  A fact the host does not have is `null`; an unknown cost is `null` with
  `basis = "unknown"`, never `0`.
- `RAPIDLM_SESSION_ID` and `RAPIDLM_TURN_ID` are exported (empty when unknown).
- It reruns when what the payload carries changes, and every
  `refresh_interval` seconds; one run at a time, off the UI thread.
- Up to five lines of its output are kept, each cut at 1024 characters, shown on
  the one row joined by ` | `, terminal control sequences neutralised.
- A run that fails or times out keeps the last output; with none yet, the row
  says why.
- **Trust:** a command named in a project's `.rapidlm/config.toml` runs only in
  a trusted project (`rapid trust grant`). A user's own config is the user's.
