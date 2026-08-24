# API Contract — Sandbox and Resource API

Select isolation, provision/acquire warm environments and manage remote worker leases.

## Types

### `SandboxSpec`
filesystem/network/env/resource/platform/isolation

### `EnvironmentLease`
resource id, generation, image/snapshot digests, identity, expiry

### `WorkLease`
signed remote task scope/input digests/capability ceiling

## Operations

- `sandbox.resolve(spec,policy)`
- `resources.acquire(spec)`
- `resources.release(lease,disposition)`
- `resources.health(id)`
- `worker.submit(worklease)`
- `worker.cancel(worklease)`

## Error/recovery semantics

Required isolation unavailable => fail, not downgrade. Dirty/uncertain environment quarantined/destroyed, not returned warm.

## Versioning/compatibility

Backend-specific fields live in extensions; portable isolation semantics stay stable.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
