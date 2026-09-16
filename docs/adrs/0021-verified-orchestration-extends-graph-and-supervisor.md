# ADR 0021 — Verified orchestration extends the existing graph and supervisor

**Status:** Accepted for `GVS-RAPIDLM-01` (Phase 0, GVS-002)  
**Date:** 2026-09-17  
**Baseline audit:** [GVS-001](../goals/gvs5h-phase0-baseline-2026-09-17.md)  
**Supersedes nothing; builds on:** ADR 0002 (event-sourced durable state), 0003 (operation journal), 0004 (graph-native orchestration), 0011 (transactional workspaces), 0012 (evidence-backed goals), 0013 (independent verifier), 0020 (migration over rewrite)

## Decision

The GVS5H-inspired adaptive, verified workflow is implemented by **extending** `agent_runtime::orchestration::Supervisor` and `scheduler::GraphService` and by **wiring them into the production paths that do not use them today** — not by adding a coordinator, a second scheduler loop, or a second memory database. Concretely:

1. **One orchestration authority.** `Supervisor` (state machine `OrchestrationState`, drivers `SupervisorDrivers`) remains the only thing that can reach `Accepted`. `GraphBackedRun` stays the single adapter between it and the graph. `workflow::execute_run` becomes a *client* of `GraphBackedRun` when `orchestration.mode = verified`, and stays as it is when the mode is `off`.
2. **One durable stream.** Every accepted state change is an `EventKind` payload in the project's `EventLedger` (`sessions.sqlite`), reduced into the supervisor snapshot and the graph. In-memory projections and the TUI never advance state on their own. The `GraphService` gains a reducer (`replay(session) -> graphs`), and the graph events carry enough payload to rebuild: the created graph (`GraphCreated` gains `graph`), the accepted proposal (`GraphRevisionCommitted` gains `proposal`), the transition (`GraphNodeStateChanged` already has it).
3. **Records live with their owners** (table in GVS-001 §5): run/finding/candidate/verdict/decision under `agent-runtime::orchestration`; proposals under `scheduler`; bodies over the event-size bound in `event-ledger::artifact_store`; publication receipts under `workspace::transaction`. No new crate.
4. **Publication is a workspace transaction.** A candidate reaches the user's tree only through `TransactionManager::begin_transaction` → verification hooks → `commit_transaction`; the `CommitReceipt` (parent revision + patch hash) is the publication receipt, and the operation journal (ADR 0003) records prepared/applied/reconciled. `agent_views::integrate` is refactored onto this path.
5. **Configuration precedence is the existing one.** `orchestration.mode` (`protocol::OrchestrationConfig`) resolves through the established managed-policy → user config → project config → CLI-flag order the rest of `RapidConfig` uses; a `--orchestration <off|verified>` flag on `rapid run` and `rapid exec` is the CLI override. `orchestration.strategy = sequential|adaptive` is added under the same section, default `sequential`. An active run keeps the configuration digest it started with (`OrchestrationSnapshot.config_digest`); a change applies to the next run or at a recorded safe boundary.

## Rationale

- The audit found the substrate exists (3.4k lines of orchestration types, a validating `GraphService`, a `TransactionManager`, an `EvidenceStore` with invalidation) but is unreachable from `apps/rapid` except for the host-only phases `goal_claim` runs. The cheapest correct path is to wire and harden, not to rebuild — the same finding `newtask.md` §0a records for the context engine, sandbox and capability broker.
- A second engine would duplicate the state machine (20 states, validated transitions), the proposal validator and the evidence store, and would create exactly the split-authority problem invariants 1–3 forbid.
- Event-derived acceptance is the only way to satisfy AC-03 (append-failure safe, idempotent, no split projections): the current `accept()` performs three writes (`supervisor.rs:691–692`, `orch.rs:93–101`) and a crash between them leaves disagreement.

## Record ownership and lifecycle

