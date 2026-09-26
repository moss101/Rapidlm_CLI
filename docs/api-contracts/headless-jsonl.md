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

## Job records (`rapid exec --jsonl`)

A run's background jobs are stopped when the run ends, and each end is recorded first. Every `job.*` record the run added to its session is then written ahead of the outcome records. These are the starts and ends of the jobs this run started, plus the reconciliation of a dead host's jobs when the run continued a session. A record another host added to the same session meanwhile is not included. Each record's `type` is its kind (`job.started`, `job.completed`, `job.orphan_reconciled`), its `data` is its payload exactly as the ledger has it (what `/jobs` projects), and its `seq` is in the run's own sequence. A run whose turn is not recorded writes none.

## Error/recovery semantics

Protocol mode writes no human prose/ANSI to stdout; diagnostics to stderr. Consumers resume using cursor/sequence when transport supports it.

## Versioning/compatibility

Semantic version in schema event; readers must ignore unknown additive fields and handle declared event versions.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
