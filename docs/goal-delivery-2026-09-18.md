# Goal delivery — GVS5H Phase 1, first slice: the production caller (2026-09-18)

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
| Retries are graph transitions | done | `running → failed → pending (retry) → running → succeeded` in the ledger (`a_failed_check_retries_on_the_graph_and_a_final_failure_is_never_accepted`), for a leaf step and for a step with dependents; a final failure leaves the node `failed` and the run unconcluded (`state: Implementing`, nothing accepted) |
| Human steps wait on the graph | done | `graph.wait(node, token)` → `waiting`; the approved resume mirrors `succeeded` and runs what is now ready (`a_human_step_waits_on_the_graph_and_the_approved_resume_is_accepted`) |
| A ledger that stops taking appends stops the run | done | `a_ledger_that_stops_taking_appends_ends_the_run_with_nothing_accepted`: the first transition fails, the step never starts, `OrchestrationFailed`, `accepted: false` |
| Refuses what it cannot verify | done | No verification step → `VerifiedRunError::NoVerificationSteps`; a verification command over the evidence bound (4 KiB) → `CommandTooLong` — both before anything runs |
| The run-state file names the graph | done | `RunState.orchestration: Option<OrchestrationRecord { mode, session, graph_id, task_id, state, accepted }>` (`skip_serializing_if` none: the default path's file is byte-identical); `rapid run --status` and the final JSON report show it only when present |
| Configuration precedence is the existing one | done | `--orchestration <off\|verified>` is a `ConfigOverride` on `orchestration.mode` resolved by `kernel::load_config` through `interactive::workflow_config` (CLI > env > user > workspace > defaults); `the_orchestration_flag_is_the_cli_layer_of_the_ordinary_config_precedence` pins the order and that the default is `off` |
| The `off` path is unchanged | done | `execute_run(…, None)` takes exactly the branches it took before; the JSON report and run-state file carry no new key; the only new work on that path is resolving `RapidConfig` the way `rapid exec` already does (a malformed config file now refuses `rapid run` as it refuses every other command) |
| Real CLI smoke | done | A temp project with `process` + `verification` steps: `rapid run smoke.json --orchestration verified` → `verified: true`, `orchestration.state: Accepted`; `--status` prints the record; `RAPIDLM_ORCHESTRATION_MODE=verified rapid run --resume <id>` → `accepted: false`, `state: Blocked`, `unmet: ["check"]`; `rapid inspect-export <session>` lists `graph.created`, `graph.revision_committed`, `graph.node_state_changed`… ; plain `rapid run smoke.json` prints nothing about orchestration |

## Run-loop defects found by the review, fixed in the follow-up commits

The graph mirror made three pre-existing `rapid run` readiness rules visible by disagreeing with them (or by turning them into stops):

- A dependency's *first* failure cancelled its dependents, so "retries in place" was only true for leaf steps; now only a failure with no attempt left (or a cancellation) cancels dependents — the graph's `PredecessorSucceeded` rule (`a_retrying_dependency_keeps_its_dependents_waiting_rather_than_cancelling_them`).
- A failed step with attempts left was re-queued without checking its dependencies (it could run beside a dependency `load_run` had reset); now it waits like any other step (`a_retryable_failure_waits_for_its_dependencies_like_any_other_step`). A denied or refused human step records `u32::MAX` attempts on the counter too, so it is never mistaken for retryable.
- A human step sharing a ready batch left its siblings `Running` forever (`ff5c45f`).
- `fresh_verification` persisted across invocations, so a resume that ran nothing reported `verified: true`; it is now cleared at the start of every invocation (finished ≠ verified, as the module doc always said), and a run that started verified and continues with the mode `off` keeps its graph pointer with `state: "abandoned …"`, `accepted: false` (`fresh_means_this_invocation_and_an_off_resume_abandons_a_verified_record`).
- Under verified orchestration the whole batch starts on the graph before any run-state entry flips, so a refusal partway leaves every step `Pending` for the resume.
- `rapid run --status`/`--resolve` no longer resolve configuration (they never needed it); a flag without its value is a usage error, not a panic; an empty `command` is rejected at load and an empty `label` falls back to the key.

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

## Explicitly not delivered (later Phase 1 slices, unchanged plan)

- **Replay (GVS-005):** a resumed run opens a *fresh* graph in a new ledger session and sets its
  nodes to the recorded step states; the run-state file — not the events — is still the
  cross-invocation authority, and attempt counts across invocations are its. The
  `OrchestrationRecord` pointer is what the reducer will start from.
- **Single-event acceptance (GVS-006):** `GraphBackedRun::accept` is still the supervisor's
  transition followed by graph `set_state` calls.
- **Identities (GVS-007):** the workspace identity is the digest of the steps' `watch` globs
  (nothing, for a playbook without them), with `require_workspace_identity: false` as in `goal
  claim`.
- **Recovery (GVS-008):** a step left `Running` by a crashed invocation is mirrored as
  `running` and, as on the default path, never re-queued; `reset_step` refuses it too.
- Model-facing surfaces, `agent_views::integrate` over `TransactionManager`, and everything in
  Phases 2–7.
