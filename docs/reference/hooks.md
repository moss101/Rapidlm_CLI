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
process). Every hook receives a JSON object on stdin and has 5 seconds, unless
its entry sets `timeout_ms` (below).

## Object-form entries

An entry is either a command line (above) or an object:

```json
{
  "hooks": {
    "pre_tool_use": [
      "sh ./scripts/audit.sh",
      { "command": "sh ./scripts/no-force-push.sh", "matcher": "shell_exec", "timeout_ms": 3000 }
    ]
  }
}
```

| Key | Meaning |
|---|---|
| `command` | required; same bounds as a command-line entry |
| `matcher` | an anchored (whole-subject) regular expression, at most 256 bytes; the hook runs only when it matches the stage's subject. No `matcher` matches everything. Matching is linear-time (no backtracking) |
| `timeout_ms` | this hook's wall-clock budget in milliseconds; the default is 5000. A hook that sets it runs in a process group of its own, so a timeout ends everything it started (a hook without it is killed alone, as before, and keeps the terminal's Ctrl-C). A hook in its own group is bounded by its timeout and is **not** reached by a terminal signal or its terminal, so a hook that prompts on the terminal (`ssh`, `gpg`, `sudo`) should not set it |
| `continue_turn` | `stop` only: `true` lets this hook's `deny` send the turn back for another section ([below](#continuing-a-turn-from-stop)); any other stage rejects the key |

The subject a `matcher` is matched against depends on the stage: the tool's wire
name (`shell_exec`, `mcp__server__tool`) for `pre_tool_use`, `post_tool_use` and
`post_tool_use_failure`; the agent type for `subagent_start` and
`subagent_stop`; the stop reason (`budget_exhausted`, `cancelled`, …) for
`stop_cancelled`. The other stages have no subject and **reject** an entry with a
`matcher`.

The largest `timeout_ms` depends on how the stage blocks you: `user_prompt_submit`
30 000; `pre_tool_use` and `subagent_stop` 120 000; `stop` 600 000; every other
stage 30 000. A stage's latency is the **sum** of its hooks' timeouts. A managed
policy's `[hooks] max_timeout_ms` lowers every one of these and the default; a
hook over it is clamped and the gate says so.

An object with a key not listed above, a missing or empty `command`, or a value
out of range is **rejected, not skipped**: it does not run, every turn warns
naming the file and position (`hooks.pre_tool_use[2]`), and `rapid doctor` lists
it. (A malformed *string* entry is still skipped silently, as it always was.)
A hook that does not match its subject is skipped without any record, and a
skipped hook keeps its place: hooks are named by position in the configured list
(`pre_tool_use[1]`), so adding a matcher or a timeout to a command never renames
it and never orphans an approval that names it.

Entries identical in every field run once (the first wins) before the limit of 8
per stage is applied; the organisation's own entries come first. This includes
plain command lines: the same line twice — in one file, in both files, or in the
managed policy and a project file — runs once.

Positions differ between two places. A warning or a `rapid doctor` line names an
entry by its position **in its settings file** (`hooks.pre_tool_use[2]`, counting
a rejected neighbour); a hook's name in the ledger and in an approval
(`pre_tool_use[1]`) is its position in the merged, de-duplicated list. Fixing a
rejected entry therefore shifts the names of the hooks after it.

A binary from before the object form skips an object entry without a word, so a
project that relies on one to gate something should say so in its documentation;
`rapid doctor` notes how many entries use the form.

## Stages

