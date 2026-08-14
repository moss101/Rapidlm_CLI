# RapidLM CLI / TUI Software Development Dossier V2 — Manifest

**Version:** 2.0 architecture baseline  
**Date:** 2026-08-14  
**Package root:** `RapidLM-CLI-Software-Development-Dossier-V2/`

## Coverage summary

- **125 files** total.
- **123 Markdown documents**.
- **33 architecture documents**, including first-class managed agents, execution handoff, human takeover, Knowledge, Playbooks/Automations, trajectory learning, Session Insights, and full Computer Use.
- **15 API-contract documents** and canonical domain/event/SQLite/config models.
- **8 evaluation specifications** and **17 runtime system-prompt documents**.
- **19 ADRs** (V2 adds ADRs 0013–0019).
- **320 atomic implementation tasks** in `prompts.md` / `task-manifest.json`; 110 are new V2 tasks.
- **146 PRD functional requirements**, all mapped in `requirements-traceability.md`.

## Entry points

- `00-README.md` — master index and V2 product/architecture thesis.
- `01-PRD.md` — product requirements.
- `02-SDD.md` — overall software/system design.
- `03-Development-Plan-and-Roadmap.md` — phased delivery plan.
- `04-Task-Breakdown.md` — hierarchical WBS.
- `prompts.md` — canonical atomic agent implementation library.
- `AGENTS.md` / `SKILLS.md` — development governance.
- `V2-CHANGELOG.md` — exact V1→V2 revision.
- `research/feature-inspiration-matrix.md` — source/inspiration mapping.
- `architecture/computer-use.md` — full Computer Use V2 architecture.
- `architecture/eval-and-agent-harness.md` — production/eval harness + learning architecture.
- `requirements-traceability.md` — requirement→design→task→verification mapping.
- `validation-report.md` — generated structural QA results.
- `SHA256SUMS` — integrity hashes for every package file except itself.

## Directory counts

| Area | Files |
|---|---:|
| `<root>` | 18 |
| `adrs` | 19 |
| `api-contracts` | 15 |
| `architecture` | 33 |
| `data-models` | 4 |
| `evaluation-specs` | 8 |
| `operations` | 4 |
| `reference` | 4 |
| `research` | 3 |
| `system-prompts` | 17 |

## Complete file inventory

```text
00-README.md
01-PRD.md
02-SDD.md
03-Development-Plan-and-Roadmap.md
04-Task-Breakdown.md
05-prompts.md
AGENTS.md
Agents.md
MANIFEST.md
SHA256SUMS
SKILLS.md
Skills.md
V2-CHANGELOG.md
adrs/0001-rust-first-runtime.md
adrs/0002-event-sourced-sessions.md
adrs/0003-stable-tool-gateway.md
adrs/0004-hybrid-context-engine.md
adrs/0005-capability-leases.md
adrs/0006-transactional-workspace-views.md
adrs/0007-evidence-backed-goals.md
adrs/0008-tiered-sandbox.md
adrs/0009-wasm-plugin-default.md
adrs/0010-mcp-acp-boundaries.md
adrs/0011-provenance-layer-over-git.md
adrs/0012-local-first-telemetry.md
adrs/0013-persistent-background-agent-pool.md
adrs/0014-managed-clean-context-workers.md
adrs/0015-execution-handoff-protocol.md
adrs/0016-knowledge-separate-from-memory.md
adrs/0017-trajectory-learning-without-hidden-cot.md
adrs/0018-full-computer-use-evidence.md
adrs/0019-human-agent-control-leases.md
api-contracts/agent-scheduler-api.md
api-contracts/capability-policy-api.md
api-contracts/computer-mobile-api.md
api-contracts/context-engine-api.md
api-contracts/execution-handoff-api.md
api-contracts/headless-jsonl.md
api-contracts/kernel-api.md
api-contracts/knowledge-playbook-api.md
api-contracts/llm-provider-api.md
api-contracts/managed-agent-api.md
api-contracts/mcp-acp-sdk.md
api-contracts/sandbox-process-api.md
api-contracts/tool-gateway-api.md
api-contracts/trajectory-insights-api.md
api-contracts/workspace-api.md
architecture/agent-pool-and-managed-agents.md
architecture/agents-panel.md
architecture/artifact-store.md
architecture/authentication-and-identity.md
architecture/capability-broker-and-policy.md
architecture/cli-tui.md
architecture/computer-use.md
architecture/context-engine.md
architecture/eval-and-agent-harness.md
architecture/event-ledger-and-sessions.md
architecture/execution-handoff.md
architecture/goal-dag-and-evidence.md
architecture/human-agent-control-handoff.md
architecture/ide.md
architecture/kernel.md
architecture/knowledge-registry.md
architecture/llm-router.md
architecture/mcp-acp-and-sdk.md
architecture/mobile-simulator.md
architecture/playbooks-and-automations.md
architecture/plugins-skills-and-hooks.md
architecture/process-supervisor-and-jobs.md
architecture/project-and-config.md
architecture/prompt-runtime.md
architecture/remote-workers-and-daemon.md
architecture/sandbox.md
architecture/security-scanners.md
architecture/session-insights.md
architecture/telemetry-and-observability.md
architecture/trajectory-learning-and-optimization.md
architecture/updates-and-release.md
architecture/vcs-provenance-and-attestations.md
architecture/workspace-vcs-and-semantic-patch.md
data-models/config-schema.md
data-models/domain-model.md
data-models/event-schema.md
data-models/sqlite-schema.sql.md
evaluation-specs/agent-evals.md
evaluation-specs/computer-mobile-evals.md
evaluation-specs/computer-use-v2-evals.md
evaluation-specs/context-evals.md
evaluation-specs/managed-agent-handoff-evals.md
evaluation-specs/router-prompt-evals.md
evaluation-specs/security-evals.md
evaluation-specs/trajectory-long-horizon-evals.md
operations/incident-recovery.md
operations/observability.md
operations/release-and-supply-chain.md
operations/testing-strategy.md
prompts.md
reference/cli-command-reference.md
reference/definition-of-done.md
reference/error-codes.md
reference/glossary.md
requirements-traceability.md
research/competitive-analysis.md
research/feature-inspiration-matrix.md
research/technology-decisions.md
system-prompts/00-core-system.md
system-prompts/background-explorer.md
system-prompts/coder.md
system-prompts/compaction.md
system-prompts/computer-use.md
system-prompts/context-curator.md
system-prompts/explorer.md
system-prompts/goal-driver.md
system-prompts/managed-coordinator.md
system-prompts/mobile-use.md
system-prompts/planner.md
system-prompts/prompt-policy.md
system-prompts/reviewer.md
system-prompts/security-reviewer.md
system-prompts/session-insights.md
system-prompts/subagent-supervisor.md
system-prompts/verifier.md
task-manifest.json
threat-model-and-security.md
validation-report.md
```
