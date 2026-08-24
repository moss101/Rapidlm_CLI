# API Contract — Tool Gateway API

Stable model-visible gateway tools and internal ToolRegistry execution boundary.

## Types

### `ToolDescriptor`
stable name, schema version, safety class, output budget

### `ToolInvocation`
tool id/schema/input/node/agent/context refs

### `ToolOutcome`
Success/Recovered/Partial/Retryable/Denied/Failed

## Operations

- `tools.project(role,node,mode,policy,model)`
- `tools.validate(invocation)`
- `tools.execute(invocation)`
- `tools.describe(name,version)`

## Error/recovery semantics

Unknown/invalid call is returned as structured model-repairable error; kernel crash is not faked as tool failure. Large output is artifact + excerpt.

## Versioning/compatibility

Model-visible names stay stable within major version; schemas are versioned and compatible aliases explicit.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