| Stage | When | Input on stdin | Can it change the outcome? |
|---|---|---|---|
| `pre_tool_use` | before a tool call runs, after the permission check allowed it | `{"tool", "arguments"}` | yes: deny, ask, rewrite, add context |
| `post_tool_use` | after a tool call succeeded | `{"tool", "summary"}` | no; may add context |
| `post_tool_use_failure` | after a tool call failed (a failed result, invalid arguments, an MCP error result, or a runtime error — other than a cancellation — that ends the turn) | `{"event", "tool", "error"}` | no; may add context (not after a runtime error: nothing follows it) |
| `user_prompt_submit` | when a prompt is submitted, before a turn starts — typed, queued, the goal loop's, or from an ACP / `rapid daemon` client | `{"event", "prompt"}` | yes: a `deny` blocks the prompt |
| `stop` | when a turn (section) completes | `{"event", "tool_calls", "tokens"}`; with a `continue_turn` hook also `continuation_index`, `stop_hook_active`, `continuations_exhausted` | no — except a `continue_turn` hook: its `deny` sends the turn back ([below](#continuing-a-turn-from-stop)) |
| `stop_cancelled` | *instead of* `stop` when a turn ends without completing | `{"event", "reason", "tool_calls", "tokens"}` | no |
| `subagent_start` | before a delegated child agent runs | `{"event", "agent_type"}` | no |
| `subagent_stop` | when a delegated child agent finishes | `{"event", "agent_type", "status", "ok"}` | yes: a `deny` blocks the child's completion |
| `session_start` / `session_end` | once per `rapid exec` run or TUI session, at its start / on every exit path | `{"event"}` | no |
| `pre_compact` / `post_compact` | around a `/compact` | `{"event", "turns", …}` | no |
| `permission_requested` | when an approval request is created (a permission ask, a hook's ask, a plan, an MCP server's request for input, a clarification, a workflow wait) | `{"event", "approval_id", "tool", "summary", "source", "arguments_digest", "session_id"}` | no — an observer |
| `permission_resolved` | when an approval is resolved, from the terminal UI, ACP, the daemon or `rapid run --resolve` | `{"event", "approval_id", "tool", "decision", "remember", "surface", "session_id"}` | no — an observer |
| `notification` | an attention signal: a turn completed, a background child reported, a background job ended, a loop reported, a monitor was stopped for flooding, a goal paused/blocked/completed | `{"event", "kind", "title", "outcome", "job_id", "loop_id", "agent_id", "session_id"}` | no — an observer |

### Observer stages

`permission_requested`, `permission_resolved` and `notification` **cannot** block,
rewrite or add context, and they fail open: a hook that fails is a recorded
`hook.failed` (a line on stderr for a run with no ledger), never an effect on the
session. They also never delay what they observe. An event is queued and run on
one worker thread — a waiting permission event ahead of a waiting notification,
otherwise in the order emitted — so a slow hook cannot hold up creating an
approval, resolving one or finishing a turn. It can delay the hooks queued behind
it: each hook has its own budget (5 seconds unless `timeout_ms` says otherwise).
The hooks run with the session and turn in `RAPIDLM_SESSION_ID` and
`RAPIDLM_TURN_ID`, as every hook does.

The queue holds 32 events. When it is full the oldest **notification** is dropped
(the new one, when only permission events wait; a permission event is dropped only
to make room for another). `hook.dropped` (`stage`, `count`) is appended to the
ledger of the session whose events were lost: the first drop at once, later ones
together at most once a minute, and whatever is left when the process exits. A
run with no ledger says so on stderr. The process waits up to ten seconds at exit
for what is still queued, and says so on stderr (as a `{"warning": …}` line in JSON
mode) when that was not enough. A process that is interrupted or killed does not
wait.

The free text in a payload (`summary`, `title`) can come from the model, a tool's
arguments, an MCP server, a loop's answer or a job's label. It is scrubbed of the
active model's credential, flattened to one line and bounded (512 bytes for
`summary`, 200 for `title`), but it is **untrusted data**: a hook must never
interpolate it into a shell command.

Observer hooks run for a trusted project only, the managed policy's own included:
an untrusted project runs none.

- `permission_requested`: `source` is `lattice`, `hook:<position>` (for example
  `hook:pre_tool_use[0]`), `plan`, `mcp:<server>`, `clarification` or `workflow`.
  `matcher` matches the tool's wire name.
- `permission_resolved`: `decision` is `approved` or `denied`; `remember` is true
  when the answer asked to be remembered and a standing answer was recorded with
  it (false for an answer given once, and when the same standing answer already
  existed); `surface` is `tui`, `acp`, `daemon` or `workflow`. `matcher` matches
  the tool's wire name.
- `notification`: `kind` is one of `turn_complete`, `agent_finished`,
  `job_finished`, `loop_result`, `monitor_flooded`, `goal_changed`, and the
  `matcher` selects among them (`"turn_complete|job_finished"`). It is not
  derived from `notification.recorded` (one record per monitor line, no kind):
  the lines of a monitor never fire one — a monitor stopped for printing too
  fast (more than 40 lines in 2 seconds) fires `monitor_flooded` once — and
  `notification.recorded` is unchanged. One underlying end can raise more than
  one kind, because each is true: a background child that ends is an
  `agent_finished` and the job that carried it a `job_finished`; a flooded
  monitor is `monitor_flooded` and then a cancelled `job_finished`; a loop's turn
  is a `turn_complete` and the loop's result a `loop_result`. Select with the
  `matcher`.

  | `kind` | `outcome` |
  |---|---|
  | `turn_complete` | `ok` |
  | `agent_finished` | `ok`, `failed`, `cancelled` |
  | `job_finished` | `ok`, `failed`, `cancelled` |
  | `monitor_flooded` | `flooded` |
  | `loop_result` | `completed`, `failed`, `interrupted`, `waiting` |
  | `goal_changed` | `paused`, `blocked`, `complete` |

  `agent_finished` is told once the child's report is spooled and the job that
  carried it has ended: a hook that asks for the job finds it finished.

`session_id` is the session whose ledger the event belongs to; `turn_id` is added
when the firing thread is inside a turn. A `notification` hook for a
`rapid exec` run fires `turn_complete` for a run that completed; a run that was
interrupted, parked for approval or failed fires none.

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

## Continuing a turn from `stop`

"Do not stop until the tests pass" is a `stop` hook that opts in:

```json
{
  "hooks": {
    "stop": [{ "command": "sh ./scripts/tests-pass.sh", "continue_turn": true, "timeout_ms": 300000 }]
  }
}
```

When a section **completed** — the model gave a final answer with no tool call
pending — and a `continue_turn` hook prints a v2 result with `"decision": "deny"`
and a `reason`, the turn is not over. The host appends a note to the section's
own history and runs the next section:

```
You ended the turn with this answer:
<the answer, at most 4 KiB>

A stop hook (stop[0]) asked the turn to continue:
<untrusted_context locator="hook:stop[0]">
<the reason>
</untrusted_context>
```

The reason is a script's output, not an instruction from you: it is fenced like
every other hook-supplied text and the model decides what to do with it. It is
at most 2 KiB (several hooks share that, in hook order). The note reaches the
model as a result in its history — the way a finished job's notice does — not as
a message you wrote. The transcript shows one line,
`[stop hook stop[0] asked to continue (1/3)]`; `rapid exec` says the same on
stderr; and the ledger holds `hook.stop_continued` (hook, `continuation_index`,
`limit`, the reason and its digest) before the next section starts.

What decides whether it continues:

- **Only a completed section is asked.** An interrupt, an approval or question
  that pauses the turn, a model or tool failure, a repeated-call stop, a spent
  budget and a cancelled turn all end exactly as they do without the feature;
  the hooks of `stop_cancelled` hear them. An interrupt always wins, and an
  interrupt while a `continue_turn` hook is running ends the wait: the hook is
  ended, the rest of the stage does not run, and `stop_cancelled` fires with
  reason `cancelled`.
- **Anything but a clean `deny` lets the turn stop.** `allow`, no result, a
  non-zero exit, a timeout or an unreadable result mean "stop" (a defective hook
  cannot trap a turn); a failure is recorded as `hook.failed`. A `deny` from a
  string-form hook, or from an object-form one without `continue_turn`, is
  recorded and has no effect, as it always has.
- **Bounds.** At most `max_stop_continuations` per turn: `[hooks]
  max_stop_continuations` in `~/.rapidlm/config.toml` (default 3, at most 8, `0`
  turns the feature off), lowered — never raised — by the managed policy's key of
  the same name. The same reason twice in a row also ends the loop (a hook that
  cannot make progress). When a bound ends it, the turn finishes as it would
  have, the ledger holds `hook.stop_continuation_exhausted` (`limit`, `reason`:
  `count` or `repeat`), and the transcript says so. A `stop` hook is told it
  cannot be continued any further with `continuations_exhausted: true` (the count
  bound; the repeat bound is only known once the hook has answered).
- **One turn.** The sections are one turn: the kernel records one turn, the goal
  loop evaluates and accrues usage once, after the last section (the sum of the
  sections), the exit code of `rapid exec` is the last section's, and a
  `notification` hook hears `turn_complete` once. Tokens and tool calls count
  against the turn's budget across sections (a later section is left what the
  earlier ones did not use); each section has its own step cap, its own limit of
  tool calls and its own repeated-call guard, as a turn resumed after an approval
  does. A pause for approval keeps the count of continuations. A `stop` hook
  cannot complete a goal: completion is still derived from criteria, fresh
  evidence and verifier verdicts.

