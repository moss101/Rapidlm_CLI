# Hooks

Hooks are commands a project runs at defined points of a session. They are
project settings: they live in `.rapidlm/settings.json` under `"hooks"`, run
only for a trusted project (`rapid trust grant`), and can request but never
grant capabilities. Contract decisions are recorded in
[ADR 0022](../adrs/0022-hook-result-v2-decisions-rewrites-and-context.md).

```json
{
  "hooks": {
    "pre_tool_use": ["sh ./scripts/review-gate.sh"],
    "post_tool_use_failure": ["sh ./scripts/explain-failure.sh"],
    "stop": ["sh ./scripts/notify-done.sh"]
  }
}
```

Each stage is an array of command lines (at most 8 per stage, each at most 512
bytes), run through `sh -c` (`cmd /C` on Windows) with a cleared environment
(`PATH`, `HOME`, `LANG`, `TMPDIR` and what the platform needs to start a
process). Every hook receives a JSON object on stdin and has 5 seconds.

## Stages

| Stage | When | Input on stdin | Can it change the outcome? |
|---|---|---|---|
| `pre_tool_use` | before a tool call runs, after the permission check allowed it | `{"tool", "arguments"}` | yes: deny, ask, rewrite, add context |
| `post_tool_use` | after a tool call succeeded | `{"tool", "summary"}` | no; may add context |
| `post_tool_use_failure` | after a tool call failed (a failed result, invalid arguments, an MCP error result, or a runtime error — other than a cancellation — that ends the turn) | `{"event", "tool", "error"}` | no; may add context (not after a runtime error: nothing follows it) |
| `user_prompt_submit` | when a prompt is submitted, before a turn starts — typed, queued, the goal loop's, or from an ACP / `rapid daemon` client | `{"event", "prompt"}` | yes: a `deny` blocks the prompt |
| `stop` | when a turn (section) completes | `{"event", "tool_calls", "tokens"}` | no |
| `stop_cancelled` | *instead of* `stop` when a turn ends without completing | `{"event", "reason", "tool_calls", "tokens"}` | no |
| `subagent_start` | before a delegated child agent runs | `{"event", "agent_type"}` | no |
| `subagent_stop` | when a delegated child agent finishes | `{"event", "agent_type", "status", "ok"}` | yes: a `deny` blocks the child's completion |
| `session_start` / `session_end` | once per `rapid exec` run or TUI session, at its start / on every exit path | `{"event"}` | no |
| `pre_compact` / `post_compact` | around a `/compact` | `{"event", "turns", …}` | no |

`stop` and `stop_cancelled` are exclusive: every section of a turn fires
exactly one of them, however it ends — including an early failure or a
crash. A turn that pauses for a human ends a section with `stop_cancelled`
(`approval_required` or `context_required` — a pause, not an abort); the
continuation that resumes it after the human answers is a section of its
own and fires its own `stop` or `stop_cancelled`. `stop_cancelled`'s
`reason` is a stable token — `cancelled`, `approval_required`,
`context_required`, `budget_exhausted`, `model_failed`, `tool_failed`,
`repeated_tool_call`, `empty_response`, `context_bound_exceeded`, `failed`
(an outcome with no stop reason), or `error` when the turn failed before
producing an outcome. A command that exits non-zero is a completed call —
its exit status is in the result — so `post_tool_use` fires for it, not
`post_tool_use_failure`.

## Two result contracts

**v1 — exit code.** A hook that prints nothing structured on stdout is a v1
hook. For `pre_tool_use`, exit 0 allows the call and a non-zero exit denies
it, with the hook's stderr (or, if stderr is empty, its stdout) as the reason
the model reads. Every other stage ignores the exit code except to record a
failure.

**v2 — a JSON result on stdout.** A hook whose stdout begins with `{` prints a
result and must print exactly one JSON object:

