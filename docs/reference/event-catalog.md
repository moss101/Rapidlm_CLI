# Event Catalog

Durable families and examples:
- session/run: `session.created`, `run.started|paused|resumed|completed|cancelled`;
- graph: `graph.revision.committed`, `node.ready|started|waiting|succeeded|failed|invalidated|superseded`;
- goal: `goal.created|updated|paused|blocked|completed|cleared`, `criterion.changed`;
- context: `context.compiled|compacted|invalidated`, `read.observed`;
- tool: `tool.started|repaired|completed|failed`;
- policy: `approval.requested|resolved`, `lease.issued|revoked`;
- workspace: `transaction.proposed|applied|conflict|rejected`, `external_mutation.detected`;
- process/resource: `process.*`, `monitor.*`, `trigger.*`, `resource.phase.*`;
- computer: `observation.captured`, `computer.action.*`, `control.transferred`;
- evidence: `evidence.recorded|invalidated`, `verification.*`;
- handoff/remote: `handoff.*`, `worker.*`;
- extension: `hook.decided` (a v2 hook decision — `rapidlm.hook.decision/v1`: hook, stage, decision, reason digest, tool, call), `hook.input_rewritten` (a hook replaced a call's arguments — `rapidlm.hook.rewrite/v1`: hook, tool, call, both inputs and their digests), `hook.failed` (a hook in a fail-open stage failed and was ignored — `rapidlm.hook.failure/v1`: hook, stage, detail digest; ADR 0022), `plugin.*`, `mcp.*`, `acp.*`;
- eval/release: `eval.*`, `experiment.*`, `release.*`.

Token streaming deltas need not be durable events unless export/replay policy requires them.
