# ADR 0025 — A `stop` hook may send a turn back: opt-in, bounded, recorded

**Status:** Accepted for `EXT-03` (slices 1–5 built)  
**Date:** 2026-10-25  
**Amends:** [ADR 0022](0022-hook-result-v2-decisions-rewrites-and-context.md) §7 (`stop` and `stop_cancelled` observe and never gate) and §8 (the managed `[hooks]` table)  
**Builds on:** ADR 0022 (hook result v2, the fence for hook-supplied text), the turn loop's approval continuation; invariants 4, 5, 8, 10, 13, 14; `SEAMS §3.2 S1, S3, S4, S5, S9, S11`

## Context

- ADR 0022 §7 and `docs/reference/hooks.md` said `stop` never gates: the stage runs after the turn's run has returned and a `deny` is recorded with no effect. "Do not stop until the tests pass" could not be expressed.
- A completed run returned no history (only a suspension did), and every run started with a fresh step cap, tool cap, token budget and repeated-call guard. The one precedent for running a turn in more than one go was the continuation after an approval, which seeds a second run from the first one's recorded history.
- The history the request layer replays holds tool exchanges and nothing else: there is no user-role message the host can append.
- Hooks could not be ended once started (only at their timeout), so a person's interrupt did not reach a hook that was running.

## Decision

1. **It is opt-in, per hook.** A `stop` entry in object form with `"continue_turn": true` may send a turn back. A string-form entry, an object without the key, and every other stage keep today's behavior: their `deny` is recorded and has no effect (kept by test). The stage payload gains `continuation_index`, `stop_hook_active` and `continuations_exhausted` only when the stage has such a hook, so a configuration that does not use the feature sees the bytes it always did (S11).
2. **Only a completed section is asked, and anything but a clean `deny` lets it stop.** An interrupt, an approval or question that pauses the turn, a model or tool failure, a repeated-call stop and a spent budget end exactly as before, through `stop_cancelled`. `allow`, no result, a non-zero exit, a timeout and an unreadable result mean "stop" and are recorded as `hook.failed`: a defective hook cannot trap a turn.
3. **Another section runs from the last one's history.** A completed run now returns its history; the host appends a note and runs the next section as a resumed approval is run (`run_live_exec_seeded`), with the same context, the same model (lent to each run) and the same tools. The first section of every turn is run exactly as it was.
4. **The note is a tool result, not a user message.** The spec recommended a user-role message (D-5). The history has no such thing, and adding one means every consumer of a history (the request layer, the suspension record, the stall detector, the child history) learns a second kind of entry. A synthetic `stop_hook` exchange is the channel a finished job's notice and a delivered message already use. Its text carries the answer the section ended with (the model's history never holds its own answers, and without it the model would not know what it had told the person; at most 4 KiB) and each hook's reason inside the fence for hook-supplied text (S4), scrubbed of the credentials the session knows and cut to 2 KiB for all hooks together. The reason is a project script's words, not the person's.
5. **The sections are one turn.** The kernel records one turn; the result is the last section's with the tokens, tool calls, model steps and cost of all of them; a goal accrues once, after the last section; `rapid exec` exits with the last section's code; `turn_complete` is announced once. A later section is left the turn's budget less what the earlier ones used (zero is exhausted, and ends as `budget_exhausted`); its step cap, tool cap and repeated-call guard are its own, as for a resumed approval. A pause for approval keeps the count of continuations in the suspension record (absent when 0).
6. **Bounds.** `[hooks] max_stop_continuations` in the user's config (default 3, at most 8, `0` turns the feature off), lowered and never raised by the managed policy's key of the same name, read for every turn. The same reason (by digest, as joined) twice in a row ends the loop. The count bound is told to the last section that may be continued (`continuations_exhausted`); the repeat bound is known only after the hooks have answered, so it ends the loop with a record and a line and no flag. An interrupt always wins, and ends a running hook: the `stop` stage is cancellable only when it has a `continue_turn` hook, the hook is ended, the rest of the stage does not run, and `stop_cancelled` fires with reason `cancelled`.
7. **Recorded and shown.** `hook.stop_continued` (hook positions, `continuation_index`, `limit`, the reason's digest and the reason) is appended before the next section starts (invariant 4); `hook.stop_continuation_exhausted` (`hook`, `limit`, `reason` `count` or `repeat`) when a bound ends the loop. The transcript shows one line from the ledger, `rapid exec` says it on stderr and, with `--jsonl`, adds one record per continuation only when one occurred.
8. **A hook cannot complete a goal.** Completion stays derived from criteria, fresh evidence and verifier verdicts (invariant 8); a stop hook has no path to it. Main-agent turns only: a child's end is `subagent_stop`.
9. **The notification-style stages record their failures.** `session_start`, `session_end`, `subagent_start`, `pre_compact` and `post_compact` ignored a failing hook without a word although the reference said every non-gating stage records `hook.failed` (D-2). They now do, as a `hook.failed` where the surface has a ledger and a `warning:` line on stderr where it has none. This is visible to a project whose notify hook was failing unnoticed.

## What is deliberately not done

- No user-role message type in the history (decision 4).
- A hook cannot change the model, the parameters or the system prompt of the next section.
- The repeat bound does not set `continuations_exhausted` in a payload.
- `stop` hooks do not run for a child; there is no stop continuation for a delegated agent.
- No per-turn token budget exists for a main-agent turn today (the agent's step cap is the only one); the cumulative rule is built in the loop and applies the day a request carries one.

## Consequences

- `docs/reference/hooks.md` has a section on the feature and the corrections the review of the baseline found; `docs/reference/event-catalog.md` lists the two records; the wire catalog and its generated types carry the two kinds.
- `rapid doctor` names each object-form entry with its matcher and timeout, shows whether a `continue_turn` hook is active, the limit in force and the layer it came from, and what a timeout does on the platform.
- An older binary skips an object-form entry without a word, so a project that depends on a `continue_turn` gate should say so; a managed policy with the new key is refused as a whole by an older binary (it runs no project hooks).
