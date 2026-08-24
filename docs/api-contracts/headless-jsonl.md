# API Contract — Headless JSONL Protocol

Machine-readable execution stream for CI/scripts with clean stdout.

## Types

### `rapid.schema`
first event with protocol/version/capabilities

### `rapid.event`
durable semantic event projection

### `rapid.delta`
optional transient stream delta

### `rapid.result`
terminal run result/exit class

## Operations

- `rapid exec --jsonl`
- `rapid run --jsonl`
- `rapid inspect --json`
- `rapid export --jsonl`

## Error/recovery semantics

Protocol mode writes no human prose/ANSI to stdout; diagnostics to stderr. Consumers resume using cursor/sequence when transport supports it.

## Versioning/compatibility

Semantic version in schema event; readers must ignore unknown additive fields and handle declared event versions.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