The payload of a `stop` stage that has a `continue_turn` hook adds
`continuation_index` (0 for the first section), `stop_hook_active` (this section
exists because a stop hook continued the turn) and `continuations_exhausted`; a
stage with none gets the payload it always did. `tool_calls` and `tokens` are the
section's own. `rapid exec --jsonl` adds one `hook.stop_continued` record per
continuation, only when there was one.

Main-agent turns only: a delegated child's end is `subagent_stop`.

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
  daemon: an error, and the decision is recorded on the session, which
  moves it on — refresh the session, with the SDK's `Session.refresh()`,
  before the next submit) and the prompt never enters the conversation
  history. ACP judges only a prompt the adapter would accept; the daemon
  runs no hook for an unknown session.
- `subagent_stop`: a `deny` replaces the child's report with
  `subagent (<type>) completion blocked by subagent_stop[<n>] hook: <reason>`,
  followed by what became of the child's changes. The hook decides before
  anything the child wrote is applied: only a child that completed and was
  not blocked is applied (automatically under `rapid exec`; held for
  `/agents integrate` in the TUI). A blocked or unfinished child's changes
  are held for review in the TUI and discarded under `rapid exec`; a failed
  or cancelled child's are discarded. `/agents` shows a blocked child as
  failed, and a child whose changes were applied as succeeded.

