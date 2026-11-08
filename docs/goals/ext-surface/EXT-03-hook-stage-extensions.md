# EXT-03 — Hook stage extensions: continuing a turn from `stop`, approval and notification observers, per-hook matcher and timeout

**Status:** draft behavior spec (not yet scheduled; see [README](README.md))  
**Size:** M (five slices; slices 1–3 are small and independent of slice 4)  
**Depends on:** nothing. Builds on ADR 0022 (hook result v2) and SEAM-01/SEAM-12 as delivered.  
**Invariants touched:** core 4, 5, 10, 13, 14; SEAMS S1–S5, S9, S11  
**Baseline:** `05ad9d6`. Every "Today" line is a reading of that tree and must be rechecked at Phase 0 (SEAMS §5.1); lines marked *(spot-checked)* were re-read by the author of this spec. Paths are under `apps/rapid/src/` unless a crate is named; `H` = `hooks.rs`, `I` = `interactive.rs`, `E` = `exec_tools.rs`, `M` = `managed_config.rs`.

---

## 1. Intent

Hooks decide, rewrite and annotate tool calls and prompts. Four practical needs are still unmet:

1. **A completion gate.** "Do not stop until the tests pass" cannot be expressed: a `stop` hook runs after the turn is already over and cannot change anything.
2. **Attention.** A user who walks away has no hook point for "the agent is waiting for you" or "a long background job finished".
3. **Selectivity.** Every hook of a stage runs for every call; a hook that cares only about shell commands has to filter inside its own script.
4. **Duration.** Every hook gets exactly five seconds, which is right for a policy check and wrong for a test run.

This spec adds, with no change for a configuration that uses none of it: object-form hook entries with a `matcher` and a `timeout_ms`; process-group termination so a longer timeout does not leak children; observer stages `permission_requested`, `permission_resolved` and `notification`; and an **opt-in** way for a `stop` hook to send the turn back for another section, bounded and recorded.

## 2. Today

