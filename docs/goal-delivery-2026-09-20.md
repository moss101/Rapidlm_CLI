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

Contract restated: `apps/rapid/src/hooks.rs` (the production hook runner), `apps/rapid/src/exec_tools.rs` (the one call site, `execute_call_traced_flagged`), `crates/protocol` (the wire type), `crates/event-ledger` (`hook.decided`), the SDK wire catalog. Migration impact: none for v1 hooks (exit-code contract byte-identical; no ledger record); one new additive event kind.

| Criterion | Status | Evidence |
|---|---|---|
| AC-01 every existing hook test passes unchanged; a v1 exit-code hook behaves byte-identically | done | the eleven pre-existing `hooks::tests` and the two `exec_tools` hook tests pass with their assertions unchanged (three `panic!` arms widened from `Allowed =>` to `other =>` because the outcome enum gained `Ask`); `plain_stdout_text_keeps_v1_semantics_and_records_nothing` pins a chatty exit-0 hook (allowed, silent), a stdout-only reason on exit 3 (still the detail) and stderr winning over stdout; `a_v1_hook_records_no_decision` pins an empty sink for a v1 hook |
| AC-02 `allow`, `deny`, `defer` each prove their effect on dispatch and their ledger record | done (`ask` in SEAM-01-2) | `v2_hook_decisions_are_applied_on_dispatch_and_recorded_for_the_ledger` (deny → `Denied` with the reason, no file; allow → `Succeeded`; defer → `Succeeded`; one record each naming hook, tool, call); `a_deferring_hook_leaves_the_decision_to_the_permission_flow` (the lattice runs first — a rule-denied call never reaches the hook stage; on an allowed path defer lets the call run); `a_v2_hook_decision_reaches_the_ledger_as_hook_decided` (a real interactive turn: `hook.decided` with `record`, `hook`, `event`, `decision`, `tool`, `call_id`, `command_digest`, `reason_digest` — no reason text — appended before `tool.denied`) |
| `ask` is never an implicit allow | done, interim | `a_hook_ask_is_a_stated_denial_until_it_can_reach_an_approval_surface`: denied with "held by pre_tool_use[0] hook: … (a hook's ask cannot reach an approval surface in this build …)", the `ask` recorded; SEAM-01-2 replaces the denial with the approval wait |
| Malformed or unknown results deny naming the hook | done | `an_unreadable_v2_result_denies_naming_the_hook` (`decision: maybe`; `version: 9`); `a_v2_deny_beside_a_nonzero_exit_lends_its_reason_and_a_v2_allow_does_not_rescue_it` (exit code stays fail-closed); `a_timed_out_hooks_partial_stdout_is_not_a_decision` |
| Later denial beats earlier ask; both recorded | done | `a_v2_ask_is_reported_unless_a_later_hook_denies` |
| Streams separated; post/notify hooks still record both | done | `run_hook_once` writes stdout and stderr to two files; `post_hooks_record_both_streams_stdout_first` |
| AC-08 fixture under `crates/protocol` | done | `crates/protocol/tests/fixtures/hooks/v2/hook_result.json`, `hook_result_v2_fixture_matches_wire_contract`; the parser's own tests in `protocol::hooks::tests` (v1 text is `None`, the four words plus `block`, schema/version refusal, typed and bounded optional fields, char-boundary truncation, grant keys observed not honoured, round trip) |
| Revert cycle | done | with `HookResult::from_stdout` fed an empty stdout (the pre-change world) the nine new v2 tests fail and every v1 test still passes; restored, all green |
| SDK wire catalog | done | `wire.v1.json` + regenerated `sdk/typescript/src/generated/index.ts` carry `hook.decided`/`hook`; the SDK's event-kind count pin moves 105 → 106 (that pin is the catalog's drift check); `pnpm generate:check`, `typecheck`, `test` green |
| Required checks | done, one environmental failure disclosed | `cargo fmt`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test -p protocol --test schema_fixtures`, the three pnpm checks all green; `cargo test --workspace` green except `interactive::tests::computer_observe_reports_the_typed_platform_gate_not_a_stub`, which drives the real desktop stack and fails on this machine with `computer-use observation failed: backend` alone and in the suite, on a diff that touches no computer-use code; CI on `main` (`7ec2a1f`, `89350d7`) passes it on the macOS runner — a host-permission condition of this session's process, not a regression |
| Rule 2.4 | done | `hooks.rs` no longer names a peer product in its module doc or the timeout constant |

Disclosed: hook identity is positional (`pre_tool_use[<index>]`) plus a 12-hex command digest — settings hooks have no ids of their own; a hook that prints a grant-shaped key has `grant_attempted: true` in its record and the keys are ignored (invariant 13).
