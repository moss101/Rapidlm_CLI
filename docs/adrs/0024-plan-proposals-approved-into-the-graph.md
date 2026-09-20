# ADR 0024 — Plan proposals: read-only by the lattice, approved through the approval wait, compiled by the playbook path

**Status:** Accepted for `SEAMS-RAPIDLM-01` (Phase 0; governs `SEAM-05`)  
**Date:** 2026-09-20  
**Baseline audit:** [SEAM-00-1](../goals/seams-phase0-baseline-2026-09-20.md) §3 (SEAM-05)  
**Builds on:** ADR 0004 (graph-native orchestration), 0021 (verified orchestration extends the graph and supervisor), 0022 (hook result v2 — the approval wait); invariants 1, 3, 5, 8; `SEAMS §3.2 S1, S2`

## Context (what exists at the baseline)

- **Read-only is already a lattice decision.** `PermissionMode::Plan` is the strictest of the six modes: `PermissionLattice::evaluate` denies every non-`ReadOnly` tool class with `DecisionReason::PlanModeDeny` *before* any allow rule is consulted (`apps/rapid/src/permissions.rs:590`, tested at `:1138` — "an allow rule must never bypass Plan mode's absolute write floor"). `rapid cron poll` already forces fired prompts into this mode (`p9_commands.rs`).
- **A model-driven plan mode exists but self-approves.** `plan_enter`/`plan_exit` tools (`exec_tools.rs:3539–3598`) flip an `ExecTools.plan_mode` flag that additionally gates writes to `.rapidlm/plan.md` (`PLAN_PATH`); `plan_exit` reads that file and reports "Plan accepted" with **no human decision, no schema, no proposal record and no graph**. The flag is in-memory (an S1 side channel) and is not the lattice's mode.
- **The compile path exists.** `workflow::PlaybookFile { steps: [Step { key, kind, label, depends_on, prompt, command, watch, question, … }] }` → `scheduler::playbook::compile` → `RuntimeGraph` with `DependsOn` edges; `rapid run --orchestration verified` drives it through `GraphService`/`GraphBackedRun` with every transition a ledger event (ADR 0021, delivered 2026-09-18). Human steps wait on the graph (`graph.wait`, `NodeState::Waiting`).
- **No proposal schema, no revision record, no `/plan`.** Nothing under `crates/protocol` names a plan; `.rapidlm/plans/` does not exist.

## Decision

1. **Plan mode is the lattice mode, not the tool flag.** `/plan` (TUI) and `rapid exec --plan` run the turn with `PermissionMode::Plan` as the *effective* mode (the managed ceiling still applies — Plan is the floor, so it always fits under any ceiling). The `plan_enter`/`plan_exit` tools keep working for compatibility but `plan_exit` no longer "accepts": it **submits** the proposal (decision 2). The in-memory `plan_mode` flag becomes a projection of the lattice mode rather than a second gate.
2. **The proposal is a versioned artifact.** `protocol::plan::PlanProposal` (`schema = "rapidlm.plan_proposal"`, `version = 1`): `title`, `summary`, `steps: [{ key, kind ∈ {agent, process, verification, human}, label, depends_on, prompt|command|question, watch }]`, `files_expected_to_change`, `verification`, `risks`, `open_questions`, `base_revision` (the workspace digest from `apps/rapid/src/digests.rs`). It is stored through `event_ledger::artifact_store` and rendered as `.rapidlm/plans/<id>.md` (a human-readable projection; the artifact is the record). A schema fixture lives under `crates/protocol/tests/fixtures/plan/v1/`. The `steps` shape is deliberately the `workflow::Step` shape so approval needs no translation layer.
3. **Approval is the existing approval wait.** Submitting a proposal appends `plan.proposed` (artifact ref, revision, digest) and raises an approval request through the same `ApprovalSink`/`approval.requested`/`TurnOutcome::Waiting` path ADR 0022 §3 uses, with `source: "plan:<id>"`. Resolution: *approve* appends `plan.approved` and compiles; *reject* appends `plan.rejected { reason }`; *edit* is decision 4. Headless `--plan` prints the proposal, records the wait and exits `NeedsApproval` (10). A restart mid-wait recovers the pending approval from the ledger, as today.
4. **Revisions are immutable and superseding.** Editing before approval writes a new artifact and appends `plan.revised { supersedes }`; the previous artifact and `.md` are untouched (invariant 3). Only the newest revision can be approved; approving an older one is a typed refusal.
5. **Approval compiles through the playbook path.** `plan.approved` writes the proposal's steps as a `PlaybookFile` under `.rapidlm/runs/` and starts a `rapid run --orchestration verified` run over it (the existing `VerifiedRun`), so the plan's steps *are* graph nodes with `DependsOn` edges, driven and journaled by the machinery ADR 0021 delivered. The `plan.approved` payload carries `{ plan_id, revision, run_id, graph_id }` — the ledger link from plan revision to nodes (AC-03). No second compiler.
6. **Stickiness and refusal.** Plan mode holds across turns until a proposal is approved or cancelled (`/plan cancel`); a write-capable call while it holds is the lattice's `PlanModeDeny`, whose reason text the model can read (S9 truthful; already `"plan mode is read-only; this call mutates state"`).

## What is deliberately not done

- No new node kind: the graph already has `Plan`, `Approval`, `Task`, `Agent` kinds and `RequiresApproval`/`DependsOn` edges; proposals map onto them through the playbook compiler.
- The proposal does not carry hidden model reasoning; `summary`/`risks`/`open_questions` are the model's stated plan.
- The cron path's forced Plan mode is untouched (S11).

## Consequences

- `SEAM-05` depends on the `SEAM-01` approval mapping (ADR 0022 §3) and on the workflow runner as delivered; its work is: the protocol type + fixture, the `plan.*` events (catalog + SDK wire list), `/plan` + `--plan` wiring of the effective mode, proposal submission through `plan_exit`, the approval resolution branch, the compile-and-run step, and `.rapidlm/plans/` rendering.
- `plan_exit`'s user-visible text changes from "Plan accepted" to "Plan submitted for approval" — a truthful correction, disclosed in the delivery record.