- **Stages.** Twelve stages, each a `Vec<String>` field of one struct (`H:31-69`); adding a stage means editing three parallel lists: the parse table (`H:79`), `stages_mut` (`H:116`) and `named_stages` (`H:149`). Stage names are `&'static str` (`H:857`).
- **Settings shape.** An array of command strings per stage. Non-string entries, empty entries, entries over 512 bytes and non-array stage values are **skipped silently** (`H:93-100`); at most 8 per stage, re-applied after merging (`H:103`, `I:3286`). Both project settings files are merged by plain append with no deduplication (`H:167`, `I:2496, 3273`); there is no user-level hooks file; settings are re-read each turn (`I:3010`).
- **Execution.** `HOOK_TIMEOUT` is a constant 5 s *(spot-checked, `H:21`)*, passed at 16 call sites. `sh -c` with a cleared environment (`H:395`); `cmd /C` with `PATH` and base variables on Windows (`H:398`). stdin is written on a thread (`H:474`); stdout and stderr go to separate temp files, stdout capped at 64 KiB and stderr at 2 KiB (`H:427, 482`, `crates/protocol/src/hooks.rs:47`). A 10 ms poll; on timeout **only the direct child is killed** (`H:493-500`). `hooks.rs` does not use `process-signal` or `process-supervisor`, although both exist and the job code uses them (`crates/process-signal/src/lib.rs:85, 136`; `E:1992, 2079`).
- **Order.** Hooks of one stage run sequentially (`H:557, 864, 960`). Different calls in a batch run on separate threads (`E:10741`). Nothing deduplicates identical commands.
- **Results.** v2 results parse through `HookResult::from_stdout` (`crates/protocol/src/hooks.rs:212`). `pre_tool_use` denies on non-zero exit, timeout or an unreadable result (`H:568-632`); every observer stage fails open and records a `HookFailure` (`H:867-918`).
- **`stop`.** A `deny` from a `stop` or `stop_cancelled` hook is parsed and recorded with **no effect** *(spot-checked, `H:255-265`)*. The stage runs after `run_live_exec` returns and before `finish_turn` (`I:14789`); suspension records are already written by then. A completed run returns **no history** — only suspensions do (`crates/agent-runtime/src/turn.rs:2018`) — and every run starts a fresh `LoopState` that resets the 32-step cap (`turn.rs:18`), the 256-tool cap, the token budget and the repeated-call detector (`turn.rs:1396`; `agent_executor.rs:279`; `loop_guard.rs:12`). Fire sites: headless `I:5669`; TUI, ACP, daemon and `/loop` `I:14794`; approval continuation `I:14197`; a fallback wrapper `I:12848`.
- **Approvals.** `permission_for` decides (`E:3961`); the gate is `E:4195-4227`; hooks run only after the permission check allowed the call. All requests pass `ApprovalSink::request` (`approvals.rs:100, 631`) from six production sites: `E:4213` (lattice ask), `E:4406` (hook ask), `E:6172` (plan), `E:6575` (MCP), `I:14871` (clarification) and `workflow.rs:1096` (`record_human_wait`: tool `ask_user` or `workflow:<kind>`). Resolutions happen in four places: the TUI `/approvals` (`I:10134`), ACP and the daemon through `acp_resolve_and_continue` (`I:13670`, `acp_serve.rs:873`, `daemon_serve.rs:495`) and `rapid run --resolve` (`workflow.rs:1115`). The kernel crate cannot call hooks, so a resolution-side stage must fire from those call sites.
- **Notifications.** `notification.recorded` is an agent-mail style record, not an attention signal: it is written for a continued child's report (`I:12538`, `source: "agent <id>"`), for **every line** a monitor prints and for a monitor's flood stop (`I:12743, 12764`, `source: "monitor <handle>"`) and for a loop's result (`loops.rs:306`, `source: "loop <id>"`); its payload carries `source`, `text`, `outcome` and a `job_id`/`loop_id`, and **no kind token**. Job completion writes `job.completed`, never `notification.recorded` (`I:12687` ledger writer). An in-memory `SessionNotices` vector holds the user-facing notices (`I:6548`).
- **Managed policy.** A closed `[hooks]` table; string-only command lists (`M:796, 817`); `gate_hooks` (`M:836-902`) implements `managed_only` and `denied_events`. A binary that sees an unknown key in the table refuses the policy and runs no project hooks (`I:3295`).
- **Identity.** A hook's identity for provenance and approvals is `stage[index]` plus a digest of the command text (`H:375-380`, `approvals.rs:554`).
- **Dependencies.** `regex` is already a direct dependency of `apps/rapid` (`apps/rapid/Cargo.toml:18`; it is not a `[workspace.dependencies]` entry). The plugin engine's matcher is a glob and its timeout ceiling is 30 s (`crates/plugin-host/src/hooks.rs:46-49, 1607`) — a separate, unwired engine (ADR 0022).
- **Documentation drift to fix in the same change** (found while grounding): `docs/reference/hooks.md:176` says every other stage records `hook.failed`, but the five notify stages discard their results (`I:3385`); `:86` says a v2 result is read only after exit 0, but `pre_tool_use` reads and records one beside a non-zero exit (`H:575-598`); `:81` says `hook_specific` is passed to the record, but nothing reads it; `:4` names one settings file where two are merged; `:20-22` omits `RAPIDLM_SESSION_ID`/`RAPIDLM_TURN_ID` (`H:411`); `:39` says "exactly one … however it ends" but headless `--json-schema` early returns (`I:5601-5623`) and `/loop` turns (`I:7317`) bypass the wrapper; `doctor.rs:1402` says `sh -c` on every OS.

## 3. Behavior

### 3.1 Object-form entries

A stage entry is either a string (unchanged) or an object:

```json
{ "command": "scripts/check-shell.sh", "matcher": "shell_exec|workspace_patch", "timeout_ms": 20000 }
```

Keys: `command` (required, same bounds as a string entry), `matcher` (3.2), `timeout_ms` (3.3) and, on `stop` only, `continue_turn` (3.5). **An object with any other key, a missing or empty `command`, or an invalid value is rejected**, not skipped: the rejection is reported once per turn (one line, capped like the MCP rejections) and shown by `rapid doctor` with its position. Rejecting rather than ignoring is deliberate: a gating hook that silently vanishes is worse than a loud error. The existing silent skip of malformed *string* entries is left as is (S11) but documented.

- The cap of 8 entries per stage applies to the merged, deduplicated list.
- **Deduplication.** After merging managed, project and any other sources, entries with identical `(command, matcher, timeout_ms, continue_turn)` run once, first occurrence wins.
- **Identity is unchanged:** `stage[index]` plus the digest of `command` only. Adding a matcher or timeout to an existing command does not orphan a pending approval that names it.
- **Older binaries** skip object entries silently (`H:97`). A project whose safety depends on an object-form gating hook should say so in its documentation; see decision D-1.

