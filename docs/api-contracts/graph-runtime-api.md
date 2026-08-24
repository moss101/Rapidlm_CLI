# API Contract — Runtime Graph API

Kernel-internal/public-inspector contract for graph creation, proposals, revisions, scheduling and explanation.

## Types

### `RuntimeGraph`
graph_id, run_id, revision, root, typed nodes/edges

### `GraphProposal`
base_revision + add/supersede/invalidate operations; host validates

### `NodeSnapshot`
state, attempt, dependencies, budgets, resources, evidence obligations

## Operations

- `graph.create(spec)`
- `graph.propose(graph_id, proposal)`
- `graph.snapshot(graph_id, revision?)`
- `graph.diff(graph_id, from, to)`
- `graph.cancel(node|run)`
- `graph.retry(node)`
- `graph.explain_ready(node)`
- `graph.explain_blocked(node)`
- `graph.subscribe(run, cursor)`

## Error/recovery semantics

Stale base revision returns Conflict with current revision; cycle/schema/policy/resource violations return typed proposal rejection. Retry cannot replay uncertain non-idempotent effects without reconciliation.

## Versioning/compatibility

`rapidlm.graph/v3`; additive Node/Edge variants require unknown-variant-safe readers or negotiated minor version.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
