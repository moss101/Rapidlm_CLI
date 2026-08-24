# Competitive and Reference Architecture Analysis — V2

**Research date:** 2026-08-14. Factual claims are drawn from official public sources and user-supplied Kimi/Grok repository material. RapidLM conclusions are architecture inferences; they are not claims that source products use RapidLM's internal design.

## 1. Kimi Code

The supplied Kimi repository material shows a TypeScript monorepo with separate CLI/TUI, agent-core generations, SDK, provider abstraction, execution abstraction, OAuth, telemetry, transcript contract, server/client, and persistent search-store packages. The supplied goal-mode design provides especially useful lifecycle semantics: `active`, `paused`, `blocked`, structured completion, turn/token/time budgets, crash recovery that parks an active goal, and main-agent-only goal control.

**RapidLM adopts:** strong package boundaries, terminal-native sessions, subagents, ACP/MCP/hooks/skills, structured goal lifecycle.  
**RapidLM extends:** evidence-gated Goal DAG, stable gateway tools, capability leases, transactional workspace ownership and multi-runtime event sourcing.

Source: user-supplied Kimi repository documents and https://github.com/MoonshotAI/kimi-code

## 2. Grok Build

The supplied Grok Build material describes a Rust implementation with separate TUI, shell/agent runtime, tool and workspace crates, plus interactive/headless/ACP operation.

**RapidLM adopts:** Rust-first low-latency runtime, explicit workspace/tool separation, terminal-first UX, headless/ACP parity.  
**RapidLM extends:** event ledger as the sole durable source, capability leases, evidence graph, context budgets and portable remote worker contracts.

Source: user-supplied Grok Build README and https://github.com/xai-org/grok-build

## 3. Qwen Code

Qwen Code's public materials expose multi-provider operation, subagents/agent teams, daemon/SDK surfaces and isolated Git worktree execution patterns in Agent Arena.

**RapidLM adopts:** provider neutrality and isolation for parallel writes.  
**RapidLM extends:** dependency-aware scheduling, explicit TaskEnvelope/AgentResult contracts, predicted write-set conflict handling, per-agent budgets and evidence-aware merging.

Sources:
- https://github.com/QwenLM/qwen-code
- https://github.com/QwenLM/qwen-code/blob/main/docs/users/features/arena.md

## 4. Augment Code

Augment's Context Engine demonstrates that repository context is valuable as an independent product/service. Its public material emphasizes semantic code understanding and cross-repository context.

**RapidLM adopts:** context as a dedicated subsystem and multi-repo semantic retrieval.  
**RapidLM extends:** local-first FTS + syntax + LSP + code graph + optional vectors, exact read tracking, MMR, token budgeting, provenance/freshness, and direct eval of useful context per token.

Sources:
- https://docs.augmentcode.com/context-services/overview
- https://docs.augmentcode.com/cli/reference

## 5. Cursor CLI

Cursor demonstrates editor/CLI continuity, repository indexing, multi-root workspaces, rule files, diff-oriented review and worktree-based isolation patterns.

**RapidLM adopts:** familiar IDE/CLI continuity and worktree ergonomics.  
**RapidLM extends:** the TUI/ACP/SDK/daemon all consume the same kernel event model; permissions normalize to capabilities rather than command text.

Sources:
- https://cursor.com/docs/cli/using
- https://cursor.com/docs/agent/tools/search
- https://cursor.com/docs/configuration/worktrees

## 6. Claude Code / Agent SDK

Claude Code validates hooks, rules, skills, configurable subagents, MCP and a reusable agent SDK as core developer-platform primitives.

**RapidLM adopts:** composable agent roles, skills, hooks and MCP interoperability.  
**RapidLM extends:** capabilities are host-enforced; plugins default to WASM; skills cannot grant permissions; every runtime surface shares the same event/capability model.

Sources:
- https://code.claude.com/docs/en/agent-sdk/overview
- https://docs.anthropic.com/en/docs/claude-code/hooks

## 7. OpenAI Codex

OpenAI public documentation separates sandbox enforcement from approval policy, which is a critical security distinction. Codex architecture descriptions also reinforce the value of a stable agent loop and prompt/tool prefix for cache efficiency.

**RapidLM adopts:** sandbox vs approval separation and small stable model-visible tool schemas.  
**RapidLM extends:** executor-validated short-lived CapabilityLeases, multiple isolation tiers, evidence-gated completion and provider-neutral routing.

