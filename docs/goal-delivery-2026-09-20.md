# Goal delivery — SEAMS-RAPIDLM-01 (2026-09-20): Phase 0 and the first Tier 1 slices

Baseline: `89350d7`. Governing document: [`docs/goals/seams-governing-principles.md`](goals/seams-governing-principles.md); worklist: [`seams-implementation-tasks.json`](goals/seams-implementation-tasks.json); audit: [`seams-phase0-baseline-2026-09-20.md`](goals/seams-phase0-baseline-2026-09-20.md).

## Phase 0 (SEAM-00-1, SEAM-00-2, SEAM-00-3) — `7ec2a1f`

| Criterion | Status | Evidence |
|---|---|---|
| Every §4 "today" claim rechecked with `file:line` | done | the audit's §3: fifteen items, eleven corrections, none widening the program |
| ADRs for the contract changes | done | ADR 0022 (hook result v2), 0023 (inbox edges), 0024 (plan proposals) |
| Worklist with explicit dependencies, Tier 1 first | done | 53 tasks; `SEAM-06-1` blocked on decision D-1; D-2/D-3 recorded with recommendations |
| Governing document corrected only through the audit's commit | done | thirteen "Audit correction (SEAM-00-1, 2026-09-20)" notes under the affected paragraphs; acceptance criteria untouched |
| Development ledger gains the program's entries | done | `docs/development-ledger.md` § SEAMS-RAPIDLM-01 |

## SEAM-01-1 — Hook result v2: protocol type, fixture, separated streams, allow/deny/defer

Contract restated: `apps/rapid/src/hooks.rs` (the production hook runner), `apps/rapid/src/exec_tools.rs` (the one call site, `execute_call_traced_flagged`), `crates/protocol` (the wire type), `crates/event-ledger` (`hook.decided`), the SDK wire catalog. Migration impact: none for v1 hooks that print plain text or nothing (exit-code contract identical; no ledger record; the two stream-separation differences are disclosed in the AC-01 row); a v1 hook whose stdout is exactly one JSON object with a `decision` key or `"schema":"rapidlm.hook_result"` is now read as v2 — that shape had no meaning before; one new additive event kind.

| Criterion | Status | Evidence |
|---|---|---|
| AC-01 every existing hook test passes unchanged; a v1 exit-code hook behaves identically | done, with two disclosed differences | the eleven pre-existing `hooks::tests` and the two `exec_tools` hook tests pass with their assertions unchanged (three `panic!` arms widened from `Allowed =>` to `other =>` because the outcome enum gained `Ask`); `plain_stdout_text_keeps_v1_semantics_and_records_nothing` pins a chatty exit-0 hook (allowed, silent), a stdout-only reason on exit 3 (still the detail) and stderr winning over stdout; `a_v1_hook_records_no_decision` pins an empty sink for a v1 hook. Two differences from the merged-stream capture, both toward the module's documented contract ("stderr as the detail"): a v1 hook that wrote to *both* streams now yields stderr as the detail rather than an interleaving; a timed-out hook's stdout reaches the detail after "hook timed out:" rather than as the whole reason |
| AC-02 `allow`, `deny`, `defer` each prove their effect on dispatch and their ledger record | done (`ask` in SEAM-01-2) | `v2_hook_decisions_are_applied_on_dispatch_and_recorded_for_the_ledger` (deny → `Denied` with the reason, no file; allow → `Succeeded`; defer → `Succeeded`; one record each naming hook, tool, call); `a_deferring_hook_leaves_the_decision_to_the_permission_flow` (the lattice runs first — a rule-denied call never reaches the hook stage; on an allowed path defer lets the call run); `a_v2_hook_decision_reaches_the_ledger_as_hook_decided` (a real interactive turn: `hook.decided` with `record`, `hook`, `event`, `decision`, `tool`, `call_id`, `command_digest`, `reason_digest` — no reason text — appended before `tool.denied`) |
| `ask` is never an implicit allow | done, interim | `a_hook_ask_is_a_stated_denial_until_it_can_reach_an_approval_surface`: denied with "held by pre_tool_use[0] hook: … (a hook's ask cannot reach an approval surface in this build …)", the `ask` recorded; SEAM-01-2 replaces the denial with the approval wait |
| Malformed or unknown results deny naming the hook | done (the malformed case landed in the follow-up fix) | `an_unreadable_v2_result_denies_naming_the_hook` (`decision: maybe`; `version: 9`); `a_malformed_result_denies_and_a_chatty_v1_hook_of_any_size_allows` (a deny whose reason broke the JSON denies as `Malformed`; a result followed by a log line denies; 84 KB of plain v1 stdout still allows — the result ceiling is for results); `stdout_that_begins_like_a_result_and_is_not_one_object_is_malformed` (parser: invalid JSON, invalid UTF-8, trailing text, a second value; `schema` alone is not a marker); `a_v2_deny_beside_a_nonzero_exit_lends_its_reason_and_a_v2_allow_does_not_rescue_it` (exit code stays fail-closed; every parsed result is recorded; the model reads "hook exited non-zero (its result said allow; the exit code decides)", never the echoed JSON); `a_timed_out_hooks_partial_stdout_is_not_a_decision` (a deny printed before the hang is neither recorded nor its reason — the stdout is a diagnostic) |
| Later denial beats earlier ask; both recorded | done | `a_v2_ask_is_reported_unless_a_later_hook_denies` |
| Streams separated; post/notify hooks still record both | done | `run_hook_once` writes stdout and stderr to two files; `post_hooks_record_both_streams_stdout_first` |
| AC-08 fixture under `crates/protocol` | done | `crates/protocol/tests/fixtures/hooks/v2/hook_result.json`, `hook_result_v2_fixture_matches_wire_contract`; the parser's own tests in `protocol::hooks::tests` (v1 text is `None`, the four words plus `block`, schema/version refusal, typed and bounded optional fields, char-boundary truncation, grant keys observed not honoured, round trip) |
| Revert cycle | done | with `HookResult::from_stdout` fed an empty stdout (the pre-change world) the nine new v2 tests fail and every v1 test still passes; for the follow-up fix, restoring "parse failure is v1" + "size check first" + "parse a timed-out stdout" fails `a_malformed_result_denies_and_a_chatty_v1_hook_of_any_size_allows`, `a_timed_out_hooks_partial_stdout_is_not_a_decision`, `plain_text_and_empty_stdout_are_v1`, `stdout_that_begins_like_a_result_and_is_not_one_object_is_malformed`; restored, all green |
| SDK wire catalog | done | `wire.v1.json` + regenerated `sdk/typescript/src/generated/index.ts` carry `hook.decided`/`hook`; the SDK's event-kind count pin moves 105 → 106 (that pin is the catalog's drift check); `pnpm generate:check`, `typecheck`, `test` green |
| Required checks | done, one environmental failure disclosed | `cargo fmt`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test -p protocol --test schema_fixtures`, the three pnpm checks all green; `cargo test --workspace` green except `interactive::tests::computer_observe_reports_the_typed_platform_gate_not_a_stub`, which drives the real desktop stack and fails on this machine with `computer-use observation failed: backend` alone and in the suite, on a diff that touches no computer-use code; CI on `main` (`7ec2a1f`, `89350d7`) passes it on the macOS runner — a host-permission condition of this session's process, not a regression |
| Rule 2.4 | done | `hooks.rs` no longer names a peer product in its module doc or the timeout constant |

Disclosed: hook identity is positional (`pre_tool_use[<index>]`) plus a 12-hex command digest — settings hooks have no ids of their own; a hook that prints a grant-shaped key has `grant_attempted: true` in its record and the keys are ignored (invariant 13); an empty `reason` is recorded as no reason (`reason_digest: null`).

### Self-review of `01e64f3` — findings fixed in the follow-up commit

The background review found: (1) a malformed v2 result (a deny whose reason broke the JSON, a result followed by a log line, invalid UTF-8) was read as v1 and **allowed** on exit 0 — a fail-open against ADR 0022 §2; (2) the 64 KiB result ceiling was applied before the `{` test, so a chatty v1 hook (lint output) exiting 0 was newly denied — a v1 regression; also any JSON with a `schema` key was a marker; (3) the interim `ask` denial led with "held", so a long reason cut at the 256-byte detail bound read as pending; (4) `a_timed_out_hooks_partial_stdout_is_not_a_decision` passed with the line it protected removed; (5) a v2 `allow`/`ask`/`defer` beside a non-zero exit was neither recorded nor acknowledged and the model read the echoed JSON as the reason; (6) the record/worklist overclaimed "byte-identical" and "malformed JSON denies", and the only ledger test was Unix-only; (7) an empty reason was digested. All seven verified and fixed: `HookResultError::Malformed`; the `{` test before the size check and `schema == "rapidlm.hook_result"` (not any `schema`) as the marker; outcome-first wording; a deny-then-hang fixture; every parsed result recorded on the non-zero path with a static reason; the rows above corrected; the ledger test runs its hook through a `sh` script file so it runs on Windows too; empty reasons normalised.

## SEAM-01-2 — Hook `ask` through the approval wait; headless recording sink and `NeedsApproval`

Contract restated: `apps/rapid/src/exec_tools.rs` (the `Ask` arm of the hook stage; a hook-only sink for surfaces with no general approval source), `apps/rapid/src/approvals.rs` (`build_hook_ask_request`, `ApprovalRequest.source`), `crates/kernel/src/client.rs` (`RecordApproval::with_source`, `ApprovalRequestedPayload.source` — additive, `skip_serializing_if` none, so every request recorded before this reads back with `source: None`), `apps/rapid/src/interactive.rs` (headless: the hook-ask sink, the suspension record, the `NeedsApproval` exit and its hint; the scripted-turn test seam now installs the production approval sink). Migration impact: a headless run with a v2 `ask` hook now exits 10 and parks instead of denying — new behaviour for a new hook word; a permission `Ask` on headless keeps its typed denial (S11).

| Criterion | Status | Evidence |
|---|---|---|
| AC-02 `ask` proves its effect on dispatch and its ledger record | done | `a_hook_ask_with_an_approval_surface_pauses_the_call_and_the_approved_resume_runs_it_once`: `ApprovalRequired`, nothing written, the request names the hook first in the summary and as `source: hook:pre_tool_use[0]` and carries the call's own scope and diff; `hook.decided { decision: ask }` recorded (the headless test reads it back from the ledger) |
| AC-03 `ask` resolves through the approval surface in the TUI | done | `a_hook_ask_pauses_an_interactive_turn_on_the_approval_surface_and_approval_continues_it`: the `/approvals` projection shows the pending item with the hook's summary; approving and running the continuation writes the file once and completes the turn; no second ask |
| AC-03 headless produces the documented exit code | done | `hooks_cli::a_hook_ask_parks_a_headless_run_with_the_needs_approval_exit_code_and_writes_nothing` (the real binary): exit **10**, stderr `needs approval: … parked in session <id>. Resolve it with rapid resume <id> and /approvals approve <n>`, nothing written, the ledger holds `approval.requested { source }`, `tool.approval_required { suspension }`, `hook.decided { ask }`, `turn.interrupted`, and no `tool.completed` |
| AC-03 a restart mid-`ask` resumes waiting with the call not re-run | done | `approvals_flow::a_hook_ask_with_a_sink_suspends_records_its_source_and_a_restart_resumes_the_call_once`: a fresh client over the same ledger sees the pending approval with its `source`, approves, and `execute_preapproved` with the same asking hook runs the call exactly once — the answered ask is not raised again |
| A hook `deny` still denies an approved resume (S5) | done | `a_hook_deny_still_denies_an_approved_resume` |
| The headless sink widens nothing else | done | `the_hook_ask_sink_serves_hook_asks_only_and_a_permission_ask_keeps_its_denial`: default-mode write → the typed denial, no request; a hook ask on an allowed read → paused |
| No surface: a stated denial, outcome first | done | `a_hook_ask_with_no_approval_surface_is_a_stated_denial` |
| Headless deny path unchanged and visible | done | `hooks_cli::a_hook_deny_is_a_model_visible_denial_and_the_run_completes` (exit 0, `blocked by pre_tool_use hook: …` in `--verbose` stderr) |
| Revert cycle | done | with the sink lookup forced to `None`, the three pause tests fail (denials); with the `preapproved` exemption removed, the two resume tests fail (the ask is raised again); restored, all green |

Disclosed: on the approved resume the hooks run again — a `deny` still wins (policy only narrows), an `ask` is treated as answered and is recorded as a second `hook.decided { ask }`; the scripted-turn test seam (`run_interactive_turn_inner_with_backing`) did not install the approval sink production installs, so no interactive approval flow had ever been exercised at that level — it now does, and every other interactive test still passes; resolving a headless ask uses the TUI (`rapid resume <session>` → `/approvals`) or an ACP client — there is no headless `approve` command (recorded for SEAM-10 as a candidate `rapid approvals` subcommand, not built here).

## SEAM-01-3 — Rewrites: validated `updated_input`, journaled digests, transcript marker

Contract restated: `apps/rapid/src/hooks.rs` (the stage returns the surviving `HookRewrite`: last rewriting hook wins, a deny discards, an ask keeps), `apps/rapid/src/exec_tools.rs` (`apply_hook_rewrite`: `ProposedToolCall::new` bounds, the tool's own `arguments_parse`, the lattice on the *rewritten* call, `hook.input_rewritten` before anything runs, the call swapped for the rest of dispatch, a one-line marker on the result), `crates/event-ledger` + SDK catalog (`hook.input_rewritten`, count pin 106 → 107), `crates/tui` (the transcript projects the record as `✎ <tool>: rewritten by <hook> hook` — `ToolActivityStatus::Rewritten`). The governing document names `crates/tool-gateway` for the re-validation; the audit found it unreachable, so the per-tool parsers are the contract (ADR 0022 §5).

| Criterion | Status | Evidence |
|---|---|---|
| AC-04 a rewrite failing the tool schema is denied naming the hook | done | `a_rewrite_that_fails_the_tools_arguments_is_denied_naming_the_hook` (`{"nonsense":1}` → "blocked by pre_tool_use[0] hook: its rewritten input does not match workspace_write's documented arguments", nothing written, no rewrite record; an over-bound rewrite is an unreadable result, denied) |
| AC-04 a valid rewrite runs the rewritten call and journals both inputs with digests | done | `a_valid_rewrite_runs_the_rewritten_call_journals_both_inputs_and_marks_the_result` (the rewritten path is written, the proposed one is not; the record carries `before`/`after` and their SHA-256s); `a_hook_rewrite_reaches_the_ledger_with_both_inputs_and_marks_the_transcript` (a real turn: `hook.input_rewritten { record, hook, tool, call_id, before, after, before_digest, after_digest }` appended before `tool.completed`) |
| AC-04 the transcript marker is present | done | the result the model sees ends with `[hook pre_tool_use[0] rewrote workspace_write input]`; the TUI transcript carries its own `✎ workspace_write: rewritten by pre_tool_use[0] hook` line projected from the ledger record, before the completion (same test) — a completed tool's summary is not shown in the TUI, so the projection is what makes the marker visible there |
| A rewrite cannot widen policy (S5) | done | `a_rewrite_cannot_widen_what_policy_allows`: an allowed path rewritten into a deny-ruled one is denied naming the hook and the rule; nothing written on either path |
| Last rewrite wins; deny discards; ask keeps | done | `the_last_rewrite_wins_a_deny_discards_them_and_an_ask_keeps_one`; `a_hook_ask_with_a_rewrite_shows_the_human_the_rewritten_call` (the approval request's scope and summary are the rewritten call's) |
| Revert cycle | done | with the stage's rewrite ignored at the dispatch site, five rewrite tests fail (the proposed call runs, nothing is journaled); restored, all green |

Disclosed: on the approved resume the hooks run again and rewrite again — the rewrite is deterministic for a deterministic hook, and it is the rewritten call the human was shown; `permission_for` on the rewritten call treats an `Ask`-class decision as approved only on the resume (`preapproved`) — before that, an `Ask` on the rewritten call is denied naming the hook rather than raised, so a hook cannot use a rewrite to reach a question the lattice would have asked about a different call.

### Self-review of `4b76689` — findings fixed in the follow-up commit

The background review found eight issues; all verified against the code, fixed with revert-cycled tests: (1) a hook `ask` on `ask_user` paused the model's own question, so approving it swallowed the clarification and answering it re-asked with an orphaned approval — `ask_user` is exempt from the hook `Ask` arm as it is from the lattice's (a hook `deny` on it still denies; `a_hook_ask_on_ask_user_is_answered_by_the_question_itself`); (2) two asks in one tool batch raced on the ledger's `expected_seq`, the loser denied with a false "no approval surface" — the sink serialises its own requests, retries a `SessionConflict` with a fresh tip **and a fresh wait token** (the kernel keeps a failed attempt's row as `Expired`, so the same token collided), and the denial now says "the approval could not be recorded (…)" when that is what happened (`concurrent_approval_requests_both_land_despite_sequence_conflicts`, 24 threads; `an_approval_that_cannot_be_recorded_denies_and_says_so`); (3) the approval summary put a 2 KiB-capable reason before the call and the kernel cut the whole at 512 bytes, so a long reason hid the command — the call comes first and the reason is bounded at 192 bytes with an ellipsis (`a_hook_ask_request_shows_the_call_first_and_bounds_the_reason`); while there, the shell summary read a `command` field the tool never sends and every shell approval said `run: (no command)` — it reads `argv` now (`a_shell_approval_summary_shows_the_argv_command`); (4) a continuation never recorded its own suspension, so the second ask of a turn resolved into "resumable state could not be loaded" — `record_outcome_suspension` is one helper both the first turn and a continuation call, and a resumed call that raises a new ask parks the turn again on the same recorded history (`a_continuation_that_pauses_again_records_its_own_suspension_and_resumes`, two writes, two approvals, both land); (5) when the lattice asked first, the hook's ask was skipped on the resume as if answered — `execute_preapproved_from` carries the approval's `source` (read from the resolved `approval.requested` by `recorded_request`) and skips only a hook ask whose `hook:<name>` matches; otherwise it is raised as its own approval (`a_lattice_approval_does_not_answer_a_hooks_ask_which_is_raised_on_the_resume`); (6) docs: ADR 0022 §3 no longer claims `rapid exec --continue` resolves a parked ask (it does not; `rapid resume` does), the exit-10 description in `headless/jsonl.rs` and the pinned getting-started table now cover exec turns, and the headless `rapid approvals` candidate is recorded on SEAM-10-1; (7) `/approvals approve <n> remember` on a hook-sourced approval recorded a lattice grant and claimed "remembered" while the hook would ask again — refused with the reason (`remember_is_refused_for_a_hooks_ask`); (8) `rapid cron` fires prompts as unattended Plan-mode turns and would have parked one per fire — no hook-ask sink is installed under a forced mode, so the stated denial stands there (read-verified; the cron path has no test that reaches a model).

### Self-review of `9253341` + `9ee5f45` — findings fixed in the follow-up commit

The background review found nine issues; all verified and fixed with revert-cycled tests: (1, **high**) after a human approved the lattice's question about the *original* call, the resume's `preapproved` flag waived the policy check on a hook's *rewritten* call, so a hook could turn `cargo test` into anything Ask-class unseen; and nothing tied a rewrite to what was approved (a hook that rewrites differently on each run passed with a matching source). Approvals now record `arguments_digest` (the SHA-256 of the arguments the human was shown — `ApprovalRequest`, `approval.requested`, additive) and the resume carries an `ApprovedAsk { source, arguments_digest }`; a hook ask is answered only by an approval naming that hook (position **and** command digest — `hook:<name>#<digest>`) about those exact arguments, an Ask-class rewritten call is raised as its own question rather than run, and a record without a digest matches nothing (`a_lattice_approval_does_not_let_a_rewrite_run_a_different_ask_class_call`, `a_rewrite_that_differs_from_the_approved_arguments_is_raised_again`, `a_reordered_hook_does_not_inherit_the_approval_of_another`); (2) `apply_hook_rewrite` lacked the `ask_user` exemption, so a rewritten question was "not allowed by policy" in Default mode (`a_rewrite_of_ask_user_is_not_blocked_by_the_lattice`); (3) batch grouping keyed writes on their *proposed* paths, so two writes a hook redirected to one path ran on two threads — with pre-tool hooks configured every write-classified call runs in one group in proposal order (`hooked_writes_serialise_in_proposal_order_even_when_rewritten_to_one_path`, the first hook run slowed so the unserialised order would differ); (4) a v1 post hook re-bounded the *whole* result at 256 bytes — a pre-existing cut of long reads — and with it the rewrite marker; the bound now applies to the hook's own text and the marker is appended last (`the_rewrite_marker_survives_a_post_hook_and_the_result_is_not_cut`); (5) `hook.input_rewritten` was recorded before a hook ask paused the call, so a call the human then denied had a `✎` line — it is recorded only when dispatch proceeds; (6) the concurrency test shared one sink, so its mutex serialised everything and the retry was never exercised — it now uses two independent sinks; (7) a doc comment claimed `tool.requested` carries arguments (it carries the call id and tool) — corrected, with the redaction-class parity to `approval.requested` stated; (8) every hook saw the model's original input, so "last rewrite wins" could pair hook A's ask with hook B's rewrite that A never saw — each hook now sees the call as it currently stands (`a_later_hook_sees_the_earlier_hooks_rewrite`); (9) an identity `updated_input` (the pass-through idiom) was journaled and marked as a rewrite — skipped when equal to what the hook was shown (`an_identity_rewrite_is_not_a_rewrite`, `an_updated_input_equal_to_the_shown_input_is_not_a_rewrite`).

## SEAM-01-4 — `additional_context` delivered after the result, fenced and bounded

Contract restated: `apps/rapid/src/hooks.rs` (`HookContext` collected by the pre stage — none on a denial — and by a new `run_post_tool_stage`, which gives post hooks the v2 contract: a v2 result's context is delivered and its decision recorded, a post hook cannot block, plain text stays the v1 output), `apps/rapid/src/exec_tools.rs` (the context follows the tool result inside `<untrusted_context locator="hook:<stage>[<n>]">…</untrusted_context>` — the fence the model already knows for untrusted retrieved content — each block passed through the same secret redaction as the result; order: result, post-hook v1 text, pre then post context, rewrite marker last). Bound: `MAX_HOOK_CONTEXT_BYTES` (4 KiB, per hook) at parse. Migration impact: none for v1 post hooks (plain text recorded as before).

| Criterion | Status | Evidence |
|---|---|---|
| AC-05 context appears after the tool result, fenced, naming the hook | done | `additional_context_follows_the_result_fenced_bounded_and_redacted` (the result first, then `hook:pre_tool_use[0]` then `hook:post_tool_use[0]` blocks, each closed) |
| AC-05 bounded by the ceiling | done | same test: a context 100 bytes past `MAX_HOOK_CONTEXT_BYTES` is cut; the parser's own bound test in `protocol::hooks` |
| Redacted like the result | done | same test: a registered canary secret inside the context is replaced by `[REDACTED:secret:…]` |
| Instructions inside are data (S4) | done | `hook_context_cannot_close_the_fence_or_smuggle_an_image` (text holding closing tags in two cases and a `DATA_URL:` line leaves exactly one closing tag, the fence's own, and no image line) — this row first cited an assertion with no model in the loop, which proved nothing; replaced by the self-review of `dae5d06` + `8d24390` |
| A denial delivers no context | done | same test (an earlier hook's context with a later deny) and `additional_context_is_collected_from_pre_and_post_hooks_and_fenced` (stage level; blank context is not delivered; post v2 context and decision recorded, post v1 text unchanged) |
| Revert cycle | done | with the collected context dropped at the dispatch site, `additional_context_follows_the_result_fenced_bounded_and_redacted` fails; restored, green |

Disclosed: a post hook's v2 `deny` is recorded as `hook.decided` and has no effect on the completed call (ADR 0022 §7: post stages observe); context is not delivered on a `Denied` or `ApprovalRequired` result — there is no result for it to follow (the complete list is in the self-review of `dae5d06` + `8d24390`).

## SEAM-01-5 — New stages: `user_prompt_submit`, `stop`, `stop_cancelled`, blocking `subagent_stop`, `post_tool_use_failure`

Contract restated: `apps/rapid/src/hooks.rs` (four new stage keys; one `named_stages()` list that `rapid doctor` now walks — it listed six of the stages, so `pre_compact`/`post_compact` hooks were never counted or path-checked; a shared fail-open `run_observer_stage` that returns decisions, context and **failures**), `apps/rapid/src/exec_tools.rs` (`post_tool_use_failure` after a failed call; `subagent_stop` through `subagent_stop_block` on both the inline and the detached path — the detached path fired neither subagent hook before), `apps/rapid/src/interactive.rs` (`user_prompt_submit` in headless `exec_turn`, the TUI composer and the message queue; `fire_turn_end_hooks` at the three places a turn ends — headless, the TUI turn, a continuation), `crates/event-ledger` + SDK (`hook.failed`, count pin 107 → 108). Migration impact: none for projects that configure none of the new stages; `rapid doctor` now counts every configured stage.

| Criterion | Status | Evidence |
|---|---|---|
| AC-06 `UserPromptSubmit` may block with a reason; queued prompts hold | done | `a_blocked_prompt_starts_no_turn_restores_the_composer_and_is_recorded` (TUI: no `turn.started`, the reason shown, the text back in the composer, `hook.decided { event: user_prompt_submit, tool: null }`); `a_blocked_queued_prompt_stays_queued_held_and_survives_a_restore` (held, in order, not re-judged every tick, restored as held after a restart); `hooks_cli::a_blocked_prompt_exits_policy_before_any_model_request_and_a_completed_run_fires_stop` (headless exits 3 with the reason); `a_prompt_submit_deny_blocks_and_a_failing_hook_fails_open_with_a_warning` (stage: payload, fail-open, `ask` recorded without effect) |
| AC-06 `StopCancelled` fires instead of `Stop` when a turn ends without completing, with the reason; `Stop` does not fire when `StopCancelled` does | done | `a_completed_interactive_turn_fires_stop_and_a_failed_one_fires_stop_cancelled` (a completed turn fires `stop` only; a failed model step fires `stop_cancelled { reason: model_failed }` only); `the_turn_end_stage_fires_stop_or_stop_cancelled_never_both` (payloads: `tool_calls`, `tokens`, `reason`); headless `stop` in the `hooks_cli` test |
| AC-06 `SubagentStop` may block the stop | done | `a_subagent_stop_deny_blocks_the_childs_completion_for_the_parent` (payload `status`/`ok`; the parent receives "completion blocked by subagent_stop[0] hook: …", not the child's report; the decision recorded against `task_spawn` and the call) |
| AC-06 `PostToolUseFailure` fires on a dispatch failure, may add context, cannot block | done | `post_tool_use_failure_fires_on_a_failed_call_adds_context_and_cannot_block` (a failed read: payload `tool`/`error`, the fenced context after the failure, the call stays failed, a success does not fire it) |
| Other events fail open with a recorded warning | done | `a_failing_observer_hook_is_a_recorded_warning_not_an_effect` (`hook.failed` through the sink); headless also prints a `warning:` line |
| Every stage is named once | done | `the_new_stages_parse_and_every_stage_is_named_once` |
| Revert cycle | done | prompt gate off, turn-end forced to `Completed`, `subagent_stop_block` → `None`, the failure stage skipped, the headless deny ignored: the six tests above fail; restored, green |

Disclosed: the headless blocked-prompt exit is `Policy` (3) — printed on stderr, no JSONL header (the prompt never reached a turn, as with a lattice-load failure); `stop_cancelled` reasons are the turn loop's stop-reason tokens (`cancelled` for an interrupt, `approval_required` for a paused turn) plus `error`; a denied permission is a tool result the model reads and does not end the turn, so it is not a reason by itself; in the TUI, a failed turn-end hook is recorded as `hook.failed` only (a turn thread has no terminal line).

### Self-review of `dae5d06` + `8d24390` — findings fixed in the follow-up commit

The background review found six issues; all verified against the code and fixed or disclosed, each fix revert-cycled: (1, **high**) when the hook that asks is not the hook that rewrites, each question accepted only an approval naming its own hook, so every resume raised the other one and the call never ran (Default mode: the lattice's question, then hook0's, hook1's, hook0's, …) — the questions one stage raises about one call are now **one** request about the call as it would run, led by the asking hook's reason, and one approval naming any of those hooks about those exact arguments answers them all (`an_asking_hook_and_a_rewriting_hook_raise_one_question_not_an_endless_pair`, both orders: two approvals, then the rewritten write runs once); (2) with any `pre_tool_use` hook configured, *every* write-classified call — `task_spawn`, `ask_user`, `mcp__*`, `shell_exec` — shared one batch group, so sibling subagents lost their concurrency, against four doc comments that said they run concurrently; a hook cannot change a call's tool, so only `workspace_write` and `workspace_patch` (the tools whose path a hook may rewrite) share the group now, every other tool keeps its key, and the four docs say so (`sibling_subagents_still_run_concurrently_when_a_hook_is_configured`: two `task_spawn` children that each wait for the other to start; serialised, the first gives up); (3) hook context was inserted into the fence raw, so text containing `</untrusted_context>` closed it early and what followed read as tool output, and a line starting `DATA_URL:` became an image part (enough of them fail the whole request on the content-part bound); the retrieved-context fence in `model.rs` had the same shape — `model::fence_untrusted` now wraps both and neutralises opening and closing tags in any case (`&lt;…`) and the `DATA_URL:` line prefix (`hook_context_cannot_close_the_fence_or_smuggle_an_image`, `untrusted_text_cannot_leave_its_fence`, and the retrieved-context test extended with a closing tag); the SEAM-01-4 evidence row that cited an assertion with no model in the loop is replaced; (4) the "invalid arguments" failure returned before the result tail, so the pre stage's context was computed and discarded and `post_tool_use_failure` never saw that failure — it takes the tail now (`an_invalid_arguments_failure_carries_the_hooks_context_and_fires_the_failure_stage`); (5) chained rewrites were credited to the last hook alone, and a chain that ended back at the model's own arguments was still marked and journaled as a rewrite — `HookRewrite.contributors` lists every hook whose rewrite shaped the final input, `hook.input_rewritten` carries `contributors`, the marker names them all, and a chain back to the proposed arguments is no rewrite (`a_chained_rewrite_credits_every_contributor_and_a_round_trip_is_no_rewrite`; the ledger test asserts `contributors`); (6) docs: the 4 KiB context bound is per hook, not per result — stated in ADR 0022 §6 and the SEAM-01-4 section; the kernel's `with_source` comment shows the digest-qualified source.

Disclosed: hook context is delivered on a `Succeeded` or `Failed` result only; a call that produced none — denied, parked for approval, answered `ContextRequired`, or ended in a runtime error rather than a result — carries no context. The total context a stage adds is not capped beyond the per-hook bound (at most 8 hooks per stage); a total cap would need a rule for which hook's text is cut, and none is proposed here.

### Rule 2.4 — peer-product names in the files SEAM-01 touched

Governing rule 2.4 asks a change that edits a file naming a peer product to remove the name in that change; the SEAM-01 commits so far edited `apps/rapid/src/exec_tools.rs`, `interactive.rs`, `model.rs` and `headless/jsonl.rs` without doing so. This commit does it for every file SEAM-01 has touched: the "… parity" parentheticals on the tool-name constants and executors, the reference-CLI example in the tool-name portability test, the "…-style" config wording, the compat-path prose in the permission and integration docs, a test name, and fixture model ids that carried a vendor's model family name (`main-model`, `big-model`, `provider-a/model-a`, `provider-b/model-b` — no test depended on them). Behaviour is unchanged: comments, a test name and fixture values only, so there is no revert cycle; the renamed and re-fixtured tests (`project_integrations_*`, the three `router_decision_*`, `every_tool_name_is_provider_portable`, `the_phases_compact_override_is_resolved_and_gated_like_a_fallback_entry`, `the_models_panel_marks_the_running_model_and_never_carries_a_credential`) pass.

Disclosed: the settings-file path another tool owns stays in `PROJECT_SETTINGS_FILES` — it is a file name the loader reads for compatibility, and removing it would change what existing projects get (S11). It is now spelled once: the tests read it from the constant (`compat_settings()`) instead of repeating the literal. The rest of SEAM-01-7 (the hooks reference page, the CLI-reference row) is still pending.

## SEAM-01-6 — Managed hook policy: `hooks.managed_only`, `hooks.denied_events`

Contract restated: `apps/rapid/src/managed_config.rs` (`ManagedHooks`, parsed from a closed `[hooks]` table of the managed policy — `managed_only`, `denied_events`, and the policy's own stage commands; `gate_hooks(project, policy)` narrows the project's hooks and returns a `GateReportEntry` per drop), `apps/rapid/src/interactive.rs` (`load_project_integrations` — the one loader every surface's hooks come from — applies the gate, so no surface can run a hook the policy drops; the gate reports ride on `ProjectIntegrations.hook_gates` and each turn that installs the hooks surfaces them as warnings — stderr on headless, a notice in the TUI), `apps/rapid/src/doctor.rs` (the hooks check warns with each gate report — a project whose every hook was dropped read "no hooks configured" before), `apps/rapid/src/hooks.rs` (`stages_named_mut`). Narrow-only: the managed hooks run first (an organisation's `pre_tool_use` gate decides before a project's); project settings can neither re-enable a denied stage nor run beside `managed_only`. Migration impact: none without a managed policy, or with one that has no `[hooks]` table.

| Criterion | Status | Evidence |
|---|---|---|
| AC-07 `hooks.managed_only` blocks non-managed hooks | done | `managed_only_runs_only_the_policys_hooks_and_reports_the_gate` (only the policy's `pre_tool_use` runs; the project's two hooks do not); `the_project_loader_applies_the_managed_hook_policy` (through the loader every surface uses) |
| AC-07 reports field/origin/remediation | done | same tests: `hooks.managed_only`, `origin=managed`, a remediation; `the_hooks_check_reports_what_the_managed_policy_dropped` (`rapid doctor` warns with the gate) |
| A denied stage does not fire; the managed hooks go first | done | `denied_events_drop_only_those_stages_and_the_managed_hooks_go_first` (the denied stage is dropped and reported with the stage named; a denied stage the project does not use reports nothing; no policy leaves the hooks unchanged) |
| Settings cannot widen either | done | the gate runs after the settings files are merged, on the merged result — both settings files are narrowed alike (loader test) |
| The `[hooks]` table is closed and validated | done | `the_hooks_table_is_closed_and_validated` (a non-boolean `managed_only`, an unknown stage in `denied_events`, a non-array stage, an unknown key — each refused naming the field) |
| A policy that cannot be loaded runs no project hooks | done | the loader test's second half: an invalid policy leaves no hooks and one `hooks` gate report saying the policy could not be loaded |
| Revert cycle | done | `gate_hooks` made a no-op: the two gate tests, the loader test and the doctor test fail; the doctor check ignoring the reports: the doctor test fails; restored, green |

Disclosed: the fields live in a `[hooks]` table beside `[policy]`, not inside `[policy]` as the worklist entry's wording has it, so the reported field ids read `hooks.managed_only` / `hooks.denied_events` — the names AC-07 and ADR 0022 §8 use. `managed_only` also drops project hooks on stages the policy declares nothing for. While here (rule 2.4), two test fixtures' model ids that carried a vendor's model family name are neutral (`cloud-model`, `alt-model`; no test depended on them).

### Self-review of `61b5114` + `4a1f521` — findings fixed in the follow-up commit

The background review found ten issues; all verified against the code and fixed, each fix revert-cycled: (1, **high**) the goal loop's compiled prompt went through the TUI's `submit_turn`, so a `user_prompt_submit` block was retried on every tick — a hook process and a `hook.decided` per pass, the composer overwritten each time (typing `/goal stop` impossible), only a double Ctrl-C out; the compiled prompt now passes the gate itself and a block stops the goal with the reason, leaving the composer alone (`a_goal_prompt_a_hook_blocks_stops_the_goal_instead_of_retrying_every_tick`: judged once, the loop stopped, the composer empty); (2, **high**) under `rapid exec` a write-capable child's worktree was three-way merged into the parent inside `SubagentRunner::run`, before `subagent_stop` ran, so a deny arrived after the files had changed — and in the TUI the child's end was recorded as succeeded before the hook decided; `SubagentRunner::settle(agent, blocked)` now runs after the hook, and `AgentViewManager::settle` applies (headless) or holds (interactive) an isolated child's changes only then — a blocked child's are never applied and stay held — while the lifecycle ends after the hook, failed when blocked; the blocked result says where the changes are, the reason cut first so the note survives the result bound (`a_blocked_childs_changes_are_settled_after_the_hook_and_it_ends_failed`, `a_blocked_childs_changes_are_held_never_applied_and_an_allowed_one_settles_as_before` — a real git worktree: blocked, the file is not in the parent and the view is kept; allowed, headless applies it); (3) `rapid acp` and `rapid daemon` started turns with no prompt gate — both submit through `spawn_acp_turn`, which now runs the shared `prompt_submit_block` first and fails the already-submitted turn with the reason (ACP stop reason `refusal`) before any model request (`an_acp_or_daemon_prompt_a_hook_blocks_fails_its_turn_before_any_model_request`); (4) turn endings that returned before `fire_turn_end_hooks` — a missing credential, a context that cannot be built, a continuation that pauses again or is cancelled, a panic — fired neither `stop` nor `stop_cancelled`, and an interrupt surfacing as an error was `error`: each turn thread's wrapper (`with_turn_end_hooks`) now fires the stage from the kernel outcome when the section did not, exactly once, and a cancelled error is `cancelled` (`every_way_a_turn_ends_fires_the_turn_end_stage_exactly_once`, `an_interrupt_that_surfaces_as_an_error_is_cancelled_not_error`); (5) a paused turn fired `stop_cancelled{approval_required}` and its continuation later `stop`, against ADR 0022 §7's "exclusive" — documented as the contract: each section of a turn fires exactly one of the pair, and a pause reason marks a pause, not an abort (ADR §7, `docs/reference/hooks.md`); (6) a held queued message blocked every message queued after it, a `/queue run` hold was never journaled, and a restored held message was announced as "will run after the current turn": a held message now stays queued without holding up the rest, every hold is recorded, and the restore says it is held (`a_blocked_queued_prompt_stays_queued_held_and_survives_a_restore`, extended) — which exposed a restore bug of its own: an edited message came back at the end of the queue (the edit re-records it and the restore re-pushed the id); an edit now keeps its place; (7) `fence_untrusted` escaped only quotes in the locator, so a repository path holding `</untrusted_context>` closed the fence from its header — the locator now carries no quote, angle bracket or line break (`untrusted_text_cannot_leave_its_fence`, extended); (8) a call failing with a runtime error (a shell timeout, an I/O failure) never reached `post_tool_use_failure` — it is observed now, its context undeliverable because nothing follows (`a_runtime_error_is_observed_by_the_failure_stage`, a plan file that cannot be read); (9) ADR 0022 §3 said a different hook's approval raises its own question and also that any hook's approval answers all, and the code followed the looser sentence, so a hook that started asking only on a later run was waved through on the approval of another hook's rewrite — the approval must name the hook whose question leads (the asking hook when one asks), and §3 says so once (`a_hook_that_starts_asking_later_is_asked_not_waved_through_on_the_rewrites_approval`: three requests, the third hook0's own); (10) `hooks_cli`'s "before any model request" is now counted — the scripted server counts `POST`s, zero for the blocked prompt — the `hooks.rs` doc names the real stop-reason tokens (`cancelled`, `failed`, `error`), and the SDK catalog lists the three ledger kinds it lacked (`message.queued`, `message.state`, `tool.context_required`; count pin 108 → 111; event catalog updated).

Disclosed: a command that exits non-zero is a completed call (its exit status is in the result), so it fires `post_tool_use`, not the failure stage; the turn-end stage fired by a wrapper reports `tool_calls`/`tokens` as 0 (the kernel outcome carries no usage); a blocked ACP or `rapid daemon` prompt leaves a failed turn in the ledger (the adapter submits before the turn thread runs), where a blocked TUI or headless prompt starts no turn; the strengthened `hooks_cli` assertion is test-only and has no revert cycle.

Revert cycles (two batched builds, each mutation with its own test): settle not told the decision, a blocked child ended as succeeded, the old any-hook answer, the failure stage skipping runtime errors, the locator only quote-escaped, a blocked child applied anyway, `/queue run` not journaling its hold, a cancelled error read as `error`, the goal loop back on `submit_turn`, a held head holding the queue, the wrapper never firing, the ACP gate never blocking — each fails its test (twelve); restored, green. The restore-order fix was caught failing by the extended queue test before it existed.

## SEAM-01-7 — Hooks reference page, catalogs, the stale CLI-reference row

Contract restated: `docs/reference/hooks.md` (new: stages and payloads, the v1 exit-code and v2 JSON contracts and their bounds, what each `pre_tool_use` decision does, approvals and the one-request rule, rewrites, fenced context, failure semantics per stage, records, the managed `[hooks]` policy, checking hooks), `docs/reference/cli-command-reference.md` (the `rapid hooks list|test|enable|disable` row named a command that has never existed — removed; hooks are pointed at the reference page), `apps/rapid/src/interactive.rs` (a test that every command the page names is dispatched). The event catalog and the SDK wire catalog already carry the three hook kinds (SEAM-01-1, -3, -5); `pnpm generate:check` is green. Migration impact: documentation only.

| Criterion | Status | Evidence |
|---|---|---|
| AC-08 `docs/reference/hooks.md` documents stages, v1/v2 contracts, failure semantics and managed gates | done | the page; kept true by the tests below and by the delivery records it summarises |
| Every command the page names exists | done | `every_command_the_hooks_reference_names_is_dispatched` — each `` `rapid …` `` span is checked against `SUBCOMMANDS` (the table `rapid --help`, `rapid <name> --help` and `rapid man` render from), a following verb against that entry's operands; tripwires assert a missing subcommand, a missing verb and the old `rapid hooks list` all fail |
| The stale CLI-reference row is gone (S9) | done | `rapid hooks …` removed; `the_reference_doc_lists_exactly_the_dispatched_subcommands` still green |
| Catalogs: `hook.decided`, `hook.input_rewritten`, `hook.failed` | done | `docs/reference/event-catalog.md`, `sdk/typescript/schemas/wire.v1.json` (count pin 111), `pnpm generate:check` |
| Rule 2.4 in the files SEAM-01 touched | done | § Rule 2.4 above (`5285f78`); `hooks.rs` names no peer product |
| Revert cycle | done | a `` `rapid serve` `` line appended to the page: the test fails naming it; removed, green |

Disclosed: the worklist's validation line asks for "a test [that] executes every command line in hooks.md against --help"; the test checks each command against the dispatch table that `--help` is rendered from instead of spawning the binary, because `--help` short-circuits before dispatch (`rapid mcp bogus --help` succeeds — see `every_subcommand_the_top_level_help_advertises_is_accepted`), so a spawned `--help` could not tell a real command from a misspelt one. Writing the page caught one real error: the review of `61b5114` + `4a1f521` called the SDK server `rapid serve`, and that name had been carried into two comments, a test name and the records before this test existed; it is `rapid daemon`, corrected before `2ec4fcd` was committed.

SEAM-01 is complete: seven tasks, six self-review rounds whose findings were all fixed and revert-cycled. The review of `b91f622` + `2ec4fcd` was running when SEAM-01 closed; its confirmed findings are the next task.

## SEAM-02-1 — Provider presets and `rapid setup --dry-run`

Contract restated: `apps/rapid/src/setup.rs` (new: the preset data table — endpoint, dialect, default model, documentation link, local-server flag, conventional key variable; rule 2.2 confines provider names to its rows and their tests — the `rapid setup` argument surface, the resolution of a choice into a **plan** — the file, every key set or unset, where the credential lives, the verification request — and `--dry-run`, which prints exactly that plan as text or one JSON object and does nothing else), `apps/rapid/src/interactive.rs` (`setup` in `SUBCOMMANDS`, so `rapid --help`, completions and `rapid man` carry it), `apps/rapid/src/p9_commands.rs` (`run_setup`), `apps/rapid/Cargo.toml` (`toml_edit`, already in the lockfile through `toml` — no new crate). The plan edits the existing config with `toml_edit`: other profiles, comments and formatting survive; only `[models] default` and the chosen `[model.<profile>]` keys change, and a credential key that would shadow the chosen one (an inline `api_key` wins at resolution) is removed and listed as unset. The resulting document must parse with `user_config::parse_config_document`, and an existing file that is not valid TOML is never overwritten. A key never arrives on argv: `--key`, `--api-key`, `--token`, `--secret` … are refused with the reason. On a terminal, a missing preset and model are asked for (a `Prompter`, scripted in tests); `--non-interactive`, and `--key-stdin` (stdin carries the key), never ask. Migration impact: none — a new command.

| Criterion | Status | Evidence |
|---|---|---|
| AC-01 `--dry-run --non-interactive --output json` prints exactly the files and keys it would write | done | `a_dry_run_prints_the_plan_and_writes_nothing_and_connects_nowhere` (the file with action and mode, the five keys in order, the credential source — never its value — and the planned ≤16-token request) |
| AC-01 no network call and no disk write | done | same test: the temporary home is byte-identical afterwards (every path and its bytes), and a listener on the chosen endpoint accepted no connection |
| Presets are data and usable as written | done | `every_preset_row_is_usable_as_written` (unique ids; each endpoint passes the model client's own URL validation; local ⇔ loopback; cloud rows name a key variable; each row plans a config the reader parses into that dialect and model) |
| Existing configs are edited, not replaced | done | `an_existing_config_is_edited_not_replaced_and_a_shadowing_key_is_unset` (comments and another profile kept; `api_key` unset and never printed; the same plan against its own result is `unchanged` with no backup); `an_existing_config_that_does_not_parse_is_left_untouched` |
| Keys never on argv; the surface is closed | done | `a_key_on_the_command_line_is_refused_with_the_reason` (the value is not echoed); `the_argument_surface_is_closed_and_consistent` |
| Terminal prompts vs. non-interactive | done | `a_missing_choice_is_asked_on_a_terminal_and_a_usage_error_otherwise` (by number, by id, a default taken, input that ends is an error, `--key-stdin` never asks) |
| The file a run would read is the one written | done | `the_config_target_is_the_file_a_run_would_read` |
| Revert cycle | done | the dry run writing the plan, key flags not refused by name, an unparseable file replaced, the Anthropic row given the wrong dialect, and the shadowing key kept: each fails its test (two builds, so one mutation does not mask another's test); restored, green |

Disclosed: a run without `--dry-run` exits 2 and says verification and writing are not in this build yet — they are SEAM-02-2 and SEAM-02-3 (the `rapid --help` summary says "dry run today" until then). The credential rule is fixed here for the slices that follow: `--key-env <VAR>` makes the config reference the variable (`env_key`) and stores no copy — a second place the secret lives would be a second place to leak it — while `--key-stdin` stores the key in the OS keychain under `rapidlm-model-<profile>` and the config names it (`keychain`, read from SEAM-02-3); the worklist's SEAM-02-3 line reads as storing both in the keychain. Preset endpoints and default model identifiers are data as of this commit and change upstream; `--model` and `--base-url` override them, and three local presets (`lm-studio`, `llama-cpp`, `vllm`) carry the placeholder model `local-model` — the served model's name belongs in `--model`. Checks: the `rapid` package's tests pass apart from one timing test that flaked under a load average of ~49 (its `sleep 30` job finished before the test looked; it passes alone), fmt and clippy clean.

### Self-review of `b91f622` + `2ec4fcd` — findings fixed in the follow-up commit

The background review found eight issues; all verified against the code and fixed, each fix revert-cycled: (1, **high**) on `rapid acp` and `rapid daemon` the prompt gate ran after the kernel had recorded the turn with its text, and the next turn's history (`conversation_through`) replays a failed turn's prompt — so a blocked prompt reached the model anyway, one turn later; the gate now runs before the prompt reaches the kernel — ACP's `session/prompt` is answered in `acp_serve` (`blocked_prompt_replies`: the reason as an agent message, stop reason `refusal`) and the daemon's `turns.submit` returns the reason as an error — so a blocked prompt never becomes a turn (`acp_serve::tests::a_prompt_a_hook_blocks_is_refused_before_it_becomes_a_turn`, `daemon_serve::tests::…`, Unix: no `turn.started`, a `hook.decided`; not gated when untrusted, for another method, or when allowed); the late gate in `spawn_acp_turn` is gone; (2, **high**) `settle` integrated whenever a view existed, so under `rapid exec` a child that failed after writing — a context overflow after effects, a panicking detached runner — had its partial writes merged into the user's tree; `settle` now takes how the child ended (`ChildEnd`: completed, blocked, incomplete, failed) and applies only a completed, unblocked child, headless only; the failure note reaches the parent (`only_a_completed_unblocked_child_is_applied_and_headless_discards_what_it_cannot_hold` over a real worktree for every end and mode; `a_blocked_childs_changes_are_settled_after_the_hook_and_it_ends_failed` extended with a failing child); (3) `/goal run` while a turn ran started a second one that bounced off the kernel's lease (ending the session) or raced a compaction — the goal loop now waits for the slot (`a_goal_run_while_the_model_slot_is_busy_waits_instead_of_starting_a_second_turn`); (4) the managed policy's own hook lines were silently dropped when empty, over 512 bytes or past 8 per stage — under `managed_only` meaning no hook at all — and project hooks cut by the merged cap were not reported: such a policy is refused (`hooks.<stage>`), and a cut is a gate report (`the_hooks_table_is_closed_and_validated` extended, `project_hooks_cut_by_the_per_stage_limit_are_reported`); (5) a blocked child's worktree was kept forever under `rapid exec`, where nothing can review or remove it and each counts against the repository's worktree limit — headless now discards a blocked or unfinished child's changes and says so, the TUI keeps holding them for `/agents` (same tests as 2); (6) `post_tool_use_failure` observed only `Failed` runtime errors, though `Invalid` (a path resolving outside the workspace, a non-UTF-8 patch target) ends the turn the same way — every runtime error but a cancellation is observed (`a_runtime_invalid_is_observed_by_the_failure_stage_too`, Unix symlink escape), and the comment's "a shell timeout" example, which is an ordinary failed result, is corrected; (7) `load_project_integrations` collected `std::env::vars()`, which panics on a non-Unicode variable, on the TUI's own thread for every prompt — it reads only the managed-policy variable now; (8) queued-message ids restart at `q1` with each process, so a restored `q1` and a new `q1` would share every state change and merge on the next restore — the counter moves past every restored id (`a_queued_id_restored_from_a_previous_run_is_never_reused`). Nits: `integrate` and `subagent_stop_block` have their own doc comments back; the one-request comment names the leading hook.

Disclosed: headless `rapid exec` fires the turn-end stage where the run ends its turn; a few `--json-schema` error paths return after the turn is recorded and fire none — pre-existing, now stated in ADR 0022 §7 rather than claimed away, and recorded as a follow-up. The gate warnings a managed policy produces are shown again on each turn that installs the hooks, as rejected MCP servers already are. Revert cycle: the ACP and daemon gates never blocking, a failed child read as completed, a headless blocked child held, the goal loop's busy check removed, the queue counter not moved, the managed line bound and the cap report removed, `Invalid` unobserved — each fails its test (nine, one batched build); restored, green.

### Self-review of `8cb1250` — findings fixed in the follow-up commit

The background review found ten issues in the setup plan; all verified and fixed, each fix revert-cycled (none had reached a file — only `--dry-run` exists — but each would have with SEAM-02-3): (1, **high**) setup accepted profile names and model ids every later run refuses (`my_openai`, a model `-x`): `--profile` and `--model` now use the run-time rules themselves (`llm_router::ProfileId`, `ModelId`), and the preset rows are checked with them (`the_argument_surface_is_closed_and_consistent` extended); (2) changing a key dropped the comment lines above it and a comment after its value (`toml_edit`'s `insert` resets the key's decoration) — an existing value is replaced in place, its decoration kept (`comments_on_a_changed_key_survive_and_an_unchanged_file_is_left_byte_for_byte`); (3) "unchanged" was decided by comparing re-serialised text, so a CRLF file, one with a byte-order mark or without a final newline always planned an `update` and a backup — changes are tracked per key and an untouched file is kept byte for byte, and a changed one keeps its CRLF and BOM (same test); (4) rejected values were echoed — a `--base-url` with credentials in it, a key pasted as an argument, `-k=<key>`, a wrong preset answer — none is echoed now, and only a plain option name is (`a_key_on_the_command_line_is_refused_with_the_reason` extended); (5) `--key-env` accepted a key shaped like a variable name (`gsk_…`, 32 letters and digits) and printed it: the name must be the conventional upper-case form, and a refused one is never echoed (same test); (6) the plan checked only that the result parses, not what a run does with it: it now resolves the result through the managed gates a run applies — a provider outside a managed allowlist is refused, and when `RAPIDLM_MODEL` or a managed locked default would make runs use another profile the plan says so; the existing file is read with the reader's own bound, only if it is a regular file (`the_plan_says_when_a_run_would_use_another_profile_and_refuses_what_a_run_refuses`, `a_config_path_that_is_not_a_regular_file_is_refused`); (7) `--base-url` was stored in the client's normalised form, so the same URL typed again planned an update — it is stored as given, without a trailing `/`; (8) inline-table configs (`models = { default = … }`, a profile as an inline table) were refused although the reader accepts them — they become standard tables with the same keys (`an_inline_table_config_is_edited_and_a_custom_endpoint_keeps_its_credential`); (9) a symlinked config would have been replaced rather than edited, and backups were named to the second — the plan edits the file the link points to and says so, and backup names carry milliseconds (`a_symlinked_config_is_planned_where_it_points`); (10) a custom endpoint with no key flag stripped the profile's existing credential — it is kept (and named), and the profile keys setup leaves alone are listed as kept (the inline-table test); `rapid setup --help` has a test (`help_names_every_flag_and_every_preset`), and the target is cross-checked against the reader's own resolution (`the_target_agrees_with_the_reader_once_the_file_exists`).

Disclosed: `--key-env` refuses a lower-case variable name even when it is a real variable's name — the rare case pays for never printing a key. Revert cycle: ten mutations in two builds (so a mutation does not mask another's test) — each fails its test; restored, green.

### Self-review of `91e400a` — findings fixed in the follow-up commit

The background review found five issues and three record errors; all verified and fixed, each fix revert-cycled: (1, **high**, a regression) the daemon's `turns.submit` ran the prompt gate after reading the client's `expected_seq`, and every v2 decision or hook failure the gate records moves the session on — so in any project whose hook prints a result, every submit conflicted with the gate's own record, and retrying re-ran the gate and conflicted again; the gate now decides first (`prompt_submit_decision`) and records after the turn is accepted (`record_prompt_submit`) — a deny records and says the session moved (`daemon_serve::tests::an_allowed_prompt_submits_at_the_clients_seq_and_the_decision_lands_after_the_turn`, beside the deny test); headless `rapid exec` had the same stale seq since `61b5114` — the gate records through the run's own session, `start_turn` then submitted at the older seq and the run went unrecorded — it submits at the session's current tip (`hooks_cli`'s allowed run asserts it is recorded); (2) `ChildEnd` read the runner's "effectively successful" child — tool calls, then an empty final message, whose work the runner keeps — as unfinished, so headless discarded work it used to apply; it is completed now, and a failed child is discarded even when a hook also blocked it (`child_end_counts_an_effective_success_and_a_failure_outranks_a_block`); (3) the goal loop's busy-wait started an iteration over an unanswered approval once the occupying turn paused, locking `/approvals` out, and ignored how the turn it waited behind ended — it waits while an approval is pending, and a goal started while a turn runs stops if that turn ends interrupted or failed (`a_goal_waits_for_a_pending_approval_and_stops_when_the_turn_it_waited_behind_fails`); (4) the ACP gate ran before the adapter validated the request, answering `refusal` before `initialize` or for an unknown or closed session and recording on it — it judges only a request the adapter would accept (initialized, an open session), otherwise the adapter's own error stands (`dispatch_refuses_a_blocked_prompt_only_where_the_adapter_would_accept_it`, through `Serve::dispatch` — the wiring the earlier test did not reach); (5) docs: a cancelled child's changes are discarded by the runner itself, not held — ADR 0022 §7 and `hooks.md` say so. Record errors: the previous section's item (7) claimed the env narrowing removed the non-Unicode-environment panic; it removed it from `load_project_integrations` only — `std::env::vars()` is still read at TUI start-up and per turn elsewhere (recorded as a follow-up); the worklist's "queue ids never repeat across restarts" is too strong — ids of messages submitted or cancelled in earlier runs can recur, harmlessly, since restore keeps the latest state per id; restored ids are never reused.

Found in passing, pre-existing and outside this work, flagged as a separate task: a second permission request in one ACP prompt kills `rapid acp` (the pending slot is taken once), and the adapter replays the previous turn's events at the start of the next prompt. Revert cycle: the daemon recording before its submit, the old `ChildEnd` precedence, the approval wait removed, the adapter-readiness and open-session checks removed, the headless stale seq, the busy-start inspection removed — each fails its test (two builds; two tests were strengthened after their first cycle did not fail: the pre-`initialize` prompt now names an existing session, and the goal test asserts why it stopped).

### Self-review of `9fae7e3` — findings fixed in the follow-up commit

The background review found eight issues in the setup plan (still dry-run only, so none reached a file); all verified and fixed, each fix revert-cycled: (1) a custom endpoint re-run without a key flag kept the profile's credential while its host changed, so the old key would go to the new host — over plain http in the review's example; a credential is kept only when the new URL has the old one's origin (scheme, host, port, default ports filled in), otherwise it is removed and the plan says so (`a_kept_credential_goes_only_to_the_endpoint_it_was_set_up_for`); (2) the refusal checks ran against the profile this shell's `RAPIDLM_MODEL` names — refusing a good plan when it named a missing profile, and letting a written profile outside a managed allowlist through when it named an allowed one; the checks now run as the file decides (`RAPIDLM_MODEL` set aside), and the shell's override is a note, never a refusal (`this_shells_model_override_is_a_note_and_the_allowlist_judges_the_written_profile`); (3) an inline table was converted to a standard one — dropping its comments and planning an update even when nothing changed — it is edited in place (`toml_edit`'s `TableLike`), and a new table inside an inline parent is inline (`an_inline_table_keeps_its_comments_and_an_identical_run_is_unchanged`); (4) a symlink to a file not created yet was refused (`canonicalize` needs the target) — links are followed by hand, and a first setup plans the create at the target (`a_symlink_to_a_file_not_created_yet_plans_a_create_there`; this also avoids Windows' `\\?\` verbatim paths); (5) `--output` still echoed its rejected value; (6) one CRLF line turned a whole LF file into CRLF — the original's majority decides (`a_lf_file_with_one_crlf_line_stays_lf_and_output_never_echoes`); (7) the plan listed keys that do not change — it lists exactly what it writes (same inline test); (8) a key a run does not read (the `keychain` alias `--key-stdin` writes until SEAM-02-3 teaches the reader) went unmentioned — such keys are named as notes (`a_key_a_run_would_not_read_is_named_in_the_plan`). Smaller: a new profile with no credential reports `none` in JSON as in text; the not-a-regular-file test runs on every platform.

Disclosed: the existing file's type is still checked before it is opened, so a FIFO swapped in at that instant would block the read — closing it needs a non-blocking open that std offers only per platform; it is recorded for the persistence slice, which opens the file anyway. A managed locked default absent from the user's config refuses the plan with the resolver's message, which does not name the lock. Revert cycle: eight mutations in three builds (so a mutation does not mask another's test) — each fails its test; one first-run mutation was too weak (it still named the key) and was redone.

### Self-review of `41df566` — findings fixed in the follow-up commit

The background review found seven issues, three record errors and four weak tests; all verified and fixed, each fix revert-cycled: (1) while an approval was pending the goal loop read the pending set from the ledger — every event, a fresh SQLite connection each — on every 50 ms tick, stalling the TUI exactly while the user types `/approvals`; it reads the TUI's own approval projection now — and, since the live tail delivers events a poll interval after they land, the loop decides nothing until that projection holds the ledger's tip, which also closes an older race: just after a turn ended, the loop could miss the turn's failure, or submit its next turn at a stale seq (the conflict the goal tests' drain loops were written around) (`a_goal_waits_for_a_pending_approval_and_stops_when_the_turn_it_waited_behind_fails` now also starts the goal before the approval reaches the projection); (2) while it waited, each pass re-read the same finished turn's answer into the loop detector, stopping the goal as "the model repeated itself" about 150 ms in — each turn's entries are inspected once (`a_goal_waiting_on_an_approval_does_not_count_one_finished_turn_as_a_loop`: busy start, the occupying turn completes, an approval pending, ten passes); (3) after a daemon deny the error said to re-read the session, which the TypeScript SDK could not do — every later `Session.run()` conflicted: the SDK gains `Session.refresh()` (`sessions.get`), and the error names it (`refresh re-reads the session…`); (4) headless `rapid exec` submitting at the session's tip let a turn another writer finished during the gate be skipped over by a run whose history omits it — it decides first, records a deny and exits, and otherwise submits at the seq it read and records after, accepted or not (`hooks_cli`'s allowed run is recorded); (5) the ACP gate still judged requests the adapter rejects — an image block, an oversize or empty text, a malformed `prompt` — running hooks on them and answering `refusal` instead of the adapter's error: the adapter exposes `validate_prompt_request` (readiness, parameters, content, an open session) and the gate runs only on its `Ok`, and only in a trusted project with prompt hooks, so a hook-less prompt costs no session lookup (`dispatch_refuses_a_blocked_prompt_only_where_the_adapter_would_accept_it`, whose pre-`initialize` case now asserts the adapter's error reply); (6) an effectively successful child was applied but recorded as failed in `/agents` and the job table — the lifecycle end and the job state now derive from `ChildEnd`, and a cancelled report (a cancellation that surfaced as an error, before the runner cleaned up) is a failed end whose changes are discarded, not held (`child_end_counts_an_effective_success_and_a_failure_outranks_a_block` extended); (7) the daemon ran the gate for an unknown session (its records silently refused) and lost the records of a prompt whose submit failed — it looks the session up first (an unknown or closed one is the kernel's error, no hook runs) and records after the submit either way (the allow test: a stale seq records a second decision; a counting hook does not run for an unknown session; it no longer spawns a production turn). Record errors: the `91e400a` section's item (3) said the loop "waits while an approval is pending" after the occupying turn paused — a paused turn ends interrupted, so that goal stops; the wait holds for an approval already pending when no turn runs; its item (4) said the ACP gate was "recording on" unknown sessions — the ledger refuses such appends; ADR 0022 §7 put ACP under "records after the accepted turn" — ACP's adapter submits at the session's tip, so its gate records first, and the ADR says so. Tests: the busy-start assertion is deterministic (the iteration offset is unchanged), and the approval-wait test drains before it starts.

Found in passing, pre-existing: the goal driver lease test (`a_driver_lease_becomes_available_again_once_dropped`) failed once in a parallel run and passes alone — a child process spawned by another test can inherit the lock's descriptor between fork and exec, briefly outliving the drop; the transcript's 4096-entry cap leaves the goal loop's inspection slice empty in a long session (flagged as a separate task). Revert cycle: the caught-up gate, the projection's approval check, the offset advance, the cancelled end, the lifecycle success, the daemon's session lookup and its record after a failed submit, the ACP validation, the headless record-before-submit, the SDK refresh — each fails its test (ten mutations, one at a time).

### Self-review of `3c9f714` — findings fixed in the follow-up commit

The background review found seven issues and a set of doc and test gaps; all verified and fixed, each fix revert-cycled where behaviour can show it: (1) the claimed saving was inverted — `get_session` rebuilds the projection from event one, a fresh ledger connection per event, so the new caught-up check (twice per pass) cost more than the pending-approval scan it replaced; the kernel client gains `session_tip` (one query for the highest committed seq), which the caught-up check and the TUI's own submit seq use, the redundant check in `step` is gone (the offset advance inspects late entries on the next pass), and the queue's per-tick pending-approval scan now happens only while the projection lags (`approval_pending`: the projection's approvals once it holds every event, the ledger otherwise — `a_queued_message_waits_for_an_approval_the_view_has_not_seen_yet`); the goal loop keeps its caught-up gate, which it needs to see a turn's end before starting another (`a_goal_waits_for_a_pending_approval_and_stops_when_the_turn_it_waited_behind_fails` now steps once before the failure reaches the view); (2) the ACP trust check and the content validation had no test: the decision is one method, `prompt_to_gate`, and the dispatch test asserts it judges a valid prompt and not an image block, an empty text block, a plain-string prompt, an untrusted project or a session with a running turn; (3) a cancelled child a hook blocked was held, not discarded — `ChildEnd::Cancelled` is checked before the block, discards its changes ("the child was cancelled"), and shows cancelled in `/agents` and the job table (`child_end_counts_an_effective_success_and_a_failure_outranks_a_block`, the settle table); the detached job state now derives from the end too (`ChildEnd::job_state`), so an applied child's job is completed — it had no test; (4) the ACP validation missed two refusals of the real path — a session whose turn is still running and the binding cap — both are checked; (5) a resumed headless run whose submit conflicts still runs, unrecorded, on the history it read — as before the gate; this is now stated (ADR 0022 §7, `open_existing`'s doc) rather than implied away; (6) `Session.refresh()` could move the seq backwards past one a concurrent run noted — it never does (the SDK test); (7) a protocol error freezes the projection, and the goal then waited silently forever — it stops, saying so (`a_goal_whose_view_froze_on_a_protocol_error_stops_and_says_why`). Doc fixes: ADR §7 no longer claims the gate prevents a stale submit seq (the TUI submits at the tip anyway), the `prompt_submit_block` / `prompt_submit_decision` docs describe headless and "accepted or not" correctly, and `drive_autonomous_goal` has its own doc comment back.

Revert cycle: the goal loop's caught-up gate, the queue's ledger fallback, the frozen-view stop, the ACP trust check, its content validation, its running-turn check, cancelled before blocked, the cancelled discard, the applied child's job state, the SDK seq — each fails its test (ten mutations, one at a time). The tip query replacing the replay changes cost, not behaviour, so no test can fail on its revert; it was checked by reading `EventLedger::last_seq` against the projection's seq.

### Self-review of `83f425f` — findings fixed in the follow-up commit

The background review found no regression in `83f425f`; its most serious finding is older, and this commit's reasoning leaned on it: (1) the TUI transcript is bounded at 4096 entries, and the goal loop marked iterations by transcript *length*, which stops moving at the bound — past it the loop saw nothing (no failure, no interrupt, no repeated answer), so a goal whose turns fail at once would retry without end on an unbounded budget; the projection now keeps a position that keeps counting (`AppState::transcript_end`) and `transcript_since(mark)` returns what came after a mark, or `None` when the bound already dropped some of it — the loop marks positions, and stops (saying so) if entries it had not read were dropped (`a_transcript_position_keeps_counting_past_the_bound`, `a_goal_at_the_transcript_bound_still_sees_the_turn_it_waited_behind_fail`); this retires the separately flagged transcript-cap task; (2) for a failed or cancelled child a hook blocked, the job state named the child's end while the spooled report and the foreground result still led with the hook's block — both lead with the block only when the block decided, and otherwise say it was moot (`a_blocked_childs_changes_are_settled_after_the_hook_and_it_ends_failed` gains a failed child a hook denies); (3) the worklist said the ACP binding cap is tested — it is checked, not tested (a test would need 1024 bound sessions); (4) stale docs: `ui_caught_up` no longer claims a stale submit seq, `AgentViewManager::settle` names the cancelled case, and the `3c9f714` section's "the detached job state now derives from the end … an applied child's job is completed" described what the old match already did — the new parts were the cancelled and failed rankings (item 2 here).

Revert cycle: the position counter (both tests), a mark read as an index, the moot note, the block leading only when it decided — each fails its test (five mutations, one at a time). Not tested: the stop when the bound drops unread entries (it needs more than 4096 entries between two passes; `transcript_since` returning `None` is tested), and the detached spool text (the detached path has no hook-block test).

### Self-review of `5f4f800` — findings fixed in the follow-up commit

The background review found no regression in the goal loop; it found the same bound blinding the screen, a truncation regression and weak tests, all verified and fixed, each fix revert-cycled: (1, older, the same bug) the renderer painted the transcript by *length* too, so past 4096 entries nothing new reached the screen — not even the goal loop's new stop message; it keeps a position (`rendered_mark`), repaints the whole transcript when it has none (the first frame) or when the bound dropped entries it never painted, and a session switch (`/resume`, `/fork`, `/rewind` — a fresh projection restarts its count) asks for a full repaint (`the_renderer_paints_what_arrives_after_the_transcript_bound`); (2, a regression in `5f4f800`) a failed child a hook blocked lost its settle note to the result bound — the failure path cut from the end; both paths now cut the reason first so the note (what became of the child's changes) always survives (`detail_keeping_note`; the settle table's failing runner now reports a long reason); (3) the new bound test could not tell the waited-behind failure from a later one — it asserts no iteration ran; (4) the stop when the bound dropped unread entries has a test (`a_goal_whose_unread_entries_the_bound_dropped_stops_and_says_so`); (5) the cancelled case of a blocked child was claimed but untested — the moot note's wording for both ends is tested (`a_moot_block_names_how_the_child_ended_and_a_long_reason_keeps_the_note`); the `83f425f` section also named four mutations while counting five. Not tested: the session switch asking for the repaint (the switch itself needs a second session's ledger; the repaint is tested directly).

### Self-review of `b05c8b6` — findings fixed in the follow-up commit

The background review found no leak and no wrong-host send; its most serious finding is older than the program and the commit's own doc comment asserted the opposite: (1, **high**, since `5e4ac8b`) `/model` resolved configuration with `HOME` set to the session's RapidLM home, so it looked in `…/.rapidlm/.rapidlm/config.toml` — on a default install `/model` listed no models and `/model select` failed, while turns read `~/.rapidlm/config.toml`; it now passes the RapidLM home as `RAPIDLM_HOME` (whose `config.toml` is what a turn reads by default), and the tests that had encoded the bug write the config where a turn reads it (`model_select_switches_lists_and_clears_through_the_session`, `kernel_action_select_model_switches_the_session_override`); (2) in the chat dialect a tool call with a bad top-level name, or a new call with no name at all, was silently dropped (its arguments discarded, the run accepting an empty answer) — both are `Permanent` now; an id repeated on a later delta of a started call is still fine; (3) the reply-side mappings also turned an over-long id's `BoundExceeded` into `Permanent` — this section's fix remapped only `InvalidRequest`, which the `fd0cb7f` review showed sends such a reply into context recovery; every reply-side failure is `Permanent` again; a table test covers a control-character id, a nameless call, a bad top-level name and a 129-byte id in the chat stream, and a bad id in the second dialect's stream and body; (4) any `metadata` error on a symlinked config was reported as a symlink chain — the OS's own reason is reported ("cannot be followed: …"); (5) the dangling-chain refusal was tested on macOS only — the test goes through a directory link so every system pays two links per hop (21 hops are 42 for it), with and without the file, on every Unix; (7) `/model` lists the managed lock when one decides; (8) a `RAPIDLM_CONFIG` or policy path set but not valid Unicode read as "not set" — it is an error; (9) under a lock a typo was answered as "the lock decides" — a name no profile has is said to be missing first (`model_select_under_a_managed_lock_says_the_lock_decides`); (10) nits: `select_model` has its doc comment back, a stale comment in `verify` is gone. Record corrections: the `24d6c6f` section's item (3) said "both dialects" before the chat fallback was covered, item (5) held on macOS only, and item (6) left a local inference server's name in one fixture (neutral now); the `model_env` doc no longer claims turn-side parity it did not have. Not tested: the lock line in `/model` list and the non-Unicode refusal (both read the process environment).

Revert cycle: the renderer's position, the note-keeping failure detail, the dropped-entries stop, `/model`'s RapidLM home, the typo check, a nameless call, reply-side `InvalidRequest` remapped, the bound kept, the dangling chain — each fails its test (nine mutations, one at a time).

### Self-review of `fd0cb7f` — findings fixed in the follow-up commit

The background review found a regression of `fd0cb7f`'s own and gaps in the renderer fix, all verified and fixed, each fix revert-cycled where a test can show it: (1, **a regression**) letting a reply's over-long tool id or name through as `BoundExceeded` sent the turn into context recovery — the host summarised the conversation to "fix" a reply, then ended the turn; every reply-side parse failure of a tool id or name is `Permanent` again (the `b05c8b6` review's advice to keep the bound was wrong, and its record item (3) is corrected here) — the table test's 129-byte id expects `Permanent`; (2) the renderer's painted copy no longer had any bound (the length bug had capped it by accident) — it is rebuilt from the projection once it holds twice the bound (`the_renderer_paints_what_arrives_after_the_transcript_bound` feeds one entry a frame past that); (3) after a rebuild the scroll anchor pointed into the old copy — a rebuild follows the tail (the same test scrolls back first); (4) the nameless-call check keyed on the index — a second call at a used index with no name of its own slipped through; it compares the id (a table case); (5) `/model` still read fewer variables than a turn: it passes the process's `HOME` and `USERPROFILE` fall-backs after the RapidLM home (the commit said "as turns read it" — true now for the file chosen); (6) `/model` list ignored a policy it could not read — it says so; (7) record errors: "the note always survives" holds unless the note itself (a failed worktree cleanup, with its path) passes the bound — then its end is cut; item (8) of the `b05c8b6` section is moot while the session reads the whole environment at start-up (the flagged non-Unicode task); the `b05c8b6` section's numbering skipped (6); its worklist entry omitted two tests; (8) tests: the session switch is tested through `/rewind` (a line painted in the parent is gone after the switch — the first version of that check could not fail and was redone), the drop-triggered repaint is tested, and the slow bound test no longer clones the whole state per entry.

Revert cycle: the reply-side bound, the nameless call at a used index, the painted copy's bound, the rebuild following the tail, the switch repaint — each fails its test (five mutations, one at a time; the switch check was strengthened after its first cycle passed). Not tested: the `/model` environment's fall-backs and the unreadable-policy line (both read the process environment).

### Self-review of `21a5362` (the reply parser) — findings fixed in the follow-up commit

The background review found an older bug of the kind `21a5362` targeted and a regression of its own in the chat dialect's tool calls, all verified and fixed, each fix revert-cycled: (1, **medium**, older) a reply's size limits still sent the turn into context recovery — the 4096-event cap, the body and header caps, and the streamed body cap returned `BoundExceeded`, which the host answers by compacting the conversation, though compaction never shrinks a reply; and each SSE delta was its own event, so a reply of about 4096 tokens (a file written through a tool call, one token a frame) broke the event cap, was compacted and asked again, and ended as a context-retry failure. Every limit on what is read is the reply's now (`reply_too_large`: the transport's reads, the parsers' event caps in both dialects, the collected stream's bounds) — `Permanent`, a rejection like any malformed reply — and adjacent deltas of the same text or the same call's arguments are merged up to the delta bound, so the event cap counts bytes rather than frames (`a_reply_of_many_small_deltas_is_merged_under_the_event_bound`, both dialects: 5000 frames of text and 5000 of arguments arrive whole in a handful of events; deltas that cannot merge still meet the cap as `Permanent`; `an_oversized_reply_is_the_providers_failure_not_a_context_bound`, buffered and streamed); (2, **a regression** in `21a5362`) the chat dialect's nameless-delta check compared the id only with the one last seen at the same index, so a server that omits `index` (every delta falling to 0) and interleaves calls had its valid reply refused — calls are judged by id now: a started id continues its call wherever it appears, repeating its name starts nothing new (a server that repeats id and name on every delta no longer emits a second start the step layer refuses), renaming it is malformed, and an id never started is still a call that could never run (`tool_calls_are_judged_by_id_wherever_their_deltas_arrive`); (3) the identifier mapping turned any error it was given, a cancellation included, into `Permanent` — it is `malformed_reply_identifier` now and maps only what an identifier parse raises; the second dialect's over-long id, streamed and whole, has a test. Record corrections: the `b05c8b6` section's "every reply-side failure is `Permanent` again" held for tool ids and names only — the size limits stayed a context bound until this commit; the `fd0cb7f` section's item (4) ("it compares the id") refused valid index-less streams. Still refused, as since `fd0cb7f`: an id whose name arrives only on a later delta.

Migration impact: a reply past a size limit ends as a provider rejection (retried by the step layer like any other) instead of compacting the conversation; long replies streamed a token a frame now arrive whole instead of failing at 4096 frames; a collected stream carries fewer, longer deltas (the live text path is unchanged).

Revert cycle: text and argument merging, the merge's delta bound, merging only one call's arguments, the event cap, the read and streamed-body limits, the kept cancellation, the renamed call, the started id at any index, the started-id memory, the second dialect's over-long id — each fails its test (thirteen mutations, one at a time).

### Self-review of `06d44ac` — findings fixed in the follow-up commit

The background review found two issues and two nits in the setup plan (still dry-run only, so none reached a file); all verified and fixed, each fix revert-cycled: (1) the origin rule covered a custom endpoint but not a preset whose URL `--base-url` overrides — `--preset openai --base-url http://10.0.0.9:8000/v1` wrote the preset's key variable for the new host, and a run would have sent the provider key there over plain http; a preset's key variable now goes only to the preset's own origin — on another origin, with no key flag, the profile keeps what it names for that endpoint (the custom-endpoint rule: kept only for the same origin as before, otherwise removed), and the plan says `--key-env <VAR>` sends it there (`a_presets_key_goes_only_to_the_presets_own_endpoint`; AC-01's loopback dry run now names its key variable explicitly; `rapid setup --help` says so); (2) under a managed locked default, a run whose shell's `RAPIDLM_MODEL` named a missing profile failed outright — `resolve_gated` resolved the shell's own choice with `?` only to decide whether to report the lock; the lock overrules such an override like any other now, and reports it (`locked_default_beats_env_and_user_selection` extended) — this was the run's behaviour, not only the plan's, and predates this program; with it, the plan's own check needs no change. Nits: the unknown-key note matched by prefix, so a quoted profile `[model."default.x"]` lent its keys to a plan for `default` — the key must sit directly in this profile (`only_this_profiles_unknown_keys_are_named`); a chain of exactly 40 links was called a loop — 40 are followed (`a_chain_of_forty_links_is_followed_and_forty_one_is_refused`, Unix).

Revert cycle: the preset's key on another origin, its note, the exact profile segment, the fortieth link, the lock over a missing override — each fails its test (five mutations, one at a time).

## SEAM-02-2 (part a) — Live verification: one bounded request, typed failure classes, a quota class of its own

Contract restated: `apps/rapid/src/model.rs` (`ConfiguredModel::probe` — one request through the model client a run builds, its own adapter and transport, returning the provider's own error class rather than the step layer's folded cause, so a 429 and a 5xx, or a 402 and a 400, stay apart), `apps/rapid/src/setup.rs` (`verify`: the planned profile resolved from the planned document — this shell's `RAPIDLM_MODEL` set aside — its key as the plan names it, `max_tokens` 16; `classify` into auth / quota / network / server / invalid, each with its own exit code 11–15 and a hint naming the next command; a run without `--dry-run` verifies first and, on failure, prints the class, the hint and "no files were changed"), `crates/llm-router` (a `402` was a generic permanent error: `ProviderError::QuotaExceeded`, never retried, with `FailureClass::Quota` and `StopReason::QuotaExhausted` in the fallback chain), `crates/agent-runtime` (`FailureCause::Quota` — the step layer used to retry a 402 five times as a "rejection"). Migration impact: a `402` now ends a turn at once with "exhausted provider quota" instead of five retries and a "provider rejection"; the fallback chain treats it like an auth failure (falls over to an explicit alternate, never retries the same model) and reports `quota_exhausted`.

| Criterion | Status | Evidence |
|---|---|---|
| One ≤16-token request through the router, with the named key | done | `a_verified_plan_sent_one_bounded_request_with_the_named_key` (a loopback endpoint receives exactly one request, `"max_tokens":16`, `Bearer` with the `--key-env` value; with `--key-stdin`, the key read from stdin) |
| AC-02 each failure class is its own exit code and hint; nothing is written | done | `each_verification_failure_has_its_own_exit_code_and_changes_no_file` (401 → 11 auth, 402 and 429 → 12 quota, a refused port → 13 network, 500 → 14 server, a non-JSON 200 → 15 invalid; the home byte-identical after each; "no files were changed" every time; the key never printed) |
| An unset key variable sends nothing | done | `an_unset_key_variable_sends_nothing` (exit 11, the variable named, no connection) |
| A 402 is an exhausted quota, never retried | done | `payment_required_is_an_exhausted_quota_never_retried` (both adapters), `an_exhausted_quota_is_its_own_class_and_stop_reason` (fallback), `an_exhausted_quota_is_not_retried` (the step layer tries once) |
| Revert cycle | done | 402 back to a generic error, the unset-key check removed, the output ceiling not set, a quota retried by the step layer, a quota read as transient by the fallback chain, 429 read as a server failure, the stdin key not used, 5xx read as invalid — each fails its test (three builds) |

Recorded decisions. **The egress receipt (S10):** `journal::record_egress` records into a project ledger's operation journal under a session; `rapid setup` runs in no session and, by AC-02, must leave the configuration directory byte-identical when verification fails — so a durable receipt for a failed probe has nowhere it may be written. The receipt is the egress proxy's audit record, which part b makes every probe produce (allowed or denied) and report in its outcome; `journal::record_egress` gets its first producer where a session exists (`rapid doctor --live` inside a project records into the project ledger). **Split:** SEAM-02-2 lands in two parts — this one (the probe, the classes, the quota class) and part b (the probe's dial bound to an egress-proxy lease, `HTTP(S)_PROXY`/`NO_PROXY` in the provider transport, `rapid doctor --live`). Until part b the probe dials as every model call does today, directly. **Non-TTY (S10):** the probe runs because the user ran `rapid setup`, whose documented job is to verify; `--no-verify` skips it.

Disclosed: a verified run still writes nothing and exits 2, saying so — writing is SEAM-02-3. `rapid --help` now summarises `setup` as "plan and verify a model configuration".

### Self-review of `734a595` — findings fixed in the follow-up commit

The background review (re-run after a rate limit) found fourteen issues; the one it rated highest — a preset's key sent to another `--base-url` — was already fixed by `82b2a5a`; the rest are verified and fixed here, each revert-cycled, together with item (2) of the `82b2a5a` review: (1, **high**) the probe's output bound went out as `max_tokens`, which the `openai` preset's first-party API refuses for its reasoning models (the preset's default model among them) — a valid key failed as "invalid"; the chat dialect now sends the bound as `max_completion_tokens` to that API (which accepts it for every model) and as `max_tokens` everywhere else (`the_first_party_endpoint_reads_the_output_bound_as_max_completion_tokens`) — a run whose profile sets `max_tokens` against that API changes the same way, and now works with its reasoning models; (2) `--key-stdin` on a terminal waited silently for end-of-file while echoing the key — it is refused before anything is read, naming the pipe (`a_key_on_a_terminal_is_refused_before_anything_is_read`; a dry run reads no key and still runs); (3) a kept credential the probe would not send — its variable unset, or a `keychain` alias this build's reader ignores — was probed without a key and a refusal blamed on "the key"; each is named and nothing is sent, and a keyless request refused with 401/403 says a key is required and none was sent, naming the preset's variable (`a_key_the_probe_would_not_send_is_named_instead_of_blamed_on_the_endpoint`); a non-dry run now prints the plan's notes too (`82b2a5a` review, item 2); (4) failures raised before anything is sent were reported as "answered": a key no header can carry is an auth-class failure saying so, and an address the transport's guard refuses is a network-class failure saying rapid does not dial it (`what_is_refused_before_sending_is_not_reported_as_an_answer`); (5) redirects fell through to "transient" — a server failure, retried five times by runs — both dialects now treat 3xx as permanent (`a_redirect_is_permanent_at_the_adapter_and_an_empty_body_is_not_a_message`, `an_exhausted_quota_reported_as_429_is_not_a_rate_limit_and_a_redirect_is_permanent`); (6) a 2xx body with no completion in it (`{}`, or `data: {}` then `[DONE]`) verified, and a run read it as an empty answer — it is a permanent error now, while an empty answer the server finished still counts (`a_body_with_no_completion_in_it_is_not_an_empty_answer`); in-stream errors name an exhausted quota, a server error and numeric status codes (`in_stream_errors_name_quota_server_and_numeric_codes`); (7) the probe skipped the managed effort floor a run applies — it carries it (`the_probe_carries_the_managed_effort_floor_a_run_would`); (8) an exhausted quota reported as a 429 (`insufficient_quota`) was a rate limit, retried five times by runs — it is `QuotaExceeded`; `FailureClass::requires_explicit_alternate` counts the quota class as the fallback plan already did; (9) the network and invalid hints now name the next command, and every usage refusal of a key says no files were changed; the CLI reference lists the quota cause and `failed:quota`. Tests added: 403, the JSON failure shape, `--no-verify` sending nothing (`no_verify_sends_nothing_and_says_nothing_was_written`).

Record corrections: the part (a) table's AC-02 "done" covers the configuration directory — its keychain half is vacuous until SEAM-02-3 writes to the keychain; "through the router" means through the adapter and transport a run builds, without the step-layer retries or the fallback chain (one request by design). Disclosed: the refused-port case binds and drops a listener to find a free port, so a parallel test could in principle take it in between. Migration impact (runs): a 3xx, an in-stream server error and an `insufficient_quota` 429 are no longer retried as transient; a body with no completion is an error rather than an empty answer; a profile with `max_tokens` against the first-party API sends `max_completion_tokens`.

### Self-review of `82b2a5a` — findings fixed in the follow-up commit

The background review found no leak and no wrong-host send; it found one inconsistency in runs and several smaller gaps, all verified and fixed, each fix revert-cycled: (1) under a lock the compaction route still re-read the shell's `RAPIDLM_MODEL` — a missing profile there dropped `[phases] compact` with a warning; the route now resolves against the primary as gated (`resolve_purpose_model_for`; `the_phases_compact_override_is_resolved_and_gated_like_a_fallback_entry` extended with a lock and a missing override); and a stale `/model select` override naming a removed profile is overruled by a lock too, as `RAPIDLM_MODEL` is (`under_a_lock_a_stale_session_override_is_overruled_not_fatal`); (2) the non-dry run hid the plan's notes and blamed a key never sent — fixed with the `734a595` review (below); (3) the lock was reported when nothing had been overridden (a config with no default) — it is reported only when the shell's `RAPIDLM_MODEL` or the file's default asked for another profile, existing or not (`locked_default_beats_env_and_user_selection` extended); (4) the unknown-key note hid a quoted dotted key of this profile (`"x.y" = 1`) — keys are matched exactly against this profile's own table (`only_this_profiles_unknown_keys_are_named`, with positive controls); (5) keyless presets on another origin keep what the profile names for that origin — documented on `Credential::Unchanged` and tested, as is the keyless preset on its own origin (`None`); (6) a chain of symlinks the system itself will not follow (32 on some) was planned — the target is checked against the OS (`resolve_symlinks`; the chain test compares with `std::fs::metadata` at 1, 32, 33 and 40 links); (7) test gaps closed: the kept credential on the proxy's own origin (kept, no note), the note in the moved case, the lock with a missing override at plan level. Record corrections for the `06d44ac` section: issue (2) was in the run path, not the dry-run plan; "the lock overrules it like any other" was not yet true of the compaction route or a session override (now it is); "the plan says `--key-env <VAR>`" holds when nothing is kept. Rule 2.4: `apps/rapid/src/user_config.rs`, edited here, no longer names a peer product, and its test fixtures no longer carry an upstream model name outside a provider module (rule 2.2).

### Self-review of `6914cb1` — findings fixed in the follow-up commit

The background review found five issues and five smaller ones; all verified and fixed, each fix revert-cycled: (1, a regression) the probe refused a config a run handles keyless — a local server whose profile names an optional key variable that is unset failed with "not set"; the probe now goes keyless exactly as a run would, and only a refusal of that keyless request says why no key was sent: the unset variable the profile names, or a `keychain` alias this build does not read (`a_key_the_probe_would_not_send_is_named_instead_of_blamed_on_the_endpoint`: the keyless local server verifies; 401s name the variable and the alias); (2) a reply the server did send was reported as "no request was made" when its usage object was malformed — reply-side parse failures are `Permanent` (the provider's failure) in both dialects, `InvalidRequest` staying for what is refused before sending; (runs cannot see the difference: the step layer folds both into "provider rejection" and maps that back for the fallback chain, so only setup's message changes — the record said otherwise, corrected in the `24d6c6f` section) (`a_body_with_no_completion_in_it_is_not_an_empty_answer` includes a malformed usage); (3) the keyless hint recommended the preset's variable on another origin, where `82b2a5a` keeps that key off the host — it names the preset's variable only on the preset's own origin; (4) record errors in the `734a595` section: runs still retry a 3xx and a body with no completion five times — the step layer retries every "provider rejection" — so item (5) said too much, the migration line had an in-stream server error backwards (it is now retried as transient; before, it was permanent), and a body with no completion is now five retries then "provider rejection" rather than an empty answer; the tests are renamed for what they prove (`a_redirect_is_permanent_at_the_adapter_and_an_empty_body_is_not_a_message`, `an_exhausted_quota_reported_as_429_is_not_a_rate_limit_and_a_redirect_is_permanent`); (5) under a lock, a planned profile outside the managed allowlist was probed and refused as "answered" — the plan refuses it first, naming the allowed providers (`under_a_lock_this_shells_missing_override_is_overruled_not_a_failure`); (6) the no-completion rule covered one dialect — an untyped Responses event with nothing a response carries is ignored, and the second dialect's `{}` is not a message; (7) the first-party API's regional subdomains get `max_completion_tokens` too; (8) in-stream numeric codes 408/409/425 are transient, and codes sent as strings are read; (9) `--key-stdin --no-verify` on a terminal is allowed until a key is read to be stored (SEAM-02-3). Nits: the fallback doc names the quota class; the refused-address hint names broadcast and multicast; the unusable-key hint names length too. Still true and disclosed: `NoKey`/`KeptKeyUnset` name the first of several `env_key` entries; two usage refusals (a lower-case `--key-env` name, conflicting key flags) do not say "no files were changed" — the record's item (9) in the `734a595` section claimed every one did.

### Self-review of `24d6c6f` — findings fixed in the follow-up commit

The background review found no leak and no wrong-host send; it found one user-visible regression, record errors and weak tests, all verified and fixed, each fix revert-cycled: (1, a regression) under a lock a valid `/model select` was skipped silently — the command answered "model switched" and every turn ran on the locked profile with no warning; the override is kept for the gate again (only the "does it exist" check is skipped under a lock), so the lock is reported, `/model` now resolves with the managed policy in force (its environment is read one variable at a time, never with `std::env::vars()`), and a lock that picks another profile is said at the command, with no override recorded (`model_select_under_a_managed_lock_says_the_lock_decides`; `under_a_lock_a_stale_session_override_is_overruled_not_fatal` asserts the gate's report for a valid and a removed profile); (2) record error: the `6914cb1` section said the fallback chain now classes a malformed reply as permanent — runs fold both errors into "provider rejection", so only setup's message changed; corrected there; (3) a tool call whose id or name a reply spells outside the alphabet was still `InvalidRequest`, reported by setup as "no request was made" — reply-side parse failures of tool ids and names are `Permanent` in both dialects (`a_body_with_no_completion_in_it_is_not_an_empty_answer`, `a_malformed_reply_is_the_providers_failure_not_a_refusal_before_sending`); (4) the plan-level lock test never read the shell's override — it plans the locked profile now; (5) `resolve_symlinks` skipped the check for a chain whose file is not created yet, and its test compared the implementation with itself — a chain the system will not follow is refused whether or not its file exists, and the test uses relative links in a canonical directory with explicit expectations (macOS: 32 followed, 33 refused, with and without the file; Linux: 40 followed); (6) record error: the `82b2a5a` section said `user_config.rs`'s fixtures no longer carried an upstream model name — a model tag, a local server's name and its key variable remained; they are neutral now, and the model-id doc comment no longer names models (the dialect names `openai-compatible` and `anthropic` are config values and stay); (7) the second dialect's malformed-usage change and its empty SSE body are tested; (8) the terminal rule's exemption for `--key-stdin --no-verify` depends on no key being read yet — SEAM-02-3's worklist entry now carries the obligation to restore the refusal when it reads a key to store it; (9) nits: `AuthNoKey` no longer carries a variable (the branch naming one was unreachable), and the plan's allowlist refusal says why — the profile it writes becomes the file's default.

Revert cycle: a locked override kept from the gate, the command's lock message, the chat and second-dialect tool-name mappings, the dangling-chain refusal, the lock over a missing override at plan level, the second dialect's malformed usage — each fails its test (seven mutations, one at a time). Not tested: the `/model` environment's managed-policy variable (it is read from the process).

## SEAM-02-2 (part b1) — The provider transport can dial through a proxy and ask a gate first

Contract restated: `crates/llm-router/src/providers/dial.rs` (new) — `ProxyConfig::from_env` reads `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` (upper case first) from explicit pairs the caller supplies; only `http://` proxies (an `https://` one is refused, naming the variable, never its value); a loopback or `NO_PROXY` target is dialled directly; credentials in the proxy URL are percent-decoded into a `Proxy-Authorization` value that `Debug` redacts. `DialGate::permit(target, via, addrs)` is asked before any connection with the target, the proxy and the addresses about to be dialled, and returns the addresses that may be dialled (so an egress policy can bind the dial to its own resolution) or refuses. `crates/llm-router/src/providers/openai_compatible.rs` — `Http1Transport::with_proxy` / `with_dial_gate`, and one `open_stream` in place of the three copies of resolve → guard → connect → TLS (`post_raw`, `execute`, `execute_streaming`): an `https` target through a proxy is a `CONNECT` tunnel with TLS to the target inside it (the target's bearer never reaches the proxy); an `http` target through a proxy is an absolute-form request carrying the proxy's credentials. Migration impact: none — the defaults are no proxy and no gate, and no composition root installs either yet (part b2 does, for the setup probe). One order changed: the bearer is obtained before the target is resolved rather than after, so an unresolvable host with an unusable token now reports the token.

| Criterion | Status | Evidence |
|---|---|---|
| Proxy variables read, loopback and `NO_PROXY` direct, credentials never printed | done | `proxy_variables_are_read_upper_case_first_and_credentials_never_printed` (percent-encoded credentials decoded), `no_proxy_and_loopback_are_dialled_directly`, `an_unusable_proxy_url_is_refused_naming_the_variable_not_its_value` |
| `http` through a proxy: absolute form, proxy credentials, both request paths | done | `an_http_target_through_a_proxy_names_its_whole_url_and_the_proxy_credentials` (a loopback stand-in proxy records both requests: `invoke_sync` and `invoke_sync_streaming`) |
| `https` through a proxy: a `CONNECT` tunnel; a refusal is a network failure | done | `an_https_target_through_a_proxy_is_a_connect_tunnel_and_a_refusal_is_a_network_failure`, `a_connect_tunnel_needs_a_2xx_from_the_proxy` |
| The gate is asked before any connection; a refusal dials nothing | done | `the_dial_gate_is_asked_before_any_connection_and_a_refusal_dials_nothing` (direct: nothing reaches the listener; proxied: the gate sees the target, the proxy and the proxy's addresses) |
| Revert cycle | done | the proxy ignored, origin-form through a proxy, no proxy credentials, no tunnel, the gate not asked, the streaming path dropping the proxy, credentials not decoded — each fails its test (seven mutations, one at a time) |

Recorded decision — **S11 and the proxy variables:** a user who changes nothing must get the same behaviour, and a user whose shell already exports `HTTPS_PROXY` would otherwise find model calls routed through that proxy after upgrading (and an `https://` proxy value, which the transport refuses, would turn working calls into failures). So the transport honours proxies only when a composition root passes a `ProxyConfig`; part b2 makes that an explicit opt-in read the same way by runs and by the setup probe (a probe that dialled differently from a run would verify the wrong path). Found while revert-cycling: restoring a mutated file by rename leaves its modification time older than the mutated build, so cargo keeps the mutated artifact; the revert scripts now touch every restored file.

### Self-review of `fce5320` — findings fixed in the follow-up commit

The background review found thirteen issues, none reachable by users yet (no composition root installs a proxy or a gate), two of them blocking part b2; all verified and fixed, each fix revert-cycled where a test can show it: (1) the gate's returned addresses were dialled without the address guard — a gate resolving a name to a metadata address would have been obeyed; what a gate returns is dialled only where the guard allows and only on the port being dialled (`what_a_gate_returns_is_dialled_only_where_the_guard_allows_and_on_the_port_dialled`); (2) the system resolver ran before the gate and its failure meant the gate was never asked — with a gate installed, an unresolvable name is the gate's to resolve (same test, a `.invalid` name the gate resolves); (3) the gate contract now says whose name it resolves (the proxy's when `via` is set); (4) a proxied target skipped the resolved-address guard — a name that resolves locally to a refused address is refused through the proxy too, best effort (a name only the proxy can resolve goes through), documented in the module doc; (5) no test opened a tunnel — one now does: after a `200` from the stand-in proxy, the next bytes are a TLS handshake carrying neither the bearer nor the proxy's credentials, and the `CONNECT` carries them (`an_opened_tunnel_carries_tls_to_the_target_and_nothing_of_the_request`); (6) `NO_PROXY` dropped CIDR ranges and `*.` names silently, cut the list at 64 and suffix-matched names against addresses — entries are names (and subdomains, `.x` and `*.x`), exact addresses, ranges and `*`; more than 256, or one that does not parse, is refused naming the variable (`no_proxy_takes_names_addresses_and_ranges_and_refuses_what_it_cannot_read`); (7) loopback in the spellings the transport decodes (`127.1`, `0x7f000001`) was proxied — it is dialled directly (same test); (8) a proxy's `407` read as a provider rejection on an http target — only a proxy sends it, so both dialects read it as a network failure (`a_proxy_refusing_its_own_credentials_is_a_network_failure`); (9) the bearer was obtained before the address was resolved and guarded — the order is restored: resolve, guard and ask the gate, then the bearer, then connect (`the_address_is_resolved_and_guarded_before_the_credential_is_looked_at`); (10) a proxied https request could take three timeouts — connecting and tunnelling share one; (11) the variables are read as most tools read them: `https_proxy` then `HTTPS_PROXY`, `http_proxy` in lower case only (the upper-case name can come from a request header in some server environments), `no_proxy` then `NO_PROXY`; a variable set but empty turns its proxy off; names compare ignoring case on Windows (`proxy_variables_are_read_lower_case_first_and_credentials_never_printed`, `the_upper_case_http_proxy_is_not_read_and_an_empty_value_turns_a_proxy_off`); (12) the module doc says an http target's request, bearer included, is visible to its proxy; (13) nits: a user without a password is `user:`, a malformed `%` escape is refused, text after `]` is refused, a `CONNECT` reply must be `HTTP/…`, and one ending in bare line feeds is read (`a_connect_tunnel_needs_a_2xx_from_the_proxy`).

Revert cycle: nineteen mutations, one at a time, each failing its test — except that a name entry's "never an address" guard cannot fail one (every dotted-number entry already reads as an address, so the guard is defence in depth), and the proxied-name guard (4) and the shared dial budget (10) have no portable test (a name resolving to a refused address, a timing bound); both are checked by reading.

### Self-review of `92a9208` — findings fixed in the follow-up commit

The background review found that `92a9208` turned Windows CI red, plus eight other issues; all verified and fixed, each fix revert-cycled where a test can show it: (1, **CI red**) on Windows the case-insensitive variable lookup let the first name tried (`https_proxy`) match an `HTTPS_PROXY` key, so two dial tests failed there (run 35974995371, the Windows job only) — names are looked up in two passes, exact spelling first across all of them, then (Windows only) ignoring case, so the lower-case preference holds everywhere and an error names the spelling the user set (`on_windows_any_spelling_of_a_name_is_read`, Windows-only); (2) the gate test's metadata address was refused by the port check before the address guard mattered — it returns addresses on the dialled port, so only the guard refuses them; (3) the address guard missed IPv4-mapped IPv6 (`::ffff:169.254.169.254` reaches the metadata address on a dual-stack socket) — on every path, direct, proxied and gate output — it reads the IPv4 address such an address maps (the same test, both spellings); this predates the program; (4) a proxy's `407` had become a network failure — retried and walked down the fallback chain, sending the same credentials again and again (a directory-backed proxy account can lock) — it is permanent again, on the tunnel and the plain-http path (`a_proxy_refusing_its_own_credentials_is_permanent_never_retried`, `a_connect_tunnel_needs_a_2xx_from_the_proxy`); (5) the best-effort local lookup of a proxied name had no time bound and ran before a gate could decide — it is skipped when a gate owns resolution; without a gate it remains best effort and unbounded, stated here; (6) `NO_PROXY` refusals named proxy URLs, were read even with no proxy set, and split on commas only — an unreadable entry is named in the error, the list is read only when a proxy is set, and whitespace separates entries too (`no_proxy_takes_names_addresses_and_ranges_and_refuses_what_it_cannot_read`); entries such as `192.168.*` are still refused (read as a range they would be a guess); (7) the address-before-credential order had a test on `post_raw` only — `execute` and `execute_streaming` are tested with a recording authorization that must never be asked (`a_request_is_planned_before_its_credential_is_read`); (8) record errors in the `fce5320` section: its item (11) cited a test that failed on Windows and one that does not run there; "each failing its test" was untrue for the address-guard mutation (item 2 here); "none reachable by users yet" missed that the `407` mapping and the bearer-before-address order applied to every request, with no proxy or gate; the worklist's "resolve-and-guard before the bearer" was tested for `post_raw` only; (9) nits: the module doc says `HTTP_PROXY` is read on Windows; the tunnel test's two text checks that could never fail are gone (the TLS-handshake bytes carry the proof).

Revert cycle: the IPv4-mapped guard, the gate-output guard, the tunnel's and the adapter's permanent `407`, the named entry, whitespace separation, reading the list only with a proxy, `execute`'s plan-before-credential order — each fails its test (eight mutations, one at a time; one first misplaced mutation hit `post_raw` and was redone on `execute`). Not tested locally: the Windows lookup (its test runs on the Windows job); the gate skipping the proxied-name lookup.

### Self-review of `01e5d87` — findings fixed in the follow-up commit

The background review found one claim that was false and smaller gaps, all verified and fixed, each fix revert-cycled: (1, **high**) "a proxy's 407 is never retried" was false: `Permanent` is folded into "provider rejection", which the step layer retries five times with backoff and the fallback chain walks as a configuration error — six requests with the same `Proxy-Authorization` per step, as before; a `407` (tunnel and plain http, both dialects) is now an authentication failure, the class the step layer never retries and the chain moves past only to an explicit alternate (`a_proxy_refusing_its_own_credentials_is_an_authentication_failure`, `a_connect_tunnel_needs_a_2xx_from_the_proxy`); disclosed: setup's hint for it names the key rather than the proxy — a cause of its own lands with part b2b, when a proxy first becomes reachable; (2) with a gate and a proxy installed, the target name was judged by nobody while the docs promised the guard — the `DialGate` contract and the module doc now say the gate alone judges the target through a proxy (it is told the target); the setup probe's gate does, pinning it to the planned endpoint; (3) a refused `NO_PROXY` entry was echoed whole, credentials included (a mistaken URL) — what precedes an `@` is withheld and the entry is cut at 64 characters; (4) "an error names the spelling that was set" was not true of the Windows any-case pass — the comment says it names the spelling looked up; (5) tests and records: the plan-before-credential test is renamed for what it proves (the bearer is not asked for; the adapter reads its credential store first) — `a_request_is_planned_before_its_bearer_is_asked_for`; the empty-value rule is tested on every system; the Windows test covers `HTTP_PROXY` too; the `fce5320` section's item (5) still quoted two assertions `92a9208` removed (the TLS-handshake bytes are the proof); the worklist's "Windows CI green again" is confirmed by the Windows job of the next run, not by that commit's own (it was cancelled when the next commit was pushed).

Revert cycle: the adapter's, the second dialect's and the tunnel's `407`, the withheld credential — each fails its test (four mutations, one at a time).

## SEAM-02-2 (part b2a) — The setup probe dials only on an egress lease, and reports the receipt

Contract restated: `apps/rapid/src/provider_egress.rs` (new) — `ProviderEgress`, a `DialGate` backed by `security::EgressProxy` with an allowlist of exactly one rule: the planned endpoint's scheme, host and port (and, once part b2b routes through one, the proxy's); every dial is authorised and its lease consumed immediately before the dial, bound to the addresses the transport resolved (no second DNS answer is used); every decision is an audit record, and `receipts()` gives one per dial (issuing and consuming a lease are one decision). `crates/security` gains `NetworkClient::Provider` (attribution only). `ConfiguredModel::build_with_gate` installs a gate on the model client's transport (`build` is unchanged: no gate). `apps/rapid/src/setup.rs` — `probe_egress(plan)` builds the gate for the planned endpoint; `verify` dials through it; the outcome reports the receipt: text lines `egress: allowed <scheme>://<host>:<port> (policy: this endpoint only)` / `egress: refused … (<reason>)`, and `"egress": [{allowed, dialled, reason}]` in JSON — on success and on failure. Migration impact: none for runs (no gate is installed on a run's transport); a setup outcome gains the receipt.

| Criterion | Status | Evidence |
|---|---|---|
| The probe is egress under policy (S10): only the planned endpoint is dialled | done | `only_the_planned_endpoint_is_dialled_and_every_decision_is_a_receipt` (the planned endpoint permitted; another port refused, with a refused receipt and its reason), `nothing_resolved_is_nothing_dialled` |
| Every probe reports its receipt, allowed or refused, and writes nothing | done | `a_verified_plan_sent_one_bounded_request_with_the_named_key` (one allowed receipt, in JSON and text), `a_failed_verification_in_json_names_its_class_and_hint_and_writes_nothing` (the refused key still went out on the lease), `an_unset_key_variable_sends_nothing` (nothing dialled, no receipt) |
| Revert cycle | done | the model client not installing the gate, a host-wide rule instead of the exact endpoint, two receipts per dial, the outcome dropping the receipt — each fails its test (four mutations, one at a time) |

Recorded decisions. **Where the receipt goes:** the probe runs in no session and, by AC-02, must leave the configuration directory byte-identical when verification fails — so its receipt is reported in the outcome, not written; `journal::record_egress` gets its first producer with `rapid doctor --live` inside a project (part b3). **Through a proxy:** the gate is told the target and judges it by name (the request must be for the planned endpoint), and authorises the dial to the proxy — the transport does not look at a proxied target's addresses (see the `01e5d87` review). Not tested yet: the proxied path of the gate (no proxy is reachable until part b2b).

### Self-review of `beca89e` + `7ee9645` — findings fixed in the follow-up commit

The background review found a regression in `7ee9645` and smaller receipt and record problems, all verified and fixed, each fix revert-cycled: (1, **a regression**) the probe's gate refused the planned endpoint whenever its name resolved to a loopback, private or shared-address (CGNAT) address — a model server on the operator's machine or network (`gateway.internal` on 10.x, a container service name, a tailnet name) — although runs reach it: the egress policy lifted its "sensitive address class" refusal only for literal-address rules; `security` gains `EgressRule::exact_operator_endpoint`, whose exact name may resolve to loopback or private addresses and never to link-local, metadata-like or unspecified ones, and the probe's gate uses it (`an_operator_endpoint_may_be_local_but_never_link_local`, `a_local_or_private_endpoint_the_operator_named_is_dialled`); the normaliser also refused any name containing `0x` anywhere (`llm-0x1.corp.example`) as an ambiguous address — only a name every label of which is a number is (`only_an_all_numeric_name_is_an_ambiguous_address`), and a name resolving to more than a lease binds is judged by its first 16 addresses; (2) an unresolvable endpoint's receipt read `refused ?://?:?` — the gate records its own receipts, one per dial asked for, naming what it would have dialled and why (`unresolved`, `sensitive_class`, `not_the_planned_endpoint`, …) (`nothing_resolved_is_nothing_dialled`, `two_dials_are_two_receipts` — the de-duplication that could merge two real dials is gone); an IPv4-mapped first address is compared as the IPv4 address it maps; (3) record error in the `01e5d87` section: the Windows test did not cover `HTTP_PROXY` (the edit had missed silently) — it does now; (4) a refused `NO_PROXY` entry's credential still reached `Debug` and, with a comma in the password, a piece of it reached the message — the stored entry is the withheld form, and a list holding a URL is refused whole before splitting; (5) setup's message when the probe's gate cannot be built names the reason, says no files were changed, and exits 2; `probe_egress` no longer sits between `verify` and its doc comment. Record corrections: the `01e5d87` section's `407` disclosure was too narrow — an intercepting proxy's `407` reached runs too, reported as an authentication failure, until `a257725` gave it its own cause; its "the chain walked it as a configuration error" contrast was overstated (configuration and authentication both move only to an explicit alternate — the real change was the step layer no longer retrying it); item (2) cited the probe's gate before `7ee9645` existed; the b2a section said the gate authorises the dial to a proxy — the probe passes no proxy until part b2b-2.

Revert cycle: the operator-endpoint rule, the link-local exclusion, the `0x` label rule, the unresolved receipt, the 16-address bound, the mapped-address comparison, the withheld stored entry, the URL refused whole — each fails its test (eight mutations, one at a time).

## SEAM-02-2 (part b2b-1) — A proxy refusing its own credentials is a failure of its own

Contract restated: a proxy's `407` (the tunnel's `CONNECT` answer and a plain-http answer, both dialects) is `ProviderError::ProxyRefused` — never retryable, kind `proxy_refused` on the wire (the public error code is unchanged: `InternalUnexpected`, as for the other provider classes without one), `FailureClass::Auth` in the fallback chain (an explicit alternate — another path — or a stop). The step layer carries it as `FailureCause::ProxyAuth` ("proxy authentication; check the user and password in the proxy variable", tag `failed:proxy_auth`), which it never retries. `rapid setup` reports it as its own failure (network class, exit 13) with a hint naming the proxy variable, not the key. Migration impact: none reachable yet — nothing routes through a proxy until the opt-in (part b2b-2); a `407` from an intercepting proxy on the network was an authentication failure (since `beca89e`) and is now this.

| Criterion | Status | Evidence |
|---|---|---|
| A `407` is its own class, never retried, and round-trips | done | `a_proxy_refusing_its_own_credentials_is_an_authentication_failure` (both dialects), `a_connect_tunnel_needs_a_2xx_from_the_proxy`, `a_proxy_refusal_round_trips_and_is_never_retried` |
| The step layer tries once; the chain moves only to an explicit alternate | done | `a_proxy_refusing_its_credentials_is_not_retried` (the backing runs once; the class is `Auth`) |
| Its cause and hint name the proxy | done | the `model::tests` mapping, `what_is_refused_before_sending_is_not_reported_as_an_answer` (exit 13, "proxy variable"); the CLI reference lists the cause and `failed:proxy_auth` |
| Revert cycle | done | the adapter's `407` as an authentication failure, the step layer retrying it, setup's plain network class, the chain treating it as transient, the model mapping it to `Auth` — each fails its test (five mutations, one at a time; the model mapping had no test until its first cycle passed) |

### Self-review of `a257725` — findings fixed in the follow-up commit

The background review found a design flaw in the fallback chain, a misleading hint and record overstatements, all verified and fixed, each fix revert-cycled: (1, **medium**) the chain walked a proxy's refusal onto every configured alternate 200 ms apart — but proxy settings are the process's, per scheme, so each alternate behind the same proxy was sent the same refused credentials (five models, five `407`s in about a second: a common lockout threshold) and the router line said `auth_failure`; `llm-router` gains `FailureClass::ProxyAuth` and `StopReason::ProxyAuthFailure` (`proxy_auth_failure`), and the chain stops on it whatever alternates are configured (`a_proxy_refusal_stops_the_chain_even_with_an_alternate_configured`; the host's `a_proxy_refusal_stops_the_chain_before_any_alternate` runs a two-model chain: the primary runs once, the alternate never, one `Stop` decision); (2) every `407` was a proxy's refusal, and its hint named the proxy variables — but a `407` on a direct connection (an https endpoint or gateway, or a transparent proxy on plain http) comes from no proxy rapid read: the transport now judges it, since only it knows whether the request went through the configured proxy — a `407` answering a request sent through that proxy is `ProxyRefused` (a tunnel's refusal already was, where it is opened), and on a direct connection it reaches the adapter as the endpoint's own refusal (`Permanent`, as before `beca89e`; setup exit 15, `invalid`, with a hint naming the endpoint) (`a_407_is_the_proxys_refusal_only_when_the_request_went_through_the_proxy` — buffered and streamed, proxied and direct; `each_verification_failure_has_its_own_exit_code_and_changes_no_file` gains the direct `407` and asserts no failure there mentions a proxy); (3) `rapid setup --help` lists a proxy's refusal under exit 13; the setup test's comment on the transport guard sits above the assertion it describes. Record corrections for the b2b-1 section: an explicit alternate is another model behind the same proxy, not "another path"; "the public error code is unchanged" was wrong — for a `407` it moved from `provider.auth_failed` (since `beca89e`) to `internal.unexpected` (no production caller reads it); "migration impact: none reachable yet" was wrong — a `407` in setup moved from exit 11 (`auth`) to 13 (`network`), and a run's message and `--verbose` tag changed; "(both dialects)" cited a test only the chat dialect has (the second dialect's assertion is in `a_redirect_is_permanent_at_the_adapter_and_an_empty_body_is_not_a_message`), and that test's name contradicted what it asserted (it is renamed); "reports it as its own failure" — machine-readable, it is the network class and exit 13 like any network failure, as the spec places it; only the hint differs.

Migration impact: a proxy's refusal ends a turn without trying any alternate (not reachable until part b2b-2 lets runs use a proxy); a `407` on a direct connection is the endpoint's refusal again — retried as a rejection, as any other 4xx the endpoint answers, and setup exit 15 — instead of an authentication (`beca89e`) or proxy (`a257725`) failure naming variables rapid never read.

Revert cycle: the chain walking a proxy refusal onto an alternate (host and router tests), the buffered and the streamed transport not judging a proxied `407`, the chat and the second dialect's adapters calling a direct `407` a proxy's refusal (adapter and setup tests) — each fails its test (seven mutations, one at a time).

## SEAM-02-2 (part b2b-2) — The proxy opt-in, read the same way by runs and the probe

Contract restated: `apps/rapid/src/user_config.rs` — a `[network]` section with `proxy = "environment" | "none"` (default `none`), overridden by `RAPIDLM_PROXY`; `resolve_proxy(env, config)` reads the proxy variables (`ProxyConfig::from_env`) only under `environment`, and an unusable one is `InvalidValue` naming the key and the variable, never its value; `ActiveModel` gains `proxy`, resolved once in `resolve_active` and carried unchanged into the `[models] fallback` chain and the `[phases]` models. `apps/rapid/src/model.rs` — `ConfiguredModel::build`/`build_with_gate` install the resolved proxy on the transport (`routed`). `apps/rapid/src/setup.rs` — `probe_egress(plan, env)` reads the planned config's opt-in as a run would and lets the gate through to exactly the endpoint and the proxy a run would reach it through; the probe's model is resolved from the same planned document, so it dials the path a run dials. Migration impact: none for a user who changes nothing (S11) — the proxy variables are not read without the opt-in, so a shell that already exports `HTTPS_PROXY` dials directly as before; `network` is no longer reported as an unknown config key.

| Criterion | Status | Evidence |
|---|---|---|
| Opt-in only; `RAPIDLM_PROXY` over the file, both ways; opted in with no variable is direct | done | `the_proxy_is_opt_in_and_every_model_of_a_run_shares_it` |
| Every model of a run goes the same way (default, fallback chain, phase model) | done | `the_proxy_is_opt_in_and_every_model_of_a_run_shares_it` |
| An unusable setting is an error naming it, never the value; not read when not opted in | done | `an_unusable_proxy_setting_is_an_error_naming_it_never_its_value` |
| A run's model client dials through the proxy it resolved (absolute form to the proxy); not opted in, the proxy sees nothing | done | `a_runs_model_client_goes_through_the_proxy_it_resolved` (config → `resolve_active` → `ConfiguredModel::build`, no gate — a run's path) |
| The probe goes through the proxy a run would, with the proxy as the receipt's dialled hop; opted in by env or by the kept config; not opted in (or opted out over the file) the proxy is sent nothing; an unusable variable dials and writes nothing | done | `the_probe_goes_through_the_proxy_a_run_would_and_only_when_opted_in` |
| Revert cycle | done | the transport not given the proxy, the mode ignored (variables always read), the probe's gate without the proxy hop, the fallback chain dropping the proxy, `RAPIDLM_PROXY` ignored — each fails its test (five mutations, one at a time) |

Recorded decisions. **The opt-in's shape:** a config key with an environment override, the precedence every other model setting has (S11: env > user). No managed-policy field: the managed layer can already narrow endpoints, and a policy-forced proxy is a separate decision if an organisation asks for one. **An unusable variable under the opt-in:** `rapid setup` refuses before the probe with its existing "a run would refuse the configuration this writes" check (exit 1, the variable named, nothing dialled or written), since a run would fail the same way. **`/model select`:** it validates the id against a narrowed environment and does not build a client; the next turn resolves with the process environment, proxy included — unchanged here. The reference page gains the `[network]` table, and its heading and three sentences no longer name a peer product (rule 2.4). Remaining for SEAM-02-2: part b3, `rapid doctor --live`.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `pnpm generate:check`, `pnpm typecheck`, `pnpm test` (29 pass) green; `cargo test --workspace --locked --no-fail-fast` 4105 passed, 1 failed — `interactive::tests::computer_observe_reports_the_typed_platform_gate_not_a_stub`, which drives this host's real desktop and fails the same way without this change (recorded under HYG-001).

## SEAM-02-2 (part b3) — `rapid doctor --live`

Contract restated: `apps/rapid/src/p9_commands.rs` — `rapid doctor [--live]` (`parse_doctor_args`: `--live` once, anything else still a usage error); the help says what `--live` does. `apps/rapid/src/doctor.rs` — `DoctorEnv.live`; after every offline row, `check_live` adds one `live:<profile>` row per `[model.<id>]` (sorted), each resolved through `select_active_model_with_override` (the managed gates a run gets) and probed; `DoctorCheck` ids become `Cow<'static, str>` so a row can be named for its profile; each probed profile's resolved key joins the report's redaction set. `apps/rapid/src/setup.rs` — `endpoint_egress` (the gate for one endpoint and its proxy, now shared by the setup probe) and `probe_resolved` (the setup verification's bounded request for an already-resolved model: 16 output tokens, the run's client, the gate; a keyless refusal is `AuthNoKey`). Migration impact: none without `--live` (S11) — the offline rows, their order and the exit code are unchanged; `rapid doctor --live` was a usage error and now runs.

| Criterion | Status | Evidence |
|---|---|---|
| AC-07: one typed check per profile — `PASS` answered, `FAIL` with the class, the next command and the receipt | done | `live_probes_every_profile_once_and_reports_one_typed_row_each` (three profiles: answered, `401` → `auth`, refused connection → `network`; exit 1) |
| AC-07: offline `doctor` unchanged; the live rows only follow the offline rows | done | the same test (the row ids are `EXPECTED_CHECKS` then `live:down`, `live:good`, `live:refusing`; a run without `--live` has no `live` row and sends nothing), `the_default_command_makes_no_network_request_to_the_configured_provider` (unchanged) |
| The probe is bounded (≤16 output tokens), one request per profile, no key printed | done | the same test (`"max_tokens":16`, one request per server, no key in stdout/stderr) |
| No model configured: one `live` row pointing at `rapid setup`, not a failure | done | `live_with_no_model_configured_warns_and_points_at_setup` |
| The argument is parsed once; anything else is still refused | done | `doctor_rejects_any_argument_instead_of_silently_ignoring_it`, `doctor_help_is_truthful_about_being_offline_read_only_and_its_exit_codes` |
| Revert cycle | done | `--live` ignored, the bound not applied, every failure classed as network, `--live` accepted twice, only the first profile probed — each fails its test (five mutations, one at a time) |

Not tested: the managed-lock `SKIP` row (the lock itself is `select_active_model_with_override`'s, tested in `user_config`); the redaction of a probed profile's key (no live row prints one — it is a backstop).

Recorded decision — **the receipt is reported, not journaled (re-scoped implementation note, no acceptance criterion dropped):** the worklist line had `journal::record_egress` get its first producer here. `record_egress` needs a session and a prepared operation in the project ledger, and `rapid doctor` runs in no session; creating one per diagnosis would add a session to `rapid sessions list` for every `doctor --live`, and would make doctor, documented and tested as read-only, write. So `doctor --live`, like `rapid setup`'s probe, reports its egress receipt in its output and writes nothing. `record_egress` keeps no producer; the natural first one is a run's own provider dial inside its session, which no SEAMS task names — left for a decision if durable per-dial receipts for runs are wanted.

SEAM-02-2 is complete: part a (the probe, the classes), b1 (proxy transport and dial gate), b2a (the probe's egress lease and receipt), b2b-1 (a proxy's refusal), b2b-2 (the proxy opt-in), b3 (`doctor --live`).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `pnpm generate:check`, `pnpm typecheck`, `pnpm test` (29 pass) green; `cargo test --workspace --locked --no-fail-fast` 4106 passed, 2 failed — `help_is_printed_without_running_any_check_and_exits_zero` (the new help line said `PASS `, which that test forbids in help; reworded, the `doctor_cli` binary then 19/19) and the host-desktop test recorded under HYG-001.

### Self-review of `931eb06` — findings fixed in the follow-up commit

The background review found one wrong message, a false recorded rationale, three inaccurate doc statements and test gaps, all verified and fixed, each fix revert-cycled: (1, **medium**) `rapid setup` judged the file it writes with this shell's `RAPIDLM_PROXY` and proxy variables in force, so a shell with `RAPIDLM_PROXY=environment` and an unusable `https_proxy` (or `RAPIDLM_PROXY=on`) refused every setup with "a run would refuse the configuration this writes … network.proxy = \"environment\"" — a key the file did not have; the file is now judged with the shell's proxy settings set aside, as `RAPIDLM_MODEL` already was (`user_config::is_shell_proxy_setting`), a run in this shell failing is a note naming the shell settings in force ("with this shell's proxy settings a run would fail: …", or `RAPIDLM_MODEL`, as before), the verification says it cannot be verified and why (exit 2, no files changed), and the error names where the opt-in came from (`RAPIDLM_PROXY=environment` or `network.proxy = "environment"`) (`the_probe_goes_through_the_proxy_a_run_would_and_only_when_opted_in`); (2) the reference page said an unusable value is named "never its value" — true for proxy URLs, not for `no_proxy`, whose bad entry is quoted with anything before an `@` withheld; that `http_proxy` is read "lower case only" — not on Windows, where names ignore case; and it did not say an `http://` endpoint's request, key included, reaches the proxy — all three corrected; (3) tests: an `https` probe through a proxy is now covered — one `CONNECT` for exactly the planned endpoint, the key never reaching the proxy, the receipt naming the proxy (`an_https_probe_through_the_proxy_is_a_tunnel_to_the_planned_endpoint`); the run-path test covers the keyed transport too; the unresolvable test name is under `.invalid` (reserved never to resolve) rather than `.test`, so the "dialled directly" assertions do not rest on the local resolver.

Record corrections for the b2b-2 section: "the managed layer can already narrow endpoints" is false — `ManagedPolicy` narrows provider kinds (`allowed_providers`), not endpoints or `base_url`s, so the file and `RAPIDLM_PROXY` choose the proxy as they choose the endpoint; whether an organisation should be able to forbid or force a proxy is a **decision needed** (options: a managed `network.proxy` lock with the same field/origin/remediation reporting as the other managed gates — recommended if asked for; or leave it to the network). "Migration impact: none" was too strong — a `[network]` table was an ignored unknown key before and is now read: `proxy` with another value, or `network` not a table, fails the load, and a `proxy = "environment"` already in a file turns the proxy on; a user without a `[network]` table (the only kind a previous build wrote) is unaffected.

Revert cycle: the file judged with the shell's proxy settings, the opt-in's origin not named, the probe's gate choosing the other scheme's proxy, the keyed transport without the proxy — each fails its test (four mutations, one at a time).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4107 passed, 2 failed — `this_shells_model_override_is_a_note_and_the_allowlist_judges_the_written_profile` (the note had stopped naming `RAPIDLM_MODEL`; it now names the set-aside settings, and the `setup` tests pass 37/37) and the host-desktop test recorded under HYG-001; `pnpm` unaffected (no TypeScript or schema change).

### Self-review of `9de9024` — findings fixed in the follow-up commit

The background review found stale surfaces, a lock case probed wrongly, unbounded row ids, wrong remediations, report damage from redaction, a serial worst case and doc overstatements, all verified and fixed, each fix revert-cycled: (1) `rapid --help`'s subcommand row still said "diagnosis (offline)" with no operands, and the doctor module's guarantee still said "Offline" — both name `--live` now, and the usage constant has its doc comment back; (2, **medium-low**) under a managed `locked_default` every other profile was `SKIP` "locked", yet a run still dials those as `[models] fallback` and `[phases]` models — they are now probed as a fallback candidate (`apply_to_fallback_candidate`, the gates a run applies to one) with a note saying so, and `SKIP` only when that refuses them; a profile a managed field gate refuses is `SKIP` (a run does not use it), a policy that does not load is one `FAIL` naming the policy (`live_under_a_locked_default_probes_the_other_profiles_as_a_run_dials_them`); (3) row ids were rendered raw — a profile named with an escape sequence put it on the terminal, and a long one padded every row: ids are now control-free and at most 64 characters (`a_profile_name_cannot_put_control_characters_on_the_report`); (4) a keyless profile's `401` said "set the variable [model.p] env_key names" when it names none — it now says the profile names no key (or names the variable it does name); `rapid setup --profile` is suggested only for a profile id it accepts; a key the transport cannot send is its own `ModelConfigError::UnsendableKey`, so another credential error is no longer blamed on "a stray line break"; (5) every probed key was registered for redaction whatever its length, so a placeholder key (`x`, `EMPTY`) blanked that text out of every row, offline ones included, only under `--live` — keys under 8 bytes are not registered (no row prints a key), and the test now compares the offline rows word for word with and without `--live` (`live_probes_every_profile_once_and_reports_one_typed_row_each`, which also covers the quota and keyless-`401` classes); (6) the probes ran one after another (N unreachable profiles, N × up to a minute) and the "cancelled" branch was unreachable — they now run side by side and the dead branch is gone; (7) the reference said every `FAIL` carries a class and a receipt — a profile whose configuration does not resolve is `FAIL` with the reason and no receipt, now said; an IPv6 endpoint is named in brackets (`a_live_probe_names_an_ipv6_endpoint_in_brackets`).

Not changed: the probe uses the baseline reminder floor, as the offline `model` row does — a project whose reminders raise the effort sends a higher effort in a run than in the probe (recorded; the probe verifies the endpoint and key, not the effort). An IPv6-literal `base_url` is refused before dialling, by the probe and a run alike — outside this slice.

Revert cycle: every key registered, ids rendered raw, the old keyless remediation, the lock case skipped, the endpoint unbracketed — each fails its test (five mutations, one at a time).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4111 passed, 1 failed — `no_subcommand_summary_is_pushed_past_the_terminal_s_width` (the new `doctor` summary rendered 106 columns; shortened to "diagnosis (offline; --live probes models)", the test then passes); the host-desktop test passed this run; `pnpm` unaffected.

### Self-review of `1a9272c` + `de91406` — findings fixed in the follow-up commit

The background review found a false claim the lock fix introduced, and three smaller problems, all verified and fixed, each fix revert-cycled: (1, **medium-low**) under a lock, `de91406` probed every other profile "as a fallback or phase model" — including one a run never dials (not in `[models] fallback`, not the `[phases] compact` model: the only phase a run builds a model for), sending it a billable request and failing the command if it was down; its own test locked the false note in (`backup` was neither). A profile is now probed under a lock only in the role a run dials it in, named in the note ("probed as a [models] fallback", "as the [phases] compact model"), and `SKIP` "a run does not dial this profile" otherwise (`live_under_a_locked_default_probes_the_other_profiles_as_a_run_dials_them`: a fallback, a compact model and an undialled default — the last sent nothing); (2) a lock on a profile whose provider the policy refuses made every other profile's row blame its own provider — the gates judge the locked profile whatever is asked, so such a refusal is now treated as the lock's and each other profile is judged in its own role (the same test's second policy); (3) setup's note named "proxy settings" whenever a proxy variable was exported, even with no opt-in (they play no part then) — it names them only when they fail to resolve (`a_note_names_only_the_shell_settings_that_make_a_run_fail`); (4) under a lock, `1a9272c` stopped checking a run in this shell at all (the lock branch never resolved with the real environment), so `--no-verify` with an unusable proxy in this shell said nothing — the shell is now checked in both branches (the same test).

Not changed: the offline path registers the plan's keys for redaction at any length (a placeholder `x` on the default model blanks `x` in every row, with or without `--live`) — pre-existing, outside this slice. A panicked probe's row is named `live`, not the profile.

Revert cycle: every locked profile probed, the lock's refusal blamed on each profile, any proxy variable named, the lock branch not checking the shell — each fails its test (four mutations, one at a time).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4113 passed, 0 failed; `pnpm` unaffected.

### Self-review of `cccdf0b` — findings fixed in the follow-up commit

The background review found one defect the previous fix introduced and a wording slip, both verified and fixed, each fix revert-cycled: (1, **medium**) when the managed policy's own allowlist refuses the locked profile, `cccdf0b` treated the refusal as the lock's and probed the other profiles as fallback candidates — but a run is refused at selection (`select_active_model_with_override` fails before the fallback chain or the compact model is built), so `--live` billed a fallback no run ever reaches and could exit 0 while every run failed. Under a lock, a failed selection — refused by a gate, or a lock naming no profile — now makes every other row `SKIP` "every run is refused at the locked default <id>" (`live_under_a_locked_default_probes_the_other_profiles_as_a_run_dials_them`: a refused lock with an allowed fallback, and a lock naming a missing profile; nothing is sent); the lock naming a missing profile no longer blames each profile's own configuration; (2) setup's note named `RAPIDLM_MODEL` whenever it was set, also when it played no part (under a lock, which sets it aside, or when it names a good profile and only the proxy fails) — it is named only when a run fails with it and without the proxy settings (`a_note_names_only_the_shell_settings_that_make_a_run_fail`).

Revert cycle: the refused lock's gate failure, the missing locked profile, `RAPIDLM_MODEL` named unconditionally — each fails its test (three mutations, one at a time).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4112 passed, 1 failed — the host-desktop test recorded under HYG-001 (it passed on the previous run); `pnpm` unaffected.

Record correction for the `cccdf0b` section: "could exit 0 while every run failed" was wrong — when a locked selection fails, the offline `model` row already fails and the command exits 1; the harm was the billed probe to a profile no run reaches (review of `6843d8b`, which found no code defect).

## SEAM-02-3 — Persistence: atomic 0600 write, `.bak` only on change, `unchanged`; keychain-held keys

Contract restated: `apps/rapid/src/setup.rs` — `persist(plan, existing, stdin_key)`: after verification (or with `--no-verify`), the `--key-stdin` key goes into the OS keychain under the plan's alias (`rapidlm-model-<profile>`) unless the same key is already there; then, unless the planned document equals the file (`unchanged`: nothing touched, not even a backup), the previous file is copied to the plan's `config.toml.<UTC time>.bak` (0600, only on an update) and the new one replaces it through `exec_tools::atomic_write_with_mode` at 0600 (a missing `.rapidlm` directory is created 0700). A file changed since the plan read it is not overwritten. Any failure after the key was stored puts the keychain back as it was and removes the backup; the message says no files were changed (exit 1). The outcome reports `written`, `action` (`create`/`update`/`unchanged`), `path`, `backup` and `key` (`stored`/`unchanged`) in JSON, and a sentence in text; exit 0. `--key-stdin` on a terminal is refused with `--no-verify` too (the worklist's obligation from the `24d6c6f` review: a non-verifying run now reads the key to store it). `apps/rapid/src/user_config.rs` — `[model.<id>] keychain = "<alias>"` (validated as a keychain alias), `CredentialSource::Keychain(alias)` after the inline key and the variables, the value still `None`. `apps/rapid/src/model.rs` — `build_with_gate` reads a keychain key where the client is built, and nowhere earlier (invariant 11); a key it cannot read is a typed `ModelConfigError::Credential` naming the alias. `apps/rapid/src/provider_keychain.rs` (new) — read / store (idempotent, returning what it replaced) / restore over `auth::os_keychain_select::production_backend`, typed `KeychainError` (unavailable, not found, invalid alias, not text); tests install an in-memory keychain per thread and can never reach the real one. `apps/rapid/src/setup.rs` `verify` — a kept `keychain` key is read as a run reads it (`KeyNotRead { alias, reason }`, exit 11, nothing sent, when it cannot be). `crates/auth` — the macOS backend passed the secret on `security`'s command line (readable by any process through the process table) and the Windows backend interpolated it, unescaped, into a PowerShell script on the command line (a `'` in a key ended the string and ran the rest as script); both now pass it on stdin (`security -i` with the secret hex-encoded; `[Console]::In.ReadToEnd()`), and both live round trips store a key full of quotes and shell characters. Migration impact: a run that used to exit 2 with "writing the configuration is not in this build yet" now writes; `keychain` is no longer an unknown key; a profile naming one now sends that key (previously keyless).

| Criterion | Status | Evidence |
|---|---|---|
| AC-03: written atomically at 0600; `.bak` only when a previous file changes (byte-identical to it, 0600); a second identical run reports `unchanged` and writes nothing (same inode) | done | `a_setup_writes_atomically_at_0600_backs_up_only_a_change_and_is_idempotent` |
| AC-03: the key is stored in the keychain, the config names the alias, never the value; the same key again writes nothing | done | `a_key_from_stdin_is_kept_in_the_keychain_and_the_config_names_only_its_alias`, `a_keychain_alias_is_a_key_a_run_reads` |
| A run reads the keychain key when it builds the client, and sends it; no keychain / nothing stored are typed and name the alias | done | `a_keychain_key_is_read_when_the_client_is_built_and_sent`, `a_keychain_alias_is_a_credential_source_after_the_inline_key_and_the_variables` |
| Typed unavailability where no keychain exists (every platform, CI included) | done | `without_a_keychain_a_stdin_key_is_refused_typed_and_nothing_is_written` |
| A failed write leaves the keychain as it was and no file changed; a file edited meanwhile is not overwritten | done | `a_file_that_cannot_be_written_puts_the_keychain_back` (Unix: a read-only directory), `a_file_changed_since_the_plan_is_not_overwritten` |
| The probe reads a kept keychain key as a run does | done | `a_key_the_probe_would_not_send_is_named_instead_of_blamed_on_the_endpoint` (sent when stored; `KeyNotRead`, nothing sent, when not) |
| A key on argv is a usage error; `--key-stdin` on a terminal refused, `--no-verify` included | done | the existing key-flag parse tests; `a_key_on_a_terminal_is_refused_before_anything_is_read` |
| The keychain backends take the secret on stdin | done | `live_keychain_put_get_delete_round_trip` (macOS, run here), `credential_manager_round_trip` (Windows CI) — both with a key of quotes and shell characters |
| Revert cycle | done | `unchanged` rewritten, the mode dropped, the backup dropped, the keychain rewritten with the same key, no rollback, the build not reading the keychain, `--no-verify` exempt on a terminal, a file edited meanwhile overwritten — each fails its test (eight mutations, one at a time). Not mechanically testable: the secret no longer on `security`'s argv (the round trip passes either way) |

Recorded decisions. **Order:** verify → keychain → backup → file; the keychain goes first so a file never names an alias with no key behind it, and is put back if the file cannot be written. **`.bak` name:** the plan's `config.toml.<YYYYMMDDTHHMMSS.mmmZ>.bak` (UTC, milliseconds); an existing file of that name is not overwritten (the run fails, nothing changed). **Directory:** created 0700 when missing; an existing one is left as it is. Not changed: the file's owner and an existing directory's mode.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `pnpm generate:check`, `pnpm typecheck`, `pnpm test` (29 pass) green; `cargo test --workspace --locked --no-fail-fast` 4120 passed, 0 failed (the live macOS keychain round trip included).

### Self-review of `9e16ffa` — findings fixed in the follow-up commit

The background security review found five defects, all verified and fixed, each fix revert-cycled: (1, **medium**) `security -i` reads a command line into a 4096-byte buffer and runs the rest as another command, so a key over about 2 KB (hex doubles it) was stored cut short — a line that would not fit is now refused whole (`StoreError::BoundExceeded`, "the key is longer than the OS keychain takes"), and the stored key is untouched (`live_keychain_put_get_delete_round_trip` puts a 3000-byte key); (2, **medium**) the macOS put deleted the previous item before adding, and `store` returned a failed put before handing back what it replaced, so a failed store lost the old key with nothing to restore it — the put now updates in place (`add-generic-password -U`, no delete), and `store` itself puts back what was there when its write fails (`a_key_no_header_carries_is_not_stored_and_a_failed_store_puts_the_old_one_back`); `get` tells "nothing stored" (exit 44) from a keychain that refused (locked, no access), which is no longer reported as "no key under the alias"; (3, **medium**) the alias was the profile name alone, so two config files (or two endpoints) with a profile of the same name shared one key — setup for one overwrote the other's, whose runs then sent it to their own endpoint: the alias is now `rapidlm-model-<profile>-<12 hex digits of SHA-256 over the config path and the endpoint origin>`, set when the plan knows both (`a_custom_endpoint_needs_a_model_and_a_stdin_key_names_its_keychain_alias`); (4, **medium**) the `shell_exec` scrubber registered only a resolved `plaintext`, which a keychain profile does not have until the client is built — and the keychain item's trusted application is `/usr/bin/security`, so a command could read it back without a prompt; the scrubber now reads and registers a keychain key too (`a_keychain_key_is_scrubbed_from_tool_output_like_any_key`), as do `doctor`'s offline and live redaction sets; (5, **low**) a key with bytes a keychain gives back differently (a tab, anything non-ASCII) round-tripped as hex text — `store` accepts printable ASCII only (what an HTTP header carries), setup says so and writes nothing, and the previous value is kept as bytes, not dropped when it is not UTF-8 (same test). Also from the review: the Linux probe ran `secret-tool lookup --unlock`, but `--unlock` is a `search` option and `lookup` exits 1 on no match, so the probe could never report the Secret Service available — it now runs `secret-tool search --all` (exit 0 whenever the service answers). Not run here (no Secret Service on this host); the Linux live test skips where none runs. `doctor`'s credential-store remediation no longer says model keys only come from config/env.

Not changed, recorded: every model build reads the keychain afresh (a `security` process or two per build: each turn, subagent, compaction); `create_dir_all` sets 0700 on the last directory it creates only. Not mechanically testable: the secret off `security`'s command line.

Revert cycle: the line bound, `-U`, the not-found exit code, the key check, the restore on a failed put, the alias digest, the keychain key in the scrubber — each fails its test (seven mutations, one at a time; the macOS ones against the live keychain).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4121 passed, 1 failed — `shell_exec_runs_argv_inside_the_root_with_bounded_output`, whose freshly written script hit its 10 s timeout while this host's executable scanner was busy (a fresh two-line script took 6.4 s to start when run by hand); run again once fresh executables started promptly, it passed; `pnpm` unaffected.

## SEAM-02-4 — Per-effort model identifiers and per-model retry policy

Contract restated: `apps/rapid/src/user_config.rs` — `[model.<id>] effort_ids = { <effort> = "<model id>" }` (effort names `none`…`ultra`, ids validated as model ids) and `retry = { max_attempts, base_ms, max_ms, on }` (`RetryPolicy`, `RetryClass` rate_limit/server/network/rejected; auth, quota and proxy refusals refused in `on`; unknown keys refused; omitted keys keep the built-in values). `apps/rapid/src/model.rs` — the client sends the id `effort_ids` names for the effort it runs at (read when it is built, after the reminder and managed floors), else `model`, in both dialects; `ConfiguredModel` reports its policy. `apps/rapid/src/host.rs` — `LiveModelCall::retry_policy` (default none; delegated by the selected model, the fallback chain — its primary — and the supervision); `SupervisedModel` retries a failure only when its class is in the policy's `on` and attempts remain, waiting the base (`RAPIDLM_RETRY_BASE_MS` over `base_ms` over 1 s) doubled per retry or a longer retry-after, capped at `max_ms`, and stops when a provider asks for longer than `max_ms`; `RetryPolicy::builtin()` is today's policy exactly (six attempts, all four classes, 1 s doubling, no cap). `crates/agent-runtime` — `FailureCause::Transient` gains `rate_limited` so a 429 and a server failure are told apart (the text and tags are unchanged: both still read "transient"); the fallback chain now maps a 429 without retry-after to `RateLimited` instead of `Transient` (both are the same transient class there). Migration impact: none without the new keys (S11) — the built-in policy is today's; `effort_ids` and `retry` are no longer unknown keys.

| Criterion | Status | Evidence |
|---|---|---|
| AC-04: `effort_ids` selects the id in the outgoing request, per dialect | done | `the_request_carries_the_model_id_named_for_its_effort_in_both_dialects` (openai-compatible and the second dialect; high/low named, medium and none fall back to `model`) |
| AC-05: a per-model `retry` overrides the global ceiling and classes; retryable classes tested; non-retryable never retried | done | `a_models_retry_policy_sets_the_ceiling_the_classes_and_the_longest_wait` (two-attempt ceiling; only-network and only-rate-limit class sets; an auth failure retried zero times under an all-classes policy; a longer retry-after than `max_ms` ends the retries, a shorter one is honoured) |
| The built-in policy is today's | done | `the_builtin_retry_policy_is_todays`, the existing step-retry tests unchanged |
| The table is read and checked | done | `effort_ids_and_a_retry_table_are_read_and_checked` |
| Revert cycle | done | the effort id ignored, the model's policy ignored, a rate limit classed as a server failure, the longest wait ignored, a 429 mapped as not rate-limited — each fails its test (five mutations, one at a time) |

Not tested: the fallback chain delegating its primary's policy (a one-line delegation; the chain's alternates are the chain's decision). The status line for retries in exec output and the TUI named in the catalog is the existing `--verbose` per-attempt line (`attempt=<n> outcome=failed:<class>`), unchanged.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green (a private `ToolBatchOutcome` in `agent-runtime` grew past clippy's variant-size threshold with the new field; the lint is allowed there, with the reason); `cargo test --workspace --locked --no-fail-fast` 4125 passed, 1 failed — `shell_exec_runs_argv_inside_the_root_with_bounded_output` again (its freshly written scripts under the host's executable scanner and a full suite's load; alone it passed in 7.6 s); `pnpm` unaffected.

### Self-review of `7554c47` — findings fixed in the follow-up commit

The background review confirmed the line bound (probed live: 4095 bytes with the newline run as one command; at 4096 the newline is a second, empty command) and found three low-severity defects and one false record claim, fixed and revert-cycled: (1, **low-medium**) the alias digest hashed the config path as spelled — `RAPIDLM_CONFIG=.rapidlm/config.toml` in two projects gave both the same alias for one origin, and the same file reached through a directory link, a `..` or a relative path (or named by `9e16ffa`'s plain `rapidlm-model-<id>`) got a new alias on rerun, rewriting the file with a `.bak` and orphaning the stored key; the digest now covers the path made absolute and resolved through every link its existing part goes through, and a profile that already names a keychain alias for the same endpoint origin keeps it (`a_profiles_keychain_alias_is_stable_across_reruns_links_and_older_builds`: an older build's alias kept and the file `unchanged`, another origin a new alias, one file through a link and through `..` one alias); (2, nit) `BoundExceeded`'s `limit` was one too high for some accounts — it now counts the newline (not tested; the only caller reports "too long" without the number); (3, **low**) the record said a failed store puts the previous item back "byte for byte", but macOS gives a non-printable item back as hex, so restoring it would have written the hex — `store` now leaves any item it could not put back alone and says so (`KeychainError::Foreign`, `a_keychain_item_rapid_could_not_put_back_is_left_alone`). Recorded, not changed: the scrubber reads the keychain on every trusted turn (two `security` processes) and `doctor` reads every keychain profile's key, offline included; a locked keychain would prompt there — untested on this host.

Revert cycle: the kept alias, the path made stable, the foreign item left alone — each fails its test (three mutations, one at a time).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4128 passed, 0 failed; `pnpm` unaffected.

### Self-review of `adfca4e` — findings fixed in the follow-up commit

The background review confirmed the built-in path is today's (counts, waits, tags, cancellation, the empty-reply path) and that the new `rate_limited` field changes no decision, text or record, and found three defects, all verified and fixed, each fix revert-cycled: (1, **medium**) a model's `retry` table was not honoured inside a `[models] fallback` chain — the chain retried each model under its own fixed policy (two same-model retries, 200 ms base), so `max_attempts = 1` or `on = ["network"]` still got two retries of a 429 and `max_ms` did not bound a long retry-after; and after the chain stopped, the supervision ran the whole chain again under the primary's table, calling the alternate that had failed under another model's policy. Now a model with a table is retried by it inside the chain, before the chain decides anything, and the chain's same-model retries are passed over for it (moving to an alternate stays the chain's decision); a chain with any table reports a single-attempt policy to the supervision, so it is not run again; a chain with none is exactly as before (`in_a_chain_a_model_with_its_own_retry_table_is_retried_by_it`: one and four attempts by the table then the alternate; every model failing — the table's two, the alternate's own, once; no table — the chain's own retries); (2, **low**) an empty reply ignored the table (`max_attempts = 1` still retried it twice) — it is a `server` failure under the table, within its ceiling and the empty-reply cap (`an_empty_reply_is_retried_only_as_the_models_table_allows`); (3, **low**) records, prices and labels named `model`, not the id `effort_ids` sent — `ModelEntry::wire_model` is now what the eval provenance, the eval rate lookup, doctor's model row, `/model select`'s message and the status bar name (`the_wire_model_is_the_one_named_for_the_effort`; the `/models` rows list the configured `model`, as a catalogue). Record correction: the SEAM-02-4 section's "the step layer retries the chain under the primary's policy" described the defect in (1).

Revert cycle: the model's table not read in the chain, the chain's same-model retries not passed over, the chain reporting the primary's table, the empty reply outside the table — each fails its test (four mutations, one at a time).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4131 passed, 0 failed; `pnpm` unaffected.

### Self-review of `58e811c` + `5476e83` — findings fixed in the follow-up commit

The background review confirmed the chain's pass-over loop terminates, cancellation and the built-in path are unchanged, and `stable_path` is sound on Windows, and found four defects, all verified and fixed, revert-cycled: (1, **medium-low**) inside a chain with any `retry` table, an empty reply was never retried — the chain reports a one-attempt policy to the supervision, whose empty-reply rule then allowed none, and the chain's own loop retried only errors; the chain now retries an empty reply itself, under the model's table (a `server` failure within its ceiling and the two-retry cap) or, for a model without one, the built-in rule (`in_a_chain_with_a_table_an_empty_reply_is_retried_as_alone`); (2, **low**) `58e811c`'s "left alone" guard could not fire on macOS: `find-generic-password -w` prints non-printable data as hex digits, which are printable, so the hex text was kept as the previous value — `get` now reads `-g`, whose `password:` line marks hex output (`0x…`) apart from printable data in quotes, and returns the bytes as stored (`live_keychain_put_get_delete_round_trip` stores raw bytes, a key of hex digits and a key full of quotes and shell characters; `a_password_line_decodes_to_the_bytes_stored`); (3, **low**) keeping a profile's existing alias let a config copied from another file keep that file's alias, so a new key overwrote the other file's (and a hand-edited alias could send a key to another profile's host) — the reuse is gone: every run names this file's own alias, and the stable path already keeps it the same across reruns and spellings (`a_profiles_keychain_alias_is_stable_across_reruns_and_links_and_never_borrowed`); (4, **low**) eval's provenance and rate lookup resolved the model without the managed gates, so a managed effort floor could make them name `model` while the run sent the floor's `effort_ids` id — they use the gated selection now (not tested: eval's process-environment resolution).

Revert cycle: the chain's empty-reply retry, `-g` in place of `-w`, the path made stable — each fails its test (three mutations, one at a time; the second against the live keychain).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4132 passed, 1 failed — `goal_host::tests::a_driver_lease_becomes_available_again_once_dropped`, in untouched `goal_host.rs`, which passed three runs out of three alone (a flake under the full suite, not accommodated); `pnpm` unaffected.

## SEAM-02-5 (part a) — Continuation on length-truncated output

Contract restated: `apps/rapid/src/user_config.rs` — `[model.<id>] continue_on_length = N` (0..=8, default 0). `apps/rapid/src/model.rs` — `ConfiguredModel::step` reads each response's finish reason (`request_once`); while a text answer ended for `Length`, fewer than `N` continuations were made, and the step's tokens are under the turn's remaining token allowance, it sends a continuation (`build_continuation`: the step's request without tools, then the answer so far as the assistant's message and a fixed instruction to continue without repeating), and stitches the parts into one `Terminal` (tokens summed, cost summed where reported); a continuation that proposes tool calls fails the step. It records each request's tokens, the first first, for `take_continuations`. `crates/llm-router` — `CanonicalModelRequest::with_messages` (same bound as `new`). `crates/agent-runtime` — `ModelStepInput::token_allowance` (the budget's `max_tokens` less what the turn used), `ModelDriver::take_continuations` (default none), `TurnEvent::ModelContinued { continuation_of, index, tokens }` emitted once per request of a continued step, before its `model.completed` (whose tokens are their sum, counted against the budget once, as a step's always are). `crates/event-ledger` — `EventKind::ModelContinued` (`model.continued`, additive, count pin 111 → 112 in the SDK catalog); the interactive ledger sink records it with `continuation_of` and `index` added to the payload of that kind only. `apps/rapid/src/host.rs` — `LiveModelCall::take_continuations`, delegated by the context driver, the supervision, the fallback chain and the selected model. `apps/rapid/src/interactive.rs` — `rapid exec` prints one stderr line per continued step (`continuation_notes`). Migration impact: none with the default `0` (S11) — one request per step, no new event, the payloads of every existing event unchanged.

| Criterion | Status | Evidence |
|---|---|---|
| AC-06: a stubbed provider returning length twice then stop yields one stitched message | done | `a_length_cut_answer_is_carried_forward_into_one_message` (three requests to a loopback server; the second and third carry the answer so far and the instruction, and no tools) |
| AC-06: three ledger events with `continuation_of`, and three budget decrements | done | `a_continued_answer_is_one_record_per_request_before_its_completion` (three `model.continued` of the step's request, indexes 0–2 with each request's tokens, before `model.completed`; the turn's usage is their sum; the budget's allowance reaches the driver) |
| AC-06: `= 0` preserves today's behaviour exactly | done | `without_continue_on_length_a_cut_answer_ends_where_it_was_cut` (one request, the cut text), the same test's plain step (no `model.continued`) |
| The count and the budget bound the continuations | done | `a_continuation_stops_at_its_count_and_at_the_turns_token_budget` |
| Continuation status in exec output | done | `a_continued_answer_is_said_once_per_step_with_its_count` |
| Revert cycle | done | the finish reason not checked, the parts not stitched, the budget ignored, tools offered to a continuation, the records not emitted, the allowance not passed — each fails its test (six mutations, one at a time) |

Recorded decisions. **Where it runs:** at the model client, where the finish reason is; the turn loop records it (events and budget live there) through `take_continuations`, so no side channel holds the only record (S1). **Budget:** a continuation is sent only while the step has used less than the turn's remaining token allowance; the records' tokens sum to the step's, which the budget counts. **Failure:** a continuation that fails fails the step (the supervision may retry the step from the start); a partial answer is not returned as if complete. Remaining for SEAM-02-5: part b — the TUI's one-line marker for a continued answer, and `reasoning_summary` for the dialects that support it.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `pnpm generate:check`, `pnpm typecheck`, `pnpm test` (29 pass) green; `cargo test --workspace --locked --no-fail-fast` 4138 passed, 0 failed.

### Self-review of `cb8d7b8` — finding fixed in the follow-up commit

The background review confirmed the `-g` decoding (the `password:` line is on stderr, attribute lines are indented; hex output is marked), the eval selection and the setup change, and found one defect, verified and fixed, revert-cycled: (**medium-low**) an empty reply the chain retried itself was dropped from every total — its tokens and cost never reached the step's result, the supervision's counter, the goal budget, `--usage-file`, the JSONL cost or the chain's per-model spend, although the provider billed it; the chain now carries each discarded empty reply's tokens and cost into the step's result and books its cost against its model at once (`in_a_chain_with_a_table_an_empty_reply_is_retried_as_alone` asserts the turn's tokens and cost include both empty replies). Observation, not changed: a config naming an alias from an earlier commit on this branch is rewritten to this file's alias on rerun (with a backup), and the old keychain item stays — no release contains `--key-stdin` setup.

Revert cycle: the discarded tokens not carried — fails its test.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4138 passed, 0 failed.

### Self-review of `80db2f3` — findings fixed in the follow-up commit

The background review confirmed the default `0` changes nothing (no record, no payload change; the kernel's recovery, the session projection and ACP ignore the new kind harmlessly; JSONL passes it through as its own type) and that streamed continuation text equals the stitched text, and found three defects, all verified and fixed, revert-cycled: (1, **high**) a continuation dropped the step's tools but kept its history, and the second dialect refuses `tool_use`/`tool_result` blocks with no tool definitions — every continued final answer after a tool exchange failed with a rejection there (the turn failed, or the chain moved on) where `0` would have delivered the cut answer; a continuation now carries the step's tools, and a reply that proposes calls leaves the answer so far (its request on record) instead of failing the step (`a_length_cut_answer_is_carried_forward_into_one_message` — the tools on both requests; `a_continuation_that_proposes_calls_leaves_the_answer_so_far`); (2, **medium**) the stitched answer could pass what one message holds (64 KiB), which the turn reads as a context overflow (compacting and re-running the turn, spending the requests again) or a failed step — a continuation is now sent only while the answer can still grow by a part within that bound (a part taken as `max_tokens` × 4 bytes), and a part that would overflow it anyway is not stitched: the answer so far stands and both requests count (`a_continuation_never_takes_an_answer_past_what_one_message_holds`); (3, **low-medium**) records went missing or stale: a step that failed after continuing left its requests unrecorded — the model keeps them as it makes them and the loop records them before `model.failed` (`a_step_that_fails_after_continuing_leaves_its_requests_on_record`); a compaction summary's continuation records were left for the next step to report — compaction discards them; the fallback chain drained every model's records — it takes only the model it last ran (not tested). Record correction for the part a section: "tools are not offered to a continuation" and "a continuation that proposes tool calls fails the step" no longer hold.

Revert cycle: the tools dropped, the room check, the overflow check, a tool-call reply failing the step, the failure-path records — each fails its test (five mutations, one at a time).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4141 passed, 0 failed; `pnpm` unaffected.

## SEAM-02-5 (part b) — The transcript marker; `reasoning_summary`

Contract restated: `crates/tui/src/state.rs` — `TranscriptEntry::Continued { continuations }`, folded from `model.continued`: the records of one step are contiguous, so the first record past the step's first request adds the line and each later one raises its count (the step's first request alone adds none); `crates/tui/src/transcript.rs` renders it as a system line — "(the answer below reached the model's output limit and was continued N time(s); it is one message)" — before the answer (S3's one-line transcript marker). Also in this commit: a continuation that replied with tool calls left the answer so far with that request on record but not in the answer's tokens and cost — it is now in both (found while writing the review brief; `a_continuation_that_proposes_calls_leaves_the_answer_so_far` asserts the tokens). Migration impact: none (a new entry for a new record).

| Criterion | Status | Evidence |
|---|---|---|
| Continuation status appears in the TUI | done | `a_continued_answer_leaves_one_marker_line_before_it` (three records of one step: one line, count 2, rendered) |
| Revert cycle | done | the marker not added — fails its test |

**Decision needed — `reasoning_summary`.** The catalog asks for a `reasoning_summary` setting "for dialects that support it". None that a user can configure does: the chat-completions style `[model.<id>]` always uses has no summary field (only `reasoning_effort`); the second dialect has no summary option; the Responses style exists in the router but is not selectable from configuration, sends no `reasoning` object, and the adapter does not surface reasoning output anywhere a user would read it. A key that is accepted and does nothing would be a false surface (S9). Options: (a) add `[model.<id>] api = "responses"` for openai-compatible endpoints, encode `reasoning = { effort, summary }` there, and project the returned summary into the transcript as its own marked entry — a real feature, several slices; (b) drop `reasoning_summary` from SEAM-02 by an ADR, leaving `reasoning_effort` as the reasoning control; (c) accept the key and warn that no configured dialect carries it — not recommended. Recommendation: (b) now, (a) as its own item if a Responses-only model is wanted. Nothing was built for it.

SEAM-02-5 is complete for AC-06 (parts a and b); `reasoning_summary` waits on the decision above.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4142 passed, 0 failed; `pnpm` unaffected.

### Self-review of `750b00b` + `05af3c1` — findings fixed in the follow-up commit

The background review confirmed no double counting of a chain's discarded empty replies, that `finish` after the overflow break does not leak, that a cancelled continuation's earlier requests are recorded, and that records never go stale across steps, and found five defects, all verified and fixed, revert-cycled: (1, **medium**) a continuation that replied with tool calls was on record but not in the answer's tokens or cost — fixed in `f232d2d` (found while writing this review's brief); (2, **medium-low**) the supervision retrying a step whose continuation failed wiped the first attempt's records (each step began by clearing them), and a failed step's records counted against no budget — records now accumulate across attempts of one step until the turn loop takes them (it takes after every step, failed or not; compaction discards its own; the chain takes from every model again, now that nothing older than the step can be left), and the loop adds a failed step's records' tokens to the turn's usage (`records_of_a_failed_attempt_survive_its_retry_and_a_large_max_tokens_still_continues`, `a_step_that_fails_after_continuing_leaves_its_requests_on_record`); (3, **low-medium**) a chain step that failed after discarding empty replies lost their tokens — the chain carries them to its next step that answers (`a_chains_discarded_empty_replies_count_even_when_their_step_fails`); (4, **low**) the room check took the next part as `max_tokens` × 4 bytes, so with `max_tokens` ≥ 16384 (in practice above ~8192) nothing ever continued, silently — the next part is judged by the one just received, which a cut part's own length bounds (same test); (5, **low**) the exec note said "continued 0 time(s)" for a failed step, and "it is one message" after a part was dropped — it now speaks only for a completed step, and says how many follow-on requests were made (`a_continued_answer_is_said_once_per_step_with_its_count`).

Revert cycle: records cleared per attempt, the old room estimate, the chain's carried usage not kept, a failed step's records not counted, a one-record step noted — each fails its test (five mutations, one at a time).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4143 passed, 1 failed — `a_continuation_never_takes_an_answer_past_what_one_message_holds`, whose sizes encoded the old room estimate (its second case could no longer reach the overflow branch); resized (a 20 KiB part, then a 50 KiB one) it passes and fails under the overflow mutation, and the `rapid` library tests pass 1040/1040; `pnpm` unaffected.

## SEAM-02-6 — Model-configuration and getting-started documentation

Contract restated: `docs/reference/model-configuration.md` — a "Setting it up" section (`rapid setup` with a preset, a variable or the keychain, a local endpoint, `--dry-run`; `rapid doctor --live`), `[models] fallback` and `locked_default`, a `[phases]` section (only `compact` builds a model), every `[model.<id>]` key (the four missing rows `reasoning_effort`, `vision`, `caching`, `reasoning` added beside `keychain`, `effort_ids`, `retry`, `continue_on_length`), the `retry` and `[network]` sections, neutral examples (a keyless local server; a gateway with a fallback, a compact model, a retry table and continuations), and corrected claims: an unconfigured run exits `5` (it said `1`); configuration errors exit `2` and provider failures `4` (it said every failure exits `1`); a keyless profile sends an empty bearer token (it said no token); `rapid exec` is no longer "one turn with no tools"; `base_url` takes `https://` (it said plain HTTP). The page's heading and prose name no peer product (rule 2.4, since `931eb06`) and no provider or model outside the dialect values the config takes (rule 2.2). `docs/getting-started.md` — the first run leads from `rapid doctor` (which exits `0` on a fresh machine with a warning — it said `1`) to `rapid setup` and `rapid doctor --live`, with the hand-written file as the alternative; its provider-named example is a neutral local server. `apps/rapid/src/user_config.rs` — `MODEL_ENTRY_KEYS`, the one list the parser checks keys against.

| Criterion | Status | Evidence |
|---|---|---|
| AC-08: the reference page and getting started describe the delivered behaviour | done | the pages; `the_reference_page_documents_exactly_the_keys_a_model_table_takes` pins the documented `[model.<id>]` keys to `MODEL_ENTRY_KEYS`; `getting_started_lists_exactly_the_exit_codes` (unchanged) pins the exit table to `JsonlExitCode::ALL` |
| AC-08: the heading names no peer product | done | since `931eb06` |
| Revert cycle | done | a documented row removed fails the pin; a key added to the parser's list without the page does not compile until the list's length changes, and then fails the pin |

SEAM-02 is complete: all six tasks, AC-01 … AC-08 evidenced (`reasoning_summary` waits on the decision recorded under SEAM-02-5 part b).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4144 passed, 1 failed — `shell_exec_runs_argv_inside_the_root_with_bounded_output`, the host-scanner timing test (alone: passed in 0.8 s; a separate task hardens it); `pnpm` unaffected.

### Self-review of `f232d2d` + `2da8bbb` — findings fixed in the follow-up commit

The background review confirmed the new transcript entry is never persisted (resume rebuilds through the reducer), two steps' markers cannot merge, nothing leaks across turns or sessions, and the default `0` changes no outcome, and found three defects, verified and fixed, revert-cycled: (1, **medium**) records of an attempt that failed were carried into the retried attempt that answered — the turn loop then showed "continued" over an answer that was never continued, and their billed tokens reached no total (the success path counts the output's tokens, which did not include them); the same after the chain moved from a model that had continued to one that had not. Now a failed attempt's billed requests count in the answer's tokens and are not records of it — at the model (a retried attempt adds them to its output) and in the chain (moving on from a model folds what it left into the answer's carried usage) — so the records describe only how the answer given was carried forward (`a_failed_attempts_billed_request_counts_in_its_retry_and_a_large_max_tokens_still_continues`, `a_model_the_chain_moves_on_from_leaves_its_billed_requests_in_the_answer`); (2, **low**) a continuation request that could not be built dropped the earlier and current records — they are set before it is built (not tested: the build fails only past the message bound); (3, **low**, cosmetic) the marker promised "the answer below" and "one message" even over a step that then failed — it now reads "(the model's answer reached its output limit; N follow-on request(s) continued it)". Record correction for the `750b00b` + `05af3c1` section: "records accumulate across attempts of a step until the turn loop takes them" is replaced by the above.

Revert cycle: the earlier attempt's tokens not counted, its records kept as this answer's, the chain not folding what a model left — each fails its test (three mutations, one at a time).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4146 passed, 0 failed; `pnpm` unaffected.

### Self-review of `1acc443` + `5f560a8` — findings fixed in the follow-up commit

The background review confirmed the two pages' other claims against the code (setup's flags, exit codes and write rules; exec's exit codes; doctor's fresh-machine exit; the fallback limit, the lock's precedence, the retry and continuation descriptions; the pinned key list; no peer product named) and found five problems, all verified and fixed, revert-cycled where they are code: (1, **medium**) an empty answer after a continuation (a reasoning model spending its cap) counted its requests twice when the supervision retried the empty reply: its tokens were counted with that answer, and the retry then added them again as an earlier attempt's — a model now knows whether its records belong to an answer (counted) or to a failed attempt (not yet counted), and adds only the latter (`an_answered_attempts_records_are_not_counted_again_by_its_retry`); (2, **medium**) when the chain moved on from a model and the next one failed, what the first left moved into the chain's carried usage and was dropped with the chain (it lives for its turn) — `ModelDriver::take_uncounted_tokens` (default none) gives the turn loop the chain's carried usage when a step fails, and it counts it (`a_model_the_chain_moves_on_from_leaves_its_billed_requests_in_the_answer`, `a_step_that_fails_after_continuing_leaves_its_requests_on_record`); (3, **medium**, docs) the `[phases]` row said in-turn context recovery uses the `compact` model — only `/compact` does; corrected; (4, **low**, docs) the credential paragraph still said keyless configs send no bearer token — they send an empty one, as the constraints section says; corrected; (5, **low**) the tests this program added to `host.rs` named providers in their model references (rule 2.2) — they use neutral ones now.

Revert cycle: an answered attempt's records counted again, the chain's carried usage not given up, the turn loop not counting it — each fails its test (three mutations, one at a time).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4147 passed, 0 failed; `pnpm` unaffected.

## SEAM-03-1 (part a) — One job registry per session for the daemon and `rapid acp`

Contract restated: `apps/rapid/src/exec_tools.rs` — `SessionJobs`, a host-wide map from session to its `JobRegistry` (`for_session` creates on first use and hands out the same table after). `apps/rapid/src/daemon_serve.rs` — one `SessionJobs` for the daemon's life, shared by every connection; `turns.submit` and an approval's continuation run with the session's registry. `apps/rapid/src/acp_serve.rs` — one for the serve's life; a prompt's turn and the continuation its permission answer resumes run with the session's registry (through `PromptRoutes`). `apps/rapid/src/interactive.rs` — `spawn_acp_turn` and `acp_resolve_and_continue` take the registry instead of making a fresh one per turn. Migration impact (disclosed, as the worklist states): in the daemon and `rapid acp` a background job now outlives the turn that started it — as it does in the TUI — and a later turn's `job_status` finds it; it stops when the host process exits (there is no session-close call to end it sooner).

| Criterion | Status | Evidence |
|---|---|---|
| A session's jobs survive its turns and a client's reconnect; sessions do not share them | done | `a_long_lived_hosts_session_keeps_one_job_table_across_turns_and_connections` |
| Revert cycle | done | a fresh registry per call — fails its test |

Not tested: the daemon's and the ACP serve's handing of the session's registry to their turn threads (a turn that starts a job needs a model driving tool calls end to end; the wiring is one argument at each of four call sites). Remaining for SEAM-03-1: a detached `task_spawn`'s `finished`, `job.orphan_reconciled` at session open, `job.*` in ACP `session/update`, `job.*` in `rapid exec` JSONL — and AC-02's end-to-end reconnect test.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4148 passed, 0 failed; `pnpm` unaffected.

### Self-review of `a5fbacf` — findings fixed in the follow-up commit

The background review traced every billing path (plain success, a model error then the supervision's retry, an empty reply then its retry, a chain with and without tables, a chain error with no retry) and found them counted once, and found two defects, fixed: (1, **low**) an empty reply the chain discarded kept its continuation records in the model, so a step then cancelled (during the empty-reply wait, or before the retried attempt cleared them) had them counted twice — once in the carried usage, once as records — and emitted `model.continued` for an answer thrown away; the chain now takes a discarded reply's records along with its tokens (`a_discarded_empty_reply_takes_its_continuation_records_with_it`; revert-cycled); (2, **low**) the record said the tests this program added to `host.rs` use neutral model references, but one added on 2026-09-25 (`a_proxy_refusal_stops_the_chain_before_any_alternate`) still named providers — it is neutral now (the remaining named references predate the program). Observation, not changed: `ExecOutcome.tokens`, the "tokens used" figure, is the supervision's counter of answered steps and never included a failed step's billed tokens; the turn's budget (`state.usage`) does now. Aligning the two is a separate change.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4149 passed, 0 failed; `pnpm` unaffected.

## SEAM-03-1 (part b) — A detached subagent's job ends in the job record

Contract restated: `apps/rapid/src/exec_tools.rs` — the detached `task_spawn` worker, which recorded its job's start through the turn's `JobEvents` sink (`register_detached`) but only ever set the in-memory state at its end, now reports the end through the same sink — `completed` (exit 0), `cancelled`, or `failed` — after spooling the report and setting the state, so the `job.*` rows (`/jobs`, a reconnecting client) reach a terminal state as a shell job's do. Migration impact: a `job.completed` record now follows every detached child's `job.started`.

| Criterion | Status | Evidence |
|---|---|---|
| A detached child's row reaches a terminal state | done | `a_detached_spawns_job_reaches_a_terminal_state_in_the_job_record` (started then finished for the same job, `completed` exit 0) |
| Revert cycle | done | the end not reported — fails its test |

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4148 passed, 2 failed — `shell_exec_runs_argv_inside_the_root_with_bounded_output` and `computer_observe_reports_the_typed_platform_gate_not_a_stub`, both host-dependent; both fail the same way with this change set aside, while a freshly written two-line script took 55 s to start on this host (its executable scanner); `pnpm` unaffected.

## SEAM-03-1 (part c) — A dead host's jobs are reconciled when a session is next opened

Contract restated: `apps/rapid/src/exec_tools.rs` — a host shell job is now spawned in `start` itself (under the child lock, so kill-all sees it the moment it exists) in a process group of its own (`process_signal::isolate_process_group`, as sandboxed jobs already were), and `JobEvents::started` receives its identity, `JobProcess { pid, process_group, started_unix_ms, host_pid }` (Unix; `None` for a detached subagent, a sandboxed job, or a spawn that failed — which now records `job.started` then `job.completed` `failed` at once, where the worker used to). Because a terminal's Ctrl-C or hang-up no longer reaches a job's group, every way this host stops a job now stops the whole group: `/jobs cancel` and kill-all (`kill_job_tree`, a group `KILL`), and the worker's cancel and timeout (`terminate_process_group_default`, `TERM`, grace, `KILL`) — before, each killed the job's own child only, and what it had started kept running. `apps/rapid/src/interactive.rs` — `job.started` carries `host_pid` (this process) and, when the job has one, `process`. `apps/rapid/src/job_recovery.rs` (new) — `open_jobs` (the session's `job.started` rows without `job.completed` or `job.orphan_reconciled`); `reconcile` (a job whose recorded host is not running and is not this process is judged: no process of its own → `lost`; otherwise `process_supervisor::reconcile_orphans` with the host probe and killer, which signals only when pid, group and start time all match → `terminated`, `exited`, or `blocked:<why>` with nothing signalled); `reconcile_session` (Unix only; one `job.orphan_reconciled` record per judged job, `{job_id, state: "orphan_reconciled", outcome}`, which the TUI already projects); `open_session_jobs` (a multi-session host's registry, reconciled on its first use in that host). Callers: the TUI on `--resume`; the daemon's `turns.submit` and approval continuation; `rapid acp`'s prompt. Migration impact: `job.started` gains two fields; a stopped job's descendants are now stopped with it; a job no longer dies with its host on a terminal Ctrl-C/hang-up of `rapid exec` or the TUI — until a later host opens the session, when it is stopped and recorded.

| Criterion | Status | Evidence |
|---|---|---|
| `job.started` names its host and its process | done | `a_background_jobs_start_record_names_its_host_and_its_process` (the live host's job is left running by a reconcile) |
| A job whose host died is found and stopped, and recorded once | done | `a_dead_hosts_running_job_is_found_and_stopped` (a real process in its own group, unit level); `opening_a_session_a_dead_host_left_stops_its_job_and_says_so` (through the kernel: `terminated`, one record, projected as `OrphanReconciled`, a second open finds nothing) |
| Only a dead host's jobs are judged; only a proven process is signalled | done | `only_a_dead_hosts_jobs_are_judged_and_only_a_proven_process_is_stopped`; `open_jobs_are_the_started_ones_without_a_terminal_record` |
| A multi-session host reconciles a session once, on its first use | done | `a_multi_session_host_reconciles_a_session_once_on_its_first_use` (first use reconciles; a later use does not; a new host does) |
| Stopping a job stops what it started | done | `stopping_a_background_job_stops_what_it_started_too` (cancelled, timed out, dropped with the registry — the grandchild of `sh -c 'sleep 30 & …'` is gone each time) |
| Revert cycle | done | host liveness ignored; the killer bypassed; the process identity not recorded; the record not appended; reconciliation on every use; on no use; `kill_job_tree` killing the child only; the timeout killing the child only — each fails its test (eight mutations, one at a time). Survives: the worker's cancel path killing the child only — `/jobs cancel` and kill-all have already killed the group when the worker sees the flag; the worker's own path only matters when their `try_lock` loses to the worker's poll, which no test can arrange |

Not tested: the TUI's `--resume` call (one line, `reconcile_session`, in `run_started_session`, which needs a terminal). Limits, disclosed: reconciliation is Unix only (elsewhere a host's liveness is not read, and nothing is judged); a job recorded before this change has no `host_pid` and is never judged; a dead host's pid reused by a live process makes its jobs look owned — they are left alone (the safe side); the start time is stamped when `spawn` returns, and the probe accepts a stamp up to 5 s after the kernel's (`MAX_SPAWN_RECORD_SKEW_MS`), so a spawn slower than that (this host's executable scanner has taken longer) leaves the job `blocked`, nothing signalled. A test's own child is not a real orphan: it stays a zombie until its parent reaps it, and macOS refuses a group signal to a group of only zombies (`EPERM`); a real orphan's parent is init, which reaps at once, so the tests reap concurrently the same way.

Review finding on `295f864`, handled here: the daemon has no `SIGTERM` handling, so a daemon stopped that way left its sessions' jobs running with no timeout. They are now stopped and recorded when a host next opens the session; nothing stops them sooner. Also disclosed: `SessionJobs` keeps a registry for every session a host has served for the host's life (a session's finished jobs hold only their spooled output). Record correction for part a: "it stops when the host process exits" is replaced by the above — a job outlived its host whenever the host was killed or crashed, and still does until a later host reconciles it.

Remaining for SEAM-03-1: `job.*` in ACP `session/update`, `job.*` in `rapid exec` JSONL, and AC-02's end-to-end reconnect test.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4156 passed, 1 failed — `computer_observe_reports_the_typed_platform_gate_not_a_stub`, which drives this host's real desktop and fails the same way without this change (part b); `pnpm` unaffected.

### Self-review of `c359239` — findings fixed in the follow-up commit

The background review confirmed several properties: a group kill cannot reach a reused pid, since it lands under the child lock before the worker reaps; the locking cannot deadlock, including kill-all during `Drop`; event order and budget accounting are unchanged; a signal needs pid, group and start time to match, rechecked before the signal; `open_jobs` handles re-started ids and records without an id; appending outside a turn is accepted; the TUI's `--resume` sees the records live; concurrent first uses in the daemon are safe. It found six defects, all verified and fixed:

1. **Medium.** Reconciliation read the session through the bounded export, which refuses a session past 10 000 events. A long-lived session — the kind that runs background jobs — was never reconciled, and nothing said so. It now replays the session from its first event (`job_recovery::job_events`, resuming after a lag, at most 64 passes). A read that does not reach the tip judges nothing, since a missed `job.completed` would make a finished job look open. Test: `a_session_past_the_export_bound_is_still_read_to_its_end`.
2. **Low–medium.** `process_supervisor`'s host killer returned as soon as the group leader died of `TERM`, so a member that ignored `TERM` outlived a job recorded `terminated`. It now sends `KILL` to the group unconditionally, as `process_signal::terminate_process_group` does. The group id cannot name anyone else while a member lives, and an empty group answers `ESRCH`. Test: `a_member_that_ignores_term_is_killed_after_its_leader_goes`.
3. **Low.** A job whose supervisor thread could not start was retracted from the table, but its child — spawned by then — was left running, unreachable, and never reaped. Its group is now stopped and its record ended `failed`. Not tested: a thread-spawn failure cannot be arranged.
4. **Low.** `JobEvents` lost its doc comment to the inserted `JobProcess`. It is restored, and now says `started` runs on the spawning call.
5. **Low.** A failure after signalling was recorded as `blocked:<why>`, which promises nothing was signalled. It is now `failed:<why>`. Test: a case in `only_a_dead_hosts_jobs_are_judged_and_only_a_proven_process_is_stopped`.
6. **Low.** The 400 ms timeout case of `stopping_a_background_job_stops_what_it_started_too` could lose the race on a loaded host. It is now 3 s, and the pre-check that the process is alive is gone.

Also from the review: `rapid exec --resume` / `--continue` now reconcile the session before the turn is submitted, after those records (`a_resumed_exec_reconciles_the_session_before_its_turn`). A `rapid exec` stopped by Ctrl-C therefore has its jobs stopped the next time any host continues its session, not only a TUI `--resume`.

Disclosed, not changed: a job now runs in a background process group of the terminal's session, so one that reads the terminal itself (a password prompt opened on `/dev/tty`) is stopped by `SIGTTIN` until its timeout. Before, it competed with the TUI for the same keystrokes. Its stdin was already empty.

Record corrections for part c:
- Callers now include `rapid exec --resume/--continue`.
- "`blocked:<why>` with nothing signalled" holds as stated; `failed:<why>` is added.
- "A session too long to export … leaves the rows as they were" no longer applies.

Revert cycle: reading through the export; the killer's early return after the leader goes; a post-signal error recorded as `blocked`; the exec resume not reconciling. Each fails its test (four mutations, one at a time).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4158 passed, 2 failed — `shell_exec_runs_argv_inside_the_root_with_bounded_output` (the host-scanner timing test; alone: passed in 0.9 s) and `computer_observe_reports_the_typed_platform_gate_not_a_stub` (drives this host's desktop; fails the same way without this change); `pnpm` unaffected.

### Self-review of `3e24ba1` — findings fixed in the follow-up commit

The background review confirmed several properties:
- The replay cannot block: the tip is read first and the ledger is append-only.
- `block_on_kernel` is always ready there.
- `ESRCH` maps to success, and a `KILL` refused (`EPERM`) now records `failed:` rather than a false `terminated`.
- The spawn-failure path reaps its child.
- No consumer parses `outcome`.
- Submitting a resumed exec's turn at the post-reconciliation tip is consistent with the history it carries.

It found four problems, all fixed:

1. **Medium, the record.** `a_member_that_ignores_term_is_killed_after_its_leader_goes` signalled before the subshell had set `trap '' TERM`, so the member often died the ordinary way. The mutation the record called caught — the killer's early return — passed 13 of 20 runs. The member now writes `ready` once it ignores `TERM`, and the test signals only after reading it. The mutation now fails 10 of 10 runs and the fix passes 10 of 10. The member's group is killed on the way out whatever the assertions find. Record correction: the self-review of `c359239` said that mutation "fails its test"; it did not reliably until now.
2. **Low, cost.** The whole-session replay read one event per SQLite connection — about 7 s for 10 000 events in a debug build, paid before a resumed TUI's first paint, in the daemon's `turns.submit`, and on `rapid exec --resume`. `EventLedger::events_of_kind` (and `InProcessKernelClient::events_of_kind`) reads the records whose kind starts with a literal prefix in one query, as one consistent snapshot. `job_recovery::job_events` uses it, and its resume and pass-limit machinery is gone. Test: `events_of_kind_reads_one_kind_of_one_session_in_order` (order, the session filter, a literal prefix — `turn_` does not match `turn.started` — and an unknown session is an error).
3. **Low.** The same gap as `start`'s, in `start_sandboxed`: a sandboxed job whose thread could not start kept its `job.started` without an end. It now records `failed`. Nothing had been spawned there. Not tested: a thread-spawn failure cannot be arranged.
4. **Info.** The host killer's comment said an empty group can only answer `ESRCH`. A freed group id reused within the grace by a new group leader would take the `KILL` — the residual risk `process_signal::terminate_process_group` also accepts. The comment now says so.

Revert cycle:
- The prefix's wildcards not escaped fails its test.
- The killer's early return fails its test 10 of 10 runs.
- `job_events` reading through the export fails `a_session_past_the_export_bound_is_still_read_to_its_end`, unchanged.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4160 passed, 1 failed — `computer_observe_reports_the_typed_platform_gate_not_a_stub`, which drives this host's desktop and fails the same way without this change; `pnpm` unaffected.
### Self-review of `625dbc8` — findings fixed in the part d commit

The background review confirmed the record's claims:
- `get` and the new read share `envelope_from_row`, and `get` does no extra validation.
- The new read fails only on a corrupt `job.*` row, where the export failed on any.
- It is one snapshot in WAL mode.
- It walks the `(session_id, seq)` primary key with no sort.
- The handshake cannot hang.
- The mutation the test guards fails 10 of 10 runs, and the guard reaps.

It found nothing at medium or above, and three leftovers, fixed:

1. **Low.** `start_sandboxed` recorded `job.started` before the table insert, whose poisoned lock returns early — a start with no end. The start is now recorded after the insert. Not tested: a poisoned lock cannot be arranged without a panic under it.
2. **Info.** `LIKE` ignored ASCII case, so `JOB.` matched `job.*`. The match is now the kind's first bytes compared exactly (`substr(kind, 1, n) = prefix`), which also retires the escaping. The test adds `JOB.` to the prefixes that must match nothing. Revert cycle: a case-folding comparison fails it.
3. **Info.** The test's guard killed the leader's group id on the success path too, after that id was free. It is now disarmed once the member is known gone.

## SEAM-03-1 (part d) — Job rows on ACP `session/update`, rebuilt on `session/load`

Contract restated: `crates/acp/src/v1.rs` —

- **Mapping.** `map_kernel_event` dropped every `job.*` record, so an editor on `rapid acp` never saw a background job. ACP has no job concept; a job is carried as what it is, a long-running `execute` tool call:
  - `job.started` becomes a `tool_call` — id `job:<job id>`, which cannot collide with a model tool call's; title `<handle>: <command>`, bounded like any tool title; status `in_progress`; kind `execute`.
  - `job.completed` and `job.orphan_reconciled` become a `tool_call_update`. The status is `completed` for `completed` with exit `0` and `failed` for any other end (a non-zero exit, `failed`, `cancelled`, `timed_out`, a dead host's job reconciled). The recorded `state` and `exit_status` or `outcome` ride in ACP's `rawOutput`, a new optional field, serialized only when present.
  - `job.output` (no producer) maps to nothing.
- **Load.** The mapping alone reaches a client only while a prompt streams, because the serve speaks inside prompts. `session/load` replied with nothing but the binding, and a prompt reports only its own turn. Now:
  - `session/load` (`HandleResult::ReplyAfterUpdates`) streams the session's job rows before it answers, as ACP has a load stream history. Each job gets its start and, once it has one, its end, from the ledger alone. The records are read in one query (`KernelClient::events_of_kind`, new on the trait, over the ledger read of `625dbc8`), however long the session; at most the latest `MAX_JOB_ROWS` (256) jobs are sent.
- **Prompt catch-up.** A prompt first sends the job ends recorded since the client last heard from the session — the ends that land between prompts. An end the client already had live is the same update again, which a tool-call update tolerates.

`apps/rapid/src/acp_serve.rs` sends a load's updates, then its reply.

Migration impact: an ACP client now sees `tool_call` rows with `job:` ids and `execute` kind for background jobs; `session/load` is preceded by `session/update` notifications when the session has jobs.

| Criterion | Status | Evidence |
|---|---|---|
| ACP `session/update` carries `job.*` | done | `a_background_job_is_an_execute_tool_call_row_on_the_wire` (the exact wire objects for start, a clean exit, the failing ends, a reconciled job; none for `job.output` or a record with no job id) |
| A reconnecting client rebuilds the rows from the ledger, and hears an end it missed | done | `a_reconnecting_client_rebuilds_the_job_rows_and_hears_the_ends_it_missed` (a new connection's `session/load` yields started/completed/started for two jobs; an end recorded between prompts is the next prompt's first update, and only it); `a_loaded_sessions_job_rows_reach_the_client_before_the_load_answers` (the serve sends the row, then the reply) |
| Revert cycle | done | no mapping for `job.started`; no rows on load; no catch-up before a prompt; every `completed` called a success; the serve dropping a load's updates; the catch-up ignoring the client's cursor — each fails its test (six mutations, one at a time) |

Limits, disclosed:
- Between prompts nothing is streamed; the serve has no idle channel. An end reaches the client at its next prompt or load.
- Job events in the daemon need no mapping: `events.subscribe` streams every record unfiltered, and an SDK client folds `job.*` itself. Each subscription ends at a turn's terminal event, so a client replaying a session subscribes again from the returned cursor — which AC-02's end-to-end test exercises.

Remaining for SEAM-03-1: `job.*` in `rapid exec` JSONL, and AC-02's end-to-end reconnect test.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4163 passed, 1 failed — `computer_observe_reports_the_typed_platform_gate_not_a_stub`, which drives this host's desktop and fails the same way without this change; `pnpm` unaffected (no SDK or wire schema change).

### Self-review of `1b8dbe7` — findings

The background review confirmed several points:
- `rawOutput`, an `in_progress` `execute` call, and updates before the load's reply are valid ACP.
- v2 passes the new result through unchanged.
- The live stream forwards job rows.
- Titles are cut on character boundaries, and the id bound is exact.
- `completed` with exit `0` is the only success any producer writes.
- The sandboxed start can no longer follow its end.

It found nothing at medium or above, and these, fixed in the commit after part e:

1. **Low.** The prompt catch-up could send an end whose start the client never had, and gave no row at all to a job still running. The catch-up sent only ends from the adapter's cursor, and `rapid acp` never moved that cursor while it streamed a prompt live, so the cursor stood where the previous prompt began. Now the serve records how far each session's prompt stream read (`StreamedThrough`) and reports it before the next prompt (`V1Adapter::heard_through`, forward only). The catch-up then sends the starts and the ends recorded after that — exactly what the client was not sent, with nothing repeated. Test: `a_prompt_sends_the_job_rows_recorded_after_what_the_client_heard` (a job started and ended between prompts by another surface is sent start first; the first prompt's jobs are not repeated).
2. **Low.** `session/load` rebuilt a dead host's jobs as `in_progress`: reconciliation ran only on a prompt. A load now reconciles the session's dead host's jobs first, through the same `open_session_jobs`. Test: `a_load_reconciles_a_dead_hosts_jobs_before_it_rebuilds_their_rows`.
3. **Info.** SQLite's `substr` counts characters, not bytes, so a non-ASCII prefix could never match. The prefix length is now its character count.
4. **Info.** The catch-up was read after the turn was submitted, so a failed read left a submitted turn nothing runs. It is now read before the submit.
5. **Info.** "as ACP has a load stream history" overstated it. A load replays job rows only; the conversation replay ACP describes is not done, and was not before.

Record corrections for part d:
- The catch-up sends the job starts and ends recorded after what the client was last sent live, not only the ends since the previous prompt began.
- "An end the client already had live is the same update again" no longer happens.

## SEAM-03-1 (part e) — A headless run's job records in `rapid exec --jsonl`

Contract restated:

- **`apps/rapid/src/exec_tools.rs`.** A job table counts its live workers (`JobWorker`, taken before a worker's thread is spawned and released once it has recorded the job's end). It holds only the counter, so a worker never keeps the table, or its `Drop`, alive. `JobRegistry::stop_all_and_settle(budget)` stops every job and waits, bounded, until each worker has recorded its end. Plain, sandboxed and detached-subagent jobs are all counted.
- **`apps/rapid/src/interactive.rs`, `rapid exec`.** The run holds its job table. When the model's turn returns, its jobs are stopped and settled (`EXEC_JOB_SETTLE`, 5 s; a warning on stderr if one does not settle). Before, they were killed by the process exiting, with no end on record — rows left `started`, then judged as a dead host's by the next host. With `--jsonl`, every `job.*` record the run added to its session follows the `router.decision` records and precedes the outcome records. That means records after the tip at open (`ExecRecording::opened`), so a continued session's reconciliation is included. Each record's type is its kind, its data is its payload exactly as on record, and its seq is the run's own.
- **`apps/rapid/src/headless/jsonl.rs`.** `JsonlRecord::recorded` builds a record from a ledger event in the run's own sequence.
- **`docs/api-contracts/headless-jsonl.md`.** Job records are described.

Migration impact: `--jsonl` output gains `job.*` records when a run starts background jobs. A run's jobs now end `cancelled` on record when the run ends, and a run with jobs still running takes up to the stop's own time to exit.

| Criterion | Status | Evidence |
|---|---|---|
| AC-06: `rapid exec` JSONL carries the same job events as the TUI | done | `jsonl_carries_the_runs_job_records_as_the_ledger_has_them` (the binary against a scripted model that starts `sleep 30` in the background: `rapid.schema`, `job.started`, `job.completed` `cancelled`, `assistant.message`, `session.finished` with increasing seq; the two job records' kinds and data equal the session ledger's `job.*` records) |
| A headless run's jobs end on record before it reports | done | `stopping_and_settling_leaves_every_jobs_end_on_record` (two running jobs; both `cancelled` on record when the call returns); the end-to-end test above |
| Revert cycle | done | no job records written; no stop before reporting; a settle that does not wait — each fails its test (three mutations, one at a time) |

Limits, disclosed:
- A run that is not recorded (its project could not be opened) writes no job records; the TUI has none for it either.
- A job whose stop takes longer than 5 s (a detached subagent mid-request) may end after the run reports; stderr says so.
- The integration test is Unix-only (its fixture path goes into a JSON string).

Remaining for SEAM-03-1: AC-02's end-to-end reconnect test.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4165 passed, 1 failed — `computer_observe_reports_the_typed_platform_gate_not_a_stub`, which drives this host's desktop and fails the same way without this change; `pnpm` unaffected.

### Self-review of `634dde0` — findings fixed in the follow-up commit

The background review confirmed several points:
- `share_job_table` reaches every tool surface a run has, before the structured-output wrapper takes it.
- The settle runs before the turn's terminal record on every path that reaches it. The early returns precede any tool.
- Worker counting pairs every increment with one decrement, and records `finished` before the release in all three workers.
- `JobTable`'s `Drop` is unchanged for the TUI, daemon and ACP.
- `opened` covers a continued session's reconciliation.
- The integration test would fail without the settle.

It found nothing at medium or above, and three low findings, fixed:

1. **Low, verified.** A finished job whose leftover process still held its pipe (`sh -c 'server &'`) kept its worker's claim until the reader reached end of file. `rapid exec` then waited the whole 5 s and warned, falsely, that an end might not be on record. The claim is now released as soon as the supervise loop ends, and every way out of that loop records the end. Test: `a_finished_job_whose_leftover_holds_its_pipe_is_already_settled` (settles in well under 2 s). The leftover itself is still not stopped when a run ends, as before: the job is over and only running jobs are stopped.
2. **Low.** A subagent child's jobs lived in the child's own table, which was never settled. They died with the child's tools, with their ends recorded on their workers' own schedule. The child's run now holds its table and settles it before the child reports. The jobs' lifetime is unchanged: they end with the child. Not tested: it needs a live subagent run.
3. **Low.** The JSONL block wrote every `job.*` record after the tip at open, including a record another host added to the same session meanwhile. It did so even when this run's turn was not recorded (a conflicting writer). It now writes only when the turn was recorded, and only:
   - the reconciliation this run did on opening, between `opened` and the tip its turn was submitted at;
   - the starts this process recorded after that (its `host_pid`), with those jobs' ends.

   Test: `a_headless_runs_job_records_are_its_own`.

Also: the `EXEC_JOB_SETTLE` comment said a stop is `TERM` then `KILL`. A stop is the group's `KILL`, or that sequence when the worker holds the child.

Record corrections for part e:
- "every `job.*` record the run added" now holds as stated. Before, it could include another host's records.
- The live-worker claim is released once the end is recorded, not when the worker thread ends.

Revert cycle: each of the following fails its test (five mutations, one at a time):
- the claim held until the readers finish;
- every record after `opened` written;
- `heard_through` a no-op;
- the catch-up without starts;
- no reconciliation on a load.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4170 passed, 0 failed; `pnpm` unaffected.

### Self-review of `1a3cd6c` — findings fixed in the part f commit

The background review confirmed several points:
- The streamed seq is exactly what the client was sent: the only events read and not forwarded are permission requests, pauses and the stop, none a job row.
- A second prompt, a continuation and a load after a prompt cannot race the position.
- A fresh bind catches up nothing, and `MAX_JOB_ROWS` drops whole jobs.
- Every exit from the plain worker's loop records the end before the claim is released.
- Every job start carries this process's pid.
- The four new tests fail under the record's mutations.

It found nothing at medium or above, and:

1. **Low, verified.** The JSONL's reconciliation window took every record between `opened` and `submitted`. That window lasts as long as the reconciliation's signalling, up to seconds, so another host's job record landing in it was written as this run's. The window now keeps only `job.orphan_reconciled` records of the jobs this run reconciled (`ExecRecording::reconciled`). Test: `a_headless_runs_job_records_are_its_own` (another host's start inside the window is left out). Also: the turn was submitted at the tip re-read after reconciling, so a turn another writer recorded meanwhile escaped the submit's conflict check. It is now submitted at that tip only when the tip moved by exactly this run's records; otherwise at the seq read before, so the submit conflicts as it always has. Not tested: it needs a writer inside the reconciliation.
2. **Low, verified.** The serve's half of the stream position — `stream_prompt` recording it, dispatch handing it to the adapter — had no test. It is now `Serve::hand_stream_position`. Test: `a_prompts_stream_position_is_where_the_next_catch_up_starts` (the stream records the turn's end; handing it moves the adapter's cursor there). Revert cycle: the stream recording nothing fails it.
3. **Info.** A load reconciled before the adapter could refuse it: a closed session, or the binding limit. Reconciliation now runs only for a session that exists and is not closed. The binding limit is still possible and rare. `SessionJobs` gains a registry per loaded session as well as per prompted one. Not tested: closing a session has no call these tests can make.
4. **Nit.** `ExecRecording`'s doc comment sat above the settle constant. It is back on the struct.
5. **Nit, record.** The self-review of `1b8dbe7` said the cursor "stood where the previous prompt began". `drain_updates` had moved it to the submit tip, which is still where the previous prompt began, not where its stream ended.

## SEAM-03-1 (part f) — AC-02 end to end: a reconnecting daemon client rebuilds the killed one's `/jobs` rows

Contract restated: `apps/rapid/tests/daemon_job_reconnect.rs` (new, Unix) runs the real `rapid daemon` on a real socket, in a trusted project, against a scripted loopback model whose first answer starts `sleep 5` in the background. The test:

1. A first SDK client speaks `rapidlm.sdk.rpc` v1: `hello`, `sessions.create`, `turns.submit`, and `events.subscribe` from the first event through the turn's end. It folds what it was streamed through the TUI's own reducer (`tui::state::reduce`) into `/jobs` rows: one job, running. The ledger holds no end for it yet.
2. The client is killed mid-stream: a live subscription is open when its connection is dropped.
3. The job ends on its own while no client is connected.
4. A second client connects and replays the session through the daemon, from its first event to the ledger's tip. The daemon ends each subscription at a turn's end, so the replay subscribes again past it.

The second client's rows through the first turn equal the killed client's. Its rows through the tip show the job completed on its own, exit `0` — not stopped with the client — and equal the rows the TUI projects from the project's ledger read directly. The test's leftovers are cleaned up whatever the assertions find. The daemon is stopped before the temp tree is removed, and its start is bounded by a deadline.

No product code is changed by this part. The same commit carries the self-review fixes above.

| Criterion | Status | Evidence |
|---|---|---|
| AC-02: killing the client and reconnecting through the daemon rebuilds identical `/jobs` rows from the ledger alone | done | `a_reconnecting_client_rebuilds_the_jobs_rows_the_killed_one_had` |
| Revert cycle | done | Each of these fails the test: the daemon's stream dropping `job.*` records; `job.started` not recorded; the daemon's turns given a fresh job registry, so a job dies with its turn and not on its own. |

SEAM-03-1 is complete:
- part a: session-scoped job registries in the daemon and ACP;
- part b: a detached job's end;
- part c: orphan reconciliation;
- part d: ACP rows;
- part e: exec JSONL;
- part f: AC-02 end to end.

AC-02 and AC-06 are evidenced.

Checks:
- `cargo fmt --check` and `cargo clippy --workspace --all-targets -D warnings`: green.
- `cargo test --workspace --locked --no-fail-fast`: 4171 passed, 1 failed. The failure is `goal_host::tests::a_driver_lease_becomes_available_again_once_dropped`, the known flake: a `flock` is inherited by a child another test forks between open and exec. It passes alone, and this change does not reach it.
- A first run was killed by a signal (exit 144) partway through the `rapid` library tests. The rerun completed.
- `pnpm`: unaffected.

### Self-review of `c386b8d` — findings fixed in the follow-up commit

The background review confirmed several points:
- The reducer fold is not vacuous: no protocol error, and the TUI needs no snapshot for job rows.
- The load gate matches the adapter's own refusals.
- The daemon test passed eight runs in a row with nothing left behind.

It found:

1. **Medium, verified.** The AC-02 test could not fail on reconnect. The daemon ends a subscription at a turn's end, so the second client replayed exactly the prefix the first had read. The equality held by construction, and a daemon that stopped a session's jobs when a client disconnected still passed. The "kill" was an idle close after the stream had ended. The test is now as recorded in part f above: a kill mid-stream, the job ending while no client is connected, and a replay to the ledger's tip that must show it completed on its own. Revert cycle: the daemon's turns given a fresh registry — the regression part a removed — fails it. Record correction: the part f entry as first committed described the weaker test. SEAM-03-1's "complete" and AC-02's "evidenced" rest on this test.
2. **Nit.** Test hygiene: a failure left the temp tree and the job behind, the tree was removed before the daemon stopped, and the daemon's start and the ACP test's stream had no deadline. All are fixed. The ACP test also now asserts the cursor was bound and behind before the handover.
3. **Info.** `reconcile_session` reported a record even when its append failed, so exec's tip arithmetic counted records that were not there. It now reports only the records that landed. Not tested: an append failure cannot be arranged.
4. **Nit, record.** Part f said "No product code changed" for a commit that also carried the self-review's code. It is now scoped to part f.

Also found while strengthening the test: the test opening the project's ledger right after the daemon printed where it listens — before the daemon opened the ledger itself — made the daemon exit, twice in two runs. Two processes opening a fresh ledger at once may race its creation. This is outside this task and flagged for a separate one.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4172 passed, 0 failed; `pnpm` unaffected.

### Self-review of `c0c9ee5` — finding fixed in the SEAM-03-2 commit

The background review confirmed the timing margins (the job's end lands about 5 s after a 70–170 ms turn, and it held under 3× CPU oversubscription), that the replay cannot stop at the first turn's end unnoticed, the revert-cycle claim, the drop order, and that `reconcile_session`'s side-effecting filter runs once per record. It found:

1. **Medium, verified.** The test still could not see a daemon that stops a session's jobs when a client disconnects. The daemon streams a subscription synchronously and does not read the socket while it does, so it notices a client killed mid-stream only at its next write — the job's own end, by which time there is nothing left to stop. The self-review of `c386b8d` claimed the test caught that regression; it did not. Now the first client is killed while idle, which the daemon sees at once, and a second client is killed mid-stream, both while the job runs. Revert cycle: a daemon that stops every session's jobs when a connection ends fails the test (`Cancelled`, not `Completed`).
2. **Nit.** The module's doc said `sleep 30`; it is `sleep 5`.
3. **Info.** The cleanup kept the job's group after the job had ended, on failure paths. It now lets go of the group once the end is on record.

## SEAM-03-2 — Moving a running command to the background: Ctrl-B and `/jobs bg`

Contract restated.

`apps/rapid/src/exec_tools.rs`:
- **The request.** The session's job table — which every turn's tools share, in the TUI as in the daemon and `rapid acp` — counts the foreground `shell_exec` commands waiting (`ForegroundWait`) and holds a request to move them to the background (`JobRegistry::request_demote`, `false` when none waits). A command's wait clears a request left from before it began.
- **The move.** A foreground command's wait loop, seeing the request, hands its running child, the output its readers are already spooling, and the rest of its timeout to `JobRegistry::adopt_foreground`. It becomes job `job-N`, and the call returns "moved to the background as job job-N while still running…" — the turn stops waiting on it. The job is supervised by the same `supervise_job` a started job is, now split out of `start` with `spawn_job_readers`, and is recorded through `JobEvents::demoted`, which by default is a start. A foreground command shares the host's process group, so an adopted job is stopped by its pid (`own_group: false`); a started job, which leads its own group, is stopped by the group as before.
- **The end.** The job's end is what the model hears at its next step, through `drain_notifications`, with everything the command wrote before and after the move.

`apps/rapid/src/interactive.rs`:
- **The ledger.** `LedgerJobEvents::demoted` records the move as a `job.started` with `moved_from_call` (the tool call it came from). `job_start_payload` is now shared by both starts.
- **The triggers.** Ctrl-B (`InteractiveInput::CtrlB`) and `/jobs bg` both call `demote_foreground`, which says what it did: "moving the running command to the background", or "no command is running in the foreground".

`crates/tui/src/commands.rs`: `/jobs bg` (`UiCommand::JobsBackground` → `KernelAction::DemoteForeground`).

Migration impact: `job.started` may carry `moved_from_call`; `/jobs` takes `bg`; Ctrl-B has a meaning in the TUI.

| Criterion | Status | Evidence |
|---|---|---|
| AC-01: moving a running command keeps it alive and ends the wait; the ledger shows the transition | done | `a_running_command_moved_to_the_background_ends_the_wait_and_keeps_running` (a real scripted turn: the model's next step is told the command moved; `job.started` with `moved_from_call: c1`; the panel's row running; the process alive) |
| Its completion reaches the model's next step | done | `a_running_foreground_command_moves_to_the_background_and_its_end_reaches_the_model` (the call returns while `sleep 2` runs; the end, with the output written before and after the move, is the next notice; the move is recorded from `c-fg`) |
| A moved command stops like a job | done | `a_command_moved_to_the_background_is_stopped_by_its_own_pid` (cancelled, it is gone within 1.5 s — by its pid, not the two-second fallback after a group signal that finds no group) |
| Ctrl-B and `/jobs bg` reach the session's table | done | `jobs_bg_and_ctrl_b_say_when_nothing_is_running_in_the_foreground` (the real loop); `ctrl_b_is_the_background_key`; `jobs_bg_moves_the_running_command_to_the_background` (`crates/tui`) |
| Revert cycle | done | Each of these fails its tests (four mutations, one at a time): no request honoured in the wait loop; an adopted job stopped as if it led its own group; the move not recorded with its call; the foreground wait not registered. |

Limits, disclosed:
- **Waking.** "Completion wakes the session" is met as the next model step: the end reaches the model when a step next runs, in this turn or a later one. A session with no further turn is not woken; the wake-on-event host (ADR 0015) has no construction yet (the Phase 0 audit).
- **Clients.** `rapid acp` and the daemon have no demote request: neither ACP nor the SDK protocol has a message for it. Their tables would honour one.
- **Timeout.** A moved command keeps the call's timeout, counted from its start, as a job started with the same arguments would.
- **Output cap.** Its output cap stays the foreground one (`MAX_SHELL_OUTPUT_BYTES`), not a job's.
- **Orphan identity.** It records no process identity for a later host to judge, since it has no group of its own: if its host dies, a later host records it `lost`.
- **Scripted model.** Its `capturing_blocks` now also records each history exchange, so a test sees the tool results a step was handed.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4178 passed, 0 failed; `pnpm generate:check`, `pnpm typecheck`, `pnpm test` (29 pass) green.

### Self-review of `12fb973` — findings fixed in the follow-up commit

The background review confirmed several points:
- The split of `start`'s worker is behaviour-preserving for started jobs: cap, overflow, settle, the cancel test, event order, where the claim is released.
- The wait loop's moves of the child and readers are sound.
- A single command's request cannot be lost.
- Two foreground waits on one table do not occur: `shell_exec` calls run serially, and subagents have their own tables.
- The TUI accepts `moved_from_call`.
- `DemoteForeground` has its own arm, never a turn interrupt.
- The history capture changes no other test's meaning.

It found:

1. **Medium, verified.** A job's end notice to the model — `drain_notifications`, the tail of its output — was never redacted; `job_output` was. A moved command therefore lost the redaction its foreground result had, and a background job never had it. The driver now redacts every notice as any tool output is redacted. Test: `a_jobs_end_notice_is_redacted_like_any_tool_output`.
2. **Medium, verified.** A moved command's spool was the foreground one: capped at 16 KiB, overflow never marked, so its notice showed the end of the first 16 KiB as if it were the command's end. A foreground command now spools as a job does, to a job's 64 KiB cap with overflow marked, and hands both to the job when it moves. Its own result is still bounded to 16 KiB. A notice now says when output past the cap was not kept. Test: `a_moved_command_that_writes_past_the_cap_says_the_rest_was_not_kept`.
3. **Medium, verified.** A moved command has no process group of its own, so stopping it killed only its pid, and what it had started — the server behind `npm run dev` — kept running. A job without a group is now stopped by its tree: `process_supervisor::kill_process_tree` reads the process table (`ps -A -o pid=,ppid=`) and kills every descendant, then the root. `process_signal::kill_process` is the one-pid `KILL`; on Windows `taskkill /T` does both. A descendant forked after the table is read escapes. Test: `a_moved_command_is_stopped_with_what_it_started`.
4. **Low.** A move request was checked before a cancel, so a Ctrl-B then Ctrl-C within one poll left the command running as a job. The cancel is now checked first.
5. **Low, record.** Corrections to the SEAM-03-2 entry:
   - `/jobs bg` takes no id; the worklist's `/jobs bg <id>` is not what shipped, since there is never more than one foreground command to name.
   - "ends the turn" is the wait: the tool call returns, and the model's next step may run more tools.
   - A foreground command inside a subagent is on the subagent's table, which Ctrl-B does not reach.
   - "stopped by its pid" is now "stopped by its process tree".
6. **Nit.** "moving the running command to the background" was printed even when the command ended first. It is now "asked the running command to move to the background".

Revert cycle: each of these fails its test (three mutations, one at a time):
- the notice not redacted;
- the overflow flag not handed to the job;
- a moved job killed by its pid alone.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4181 passed, 0 failed; `pnpm` unaffected.

### Self-review of `7f098cb` — findings fixed in the SEAM-03-3 (parts a and b) commit

The background review confirmed several points:
- Every path that gives job output to the model is now redacted.
- `own_group` is true exactly for jobs that lead their own group.
- The tree walk cannot reach init, tolerates zombies, and falls back to the root alone when `ps` is missing.
- It costs about 22 ms with about a thousand processes.
- A looping root respawned no child in 25 rounds.
- The Windows arm compiles by reading.

It found:

1. **Medium, verified.** The notice's 512-byte tail was taken from the raw spool and redacted afterwards. Redaction matches whole secrets, so a secret cut by the tail's start left its end in the notice. Each job's whole spool is now redacted first, and the tail taken from the result (`drain_notifications_through`). Test: `a_jobs_end_notice_is_redacted_like_any_tool_output` now places the secret so a raw tail would begin inside it, and asserts no part of it reaches the notice. Revert cycle: redacting after the tail fails it. Found alongside, pre-existing and flagged as its own task: `job_output` redacts page by page, so a secret straddling a page boundary comes back in two unredacted halves.
2. **Low, verified.** The overflow test did not catch the foreground spool going back to 16 KiB. It now asserts the kept output reaches past line 900, which 16 KiB cannot hold. Revert cycle: a 16 KiB spool fails it.
3. **Low.** `kill_process`'s doc comment had been pasted into the middle of `process_exists`'s; they are separated.
4. **Info, record.** The foreground result is not byte-for-byte what it was. Its spool can now exceed 16 KiB, so a result over that ends in `bounded_text`'s `[truncated]` marker, where before it was cut silently. The kept content is the same.

## SEAM-03-3 (parts a and b) — Loops in the cron store, and `rapid loop add|list|rm`

Contract restated.

`crates/event-ledger` (migration v6): cron rows gain `kind` (`cron`, the default for every existing row, or `loop`) and `expires_at_ms` (none for a cron job). In `cron.rs`:
- `CronStore::add_loop` inserts a loop that expires. It refuses a 51st active loop (`MAX_ACTIVE_LOOPS`) with the typed `CronStoreError::TooManyLoops`, counted in the insert's own IMMEDIATE transaction so two adds at once cannot both pass. A cron job, an expired loop or a quarantined one does not count, and the store's own total cap still applies.
- `claim_due` never claims a row whose expiry has passed.
- `remove_expired` deletes the expired loops, leaving a `firing` one to finish its lease, and returns them.
- `DEFAULT_LOOP_LIFETIME_MS` is seven days.

`crates/scheduler`: `PromptCron::add_loop` validates the schedule and sets the expiry from a lifetime. `poll` removes expired loops before it claims, and reports them (`PollReport::expired`).

`apps/rapid/src/loops.rs` (new):
- `interval_schedule` turns an interval into the five-field schedule that fires on it exactly: minutes dividing an hour, hours dividing a day, or `1d`. Anything else is refused — `*/7` minutes would fire at :56 and again at :00.
- `lifetime_ms` parses `--for` (minutes, hours or days, at most 30 days).
- `rapid loop add <interval> <prompt…> [--for <lifetime>] [--session <id>]`, `list` (loops only; it does not create the store it lists) and `rm <id>` (a loop's id only; a cron job is `rapid cron remove`'s).

`rapid cron poll`'s summary line adds `expired=`. The reference doc lists `rapid loop`.

| Criterion | Status | Evidence |
|---|---|---|
| AC-03: the cap refuses the 51st with a typed error | done | `a_51st_active_loop_is_refused_and_only_active_loops_count` (`TooManyLoops { limit: 50 }`, nothing written; cron jobs, an expired loop and a quarantined one do not count) |
| AC-03: a loop expires after its lifetime | done | `an_expired_loop_is_never_claimed_and_is_removed` (store); `a_poll_removes_an_expired_loop_instead_of_firing_it` (facade: reported, not fired, gone) |
| Existing rows survive the migration as cron jobs | done | `upgrade_from_v5_keeps_every_cron_row_a_cron_job_with_no_expiry` |
| `rapid loop` | done | `an_interval_becomes_the_schedule_that_fires_on_it_exactly`, `a_lifetime_is_bounded`, `rapid_loop_adds_lists_and_removes_only_loops` |
| Revert cycle | done | Each of these fails its test (five mutations, one at a time): no cap; `claim_due` ignoring expiry; the poll not removing expired loops; any minute count accepted; `rm` removing a cron job. |

Remaining for SEAM-03-3:
- part c: `/loop`, a poller in the session loop firing due loops as background Plan-mode turns with bounded context, and results recorded as `notification.recorded`, never in the foreground transcript;
- part d: `/jobs` showing each loop's next fire and expiry, deletable from the panel.

Until part c, a loop fires only through `rapid cron poll`, which already runs every due row — loops included — as a Plan-mode turn.

Checks:
- `cargo fmt --check` and `cargo clippy --workspace --all-targets -D warnings`: green.
- `cargo test --workspace --locked --no-fail-fast`: 4187 passed, 1 failed. The failure was `no_subcommand_summary_is_pushed_past_the_terminal_s_width`: the new `rapid loop` help line was 81 columns. Its summary was shortened, and that test and the two help-list pins were re-run and pass.
- `pnpm`: unaffected.

### Self-review of `700d33e` — findings fixed in the follow-up commit

The background review confirmed several points:
- Migration v6 is sound, and a downgrade refuses as the existing policy says.
- Every query reads the new columns.
- The cap is counted in the insert's transaction, and counts firing loops.
- `add`'s total-cap count becoming atomic only closes a race.
- The expiry boundaries agree.
- A firing loop past its expiry is removed on the next poll.
- The redaction test's arithmetic holds.

It found:

1. **Medium, pre-existing, verified.** A spool that overflowed was cut at its cap at an arbitrary byte, so it could end with a secret's first bytes, which whole-secret redaction cannot recognise. A notice then carried them — 20 bytes of the canary in the probe. An overflowed spool now drops its last `RedactionSnapshot::holdback_len()` bytes before redaction: `holdback_len` (new) is the longest a secret can be short of its end. Test: `a_secret_cut_by_the_spools_cap_leaves_no_part_of_itself_in_the_notice`. Revert cycle: no holdback fails it. `job_output`, which pages its spool, has the same two cuts (the cap, and a page boundary). That is flagged as its own task.
2. **Low–medium, pre-existing, verified.** `MigrationRunner::apply` read `user_version` before its IMMEDIATE lock and never again. Two hosts opening one ledger at once — the first run after this migration — made one fail with `duplicate column`. The version is now read again under the lock. Separately, switching a fresh database to WAL fails `SQLITE_BUSY` without waiting on the busy handler — likely what made the daemon exit in the AC-02 test when the test opened the ledger alongside it — so it is now retried within the busy timeout. Test: `hosts_opening_one_ledger_at_once_all_succeed` (six openers, a fresh database and a v5 one). Revert cycle: no re-read fails it three runs of three; no retry fails it two of three, as a race does. The daemon now says it is listening only once its ledger is open. The separate task flagged for the fresh-ledger race is withdrawn: this is it.
3. **Low.** The reference page said a loop runs "as a background turn"; until part c, `rapid cron poll` fires it as a Plan-mode turn. The page says so now.
4. **Low.** Expired loops were removed only by a poll: they listed as `active`, and filled the store's 512-row total for want of one. An add now removes them first, and `rapid loop list` shows an expired loop as `expired`. Test: the cap test now asserts an expired loop is gone after the next add. Revert cycle: no removal fails it.
5. **Low.**
   - `+5m` was accepted (`u32` parsing allows a leading `+`); a count is now digits only.
   - `rm` of a cron job's id said "not found"; it now says "not a loop".
   - A prompt word equal to a flag (`--for`, `--session`, `--db`) is taken as the flag: quote a prompt that contains one.
6. **Info.** A negative lifetime through the `PromptCron::add_loop` API gives a loop already expired. The command line refuses it.

## SEAM-03-3 (part c1) — A session fires only its own loops

Contract restated:
- **Store.** `CronStore::claim_due` — what `rapid cron poll` claims — now takes the unattended rows only: cron jobs, and loops no session owns. `claim_due_for_session` takes one session's loops.
- **Scheduler.** `PromptCron::poll_session_loops` polls the latter, removing expired loops whoever owns them, as `poll` does.

A session's loops are that session's to fire, so its host's poller (part c3) cannot take a cron job or another session's loop, nor they its.

| Criterion | Status | Evidence |
|---|---|---|
| Claims are scoped | done | `a_sessions_loops_are_its_own_to_fire` (the headless claim takes the cron job and the unowned loop; a session's claim takes its own loop only; another session's stays active) |
| Revert cycle | done | an unscoped headless claim fails it |

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4191 passed, 0 failed; `pnpm` unaffected.

### Self-review of `8eea456` — findings fixed in the part c2 commit

The background review confirmed several points:
- The claim SQL is right in every case: a cron job with a session is still the headless poller's, and the index is still used.
- Removing expired loops on add runs under the insert's lock, before the total count, and rolls back with a refusal.
- The concurrent-migration test is not vacuous: without the re-read it fails 20 of 20 runs, and without the WAL retry 7 of 20.
- The holdback is on the right side of both spools.
- Nothing but the AC-02 test reads the daemon's "listening" line.

It found:

1. **Medium, verified.** Part c1 left `rapid loop add --session <id>` storing a loop that nothing fires. The headless poller no longer takes an owned loop, and no session poller exists until part c3. The reference page said such loops are fired by `rapid cron poll`. `--session` is now refused with the reason until the session poller lands. Test: `rapid_loop_adds_lists_and_removes_only_loops` (refused, nothing stored). Revert cycle: accepting it fails the test.
2. **Low.** `rapid loop rm` of an id that does not exist said "not a loop … a cron job". It now says "not found"; a cron job's id still gets "not a loop".
3. **Info.** `AppliedMigration.from` reports the version read before the lock even when another host did the migrating. Only tests read it.
4. **Info, out of scope.** `crates/context-engine`'s stores switch to WAL without the retry. They share the exposure if two hosts open one at once.
5. **Info.** Registered secrets past about 21 KiB (their encodings included) make the holdback swallow a whole overflowed spool: the notice is then only the state and the marker. That is safe.

## SEAM-03-3 (part c2) — `notification.recorded`, projected as notices

Contract restated:
- **Event kind.** `crates/event-ledger` gains `EventKind::NotificationRecorded` (`notification.recorded`, family `notification`): something the user is told outside the conversation. Its payload is `source`, `text` (bounded by the producer to 1 KiB), and the `loop_id`/`outcome` it came from.
- **TUI.** `crates/tui` projects it into `AppState::notifications` (newest last, at most `MAX_NOTIFICATIONS` = 32), never the transcript. The jobs panel lists the latest five under its rows, newest first.
- **SDK.** The wire catalog and generated types gain the kind and the family (113 kinds).
- **Docs.** The event catalog names it.

The producer — a loop's result — is part c3.

| Criterion | Status | Evidence |
|---|---|---|
| A notification is a notice, not a turn of the conversation | done | `a_notification_is_a_notice_in_the_jobs_panel_never_the_transcript` (35 recorded: the transcript unchanged, 32 kept, the panel lists them newest first) |
| The SDK catalog matches | done | `pnpm generate:check`, the 113-kind pin |
| Revert cycle | done | the reducer ignoring the kind fails the test |

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4191 passed, 1 failed — `computer_observe_reports_the_typed_platform_gate_not_a_stub`, which drives this host's desktop (the known host flake); `pnpm generate:check`, `pnpm typecheck`, `pnpm test` (29 pass) green.

### Self-review of `e1cbe31` — findings fixed in the part c3 commit

The background review confirmed several points:
- Every other match on event kinds has a wildcard.
- The Rust kinds and the SDK catalog agree (113 of 113).
- `AppState` is never persisted.
- The cap and the `--session` refusal work.
- No wire version bump is needed for an additive kind.

It found:

1. **Medium, verified, latent until c3.** A notice whose text passed 16 KiB, or whose field was not a string, was a reducer error. That blocked every later event of the session, and resuming replayed the same event and froze again. The arm is now tolerant: text is cut at 4 KiB (`MAX_NOTIFICATION_TEXT_BYTES`), a source at 128 bytes, a missing or malformed field becomes a placeholder, and a secret event becomes "(redacted)" rather than a blank row. The producer also bounds its text to 1 KiB. Test: `a_hostile_notice_neither_freezes_the_session_nor_drives_the_terminal` (a 17 KiB text and a numeric source are kept, and a later job still lands). Revert cycle: the strict reading fails it.
2. **Medium, verified, latent until c3.** The notice rows went to the terminal raw, and a loop's answer is model output. They now pass through `sanitize_untrusted` like the log view. The same test finds no escape, bell or carriage return in the painted panel. Revert cycle: an unsanitised row fails it.
3. **Low.** The panel cut to its height after the job rows, so enough jobs hid every notice. Job rows now give way first. Test: `notices_are_not_what_a_full_jobs_panel_cuts` (ten jobs, six rows, the notice shown). Revert cycle: no room kept fails it.
4. **Low.** Covered by 1: a secret notice shows "(redacted)".
5. **Low, record.** The `rm` fix had no test that could tell the two messages apart. The message is now `rm_refusal`, tested by `rm_says_why_it_removed_nothing`.
6. **Pre-existing, now fixed.** `rapid daemon`'s hello named a wire-catalog hash seven catalog changes old, so the TypeScript SDK refused every real daemon (`unsupported_schema`), and c2 moved the expected hash again. The daemon now names `WIRE_SCHEMA_SHA256`, pinned to the SDK's generated hash by `the_daemon_names_the_wire_catalog_the_sdk_was_generated_from`. Revert cycle: the stale hash fails it.

## SEAM-03-3 (part c3) — A session fires its due loops as background Plan-mode turns

Contract restated.

`apps/rapid/src/loops.rs`:
- **`fire_loop`** runs a fired loop's prompt as the only turn of a fresh session of its own. There is no history, so its context is bounded by the prompt, and nothing of it enters the session that owns the loop. It is finished as the kernel records every turn.
- **`record_loop_notification`** tells the owner how it went: one `notification.recorded` with `source` `loop <id>`, the answer or failure bounded to `MAX_NOTIFICATION_TEXT_BYTES` (1 KiB, well under the TUI's display limit), `loop_id` and `outcome`.
- **`fire_due_loops`** claims the owner's due loops (`poll_session_loops`), fires each, records its notice, and reports the outcome to the store, which quarantines a loop after three failures in a row.

`apps/rapid/src/interactive.rs`:
- **`TurnSurface`.** The interactive turn assembly now takes a `TurnSurface`. `INTERACTIVE` is as before. `LOOP` forces Plan mode whatever the session's (read-only tools only) and installs no approval sink, so an `Ask` is denied, not left waiting for nobody.
- **`run_loop_turn`** runs a fire on that surface with fresh shared state and its own job table, settled when it ends.
- **The poller.** The session loop's tick calls `poll_loops`: at most every 30 s (`LOOP_POLL_EVERY`), and one poll's fires at a time, on a thread of their own. It runs only in a real session, which names its ledger (`SessionShared::loops`); scripted test sessions do not.

Migration impact: a TUI session now fires the loops it owns.

| Criterion | Status | Evidence |
|---|---|---|
| AC-03: a loop fires in a background node; its output never enters the foreground transcript | done | `a_due_loop_runs_on_a_session_of_its_own_and_comes_back_as_a_notice` (the prompt runs on a new session, finished there; the owner has no `turn.*` record, one `notification.recorded` with the loop's id, `completed`, the answer bounded; a failure's notice says `failed` and why) |
| A loop's turn is read-only and asks nobody | done | `a_loops_turn_is_read_only_and_asks_nobody` (the loop surface builds a Plan lattice even over a bypass-mode session; no approval sink) |
| Revert cycle | done | Each of these fails its test (three mutations, one at a time): no notice recorded; the loop surface not forcing Plan; the answer not bounded. |

Not tested:
- `poll_loops`' throttle and thread, which need a real session loop.
- A loop's turn against a real model: the scripted turn path forces its own mode.

`rapid loop add --session` stays refused until `/loop` (part c4) gives a session a way to create its own loops.

Checks:
- `cargo fmt --check`: green.
- `cargo clippy --workspace --all-targets -D warnings`: green after one collapsed `if`, style only. The `tui` crate's tests were re-run after it: 251 pass.
- `cargo test --workspace --locked --no-fail-fast`: 4197 passed, 1 failed — `computer_observe_reports_the_typed_platform_gate_not_a_stub`, the known host flake.
- `pnpm`: unaffected.

### Self-review of `fb5db9f` — findings fixed in the part c4 commit

The background review confirmed several points:
- Plan mode denies every non-read-only tool on a loop's turn — shell, MCP, `task_spawn`, `ask_user`, writes — and with no approval sink an `Ask` is denied.
- Stop hooks still fire for a loop's turn.
- A loop row is rescheduled before its fire, so a crash drops that fire rather than sticking it.
- The notice rows are sanitised, and the panel's height reservation is right in its edge cases.
- The daemon's pinned hash matches the generated one.

It found:

1. **High, verified.** The Windows clippy gate would fail: the daemon's `WIRE_SCHEMA_SHA256` is used only by the Unix hello, so on Windows it is dead code. It is now `#[cfg(any(unix, test))]`.
2. **Medium.** Every fire creates a session nothing marked as background. While a loop's turn streamed, or after the TUI exited mid-fire, that session was the newest: `rapid exec --continue` and `rapid resume` with no id opened it, and the known-sessions hint listed a 5-minute loop's 288 sessions a day. Now:
   - a loop's session is marked by an `automation.trigger_received` (`source: loop`) before its turn runs;
   - the ledger's `SessionSummary` gains `background` (set when a session holds that event);
   - "the most recent session" and the hint pass over such sessions.

   Tests: `a_session_with_a_trigger_is_listed_as_background_work` (ledger), `a_loops_session_is_never_where_someone_left_off` (a newer loop session is passed over by both), and the loop-fire test now asserts the marker. Revert cycle: each of these fails a test — no marker, the flag always false, the filter removed. `rapid sessions list` still lists every session.
3. **Low.** A panic outside the turn's own catch left the poll's `running` flag set, and that session's loops never fired again. It is now cleared by a drop guard, however the thread ends.
4. **Low.** A record appended by a background writer (a loop's notice, a job's end) between a prompt's tip read and its submit made the submit conflict, and the error ended the session. A conflict is now retried at the fresh tip up to four times (`SUBMIT_CONFLICT_ATTEMPTS`); a concurrent turn is still refused by the kernel's turn lease. Not tested: the window cannot be arranged.
5. **Nit.** `dequeue_if_ready`'s doc comment had been left above `poll_loops`; it is back on its function.
6. **Info, disclosed.**
   - Each fire sets up and tears down the project's MCP servers, whose tools Plan mode then denies.
   - A loop turn's usage accrues to the project's active goal.
   - A loop ignores the session's `/model select`, runs no `user_prompt_submit` hook, and loses its warnings.
   - Ctrl-C or the session ending does not cancel a loop's turn in flight.
   - `web_fetch` is read-only, so a loop turn can fetch within the allowlist.

## SEAM-03-3 (part c4) — `/loop`

Contract restated.

`crates/tui/src/commands.rs`:
- `/loop` and `/loop list` (`LoopList`).
- `/loop rm <id>` (`LoopRemove`).
- `/loop add <interval> <prompt>`, and `/loop <interval> <prompt>` as the short form (`LoopAdd`). The interval and prompt are the host's to judge.
- These dispatch to `KernelAction::{ListLoops, AddLoop, RemoveLoop}`.
- The catalog line is `/loop [list|rm <id>|add <interval> <prompt>]`.

`apps/rapid/src/command_help.rs`: the three actions are supported.

`apps/rapid/src/loops.rs`, `session_loop_action`, over the session's loops in the project's cron store:
- A list shows each loop's id, schedule, time to its next fire, time to expiry, and prompt.
- An add goes through the same interval rules as `rapid loop`, owned by this session with the default 7-day lifetime, and says how it will run.
- A removal takes this session's own loops only.
- A session with no ledger — a scripted one — says loops need a recorded one.

`apps/rapid/src/interactive.rs`: `SessionLoop::run_loop_action` shows the lines.

`rapid loop add --session <id>` is accepted again: such a loop is fired by that session's terminal UI while it is open.

| Criterion | Status | Evidence |
|---|---|---|
| `/loop <interval> <prompt>` starts a loop this session fires | done | `loop_takes_a_list_a_removal_and_an_interval_with_a_prompt` (the forms and their dispatch); `a_session_adds_lists_and_removes_its_own_loops` (add, a refused interval, a missing prompt, the list, another session neither seeing nor removing it, the removal, no ledger) |
| `/help` lists it | done | `every_synthesized_invocation_parses`, `availability_reflects_what_the_dispatcher_really_does` and `the_annotated_help_still_lists_exactly_the_catalog` pass with the new line |
| `rapid loop --session` | done | `rapid_loop_adds_lists_and_removes_only_loops` (a session's loop stored for its poller) |
| Revert cycle | done | Each of these fails its test: the list not filtered to this session; the short form not parsed. |

Remaining for SEAM-03-3, part d: `/jobs` showing each loop's next fire and expiry, and deletable from the panel. `/loop` already lists and removes them.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4202 passed, 0 failed; `pnpm` unaffected.

### Self-review of `20325fb` — findings fixed in the part d commit

The background review confirmed several points:
- The submit retry masks no concurrent turn. An occupied session, an active turn or a closed session refuses all four attempts; only a settled foreign append is retried past. The autonomous and queued paths share it.
- The `/loop` forms behave.
- Ownership is checked before a removal.
- `SessionSummary.background` changes no printed output, and only `fire_loop` emits the marker.
- The Windows gate is right.

It found:

1. **Medium-low, verified.** `/loop` (and, by the same line, the panel) showed a loop quarantined after three failed fires as due — "next in 2m" — while `rapid loop list` said `quarantined`. A stopped loop now reads `stopped: <reason>`. Test: `a_session_adds_lists_and_removes_its_own_loops` quarantines one and asserts the line. Revert cycle: no quarantine branch fails it.
2. **Low.** `rapid loop add --session` took any string, so a typo stored a loop nothing fires, holding one of the 50 slots until it expired. The id must now parse as a session id. Test: `rapid_loop_adds_lists_and_removes_only_loops` refuses `s-1` and accepts a real id. Revert cycle: accepting any string fails it.
3. **Low.** `fire_loop` marked its session after submitting the turn, so the marker landed after `turn.started`, and a failed submit left the session unmarked — "the most recent" again. The marker is now written right after the session is created, and the turn submitted at the tip after it. Test: the loop-fire test asserts the marker precedes `turn.started`. Revert cycle: marking after the submit fails it. Record correction: the self-review of `fb5db9f` said the marker lands before the turn runs; that holds now.

## SEAM-03-3 (part d) — Loops in the `/jobs` panel

Contract restated.

`crates/tui`:
- `LocalUiEvent::SyncLoops` projects the session's loops (`LoopRow`: the id `/loop rm` takes, and the line the host wrote — schedule, time to the next fire, time to expiry, prompt) into `AppState::loops`, at most 64 rows. Like the configured models, they are host state with no kernel event.
- The jobs panel lists them under `loops (/loop rm <id>)`, sanitised, between the job rows and the notices.

`apps/rapid/src/loops.rs`: `session_loop_rows` reads the session's own loops and does not create a store that is not there. It shares `/loop`'s line format (`loop_line`).

`apps/rapid/src/interactive.rs`: `SessionLoop::sync_loops` refreshes the rows on the poll's 30 s cadence (first at the session's first tick) and after every `/loop`.

| Criterion | Status | Evidence |
|---|---|---|
| AC-03: `/jobs` shows each loop's next fire and expiry; deletable from the panel | done | `the_jobs_panel_lists_the_sessions_loops` (the section, the row with its next fire and expiry, sanitised); `the_panel_rows_are_the_sessions_own_loops` (own loops only; no store created). The panel names `/loop rm <id>`, which removes one (part c4) — the panel has no keyboard selection to delete a row from directly. |
| Revert cycle | done | Each of these fails its test: the section not painted; the row unsanitised; the rows not filtered to the session. |

SEAM-03-3 is complete (parts a–d). AC-03 is evidenced part by part:
- a loop fires as a background node, its output never in the foreground transcript (c3, fired through `fire_due_loops` with a scripted turn runner);
- it expires after its lifetime (a);
- the 51st is refused with a typed error (a);
- `/jobs` shows it with its next fire and expiry (d).

Not tested: a fire driven by the 30 s poll against a real model.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4204 passed, 0 failed; `pnpm` unaffected.

### Self-review of `21d28fa` — findings fixed in the SEAM-03-4 commit

The background review found:

1. **Medium, verified.** While a loop fire ran, the poll returned before it resynced, so the panel's loop rows froze — the fired loop showed "next in 0s" for the whole turn. The poll now resyncs on every throttled tick, even mid-fire. The fire thread's drop guard asks for a resync when it ends, so the next tick refreshes the rows the fire moved. Not unit-tested: the poll's thread timing has no seam. The change is recorded here, not revert-cycled.
2. **Medium, verified.** The jobs panel dropped rows from the top when too tall, so a short panel lost the loops section first, or left a header with no rows under it. The panel is now budgeted:
   - notices first (the header and up to 5);
   - then loops, cut to a trailing "… n more (/loop lists them)", or a single "loops: N" line when one row is all that fits;
   - then the job rows with the room left.

   Test: `a_short_panel_cuts_loops_with_a_count_never_a_bare_header`. Revert cycle: each of these fails it — no room kept for the "… more" line; the one-row count removed.
3. **Low, accepted.** `sync_loops` reads sqlite on the UI thread every 30 s. It is one indexed read of at most 64 rows. Disclosed, not moved.
4. **Low, disclosed.** The host's `SyncLoops` wiring — the poll to the panel — has no end-to-end test. The projection and the row reader are each tested.
5. **Record correction.** SEAM-03-3's validation asks for "fires on schedule with a scripted model", but the part c3 test fired through `fire_due_loops` with a scripted turn runner (a closure), not a scripted model through the loop surface. `a_loops_turn_is_read_only_and_asks_nobody` covers the surface itself. A scripted model driven through `run_loop_turn` needs a test seam there. This is **not done**, and is recorded as partly met.

## SEAM-03-4 — Monitor

Contract restated: a monitor runs a command through the job path. Each stdout line becomes a bounded `notification.recorded` with provenance. Above a rate, the monitor stops itself with a notice and a hint. With `persistent: true` it lives for the session; otherwise it ends with its turn. `/jobs cancel` stops it.

**Deviation — a mode, not a new tool.** A monitor is `shell_exec` with `"monitor": true`, not a separate `monitor` tool. It reuses the command's whole permission surface — the approval rules, command classification, workspace confinement, and the job path's output caps and redaction — rather than a second copy that could drift. `persistent` requires `monitor`. A monitor cannot be `sandbox`ed, since the sandbox runs to completion. A monitor with no timeout gets 12 h.

`apps/rapid/src/exec_tools.rs`:
- `spawn_monitor_reader` spools stdout exactly like a background job, so `/jobs logs` holds the whole output. It splits lines, cuts each at 512 bytes, and calls `JobEvents::line` in order.
- More than 40 lines in 2 s is a flood: the reader cancels the job and calls `JobEvents::flooded` once.
- Stderr goes through the ordinary job readers.
- A monitor started without `persistent` is turn-scoped (`JobShared::turn_scoped`). `JobRegistry::stop_turn_scoped` stops those.

`apps/rapid/src/interactive.rs`:
- `LedgerJobEvents` writes each line and each flood as `notification.recorded`:
  - source `monitor job-N`;
  - the job id;
  - outcome `line` or `flooded`, where the flood notice names the limit and says to restart with a tighter filter.
- Both interactive turn paths call `stop_turn_scoped` when the turn ends.

| Criterion | Status | Evidence |
|---|---|---|
| AC-04: one notification per line with provenance | done | `a_monitor_delivers_each_line_it_prints_in_order` (every line, in order, a final line with no newline); `a_monitors_lines_arrive_as_notices_and_a_turns_monitor_ends_with_it` (session: each line a `notification.recorded` with source `monitor job-1` and the job id, a notice in the panel, not in the transcript) |
| A flood auto-stops with a notice | done | `a_flooding_monitor_is_stopped_once_and_says_so` (exactly 40 lines delivered, one flood notice, the job ends cancelled) |
| A persistent monitor survives turn boundaries and stops on kill | done | `a_monitor_ends_with_its_turn_unless_persistent` (the turn's end stops only the turn-scoped monitor; the persistent one runs until cancel stops it); the session test (a turn's monitor ends with the turn) |
| Arguments | done | `a_monitor_is_a_shell_exec_mode_and_persistent_is_a_monitors` |
| Windows runs the same test | pending CI | The tests use `sh -c` and `test_fixtures::tool_str("sleep")`, the same as the existing ungated background-job tests the Windows gate runs. Not observed locally. |
| Revert cycle | done | Each of these fails its test: the turn-end stop removed; the flood gate removed; the ledger line not written; `persistent` ignored. |

Not done: the model does not read monitor lines back. Notices reach the user (panel) and the ledger; only the TUI and loops consume `notification.recorded`. A turn that wants the output reads `/jobs logs` or `job_output`. The daemon and `rapid acp` get monitors through the shared tool, but their job events do not turn lines into notices yet (`JobEvents::line` defaults to nothing).

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4210 passed, 0 failed; `pnpm` unaffected.

### Self-review of `e20fd1a` — findings fixed in the SEAM-03-5 part a commit

The background review verified two defects, found four plausible ones and two overclaims in the record. The panel budget and the loop resync checked sound. All are fixed:

1. **High, verified.** Monitor lines reached the ledger and the panel unredacted, so a secret a watched command printed was recorded for good. The record's claim that a monitor reuses "the job path's … redaction" was false. Now every line passes through the session's redaction before anything records it. It is redacted whole, then cut, so a cut cannot split a secret past recognition. A line redaction cannot judge is not recorded. Test: `a_monitors_lines_are_redacted_before_anything_records_them`. Revert cycle: no redaction fails it.
2. **Medium, verified.** A line longer than 2 KiB came out as a notice per 2 KiB piece: 25 notices for one 100 KB line, and one line of about 170 KB tripped the flood gate. Now a long line is one notice, its head. The rest up to the newline is dropped from the notices, and the log keeps it all. Test: `a_long_line_is_one_notice_and_the_next_line_its_own`. Revert cycle: not skipping the rest fails it.
3. **Medium, plausible.** The 40-in-2-s gate did not bound ledger volume: a steady 19 lines/s for the 12 h default was about 820 000 records. A sustained limit now applies too, 300 lines in 60 s, and the notice names whichever limit broke. A slow ledger cannot hide from it either, because it counts delivered lines over a minute. Test: `a_steady_stream_under_the_burst_limit_is_stopped_by_the_sustained_one` (`FloodGate` at 10 lines/s trips on line 301; a burst still trips the burst limit). Revert cycle: no sustained limit fails it.
4. **Medium, plausible.** The turn-end stop was on the wrong boundaries in two ways:
   - A turn paused on an approval (`Waiting`) stopped its own monitors, though its continuation is the same turn.
   - A continuation's end (`continuation_turn_inner`) stopped none.

   `end_turn_scoped` now stops them on every outcome but `Waiting`, after both the first run and a continuation. Test: `a_paused_turn_keeps_its_monitors_and_its_end_stops_them`. Revert cycle: stopping on `Waiting` fails it. The continuation wrapper itself is not driven by a test.
5. **Low-medium, plausible.** Lines could be recorded after the job's end: the supervisor waits only 250 ms for readers, and each line is a ledger write. Now a monitor's end is recorded under the gate its reader delivers lines under (`MonitorEvents`), so no line follows the end. The monitor's settle is 2 s, so a command's last lines normally land first. Tests:
   - `every_line_is_recorded_before_the_monitors_end`: 30 lines at 20 ms a write, all before the end.
   - `no_line_is_recorded_after_the_monitors_end`: at 150 ms a write, the end is last and the rest stay in the log.

   Revert cycles: a plain job's settle fails the first; no gate fails the second.
6. **Low, plausible.** A monitor the flood gate stopped read "cancelled at shutdown" in `/jobs` and `job_status`. It now reads "stopped: it printed lines faster than a monitor may". The flood test asserts it. Revert cycle: the old label fails it.
7. **Low — record correction.** The SEAM-03-4 section said the daemon's and `rapid acp`'s monitor lines "do not turn into notices (`JobEvents::line` defaults to nothing)". That is wrong. Both run turns through `run_interactive_turn_inner` with `LedgerJobEvents`, so their lines are recorded as `notification.recorded`. The real gap is that neither surfaces those records to its client yet. Two related points:
   - A monitor's explicit `timeout_ms` was capped at the command limit (600 s) while no timeout gave 12 h; a monitor now takes a timeout up to 12 h.
   - A subagent's `persistent` monitor ends with the subagent, whose jobs are stopped when it ends, although the tool's summary says "for the session". Disclosed, not changed.

## SEAM-03-5 (part a) — What a resumed session still has running

Contract restated: on `/resume` and `rapid exec --continue`, the model gets a block derived from the session's non-terminal work — jobs, subagents, loops. It is injected through `host::build_packet`, and nothing is stored. A session with nothing running gets no block.

`apps/rapid/src/still_running.rs`: `running_block(client, session, ledger)` derives the block each time it is asked:
- The session's `job.*` records through `job_recovery::open_jobs` give the open jobs. A job counts only while the host that recorded it is alive: this process, or another such as the daemon. A record naming no host is listed as "last recorded".
- Its `agent.*` records give the subagents spawned and not ended whose recorded host is alive. `agent.spawned` now carries `host_pid`. A subagent with no recorded host is left out: it ran inside an earlier host's turn.
- `loops::session_loop_rows` gives its loops.

The block is at most 20 rows of 160 bytes, one line each, then "… n more". It is always under `MAX_RUNNING_BLOCK_BYTES` (4 KiB).

`apps/rapid/src/host.rs`: `PreservedLiveContext::with_running_block` compiles it as the system block `jobs/running`. Like every preserved field, it survives the stall and overflow recompiles.

`apps/rapid/src/interactive.rs`:
- `SessionShared::resumed` is set when a TUI starts on an existing session and by `/resume`. The next turn takes it (`take_running_block`), derives the block then, and clears it.
- `/resume` first reconciles the jobs a dead host left in the target, as a TUI started on a session does, so the block is not told a dead host's job still runs.
- `rapid exec --continue` / `--resume` adds the block to its one turn.

| Criterion | Status | Evidence |
|---|---|---|
| AC-05: resume with a background process alive yields the block | done | `a_resumed_sessions_next_turn_is_told_what_still_runs_and_only_then` (a scripted session: a background `sleep 30` and a loop; the resumed turn's compiled context has `jobs/running` naming both; the next turn has none) |
| With nothing running, no block (compiled-context test) | done | the same test's first turn (resumed, nothing running: no block); `binary_exec_continue_runs_the_next_turn_of_the_recorded_session` (the binary: a continued run with nothing running has no block; after `rapid loop add --session`, the next continued run's model request names the loop) |
| Only what really runs | done | `a_job_counts_while_its_host_lives_and_until_it_ends`, `a_subagent_counts_only_with_a_live_recorded_host`, `nothing_running_is_no_block_and_a_long_list_is_bounded`, `the_running_block_is_compiled_as_a_system_block_and_only_when_given` |
| `/resume` sets it | done | `resume_returns_to_the_session_you_left` asserts the flag after a bare `/resume` |
| Revert cycle | done | Each of these fails its test: the flag read, not taken; the flag ignored; the block not compiled; the exec path not adding it; a dead host's job counted; a hostless subagent counted; `/resume` not setting the flag. |

Not covered:
- A TUI started with `--resume` / `--continue` sets the flag from `options.resume`; no test drives that start.
- Workflow runs are not in the block: their records are not read here.
- A monitor is listed as a job; `job.started` does not mark it a monitor.
- On Windows a host's liveness cannot be read (`process_exists` is always false there), so only this process's own jobs and subagents are listed.

Remaining for SEAM-03-5, part b: the wait ceiling.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green. `cargo test --workspace --locked --no-fail-fast`: 4219 passed, 2 failed.
- `a_resumed_sessions_next_turn_is_told_what_still_runs_and_only_then`: its final clean-up asserted that the `sleep 30` job was still there to cancel, and under the full suite's load the turns outlasted it. The clean-up no longer asserts, and the test passes.
- `shell_exec_runs_argv_inside_the_root_with_bounded_output`: host timing, not this change. A freshly written script takes about 4 s to start on this machine outside any test (`time` on a new two-line script: 4.2 s), which is the operating system checking a new executable. The test runs two such scripts inside a 10 s timeout, and alone it passes in 8.0–8.4 s or times out at 10 s. It does not touch the monitor or resume code.

`pnpm` unaffected.

### Self-review of `b3cff64` — findings fixed in the SEAM-03-5 part b commit

The background review found one defect verified by a test and six by reading, and three overclaims in the record. It checked these as sound: no lock-order deadlock between the reader's gate and the end record; chunk edges in the long-line skip; a bounded flood-gate queue; the `resumed` flag kept when turn preparation fails and never taken by a loop or continuation; `/resume`'s reconcile touching only dead hosts' jobs; and both ledger paths.

1. **Medium.** A turn-scoped monitor was stopped by *any* turn's end: `stop_turn_scoped` stopped every turn-scoped monitor in the table. The TUI keeps one table across `/resume`, so a turn ending in session B stopped a monitor session A's paused turn had started, and nothing said so. Keeping monitors across a pause (the `e20fd1a` fix) also meant an approval never answered left one running for 12 h.

   Now each turn run's tool surface takes its own monitor scope (`WorkspaceTools::monitor_scope`), and a monitor carries the scope of the run that started it. A run's end — completed, failed, or paused for an approval — stops exactly its own (`end_turn_scoped(jobs, scope)`), after the first run and after a continuation alike. The tool now says so: "until this turn ends or pauses for an approval", and `persistent` keeps it across both.

   **Record correction:** the `e20fd1a` item 4 fix ("a paused turn keeps its monitors") is replaced by this. A pause ends the paused run's monitors.

   Test: `a_turn_runs_end_stops_its_own_monitors_and_no_other_runs` (one run's end stops its monitor, not another run's or a persistent one); the exec_tools turn test asserts another scope stops nothing. Revert cycle: stopping every scoped monitor fails it.
2. **Medium.** The block named another process's jobs by their `job-N` handle and said `job_status` reads them. A handle belongs to its host, so this process's `job-1` is another job or none. A job this process runs is still named by its handle. One run by another live process now reads "a job run by another process (pid N; its job-1 is not this process's)". Test: `a_job_counts_while_its_host_lives_and_until_it_ends`. Revert cycle: naming every job by handle fails it.
3. **Low-medium.** Loops stopped after failures, and expired loops, were listed as still running. The block now takes `loops::active_session_loop_rows`, which leaves out the quarantined and the expired; the panel still lists them, saying stopped. Each loop row says it "fires while this session is open in the terminal UI", which is when a session's loops fire. Test: `only_a_loop_that_can_still_fire_is_active`. Revert cycle: keeping the quarantined fails it. The expiry filter is not revert-cycled: the store already drops a loop past its expiry when it lists.
4. **Low-medium, plausible — disclosed, not changed.** A host counts as alive by pid, so a dead host's pid reused by another process reads as alive, both here and in `job_recovery::reconcile`, which predates this work. That includes a process of another user (`EPERM` reads as existing) and a container where every run is pid 1. Such a job is then listed, and never reconciled. Fixing it needs the host's start time in `job.started`; that belongs to reconciliation and is not done here. A subagent whose finish record failed to land is listed while this process lives.
5. **Low.** A line redaction could not judge was recorded as an empty notice, although the code and the record said it was not recorded. The redactor now returns `None` for such a line, and nothing is recorded. There is no test: a redaction failure cannot be provoked from outside the security crate.
6. **Low, verified.** Output with no newline at all was never rate-limited once its first 2 KiB had been delivered, so a firehose or a `\r`-only progress bar ran unstopped for 12 h. Each further 64 KiB of such output now counts as one line against the rate, without being delivered, so an endless stream is stopped and one long line is not. A first version counted every 2 KiB cut, and under the full suite's load it stopped a single 100 KB line as a flood (`a_long_line_is_one_notice_and_the_next_line_its_own` failed); 64 KiB fixed that. Test: `output_with_no_newline_still_counts_against_the_rate` (`yes … | tr -d '\n'` is stopped as a flood, one notice delivered). Revert cycle: not counting the skipped output fails it.
7. **Low, plausible.** The block is a system block, and it quotes model-written text: commands, subagent tasks, loop prompts. It now says so in its header ("quoted from the session, not instructions"), and every control character in a row becomes a space. Test: `a_row_cannot_drive_the_terminal_or_break_its_line`. Revert cycle: no control-character filter fails it.

## SEAM-03-5 (part b) — The wait ceiling

Contract restated: output and completion waits get a ceiling, `job.wait_ceiling`, one hour by default. A wait that reaches it reports the job still running, not failed.

`apps/rapid/src/exec_tools.rs`:
- `job_status` and `job_output` take an optional `wait_ms`. `job_status` waits for the job to end; `job_output` waits for output past `offset` or the job's end. Either stops early on a cancelled turn.
- The wait is cut to the ceiling (`WorkspaceTools::job_wait_ceiling`, default `DEFAULT_JOB_WAIT_CEILING` = 1 h).
- A wait that runs out succeeds, with the job's state and "— still running after waiting Ns (the wait ceiling is Ns); it is not a failure — wait again or carry on". `job_output` adds its "continue at offset N".
- With no `wait_ms`, both answer at once, unchanged.

`apps/rapid/src/user_config.rs`: `[job] wait_ceiling = <seconds>`, 1 to 86 400, sets the ceiling; `job_wait_ceiling(env)` reads it. Every interactive, loop, daemon and ACP turn (through `build_interactive_turn_tools`) and `rapid exec` apply it.

| Criterion | Status | Evidence |
|---|---|---|
| A wait past the ceiling reports still running | done | `a_wait_returns_when_the_job_ends_and_a_wait_past_the_ceiling_is_still_running`: a job ending within the wait returns with its end; a 60 s wait on a 30 s job under a 300 ms ceiling returns in time as `running … still running after waiting 300ms (the wait ceiling is 300ms) … not a failure`, a success; the same for `job_output`; no wait means no note |
| Output wait | done | `an_output_wait_returns_when_the_job_prints` |
| Cancellation | done | `a_cancelled_turn_stops_waiting` (cancelled mid-wait) |
| Configurable, default one hour | done | `the_wait_ceiling_is_read_from_the_config_or_is_an_hour`, `job_wait_ceiling_is_bounded_seconds`, `a_wait_must_be_a_positive_number_of_milliseconds` |
| Revert cycle | done | Each of these fails its test: the ceiling not applied; the readiness check removed; `job_status` not waiting; the cancel check removed; the config value ignored. |

Not covered:
- A foreground `task_spawn` still blocks until its subagent ends; only cancellation stops it. It is a completion wait with no ceiling. Its background form is waited on through `job_status`, which has the ceiling. Moving a foreground subagent to the background at the ceiling is not done.
- The default reaching the ceiling in a real hour is not tested; the ceiling is set short in the test.

SEAM-03-5 is complete: AC-05 is evidenced in part a, and the waits in part b.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `pnpm generate:check` green (the SDK carries no tool schemas). `cargo test --workspace --locked --no-fail-fast`: 4229 passed, 1 failed — `computer_observe_reports_the_typed_platform_gate_not_a_stub`, the known host-desktop flake, which has nothing to do with this change.

The run before this one failed three more tests, all fixed here:
- `a_long_line_is_one_notice_and_the_next_line_its_own`: the 2 KiB counting — see item 6.
- `a_monitors_lines_arrive_as_notices_and_a_turns_monitor_ends_with_it` and `a_resumed_sessions_next_turn_is_told_what_still_runs_and_only_then`: under the suite's load their `sleep 30` ended before their assertions. They now sleep 300 and are stopped at the end.

### Self-review of `80306c0` — findings fixed in the next commit

The background review found one defect it verified, three plausible ones and two overclaims. It checked these and found them sound: the loop filter (`CronJobStatus` has only Active, Firing and Quarantined); exact-scope matching; the background `task_spawn` wait; `wait_ms` arithmetic; early returns in both turn paths, none of which can leave a monitor running; and scope isolation between a parent and its subagents.

1. **Medium, verified.** A `job_output` wait at an overflowed spool always ran to the ceiling. The spool stops at 64 KiB, so output past `offset` could never arrive; a model following "continue at offset 65536" with `wait_ms` froze its turn for up to an hour. A wait now also returns when the spool has overflowed. Test: `an_output_wait_at_the_capture_limit_returns_at_once`. Revert cycle: without the overflow check it waits out its 30 s and fails.
2. **Low-medium.** Subagents' tools never got the configured ceiling; they kept the 1 h default. The child's tool surface now applies `[job] wait_ceiling` too. No test drives a child's wait; the change is one call beside the child's other shared settings. **Record correction:** part b's "every interactive, loop, daemon and ACP turn" now includes subagents.
3. **Medium, plausible — disclosed, not changed.** A long wait holds the whole tool batch: the batch joins every call, so a sibling call's approval prompt, answered at once, cannot resume the turn until the wait returns. The wait is at most the ceiling, and Ctrl-C ends it (TUI, daemon, ACP, exec and subagent turns all pass a cancellable token; a loop turn's token is never cancelled, but loop turns run read-only on their own job table). Ending a wait when an approval is pending needs the approval state inside the tool, which it does not have. The model is told a wait can be shortened, and the ceiling is configurable.
4. **Low, plausible.** A turn run that panicked never reached the turn-end stop, so its turn-scoped monitors ran on until their 12 h timeout: nothing sweeps a lost scope any more. Now each run holds a `TurnMonitors` guard whose drop stops that run's monitors, on every return and when a panic unwinds. It replaces the three explicit calls. Test: `a_turn_run_that_panics_still_stops_its_monitors`. Revert cycle: a drop that does nothing fails it.
5. **Low.** A wait cut short by Ctrl-C returned the job's state with nothing saying so. It now adds "— the wait was cut short: the turn was cancelled". Test: `a_cancelled_turn_stops_waiting` asserts it. Revert cycle: no note fails it.
6. **Overclaim, corrected.** The `b3cff64` item 6 said an endless stream is stopped, and named a `\r`-only progress bar. Newline-less output counts one line per 64 KiB, so the limits trip only above about 1.25 MiB/s (burst) or about 320 KiB/s (sustained). A real firehose such as `yes | tr -d '\n'` is stopped. A slow `\r` progress bar is not: its first notice is its head, and it runs until its timeout or its turn's end. `\r` is not a line end here.
7. **Overclaim, low.** "Nothing that drives a terminal" covered only control characters. The line and paragraph separators (U+2028, U+2029), zero-width and direction marks, embeddings, overrides, isolates and the byte-order mark now become spaces too. Test: `a_row_cannot_drive_the_terminal_or_break_its_line` adds U+2028 and U+202E. Revert cycle: controls only fails it.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4232 passed, 0 failed; `pnpm` unaffected.

### Self-review of `588dfd8` — findings folded into the SEAM-04-1 parts a–b commit

The background review found no correctness defects. It checked these as sound:
- the guard's placement and drop order (the stream-buffer flush now runs before the stop instead of after; both still precede the turn-end hooks);
- early returns (the continuation's returns after the replayed call now stop that call's monitors — a fix the record did not mention);
- overflow readiness in every case;
- the cancel note appearing only after a wait;
- the subagent ceiling matching the parents'.

Its minor findings:
1. **Low, fixed.** After an overflow, `job_output` still said "continue at offset 65536", inviting a loop of empty polls. At the capture limit it now says "nothing more is captured — job_status says when it ends". Test: `an_output_wait_at_the_capture_limit_returns_at_once` asserts it. Revert cycle: the old advice fails it. A bounded page below the limit still gives its next offset.
2. **Nit, fixed.** `JobWait::note`'s doc comment now includes the cancelled case.
3. **Nit, record.** A cancelled turn's tool result rarely reaches the model. The note is for the record and the user, not something the model acts on.
4. **Nit, not changed.** The overflow and cancel tests leave their `sleep 300` job behind if an assertion fails before their cancel; the job ends at its own timeout.

## SEAM-04-1 (parts a–b) — Agent types from definitions, on the spawn path

Contract restated:
- A definition gains `base_role`, `instructions`, `model`, `reasoning_effort`, `inputs` and `outputs`, under a closed schema with narrow-only grants.
- `~/.rapidlm/agents` loads below the project's directory and cannot shadow built-ins.
- `task_spawn`'s `type` resolves through the inventory, and the child gets the definition's role, tool surface, model, effort and instructions.
- An unknown type is a typed refusal listing the known ones (ADR 0023 §5).
- Validation: an overlay asking for a tool outside its base role is rejected with field-level remediation; a valid overlay appears in `/agents` and `rapid agents list`; its model and effort reach the child's request.

`crates/agent-runtime/src/agent_defs.rs` (part a):
- The `[agent]` fields are `id`, `description`, `base_role` (the older `role` still works; both at once is refused), `tools`, `instructions` (at most 8 KiB), `model` (a configured `[model.<id>]` id), `reasoning_effort` (the router's seven names), and `inputs` / `outputs` (at most 16 distinct names each). An unknown field is refused, listing the fields.
- Every field error names its field and a remedy (`AgentDefError::FieldInvalid`: "field `reasoning_effort`: not a reasoning effort; use one of: none, minimal, low, medium, high, xhigh, ultra").
- A tool outside the base role reads "field `tools`: … Remove 'write' from `tools` (role 'explorer' exposes: read, net, mcp), or choose a `base_role` that exposes it".
- `DefSource::User`.
- `layered_inventory(project, user, impls)`: built-ins, then the project's (`None` when untrusted), then the user's.
  - A user definition with a built-in's or the project's id is rejected, saying which it lost to.
  - A user file colliding with a built-in is rejected alone (`load_directory_from` with `DefOrigin::User`), while a project collision still fails the project directory as before.
  - A user directory that cannot be scanned is one rejection.
- `resolve` returns `UnknownAgentType` ("no agent type 'x'; the known types are: …").
- The spawn types `general-purpose` (Coder) and `plan` (Planner) are now built-ins beside `explore` and `patch`, so every spawn resolves through one list. `patch` gains `write`, which it described but lacked.

`apps/rapid/src/agent_types.rs`: `spawn_inventory(root, trusted)` and `inventory_of`, used by both the spawn path and `rapid agents list`, so the two cannot disagree. A project directory that fails as a whole is one rejection; built-ins and the user's still load. The implementation registry moved here from `p9_commands`.

`apps/rapid/src/exec_tools.rs` (part b):
- `task_spawn` accepts any id in the definition alphabet.
- It resolves the type against the tools' inventory before anything is spawned or recorded; an unknown type is a `Failed` naming the known ones.
- The `type` argument's enum and description list every loaded type with its role and purpose, bounded to 2 KiB.
- `narrow_to_role_surface` maps each tool to its class (reads, edits, commands, the web, MCP; bookkeeping tools belong to every surface; an unknown tool to none). It removes the rest from the offered surface and denies them if called. It narrows only.
- The fixed `AGENT_TYPES` list is gone.

`apps/rapid/src/interactive.rs`:
- `configure_trusted_model_tools` builds the inventory once and gives it to the tools and the `LiveSubagentRunner`.
- The runner resolves the definition, and:
  - runs the child on `child_active_model`: the definition's model, resolved as `/model select` resolves one (managed lock included; an unknown id is refused, named), else the parent's; then its effort;
  - makes it write-capable iff the surface has `write`;
  - narrows a defined type's tools to its surface (built-ins keep exactly the surface they had);
  - appends its instructions to the child's system prompt under "## Agent type '<id>'", refusing if they do not fit;
  - sets `AgentSpec`'s role to the base role.

`rapid agents list` prints the user directory, each definition's source (`builtin`, `project:<path>`, `user:<path>`), and its model, effort, inputs and outputs.

| Criterion | Status | Evidence |
|---|---|---|
| AC-04: an overlay requesting a tool outside its base role is rejected with field-level remediation | done | `a_tool_outside_the_base_role_is_refused_naming_the_field_and_what_the_role_exposes`, `a_bad_field_says_which_and_what_would_do` |
| New fields, user directory, no shadowing | done | `a_definition_names_its_instructions_model_effort_and_io`, `the_user_directory_loads_below_the_project_and_never_shadows`, `every_spawn_type_is_a_builtin_and_an_unknown_one_lists_the_known` |
| A valid overlay appears in `rapid agents list` | done | `binary_a_defined_agent_type_sets_the_childs_model_effort_instructions_and_tools` (a project and a user definition listed with their sources, model and effort) |
| Its model and effort reach the child's request (scripted model) | done | the same binary test: against a scripted server, the child's own request carries `"model":"fast-wire"`, `"reasoning_effort":"low"`, the instructions, `repo_read`, and not `shell_exec`, `workspace_write` or `web_fetch`, while the parent's carries its own model and no effort. Also `a_childs_type_names_its_model_and_effort_or_keeps_its_parents` (resolution, an unknown model refused) |
| Unknown type is a typed refusal | done | `a_spawn_names_a_defined_type_or_is_refused_with_the_known_ones` (refused naming the known types, the runner never called; the offered enum lists the defined type); the binary test's second run (no child request for `wizard`) |
| Narrow-only tool surface | done | `a_defined_types_surface_narrows_the_tools_and_never_widens` |
| Revert cycle | done | Each of these fails its test: the definition's model ignored; its effort ignored; its instructions dropped; its surface not applied; resolution skipped; the offered surface not narrowed; user shadowing allowed; a user collision failing the directory; the remediation list dropped; the effort check removed. |

Remaining for SEAM-04-1, part c: the TUI's `/agents` lists definitions. It shows running and finished children only today.

Disclosed:
- A defined type's child built from the scripted-subagent test seam bypasses all of this, as it bypasses the live runner.
- `inputs` and `outputs` are parsed and listed, but not yet used: SEAM-04-5 checks outputs.
- The effort a definition names is not raised by the parent's reminder floor.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green. `cargo test --workspace --locked --no-fail-fast`: 4240 passed, 1 failed — `computer_observe_reports_the_typed_platform_gate_not_a_stub`, the known host-desktop flake. `pnpm` unaffected.

### Self-review of `cf1dbd7` — findings fixed in the SEAM-04-1 part c commit

The background review verified one high-severity defect by a running test, and found two medium, three low and a record overclaim. It checked these as clean:
- untrusted projects never reach the spawn inventory (their turns build no-op tools; headless exec configures only trusted);
- narrowing only intersects, and unknown tools are denied;
- a project definition reusing a built-in id fails its directory;
- detached spawns resolve the same way;
- the `AgentSpec` role change has no consumer on the run path;
- instructions are appended after the host prompt and bounded.

1. **High, verified.** A definition's `reasoning_effort` bypassed the managed `min_reasoning_effort` floor: it was set after the managed gate had run. A user-home file, a project file, or one the parent model wrote could run children below an administrator's floor. A definition naming `model` without an effort also lost the floor's re-application.

   `child_active_model` now applies the managed floor last, whatever the definition names. A policy that cannot be read refuses the child, as it refuses the parent.

   Test: `a_childs_type_names_its_model_and_effort_or_keeps_its_parents` puts a policy of `min_reasoning_effort = "high"` against `none`, `model` + `low`, and `model` alone: each is raised to high; `ultra` stays. Revert cycle: not raising fails it.

   The parent's *reminder* floor (a quality nudge, not policy) is still not applied to a child's type-named effort. Disclosed.
2. **Medium, plausible — disclosed, not changed.** The parent model can write `.rapidlm/agents/*.toml` in a trusted project and spawn the type next turn. With (1) fixed, what that buys is a configured model within the managed allowlist, an effort at or above the floor, instructions, and a narrower surface — no more than writing `AGENTS.md` or `.rapidlm/settings.json` already buys. Blocking the file tools there alone would be undone by `shell_exec`. A write guard for `.rapidlm/` as a whole is the right fix; it is recorded for SEAM-10.
3. **Medium, verified by reading.** A child on another model had only the parent's credential scrubbed from its output, so a write-capable child could read its own key back. `child_redaction` now registers both the parent's and the child's credential when they differ. Test: the same test scrubs both keys. Revert cycle: keeping only the inherited snapshot fails it.
4. **Low.** A definition with `exec` but no `write` got read-only tools, so it could never run a command. Commands can write, so such a type now works in its own worktree, as a writing one does, still narrowed to its surface (`child_needs_worktree`). Test: `a_type_that_writes_or_runs_commands_works_in_a_worktree`. Revert cycle: write-only fails it.
5. **Low — record correction.** "Built-ins keep exactly the surface they had" is true of `general-purpose`, `explore` and `plan`. `patch` was not a spawn type before `cf1dbd7`; it is now, and like every built-in it is not narrowed, so it runs with `shell_exec` and `web_fetch` beyond its declared read/write/git. `general-purpose` likewise keeps `web_fetch` beyond its declared surface. Built-ins' declared surfaces describe them; they do not bound them.
6. **Low — record correction.**
   - `rapid agents list` reads the current directory's `.rapidlm/agents` whether or not the project is trusted, while a spawn in an untrusted project reads none. "The two cannot disagree" holds for trusted projects only.
   - Resolving the user directory (`user_home_from`) creates `~/.rapidlm` if it is missing, on every trusted turn and every `list`, as the rest of the binary already does.
   - Lib tests that build a trusted turn read the developer's own `~/.rapidlm/agents`, so they are not hermetic against a user definition that fails its directory. Built-ins still load in that case.
7. **Low.** The offered `type` enum was unbounded, and types past the description's 2 KiB cut were offered with no description. The enum is now exactly the described types, and the description ends "and N more (rapid agents list)". Those types still spawn by name. Test: `the_offered_types_are_bounded_and_say_how_many_more`. Revert cycle: offering every id fails it. The schema still changes when a definition changes; that costs a prompt-cache miss, not correctness.

## SEAM-04-1 (part c) — Agent types in `/agents`

- `crates/tui`: `LocalUiEvent::SyncAgentTypes` projects rows (`AgentTypeRow`: the id `task_spawn` names, and the host's line) into `AppState::agent_types`, at most 64.
- The agents panel lists them under "agent types (task_spawn type=<id>)", below the session's agents.
  - They take at most half the height, or all of it with no agents.
  - They are cut to "… n more (rapid agents list)", never a bare header, and sanitised.
- `apps/rapid/src/interactive.rs`: `sync_agent_types` projects the same inventory the spawn resolves against. Each row gives the id, base role, source, purpose, and any model or effort; a refused definition file is a row saying why ("refused widen.toml: field `tools`: …"). It runs when the session starts and each time `/agents` opens, so a definition written mid-session shows.

| Criterion | Status | Evidence |
|---|---|---|
| AC-04: a valid overlay appears in `/agents` | done | `agents_show_selects_the_agent_that_was_named` (a definition written after the session started is listed when `/agents` opens, with its effort, beside the built-ins; an untrusted project's is not); `the_agents_panel_lists_the_agent_types_under_the_agents` (the section, sanitised, cut with a count); `a_refused_definition_file_is_a_row_saying_why` |
| Revert cycle | done | Each of these fails its test: no sync on open; trust ignored; rows unsanitised; no room kept for the count line. |

SEAM-04-1 is complete: AC-04 is evidenced by parts a–c. `inputs` and `outputs` are parsed and listed; SEAM-04-5 checks outputs.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green. `cargo test --workspace --locked --no-fail-fast`: 4244 passed, 1 failed — `computer_observe_reports_the_typed_platform_gate_not_a_stub`, the known host-desktop flake. `pnpm` unaffected.

### Self-review of `db2ffbc` — findings fixed

The background review verified two defects by failing tests, and found four more. It checked these as clean:
- the managed floor (applied last, idempotent, fails closed);
- refused rows (not selectable);
- types that spawn by name while unlisted (true locally; a provider enforcing enums would not let the model name one);
- the scan on `/agents` (bounded).

1. **High, verified — a regression of `db2ffbc`.** `child_redaction` dropped the parent's keychain key. A keychain model's plaintext is `None`; its key was only in the inherited snapshot, which was discarded whenever the child's key differed. Such a child could read the parent's key back unscrubbed. `child_redaction(parent, child)` now registers both models' keys, each read as a command could read it: the configured plaintext, or the OS keychain (`model_credential_secret`). The parent's own redaction uses the same helper, and the inherited snapshot is gone (it only ever held that key). Test: `a_childs_type_names_its_model_and_effort_or_keeps_its_parents` scrubs both keys. Revert cycle: dropping the parent's key fails it. The keychain read itself is not driven by a test; it is the same call the parent's redaction always used.
2. **Medium, verified by reading.** A child whose own model keeps its key in the keychain got no scrubbing of it (plaintext `None`). Fixed by the same change.
3. **Medium, verified.** The types section cut the selected agent's detail block — its blocker, evidence and merge lines — and at small heights hid the agents entirely. The agents now render first at the full height, and the types take only the rows they leave (`agent_panel_lines`); with no agents, the types have the panel. Test: `the_agent_types_never_cut_an_agents_detail` (at heights 4, 12 and 40 the agents' lines are exactly what they are without types). Revert cycle: giving the types half the height fails it.
4. **Medium — record correction, not changed.** A command-running type is, in effect, a writing type, with all that implies:
   - its worktree starts at `HEAD`, so it does not see the parent's uncommitted edits;
   - headless, whatever its commands leave is merged; interactive, its worktree is held until `/agents abandon`;
   - a project that is not a git repository refuses it;
   - `write_scope` does not bind its commands.

   These are the same consequences as for writing types. Running commands in the parent's live tree would give up isolation, so the worktree stays. The `cf1dbd7` item 4's "still narrowed to its surface" is true of its tool list, not of what its commands do.
5. **Low.** With more than 64 types, the panel's "… n more" counted only the 64 it held. The host now keeps 63 rows and a last one saying "… and N more types (rapid agents list)" with the true count. Test: `more_types_than_the_panel_holds_end_with_the_true_count`. Revert cycle: no cap row fails it.
6. **Low.** `sync_agent_types`'s doc comment had been inserted under `sync_memory_index`'s. Both are back on their own functions.

The unused `LiveSubagentRunner::redaction` field and `redaction_handle` (now test-only) went with item 1.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4247 passed, 0 failed; `pnpm` unaffected.

### Self-review of `c634f59` — findings fixed in the SEAM-04-2 commit

The background review found one defect verified by a scratch test, and several low ones. It checked alias names, dedupe, parent behaviour, the dropped snapshot and the panel arithmetic as clean.

1. **Medium, verified.** The count row did not work. The host's "… and N more" row was the 64th kept row, so a panel shorter than about 66 rows cut it and counted it as one more type: 134 types showed "… 46 more" where 116 were hidden. Now the host sends every row, and the projection keeps at most 64 plus the total it was sent (`AppState::agent_types_total`); the panel counts from the total. Test: `the_types_count_is_every_type_synced_not_those_kept` (134 types at height 20: 18 rows, then "… 116 more"); `every_type_reaches_the_panel_which_keeps_the_count` replaces the host-side cap test. Revert cycle: counting only kept rows fails it.
2. **Low.** When the agents filled the panel, the types vanished with no hint. One spare row now says "agent types: N (rapid agents list)".
3. **Low — record correction.** "Never a line of an agent's detail" meant the types take no line the agents would have painted. The agents' own render still truncates its selected detail without a marker when there are many agents, as before.
4. **Low.** Every spawn read the keychain up to three times. The parent's key is now read once per turn (`LiveSubagentRunner::parent_secret`), and a child on the same profile does not read it again. A keychain-backed child on another profile still reads its own key once more, outside the model builder.
5. **Low.** A stale comment naming the removed `redaction` field was rewritten, and the count wording no longer calls refused rows "types".

## SEAM-04-2 — Messages to a running subagent

Contract restated (ADR 0023 §1–2):
- A message to a running child is recorded (`agent.mail.sent`) before it is queued.
- *interject* interrupts what the child waits on; *steer* is heard at its next step boundary; *queue* is heard after its run completes.
- Each delivery is recorded (`agent.mail.delivered`, with where), and the child's transcript labels the mode.
- A message that cannot be delivered is `agent.mail.dropped` with a reason.

`crates/agent-runtime/src/turn.rs`:
- `ToolDriver::interject_flag` (default `None`).
- When a driver has one, each model step runs under a token that trips on the turn's cancellation or the flag (`step_interruptible`, a scoped watcher). A step the flag interrupts is closed (`model.failed`) and run again, after `drain_notifications` has delivered the message and lowered the flag.
- At most 16 steps a turn are interrupted this way (`MAX_INTERJECTED_STEPS`), so a driver that never lowers its flag cannot hold a turn in a loop.
- A model that panics does not hang the turn: the watcher is stopped by a drop guard. The first version stopped it only after a normal return, and a revert cycle that made the test model panic hung for hours.

`crates/event-ledger`: `agent.mail.delivered` (family agent), with the SDK wire catalog (114 kinds), the regenerated types, the daemon's pinned schema hash, and `docs/reference/event-catalog.md`.

`apps/rapid/src/exec_tools.rs`:
- `Inbox` — the session's, one box per running child, at most 16 waiting (`MAX_PENDING_MAIL`).
- `MailDelivery`, `AgentMail` (body at most 16 KiB) and `MailRefusal` (`unknown_agent`, `bounded`).
- `ChildMailbox`: a child's tools deliver its waiting interjections and steers from `drain_notifications`, each as a synthetic `agent_mail` exchange whose result is the labelled message "[steer from user → <agent>] …", and record each as delivered. `interject_flag` is its box's flag.
- `AgentEvents` gains `mail_delivered` and `mail_dropped`.

`apps/rapid/src/interactive.rs`:
- `SessionShared::inbox` is shared into every turn's tools and each `LiveSubagentRunner`. The runner opens a child's box as it starts.
- A child that completes continues with its queued messages, at most 4 times (`queued_continuation`: its task, its report, the labelled messages). Each continuation runs on a freshly built model, and each message is recorded as delivered `after_completion`.
- When the child ends, its box is closed, and whatever was never delivered is recorded dropped (`terminal_without_continue`).
- `/agents send <id> [--interject|--steer|--queue] <message>` (steer by default) records `agent.mail.sent`, `from: user`, then posts. A refusal is recorded `agent.mail.dropped` with its reason — `terminal_without_continue` for a child the session saw end.

| Criterion | Status | Evidence |
|---|---|---|
| AC-01: interject — when, event, label | done | `an_interjection_interrupts_the_step_it_arrives_in` (a real child turn: the message posted during step 1 interrupts it; step 2 hears the labelled message; delivered at `wait`); `an_interjection_interrupts_the_step_in_flight_and_the_next_step_hears_it` (turn loop) |
| AC-01: steer | done | `a_steer_is_heard_at_the_next_step_boundary` (step 1 runs to its end; step 2 hears it; delivered at `turn_boundary`) |
| AC-01: queue | done | `a_queued_message_waits_for_the_run_to_complete` (never heard or delivered during the run; still waiting after it) |
| Recorded before queued; dropped with a reason | done | `agents_send_records_the_message_first_and_a_drop_with_why`, `a_message_to_no_running_child_or_a_full_box_is_refused` |
| Cancellation unchanged; a panicking model does not hang | done | `with_no_interjection_a_cancelled_step_still_ends_the_turn`, `a_model_that_panics_mid_step_does_not_hang_the_turn` |
| Revert cycle | done | Each of these fails its test: no redo; delivery skipped; queued taken early; flag never raised; no bound; the wrong record kind; the drop reason inverted; no drop guard. |

Deviations and gaps:
- **A message reaches the child as a labelled synthetic exchange, not a user-role message.** It is the path a finished job's notice takes. History holds only exchanges, and a user-role variant would reach every model encoder.
- **The runner's queued-continuation loop is not driven by a test.** It needs a live child model; the inbox side of queueing is tested.
- **An interjection interrupts a model step, not a tool call in flight.** It is delivered at the step boundary after the tool returns.
- **A continuation is a fresh turn seeded with the task and the report, not the child's full history.** SEAM-04-3's lineage replaces it.
- **Only the user sends, from the TUI.** No parent-model tool, and no daemon or ACP sender.

The `/agents` usage line first read `… [id] | /agents send …`, a shape the help synthesis does not understand, and `every_synthesized_invocation_parses` failed on it. It is now `/agents [list|show|pause|resume|sleep|cancel|terminate|send <id> <message>] [id]`, and a bare `/agents send` parses and is answered with the full usage, mode flags included.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `pnpm generate:check`, `typecheck`, `test` green. `cargo test --workspace --locked --no-fail-fast`: 4254 passed, 2 failed — `computer_observe_reports_the_typed_platform_gate_not_a_stub` (the host-desktop flake) and `shell_exec_runs_argv_inside_the_root_with_bounded_output` (the host's 4 s start of a new script; see the SEAM-03-5 part a record). Both ran under another project's concurrent test runs on this machine. The run before this one also failed `no_line_is_recorded_after_the_monitors_end` under that load; it passed twice alone.

### Self-review of `6bf390a` — findings fixed

The background review was by reading only: it did not extract the commit or run tests.

1. **Medium.** An interrupted step and the step run again shared a request id: the record read "requested m1, failed m1, requested m1", as if a failed request restarted. The abandoned request now counts as a step, so the next has its own id — and it counts against the turn's step budget, which it spent. Test: `an_interjection_interrupts_the_step_in_flight_and_the_next_step_hears_it` asserts two distinct request ids. Revert cycle: not counting it fails it.
2. **Medium.** A queued message was recorded delivered before its continuation's model and context were built. If either failed, the record said delivered and the child never heard it. It is now recorded delivered only once the continuation that hears it starts.
3. **Medium.** A continuation that could not start turned the child's successful report into a failure. Now its report stands, and the messages are recorded dropped (`terminal_without_continue`).
4. **Medium.** Each continuation's task compounded every earlier round. It is now the original task, the last report, and the new messages.
5. **Low-medium.** A cancel between continuations could replace a good report with an interrupted one. The loop now stops before a continuation once the turn is cancelled.
6. **Not applicable.** "Grandchildren are unreachable through `/agents send`": children cannot spawn (depth 1), so there are none.
7. **Low, not changed.** Each step of a child with a mailbox runs a watcher that polls every 10 ms: up to 10 ms of latency at a step's end, and of detection lag.
8. **Low.** After 16 interrupted steps (now also bounded by the step budget), interjections behave as steers; and a tool call in flight is never interrupted. `/agents send --interject` now says "it hears it as soon as its current model step can be interrupted, or at its next step", not "now".
9. **Low, recorded.**
   - A steer arriving during a child's last step is recorded dropped, although the send said it would be heard.
   - The body is recorded as typed, unredacted.
   - The terminal check reads the UI's projection, which can lag the record, so a just-ended child can read `unknown_agent`.

The runner's changes (items 2–5) are not driven by a test: the loop needs a live child model, as the SEAM-04-2 record says.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green. `cargo test --workspace --locked --no-fail-fast`: 4255 passed, 1 failed — `shell_exec_runs_argv_inside_the_root_with_bounded_output`, the host's slow start of a new script.

### Self-review of `814450c`

The background review, by reading, found no correctness defects. It confirmed:
- every taken message is delivered or dropped;
- no stale task survives;
- the cancel check and the wording are right.

Recorded as behaviour changes:
- **Steps.** An interrupted step counts against `max_model_steps`, so a turn interrupted on its last allowed step now stops as budget-exhausted, and with a budget of one step any interjection ends it. `usage.model_steps` now counts requests issued, abandoned ones included, not completed exchanges. The only reader outside the turn's own tests is a goal-driver test with no interjection.
- **Continuations.** A continuation that cannot start now returns the child's earlier report instead of an error. A later round sees earlier rounds' messages only through the child's report.

## SEAM-04-3 (part a) — A child's history is kept, and continuations resume from it

Contract restated (ADR 0023 §3): a message to a finished child starts a new turn for the same `AgentId`, seeded with its recorded history plus the message; the lineage is recorded and visible in `/agents`. Part a keeps the history and makes the queued continuation (SEAM-04-2) resume from it. Part b continues a child that has already ended.

- `crates/agent-runtime/src/turn.rs`: `ToolDriver::observe_history` (default no-op) is called with every exchange the loop adds to a turn's history — each tool step and each drained notice or message — in order.
- `apps/rapid/src/exec_tools.rs`:
  - `ChildHistory` is a child's history across its runs, bounded to 256 exchanges (`MAX_CHILD_HISTORY`, oldest dropped). A child's tools record into it (`record_history`).
  - `report_exchange` holds what a finished run answered, which history does not.
- `apps/rapid/src/interactive.rs`: the runner keeps each child's history. A queued continuation now runs `run_live_exec_seeded` on the original task, seeded with that history, the run's report and the labelled messages. It no longer runs a fresh turn whose task restated the report; `queued_continuation` is gone.

| Criterion | Status | Evidence |
|---|---|---|
| History observed in order | done | `a_driver_observes_every_exchange_the_history_gains_in_order` |
| Kept, bounded, and a continuation resumes from it | done | `a_childs_history_is_kept_and_a_continuation_resumes_from_it` (a real run's glob call is kept; the seeded second run sees it, the report, then the message); `a_childs_kept_history_is_bounded` |
| Revert cycle | done | Each of these fails its test: exchanges not recorded; no bound; the loop not reporting. |

The model's own text between tool calls is not history (only exchanges are), so a continuation sees its calls, their results and its final report, not its intermediate prose.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4259 passed, 0 failed; `pnpm` unaffected.

### Self-review of `3217329` — findings fixed

The background review, a static read, found:

1. **Medium.** The kept history had a count cap but no byte cap: 256 exchanges of large reads could overflow a continuation's context. It is now bounded by bytes too, at the same bound as a suspended turn's history (`MAX_SUSPENSION_HISTORY_BYTES`, 256 KiB). Whole exchanges are dropped from the front, and the snapshot opens with "N earlier steps omitted", as a suspension's history does. Test: `a_childs_kept_history_is_bounded` (six 100 KiB results keep only what fits, the newest last). Revert cycle: no byte bound fails it.
2. **Verified sound.** The bound drops whole exchanges, so a result never loses its call.
3. **Low, unverified.** `agent_report`, `agent_history` and `agent_mail` are tools the child is not offered, appearing in its history. `agent_mail` and `background_jobs` notices already took this shape through every encoder; that each provider accepts calls to unoffered tools in history is not tested.
4. **Not a defect.** Mail call ids repeating: each message has its own id, and is delivered once — drained, queued or dropped.
5. **Info.** The kept history holds what the child's model already saw, as redacted; it lives in memory only.
7. **Record correction.** Each continuation's history grows. The bound is now in bytes as well as count, not "compounding avoided".

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4259 passed, 0 failed.

## SEAM-04-3 (part b) — A message to a completed child continues it

`apps/rapid/src/exec_tools.rs`:
- `SubagentRunner::resume(from, agent, mail, cancel)` — refuses by default.
- `Inbox::park` / `take_finished`: a child whose spawn completed is kept continuable with its runner, at most 16 (`MAX_CONTINUABLE_CHILDREN`).
- `continue_finished` runs a continuation on a thread of its own, as a **new** agent id: the session projection refuses a terminal agent returning to running, and ADR 0023 §3 calls the continuation a superseding revision. It is:
  - registered, so `/agents cancel` stops it;
  - recorded `agent.spawned` with `continued_from` and `parent_id` naming the child it continues;
  - recorded as the message delivered `after_completion`;
  - on its end, recorded finished, with its report told as a notice (`notification.recorded`, source `agent <id>`, outcome `continued` — no turn waits on it);
  - kept continuable in turn.
- `AgentEvents` gains `continued` and `continued_report`.

`apps/rapid/src/interactive.rs`:
- `LiveSubagentRunner` keeps each completed child's history, task, type and report (`Resumable`, at most 16). `run` became `run_seeded`, whose first run is seeded with a kept history.
- `resume` seeds it with that history, the report, then the labelled message.
- A type that writes or runs commands is refused: its worktree was settled when it ended.
- `/agents send` to a child that has ended and was kept continues it ("continuing it as agent <new> … its report arrives as a notice") instead of recording a drop.

| Criterion | Status | Evidence |
|---|---|---|
| AC-02: messaging a completed child continues it in the same lineage | done | `a_message_to_a_completed_child_continues_it_as_a_new_one_in_its_lineage` (a new id; resumed with the message; cancellable while it runs; recorded continued, delivered, finished, reported; kept in turn; a child nobody kept is not continued); `agents_send_records_the_message_first_and_a_drop_with_why` (end to end in a session: `agent.spawned` with `continued_from`, the report as a notice, no drop, and `/agents` shows it under the child it continues) |
| The original report is unchanged | done | The continuation is a new agent; the first one's records are untouched. |
| Revert cycle | done | Each of these fails its test: not kept in turn; not registered; no `continued_from`; no `parent_id`. |

Not covered:
- `LiveSubagentRunner::resume` with a live model: its seeding is `run_seeded` and the history path already tested in part a, but it is not driven end to end.
- Continuing a writing or command-running type is refused.
- A detached (`background: true`) child that completed is not parked.
- Kept children live in memory for the session: after a restart, a message to one is dropped (`terminal_without_continue`).

SEAM-04-3 is complete: parts a and b.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4260 passed, 0 failed; `pnpm` unaffected.

### Self-review of `3f0b8ec` (and `645ec77`) — findings fixed in the SEAM-04-4 commit

The background review, by reading, found:

1. **High.** A continuation was recorded (`agent.spawned`, `continued_from`) and its message marked delivered before the runner said whether it could continue — a writing type, or nothing kept — so the record showed a child spawned and failed. `SubagentRunner::can_resume` now answers first, and `continue_finished` returns the refusal without recording anything. The child stays kept, since `take_finished` now runs only once continuing is certain. `/agents send` then records the message dropped (`terminal_without_continue`) and says why. Test: `a_child_that_cannot_continue_is_refused_before_anything_is_recorded`. Revert cycle: no check fails it.
2. **High.** The continuation's thread was never joined: a session that closed mid-run left the child `running` in the record, and its records went to a client being dropped. The inbox now tracks continuation threads, and the session's end (`SessionLoop::run`, around `run_session`) stops each through the registry and waits for its end to be recorded (`settle_continuations`, at most the job settle each). Test: `the_sessions_end_stops_a_continuation_and_records_its_end`. Revert cycle: not cancelling fails it.
3. **High — recorded, not changed.** Ctrl-C interrupts a turn; a continuation is not a turn, so it does not reach it. `/agents cancel` does, and so does the session's end (2).
4. **Medium.** The inbox parked every completed child, but the runner kept only effective successes, up to 16 per turn. A parked child with nothing kept then failed after being recorded. A child is now parked only when its runner can continue it (`can_resume`), and (1) settles the rest before recording.
5. **Medium — recorded.** A parked runner holds its turn's configuration, parent key and kept histories for the session's life, up to 16 runners. A continuation counts against its original turn's budget counters and write locks.
6. **Medium — recorded.** A continuation's report reaches the notice as the child wrote it. Its tools' outputs are redacted, but its own text is not scanned again.
7. **Low.** `parent_id` naming a child absent from the projection is accepted: the projection does not require a parent to exist.
8. **Low.** A refused `/agents send` no longer loses the child (1).
9. **Low, `645ec77` — record correction.**
   - One exchange larger than the byte bound is kept alone: the bound drops from the front but always keeps the newest.
   - Its size is counted over call fields and each result's serialized size, the way the suspension path counts; not the whole exchange serialized.

## SEAM-04-4 — Admission: spawns above the ceiling wait in order

Contract restated (ADR 0023 §4): spawns above a concurrency ceiling wait in order for a slot rather than failing, and the queue is visible in the tool result and `/agents`. `managed.max_concurrent_subagents` narrows the built-in ceiling. The per-turn spawn budget is unchanged.

`apps/rapid/src/exec_tools.rs`:
- `SubagentRegistry` gains an admission queue: a ceiling (`DEFAULT_MAX_CONCURRENT_SUBAGENTS` = 4), the running count, and a FIFO of tickets.
  - `enqueue` returns a ticket and how many are ahead.
  - `admit` waits until the ticket is first in line and a slot is free, or until cancelled, which removes it from the queue. The returned `AdmissionSlot` frees its slot when dropped.
  - `narrow_concurrency` only lowers the ceiling.
- A foreground spawn takes its ticket at the call and runs once admitted. A detached spawn no longer fails at 4 running: its ticket is taken at the call, and its result says "detached subagent queued: N ahead of it, it starts when a slot frees"; its worker waits to be admitted.
- A queued child is recorded `queued` ("waiting for a subagent slot (N ahead)") and `running` once admitted (`AgentEvents::queued` / `admitted`), so `/agents` shows it waiting.

`apps/rapid/src/managed_config.rs`: `max_concurrent_subagents` (a positive integer) is applied through `ExecTools::narrow_subagent_concurrency`, which narrows the session's registry at each spawn. User and project configuration have no key to raise it.

| Criterion | Status | Evidence |
|---|---|---|
| AC-03: twice the ceiling queues in order rather than erroring | done | `twice_the_ceiling_queues_in_order_rather_than_erroring` (8 detached spawns: 4 started, 4 "queued: 1…4 ahead"; the queued ones start in call order; never more than 4 at once) |
| A managed ceiling cannot be exceeded | done | `a_managed_concurrency_ceiling_narrows_and_nothing_raises_it` (at 2, at most 2 run; a larger ceiling later does not raise it); `a_managed_policy_narrows_how_many_subagents_run_at_once` (the key parses; zero is refused) |
| Revert cycle | done | Each of these fails its test: admission out of order; no ceiling; narrowing that raises; the key not parsed. |

Also recorded:
- `detached_spawn_enforces_the_session_concurrency_bound`, which asserted the old refusal, is replaced by the test above.
- A child is recorded `running` at its spawn and then `queued` when it must wait; the spawn record does not yet carry the queued state.
- Continuations of finished children (SEAM-04-3) do not pass through admission.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4264 passed, 0 failed; `pnpm` unaffected.

### Self-review of `9180280` — findings fixed

The background review, by reading, found:

1. **Critical.** An admission ticket dropped without being admitted stayed in the queue. A detached spawn whose job registration failed after it enqueued did exactly that; so would a panic between enqueue and admit, which is finding 2. The next ticket could then never be first in line, so every later spawn in the session waited forever. `AdmissionTicket` now leaves the queue when dropped unadmitted, and wakes the others. Test: `a_ticket_dropped_without_admission_never_holds_the_line` (bounded by a canceller). Revert cycle: no drop guard makes it wait out its bound and fail.
2. **High.** A panic between enqueue and admit on the foreground path leaked a ticket. Covered by (1).
3. **High, plausible — recorded.** A foreground spawn now waits behind running detached children. If one of them could not finish until the parent turn does something, the spawn would wait until cancelled. A detached child needs nothing from its parent turn: it runs to its end on its own thread, under its own lattice. Ctrl-C or `/agents cancel` ends the wait.
4. **Medium.** Detached children, running or queued, were unbounded once the refusal went: each queued one holds a thread and a job row. `claim_detached` bounds them again, at 16 (`MAX_DETACHED_SUBAGENTS`), a refusal naming the bound; admission still bounds how many run. Test: `detached_children_running_or_queued_are_bounded`. Revert cycle: no bound fails it.
5. **Medium, cosmetic — recorded.** A queued child is recorded started, then queued, then running. `registry.running()` counts it while it waits, which is what lets `/agents cancel` reach it.
6. **Low.** The `ahead` count is right while no ticket leaks, which (1) now ensures. A later narrowing leaves an earlier report stale.
7. **Verified sound.** The Condvar predicate loop, the timed wait, and the wakes on pop, cancel and drop.
8. **Low — record correction.** `settle_continuations` runs after `run_session` has consumed the session loop, whose client handle is dropped by then. Continuations write through their own client clones, so their ends are still recorded; the comment's "before the ledger client goes" is true of those clones only.
9. **Low — recorded.** Settling waits up to the job settle per continuation, in turn. One still running at the deadline is left, its end unrecorded.
10. **Verified sound.** `can_resume` races: the loser reads as unknown, and there is no double resume.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green. `cargo test --workspace --locked --no-fail-fast`: 4264 passed, 2 failed — `shell_exec_runs_argv_inside_the_root_with_bounded_output` (the host's slow start of a new script) and `acp_cli::a_disconnect_while_the_turn_runs_interrupts_it`, which passed twice alone.

### Self-review of `945a500`

The background review, by reading, found no defect in the ticket drop guard: no double removal, no lock held across the drop, claims paired on every path. It found one weakness, now fixed:
- **Medium.** A detached worker's claim on the 16 was released by an explicit call. A panic in the worker before it — the runner is caught, hooks and settling are not — leaked the claim for good, and 16 leaks would refuse every detached spawn. The worker now holds a `DetachedClaim` that releases on drop, exactly once. Test: `a_detached_claim_is_released_once_even_when_its_worker_panics`. Revert cycle: a drop that does nothing fails it.

Record corrections:
- `9180280` item 2 ("covered by (1)") covered the ticket, not this claim.
- Item 7's "wakes on drop" were new in that commit, not previously reviewed.
- The field `detached_running` counts running and queued children.
- `shell_exec_runs_argv_inside_the_root_with_bounded_output` was attributed to the host without a rerun: its cause is the one measured in the SEAM-03-5 part a record, not re-shown.

## SEAM-04-5 — Declared outputs checked at completion; the wait on a detached child

Contract restated (ADR 0023 §6–7):
- A declared output missing from a child's result is a typed integration failure, never accepted.
- Waits on detached children have a ceiling, one hour by default, whose expiry reports the child still running and leaves it alive.

`apps/rapid/src/interactive.rs`:
- A child whose type declares outputs is told, after its host prompt: "End your final report with one line per declared output, `<name>: <value>`".
- At completion, `missing_outputs` finds the declared names no line carries with a value. A report missing any has status `integration_failed` and a blocker per missing output ("missing declared output: verdict").
- That status is not `succeeded`, so the spawn path treats the child as incomplete:
  - its end is recorded failed (`integration_failed`);
  - its worktree changes are not integrated;
  - the parent reads the status and the blockers.

| Criterion | Status | Evidence |
|---|---|---|
| AC-05: a missing declared output is a typed failure, never accepted | done | `binary_a_child_missing_a_declared_output_is_an_integration_failure` (against a scripted server: the child is told to end with `verdict`; it does not; the parent's next request carries `integration_failed` and "missing declared output: verdict"); `a_declared_output_is_carried_only_by_a_line_with_its_value` |
| A wait past the ceiling reports still running and leaves the child alive | done | `a_wait_on_a_detached_child_past_the_ceiling_says_still_running_and_leaves_it_alive`. A detached child is a job, so `job_status` / `job_output` waits on it take the job wait ceiling (SEAM-03-5 part b). |
| Revert cycle | done | Each of these fails its test: the check skipped; an empty value accepted. |

Deviations:
- **The status is not a new `SubagentEnd::IntegrationFailed` variant.** It is a report status (`integration_failed`) with the missing outputs as blockers. The report already crosses to the parent and to the record typed, and a new end variant would reach every `AgentEvents` implementor.
- **The ceiling is the job wait's, `[job] wait_ceiling`, not a separate `subagent.wait_ceiling`.** A detached child is waited on only through its job.
- **A foreground `task_spawn` still has no ceiling** (recorded in SEAM-03-5 part b).

SEAM-04 is complete: SEAM-04-1 to 04-5, AC-01 to AC-05.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green. `cargo test --workspace --locked --no-fail-fast`: 4268 passed, 2 failed — `shell_exec_runs_argv_inside_the_root_with_bounded_output` and `shell_exec_scrubs_a_registered_secret_from_captured_output`, both of which run a freshly written script; both passed twice alone, in about a second.

### Self-review of `fdb1db8` — findings fixed

The background review, by reading, found:

1. **High — a defect, and an overclaim in the SEAM-04-5 record.** `ChildEnd::of` counted the "effectively successful" child — tool calls, then an empty final message — as completed without looking at its status. Such a child can never carry a declared output, so it is `integration_failed`, yet its worktree was applied under headless auto-integration and `/agents` showed it succeeded. The record's "its worktree changes are not integrated" was false for that case. An `integration_failed` report is now incomplete however the turn ended. Test: `an_integration_failure_is_never_completed_even_after_an_empty_answer`. Revert cycle: without the arm it fails.
2. **Verified sound.** The check covers continuations: `resume` ends in `run_seeded`, where it runs.
3. **Minor.** A declared output's name matched case-sensitively, so a model writing `Verdict:` failed as missing `verdict`. It now matches in any case; `a_declared_output_is_carried_only_by_a_line_with_its_value` covers it. Still lenient, recorded:
   - a matching line inside a code fence counts;
   - the line need not be at the end.
4. **Verified sound.** `DetachedClaim` releases exactly once, and the registration-failure path releases explicitly.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4271 passed, 0 failed.

## SEAM-05-1 — Plan mode is the lattice's mode; the proposal type

Contract restated (ADR 0024 §1–2):
- `/plan` and `rapid exec --plan` make `PermissionMode::Plan` the effective mode under the managed ceiling.
- The lattice carves out the plan files, so the tools' plan flag becomes a projection of the lattice rather than a second gate.
- `protocol::plan::PlanProposal` v1 comes with a fixture.
- Validation: a write in plan mode is refused by `PermissionLattice::evaluate` (policy, not prompt); the fixture round-trips; the cron path's forced Plan mode is unchanged.

`apps/rapid/src/permissions.rs`:
- In Plan mode, `evaluate` allows exactly one kind of write: a file edit to a plan file (`is_plan_file`: `.rapidlm/plan.md`, or a `.md` under `.rapidlm/plans/`, with no `..` or empty segment), reason `PlanFileCarveOut`. A deny rule matching it still wins, as `DenyRule`.
- Every other non-read call is `PlanModeDeny`, before any allow rule, as before.
- `PermissionLattice::in_plan_mode` is the same lattice — rules, grants, ceilings — in Plan mode.

`apps/rapid/src/exec_tools.rs`: the plan tools' second gate is gone. While `plan_enter` holds, each call is judged by `permissions.in_plan_mode()`, so plan mode has one set of rules and one carve-out.

`apps/rapid/src/interactive.rs`:
- `/plan` sets the session's permission-mode override to Plan (sticky across turns), and `/plan cancel` clears it. It is the override ACP's `session/set_mode` sets, still narrowed by the managed ceiling, under which Plan, the strictest mode, always fits.
- `rapid exec --plan` forces Plan (`exec_forced_mode`), over any wider mode a caller passed. The cron path's own forced Plan is unchanged.

`crates/protocol/src/plan.rs`: `PlanProposal` has:
- `schema: "rapidlm.plan_proposal"`, `version: 1`, `title`, `summary`;
- `steps`, each a `key`, `kind` (agent, process, verification, human), `label`, `depends_on`, and a `prompt`, `command`, `question` or `watch`;
- `files_expected_to_change`, `verification`, `risks`, `open_questions`, `base_revision`.

Unknown fields are refused at parse. `validate` checks the marker and version, the bounds (64 steps, 64 entries per list, 8 KiB per text), unique keys, known dependencies, and each step's payload for its kind. The fixture is `crates/protocol/tests/fixtures/plan/v1/plan_proposal.json`.

| Criterion | Status | Evidence |
|---|---|---|
| AC-01: a write in plan mode is refused by the lattice | done | `plan_mode_allows_only_the_plan_file_and_only_by_a_file_edit` (policy layer: plan paths allowed; `..`, other extensions, other paths, and a command naming the plan all refused; a deny rule wins); `plan_mode_denies_writes_even_when_an_allow_rule_matches`; `plan_enters_the_lattices_plan_mode_so_a_named_plan_may_be_written` |
| `/plan`, `--plan` | done | `agents_show_selects_the_agent_that_was_named` (`/plan` then `/plan cancel` in a session); the command parse test; `exec_plan_is_a_flag_and_plan_is_the_lattices_mode` (including the cron path's Plan unchanged) |
| The proposal fixture round-trips | done | `plan_proposal_v1_fixture_matches_wire_contract`, `a_plan_proposal_is_refused_when_its_shape_is_wrong` |
| Revert cycle | done | Each of these fails its test: the `..` guard removed; the carve-out not limited to file edits; the plan flag not judged by the lattice; `--plan` ignored; `/plan` not setting the mode. |

`plan_exit` still reads `.rapidlm/plan.md` and says "Plan accepted". Submitting a proposal for approval is SEAM-05-2.

`bare_help_marks_the_commands_this_build_cannot_perform` failed in the first full run: `/plan` joined the catalog, so `/playbook` scrolled out of its 24-row frame. It now checks `/handoff`, also marked unavailable and still in view; the whole catalog's markers stay asserted in `the_rendered_help_marks_only_what_is_missing`.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green. `cargo test --workspace --locked --no-fail-fast`: 4274 passed, 2 failed — that help test (fixed as above, then passing) and `computer_observe_reports_the_typed_platform_gate_not_a_stub` (the host-desktop flake).

### Self-review of `3c2397a` — findings fixed

The background review, by reading, found:

1. **Medium.** The plan carve-out checked only the path string, and the workspace resolver refuses a symlink only when it leads outside the root. A committed `.rapidlm/plans -> ../src`, or a linked plan file, let a plan-mode write overwrite source files. A plan-file write whose path has any symlinked component is now refused (`has_symlink_component`, checked where the tools apply the lattice's `PlanFileCarveOut`). Test: `a_plan_path_reached_through_a_symlink_is_refused` (a linked directory and a linked file, on Unix). Revert cycle: no check fails it.
2. **Low.** An ask rule on a plan path was skipped. It now asks; a deny rule still wins first. Test: `plan_mode_allows_only_the_plan_file_and_only_by_a_file_edit`. Revert cycle: skipping ask fails it.
3. **Verified sound.** The admin denied-tools check and both write-scope ceilings run before the carve-out, and the plan flag's evaluation only narrows.
4. **Verified sound.** No regression against the old plan flag: `plan_enter`/`plan_exit` pass; `todo_write` is refused, as before; a patch to `.rapidlm/plan.md` is allowed, as before.
5. **Verified sound.** Path spellings fail closed: absolute paths, `./`, case, trailing dot or space, lookalikes. Windows device names and alternate streams pass the string check but cannot leave the workspace.
6. **Low — recorded.** `/plan` replaces any mode override — one ACP `set_mode` put there, say — and `/plan cancel` clears it to none, not the earlier mode.
7. **Info.** `rapid exec --plan` forces Plan over any wider mode.
8. **Medium.** `PlanProposal::validate` accepted dependency cycles, including a step depending on itself. It now refuses a loop (`Cycle`, naming the steps in it) and a self or repeated dependency (`InvalidDependency`).
9. **Low.** Now also refused:
   - a key that is empty, padded or over 64 bytes (`InvalidKey`);
   - an empty label;
   - an empty list entry;
   - a payload the step's kind does not take (`ExtraPayload`) — an agent step takes a prompt, process and verification steps a command (a process step may also watch), a human step a question.

   Test: `a_plan_proposal_is_refused_when_its_shape_is_wrong`. Revert cycles: no cycle check fails it; extra payloads allowed fails it.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green. `cargo test --workspace --locked --no-fail-fast`: 4276 passed, 1 failed — `shell_exec_runs_argv_inside_the_root_with_bounded_output`, which passed when rerun alone.

### Self-review of `9613fef`

The background review, by reading, confirmed sound:
- the symlink check — every dispatch path calls it, a dangling link counts, and the check-then-write gap is residual, since plan mode runs no call that could make a link;
- the ask branch — it becomes a typed denial when headless;
- the cycle check — correct, and trivial at 64 steps;
- the record.

One defect, fixed:
- **Medium.** The `ExtraPayload` rule refused `watch` on agent, verification and human steps. ADR 0024 §2 keeps `watch` apart from the prompt, command or question choice. The step shape is the workflow's, where any kind may watch: `watch` is its evidence scope. `watch` is now allowed on every kind; only prompt, command and question are tied to a kind. Test: `a_plan_proposal_is_refused_when_its_shape_is_wrong` accepts a verification step and a human step that watch. Revert cycle: refusing a human step's watch fails it. Record correction: `3c2397a` item 9's payload rule for `watch` was wrong.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test -p protocol` 94 passed, 0 failed (the change is confined to the protocol crate).

## SEAM-05-2 — A plan is submitted for approval, revised, rejected or approved

Contract restated (ADR 0024 §3–4, §6):
- `plan_exit` stores the proposal as an artifact, renders `.rapidlm/plans/…md`, records `plan.proposed`, and raises the approval.
- An edit before approval records `plan.revised {supersedes}` and leaves the earlier artifact and file untouched.
- A rejection records `plan.rejected {reason}`.
- Headless `--plan` prints the proposal and exits NeedsApproval.

`crates/event-ledger`: a `plan` family — `plan.proposed`, `plan.revised`, `plan.rejected`, `plan.approved`. The SDK wire catalog (118 kinds), the regenerated types and the daemon's pinned hash come with it.

`apps/rapid/src/exec_tools.rs`:
- `plan_exit` with a proposal in its arguments — title, summary, steps, files, verification, risks, open questions — is a submission:
  - The host adds the schema, version and `base_revision` (the workspace digest), then parses and validates.
  - The proposal's JSON is stored in the project's content-addressed artifact store (`.rapidlm/artifacts`).
  - `.rapidlm/plans/<plan>.r<revision>.md` is written new, never over an existing file and never through a symlink, so every revision keeps its own file.
  - The submission is recorded (`plan.proposed`, or `plan.revised` superseding the pending revision), and an approval is raised with source `plan:<plan>#r<revision>`. Its diff is the rendered proposal and its scope the files expected to change. The turn waits.
- A second submission while one is pending is its next revision.
- On the approval's resume, `plan_exit` does not submit again: it records `plan.approved`, only for the newest revision — an older one is a typed refusal naming the newest — and leaves plan mode.
- A bare `plan_exit` still returns `.rapidlm/plan.md` as before. The tool's description and schema name the proposal's fields.

`apps/rapid/src/interactive.rs`:
- `LedgerPlanEvents` reads the pending plan back from the session's `plan.*` records and records submissions and approvals. An approval ends the session's `/plan` mode (ADR 0024 §6).
- `/approvals deny <n> [reason]` on a plan records `plan.rejected` with the reason.
- Headless, a run whose waiting call is a plan's prints the rendered proposal to stdout, says on stderr how to approve or reject it, and exits NeedsApproval (10). The approval is recorded through the ledger sink a hook's ask uses. `--plan` keeps that sink; the cron path's forced Plan mode still has none, as before.

| Criterion | Status | Evidence |
|---|---|---|
| Submission: artifact, file, `plan.proposed`, the approval | done | `a_submitted_plan_waits_for_approval_and_a_revision_leaves_the_first_intact` (a scripted session: the turn waits; the record, the artifact, revision 1's file, the approval with its plan source) |
| An edit is a new revision; the old file and artifact are byte-identical | done | the same test: `plan.revised {supersedes: 1}`, a new artifact, revision 1's file byte-identical |
| Rejection records why | done | the same test: `/approvals deny <n> too risky this week` records `plan.rejected` with the reason |
| Only the newest revision is approved; approval ends plan mode | done | `only_the_newest_revision_of_a_plan_can_be_approved`; `approving_the_newest_revision_records_it_and_ends_plan_mode` (end to end through `/approvals approve`) |
| Headless `--plan` prints the proposal and exits 10 | done | `binary_exec_plan_prints_the_proposal_and_exits_needs_approval` (against a scripted server) |
| A restart mid-wait recovers the pending approval | partly | The wait is a ledger record, which `/approvals` reads and a TUI started on the session surfaces (`surface_pending_approvals`, existing). No test restarts mid-wait. |
| Revert cycle | done | Each of these fails its test: a new plan instead of a revision; approving a superseded revision; the rejection not recorded; `--plan` losing its sink; approval not ending plan mode. |

Test-harness note: the TUI's approval continuation resolves the configured model, not a test's scripted one, so no test waits for that continuation to finish. The approval is asserted on the record it writes.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4281 passed, 0 failed; `pnpm generate:check`, `typecheck`, `test` (29) green.

### Self-review of `07c17bf` (and `bb8334a`) — findings fixed

The background review, by reading, found `bb8334a` correct, and:

1. **High.** `approve_plan` refused only when some *other* revision was pending. With nothing pending — the newest rejected, say — approving a stale revision's still-queued wait recorded `plan.approved` for a rejected plan and ended plan mode. Now only exactly the pending revision is approved; one superseded, rejected or already approved is refused ("is not waiting for approval"). Test: `only_the_newest_revision_of_a_plan_can_be_approved`, now with nothing pending. Revert cycle: accepting when nothing is pending fails it. The record's "only the newest revision is approved" held only while one was pending; it holds now.
2. **Medium.** The pending fold cleared on any approval or rejection of the same plan, whatever the revision. Rejecting a stale revision so cleared the newest, and the next submission started a new plan. It now matches plan and revision. Test: `a_submitted_plan_waits_for_approval_and_a_revision_leaves_the_first_intact` rejects revision 1 and asserts revision 2 still pending. Revert cycle: matching the plan alone fails it.
3. **Medium — recorded.** A superseded revision's approval stays queued: the earlier wait is not withdrawn. (1) and (2) make resolving it harmless: approving it is refused, and rejecting it records only its own rejection.
4. **Medium.** A structured `plan_exit` bypassed plan mode: in any mode it could write plan files and artifacts and open approval waits. A proposal is now submitted only from plan mode — `plan_enter`'s, `/plan`'s or `--plan`'s. Test: `a_proposal_is_submitted_only_from_plan_mode` (refused, nothing written). Revert cycle: no check fails it. The session tests now enter plan mode before submitting, since their harness forces a permissive lattice.
5. **Low.** A plan's resume did not check that its arguments were the ones approved. It now does, as a hook's resume does (`ApprovedAsk::covers`).
6. **Low — recorded.** After approval, the continuation's own later calls are judged as the turn began. The session's `/plan` mode ends for later turns.
7. **Low — recorded.** A batch holding `plan_exit` and other calls returns the wait after all run. A resumed batch replays only the waiting call, so no second submission.
8. **Verified.** No path or source injection: the plan id is the host's, and the approval source is set in code.
9. **Verified.** A hook's headless ask is unchanged.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` green; `cargo test --workspace --locked --no-fail-fast` 4282 passed, 0 failed.

### Self-review of 3aad86d
No defects found. Gap closed: `a_plans_resume_must_carry_the_arguments_that_were_approved` pins the `covers` check; with the check disabled the test fails (revert-cycled). Noted, not fixed: a superseded plan wait stays listed in `/approvals` and cannot be resolved; SEAM-05-3 should clear it.

## SEAM-05-3 — Approval compiles the proposal into a verified run

Approval writes .rapidlm/runs/<plan_id>.r<rev>.playbook.json (exec_tools::plan_playbook) and opens a verified run over it (p9_commands::start_verified_run) before plan.approved is recorded with plan_id, revision, run_id, graph_id, run_session, playbook; the run's nodes and DependsOn edges (VerifiedRun::node_keys/depends_on_edges) must equal the steps and depends_on or the approval fails and the plan stays pending; steps run on a background thread after the record. Tests: an_approved_plan_is_a_playbook_of_the_same_steps, approving_the_newest_revision_records_it_and_ends_plan_mode, only_the_newest_revision_of_a_plan_can_be_approved (superseded refusal). Revert cycles: dropped depends_on and human->approval fail the playbook test; renaming run_id fails the approval test; disabling the graph-equality check fails nothing (defensive: the graph is built from the same playbook). fmt, clippy -D warnings, cargo test --workspace 4284 passed 0 failed.

Known limits: a headless approval runs agent steps untrusted (fail-closed); a plan with no verification step cannot be approved (VerifiedRun refuses it) and stays pending; the superseded refusal is a failure text, not a typed enum; a superseded wait still lists in `/approvals`.

### Self-review of 3745f04
Fixed: (1) an approval that failed to record left a saved, resumable run (and one per retry) — the run state is now saved only after `go` (`a_run_never_told_to_go_leaves_nothing_to_resume`; revert cycle: saving at open fails it); (2) an untrusted (headless) plan run still ran its model-authored commands — command steps are now refused when untrusted (`an_untrusted_plan_run_does_not_run_its_commands`; revert cycle: always-trusted fails it). Checked and not a defect: the second ledger connection (WAL + busy_timeout on every open). Noted, not fixed: an orphan graph (no run state) still stays in the ledger after a failed approval; the run's outcome is not surfaced to the session and the run has no cancellation. fmt, clippy -D warnings, cargo test --workspace 4286 passed 0 failed.

### Self-review of aff2fe8
No blocking defects. Noted, not fixed: a save failure after `go` is silent (plan.approved then names a run with no state file); a failed approval still leaves an orphan graph (resume keys off the state file, so nothing resumes); `a_run_never_told_to_go_leaves_nothing_to_resume` leans on a 200 ms sleep (not flaky: the channel closes at once).

## SEAM-06-2 — MCP and plugin-source allow/deny lists, and the trust gate on mcp add

managed.mcp.allowed_servers/denied_servers (patterns over name, command, URL; denied wins) and managed.plugins.allowed_sources (patterns over the manifest path) parse closed in [mcp]/[plugins]. Enforced at: rapid mcp add (refusal before any write: untrusted project, unreadable policy, denied or unlisted server), session bind (load_project_integrations_with via gate_mcp_config; an unreadable policy binds none; warning per gate), plugin install (TUI plugin_install_into) and rapid plugins register (gate before the manifest is read). Reports carry field, origin=managed, remediation; rapid mcp list/probe print blocked lines; doctor's mcp check warns with them. Behaviour change (disclosed): an untrusted project's mcp add is a refusal that writes nothing. Tests: add_to_an_untrusted_project_is_refused_and_writes_nothing (unit + binary), add_of_a_server_the_managed_policy_refuses_writes_nothing (project tree byte-identical), list_and_probe_name_what_the_policy_blocks, the_project_loader_binds_only_the_mcp_servers_the_policy_allows, a_plugin_source_the_policy_does_not_allow_installs_nothing (catalog absent), the_mcp_check_lists_what_the_managed_policy_blocks, mcp_and_plugin_lists_parse_closed_and_match_by_pattern, a_pattern_matches_the_whole_text_with_stars_for_runs. Revert cycles: each of 7 gates disabled fails its test. fmt, clippy -D warnings; cargo test --workspace 4292 passed, 1 failed (mcp_cli round trip added in an untrusted project; fixed by granting trust, mcp_cli 15/15).

Known limits: plugin sources are matched against the manifest path only, so a URL locator cannot be allowed by pattern; `rapid mcp probe <blocked>` reports the name as unknown after the blocked line.

### Self-review of 496a86c
Fixed: (1) an allowed pattern admitted a server by the name the project picks (`github` → `/tmp/evil`) — allowed patterns now match only what a server runs (URL, or command, its file name, the command line); names only deny; (2) plugin sources were matched as written, so `/opt/../tmp/x` and a symlink under an allowed directory passed — the manifest is canonicalized, judged and read at that path; (3) deny patterns saw only the command as written — they now also match its file name and the command line with arguments (`evil` catches `/usr/bin/evil`, `*evil-pkg*` catches `npx evil-pkg`); `rapid mcp` reads the policy once per command. Behaviour change: a policy that allow-listed servers by name must name the command or URL. Revert cycles: name-admits, command-only targets, uncanonicalized path each fail their test. fmt, clippy -D warnings, cargo test --workspace 4294 passed 0 failed. Noted: an MCP source outside project settings (user-level, plugins) would need its own gate; none exists today.

### Self-review of cb944a5
Fixed: allowing by any target let an allowed pattern admit a server by its command's file name (`npx` admitted `/tmp/attacker/npx`) or by an argument (`*ok*` admitted `/tmp/evil /x/ok-server`). An allowed pattern now matches only the command as written, or the URL; file name, command line and name only deny. Revert cycle: allowing on any target fails `add_of_a_server_the_managed_policy_refuses_writes_nothing`. fmt, clippy -D warnings, cargo test --workspace 4294 passed 0 failed. Noted, not fixed: a narrow canonicalize-then-open race on plugin manifests needs write access inside an allowed directory; closing it needs platform-specific open-then-verify.

## SEAM-06-3 — Tool-name collision rule and User-Agent

User-Agent: rapid/<workspace version> (llm_router::providers::openai_compatible::USER_AGENT) is written by write_http_request, the one writer provider POSTs and the MCP streamable-HTTP transport (post_raw) share; a caller's own User-Agent stands and is not doubled. Collision rule: MCP tools are only reachable as mcp__<server>__<tool>, so a built-in keeps its name; surface assembly lists a twice-registered MCP name once and never lets one take a built-in's name; an MCP tool whose own name is a built-in's is reported once per process (warning at registration, tool_name_collisions/unreported_collisions). No plugin tool tier exists in the exec toolset today. Tests: the_raw_writer_the_mcp_transport_uses_names_rapid_once (scripted loopback server captures the head: one User-Agent, rapid's; a caller's stands), the streamed-completion fixture asserts user_agent == USER_AGENT, a_built_in_keeps_its_name_and_a_twice_registered_mcp_tool_is_listed_once. Revert cycles: no header, no dedup, no collision filter each fail their test. fmt, clippy -D warnings (after one lint fix), cargo test --workspace 4296 passed 0 failed.

Found in passing (flagged as its own task, not fixed here): the MCP HTTP adapter puts `Authorization` in the headers it hands `post_raw`, which refuses any request carrying one — a bearer-token MCP HTTP server likely fails every exchange.

### Self-review of b4d4c30
Fixed: a wire name two registrations share (server `a` tool `_b` and server `a_` tool `b` are both `mcp__a___b`; `__` is refused in server names, a trailing `_` is not) was listed as the first registration but dispatched by splitting at the first `__`, reaching the other. `execute_mcp_tool` and `offers_tool` now resolve a name through the registration the surface lists; a server named `db_`/`gh_` is now reachable and its grant is named (`a_lattice_ask_names_the_grant_that_answers_it_only_where_one_would` updated). Test: `an_ambiguous_mcp_name_reaches_the_registration_that_is_listed`; revert cycle: splitting the name fails it. fmt, clippy -D warnings, cargo test --workspace 4297 passed 0 failed. Noted: the proxy CONNECT sends no User-Agent; web fetch keeps `rapidlm-web-fetch` deliberately; `tool_name_collisions` rebuilds the surface once per registration.

### Self-review of 5286250
Fixed: resolving a shared wire name to its first registration let a remembered grant move — the grant is keyed by the name, and when the first server drops and re-registers the order flips, so `mcp__a___b` approved for (`a_`, `b`) would auto-approve (`a`, `_b`). A name two different tools share is now listed, offered and called for neither, and reported once as a collision (`mcp_registration` → `Ambiguous`); the same tool registered twice is still one tool. Test: `an_ambiguous_mcp_name_is_listed_offered_and_called_for_neither`; revert cycle: never ambiguous fails it. fmt, clippy -D warnings, cargo test --workspace 4297 passed 0 failed.

### Self-review of 86bbf76
Fixed: a poisoned `mcp_surface` lock read as "no registration", so the call fell back to splitting the name and could reach one of an ambiguous pair; the dispatch now recovers the lock as `offers_tool` does. Noted: `mcp_registration` makes surface assembly and collision listing O(n²) in registrations (harmless at the per-project server cap).

## SEAM-06-4 — Installer by shape

rapid mcp install <name> --into <path> [--format jsonc|toml|yaml] [--key a.b] [--dry-run] puts one of this project's servers (trusted project; managed policy already applied) into another program's file; rapid mcp install --discover lists files in the home/XDG roots (or RAPIDLM_CONFIG_ROOTS) whose shape declares a servers map (mcpServers|mcp_servers|servers|context_servers, top level or one down, members naming command or url), by path only (rule 2.5). Edits (apps/rapid/src/mcp_install.rs): a narrow JSONC editor (comment-aware scanner; insert or replace one member, keeps trailing-comma style and end-of-line comments), toml_edit for TOML, a block-mapping YAML editor (flow maps other than {} refused); timestamped .bak (0600) beside the file; atomic 0600 write; per-user lock (<home>/mcp-install.lock, held read→write); symlink targets refused; identical entry → `unchanged`; --dry-run prints a line diff and writes nothing. Tests: 8 unit (JSONC/TOML/YAML keep comments, order and other keys — every old byte kept in order, additions only; unchanged on repeat; missing map created; stale entry replaced in place; non-objects refused; discovery by shape with neutral names; roots from env first; lock refuses a second holder; diff) + 3 through the CLI (dry-run writes nothing, install writes entry + one .bak equal to the original + mode 0600, second run unchanged with no new backup; untrusted / unknown server / held lock refused with the target untouched; --discover by shape). Revert cycles: dry-run, backup, unchanged, lock, the JSONC/YAML/TOML unchanged checks and the shape test each fail their test. fmt, clippy -D warnings; cargo test --workspace 4307 passed, 1 failed (exec_tools::tests::twice_the_ceiling_queues_in_order_rather_than_erroring — untouched admission-queue timing test; passes 3/3 alone).

Interpretation: rapid has no MCP server mode of its own, so what is installed is a server this project already configures. Known limits: the YAML editor handles block mappings only; JSON output of an entry lists its fields in sorted order; values are written as JSON strings in YAML (valid double-quoted scalars).

### Self-review of 82dd591
Fixed: (1) a YAML servers block written as a list got a mapping appended into it — invalid YAML — and `--discover` recommended exactly those files: a list is now refused and not discovered; discovery no longer stops at the first known key with an empty block; (2) JSONC trailing commas (`,}`) made a file undiscoverable and every repeat install a rewrite: `strip` drops a comma before `}`/`]` outside strings; (3) `--dry-run` printed `env`/`headers` values: the diff shows them `<redacted>` and counts replaced lines instead of echoing them; (4) YAML keys a reader retypes (`null`, `on`, `Y`, `12`, …) are quoted; (5) CRLF endings and a UTF-8 BOM are kept (the editors see LF and no BOM; both are restored); (6) symlinked config files are discovered (symlinked directories are still not followed); each root gets its own entry budget. Tests: a_yaml_list_of_servers_is_refused_and_not_discovered, jsonc_trailing_commas_are_read_…, yaml_keys_a_reader_would_retype_are_quoted, crlf_and_a_byte_order_mark_are_kept, redacted_hides_env_and_header_values, discovery_reads_a_symlinked_file_…, and the CLI dry-run now asserts no secret. Revert cycles: each of 8 mutations fails its test. fmt, clippy -D warnings, cargo test --workspace 4314 passed 0 failed. Noted, not fixed: a crashed install leaves its lock (the refusal says to remove it); the write sets 0600 on a more permissive existing file (documented).

### Self-review of 4884ed6
Fixed: (1) a file with any CRLF had every line converted — now only a file whose every line ends in CRLF is normalized and restored, a mixed file is edited as it is (`a_file_mixing_line_endings_keeps_every_other_line_as_it_was`); (2) a YAML list at its key's own indent (`mcpServers:\n- name: a`) escaped the list refusal and discovery — the first line after the key is looked at, at any indent ≥ the key's (`a_yaml_list_at_its_keys_own_indent_is_refused_and_not_discovered`); (3) the dry-run fallback printed the real diff when the redacted edit came to nothing — now no diff is shown; (4) discovery reads are bounded as read. Revert cycles: (1) and (2) each fail their test.

## SEAM-06-5 — Skill package and --help drift test

skill/SKILL.md (open skill format: name/description frontmatter) teaches an external host to drive rapid headlessly — trust and setup first, one `rapid exec` turn, the stdout/stderr contract, --jsonl / --json-schema / --usage-file / --plan / --max-wall-time, the per-run environment, the exit-code table, resume rules (resume the exact session; never start a second goal because a resume was interrupted; an unknown id exits 2), key handling (never on the command line; --key-env / --key-stdin); skill/long-running-goals/SKILL.md covers goal create/replace (criteria, requires, budget), driving turns, show/verify, claim with checks (exit 0 or 6) and the lifecycle. Drift test apps/rapid/tests/skill_package.rs: every `rapid …` line in the package's shell blocks is split as a shell would and checked against `rapid <subcommand> --help` — every --flag and the subcommand word must be named there; the skill's exit-code table must equal docs/getting-started.md's (itself checked against JsonlExitCode::ALL); frontmatter present. To make the goal sub-skill checkable, `rapid goal --help` now documents every subcommand and flag (was one line), and the top-level usage lists `rapid mcp …|install`. Revert cycles: a made-up flag (`--jsonl-out`) and a made-up subcommand word (`goal suspend`) each fail the drift test. docs/reference/cli-command-reference.md gains install/--discover, the trust gate on add and the managed MCP policy.

Checks: fmt, clippy -D warnings; cargo test --workspace: 4319 passed, 1 failed (`no_subcommand_summary_is_pushed_past_the_terminal_s_width` — my longer `rapid mcp` summary; shortened, test and the drift test pass); full re-run recorded below.
Re-run after the fix: fmt, clippy -D warnings; cargo test --workspace 4319 passed, 1 failed (`twice_the_ceiling_queues_in_order_rather_than_erroring`, the recorded admission-queue load flake; passes alone).

### Self-review of 7ac4098
Fixed: (1) `-h`/`--help` anywhere in `rapid goal`'s arguments printed help and exited 0, so `rapid goal create fix the -h flag …` created nothing and reported success — help is now only `rapid goal --help` or `rapid goal <sub> --help` (`goal_help_is_asked_for_only_where_a_flag_could_be_it`; revert cycle: any-position fails it); (2) the goal sub-skill claimed an exhausted goal budget exits 8, which the headless path does not do — the line now points at `rapid goal show`; (3) the drift test was weak: it now reads inline `rapid …` spans and untagged blocks, checks every nested command word (not only the first), judges a nested command's flags against its own help entry, and refuses `--flag=value` (the parser has no `=` form); `the_check_catches_what_it_is_for` feeds it six wrong commands. The stricter check found `rapid goal --help` saying `replace` takes "the same flags as create" without naming them; they are listed now. Noted, not fixed: `parse_flags` silently accepts unknown `--key value` pairs for every goal command. fmt, clippy -D warnings, cargo test --workspace 4321 passed 0 failed.

### Self-review of 46a0588
No blocking defect. Fixed: `rapid goal evidence record|list --help` had stopped printing help (`list` printed the evidence instead) — `evidence <action> --help` is help again; the drift test scoped a nested command's flags by its first word only, so `evidence list --kind` passed on `record`'s entry — it now takes the longest nested command the help gives an entry. Revert cycles: both fail their test. Noted: inline-span scanning pairs backticks per line (double-backtick spans and spans across lines are missed — they only ever miss, never fail wrongly). fmt, clippy -D warnings, cargo test --workspace 4321 passed 0 failed.

### Self-review of 6fcba00
No defects.

## SEAM-07-1 — Cost in the ledger, `rapid usage` (slice 1 of 2)

Slice 1 (ledger + CLI): model.completed gains input_tokens, output_tokens, cached_tokens (null when unknown), tokens_estimated and cost in AccountedCost's encoding ({kind: reported, usd_micros} | {kind: unknown}) — additive; the SDK wire catalog lists kinds only, so WIRE_SCHEMA_SHA256 is unchanged. Source: agent_runtime::StepUsage via ModelDriver::take_step_usage (ConfiguredModel sums every request of a step — continuations, retried attempts — a part unknown makes the whole unknown; forwarded by LiveContextModelDriver, SupervisedModel, FallbackChainModel, SelectedModel, the boxed driver); a driver without it records the step's reported cost and an unknown split. `rapid usage [<session>] [--project] [--since <time>] [--output tsv|json] [--quiet]` (apps/rapid/src/usage_report.rs, p9_commands::usage_from_ledger) reduces model.completed per turn with totals; cost is reported or unknown — `unknown(>=<reported part>)`, never zero; basis reported|estimated|unknown|none; pre-change records read as estimated with cost unknown; a missing ledger is an empty report and is not created; an unknown session exits 2. Tests: binary_exec_uses_configured_model_end_to_end runs rapid exec against the scripted provider then rapid usage --output json (3 input + 5 output tokens, cost unknown); usage_totals_are_the_sum_of_the_sessions_ledger_records (fixture ledger: session totals = sum of records, pre-change session, --project, --since, unknown session, no ledger created); usage_report unit tests (sums, unknown never zero, estimates flagged, pre-change ledger, known zero for no steps, --since parsing); agent-runtime a_step_records_its_reported_cost_and_the_drivers_split and step_usage_merges_known_parts_…. Revert cycles: no payload fields, unknown merged as 0, missing cost not counted unknown, and the turn-loop fallback each fail their test. fmt, clippy -D warnings; cargo test --workspace 4328 passed, 1 failed (the_reference_doc_lists_exactly_the_dispatched_subcommands — `usage` added to the CLI reference; passes). Slice 2 (/usage beside /context in one tabbed modal, legend summing to 100%) pending.

### SEAM-07-1 slice 2 — `/usage` beside `/context`, and self-review of 9aa9823

Slice 2 (/usage): the context sidebar route is one panel with two tabs — `/context` (window) and `/usage` (session usage) — a `[context]  usage` / ` context  [usage]` header; UiCommand::OpenUsage → Inspector::Context { tab: Usage }; LocalUiEvent::SelectContextTab. The window tab's legend gives every class's share of the window and the free share as whole percentages that sum to exactly 100 (largest remainder, ties to the earlier row). The usage tab folds model.completed (and model.failed records carrying usage) into tui::state::SessionUsage by the rule rapid usage uses — the_tui_and_rapid_usage_total_the_same_records_alike checks the two agree. Self-review of 9aa9823 fixed: a failed step's usage was never taken and leaked into the next step's record — it is taken with the failure and written on model.failed (TurnEvent::ModelFailed { usage }), and both reducers count it; a provider reporting only one side of the split recorded the other as a known 0 — the split is now unknown (UsageDetail.split_reported). Tests: the_context_legend_sums_to_one_hundred, the_usage_tab_folds_the_sessions_model_steps_and_never_calls_unknown_zero, usage_opens_the_context_panels_usage_tab, a_failed_steps_usage_is_its_own_record_never_the_next_steps, the ledger fixture test now with failed records, the half-split case in the fold_stream test. Revert cycles: remainder distribution, the tab select, the failed-record fold (TUI and CLI), taking a failed step's usage, split_reported — each fails its test. fmt, clippy -D warnings; cargo test --workspace 4333 passed, 1 failed (a_finished_turn_reports_the_context_it_actually_used read the totals from line 0, now the tab header — updated, passes).

### Self-review of f95e377
Fixed: the `/usage` tab counted a turn per run of consecutive steps, so a subagent's steps interleaved with its parent's (A, B, A) made three turns where `rapid usage` counts two — it now counts distinct turn ids, a step without one being its own; the parity test feeds interleaved turns (revert cycle: counting runs fails it). Noted, not fixed: a failed step whose provider reported only one side of the split adds 0 tokens to the session total (its cost and basis read `unknown`, so the gap is flagged); when partitions' used exceeds the limit the legend shares are of the used sum. fmt, clippy -D warnings, cargo test --workspace 4334 passed 0 failed.

### Self-review of d45f155
Fixed: `SessionUsage.seen_turns` had no serde default, so a stored state with a `session_usage` from before it existed would fail to load (latent — nothing deserialises AppState today): defaulted, and `a_session_usage_stored_before_turns_were_kept_still_loads` loads the old shape (revert cycle: removing the default fails it).

## SEAM-07-3 — A configurable status line

[ui.status_line] (apps/rapid/src/status_line.rs): type builtin|command|disabled, items (built-in names), command (one line), refresh_interval 1–86400 s; user config.toml outranks the project's .rapidlm/config.toml; closed and bounded (bad key/value fails naming it); registered in protocol's config spec as a known key (LeafKind::Text added). protocol::status::StatusPayload v1 (rapidlm.status_payload; session_id, turn_id, model, effort, context.{used_tokens,limit_tokens,used_percent}, cost.{usd_micros,basis}, goal.{id,state}, worktree, workspace.{cwd,repo}, trigger state|refresh_interval) with fixture crates/protocol/tests/fixtures/status/v1/status_payload.json and validate(). A command runs through sandbox_exec::run_sandboxed (the shell_exec tier) with a 5 s timeout: a fixed one-line wrapper takes the command, payload and ids as positional arguments (never spliced; the sandbox refuses newlines and empty args), pipes the payload on stdin and exports RAPIDLM_SESSION_ID/RAPIDLM_TURN_ID. StatusRunner drives it from the TUI loop off-thread: reruns on a changed payload or an elapsed interval, keeps ≤5 lines of ≤1024 chars, keeps the last output when a run fails (with none, the row says why). A project's command is refused in an untrusted project; a missing tier is StatusRun::Unavailable, other sandbox refusals Failed. TUI: StatusMode (Builtin{items}|Command{lines}|Disabled) on AppState; the row filters built-in items, shows a command's lines sanitised, or nothing. Docs: docs/reference/status-line.md. Tests: status_line unit tests (config, precedence, five lines, trust refusal, stdin + env delivery with a `$(…)` in the payload not expanded, timeout, runner rerun/keep-last), a_command_status_line_runs_from_the_session_loop_and_fills_the_status_row, the_status_line_mode_decides_what_the_row_shows, status_payload_v1_fixture_matches_wire_contract, a_status_payload_is_refused_when_its_shape_is_wrong, the_status_line_table_is_a_known_key_and_shape_checked, a Windows-only typed-unavailable test. Revert cycles: trust refusal, line cap, env export, change-rerun, keep-last, item filter, sanitising — each fails its test. fmt, clippy -D warnings (three mechanical lint fixes after the run), cargo test --workspace 4347 passed 0 failed.

### Self-review of 4b6ec8c
No security defect (the wrapper passes command, payload and ids positionally; nothing is re-expanded). Fixed: (1) every drain tick (50 ms) cloned the whole AppState and stat'ed `.git` even for the default built-in row — the tick returns unless the status line is a command, and borrows the state; (2) a payload that changes as a turn streams reran the command back to back — a changed payload reruns no sooner than `MIN_STATE_RERUN` (1 s, or the interval if shorter; `a_changed_payload_does_not_rerun_sooner_than_the_least_gap`); (3) trust was fixed at session start, so `/trust revoke` mid-session left a project's command running — trust is read from the catalog before each run (`trust_is_read_before_each_run_and_an_untrusted_project_starts_no_run`); (4) an untrusted project spawned a thread per interval just to refuse — refused before any run starts, shown once. Revert cycles: start-time trust, no gap — each fails its test. Noted: a status command can write the workspace (the sandbox's root is read-write, as for `shell_exec`). fmt, clippy -D warnings, cargo test --workspace 4349 passed 0 failed.

### Self-review of e86d059
Fixed: a refusal recorded itself as a run (`last_run`, `last_state`), so after `rapid trust grant` an idle session kept the refusal for up to a whole `refresh_interval` (up to a day) — a refusal now leaves the schedule alone, and while refused trust is asked again every `TRUST_RECHECK` (5 s); the trust result is passed to `run_command`, so its own gate is no longer dead. The test no longer back-dates the last run (revert cycle: recording the refusal as a run fails it). Noted: the runner's lock is held while the payload is built and the catalog read (one reader, the UI thread). fmt, clippy -D warnings, cargo test --workspace 4349 passed 0 failed.

### Self-review of ab28305
Fixed: while refused, a refusal left the run schedule due, so every 50 ms tick still read the trust catalog — `TRUST_RECHECK` only ever added reasons to proceed. While refused, the recheck clock alone now decides; the trust test ticks five times between rechecks and asserts trust was not read (revert cycle: removing the gate fails it).
fmt, clippy -D warnings, cargo test --workspace 4349 passed 0 failed.

### Self-review of 8447e87
No defects.

## SEAM-07-4 — Opt-in OTLP exporter

[telemetry.otlp] endpoint (apps/rapid/src/telemetry_otlp.rs), read from the user's config only (a project cannot redirect telemetry); registered in protocol's config spec. Off by default: no endpoint, nothing built — no thread, no socket. On: crates/telemetry gets its first dependent — Telemetry with an OtlpSink over HttpOtlpTransport; every record passes the redaction pipeline (forbidden keys, registered secrets masked); the transport hands a record to one worker through a bounded queue (64; full → dropped, never blocks); the worker posts through ProviderEgress::for_endpoint (allows exactly the collector) with Http1Transport::post_raw, 5 s timeout, and keeps every egress decision as a receipt — in memory and appended to <RapidLM home>/telemetry/egress-receipts.jsonl (the ledger has no egress kind; adding one changes the SDK wire catalog). rapid exec exports each turn (status, tokens, tool calls, cost or `unknown`; no prompt/answer/path/command text) and flushes for at most 5 s before exit; loss is never the turn's failure. crates/telemetry fix: OtlpSink kept every payload in a local queue that never drained, so after MAX_LOCAL_QUEUE (1024) records every later one was dropped unsent — with a collector nothing is kept locally. Tests: binary_exec_exports_a_redacted_turn_record_only_when_the_user_opts_in (off: nothing sent, no telemetry dir; on: one record, no model key, no prompt, an allowed receipt), off_unless_the_users_config_names_an_endpoint, only_redacted_records_leave_and_every_dial_is_a_receipt, a_dead_collector_never_fails_or_holds_up_the_caller (a collector that accepts and never answers), telemetry's a_collector_sink_keeps_exporting_past_the_local_bound. Revert cycles: keeping payloads locally, a blocking hand-off (fails after 644 s), no secret masking, no receipt recording — each fails its test. fmt, clippy -D warnings, cargo test --workspace 4354 passed 0 failed.

Deviation: the spec names a `record_egress` and a ledger receipt; neither exists, and a new ledger kind changes the wire catalog — receipts are kept by the exporter and in a local log instead. The collector receives a placeholder bearer header (`rapidlm-telemetry`), which the raw writer requires.

### Self-review of 4ccda8c
Fixed: (1) the payload was `crates/telemetry`'s own record JSON, which a real collector rejects — the worker now sends OTLP/HTTP JSON (`resourceLogs` → `scopeLogs` → one `logRecords` entry with the record's attributes as string attributes; `otlp_logs`), and the test parses the collector's body as OTLP; (2) IPv6: `http://[::1]:4318` was refused and bracketed hosts mis-split — bracketed literals are parsed and `[::1]` is loopback; (3) telemetry dials were audited as `NetworkClient::Provider` — a `Telemetry` client (and `ProviderEgress::for_client`); (4) the receipt log's `at` was a UUID — now RFC 3339; rotation renamed to `.1` instead of deleting the trail; the file is created 0600; (5) the exit flush is bounded at 2 s (`EXIT_FLUSH_LIMIT`), not the 5 s export timeout; (6) the module doc claimed registered secrets are masked in production — none are registered; it now says a turn record carries only counts and a status class. Revert cycles: raw payload, IPv6 parsing, receipt mode — each fails its test. Noted: the collector gets a placeholder bearer header (the raw writer requires one); an operator-named private address is dialled (the user's own config). fmt, clippy -D warnings, cargo test --workspace 4354 passed 0 failed.

### Self-review of ee6d55b
Fixed: an IPv6 collector was accepted by config but could never be reached — the HTTP client (`parse_http_url`) refuses bracketed literals, so every export failed before dialling. `Collector::parse` now refuses an IPv6 endpoint with a plain message (name the collector by host name or IPv4) instead of claiming support; revert cycle: accepting it fails the test. Confirmed: the OTLP JSON body matches the OTLP/HTTP JSON mapping; `AttributeSet` serialises as `{fields, omitted}` and is read correctly. Noted: `result_class`/`latency_ms` are not exported; an existing receipt log keeps its old mode until rotated. fmt, clippy -D warnings; cargo test --workspace 4354 passed, 1 failed (`computer_observe_reports_the_typed_platform_gate_not_a_stub` — the recorded host-dependent desktop test).

## SEAM-08-1 — `--worktree` runs over GitWorktreeStore

Decision D-2 recorded (a): worktrees stay under the repository's .git/rapidlm/worktrees (records under .git/rapidlm/views) — docs/goals/seams-phase0-baseline-2026-09-20.md. `rapid exec --worktree[=<name>]` (a name is joined with `=`: a bare following word is the prompt) makes a linked worktree of the project at its HEAD through GitWorktreeStore::create_view (agent_views::create_run_view; local, no fetch) and roots the run's tools there; trust, settings and the ledger stay the project's; the path is printed on stderr; an untrusted project is refused (exit 2) with nothing made. The worktree record gains session_id and name (GitWorktreeStore::label_view; both optional, so older records load). `rapid goal create --worktree[=<name>]` (trust checked before the goal is made) makes the goal's worktree and records it in .rapidlm/goal-worktree.json; while that goal is active every `rapid exec` without the flag works in it. Help: `rapid exec --help` and `rapid goal --help` document the flags. Tests: binary_exec_worktree_runs_in_a_linked_worktree_and_leaves_the_project_untouched (every proxy variable at a closed port, loopback exempt for the scripted model: the patch lands in the worktree, the project's file and `git status --porcelain` unchanged, the record names the session and `fix-notes`), binary_goal_worktree_is_where_the_goals_exec_turns_work, binary_exec_worktree_needs_a_trusted_project (a real git repo; nothing under .git/rapidlm), exec_worktree_is_a_flag_with_an_optional_joined_name, workspace's a_view_is_labelled_with_its_session_and_name_and_it_persists. Revert cycles: tools not re-rooted, no session label, no goal pickup, untrusted allowed — each fails a test. Deviation: the spec's `branch-tip fetch default, --full-history opt-in` is not built — the store never fetches (the base is always a local commit), so no flag was added that would do nothing. fmt, clippy -D warnings; cargo test --workspace 4358 passed, 1 failed (computer_observe_reports_the_typed_platform_gate_not_a_stub, the recorded host-dependent desktop test).

### Self-review of a81f497
Fixed: (1) subagents of a `--worktree` run were configured at the project root, so a child branched from the project and a headless run integrated its patch into the project — the tree the flag promises to leave alone; they now branch from and integrate into the run's worktree (`binary_exec_worktree_subagents_work_in_the_worktree_not_the_project`; revert cycle: the project root fails it); (2) the evidence invalidator pointed at the worktree's `.rapidlm`, where no evidence lives, so stale goal evidence in the project was never invalidated — it is the project's again; (3) `goal create --worktree` made the goal first, so a failed worktree left a goal without one and no way to attach it — the view is made first, and a failure makes no goal; (4) a failure to make the worktree exits as a runtime failure, not a usage error. Noted, not fixed: hooks and MCP servers are configured at the project root while the file tools work in the worktree (the project's `.rapidlm` settings are usually ignored by git, so the worktree has none of its own); a completed goal's worktree and record are not removed (reclaim is SEAM-08-2). fmt, clippy -D warnings, cargo test --workspace 4360 passed 0 failed.

### Self-review of ec80e26
Fixed: moving subagents to the worktree root also moved agent-type discovery there — a project's own definitions in `.rapidlm/agents` (usually ignored by git, so absent from the worktree) were lost under `--worktree`, and a spawn naming one was refused. The roots are split (`configure_trusted_model_tools_in`): definitions come from the project, children branch from and integrate into the worktree. The test now spawns a project-defined type and asserts the run's last reply, since a refused spawn let the parent apply the scripted patch itself and the test passed anyway; revert cycle: the worktree root for definitions fails it. fmt, clippy -D warnings, cargo test --workspace 4360 passed 0 failed.

### Self-review of b49ba0b
No confirmed findings: the agent-type inventory reads the project, the runner (child views, settlement, read-only child tools) the worktree; other callers pass one root twice. Noted: nothing checks a child worked in its own view rather than the run's worktree directly.

## SEAM-08-2 — reclaim rule; `rapid worktree list|reclaim [--dry-run]|abandon`

`rapid worktree list|reclaim [--dry-run]|abandon <view-id|name>` (apps/rapid/src/worktree_cmd.rs over agent_views::worktree_entries / reclaim_worktrees / abandon_worktree; trusted projects only). The reclaim rule: a worktree the store made is reclaimable only when it is abandoned (the project journal holds a committed `workspace.abandon` for its view, worktree and base) or merged (its HEAD is in the project's history, so nothing in it is unpublished); and it is clean (`git status --porcelain` empty); and neither the current goal (active, paused or blocked — .rapidlm/goal-worktree.json) nor a live `rapid exec --worktree` run (a pid lease under .git/rapidlm/leases, held for the run and checked with process_signal::process_exists) holds it. The primary checkout is never a candidate (the store lists only its own views; one resolving to the project root is kept). `abandon` resets the worktree to its base and removes untracked files (worktree-local), journaled prepared → executing → committed as `workspace.abandon`, refused while a run or the goal holds it. `reclaim` removes each reclaimable worktree through GitWorktreeStore::remove_view (never forced), each removal journaled as `workspace.reclaim`; `--dry-run` lists them and neither removes nor journals. Tests: agent_views a_worktree_is_reclaimed_only_when_nothing_in_it_would_be_lost (merged untouched view; uncommitted and unmerged commits kept; a live lease keeps it and refuses abandon; abandon → reclaimable; dry run removes nothing and journals nothing; reclaim removes it and the journal records workspace.reclaim committed; primary untouched), the_current_goals_worktree_is_never_reclaimed, binary_worktree_reclaim_keeps_unpublished_work_until_it_is_abandoned (end to end through `rapid exec --worktree=wip`). Revert cycles: no dirty check, lease ignored, goal hold ignored, dry run removing — each fails a test. Deviation: "published" for a run's worktree is read from git (its HEAD in the project's history), not the publication journal — a run worktree's changes never pass through `workspace.publish` (that journal records child-view integrations, whose views are released when published); abandon and reclaim are journaled as specified. fmt, clippy -D warnings; cargo test --workspace 4362 passed, 1 failed (the reference-doc subcommand list, then fixed; the rapid lib target re-run: 1196 passed 0 failed).

### Self-review of 92353e3
Fixed: (1) data loss — an abandon's verdict never expired, so work committed in the worktree after `rapid worktree abandon` was reclaimed and its commits made unreachable; the abandon now counts only while HEAD is still the base it reset to; (2) a new `--worktree` view was visible, clean and "merged" before its run took a lease, so a concurrent reclaim could remove it under the starting run — `create_run_view` takes the lease before the view exists and returns it (goal create holds it until the goal's record is written), and a run that cannot hold its worktree does not run in it (the lease failure was ignored); (3) Windows: `process_exists` is always false off Unix, so every lease was ignored — where liveness cannot be checked any lease holds its worktree; (4) two runs in one worktree shared one lease file and the first to end released the other's claim — a lease is `<view>.<pid>`, one per run; (5) ignored files (a local `.env`, build output) counted as clean and were deleted by removal — `status --porcelain --ignored`, and abandon now removes ignored files too (`clean -fdqx`); (6) help: abandon says commits are dropped; the rule is stated as enforced; no arguments prints usage to stderr (exit 2). Tests: a_new_worktree_is_held_from_birth_and_each_run_holds_it_separately (Unix: a second live pid's lease keeps it after the first releases; a dead pid's claims nothing), work_after_an_abandon_and_ignored_files_are_never_reclaimed. Revert cycles: abandon without the HEAD check, status without --ignored, the birth lease released — each fails a test. fmt, clippy -D warnings, cargo test --workspace 4365 passed 0 failed.

### Self-review of e540389
Fixed: where liveness cannot be checked (off Unix) a lease a crashed run left behind held its worktree for good — `list`, `reclaim` and `abandon` all refused with no way out named. The refusal now names the lease file to delete if that session is not running, and a reclaim removes every lease on the worktree it removed (on Unix, dead leases no longer accumulate). The platform rule is testable everywhere (`lease_holder_by` takes the liveness check): where_liveness_is_unknown_every_lease_holds_until_its_worktree_is_reclaimed; revert cycle: no release on reclaim fails it. Confirmed: the goal-create lease lives until the goal's record is written; a cron's sequential turns share a pid but never overlap. fmt, clippy -D warnings; cargo test --workspace 4367 passed, 1 failed (the `du` summary's help width, fixed in the next commit).

## SEAM-07-2 — `rapid du` with a reclaim plan that never deletes

`rapid du [--reclaim-plan] [--output text|json]` (apps/rapid/src/disk_usage.rs): bytes RapidLM keeps by the real layout (baseline §3: sessions, index, findings, goal files live per project) — the project's `.rapidlm` tree, the user's RapidLM home (config, trust and permission files) and the worktree store under `.git/rapidlm` (`worktrees` and the store's records), each area with its top-level entries largest first; file lengths, symlinks not followed (a link counts as itself). `--reclaim-plan` (trusted projects) calls agent_views::reclaim_worktrees with dry_run and prints the same lines as `rapid worktree reclaim --dry-run` (shared worktree_cmd::plan_line) with the total it would free; it never deletes. Tests: disk_usage sizes_match_the_files_and_symlinks_are_not_followed (a fixture tree, a symlink to a 1 MiB tree inside a subdirectory counted as the link); binary_du_measures_the_real_layout_and_its_plan_is_reclaims_dry_run (a fixture home and git project; the project area's path and bytes equal a recursive walk of `.rapidlm` and list a probe file at its exact length; a worktree its cancelled goal made appears in the plan, the plan lines equal `reclaim --dry-run`'s, and the worktree is still listed after). Revert cycles: the plan run for real, symlinks followed — each fails a test. Sizes are file lengths, not allocated blocks: the validation's 'within block rounding' is met exactly. fmt, clippy -D warnings; cargo test --workspace 4367 passed, 1 failed (`du`'s help summary too wide for the terminal check — shortened; the test passes).

### Self-review of f8a36a0 and 1a7d791
Fixed: (1) `rapid du --reclaim-plan` wrote to disk — the plan opened the project journal, which creates the ledger's directory, the ledger and a publication session; listing and planning now read the journal only if it exists (`PublicationJournal::open_existing`), a dry run opens none, and a project with no journal holds no abandon (`listing_and_planning_write_nothing`; revert cycle: opening the writable journal fails it). The binary test snapshots `.rapidlm` and `.git/rapidlm` around the plan and requires them unchanged; (2) the `worktree-records` area was all of `.git/rapidlm` and so counted every worktree twice — the areas are now `worktrees` and `worktree-store` (the rest), which never overlap; the test requires the two to sum to a walk of `.git/rapidlm` (revert cycle: the overlap fails it); (3) `rapid du` arguments: `--output yaml` and an unknown flag exit 2 (tested). Not changed: `release_leases` is exercised only on Unix (its test needs a reclaim, which needs a live-pid check). fmt, clippy -D warnings, cargo test --workspace 4369 passed 0 failed.

### Self-review of 57bfcb1
Fixed: "listing writes nothing" was still untrue for an existing ledger — `EventLedger::open` applies migrations (an older ledger was migrated by `rapid worktree list` or `rapid du --reclaim-plan`), creates its directory and switches to WAL, and a zero-byte file became a database. The listing now reads the journal through `event_ledger::journal::peek_latest_state` (via `publication::peek_committed`): opened read-only with no migration and no session, and — because a read-only connection to a WAL database leaves `-wal`/`-shm` behind — as `immutable=1` when no WAL exists (no writer; everything is in the file), a plain read-only connection only when a live writer's WAL already exists. `a_peek_reads_the_latest_state_and_writes_nothing` requires the ledger's files (names, sizes, mtimes) unchanged, a path with a space, `#` and `%` read through its URI, and a missing ledger left missing; revert cycle: the plain read-only open fails it. fmt, clippy -D warnings; cargo test --workspace 4369 passed, 1 failed (`protocol` schema_fixtures `explicit_update_flag_writes_then_locks_fixture`, an EINVAL on its temp directory; 3/3 green alone, untouched by this change).

### Self-review of dbf6bf4
No data-loss path: every failure of the read-only peek errs toward Kept. Fixed: (1) a ledger from before the journal (no `operation_journal` table) made every worktree Kept — the old writable open migrated it first; an unreadable journal now holds no readable abandon and the merge check still decides (`an_unreadable_journal_leaves_the_merge_check_to_decide_and_is_left_alone`, the file left byte-identical; revert cycle: Kept on error fails it); (2) Windows verbatim (`\\?\C:\…`, what `canonicalize` returns) and UNC paths made a URI SQLite rejects — written as the plain path, UNC as `file:////server/share/…` (`an_immutable_uri_names_the_same_file_on_every_path_shape`); (3) a doc comment moved back onto `publication_session`. fmt, clippy -D warnings, cargo test --workspace 4372 passed 0 failed.

### Self-review of 185deb8
No confirmed findings: `file:////server/share/…` is the URI form SQLite accepts for UNC (an authority other than empty or `localhost` is refused); the verbatim-prefix mapping is ordered correctly; an unreadable journal can only drop an abandon, never add a reclaim.

## SEAM-09-1 — persisted never-allow answers; every rule listed with its origin

Persisted "never allow" answers beside the allow grants: the per-project store (`<home>/project-permissions.json`, same lock, 0600 and bounds) gains a `deny` list per project (written only when non-empty, so a store without refusals renders byte-identical); `PermissionGrants::deny` and `allow` replace each other (the newer answer stands), `revoke` removes either. The lattice gains `with_denials`, checked at step 0.5 — after the managed tool ban and write ceilings, before plan mode, every project rule, grant and mode, and for read-classified calls too (a fetch domain, an MCP tool): `Decision::Deny(PersistedDeny)`. A store that exists but cannot be read refuses every call (`PersistedAnswersUnreadable`, with a warning naming the file) rather than silently drop a refusal; a missing store is no refusals. Answers: `rapid permissions deny <pattern>...`; `/approvals deny <n> never` in the TUI records the ask's own standing pattern (`remember_as`: a file, an MCP tool, `web_fetch(domain:<host>)`) and is refused, with nothing resolved, where none exists (a shell command, a hook's ask, a plan); ACP offers "Reject always" beside "Allow always" (only where a pattern is kept) and records the refusal. `rapid permissions list` prints `denials=`/`deny=` and then every rule in effect with its origin — `rule=<effect> <pattern> origin=persisted|settings:<file>|managed`. Tests: permissions a_persisted_never_allow_outranks_rules_grants_and_every_mode (default/acceptEdits/bypass, an allow rule and a grant; a subagent keeps it; a managed ban still first; unreadable refuses all), never_and_always_answers_persist_replace_each_other_and_revoke (render/parse round trip); permissions_cli a_never_answer_is_recorded_listed_with_every_rule_and_its_origin, an_unreadable_store_is_an_error_for_the_next_run_not_no_refusals; interactive deny_never_records_a_standing_refusal_the_next_ask_never_reaches; acp_cli a_reject_always_answer_is_kept_so_the_same_call_is_refused_unasked (a new serve refuses it without a permission request; `tool.denied` names the reason); configured_model_integration binary_a_persisted_never_allow_blocks_the_next_run_without_asking (bypassPermissions, the patch refused, `list` shows `origin=persisted`); acp v1 standing_answers_are_offered_only_with_a_pattern_to_record and the compat golden. Revert cycles: denials not wired into the run's lattice, the step-0.5 check removed, the TUI recording a grant instead, ACP not keeping the refusal — each fails a test. The TUI's `/permissions deny <pattern>` writes through the same path (permissions_slash_deny_records_a_never_allow_the_next_run_reads). fmt, clippy -D warnings; cargo test --workspace 4379 passed, 1 failed (`twice_the_ceiling_queues_in_order_rather_than_erroring`, the recorded flake; 3/3 green alone).

### Self-review of c8da566
Fixed: a build that predates refusals read the store (schema 1, `deny` ignored) and its writer rewrote it without them — every "never allow" silently gone, and not enforced meanwhile. A store holding a refusal is now written as schema 2, which such a build refuses (its writer will not overwrite an unparsable store; its runs fail closed on grants), and a store without one stays schema 1; `deny` is read only from schema 2 (revert cycle: always schema 1 fails `never_and_always_answers_persist_replace_each_other_and_revoke`). Noted, not changed: with no resolvable home no store is read (none can have been written either); `list` reads managed policy from the process environment, as the run does, so no test asserts the `managed` origin; `list` reads settings from the project root while a run reads them from its working directory (the run's behaviour predates this change); a `never` answered in a session applies from the next lattice built, re-asking meanwhile. fmt, clippy -D warnings, cargo test --workspace 4380 passed 0 failed.

### Self-review of 04ff79e
Fixed: `deny` was read only from schema 2, so a store c8da566 wrote (schema 1 with refusals) lost them silently and the next write made that permanent; refusals are read from any schema (a found refusal is honoured) — tested by re-reading a rendered store as schema 1. Confirmed: every reader goes through `parse_grants`; revoking the last refusal writes schema 1 again, which is safe. Noted: a build that predates refusals reads a schema-2 store as unusable, so its grants stop applying there without a message of its own. fmt, clippy -D warnings, `rapid` permissions tests 55 passed (the change is confined to the store reader).

## SEAM-09-2 — shell rules match the parsed script; a safe list in auto mode

`security::parse_shell_script` (over the existing `tokenize_shell`) splits a script into its simple commands across `;`, `&&`, `||`, pipelines, `&`, newlines and subshell parentheses, with quote removal and brace expansion, leading `NAME=value` assignments set apart, redirect targets dropped from the words (the command marked redirected), shell keywords dropped with the script marked opaque (the command after `then`/`do` still read), and a substitution marking it opaque; a heredoc, process substitution or unterminated quote is an error. `PermissionLattice::evaluate_shell(argv)` (called by the driver's `permission_for` for every `shell_exec`, plan mode included) judges a shell's `-c` script (also `-lc`/`-ec` clusters and `busybox sh -c`, nested `sh -c` read to depth 4) command by command through `ShellParts`: a managed ban, a "never allow", a deny or an ask pattern matching any one command — or the joined argv, so rules written against it keep denying — decides; an allow pattern or a grant allows only when every command is covered; a script not readable whole (substitution, heredoc, compound command, `eval`/`source`/`exec`/`command`/`builtin`, or unparseable) is never allowed by a pattern, only by the mode. `auto` mode (only) runs a call made solely of `mkdir`, `touch`, `ls`, `pwd` with no redirect or assignment (`DecisionReason::AutoSafeCommand`); a deny rule still wins. Tests: permissions shell_rules_match_the_parsed_script_command_by_command (direct argv; `;`/`&&`; a quoted `;` and a quoted `$MSG` neither bypass nor over-prompt; pipelines and subshells; substitution, backticks, heredoc, `if`, `eval` ask; the old bypass — a `bash -c git *` rule over `git status; rm -rf ~` — no longer allows; deny through an assignment, a keyword, a nested shell and a subshell in default and bypass mode; a deny against the joined argv still denies) and auto_mode_runs_only_the_safe_list_and_only_in_auto (redirects incl. `(ls) > file`, assignments, substitutions, a piped shell ask; default and acceptEdits ask; a deny rule wins); security a_script_parses_into_its_simple_commands, what_the_words_do_not_show_is_opaque_or_unparseable; exec_tools a_shell_rule_covers_a_script_only_when_it_covers_every_command (the driver: `sh -c git *` no longer runs `touch pwned` after `git status`). Revert cycles: `any` for `all` in coverage, unknown scripts covered, redirect ignored for the safe list, keyword kept as the command word, the driver judging the joined argv, a bare `(…) > file` safe — each fails a test. fmt, clippy -D warnings, cargo test --workspace 4385 passed 0 failed.

### Self-review of 2586463 and d15946b
Fixed, security: (1) a `#` inside a word ended the word and the tokenizer read the rest of the line as a comment, so `bash -c "mkdir a#;rm -rf ~"` read as `mkdir a` alone — auto mode's safe list or a `mkdir *` rule would have run `rm -rf ~` unasked, and deny rules never saw it; a comment now starts only at the start of a word (`is_unquoted_break` no longer includes `#`), which also corrects the command risk scanner; (2) the auto safe list trusted the basename — `./ls`, `/tmp/evil/touch` ran unasked; it now requires the bare name; (3) where the script is was guessed as the argument after the `-c` cluster — `bash -c -e 'rm -rf x'` read the script as `-e`; an option after `-c`, an option taking an argument (`-o`, `-O`), or a word before `-c` now make the invocation unclear (no rule covers it; the mode decides); (4) `$'…'` and `$"…"` quoting, a backslash-newline continuation and a command named by a variable (`X=rm; $X`) now make the script opaque; (5) what a wrapper runs — `env`, `sudo`, `timeout`, `nice`, `nohup`, `xargs`, … (every tail of its arguments) and `find … -exec` — is matched by deny and ask rules, "never allow" answers and managed bans (never counted toward an allow), including a `sh -c` a wrapper runs, and for a direct argv too. Tests extend shell_rules_match_the_parsed_script_command_by_command, auto_mode_runs_only_the_safe_list_and_only_in_auto and the security parser tests (a_hash_inside_a_word_is_not_a_comment). Revert cycles: `#` a word break, wrapper tails removed, basename for the safe list, options after `-c` accepted, `-o` accepted, undecoded quoting not opaque — each fails a test. d15946b: no findings. fmt, clippy -D warnings; cargo test --workspace 4385 passed, 1 failed (`twice_the_ceiling_queues_in_order_rather_than_erroring`, the recorded flake; 3/3 green alone).

### Self-review of ed62d34
Fixed, security: (1) wrapper tails recursed — each tail's own tails re-read at every level — so `env env … env rm` of n words cost ~n^4 joined strings: a crafted argv hung the permission check. Tails are now listed once, flat (a tail's tails are among them), only a tail that is a shell's script is parsed further, and the whole reading is bounded (`MAX_SHELL_PARTS`, past which the call is unknown — never allowed); an `rm` after 300 `env`s still meets its deny rule in bypass mode, in bounded time (`a_nest_of_wrappers_is_read_in_bounded_work_and_never_allowed`); (2) an unreadable script behind a wrapper left the call "known", so a wrapper's allow rule (`timeout *`, `env *`) covered `timeout 5 bash -c "$'\x72m' -rf ~"` while the deny rule could not read it — unknown now holds wherever it sits (tested in shell_rules_match_the_parsed_script_command_by_command). Revert cycles: the wrapped-unknown exemption restored, the budget removed — each fails a test. Confirmed: a comment still starts at a word's start; `exec` makes a script unknown; `cd` is not on the safe list. Environment note: this machine's `syspolicyd` holds some freshly linked test binaries in `_dyld_start` for minutes; one (`doctor_cli`) was stopped and run alone (21 passed). fmt, clippy -D warnings; cargo test --workspace 4369 passed 0 failed plus doctor_cli 21 passed.

## SEAM-09-3 — `rapid exec --allow` within the managed ceiling; the user's default mode

`rapid exec --allow <rule>` (repeatable; also `--allow=<rule>`; a rule the pattern grammar rejects is a usage error, exit 2) adds grants for that run only, beside the persisted ones — so the managed tool ban, the managed `max_permission_mode` ceiling (a plan ceiling denies the write before any grant), plan mode, deny and ask rules and "never allow" answers all still outrank it; a subagent's lattice carries them as it carries persisted grants. The user config gains `[permissions] default_mode` (a known mode name, else a config error; other keys under it reported as unknown): the mode resolution is now `RAPIDLM_PERMISSION_MODE` > the user's `default_mode` > project settings > `default`, narrowed by the managed ceiling as before. `rapid exec --help` and the reference document both. Tests: configured_model_integration binary_exec_allow_grants_the_run_only_within_the_managed_ceiling (a bad rule exits 2; default mode refuses the patch; `--allow workspace_patch(notes.txt)` applies it; with managed `denied_tools = ["workspace_patch"]` and with `max_permission_mode = "plan"` it is refused), binary_the_users_default_mode_starts_the_run_and_the_environment_overrides_it (a user `acceptEdits` default applies the patch over a project that ships plan mode; `RAPIDLM_PERMISSION_MODE=default` overrides it); user_config the_permissions_default_mode_is_a_known_mode. Revert cycles: `--allow` not applied, the user default ignored — each fails a test. fmt, clippy -D warnings; cargo test --workspace 4369 passed 0 failed, plus doctor_cli run alone (21 passed) after this machine's syspolicyd held its binary at launch.

### Self-review of c97d695 and aa91ed7
Fixed, security: (1) a script longer than the reading bound hid what came after it — `bash -c 'true; … ×512; rm -rf ~'` in bypass mode ran the `rm` past a `shell_exec(rm *)` deny rule; more generally any shell call not read whole (a substitution, undecoded quoting, the bound) fell to the mode, so bypass ran it unchecked. Now such a call asks (`DecisionReason::ShellUnreadable`) — whatever the mode — whenever a deny or ask rule, a "never allow" or a managed ban names `shell_exec`; with none, the mode decides as before (an_unreadable_shell_call_asks_whenever_a_refusal_names_shell_exec, and the long-script case in a_nest_of_wrappers_is_read_in_bounded_work_and_never_allowed); (2) the `find -exec` tails behind a wrapper were read outside the bound — now within it (a timed case); (3) a user config that exists but cannot be read made `default_mode` silently vanish, leaving the project's own mode (possibly wider) to start the run; it now warns and starts in `default`. Revert cycles: the unreadable gate off, the `find` budget off — each fails a test. Noted: `--allow` rules past `MAX_GRANTS` (persisted grants fill first) are dropped without a warning (fail-closed); the broken-config fallback has no test of its own (the run's model resolution reads the same file and fails first). fmt, clippy -D warnings, cargo test --workspace 4391 passed 0 failed.

### Self-review of c175eb9
Fixed: (1) the unreadable-user-config fallback started the run in `default` before reading the project's settings, so a project shipping `plan` (or `dontAsk`) was widened to `default`; it now takes the project's mode but never wider than `default` (`start_mode`, an_unreadable_user_config_never_widens_the_start_mode); (2) the unreadable-shell gate asked even in `dontAsk`, whose contract is to refuse silently what is not pre-approved — it now denies there (tested). Confirmed: managed ban, never-allow, plan mode, deny and ask rules all rank ahead of the gate; a refusal naming another tool does not trigger it. Revert cycles: the fallback returning the project's mode, `dontAsk` asking — each fails a test. fmt, clippy -D warnings, cargo test --workspace 4392 passed 0 failed.

## SEAM-09-4 — the approval modal shows the full script and the diff

The production approval modal (`compositor::modal_lines`, painted by `paint_screen`) now shows what the human decides on: the tool, the summary, the scope, and the body — an edit's diff, a command's full script — expanded to fill the modal, with a count of the lines that did not fit and the answer line always kept visible. The TUI's approval projection folds the `approval.requested` record's `scope` (up to 16 entries) and `diff` (cut to 64 KiB with a marker — a long diff never becomes a protocol error that freezes the session; a Secret-redacted record shows neither). A shell ask's body is the command as the shell gets it: each argument POSIX-quoted where needed (the joined summary cannot tell `["a b"]` from `["a", "b"]`), and a shell's `-c` script verbatim under `$ bash -c <script below>`. Tests: compositor the_approval_modal_shows_the_full_script_and_the_diff (a golden of the modal's lines for a multi-line script and for a patch diff — replacing the old three-line modal: tool, summary, how to answer; the old test that only checked the header remains), a_body_longer_than_the_modal_is_counted_and_the_answer_stays_visible; state an_approvals_long_diff_is_kept_in_part_never_a_protocol_error; approvals a_shell_ask_shows_the_command_quoted_and_a_script_in_full. Revert cycles: the body not expanded, a long diff read through the display bound (a fold error), the shell body as the joined argv — each fails a test. Deviation: the spec names `tui::ApprovalViewModel`; its capability, risk class, policy layer, lease clock and fingerprint have no source in the `approval.requested` record the TUI folds, so the production modal renders from the projection rather than invent them (the view model's own snapshot tests are unchanged). fmt, clippy -D warnings; cargo test --workspace 4394 passed, 2 failed — `a_shell_approval_summary_shows_the_argv_command` asserted a shell ask has no body (updated: its body is `$ git status --short`; approvals tests 4/4 green) and `computer_observe_reports_the_typed_platform_gate_not_a_stub` (the recorded host-dependent desktop test; green alone).

### Self-review of f2625e8 and f0e4227
Fixed, security: (1) the modal's new lines — a model-authored script or diff, the summary, the scope — were painted without `sanitize_untrusted`, which every other pane applies, so an escape sequence or carriage return in a `shell_exec` script (`ls\n\x1b[2J\x1b[H… run: git status`) could clear the modal and draw a harmless-looking ask over the real one; every modal line is now sanitized, and tabs expanded to spaces so a line cannot run past the modal (a_script_cannot_repaint_the_modal_with_control_characters: no ESC, CR, bidi override or tab reaches the modal, one header only); (2) the shown script disagreed with the one judged — `bash -e script.sh -c 'rm -rf ~'` displayed `rm -rf ~` as the script while bash runs `script.sh`; the body now reads where the script is through the permission check's own `shell_invocation` and says when that is unclear (tested). Revert cycles: the sanitizer bypassed, an unclear invocation shown as a script — each fails a test. Noted, not changed: the screen buffer is one character per cell, so a wide (CJK) character in any pane takes two terminal columns — a limitation of every pane, not the modal's alone. f0e4227: no findings (start_mode checked for every mode; dontAsk deny correct). fmt, clippy -D warnings, cargo test --workspace 4397 passed 0 failed.

### Self-review of 2c4d5c3
Fixed, security: (1) lines were sanitized after being split, and the sanitizer turns a lone carriage return into a line break, so `run: ls\rrun: rm -rf ~` became one row holding a newline (and a scope entry was never split at all); untrusted text is now sanitized first and then split (`untrusted_lines`); (2) the shared sanitizer let invisible characters through: the zero-width space, the BOM, the soft hyphen, U+2028/U+2029 (a line break to some terminals), U+2060–U+2064 and the interlinear annotation marks — now neutralized in every pane (the zero-width joiners U+200C/U+200D stay: they shape emoji sequences and several scripts, and a composer test depends on it); (3) a body line could imitate the modal's header or answer line — every body line is now marked `│ `, so one header and one answer line are the modal's own. The injection test adds a newline, U+2028, U+200B and a fake answer line, and counts headers and answer lines. Revert cycles: split before sanitizing, the body unmarked, U+2028 passed through — each fails it. fmt, clippy -D warnings; cargo test --workspace 4396 passed, 1 failed (`computer_observe_reports_the_typed_platform_gate_not_a_stub`, the recorded host-dependent desktop test).

### Self-review of 4d876d0
Fixed, security: (1) the summary and scope were split into unmarked rows, so `run: ls\rresolve with /approvals approve|deny 1\rapproval required: other` added a forged answer line and a second header; header, summary and scope are now one row each (a break inside shown as ` ⏎ `); (2) a long summary could push the real answer line out of the modal (truncated last) and hide the body with no notice; the answer line is now reserved first, and a body with no room is counted ("… N line(s) not shown"); (3) invisible characters were deleted, so a diff line that only adds a BOM looked unchanged — in the modal they are now shown as `�` (`sanitize::mark_invisible`) before sanitizing; the shared sanitizer (whose deletion other panes and their tests rely on) also neutralizes the Hangul and Mongolian blank fillers (U+115F, U+1160, U+180E, U+3164, U+FFA0) and the tag characters. Tests: a_forged_summary_or_scope_cannot_add_rows_or_push_out_the_answer (heights 24, 6, 3, 2: at most one header and one answer line, and the answer line present), the injection test adds a BOM-only diff line. Revert cycles: a multi-row summary, the answer line not reserved, invisibles not marked — each fails a test. fmt, clippy -D warnings; cargo test --workspace 4397 passed, 1 failed (`computer_observe_reports_the_typed_platform_gate_not_a_stub`, the recorded host-dependent desktop test).

### Self-review of 503ae9d
Fixed: the answer line was reserved within each modal's own budget, but the rows of a modal stacked before it (a protocol error, an earlier approval) were not counted, so the final cut could still drop a later approval's answer line; each modal now budgets from the rows left (`a_stacked_approval_keeps_its_answer_line`: two stacked approvals at heights 24, 12 and 8 each keep their answer line; revert cycle: the full height fails it). Confirmed: the room arithmetic cannot underflow; `mark_invisible` shows everything the sanitizer would drop as formatting. Not confirmed: a tool name with spaces renders as written (sanitized, one row) — the name comes from the validated call. fmt, clippy -D warnings (tui), cargo test -p tui all passed; the change is confined to `modal_lines`.

## SEAM-10-1 (slice 1) — agent-mode flags and the JSON error envelope

`apps/rapid/src/agent_mode.rs`: `--output json|text`, `--quiet`, `--non-interactive`, `--dry-run`, `--yes`, `--no-color`, `--timeout <secs>` before the subcommand, with `RAPIDLM_*` mirrors (and `NO_COLOR`), parsed once in `run()` before dispatch (a bad value exits 2). Each subcommand declares its forms (`agent_mode::forms`): a native JSON/quiet/dry-run form is passed through; `--dry-run` on a read-only command is accepted; on a state-changing command without one it is refused before anything runs; `--non-interactive` refuses the TUI (`rapid`, `rapid resume`). `--timeout` stops the run with exit 130. `protocol::cli::CliError {code, message, hint}` with a v1 fixture (`cli/v1/error_envelope.json`); every `InteractiveError` has a stable `code()` and a `hint()` naming the next command; `main` writes the envelope on stderr in JSON mode, else the message and `hint: …`. Exit codes unchanged. Tests: agent_mode flags_come_before_the_command_and_win_over_their_mirrors, every_subcommand_honours_or_refuses_each_flag (table over every entry of SUBCOMMANDS); agent_mode_cli (envelope via flag and env with exit 2 kept, text hint, dry-run refused for `trust grant`, accepted for `completions`, non-interactive refusals, a bad timeout, the watchdog stopping `acp` with 130); protocol cli_error_envelope_v1_fixture_matches_wire_contract. Revert cycles: envelope off, dry-run refusal off, watchdog off — each fails a test. Deviation: stdout that is not a terminal defaults to JSON only in agent mode (`--non-interactive`), so existing pipelines keep their text. `--yes` and `--no-color` are accepted: no subcommand prompts or colours its output. Remaining: `session.created` origin and `sessions list --origin`. fmt, clippy -D warnings; cargo test --workspace 4404 passed, 1 failed (`computer_observe_reports_the_typed_platform_gate_not_a_stub`, the recorded host-dependent desktop test).

### Self-review of 4229732
Fixed: (1) an agent-mode flag's value was read as the subcommand — `rapid --output json --help` was a usage error and `rapid --timeout 30` never reached the TUI; the launch is now classified on what follows the owned flags; (2) `--dry-run` was appended for every `worktree`/`mcp` verb though only `reclaim` and `install` take it — it is now verb by verb (`list`, `get`, `probe` read only; other verbs refused); (3) agent-mode refusals (a bad flag value, the TUI under `--non-interactive`, a flag a command cannot honour) printed plain text and returned exit 2 directly, bypassing the envelope — they are now `InteractiveError::Refused` and reach `main`; (4) a p9 handler's error lost its message in the envelope — it now carries it, and where the handler already printed its reason before a bare `Usage` the envelope says so. Tests: agent_mode_flag_values_are_not_the_command_and_refusals_are_envelopes; the table test checks each verb. Revert cycles: classifying on the raw args, verb-blind dry run — each fails a test. Remaining (recorded): the ~100 p9 handlers that print their own usage reason still write it as plain text ahead of the envelope; converting them to carry it is follow-up work. fmt, clippy -D warnings, cargo test --workspace 4406 passed 0 failed.

### Self-review of 3b53163
Fixed: `mcp probe` was classed read-only under `--dry-run`, but it starts every configured server's command — it is now refused (tested; revert cycle: read-only fails it). Confirmed: slicing after the owned flags keeps the command and its operands (`--jsonl` included), `--version` still works, `run_subcommand` does not re-parse agent flags, every `InteractiveError` match handles `Refused`, no test relied on the old p9 stderr text. fmt, clippy -D warnings, agent_mode tests green; the change is one match arm in `dry_run_for`.

## SEAM-10-1 (complete) — session origin and `sessions list --origin`

Agent-mode flags parsed once before dispatch with RAPIDLM_* mirrors, each honoured per command or refused before anything runs (agent_mode.rs; commits 4229732, 3b53163, 7aee787); protocol::cli::CliError envelope with a v1 fixture, every InteractiveError with a code and a hint, written on stderr in JSON mode (exit codes unchanged, pinned by agent_mode_cli). session.created gains `origin` (headless, interactive, acp, daemon, workflow, loop; omitted when unnamed, so older records are unchanged); SessionSummary reads it from the ledger; `rapid sessions list|search [--origin <origin>]` prints `origin=` (`unknown` for older sessions) and filters by it. Tests: every_subcommand_honours_or_refuses_each_flag (table over every subcommand), agent_mode_cli, cli_error_envelope_v1_fixture_matches_wire_contract, binary_a_headless_session_records_its_origin_and_sessions_list_filters_by_it. Revert cycles: envelope off, dry-run refusal off, watchdog off, flag values read as the command, verb-blind dry run, probe read-only, headless origin removed — each fails a test. Deviations: non-TTY JSON default only in agent mode (--non-interactive); p9 handlers that print their own usage reason still print it as text ahead of the envelope (which points at it); --yes/--no-color accepted as no-ops (no command prompts or colours). fmt, clippy -D warnings, cargo test --workspace 4407 passed 0 failed.

### Self-review of 5012ca4
Fixed: (1) the daemon client dropped the caller's origin and the server always wrote `daemon` — `create_session` now carries `origin` over IPC and the server records it when it is a known surface, else `daemon` (a_session_created_over_the_daemon_keeps_its_callers_origin: named, none, forged); (2) `sessions list --origin <typo>` printed `count=0` and exited 0 — an origin that is not a known surface (or `unknown`) is a usage error (`SESSION_ORIGINS`, `is_known_origin`). Revert cycles: origin not sent, validation off — each fails a test. Noted, not changed: a fork (`session/fork.rs`) inserts its session row without a `session.created` event, so it lists as `origin=unknown`. Confirmed: SQLite's JSON1 is built into the bundled library; no strict reader of `session.created` rejects the new field; every non-test creator is tagged. fmt, clippy -D warnings; cargo test --workspace 4406 passed, 2 failed — `a_driver_lease_becomes_available_again_once_dropped` (recorded flake; 3/3 green alone) and `computer_observe_reports_the_typed_platform_gate_not_a_stub` (recorded host-dependent test).

### Self-review of 72fcfbd
No findings (old/new daemon and client interoperate; `origin: null` decodes; search shares the validation).

## SEAM-11-1 — MCP elicitation through the approval wait

MCP elicitation through the approval wait (ADR 0022 §3). The client declares the `elicitation` capability; `McpSession::tools_call_with` answers every request the server makes during a call through a handler (never leaving one unanswered), and a server request that reuses the pending call's id is no longer taken for its response (`has_response_id` requires no `method`). `execute_mcp_tool` handles `elicitation/create`: with an answer source (the continuation of an answered wait installs one) the answer is checked against the requested schema (a JSON object; required properties present; primitive types matched; unknown fields refused) and sent as `accept`, and a non-fitting answer is a typed refusal, never sent; with none it cancels the request so the call ends, then raises an approval wait with source `mcp:<server>`, the message as summary and the schema as the body; headless with no surface it is `ContextRequired`. In the TUI `/approvals answer <n> <json>` answers it and re-runs the call with the answer (the ask_user answer continuation), and `approve` is refused. Durable across restart as every approval is: the pending wait and its answer live in the ledger. Tests: exec_tools an_mcp_request_for_input_is_asked_answered_and_checked (stub server: no surface → input required; a sink → a wait sourced `mcp:demo`; an answer → accepted and greeted; a mistyped answer → typed refusal; the request reuses the call's id), interactive an_mcp_request_for_input_is_answered_not_approved. Revert cycles: capability undeclared, request taken for the response, schema check off, answer gate off — each fails a test. fmt, clippy -D warnings; cargo test --workspace 4409 passed, 1 failed (`computer_observe_reports_the_typed_platform_gate_not_a_stub`, the recorded host-dependent desktop test).

### Self-review of 985a851
Fixed: (1) no bound on the requests a server may make during one call — a server looping `elicitation/create` spun (each answered `cancel` at once) until the 30 s watchdog; `tools_call_with` now fails the call past `MAX_SERVER_REQUESTS_PER_CALL` (16) and closes the session, whose next frames would be out of step; (2) a second request for input in one call, after the first was answered and sent, was raised as a new approval wait although the server already had the first answer — one request per call is honoured, a second fails the call with nothing further sent. Tests extend an_mcp_request_for_input_is_asked_answered_and_checked (a server asking twice; one asking 10 000 times, stopped promptly, its session closed so a well-answered call no longer reaches it). Revert cycles: the one-ask rule off, the bound off — each fails it. Confirmed: headless `rapid exec` installs no stdin answer source, so a server cannot prompt the terminal; the one-shot answer cannot be taken twice. fmt, clippy -D warnings; cargo test --workspace 4409 passed, 1 failed (`computer_observe_reports_the_typed_platform_gate_not_a_stub`, the recorded host-dependent test); the last change after that run only tightens the test.

## SEAM-11-2 — web_search behind a backend, bounded, filtered and receipted

`web_search` (apps/rapid/src/web_search.rs): `[toolset.web_search]` in the user config (strict: backend, endpoint, allowed_domains, excluded_domains, max_results 1..=10; a bad section is a warning and the tool stays unavailable); a `SearchBackend` trait with the `json` backend — a POST of {q, count} to the endpoint through `ProviderEgress` (NetworkClient::Tool) and the HTTP/1 transport, reading {results:[{url,title,snippet}]}; results filtered by the domain lists (a subdomain of an allowed domain passes; a look-alike host or a subdomain of an excluded one does not), capped at max_results, fenced as `<search-results source="web_search" untrusted="true">` with bounded titles/URLs/snippets and the omitted count; no backend configured is typed unavailability. Receipts: every web_search egress decision and every web_fetch (allowed, or refused with the reason) appended to `.rapidlm/egress-receipts.jsonl` (rotated past 1 MiB). Tests: web_search the_section_is_parsed_strictly, results_pass_only_the_domain_lists; exec_tools web_search_is_unavailable_until_configured_then_filtered_fenced_and_receipted, the_json_search_backend_goes_through_the_egress_gate_and_parses_results (a local fixture; the gate's receipt names its port; web_fetch's refusal receipted); the surface test now lists seventeen tools. Revert cycles: domain filter off, cap off, receipts off — each fails a test. Deviation: the json backend POSTs (the HTTP client has no GET); a GET-only engine such as SearxNG needs a small adapter in front. fmt, clippy -D warnings; cargo test --workspace 4412 passed, 2 failed — the surface-count test (updated for the new tool; green) and `computer_observe_reports_the_typed_platform_gate_not_a_stub` (the recorded host-dependent test).

### Self-review of 2278483
Fixed, security: a result's title, URL or snippet (or the query) went into the fence unescaped, so `</search-results>` in a snippet closed it and a newline could forge a numbered entry — everything after read as trusted. Every field is now made inert (`<`/`>` as `‹`/`›`, quotes, line breaks and control characters as spaces) and kept to one line (a_result_cannot_close_the_fence_or_forge_an_entry: one closing tag, no injected element, exactly one entry; revert cycles: brackets kept, line breaks kept — each fails it). Confirmed: web_search is gated like web_fetch (Net, read-only, tool-name rules, managed bans); `admitted` handles case, userinfo, ports and IPv6 through `host_of`. Noted: web_search has no rule subject (rules match the tool name only); receipt writes are best effort, and under `--worktree` they land in the worktree's `.rapidlm/`. fmt, clippy -D warnings, web_search tests green; the change is confined to `render`.

## SEAM-12-1 (slice 1) — RAPIDLM_SESSION_ID / RAPIDLM_TURN_ID at every spawn site

`apps/rapid/src/run_identity.rs`: a run's identity (session, turn) held per thread and set around the work that spawns, so two sessions in one process (the daemon) never see each other's ids. `WorkspaceTools::set_run_identity` (set on every turn path: headless exec with its turn id; the interactive, continuation and workflow turns through `LedgerSinks`, the turn read from the kernel's active turn) and `execute_call` scopes it around each call. Exported by: a tool's foreground command and background job (`shell_exec`), every hook (`hooks::command_builder`), an MCP server (session only — it outlives a turn); the status command already exported both (SEAM-07-3). Test: binary_a_tools_command_and_a_hook_carry_the_session_and_turn_ids (a scripted `shell_exec` and a `pre_tool_use` hook each write both variables; the session is the one `sessions list` shows, the turn a full id). Revert cycles: the call not scoped, hooks not exporting — each fails it. Remaining in SEAM-12-1: `/aside`. fmt, clippy -D warnings; cargo test --workspace 4415 passed, 1 failed (`computer_observe_reports_the_typed_platform_gate_not_a_stub`, the recorded host-dependent test).

### Self-review of d4a7d01
Fixed: (1) sandboxed commands and jobs got no ids — both sandbox backends clear the environment; `SandboxSpec` gains `env_var` (validated names, values bounded and NUL-free, at most 16), set by the host-restricted and seatbelt backends after the clear, and `sandbox_exec::build_spec` passes the run identity (a_specs_variables_reach_the_process_and_nothing_else_does: the variable arrives, `HOME` does not; revert cycle: the backend not setting them fails it); (2) a sandboxed job builds its spec on its own worker thread, where the per-thread identity was empty — the worker now carries the caller's identity; (3) hooks the host fires (prompt_submit, turn_end, notify) and MCP servers started during a TUI turn ran outside any tool call — the interactive and continuation turn threads now set the identity at their start (`run_identity::set_current`). Noted, not changed: in headless `rapid exec`, `session_start` hooks and MCP servers start before the session exists, so they carry no id; parallel batch threads re-scope per call (confirmed); no stale identity can leak through a reused MCP connection. fmt, clippy -D warnings; cargo test --workspace 4416 passed, 1 failed (`computer_observe_reports_the_typed_platform_gate_not_a_stub`, the recorded host-dependent test).
