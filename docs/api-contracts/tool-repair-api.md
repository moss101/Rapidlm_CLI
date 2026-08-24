# API Contract — Tool Contract Repair API

Localized validator-driven repair and cross-tool invariant evaluation.

## Types

### `ValidationIssue`
JSON pointer/path, expected/actual, code

### `RepairRecord`
rules, issue paths, hashes, confidence

### `InvariantDecision`
Allow/Recover/Reject with reason/ref

## Operations

- `repair.attempt(schema, original, issues, model_profile)`
- `invariants.evaluate(invocation, execution_state)`
- `repair.telemetry(model,tool,schema)`

## Error/recovery semantics

Repair only after original validation fails; ambiguous/destructive/security-semantic fields reject. Repaired call is always revalidated.

## Versioning/compatibility

Repair rules are versioned by tool schema and harness version; experiment rollout supports per-model gating.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
