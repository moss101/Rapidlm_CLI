# Architecture — Process Supervisor, Background Jobs, Monitors and Triggers

## 1. Responsibility
Own shell/PTY process trees, durable background tasks, wake-on-event monitors, cron/event triggers and process output artifacts.

## 2. Non-negotiable design rules
- LLMs do not poll for process completion.
- Owned process groups are cancellable without touching unrelated user processes.
- Full output persists outside prompt context.

## 3. Components
- **ProcessSupervisor** — spawn/PTY/process group
- **OutputSpool** — CAS logs + head/tail excerpts
- **MonitorService** — exit/regex/port/file/event predicates
- **TriggerService** — cron/event/file/webhook-like sources
- **JobLeaseService** — generation/heartbeat/idempotency

## 4. Canonical contracts
`ProcessSpec`, `ProcessHandle`, `MonitorSpec`, `TriggerSpec`, `JobLease`, `ProcessOutcome`.

## 5. Failure and recovery
Daemon restart reconciles child liveness and marks impossible-running handles interrupted. Missed one-shot policy is explicit; recurring jobs use deterministic jitter and cursor/idempotency state.

## 6. Security and trust
Commands/network/env classified before spawn; output fenced untrusted; secrets redacted from logs where detectable; process resource caps enforced.

## 7. Implementation notes
Long-running threshold can auto-detach to daemon. Monitor nodes wake graph scheduler with typed events; sleep is a timer condition, not model busy-wait.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
