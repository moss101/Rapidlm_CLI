# Configuration Schema and Precedence

Layers: compiled defaults/safety → organization managed policy → user global → trusted project → session/CLI overrides where permitted. Lower layers cannot broaden higher security ceilings.

Configuration sections: runtime/daemon, providers/models/router, context/indexing, graph/scheduler, agents/prompts/skills, tools/repair, workspace/VCS, policy/approvals, sandbox/resources, process/jobs/triggers, computer/browser/mobile/preview, MCP/ACP/plugins/hooks, memory/knowledge/preferences, telemetry/eval, update/release.

Every config load produces `ConfigGeneration` and source metadata; changes invalidate dependent caches/projections. Executable project extensions remain disabled until trust.
