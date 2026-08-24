# API Contract — Workspace and Transaction API

Workspace view allocation, reads/status, patch transaction, merge, rewind/fork.

## Types

### `WorkspaceView`
backend/base/scope/owner/generation

### `WorkspaceTransaction`
base hashes, patch ops, attribution, state

### `ExternalMutation`
detected changed path/hash without first-party attribution

## Operations

- `workspace.create_view(spec)`
- `workspace.read/status/diff`
- `workspace.propose(transaction)`
- `workspace.apply(transaction)`
- `workspace.reject(transaction)`
- `workspace.integrate(child,parent)`
- `workspace.checkpoint`
- `workspace.fork(checkpoint)`

## Error/recovery semantics

Preimage mismatch returns Conflict. Failed verification leaves staged/reviewable transaction. No partial multi-file commit acknowledgment.

## Versioning/compatibility

Transaction format versioned; textual Git diff remains interoperability/export layer.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
