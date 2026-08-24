# API Contract — Process, Monitor, Trigger and Job API

Supervised foreground/background process execution and wake-on-event scheduling.

## Types

### `ProcessSpec`
argv/cwd/env handles/pty/limits/sandbox/output budget

### `MonitorSpec`
process/event/regex/port/file predicate

### `TriggerSpec`
cron/time/file/event source

### `JobLease`
job/generation/owner/expiry/heartbeat/idempotency

## Operations

- `process.start/write/cancel/status/log`
- `monitor.create/cancel`
- `trigger.create/pause/resume/delete`
- `jobs.claim/heartbeat/release`

## Error/recovery semantics

Process tree cleanup is owned. Monitor timeout returns explicit condition result. Scheduler lease expiry permits safe takeover only with idempotency/reconcile checks.

## Versioning/compatibility

JSONL/SDK expose semantic events, not terminal scraping.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