| Record | Authority for its truth | Versioned shape | Where the body lives |
|---|---|---|---|
| Run execution (`OrchestrationSnapshot` + run identity, budget ledger, incumbent) | Event stream (`orchestration.run.*` events); the snapshot is a reduction | `rapidlm.orchestration.run/v1` | ledger payload |
| Task proposal | `GraphRevisionCommitted` payload | `rapidlm.graph.proposal/v1` | ledger payload, artifact if over bound |
| Finding | `orchestration.finding.{proposed,supported,refuted,superseded}` events | `rapidlm.orchestration.finding/v1` | statement in payload; sources/evidence by `EvidenceId`/`ArtifactRef` |
| Candidate | `orchestration.candidate.{created,checked,verified,published,retired}` events | `rapidlm.orchestration.candidate/v1` | patch as artifact (`ArtifactRef`), digests in payload |
| Check receipt | `orchestration.check.recorded` event; also an `EvidenceRecord` (ADR 0012) so `goal claim` citations keep working | `rapidlm.orchestration.check/v1` | bounded output as artifact |
| Verification verdict | `orchestration.verdict.recorded` event | `rapidlm.orchestration.verdict/v1` | ledger payload |
| Budget reservation / settlement | `orchestration.budget.{reserved,settled}` events; settlement idempotent by `(run, attempt, request)` | `rapidlm.orchestration.budget/v1` | ledger payload |
| Decision | `orchestration.decision` event | `rapidlm.orchestration.decision/v1` | concise reason; no raw hidden reasoning |
| Publication receipt | `CommitReceipt` + operation-journal entry (`prepared` → `applied` → `reconciled`) | existing `workspace` types | journal |

Lifecycle states reuse `OrchestrationState`. The plan's "paused / waiting for approval / cancelled / blocked / failed / completed" map onto `Blocked` (waiting on approval or input, with a `BlockReason`), `Cancelled`, `Failed`, `Accepted`; `Paused` is a new variant added in GVS-004 because recovery must restore an active run as paused (invariant 11) and no existing state says that.

### Publication ordering (normative)

1. Freeze: candidate id + patch artifact + baseline/result digests + verification inputs recorded (`candidate.verified`).
2. Prepare: `begin_transaction(parent_view, candidate_patch)` records `parent_revision`; journal `prepared`.
3. Recheck: parent revision equals the frozen baseline **and** policy allows; otherwise `candidate.stale` and stop — the candidate is retained, never dropped.
4. Apply: `commit_transaction` runs the integration checks as verification hooks; journal `applied` with the `CommitReceipt`.
5. Complete: only after `applied` is durable is `TaskAccepted` appended, carrying the receipt. A failed append leaves the tree changed but the run not completed; recovery reconciles by reading the journal (`applied` without `TaskAccepted` → append it; `prepared` without `applied` → roll the transaction back).
6. Idempotency: a repeated acceptance/publication request with the same candidate id and parent revision is answered from the journal; it cannot apply twice.

## Schema and migration strategy

- Additive only. New `EventKind` variants are added with their `v1` payloads; readers that do not know a kind skip it. GVS-004 adds the schema fixtures (`crates/protocol/tests/schema_fixtures`) and an old-reader/new-reader test.
- Historical `GraphCreated`/`GraphRevisionCommitted` events without payloads (everything before GVS-005 lands) reduce to `GraphReplay::Unsupported { first_seq }` — a typed, reported outcome; the graph is not reconstructed by guessing. Since nothing in production creates graphs today, no user data is affected; the outcome exists so the reducer is honest by construction.
- `OrchestrationSnapshot` gains fields with defaults; `resume()` accepts a snapshot missing them.
- SDK wire types (`sdk/`) get the new event kinds through the existing generator (`pnpm generate:check` gates it).

## Consequences

- Phase 1 order becomes: production caller (`rapid run --orchestration verified` over `GraphBackedRun`) → payload-complete events + reducer → single-event acceptance → identities + invalidation → recovery. Each slice is small, testable, and leaves the `off` path byte-for-byte unchanged.
- `agent_views::integrate` changes shape (transaction instead of raw `git apply`); its tests (`a_clean_child_patch_integrates_and_releases_the_view`, …) are the regression suite for that refactor.
- `goal_claim::run_claim` keeps its contract until Phase 3 replaces `HostBlocked` drivers with live ones; its zero-skeptic policy is a documented limitation until then, not a claim of independent review.
- Every new process-spawning test uses `crates/test-fixtures`; Windows is a gate.
- Anything that would need a Python coordinator, a second scheduler loop beside `execute_run`/`GraphBackedRun`, or a separate memory database is out of scope by this decision; a superseding ADR is required to change that.
