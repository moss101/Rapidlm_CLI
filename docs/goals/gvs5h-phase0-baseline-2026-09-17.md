# GVS-001 — Baseline audit and existing-work map for `GVS-RAPIDLM-01`

**Date:** 2026-09-17  
**Goal:** [`GVS-RAPIDLM-01`](gvs5h-end-to-end-implementation.md), Phase 0  
**Worklist entries covered:** GVS-001 (this document), GVS-002 (ADR 0021 + the contracts section below), GVS-003 (the preregistration file linked in §7)  
**Method:** every claim below was checked against the source tree at the baseline commit by reading the code or running `grep`; nothing is carried over from an older assessment without being rechecked. Where a lead from the research document turned out to be inaccurate, the correction is recorded rather than the lead.

## 1. Baseline

| Fact | Value |
|---|---|
| Planning baseline named by the goal | `bc87a89b696234c48065eb45e2f67cbffbca34fd` (2026-09-15) |
| Baseline for this audit | the commit that adds this file; every `file:line` reference below is to that tree |
| Dirty working state at audit time | `.rapidlm/goal.json` (pre-existing local modification, not part of any commit); pre-existing untracked assessment documents (`docs/codebase-assessment-2026-09-15.md`, `docs/implementation-recheck-2026-09-15.md`) and offline evaluation results under `eval/results/` — all preserved untouched |
| Toolchain | `rust-toolchain.toml` 1.97.1; Node 24.19.0; pnpm 11.20.0 (pinned in `.github/workflows/ci.yml`) |
| Required checks | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked --no-fail-fast`, `cargo test --locked -p protocol --test schema_fixtures`, `pnpm generate:check`, `pnpm typecheck`, `pnpm test` |

### Supported-platform contract (rechecked, not copied)

The planning document says "at the planning baseline, Windows builds/lints are required while its broad test step is informational". That is no longer the contract:

| Platform | Build + lint | Test suite | Notes |
|---|---|---|---|
| macOS (Apple silicon) | gate | gate | Seatbelt sandbox tier available |
| Linux x86_64 | gate | gate | container/gVisor tiers when the runtime is present; host-restricted tier via `ulimit`/`ps` |
| Windows x86_64 | gate | **gate** as of the commit that removes `continue-on-error` from `.github/workflows/ci.yml` (2026-09-17) | PTY (`script(1)`) and the host-restricted sandbox tier report typed unavailability (`PtyError::Unsupported`; `HostRestrictedBackend::health` → `HealthReason::PlatformUnsupported`); daemon/socket Unix-only; process trees stopped through `taskkill` |

Every new contract added by this goal must therefore carry portable tests or a Windows-side contract test of its unsupported path; "informational on Windows" is not an option any more.

## 2. Entry-point call paths (verified)

All user-facing entry points go through one dispatcher, `apps/rapid/src/interactive.rs::run` → `run_subcommand` → the `SUBCOMMANDS` table (`interactive.rs:583`). The paths this goal touches:

| Surface | Path today | What it runs |
|---|---|---|
| `rapid exec <prompt>` (headless turn) | `exec_subcommand` → `exec_turn` (`interactive.rs`) | one agent turn over `ExecTools`; no supervisor, no graph |
| `rapid` (TUI) | `run_interactive` → `SessionLoop` | same turn machinery interactively; background jobs via `JobRegistry` |
| `rapid run <playbook>` | `p9_commands::run_run_command` → `workflow::load_run`/`execute_run` (`apps/rapid/src/workflow.rs:583`) | dependency-ready batches through injected `AgentStepFn`/`CommandStepFn` closures over a loaded step list and a separate `RunState`; **not** the scheduler's `GraphService` |
| `rapid goal …` + `/goal claim` | `goal_claim::run_claim` (`apps/rapid/src/goal_claim.rs:625`) | the only production construction of `agent_runtime::orchestration::Supervisor` (`goal_claim.rs:759–777`, drivers at `:759`, `Supervisor::start` at `:777`): host-owned phases only (`HostPhases` planner/explorer/retriever, `HostBlocked` implementer/strategist, `CachedChecks`), stops before any phase that needs a model |
| `/agents spawn|integrate|abandon` | `agent_views.rs` (`integrate` at `:187`) | child worktrees via `workspace::GitWorktreeStore`; integration is `git apply` of the child's patch (`agent_views.rs:228`) after a per-file "parent unchanged since base" check — **not** `workspace::TransactionManager` |
| daemon / ACP / SDK | `daemon_serve.rs`, `acp_serve.rs`, `sdk/` | expose the same turn path; nothing orchestration-specific |

## 3. Research leads rechecked against source

| Lead (from the 2026-09-15 research) | Verdict | Evidence |
|---|---|---|
| `GraphService::open()` starts with empty graph/history maps and persisted events cannot rebuild a graph | **Confirmed** | `crates/scheduler/src/service.rs:33–48` builds `graphs: BTreeMap::new()`; `GraphCreated` carries `graph_id/revision/root` only (`:72–78`); `GraphRevisionCommitted` carries `graph_id/revision/base_revision` (`:96–102`), not the proposal; `GraphNodeStateChanged` carries ids and a state string. No replay/reducer exists in the crate. |
| Acceptance is split across in-memory state and separate adapter calls | **Confirmed** | `Supervisor::accept` applies the `Accept` transition then emits `TaskAccepted` (`supervisor.rs:691–692`); `GraphBackedRun::accept` (`orch.rs:93–101`) then calls `set_state` twice. Three durable writes, no single acceptance record. |
| `run_claim` uses zero skeptics and disables workspace-identity enforcement; identity is a hash of the goal snapshot | **Confirmed** | `goal_claim.rs:470` (`skeptic_count: 0`) and `:475` (`require_workspace_identity: false`); `workspace_identity(snapshot)` at `:499` hashes the goal snapshot. |
| Evidence invalidation on writes has no production caller | **Confirmed** | `EvidenceStore::invalidate_subject` (`crates/agent-runtime/src/evidence.rs:767`) and the `EvidenceService` wrapper (`:868`) have no callers outside that file. |
| Children start from the parent's HEAD; headless success can integrate without a check | **Confirmed for the check** (`integrate(..., check_command: Option<&str>)`, `agent_views.rs:187–192`); base-commit binding is HEAD at spawn (`child.base_commit`). |
| `workflow::execute_run` is a usable delivery surface | **Confirmed**, with the caveat in §4: it is a separate runner, not the canonical graph. |
| **New finding:** the "canonical Runtime Graph path" the plan integrates into has no production caller | `GraphService` and `GraphBackedRun` are referenced only inside `crates/scheduler` (`grep -rn GraphService apps crates` → `crates/scheduler/src/lib.rs` only). Nothing `apps/rapid` runs goes through them. |
| **New finding:** the V3 manifest overstates graph durability | `docs/task-manifest.json` marks P2-007 "graph revision persistence" and P2-023 "graph checkpoints/resume" `SATISFIED_BY_V3`; the source above cannot reconstruct a graph after restart. AC-02 must be proven by tests, not inherited from the manifest. |
| **New finding:** `TransactionManager` has no production caller either | `crates/workspace/src/transaction.rs` is used by `crates/agent-runtime/src/agent/result.rs` only; `apps/rapid` never begins a transaction. The staged-publication path in the plan (invariant 6, lifecycle step 5) is unwired today. |

## 4. Existing functionality map

"Reachable" means a path from an `apps/rapid` entry point exercises it in production, not that tests exist.

| Capability | Where it lives | Reachable today | What the goal needs |
|---|---|---|---|
| Orchestration state machine and roles | `crates/agent-runtime/src/orchestration/` (3.4k lines): `OrchestrationState` (20 states, `state.rs:12`), `OrchestrationTransition`, `Supervisor` with `SupervisorDrivers { planner, explorer, retriever, implementer, verifiers, strategist, checks }` (`supervisor.rs:117`), `OrchestrationSnapshot` (`:129`, serializable-by-intent, `resume()` exists at `:261`), `VerifierPanel`, `StagnationDetector`, `RoleModelResolver`, `TaskContract`, `GapNode`/`RepairDirective`, `Attestation`/`VerificationVerdict` | Partially: `goal_claim.rs` runs the host-only phases | Live drivers (Phase 3), durable snapshot + event-derived transitions (Phase 1), candidate/finding records (Phase 1/2) |
| Runtime graph | `crates/scheduler`: `GraphService` (create/propose/set_state/diff/cancel/retry/wait/pause/resume/invalidate_from/fan_out/next_fair_ready/export/inspect), `proposal.rs` validation, `GraphBackedRun` adapter | **No** | A production caller and replay-complete events before anything else |
| Durable events | `crates/event-ledger` (`EventLedger::append`, sessions, artifact store), `EventKind::Graph*` variants | Yes (sessions, jobs, turns) | Payload-complete graph/orchestration events; reducer |
| Evidence | `crates/agent-runtime/src/evidence.rs` (`EvidenceStore`, `EvidenceSpec` with hash/locator/command/ledger ref/freshness; `invalidate_subject`) | Yes via `goal_claim` (records check evidence, cites ledger) | Real candidate/test/environment digests as subjects; invalidation wired to writes |
| Isolated child views | `crates/workspace/src/backends/git_worktree.rs` (`GitWorktreeStore`: refs `refs/rapidlm/views/<id>`, detached worktrees under `.git/rapidlm/worktrees`, metadata, lease), `apps/rapid/src/agent_views.rs` | Yes (`/agents`) | Snapshot capture of dirty parent state (Phase 2); candidate identity per view |
| Staged patch / merge preview / transaction | `crates/workspace/src/{merge.rs,transaction.rs}` (`TransactionManager::begin_transaction`/`commit_transaction`, `CommitReceipt` with parent revision + preview/patch hashes, verification hooks) | **No** | Becomes the publication path (Phase 1 GVS-006) |
| Checks through governed processes | `goal_claim::run_check_command` (no shell, quoted tokens, bounded output, timeout) and `sandbox_exec::run_sandboxed` (host-restricted tier, POSIX only) | Yes | Reuse; add check-definition digest and receipts (Phase 3) |
| Context compilation | `crates/context-engine/src/compile.rs`, memory/retrieval | Yes (turns) | Role packets from run records (Phase 2) |
| Model routing | `crates/llm-router/src/phase.rs` (`ModelPurpose`, profiles); `apps/rapid/src/model.rs` | Yes (`Chat` purpose) | Role → purpose/profile mapping (Phase 4) |
| Evaluation | `apps/rapid/src/eval_serve.rs` (protected-file checks, mutation checks, negative controls, typed infra outcomes, repeated trials, provenance, all-attempt accounting), `crates/harness` graders, `eval/suite` (12 + 4 held-out + 40-case smoke) | Yes (`rapid eval`) | Arms and fixtures (Phase 6); the 64-task loader bound (`eval_serve.rs`) needs sharding or a tested change |

## 5. Source ownership for the additive records (GVS-002 input)

Resolved to existing crates and, where one exists, the type to extend rather than duplicate:

| Record (plan §3) | Owner crate | Extend / reuse | New |
|---|---|---|---|
| Run execution | `agent-runtime::orchestration` | `OrchestrationSnapshot` gains `run_id: RuntimeId`, `graph_id: Option<GraphId>`, `goal_id: Option<GoalId>`, `config_digest: ArtifactId`, `baseline: WorkspaceIdentity`, `budget: BudgetLedger`, `incumbent: Option<CandidateId>`, `last_durable_seq: u64` | `BudgetLedger` (reservations + settlements) |
| Task proposal | `scheduler::proposal` | `GraphProposal` already validates base revision and operations; persist its serialized form in `GraphRevisionCommitted` | none |
| Finding | `agent-runtime::orchestration` (new `findings.rs`), bodies in `event-ledger::artifact_store` | `EvidenceNode`/`EvidenceTrust` for sources; `stable_gap_id` pattern for ids | `Finding { id, statement, source_refs, evidence_refs, dependency_hashes, status, supersedes }` |
| Candidate | `agent-runtime::orchestration::evidence` | `CandidateCompletion` gains `patch_artifact: ArtifactRef`, `baseline_digest`, `result_digest`, `predecessor`, `publication: PublicationState` | `CandidateId` (in `protocol::id`) |
| Check receipt | `agent-runtime::orchestration::checks` | `CheckResult` gains `definition_digest`, `environment_digest`, `discovered_tests: Option<u32>`, `output_ref: Option<ArtifactRef>` | none |
| Verification verdict | `agent-runtime::orchestration::verification` | `VerificationVerdict` gains `candidate: CandidateId`, `receipt_refs`, `verifier_profile` | none |
| Budget reservation | `agent-runtime::orchestration::policy` | `OrchestrationBudget` stays the ceiling type | `BudgetReservation { run, attempt, request, reserved, settled: Option<Usage>, unknown_usage: bool }` |
| Decision event | `agent-runtime::orchestration::events` | `OrchestrationEvent` gains a `Decision { action, reason, evidence_refs, budget_state }` kind | none |

Serialized shapes are versioned as `rapidlm.orchestration/v1` records inside `EventKind` payloads and, for bodies over the event-size bound, as artifacts referenced by `ArtifactRef`. Old readers ignore unknown kinds (the ledger already tolerates unknown `EventKind` strings at read time — to be re-verified in GVS-004's compatibility test, not assumed).

## 6. What this changes in the plan (the optimisation)

1. **Phase 1 must start with a production caller, not with persistence.** Persisting replay-complete graph events (GVS-005) for a `GraphService` nothing runs would repeat the pattern `newtask.md` §0a documents (built, tested, unreachable). The first Phase 1 slice is therefore: `rapid run --orchestration verified` (behind `orchestration.mode`) creating a real `GraphService` graph for the playbook's steps and driving them through `GraphBackedRun`, with the existing `workflow::execute_run` closures as the step executors. Only then does replay have something to replay.
2. **Acceptance becomes one event first, projections second.** GVS-006's minimum: a single `TaskAccepted` payload carrying goal node, verify node and candidate id, appended before any in-memory mutation, with the supervisor and the graph both reduced from it. This is a refactor of `Supervisor::accept` + `GraphBackedRun::accept` into `apply(event)`, not a new engine.
3. **Identity before invalidation.** GVS-007's candidate digest comes from `GitWorktreeStore` (tree hash of the view's index + untracked files, computed with `git`), the test-definition digest from the check command's resolved program + args + the files it names, and the environment digest from the toolchain line (`rustc --version`, `node --version`) — all producible today without new infrastructure. `invalidate_subject` then gets its first production caller in `ExecTools`' write path (`workspace_write`/`workspace_patch` and `git apply` in `agent_views::integrate`).
4. **Publication through `TransactionManager`.** `agent_views::integrate` keeps its per-file parent-unchanged check but performs the apply as `begin_transaction` → hooks (the check command) → `commit_transaction`, so the `CommitReceipt` (parent revision, patch hash) is the publication receipt the plan requires — again a wiring change, not new machinery.
5. **Windows is a gate.** Every new test that spawns a process uses `crates/test-fixtures`; POSIX-only behaviour gets a `cfg(not(unix))` contract test.

## 7. Where the other Phase 0 artifacts are

- **GVS-002** — [ADR 0021: verified orchestration extends the existing graph and supervisor](../adrs/0021-verified-orchestration-extends-graph-and-supervisor.md) (decision, record ownership, lifecycle, publication reconciliation, config precedence, schema/migration strategy).
- **GVS-003** — [Experiment preregistration](../evaluation-specs/gvs5h-experiment-preregistration.md) (baseline build, splits, grader version, arms, budgets, statistics, promotion gates, provenance fields).
- The worklist `gvs5h-implementation-tasks.json` records these three as `done` with this document, the ADR and the preregistration as evidence; every other task stays `pending`.
