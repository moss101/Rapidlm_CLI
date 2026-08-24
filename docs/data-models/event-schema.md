# Event Schema

`EventEnvelope { schema:u16, event_id, session_id, seq:u64, recorded_at, actor, trace_id, kind, payload, redaction }`.

Families: session/run, graph revision/node/attempt, goal/criterion, context/read/compaction, tool/repair, policy/approval/lease, workspace/transaction, process/monitor/trigger/job, sandbox/resource, agent/model, computer/browser/mobile/control, evidence/verification, artifact/provenance, handoff/remote, hook/plugin/MCP/ACP, eval/trajectory/preference, security/release.

Events are facts, not arbitrary logs. High-volume streaming token deltas may remain trace/transient and need not enter durable ledger unless required by replay/export policy. Every durable event kind has schema fixture and redaction classification.
