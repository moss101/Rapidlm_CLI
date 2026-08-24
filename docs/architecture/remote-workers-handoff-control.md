# Architecture — Remote Workers, Execution Handoff and Human Control

## 1. Responsibility
Move durable work across local/remote executors and transfer mutable input ownership without split-brain state.

## 2. Non-negotiable design rules
- At most one valid write-capable session execution generation.
- Handoff bundle excludes capability leases/plaintext secrets.
- Human and agent cannot concurrently own the same mutable control domain.

## 3. Components
- **HandoffCoordinator** — quiesce/bundle/restore/commit
- **SessionExecutionLease** — generation writer ownership
- **RemoteWorkerService** — worker selection/health
- **ControlLeaseService** — workspace/terminal/desktop/mobile input ownership
- **Reconciler** — post-takeover/handoff state

## 4. Canonical contracts
`HandoffBundle`, `SessionExecutionLease {generation,owner,expiry}`, `ControlLease {domain,holder,generation}`, `WorkerCapabilityProfile`.

## 5. Failure and recovery
Partition/failure before COMMIT leaves source authoritative or session parked; target cannot write until generation commit. Human takeover resume triggers mutation scan/reobservation.

## 6. Security and trust
mTLS worker identity, signed WorkLease, digest inputs/results, reissued target credentials. No trust inheritance from worker-produced content.

## 7. Implementation notes
Handoff sequence: source quiesce → durable bundle → target restore/verify → generation transfer → fresh leases/observations → resume.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
