# Goal delivery — GVS5H Phase 1, first three slices: production caller, replay reducer, single-event acceptance (2026-09-18)

Baseline: `b63793a` (the last commit of the 2026-09-17 delivery). Scope: the first Phase 1
slice ADR 0021 §"Consequences" orders before everything else — a production caller for
`scheduler::GraphService`/`GraphBackedRun` and the supervisor, so that the later slices
(replay, single-event acceptance, identities, recovery) have something real to attach to.

## What `rapid run --orchestration verified` does

| Criterion | Status | Evidence |
|---|---|---|
| A production path constructs `GraphService` and `GraphBackedRun` | done | `apps/rapid/src/workflow_verified.rs::VerifiedRun::open` — called from `p9_commands::run_run_command` when `orchestration.mode = verified`; the playbook's steps become the graph's plan (`RunPlan`, `GraphBackedRun::start_planned`), one node per step, `DependsOn` per declared dependency, the entry steps under the goal root |
| Every step transition is a durable graph event before the run state changes | done | `execute_run` calls `step_started`/`step_succeeded`/`step_failed`/`step_cancelled`/`step_waiting` before the matching `RunState` mutation; `GraphService` appends before it applies. `a_verified_run_records_every_transition_and_accepts_only_on_fresh_checks` reads the nine `graph.node_state_changed` events back from the ledger session in order |
| The graph and the run state must agree | done | `VerifiedRun::confirm_ready` compares the graph's ready set with the run loop's before every batch; a disagreement ends the run as `RunOutcome::OrchestrationFailed`, never picks a side |
| `verified` is the supervisor's acceptance | done | The verification steps are the contract's mandatory requirements (`REQ-<n>`/`AC-<n>` by playbook position); their real results are the checks (`ReplayedChecks` replays, never re-runs); the host verdict requires a passing check *from this invocation* per requirement; `GraphBackedRun::accept` refuses while any verification node is failed/cancelled. Resuming a finished run is `CompletedUnverified { unmet }` with `accepted: false`, `state: Blocked` (same test) |
| Retries are graph transitions | done | `running → failed → pending (retry) → running → succeeded` in the ledger for a leaf step; a retrying *dependency* is not cancelled and its dependent still runs (`a_failed_check_retries_on_the_graph_and_a_final_failure_is_never_accepted` asserts both); a final failure leaves the node `failed` and the run unconcluded (`state: Implementing`, nothing accepted) |
| Human steps wait on the graph | done | `graph.wait(node, token)` → `waiting`; the approved resume mirrors `succeeded` and runs what is now ready (`a_human_step_waits_on_the_graph_and_the_approved_resume_is_accepted`) |
| A ledger that stops taking appends stops the run | done | `a_ledger_that_stops_taking_appends_ends_the_run_with_nothing_accepted`: the first transition fails, the step never starts, `OrchestrationFailed`, `accepted: false` |
| Refuses what it cannot verify | done | No verification step → `VerifiedRunError::NoVerificationSteps`; a verification command over the evidence bound (4 KiB) → `CommandTooLong` — both before anything runs |
| The run-state file names the graph | done | `RunState.orchestration: Option<OrchestrationRecord { mode, session, graph_id, task_id, state, accepted }>` (`skip_serializing_if` none: the default path's file is byte-identical); `rapid run --status` and the final JSON report show it only when present |
| Configuration precedence is the existing one | done | `--orchestration <off\|verified>` is a `ConfigOverride` on `orchestration.mode` resolved by `kernel::load_config` through `interactive::workflow_config` (CLI > env > user > workspace > defaults); `the_orchestration_flag_is_the_cli_layer_of_the_ordinary_config_precedence` pins the order and that the default is `off` |
| The `off` path's behaviour is unchanged for a run that never went verified | done | `execute_run(…, None)` runs the same steps to the same outcome; a run-state file with no `orchestration` record stays without one and the JSON report carries no new key. Two deliberate, disclosed differences: a run that *started* verified and is resumed with the mode off keeps its graph pointer marked `abandoned` (it cannot be silently accepted on the off path), and `rapid run` (when a run actually executes — not `--status`/`--resolve`) now resolves `RapidConfig` the way `rapid exec` does, so a malformed config refuses it |
| Real CLI smoke | done | A temp project with `process` + `verification` steps: `rapid run smoke.json --orchestration verified` → `verified: true`, `orchestration.state: Accepted`; `--status` prints the record; `RAPIDLM_ORCHESTRATION_MODE=verified rapid run --resume <id>` → `accepted: false`, `state: Blocked`, `unmet: ["check"]`; `rapid inspect-export <session>` lists `graph.created`, `graph.revision_committed`, `graph.node_state_changed`… ; plain `rapid run smoke.json` prints nothing about orchestration |

## Run-loop defects found by the review, fixed in the follow-up commits

The graph mirror made three pre-existing `rapid run` readiness rules visible by disagreeing with them (or by turning them into stops):

- A dependency's *first* failure cancelled its dependents, so "retries in place" was only true for leaf steps; now only a failure with no attempt left (or a cancellation) cancels dependents — the graph's `PredecessorSucceeded` rule (`a_retrying_dependency_keeps_its_dependents_waiting_rather_than_cancelling_them`).
- A failed step with attempts left was re-queued without checking its dependencies (it could run beside a dependency `load_run` had reset); now it waits like any other step (`a_retryable_failure_waits_for_its_dependencies_like_any_other_step`). A denied or refused human step records `u32::MAX` attempts on the counter too, so it is never mistaken for retryable.
- A human step sharing a ready batch left its siblings `Running` forever (`ff5c45f`).
- `fresh_verification` persisted across invocations, so a resume that ran nothing reported `verified: true`; it is now cleared at the start of every invocation (finished ≠ verified, as the module doc always said), and a run that started verified and continues with the mode `off` keeps its graph pointer with `state: "abandoned …"`, `accepted: false` (`fresh_means_this_invocation_and_an_off_resume_abandons_a_verified_record`).
- Under verified orchestration the whole batch starts on the graph before any run-state entry flips, so a refusal partway leaves every step `Pending` for the resume.
- `rapid run --status`/`--resolve` no longer resolve configuration (they never needed it); a flag without its value is a usage error, not a panic; an empty `command` is rejected at load and an empty `label` falls back to the key.

A second review of those fixes found a regression they introduced and two gaps, all fixed with revert-cycled tests:

- The graph mirror read the retry ceiling straight off the `attempts` counter while the run loop read `failed_for_good` (which also folds in a `Failed{u32::MAX}` count). A `rapid run --resolve <id> deny` writes `Failed{u32::MAX}` without the counter, so a `--resume --orchestration verified` had the mirror re-queue the denied gate on the graph while the loop treated it as final — `graph and run state disagree`, on every resume. `mirror` now uses the shared `failed_for_good`, and `resolve_command`'s deny writes the counter too (`a_denied_human_step_resumed_verified_re_asks_rather_than_wedging`).
- `reset_step` promised to un-cancel a retried step's dependents and did not, so `--retry` could not recover a run whose dependents were cancelled by a final failure (and verified acceptance refuses a cancelled verification node). It now un-cancels them transitively (`reset_step_uncancels_the_dependents_a_final_failure_cancelled`).
- `fresh_verification` was cleared in memory but not persisted when a resume ran nothing, so `--status` kept reading a stale `[verified fresh]`. The cleared state is now saved before the first batch (`a_resume_that_runs_nothing_persists_the_cleared_freshness`).
- Truthfulness: the off-path row is split into the unchanged case and the two disclosed differences; the retry row states what it actually asserts; the `monitor` step help no longer claims a repeat-until-success poll (it runs once, like `process`).

## Substrate fixes the slice needed

- `scheduler::NodeSpec` gains `max_attempts` (serde default 3) so a proposal carries the
  step's retry ceiling; `GraphBackedRun` gains `RunPlan` / `start_planned`, `task_nodes` /
  `verify_nodes`, and an `accept` that checks the verification nodes before touching the
  supervisor.
- `GraphService` attempt counting was retries-only: `attempts` incremented on `retry()` and the
  gate was `attempts + 1 <= max_attempts`, so a node compiled with `max_attempts: 1` retried
  once, and the playbook compiler's per-step ceiling meant something different from the
  service's. Now every `Pending`/`Ready → Running` is an attempt (`Paused → Running` is not),
  `retry()` requires `attempts < max_attempts` and does not count itself, and the
  `graph.node_state_changed` payload carries `attempts`. `docs/development-ledger.md` P2-016/017
  updated to say so.

## Second slice: payload-complete graph events and a replay reducer (GVS-005, partial)

| Criterion | Status | Evidence |
|---|---|---|
| Graph events carry enough to rebuild | done | `GraphCreated` gains the whole `graph`, `GraphRevisionCommitted` the validated `proposal` (`crates/scheduler/src/service.rs`); `GraphNodeStateChanged` already carried `node_id`/`state`/`attempts`. SDK wire types are unchanged — graph payloads are opaque `JsonValue` there |
| A reducer rebuilds a session's graphs from events alone | done | `GraphService::replay(session) -> GraphReplay` re-applies each proposal through the same `validate_and_apply` and folds node-state/attempts changes, skipping non-graph events; `replay_rebuilds_the_live_graphs_after_a_restart_at_every_transition` builds a create/propose/fan-out/run/fail/retry history through the live API, reopens the ledger, and asserts the replayed graph equals the uninterrupted one |
| Historical (pre-payload) events are explicit, not guessed | done | A `graph.created`/`graph.revision_committed` without its payload reduces the whole replay to `GraphReplay::Unsupported { first_seq }` (`replay_reports_unsupported_for_a_pre_payload_created_event`); nothing in production created graphs before this, so no real history is affected |
| Full crash-at-each-boundary matrix; a resume-time caller | pending | The reducer is the durability primitive GVS-006 (single-event acceptance) and GVS-008 (recovery) consume; `VerifiedRun` still mirrors the run-state file at resume. GVS-005's crash-injection matrix lands with GVS-008 |

## Third slice: acceptance is one durable record (GVS-006, core)

Before: `Supervisor::accept` applied the transition and emitted in memory, then `GraphBackedRun::accept` called `set_state` once per node — three or more durable writes with no single acceptance record, so a crash between them left the supervisor and the graph disagreeing about whether the run was accepted (the baseline audit's confirmed finding).

| Criterion | Status | Evidence |
|---|---|---|
| Validation is separable from reduction | done | `Supervisor::acceptance_record()` checks state, verdict, attestation identity and evidence for every mandatory requirement, and mutates nothing; `apply_acceptance(&record)` only reduces (and is idempotent, refusing a record for another task). `accept()` remains both for `goal claim`, which owns no second projection |
| Acceptance is one durable append, before any mutation | done | `GraphBackedRun::accept` appends a single `orchestration.task_accepted` carrying the goal node, verification nodes, task id, candidate digest and verdict, then reduces both projections; `accept_refuses_while_a_verification_node_failed_and_skips_succeeded_ones` asserts the appended kinds are exactly `[OrchestrationTaskAccepted]` — no per-node state events |
| A failed append is no false completion and no split projection | done | `a_failed_acceptance_append_leaves_both_projections_untouched`: with the ledger file gone the accept returns `Sink`, the supervisor stays `Verified` and the goal node stays `Pending`. Revert-cycled — the old shape advances the supervisor to `Accepted` while the append fails |
| The record alone rebuilds the acceptance *on the graph* | done | `GraphService::replay` folds `orchestration.task_accepted`, so the replayed graph equals the live one with acceptance included (asserted in the same test). The supervisor snapshot has no reducer yet — restoring it from the stream is GVS-008 — so this is the graph half, not full cross-process recovery |
| A shared wire kind cannot poison the replay | done | `orchestration.task_accepted` is also the supervisor's own event kind, so the graph's record carries a `record: "rapidlm.graph.acceptance/v1"` marker and `replay` skips any payload without it (`replay_restores_a_wait_token_and_ignores_another_producers_task_accepted`). Revert-cycled: without the marker a foreign event fails the whole session's replay |
| An interrupted reduction is reconciled, not stranded | done | The append is the single durable write, but the two reductions after it are not atomic with it. `apply_acceptance` now commits its state change last, after every fallible step, and a repeated `accept` on an already-accepted run re-applies the (idempotent) graph reduction instead of returning `Ok` on the supervisor's state alone (`a_repeated_accept_reconciles_a_graph_an_interrupted_reduction_left_behind`, revert-cycled) |
| Repeating the request cannot apply twice | done | An already-`Accepted` run returns `Ok` without appending (asserted); `apply_acceptance` is a no-op in that state |
| Publication receipts (prepared → applied → reconciled via `TransactionManager`) | pending | The other half of GVS-006; it lands with the workspace-transaction wiring, which `agent_views::integrate` also needs |

Two consequences worth stating rather than discovering later: the goal node's completion no longer produces a `graph.node_state_changed` event (it rides the acceptance record), so an external consumer of that published wire kind sees node states for every step but not the goal's completion; and a `graph.node_state_changed` now carries the node's `wait_token`, which `replay` restores — without it a replayed waiting node no longer knew what it waited on, and the GVS-005 "replayed == live" claim held only for wait-free histories.

## Explicitly not delivered (later Phase 1 slices, unchanged plan)

- **Replay (GVS-005):** a resumed run opens a *fresh* graph in a new ledger session and sets its
  nodes to the recorded step states; the run-state file — not the events — is still the
  cross-invocation authority, and attempt counts across invocations are its. The
  `OrchestrationRecord` pointer is what the reducer will start from.
- **Identities (GVS-007):** the workspace identity is the digest of the steps' `watch` globs
  (nothing, for a playbook without them), with `require_workspace_identity: false` as in `goal
  claim`.
- **Recovery (GVS-008):** a step left `Running` by a crashed invocation is mirrored as
  `running` and, as on the default path, never re-queued; `reset_step` refuses it too.
- Model-facing surfaces, `agent_views::integrate` over `TransactionManager`, and everything in
  Phases 2–7.
