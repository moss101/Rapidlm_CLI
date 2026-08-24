# Long-Horizon and Chaos Evals

Release lanes: 1h, 4h, 12h, 24h; at least one >1,000 tool calls. Inject model/provider throttles, network partitions, process crashes, daemon restart, SQLite busy/interruption, sandbox/resource restart, remote worker loss, handoff interruption, compaction cycles, context invalidation, approval waits and preview failures. Assert bounded memory/storage growth, no duplicate effects and eventual recoverable state.
## Common gate
Run against production contracts. Prefer deterministic assertions before model judges. Record exact fixture/version/model/prompt/context/tool schemas. A security/data-loss/duplicate-effect/false-completion regression is a hard failure regardless of aggregate score.