### 3.2 Matcher

`matcher` is an anchored (whole-subject) regular expression, ≤ 256 bytes, compiled at load with the workspace `regex` crate (linear-time matching; no backtracking). An invalid expression rejects the entry.

The **subject** depends on the stage:

| Stages | Subject |
|---|---|
| `pre_tool_use`, `post_tool_use`, `post_tool_use_failure`, `permission_requested`, `permission_resolved` | the tool's wire name (`shell_exec`, `mcp__server__tool`) |
| `subagent_start`, `subagent_stop` | the agent type id |
| `stop_cancelled` | the stop-reason token (`budget_exhausted`, `repeated_tool_call`, …) |
| `notification` | the notification kind (3.4) |
| every other stage | none — a `matcher` on these is rejected |

An entry whose matcher does not match is skipped without any record. An absent matcher matches everything.

### 3.3 Timeout

`timeout_ms` is an integer; the default is today's 5 000 ms. The accepted range depends on how the stage blocks the user:

| Stage class | Maximum |
|---|---|
| `user_prompt_submit` (blocks the terminal UI and ACP loops, `I:7746`, `acp_serve.rs:485`) | 30 000 |
| `pre_tool_use`, `subagent_stop` | 120 000 |
| `stop` | 600 000 |
| observer stages (3.4) and all other stages | 30 000 |

A larger value rejects the entry. A managed `[hooks] max_timeout_ms` lowers every ceiling above (narrow-only, S5); a hook that exceeds it is clamped to it and reported by the gate with field, origin and remediation. The documentation states plainly that a stage's latency is the **sum** of its hooks' timeouts.

**Termination.** On timeout the hook's whole process group is terminated, not only the direct child, using the existing process-signal helpers (`isolate_process_group`, `terminate_process_group_default`). Where a platform cannot do that, the limitation is documented and covered by a typed-unavailability test: on such a platform object-form timeouts above 5 000 ms are rejected rather than leaking children.

### 3.4 New observer stages

Three stages that **cannot** block, rewrite or add context and that fail open (a defect in the hook is a recorded `hook.failed`, never an effect on the session):

- `permission_requested` — fired when an approval request is created, from each of the six request sites. Payload: `event`, `approval_id`, `tool`, `summary` (≤ 512 bytes, redacted), `source` (`lattice`, `hook:<position>`, `plan`, `mcp:<server>`, `clarification`, `workflow`), `arguments_digest`, `session_id`, `turn_id`.
- `permission_resolved` — fired at each of the four resolution sites. Payload: `event`, `approval_id`, `tool`, `decision` (`approved`/`denied`), `remember` (bool), `surface` (`tui`, `acp`, `daemon`, `workflow`), `session_id`.
- `notification` — an **attention** signal, defined here rather than derived from `notification.recorded` (which is too chatty — one record per monitor line — and carries no kind). It fires from the call sites that already know the event, with this closed set of kinds: `turn_complete` (a turn ended; `stop` site), `agent_finished` (a background or continued child reported; `I:12538` and the detached-child end), `job_finished` (a background process ended; the `job.completed` writer), `loop_result` (`loops.rs:306`), `monitor_flooded` (`I:12764`; **not** each monitor line) and `goal_changed` (a goal became paused, blocked or complete). Payload: `event`, `kind`, `title` (≤ 200 bytes, redacted), `outcome` (a short token such as `ok`/`failed`/`cancelled` where the source has one), `job_id`/`loop_id`/`agent_id` where applicable, `session_id`. It writes no new ledger record beyond `hook.failed`/`hook.dropped`, and `notification.recorded` is unchanged (S11). Phase 0 confirms each call site and may drop a kind whose site does not exist.

**Delivery is off the emitting path.** Observer hooks run on a bounded worker so a slow hook never delays creating an approval, resolving it or finishing a turn. Order is preserved per stage; the queue holds 32 events; when full the oldest is dropped and a single `hook.dropped` warning (count since the last warning, at most one per 60 s) is recorded. Each run is bounded by its own timeout. These stages need no new ledger event beyond `hook.failed` and `hook.dropped`.

`RAPIDLM_SESSION_ID` and `RAPIDLM_TURN_ID` are exported to every hook as they are today.

### 3.5 `stop` can continue the turn — opt-in

A `stop` entry in object form with `"continue_turn": true` may send the turn back for another section.