## Records

Every v2 decision is recorded as `hook.decided` (hook, stage, decision, the
digest of the reason, the tool and call when there is one); v1 hooks record
nothing. A `stop` hook that sends the turn back adds `hook.stop_continued`, and
a bound that ends the loop adds `hook.stop_continuation_exhausted`. See the
[event catalog](event-catalog.md).

## Managed policy

An organisation's managed policy (`RAPIDLM_MANAGED_CONFIG`) can narrow hooks
with a `[hooks]` table:

```toml
schema = "rapidlm.managed_config.v1"
[policy]
[hooks]
managed_only = true                  # only the hooks declared here run
denied_events = ["post_tool_use"]    # stages project settings may not use
max_timeout_ms = 60000               # no hook runs longer than this
max_stop_continuations = 2           # a stop hook may send a turn back this often (0 turns it off)
pre_tool_use = ["/opt/org/bin/review-gate"]
post_tool_use = [{ command = "/opt/org/bin/audit", matcher = "shell_exec", timeout_ms = 20000 }]
```

The policy's own hooks run first (at most 8 per stage, each non-blank and at
most 512 bytes — a policy that breaks this is refused, not trimmed). An entry may
be a string or a table with the keys of the object form; one the loader would
reject refuses the whole policy. `max_timeout_ms` (1–600000) lowers every stage's
own maximum and the 5-second default alike: a hook over it is clamped, never
dropped, and the gate reports `hooks.max_timeout_ms`. `max_stop_continuations`
(0–8) lowers the user's own limit (3 unless `[hooks] max_stop_continuations` in
their config says otherwise) and never raises it; it is read afresh for every
turn, so a restored session honors it. A binary from before these
keys refuses the whole policy (an unknown key, or an entry that is not a string),
so it runs no project hooks and says so; a fleet that mixes versions should
roll the policy out after the binaries. Project
hooks the policy drops, including those cut because the stage is full, are
reported
with the field, its origin and the remediation, for example
`hooks.managed_only enforced at '2 project hook(s) not run; …' (origin=managed)`.
A managed policy that cannot be loaded runs no project hooks.

## Checking hooks

`rapid doctor` lists every configured stage, flags a hook whose program path
does not exist, lists the rejected object-form entries (the first 16, as the
per-turn warning does) and warns with every hook the managed policy dropped; it
notes how many entries use the object form (a binary from before it skips them).
It never runs a hook. A `matcher` that is valid but never equals a subject (a
typo such as `Shell_Exec`) cannot be detected: the hook simply never runs. `rapid plugins hook-test` dry-runs
a plugin hook specification against a fixture event.
