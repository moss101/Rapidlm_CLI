# API Contract — Kernel Client API

One application contract for in-process TUI, daemon IPC, SDK and adapters.

## Types

### `RunSpec`
mode,task/goal,project,model/policy pins,budgets

### `RunSnapshot`
graph/session/goal/state/usage

### `EventCursor`
session/run sequence

## Operations

- `run.create/cancel/resume/snapshot`
- `events.subscribe(cursor)`
- `sessions.list/get/fork/rewind`
- `approvals.list/resolve`
- `health`

## Error/recovery semantics

Transport loss does not imply run failure; clients reconnect and read durable state. Authentication failure never falls back to unauthenticated loopback.

## Versioning/compatibility

Kernel API semver; internal Rust traits may evolve pre-1.0 but wire fixtures are versioned.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