```json
{
  "schema": "rapidlm.hook_result",
  "version": 2,
  "decision": "deny",
  "reason": "writes under docs/ are generated",
  "updated_input": { "path": "docs/draft.md", "content": "…" },
  "additional_context": "the docs directory is regenerated nightly"
}
```

- `decision` is required: `allow`, `deny`, `ask` or `defer` (`block` is read as
  `deny`). `schema` and `version` are optional; if present they must be
  `rapidlm.hook_result` and `2`.
- `reason` (≤ 2 KiB) is what the model or the human reads.
- `updated_input` (≤ 8 KiB) replaces the call's arguments (`pre_tool_use` only).
- `additional_context` (≤ 4 KiB per hook) is shown to the model after the result.
- `hook_specific` is an object passed through to the record.
- Unknown fields are ignored. Keys that look like grants (`permissions`,
  `capabilities`, `lease`, …) are recorded as an attempt and ignored — a hook
  cannot widen what the session may do.

A v2 result is only read from a hook that exited 0. An unknown decision, a
wrong schema or version, invalid JSON, invalid UTF-8, text after the object,
or more than 64 KiB is an *unreadable result*: for `pre_tool_use` the call is
denied naming the hook; for the other stages the hook is ignored with a
recorded warning. A v1 hook may print any amount of plain text.

## What each decision does in `pre_tool_use`

- **allow** — the call proceeds.
- **defer** — no opinion; the call proceeds (the permission check has already
  run).
- **deny** — the call does not run; the model reads
  `<tool> blocked by pre_tool_use hook: <reason>`. A deny from any hook
  discards every rewrite in the stage.
- **ask** — a human decides, on the same approval surface a permission prompt
  uses. The pending approval names the call first, then the hook and its
  reason (`create notes.txt — pre_tool_use[0] hook asks: …`). In the TUI,
  `/approvals approve <n>` or `/approvals deny <n>` resolves it and the turn
  continues; the approved call runs exactly once. A restart keeps the
  question. In headless `rapid exec` the turn is parked and the process exits
  `10`; resolve it with `rapid resume <session>` and `/approvals`. Where no
  approval surface exists (an unattended `rapid cron` run), an ask is a
  denial that says so — never an implicit allow. `/approvals approve <n>
  remember` is not offered for a hook's ask: a persisted grant answers the
  permission check, and the hook would ask again.

An approval answers the hooks' question about *one* set of arguments: it
records the hook (`hook:pre_tool_use[0]#<command digest>`) and the SHA-256 of
the arguments shown. On the resume the hooks run again; a question from a
different hook, or about different (rewritten) arguments, is asked again.

When one stage raises more than one question about a call — a hook asks and
another hook's rewrite needs a permission prompt — they are one request about
the call as it would run, led by the asking hook's reason, and approving that
request answers them all. A hook that starts asking only on a later run is
asked then: an approval of another hook's question is not its answer.

## Rewrites

`updated_input` replaces the call's arguments when it:

1. is a well-formed tool call within the argument bound,
2. parses as the tool's documented arguments, and
3. is allowed by the permission check *as rewritten* — a hook cannot widen what
   the model may do by rewriting into it. If the rewritten call would need a
   permission prompt, the human is asked about the rewritten call.

Otherwise the call is denied naming the hook. The last hook in the stage that
rewrites wins, and each hook sees the call as the hooks before it left it; an
`updated_input` equal to what the hook was shown is not a rewrite, and neither
is a chain of rewrites that ends at exactly the arguments the model proposed.
A rewrite that runs is recorded (`hook.input_rewritten`, with both inputs,
their digests and every hook whose rewrite shaped the result), the result the
model reads ends with `[hook pre_tool_use[0], pre_tool_use[1] rewrote <tool>
input]` (each contributing hook), and the TUI shows
`✎ <tool>: rewritten by pre_tool_use[1] hook`.

