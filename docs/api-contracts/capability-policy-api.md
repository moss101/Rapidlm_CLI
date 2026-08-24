# API Contract — Capability, Policy and Approval API

Normalize privileged actions, evaluate layered policy and issue/verify scoped leases.

## Types

### `Capability`
typed fs/proc/net/git/secret/browser/desktop/mobile/mcp/plugin/external actions

### `PolicyDecision`
allow/ask/deny + normalized reason

### `CapabilityLease`
subject/action hash/scope/expiry/uses/signature

### `ApprovalRequest`
normalized action, risk, options, expiry

## Operations

- `policy.evaluate(request, generations)`
- `broker.authorize(request)`
- `approval.resolve(id, decision)`
- `lease.verify(lease, action)`
- `policy.explain(action)`

## Error/recovery semantics

`dont-ask` maps Ask→Deny. Expired/stale-generation lease is invalid. Higher-level deny cannot be overridden.

## Versioning/compatibility

Capability names are stable/versioned; policy readers reject unknown privilege-broadening semantics.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
