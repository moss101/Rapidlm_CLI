# API Contract — Goal, Evidence and Verification API

Goal lifecycle, criterion/claim/evidence and completion-gate contracts.

## Types

### `GoalContract`
statement, criteria, boundaries, proof policy, budgets, stop rules

### `Evidence`
kind, provenance, revision, freshness, artifact/source refs

### `VerificationRecord`
criterion/claim, evidence set, oracle, PASS/REJECT/INCONCLUSIVE

### `CompletionCandidate`
claimed criteria, evidence refs, residual risk

## Operations

- `goal.create/show/pause/resume/cancel/budget`
- `evidence.record/invalidate/explain`
- `verify.run(goal|criterion|claim)`
- `goal.submit_completion(candidate)`
- `goal.completion_status(goal)`

## Error/recovery semantics

Missing/stale proof yields reject/inconclusive, never implicit pass. Active goal after process recovery parks paused.

## Versioning/compatibility

Goal/event schema migrations preserve historical evidence/verdicts; current satisfaction is recomputed from current revisions.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
