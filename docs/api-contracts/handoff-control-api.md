# API Contract — Execution Handoff and Control Lease API

Anti-split-brain session execution migration and human/agent mutable-surface ownership.

## Types

### `HandoffBundle`
portable graph/session/workspace/artifact refs, no leases/secrets

### `SessionExecutionLease`
session generation/owner/expiry

### `ControlLease`
domain/holder/generation

## Operations

- `handoff.prepare/restore/commit/abort`
- `execution_lease.status`
- `control.take/release/status`

## Error/recovery semantics

Failed commit proves either source remains owner or session parks; never two valid writers. Resume after human control reconciles mutations/surface generation.

## Versioning/compatibility

Bundle schema versioned and portable; target reissues local credentials/capability leases.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