Sources:
- https://developers.openai.com/codex/agent-approvals-security
- https://developers.openai.com/codex/sandboxing

## 8. Atomic Semantic Change Graph

Atomic frames agent changes as intent/provenance rather than only file diffs, with causal relationships and attestations.

**RapidLM adopts:** `goal → evidence → decision → patch → verification → attestation` as a provenance layer.  
**RapidLM choice:** Git remains the compatibility VCS; the provenance graph sits above it.

Source: https://github.com/atomicdotdev/atomic

## 9. Muse Code / Muse Spark 1.2 — V2 primary reference

Meta's 2026 Muse Code release contributes four architecture ideas that materially change RapidLM V2.

### 9.1 Persistent async background agents

Muse Code describes specialized background agents that remain alive throughout the session instead of being spawned for individual tasks. Their persistence reduces repeated information gathering and latency.

**RapidLM adoption:** `AgentPool` maintains bounded session-long read-only background roles such as Explorer, Context Curator and Test Watcher. They communicate through typed mailboxes and evidence references, not an unbounded shared context window. Write work is still isolated into explicit managed workers.

### 9.2 Replay-exact local event log

Muse Code describes every model call, tool run, approval and edit being appended to a local event log so a crash can resume precisely.

**RapidLM adoption:** V1's Event Ledger becomes explicitly replay-exact at the observable harness boundary. Side effects carry correlation/idempotency metadata and snapshots reference exact event sequence and artifact hashes.

### 9.3 Harness/model co-design

Meta states Muse Spark 1.2 was co-trained with Muse Code using rejection-sampled harness trajectories and recipe optimization around goals, compaction, subagents and tools.

**RapidLM adoption:** the V2 harness becomes a trajectory/data factory. It collects observable trajectories, ranks candidates, creates reproducible eval/training bundles, and can optimize prompts, context ranking, routing, compaction and smaller specialist models without changing the production kernel contract.

### 9.4 Long-horizon endurance and self-generated environments

Meta reports whole-repository/long-project training and a kernel optimization case exceeding 1,000 tool calls and up to 24 hours, plus self-generated difficult coding environments graded to create data.

**RapidLM adoption:** release engineering adds long-horizon stress suites, restart/compaction injection, tool-call endurance and task-environment generation. A 15-minute benchmark is not enough evidence for an autonomous runtime intended to work for hours.

Official source: https://research.meta.ai/blog/introducing-muse-code-and-muse-spark-1-2

## 10. Devin — V2 primary reference

Cognition's public Devin materials contribute product/runtime patterns around autonomous engineering operations and full-computer validation.

### 10.1 Managed Devins and clean-slate workers

Cognition documents a coordinating Devin that decomposes work and delegates child Devins, each in an isolated VM with its own shell/browser/development environment. The parent monitors them, can inspect trajectories, tracks per-child compute and can sleep/terminate workers.

**RapidLM adoption:** coordinator + managed worker fleet, clean TaskEnvelope contexts, independent workspace/sandbox ownership, per-agent budgets, trajectory summaries, and parent-visible status without cloning the entire parent transcript.

Source: https://cognition.ai/blog/devin-can-now-manage-devins

### 10.2 Local-to-cloud handoff

Devin CLI `/handoff` packages conversation context, the current branch and uncommitted changes into an independent cloud session that keeps working after the local machine exits.

**RapidLM adoption:** provider-neutral `HandoffBundle` between local, SSH, Kubernetes, hosted microVM, CI, GPU and macOS workers. V2 adds an exclusive `SessionExecutionLease` so source and target cannot both write after handoff.

Source: https://docs.devin.ai/cli/handoff

### 10.3 Full Computer Use

Devin documents full Linux/Windows desktop interaction—not browser-only automation—including mouse, keyboard, screenshots, GUI apps and recording. Its Android workflow combines `adb` for deterministic operations with desktop Computer Use for visual interaction and video proof.

**RapidLM adoption:** browser + desktop + TUI + emulator `SurfaceRef`, accessibility-first targeting, screenshot/vision fallback, action verification, annotated recordings and a human takeover path for MFA/CAPTCHA/ambiguous security prompts.

Sources:
- https://docs.devin.ai/work-with-devin/computer-use
- https://docs.devin.ai/work-with-devin/testing-and-recordings
- https://docs.devin.ai/onboard-devin/environment/android-emulation

### 10.4 Human takeover and shared engineering console

Devin's session tools let the user pause the agent, take over terminal/IDE/browser, make changes and then resume.

