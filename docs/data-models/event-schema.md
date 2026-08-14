# Event Schema and Event Ledger Contract

## Envelope

Every event is immutable and serialized as JSON in headless exports.

```json
{
  "schema": 1,
  "event_id": "019c...",
  "session_id": "019c...",
  "seq": 42,
  "recorded_at": "2026-08-14T15:20:04.123Z",
  "actor": {"kind":"agent","id":"019c..."},
  "trace_id": "8f...",
  "kind": "tool.completed",
  "redaction": "project",
  "payload": {}
}
```

Rules: `seq` increases by exactly one per session append; `(session_id, seq)` and `event_id` are unique; timestamps are observational and never used to order a session; consumers ignore unknown payload fields; incompatible semantic changes require a new envelope or payload schema version.

## Required event families

- `session.created|recovered|forked|closed`
- `turn.started|interrupted|completed|failed`
- `model.requested|stream_delta|completed|failed`
- `tool.requested|authorized|approval_required|started|completed|failed|denied`
- `approval.requested|resolved|expired`
- `goal.created|updated|blocked|paused|resumed|completed|cancelled|budget_updated`
- `agent.spawned|started|state_changed|result|cancelled`
- `workspace.view_created|mutation_detected|patch_staged|transaction_committed|transaction_rolled_back`
- `job.started|output|completed|orphan_reconciled`
- `context.indexed|retrieved|compiled|memory_written`
- `evidence.recorded|validated|rejected`
- `security.finding|scan_completed`
- `artifact.created|redacted|expired`

High-volume stream deltas and process output MAY be stored in chunk artifacts with summary events, but all lifecycle boundaries remain in the ledger.

## Projection checkpoint

A checkpoint records `session_id`, `through_seq`, projection schema version and compressed projection artifact hash. Recovery verifies hash, loads the latest compatible checkpoint, then replays later events.

## Durability

Critical lifecycle/security events use SQLite transaction durability before success is returned. WAL is enabled. Tests MUST simulate process termination between event append and projection update and prove atomicity.

## V2 event families

Additive V2 event kinds:

```text
agent.pool.background_started
agent.pool.background_parked
agent.mail.sent
agent.mail.dropped
agent.task_envelope.created
agent.trajectory_summary.created
handoff.requested
handoff.source_parked
handoff.bundle_ready
handoff.target_restored
handoff.execution_lease_committed
handoff.aborted
control.transferred_to_human
control.transferred_to_agent
knowledge.proposed
knowledge.approved
knowledge.deprecated
playbook.run_started
playbook.step_completed
automation.trigger_received
trajectory.collected
trajectory.graded
insights.generated
computer.surface_created
computer.observed
computer.target_resolved
computer.action_executed
computer.assertion_completed
computer.recording_completed
```

All handoff/control transfer events include a monotonically increasing generation. Computer events reference Observation/action/evidence IDs rather than embedding large screenshots/video.