1. **When it is consulted.** Only when a section ended **completed** — the model produced a final answer with no pending tool call. It is never consulted for `stop_cancelled` outcomes: an interrupt, an approval pause, `context_required`, budget exhaustion, a model or tool failure and a repeated-call stop all end the turn exactly as they do now. A user interrupt always wins.
2. **How it answers.** The hook prints a v2 result with `decision: "deny"` and a `reason` (≤ 2 KiB): "do not stop yet; here is why". Any other decision, no result, a non-zero exit, a timeout or an unreadable result means "let the turn stop": a defective hook must not be able to trap a turn. Failures are recorded as `hook.failed`.
3. **What the model sees.** The host appends a host-authored user-role message — `A stop hook (<position>) asked the turn to continue:` followed by the reason inside the existing fence for hook-supplied text (`HookContext::fenced`, `H:250`; S4). The reason is data from a project-controlled script; the model decides what to do with it. The transcript shows one line `[stop hook <position> asked to continue (<n>/<limit>)]` (S3). When several `continue_turn` hooks answer `deny`, their reasons are joined in hook order, each fenced, within the 2 KiB total.
4. **Mechanics.** The turn re-enters the run with the section's history, as an approval continuation does (`host.rs:2062` `run_live_exec_seeded`). A completed run must therefore return its history (today only suspensions and subagent children do — `ChildHistory`, `E:900` — the precedent to follow). The host, not the run, owns the **cumulative** counters across sections: tokens against the turn's token budget, wall time, tool-call count. A continuation is refused — the turn ends through the normal `stop_cancelled` path with `budget_exhausted` — when the cumulative token budget is spent. The per-run caps (32 steps, 256 tools, the repeated-call detector) reset for each section as they do for an approval continuation.
5. **Bounds.** At most `max_stop_continuations` per kernel turn: user `[hooks]` key, default **3**, hard maximum 8 (the ceiling already used for length continuations, `user_config.rs:193`); a managed `[hooks] max_stop_continuations` lowers it, and `0` disables the feature. Two further stops: the **same reason digest twice in a row** ends the loop (a hook that cannot make progress), and a hook that needs to avoid looping receives the facts it needs (below). When a bound ends the loop the turn finishes through the normal `stop` path with `continuations_exhausted: true` in its payload, one transcript line, and the event `hook.stop_continuation_exhausted`.
6. **Payload.** The `stop` stage payload gains `continuation_index` (0 for the first section) and `stop_hook_active` (true when this section exists because a stop hook continued the turn). `stop` still fires exactly once per section.
7. **Goals and usage.** A continuation is inside one turn: the goal loop's evaluation and usage accrual happen after the last section, and accrual **sums** the sections (`I:14801` reads one run today). A stop hook cannot mark a goal complete — completion stays derived from criteria plus fresh evidence plus verifier verdicts (core invariant 8).
8. **Headless.** `rapid exec` continues in-process; the exit code derives from the final section; the JSONL stream gains one record per continuation **only when one occurred**, so a run without the feature is byte-identical.
9. **Scope.** Main-agent turns only. `subagent_stop` already blocks a child's completion; `stop` hooks do not run for children.
10. **Without the opt-in nothing changes:** a string-form `stop` hook, and an object-form one without `continue_turn`, that prints `deny` has no effect (today's behavior, kept by test).

### 3.6 Managed policy

The closed `[hooks]` table gains `max_timeout_ms` and `max_stop_continuations`; `denied_events` accepts the three new stage names; policy-declared hooks may use the object form (the managed parser stops being string-only). Managed hooks still run first, still at most 8 per stage, and a managed policy with an unknown key is still refused as a whole by older binaries (they run no project hooks — fail closed).

### 3.7 Visibility

The doctor `hooks` row (`doctor.rs:1364-1420`) lists object-form entries with their matcher and timeout, flags a rejected entry with its position and reason, shows whether any `continue_turn` hook is active and the resolved limits with the layer each came from, and says which platforms terminate process groups.

## 4. Configuration surface

```json
{
  "hooks": {
    "pre_tool_use": [
      "scripts/audit.sh",
      { "command": "scripts/no-force-push.sh", "matcher": "shell_exec", "timeout_ms": 3000 }
    ],
    "stop": [
      { "command": "scripts/tests-pass.sh", "continue_turn": true, "timeout_ms": 300000 }
    ],
    "notification": [
      { "command": "scripts/desktop-notify.sh", "matcher": "turn_complete|job_finished" }
    ],
    "permission_requested": [ "scripts/log-approval.sh" ]
  }
}
```

```toml
# ~/.rapidlm/config.toml
[hooks]
max_stop_continuations = 3

# managed policy
[hooks]
max_timeout_ms         = 60000
max_stop_continuations = 2
denied_events          = ["notification"]
```

## 5. Records and contracts

| Surface | Change |
|---|---|
| Ledger `hook.stop_continued` | `{ hook, continuation_index, reason_digest, reason (≤ 2 KiB, redacted) }` — appended before the next section starts (core invariant 4) |
| Ledger `hook.stop_continuation_exhausted` | `{ limit, reason: "count"\|"repeat" }` |
| Ledger `hook.dropped` | `{ stage, count }` |
| Stage names | three new names added to the parse table, `stages_mut`, `named_stages`, `HOOK_STAGE_COUNT`, doctor, the managed gate and `denied_events` |
| Event kinds | registered as in EXT-01 §5 (`define_event_kinds!`, `wire.v1.json`, SDK regeneration, `WIRE_SCHEMA_SHA256`) |
| `rapidlm.hook_result` | unchanged; `hook_specific` stays a pass-through |

## 6. Failure and edge behavior

- A `matcher` on a subject-less stage, a regex over 256 bytes and a `timeout_ms` of `0` or above the stage maximum each reject the entry naming the position.
- Observer queue overflow drops the oldest event and never blocks.
- A `continue_turn` hook that is slow: the turn waits for it up to its timeout; the user can interrupt, which cancels the wait and fires `stop_cancelled`.
- A continuation whose section produces another approval pause ends that section with `stop_cancelled` (`approval_required`) as any section would; the pause is resolved and its continuation proceeds normally; the count of stop continuations does not reset.
- A restart between sections: the continuation message and the count are in the ledger; a turn whose state cannot be rebuilt resumes as paused, not continued (core invariant 14).
- Windows: `cmd /C` and the documented limits; process-group termination uses the process-signal Windows path or is typed-unavailable and tested.

## 7. Acceptance criteria

- **AC-01** A configuration of strings only behaves byte-identically: every existing hook test passes unchanged, the timeout is 5 s, JSONL and exit codes are unchanged (S11).
- **AC-02** Object-form parsing: each valid shape, and each rejection (unknown key, empty command, bad regex, over-long matcher, matcher on a subject-less stage, timeout `0` / over the stage maximum, `continue_turn` off `stop`) is rejected with its position and appears in `rapid doctor` and the per-turn warning; the 8-per-stage cap counts the merged, deduplicated list.
- **AC-03** Matcher semantics: anchored full-match fixtures for built-in and MCP tool names, agent types, stop reasons and notification kinds; a non-matching entry produces no record and no process.
- **AC-04** Per-hook timeouts are honored; stage maxima are enforced; a managed `max_timeout_ms` clamps and reports (field, origin, remediation).
- **AC-05** A hook that spawns a grandchild which outlives the timeout leaves no process after the timeout (Unix); on a platform without group termination the over-5-second timeouts are rejected (typed-unavailability test).
- **AC-06** Deduplication across both settings files and the managed policy; a pending approval created for the bare-string hook is still answerable after a matcher and timeout are added to the same command (restore test).
- **AC-07** `permission_requested` fires once per request with the documented payload and `source` for each of the six request sites; `permission_resolved` fires from each of the four resolution surfaces with the right `surface` (one test each).
- **AC-08** `notification` fires once for each of the six kinds at its documented call site (one test each; a kind whose site Phase 0 cannot find is dropped from the spec, not stubbed); the matcher selects by kind; a monitor printing 100 lines produces no `notification` events; `notification.recorded` records are byte-identical to before.
- **AC-09** A deliberately slow observer does not delay approval creation or turn completion (ordering assertion: the approval is recorded before the hook finishes, no wall-clock threshold); overflow of the 32-event queue drops the oldest and records `hook.dropped` with the count.
- **AC-10** `stop` continuation happy path with a stub model: the section completes, the hook denies with a reason, the next section receives the fenced reason as a user-role message, the model answers, the hook allows, the turn ends; the ledger holds `hook.stop_continued`; the transcript marker is present; `stop` fired once per section with the right `continuation_index` and `stop_hook_active`.
- **AC-11** A string-form `stop` hook printing `deny`, and an object-form one without `continue_turn`, have no effect.
- **AC-12** Crash, timeout and unreadable output from a `continue_turn` hook let the turn stop and record `hook.failed`.
- **AC-13** The count bound, the managed lowering (including `0`) and the repeated-reason bound each end the loop with `hook.stop_continuation_exhausted` and `continuations_exhausted: true`; exceeding the cumulative token budget refuses the continuation and ends through `budget_exhausted`.
- **AC-14** An interrupt, an approval pause, `context_required`, a model failure and a repeated-call stop never continue (one test each), and a user interrupt during a hook's wait cancels it.
- **AC-15** Usage accrual sums the sections; a goal's completion still requires criteria and evidence (a hook cannot satisfy them).
- **AC-16** Headless `rapid exec`: the continuation happens; the exit code derives from the final section; the JSONL record is present only when a continuation occurred; a run without the feature has an unchanged stream.
- **AC-17** Managed: `denied_events` covers the new names; object-form managed hooks run first; the closed table rejects unknown keys; a restored session honors `max_stop_continuations`.
- **AC-18** The documentation drift items in §2 are corrected in the same change and `docs/reference/hooks.md`, `event-catalog.md` and ADR 0022 §3/§7/§8 are amended (a short ADR 0025 records the `stop` decision, since ADR 0022 §7 and `hooks.md:32` state that `stop` never gates).
- **AC-19** Revert cycle (S8) for every AC; Windows tests for parsing, matchers, observers and the continuation (stub model), with typed unavailability for group termination if needed.

## 8. Slices

1. **Object form, matcher, timeout, dedupe, managed keys** — AC-01…04, 06, 17 (partly). No new stages.
2. **Process-group termination** — AC-05.
3. **Observer stages** — three stages, worker, `hook.dropped`. AC-07…09.
4. **`stop` continuation** — history return, cumulative counters, bounds, records. AC-10…16.
5. **Doctor, documentation, ADR 0025.** AC-18.

## 9. Out of scope

- Running the hooks of one stage concurrently.
- A stage for a prompt queued during a turn, and a periodic heartbeat stage.
- Wiring the plugin-host hook engine into the exec loop (ADR 0022 "not done").
- Hooks that change model, parameters or the system prompt.
- `stop` hooks for subagents; any hook that completes a goal.

## 10. Decisions needed

| # | Decision | Options | Recommendation |
|---|---|---|---|
| D-1 | Older binaries skip object entries silently | (a) accept and document; (b) forbid the object form on gating stages until a minimum-version marker exists; (c) add a settings key older binaries refuse | (a), with a doctor warning on the newer binary when a file mixes forms and a note in the reference. (b) removes the most valuable use (a matcher on `pre_tool_use`). |
| D-2 | Notify stages record no `hook.failed` today although the docs say they do | (a) make the code match the docs; (b) change the docs | (a): the records are cheap and the docs describe the safer behavior. It is a visible change for existing configurations, so call it out in the changelog. |
| D-3 | Stage timeout maxima in 3.3 | as tabled; or one global maximum | As tabled; a UI-blocking stage must not accept ten minutes. |
| D-4 | Default `max_stop_continuations` | 3, 1, 0 (off until configured) | 3 once a hook opts in with `continue_turn`; the opt-in is the safety. |
| D-5 | Role of the continuation message | user-role host-authored message; system-role | User-role, labelled and fenced: keeps the provider's message order simple and the content visibly outside the system prompt. |

## 11. Contract surfaces to update

`docs/reference/hooks.md` (stages table, a new object-form section, the corrections in §2), `docs/reference/event-catalog.md`, `docs/reference/cli-command-reference.md:18, :134`, `docs/getting-started.md:40`, `docs/reference/managed-policy-signature.md` if the table is documented there, ADR 0022 (amended by ADR 0025), `sdk/typescript/schemas/wire.v1.json` with SDK regeneration and `WIRE_SCHEMA_SHA256`, `crates/protocol` fixtures only if the result shape changes (it does not). Tests to extend: `the_new_stages_parse_and_every_stage_is_named_once`, `parse_settings_hooks_object`, `hung_hook_is_denied_after_the_bounded_timeout`, `the_turn_end_stage_fires_stop_or_stop_cancelled_never_both`, `every_way_a_turn_ends_fires_the_turn_end_stage_exactly_once`, `the_hooks_table_is_closed_and_validated`, `denied_events_drop_only_those_stages_and_the_managed_hooks_go_first`, `the_hooks_check_reports_what_the_managed_policy_dropped`.