**RapidLM adoption:** `HumanControlLease` and `AgentControlLease` provide exclusive write ownership of workspace/desktop/terminal surfaces. Resumption performs external-mutation detection before the agent continues.

Source: https://docs.devin.ai/work-with-devin/devin-session-tools

### 10.5 Knowledge and Session Insights

Devin Knowledge uses trigger descriptions to retrieve small relevant knowledge items. Session Insights analyzes completed sessions and produces actionable recommendations/improved prompts.

**RapidLM adoption:** Knowledge becomes a separate governed registry, not a synonym for memory. Session Insights derives from the Event Ledger and can propose—but never silently apply—new Knowledge, Playbook, prompt or router changes.

Sources:
- https://docs.devin.ai/product-guides/knowledge
- https://docs.devin.ai/product-guides/session-insights

## 11. Comparative architecture matrix

| Capability | Kimi | Grok | Qwen | Augment | Cursor | Claude | Muse Code | Devin | RapidLM V2 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Terminal-native agent | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ CLI | ✓ |
| Event-sourced/replay-exact runtime | partial/source-specific | session | session | session | session | session | **explicit local event log** | durable cloud sessions | **canonical Event Ledger** |
| Persistent background agent pool | — | background tasks | teams | — | agents | subagents | **✓** | long-running/managed patterns | **✓ typed bounded pool** |
| Managed isolated worker fleet | subagents | subagents/worktrees | teams/worktrees | subagents | worktrees | subagents | multi-agent | **✓ isolated VMs** | **✓ workspace+sandbox workers** |
| Parent reads child trajectories | limited | inspectable | inspectable | — | inspectable | inspectable | async feedback | **✓** | **✓ summary/evidence + on-demand trace** |
| Local↔remote handoff | — | remote/headless patterns | daemon | cloud/CLI | cloud patterns | cloud patterns | — | **✓ `/handoff`** | **✓ generalized handoff protocol** |
| Human takeover/resume | approvals | interactive | interactive | interactive | interactive | interactive | interactive | **✓ IDE/Desktop** | **✓ exclusive ControlLease** |
| Trigger-scoped Knowledge | memory | skills/config | memory/skills | context | rules/memory | memory/rules | background memory patterns | **✓** | **✓ governed Knowledge Registry** |
| Session postmortem/insights | traces | traces | logs | analytics | history | history | trajectories | **✓ Session Insights** | **✓ Session Insights + proposals** |
| Harness/model co-optimization | — | — | — | — | — | — | **✓ published** | internal evals | **training-ready trajectory flywheel** |
| 1k+ tool-call / 24h endurance focus | goal mode | long tasks | long tasks | — | cloud agents | long tasks | **explicit case study** | autonomous cloud work | **release endurance suite** |
| Full desktop Computer Use | ecosystem | tools/plugins | ecosystem | ecosystem | ecosystem | computer use ecosystem | multimodal demos | **✓ Linux/Windows** | **✓ browser/desktop/TUI/mobile** |
| Video E2E proof | input/artifacts | artifacts | ecosystem | ecosystem | browser | ecosystem | video input/demo | **✓** | **✓ evidence node** |
| Capability leases | — | permissions | permissions | permissions | permissions | permissions | approvals | permissions/sandbox | **executor-validated leases** |
| Transactional isolated writes | patterns | worktrees | ✓ | cloud/workspaces | worktrees | worktrees | subagents | independent VMs | **mandatory for sibling writers** |
| Evidence-gated goal completion | structured goals | task state | task state | workflows | task state | task state | `/goal` | verification | **first-class criterion graph** |

`—` means the reviewed material did not establish an equivalent first-class primitive; it is not a claim that the product lacks all related capability.

## 12. V2 strategic position

RapidLM should not compete by having the longest feature list. Its differentiating architecture is the combination of:

1. **replay-exact Event Ledger** across local/daemon/remote execution;
2. **Goal/Evidence DAG** that separates model claims from runtime proof;
3. **persistent background AgentPool + clean managed workers**;
4. **token-budgeted Context Compiler + trigger-scoped Knowledge**;
5. **stable gateway tools + CapabilityLease enforcement**;
6. **transactional WorkspaceViews + provenance/attestations**;
7. **local↔remote Execution Handoff + human takeover**;
8. **full Computer Use with evidence-producing E2E testing**;
9. **Harness Learning Engine** that evaluates and improves the agent system from observable trajectories.

The product north-star is therefore: **verified engineering throughput per token, per wall-clock hour, and per unit of privilege granted**.
