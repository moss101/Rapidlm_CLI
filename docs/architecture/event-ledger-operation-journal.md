# Architecture — Event Ledger, Operation Journal and Replay

## 1. Responsibility
Provide crash-safe durable user/security facts, projections, graph revision history and exactly-once-oriented effect recovery.

## 2. Non-negotiable design rules
- Event append precedes acknowledged durable state.
- Event Ledger and effect journal have distinct responsibilities.
- Uncertain non-idempotent effects are reconciled, not replayed.

## 3. Components
- **EventStore** — append-only sequence + schema
- **ProjectionStore** — rebuildable read models/checkpoints
- **OperationJournal** — prepare/executing/terminal/reconcile
- **ReplayEngine** — rebuild state deterministically
- **EgressReceiptChain** — attempt-level off-machine audit

## 4. Canonical contracts
`EventEnvelope {schema,event_id,session_id,seq,recorded_at,actor,trace_id,kind,payload,redaction}`; `OperationRecord {effect_fingerprint,state,idempotency,reconcile_ref}`.

## 5. Failure and recovery
On crash replay after checkpoint, inspect live tombstones, reconcile process/resource state and mark active user goals paused. Corruption fails safely with diagnostic bundle and recovery commands.

## 6. Security and trust
Secret-bearing payloads are redacted/handle-based. Hash-linked egress receipts cover attempts even when delivery fails.

## 7. Implementation notes
Use SQLite WAL/transactions for local baseline; large bodies in CAS. Event schemas use additive evolution unless an ADR approves breakage.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