While any `pre_tool_use` hook is configured, the `workspace_write` and
`workspace_patch` calls of one model step run one after another in the order
proposed (a hook may rewrite their path, so their proposed paths cannot be
used to decide which of them may run at the same time). Other tools are
grouped as without hooks.

## Additional context

A hook's `additional_context` follows the tool result it accompanies, fenced
as untrusted content and naming the hook:

```
<untrusted_context locator="hook:pre_tool_use[0]">
the docs directory is regenerated nightly
</untrusted_context>
```

It is bounded (4 KiB per hook, so a stage of several hooks can add several
blocks), scrubbed of known secrets like any tool output, and treated as data —
instructions inside it are not followed. The text cannot end its own fence:
`<untrusted_context` and `</untrusted_context` inside it (in any case) are
written `&lt;untrusted_context…`, and a line starting `DATA_URL:` is renamed so
it is never read as an image.

Context follows a result: a call that succeeded or failed (invalid arguments
included). A call that produced no result — denied, waiting for approval,
answered "needs more context", or stopped by an internal error — carries no
context.

## Failure semantics

- `pre_tool_use`: a hook that crashes, exits non-zero, times out or prints an
  unreadable result **denies** the call.
- Every other stage **fails open**: the hook is ignored, and the failure is
  recorded (`hook.failed`) — on headless also as a `warning:` line on stderr.
- `user_prompt_submit`: `ask` has no one to ask (the prompt's author is the
  human) and is recorded without effect. A blocked prompt in the TUI is shown
  with its reason and the text goes back into the composer; a blocked
  *queued* message stays queued and is marked held (`/queue` shows why, a
  restart keeps it) until `/queue edit`, `/queue run` or `/queue cancel` —
  the messages queued after it still run. A blocked goal-loop prompt stops
  the goal (`/goal run` again after changing the goal or the hook). Headless
  `rapid exec` prints the reason and exits `3` before any model request. An
  ACP or `rapid daemon` prompt that is blocked is refused before it becomes
  a turn — the client gets the reason (ACP: stop reason `refusal`; the
  daemon: an error, and the decision is recorded on the session, so re-read
  it before the next submit) and the prompt never enters the conversation
  history.
- `subagent_stop`: a `deny` replaces the child's report with
  `subagent (<type>) completion blocked by subagent_stop[<n>] hook: <reason>`,
  followed by what became of the child's changes. The hook decides before
  anything the child wrote is applied: only a child that completed and was
  not blocked is applied (automatically under `rapid exec`; held for
  `/agents integrate` in the TUI). A blocked or unfinished child's changes
  are held for review in the TUI and discarded under `rapid exec`; a failed
  or cancelled child's are discarded. `/agents` shows a blocked child as
  failed.

## Records

Every v2 decision is recorded as `hook.decided` (hook, stage, decision, the
digest of the reason, the tool and call when there is one); v1 hooks record
nothing. See the [event catalog](event-catalog.md).

## Managed policy

An organisation's managed policy (`RAPIDLM_MANAGED_CONFIG`) can narrow hooks
with a `[hooks]` table:

```toml
schema = "rapidlm.managed_config.v1"
[policy]
[hooks]
managed_only = true                  # only the hooks declared here run
denied_events = ["post_tool_use"]    # stages project settings may not use
pre_tool_use = ["/opt/org/bin/review-gate"]
```

The policy's own hooks run first (at most 8 per stage, each non-empty and at
most 512 bytes — a policy that breaks this is refused, not trimmed). Project
hooks the policy drops, including those cut because the stage is full, are
reported
with the field, its origin and the remediation, for example
`hooks.managed_only enforced at '2 project hook(s) not run; …' (origin=managed)`.
A managed policy that cannot be loaded runs no project hooks.

## Checking hooks

`rapid doctor` lists every configured stage, flags a hook whose program path
does not exist and warns with every hook the managed policy dropped; it never
runs a hook. `rapid plugins hook-test` dry-runs
a plugin hook specification against a fixture event.
