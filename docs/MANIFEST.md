# RapidLM CLI / TUI V3 Dossier Manifest

**Version:** 3.0
**Date:** 2026-08-24
**Validation:** PASS
**Atomic tasks:** 412
**PRD requirements:** 97

## Directory counts

| Area | Files |
|---|---:|
| `<root>` | 22 |
| `agents` | 9 |
| `docs` | 106 |
| `skills` | 8 |
| `system-prompts` | 17 |

## Complete inventory

```text
00-README.md
01-PRD.md
02-SDD.md
03-Development-Plan-and-Roadmap.md
04-Task-Breakdown.md
05-prompts.md
AGENTS.md
PRD.md
README.md
SDD.md
SKILLS.md
V3-CHANGELOG.md
agents.md
agents/coder.md
agents/computer-operator.md
agents/context-scout.md
agents/main.md
agents/planner.md
agents/reviewer.md
agents/security-reviewer.md
agents/tester.md
agents/verifier.md
development-ledger.md
development-plan.md
docs/adrs/0001-rust-first-runtime.md
docs/adrs/0002-event-sourced-durable-state.md
docs/adrs/0003-operation-journal.md
docs/adrs/0004-graph-native-orchestration.md
docs/adrs/0005-specialized-graph-fabrics.md
docs/adrs/0006-context-compiler.md
docs/adrs/0007-context-scout.md
docs/adrs/0008-repairable-tool-contracts.md
docs/adrs/0009-cross-tool-invariants.md
docs/adrs/0010-capability-leases.md
docs/adrs/0011-transactional-workspaces.md
docs/adrs/0012-evidence-backed-goals.md
docs/adrs/0013-independent-verifier.md
docs/adrs/0014-resource-pool.md
docs/adrs/0015-wake-on-event.md
docs/adrs/0016-computer-observation-generation.md
docs/adrs/0017-wasm-plugin-default.md
docs/adrs/0018-protocol-adapters-not-authorities.md
docs/adrs/0019-preference-as-soft-evidence.md
docs/adrs/0020-migration-over-rewrite.md
docs/api-contracts/agent-executor-api.md
docs/api-contracts/artifact-api.md
docs/api-contracts/capability-policy-api.md
docs/api-contracts/computer-use-api.md
docs/api-contracts/context-api.md
docs/api-contracts/goal-evidence-api.md
docs/api-contracts/graph-runtime-api.md
docs/api-contracts/handoff-control-api.md
docs/api-contracts/headless-jsonl.md
docs/api-contracts/hooks-plugins-api.md
docs/api-contracts/kernel-api.md
docs/api-contracts/mcp-acp-sdk.md
docs/api-contracts/process-trigger-api.md
docs/api-contracts/sandbox-resource-api.md
docs/api-contracts/tool-gateway-api.md
docs/api-contracts/tool-repair-api.md
docs/api-contracts/workspace-api.md
docs/architecture/agent-harness.md
docs/architecture/artifacts-provenance-attestations.md
docs/architecture/auth-credentials-identity.md
docs/architecture/build-release-supply-chain.md
docs/architecture/capability-broker-policy-projection.md
docs/architecture/cli-tui.md
docs/architecture/computer-use.md
docs/architecture/context-engineering.md
docs/architecture/eval-harness.md
docs/architecture/event-ledger-operation-journal.md
docs/architecture/goals-evidence-verification.md
docs/architecture/graph-engineering.md
docs/architecture/kernel-runtime.md
docs/architecture/llm-router.md
docs/architecture/mcp-acp-sdk-external-agents.md
docs/architecture/memory-knowledge-preference.md
docs/architecture/plugins-skills-hooks.md
docs/architecture/preference-learning-trajectory.md
docs/architecture/preview-visual-verification.md
docs/architecture/process-background-trigger-runtime.md
docs/architecture/project-config-trust.md
docs/architecture/prompt-runtime.md
docs/architecture/remote-workers-handoff-control.md
docs/architecture/sandbox-resource-pool.md
docs/architecture/security-scanners.md
docs/architecture/sessions-checkpoints-time-travel.md
docs/architecture/telemetry-session-insights.md
docs/architecture/tool-contract-and-recovery.md
docs/architecture/workspace-vcs-semantic-patch.md
docs/data-models/config-schema.md
docs/data-models/domain-model.md
docs/data-models/event-schema.md
docs/data-models/sqlite-schema.sql.md
docs/evaluation-specs/agent-harness-evals.md
docs/evaluation-specs/computer-use-evals.md
docs/evaluation-specs/context-evals.md
docs/evaluation-specs/goal-verification-evals.md
docs/evaluation-specs/graph-runtime-evals.md
docs/evaluation-specs/long-horizon-chaos-evals.md
docs/evaluation-specs/performance-token-evals.md
docs/evaluation-specs/preference-trajectory-evals.md
docs/evaluation-specs/process-resource-evals.md
docs/evaluation-specs/protocol-extension-evals.md
docs/evaluation-specs/security-evals.md
docs/evaluation-specs/tool-contract-evals.md
docs/evaluation-specs/workspace-evals.md
docs/operations/build-and-ci.md
docs/operations/data-retention-privacy.md
docs/operations/incident-recovery.md
docs/operations/observability-slo.md
docs/operations/release-and-supply-chain.md
docs/operations/testing-strategy.md
docs/reference/build-system.md
docs/reference/cli-command-reference.md
docs/reference/computer-use-command-reference.md
docs/reference/definition-of-done.md
docs/reference/error-codes.md
docs/reference/event-catalog.md
docs/reference/glossary.md
docs/reference/graph-node-edge-catalog.md
docs/research/adoption-not-copying.md
docs/research/feature-inspiration-matrix.md
docs/research/migration-v2-to-v3.md
docs/research/source-ledger.md
docs/security/threat-model.md
docs/walkthroughs/end-to-end-code-change.md
docs/walkthroughs/long-running-goal-recovery.md
docs/walkthroughs/open-model-tool-repair.md
docs/walkthroughs/visual-debugging.md
prompts.md
requirements-traceability.md
skills.md
skills/computer-use.md
skills/context-scout.md
skills/goal-verification.md
skills/graph-engineering.md
skills/release.md
skills/repo-archaeology.md
skills/security-review.md
skills/tool-contracts.md
system-prompts/00-core-constitution.md
system-prompts/01-main-agent.md
system-prompts/background-explorer.md
system-prompts/coder.md
system-prompts/compaction.md
system-prompts/computer-use.md
system-prompts/context-curator.md
system-prompts/context-scout.md
system-prompts/debugger.md
system-prompts/independent-verifier.md
system-prompts/planner-architect.md
system-prompts/repair-agent.md
system-prompts/reviewer.md
system-prompts/security-reviewer.md
system-prompts/session-insights.md
system-prompts/system-prompt-composition.md
system-prompts/tester.md
task-manifest.json
tasks.md
threat-model-and-security.md
validation-report.md
MANIFEST.md
SHA256SUMS
```
