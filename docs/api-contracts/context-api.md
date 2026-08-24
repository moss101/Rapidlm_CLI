# API Contract — Context Fabric API

Contracts for InformationNeed, Context Scout, search/read, ContextPacket and invalidation.

## Types

### `InformationNeed`
questions, scope, anchors, completeness flags, token budget

### `ContextPacket`
visibility generation + provenance/freshness/token-bearing ContextItems

### `ContextScoutReport`
summary/scope/references/negative findings/open questions/snippets

### `ReadObservation`
path/revision/hash/range/full-partial/clamp metadata

## Operations

- `context.compile(need, node)`
- `context.scout(need)`
- `context.search(query, scope)`
- `context.read(ref, budget, cursor?)`
- `context.explain(packet)`
- `context.invalidate(changed_refs)`
- `context.compact(agent, target_budget)`

## Error/recovery semantics

Out-of-scope/trust-filtered content is not returned. Partial read returns continuation cursor. Stale packet/read returns StaleReference rather than silently applying old state.

## Versioning/compatibility

`rapidlm.context/v3`; retrieval internals may evolve without changing packet provenance/freshness guarantees.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
