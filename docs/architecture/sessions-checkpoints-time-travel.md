# Architecture — Sessions, Checkpoints, Rewind, Fork and Resume

## 1. Responsibility
Provide durable user conversation/run identity with non-destructive graph/workspace time travel and deterministic resumption.

## 2. Non-negotiable design rules
- Session identity survives execution location changes.
- Rewind never rewrites Git history automatically.
- Resume reconciles external/process effects before scheduling.

## 3. Components
- **SessionService** — messages/runs/current goal
- **CheckpointService** — graph/context/workspace refs
- **RewindService** — conversation/graph/workspace selection
- **ForkService** — new session/run lineage
- **ResumeService** — replay/reconcile

## 4. Canonical contracts
`SessionCheckpoint {seq,graph_revision,workspace_ref,context_summary_ref,process_snapshot_refs}`, `RewindSpec`, `ForkSpec`.

## 5. Failure and recovery
If a selected workspace checkpoint cannot be restored atomically, no partial restore is acknowledged. Active goals park paused on ordinary process recovery.

## 6. Security and trust
Checkpoint artifacts obey data policy; secret/capability leases are not serialized. Fork inherits only explicitly portable state.

## 7. Implementation notes
TUI timeline shows event/graph/workspace checkpoints and supports restore conversation, graph, workspace or fork with clear consequences.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
