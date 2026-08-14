# RapidLM CLI / TUI — Software Development Dossier V2

**Status:** Implementation baseline v2.0  
**Baseline date:** 2026-08-14  
**Audience:** Product, architecture, security, platform, agent-runtime, TUI, context/retrieval, evaluation, Computer Use, mobile, and infrastructure agents  
**Primary implementation:** Rust workspace + TypeScript public SDK  
**Document authority:** Requirements in `01-PRD.md`; architecture in `02-SDD.md`; machine-executable work decomposition in `prompts.md`.

## 0. The problem RapidLM solves

Coding agents can edit files, run commands, browse the web, and increasingly operate whole desktops, but most systems still make teams trade off properties that must coexist in a production engineering agent:

1. **Autonomy vs. control.** Long-running work requires goals, background agents, remote execution and parallelism; users require bounded spend, explicit control ownership, approvals, rollback and deterministic recovery.
2. **Context quality vs. token efficiency.** Agents repeatedly rediscover code, resend unchanged files and inherit bloated parent histories. Large and multi-repository systems make this expensive and error-prone.
3. **Parallelism vs. coherence.** More agents are useful only when their contexts are clean, their workspaces are isolated, their budgets are bounded and their results merge through explicit evidence-bearing contracts.
4. **Speed vs. isolation.** Host execution is fast but risky; strong isolation is safer but costs startup and resources. The runtime must choose an appropriate sandbox tier without silently weakening policy.
5. **Extensibility vs. security.** MCP, plugins, hooks, skills, browser/desktop control and automations expand capability and prompt-injection attack surface simultaneously.
6. **AI operation vs. human ownership.** A production agent must be interruptible and hand control of terminal/browser/desktop sessions to a human without split-brain input or losing session continuity.
7. **Learning vs. privacy/reliability.** Agent trajectories are valuable for improving prompts, routing, retrieval and future models, but observable training data must be governed, redacted and separated from hidden chain-of-thought.

RapidLM is the **local-first control plane for agentic software development**: a terminal-native runtime that treats context, goals, managed agents, permissions, execution, Computer Use, evidence, model routing, session durability, handoff, learning and change provenance as one coherent system.

## 1. Target users and pain points

| User | Pain today | RapidLM V2 answer |
|---|---|---|
| Senior engineer | Agent rereads code, loses architecture, emits noisy diffs | Hybrid context compiler, read tracking, code graph, semantic patches |
| Staff/Principal engineer | Large changes are difficult to delegate safely | Goal/Evidence DAG, coordinator, persistent background agents, clean-context managed workers, isolated workspace views |
| Security engineer | Shell/MCP/browser/desktop tools have broad authority | Capability Broker, deny-first policy, action-scoped leases, tiered sandbox, Computer Use risk classes, audit ledger |
| Platform team | Local agents cannot reliably continue off-device | Versioned Execution Handoff, daemon, remote workers, anti-split-brain execution leases |
| AI infrastructure engineer | Harness changes are hard to optimize scientifically | Deterministic/live/replay harness, trajectory ranking, experiment registry, held-out gates, long-horizon endurance suites |
| Web/desktop engineer | Agents cannot prove visual behavior correctly | Browser + full desktop/TUI Computer Use, accessibility-first targets, annotated video and structured evidence |
| Mobile engineer | Emulator flows are fragmented and manual | Android/iOS simulator supervisors normalized into Computer Use observe→act→verify contracts |
| Engineering manager | Parallel-agent cost/progress is opaque | Agents Panel with lineage, budgets, blockers, evidence, workspace ownership, cost and trajectory summaries |
| Organization/team | Rules and workflows are repeatedly rediscovered | Trigger-scoped Knowledge Registry + versioned Playbooks/Automations, distinct from conversational memory |
| Local-model/OSS user | Vendor lock-in and cloud-only context | Provider-neutral LLM Router, local indexing/embeddings, offline lexical+structural path |

## 2. V2 product thesis and differentiators

### 2.1 Managed Agent Mesh, not transcript cloning

RapidLM distinguishes **persistent background agents** from bounded **managed workers**. A coordinator sends each child a typed `TaskEnvelope` containing objective, acceptance criteria, a minimal context packet, Knowledge references, workspace access, capability ceiling, model policy and budget. The raw parent transcript is not copied into children. Write-capable workers receive separate transactional workspace views; results return through `AgentResultV2` with evidence, artifacts/ChangeSets, usage and blockers.

### 2.2 Context compiler, not context stuffing

The Context Engine compiles a bounded packet from current goal, applicable policy/Knowledge, read-set deltas, symbol/graph hits, lexical and optional vector retrieval, current diff, tests, artifacts and explicit attachments. Every context item carries token cost, provenance, freshness and reason-for-inclusion. Hybrid ranking + MMR + token budgeting reduce repeated context.

### 2.3 Stable model-visible tool gateway

Models see a deliberately small stable gateway (`repo.search`, `repo.read`, `workspace.patch`, `shell.exec`, `agent.spawn`, `browser.act`, `computer.act`, `goal.update`, `external.call`). Capability resolution occurs behind the gateway. This keeps policy independent from provider tool catalogs and reduces schema/prompt-cache churn.

### 2.4 Goal DAG + evidence-backed completion

Autonomous work is represented as goals with criteria, dependencies, budgets and stop reasons. Evidence nodes bind tests, command outputs, scanner results, code locations, screenshots, recordings and review decisions to criteria. Model prose alone cannot mark mandatory work complete.

### 2.5 Execution Handoff without split brain

A session can migrate among local, daemon and remote workers through a signed, versioned `HandoffBundle`. The source quiesces writes, captures goal/event/context/workspace/agent state, transfers no live capability leases or secret plaintext, and commits a new `SessionExecutionLease` generation only after target verification. At most one execution generation may own side effects.

### 2.6 Human/agent control ownership

Terminal, browser, desktop and mobile input are protected by `ControlLease`s. A human can pause agent input, take control for MFA/CAPTCHA/manual inspection or arbitrary intervention, then return control after a fresh observation and reconciliation. Human actions are attributed as human actions, not fabricated as agent actions.

### 2.7 Full Computer Use as an engineering subsystem

Computer Use is not browser-only. It normalizes `BrowserPage`, `Desktop`, `Window`, `Tui`, `AndroidEmulator`, `IosSimulator` and `RemoteDesktop` surfaces. Target resolution is semantic-first: DOM/test-id/accessibility/native controls/TUI semantics → vision → coordinates. Coordinate actions are observation-bound and rejected if stale. Browser/desktop/mobile E2E runs can capture structured assertions, application logs, screenshots and annotated video as redacted Evidence Nodes.

### 2.8 Knowledge, Memory, Skills, Playbooks and Policy are distinct

- **Memory:** what happened in prior work.
- **Knowledge:** governed facts/rules retrieved by explicit triggers and scope.
- **Skills:** reusable procedural instructions/capabilities; they grant no authority.
- **Playbooks:** versioned multi-step operational workflows.
- **Policy:** what actions are allowed/require approval/denied.

Conflating these concepts causes stale context and privilege ambiguity, so they have separate contracts and lifecycle rules.

### 2.9 Harness as optimization infrastructure

The Eval & Agent Harness runs deterministic scripted-model tests, trace replay, real sandbox scenarios and live-model experiments using production contracts. V2 additionally emits governed observable `TrainingTrajectory` records, ranks candidates after hard correctness/security gates, supports experiment registries and held-out evaluation, and runs 1h/4h/12h/24h endurance suites including >=1,000-tool-call fault scenarios. It never requires or reconstructs hidden chain-of-thought.

### 2.10 Security is the execution substrate

Every side effect requires broker authorization and executor re-validation of a short-lived capability lease. Sandboxes expose explicit file/network/env/resource policies. GUI sandboxes add app/window/clipboard/file/network boundaries. Required isolation must fail closed rather than silently degrade.

## 3. High-level product vision

**RapidLM V2 should feel less like a chatbot attached to a shell and more like a terminal-native engineering operating system.** A developer can begin locally, delegate to a managed agent fleet, inspect or take over any execution surface, hand the session to a remote worker, return later, replay what happened, inspect evidence and cost, and use governed trajectory insights to improve future runs—all while using the same kernel/session/tool/policy contracts.

The same runtime powers:

- interactive TUI;
- deterministic headless JSONL;
- ACP/editor integration;
- SDK embedding;
- daemon/background goals;
- local and remote sandbox workers;
- managed agent fleets;
- browser, desktop, TUI and mobile Computer Use;
- scheduled/event-driven Playbooks and Automations.

## 4. Competitive design lineage

V2 explicitly studies and synthesizes the strongest primitives from Muse Code, Devin, Kimi Code, Grok Build, Qwen Code, OpenAI Codex, Claude Code, Augment, Atomic and Cursor. The source-backed extraction is in [`research/competitive-analysis.md`](research/competitive-analysis.md), and the feature-by-feature mapping is in [`research/feature-inspiration-matrix.md`](research/feature-inspiration-matrix.md).

The design principle is **synthesis, not feature copying**: RapidLM preserves one coherent kernel, event, policy, evidence and workspace model across all borrowed ideas.

## 5. Dossier navigation

### Core implementation documents

1. [`01-PRD.md`](01-PRD.md) — product requirements, including 61 V2 requirements for managed agents, handoff, control ownership, Knowledge/Playbooks, trajectory learning and full Computer Use.
2. [`02-SDD.md`](02-SDD.md) — complete V2 system architecture and runtime flows.
3. [`03-Development-Plan-and-Roadmap.md`](03-Development-Plan-and-Roadmap.md) — phased roadmap through V2 GA.
4. [`04-Task-Breakdown.md`](04-Task-Breakdown.md) — hierarchical work breakdown.
5. [`prompts.md`](prompts.md) — canonical **320-task** atomic implementation DAG. Agents execute from here.
6. [`task-manifest.json`](task-manifest.json) — machine-readable task DAG.
7. [`V2-CHANGELOG.md`](V2-CHANGELOG.md) — exact V1→V2 architectural changes.
8. [`AGENTS.md`](AGENTS.md) / [`SKILLS.md`](SKILLS.md) — development-agent governance and specialist workflows.

### Architecture — core runtime and development platform

- [`architecture/kernel.md`](architecture/kernel.md)
- [`architecture/event-ledger-and-sessions.md`](architecture/event-ledger-and-sessions.md)
- [`architecture/goal-dag-and-evidence.md`](architecture/goal-dag-and-evidence.md)
- [`architecture/context-engine.md`](architecture/context-engine.md)
- [`architecture/capability-broker-and-policy.md`](architecture/capability-broker-and-policy.md)
- [`architecture/sandbox.md`](architecture/sandbox.md)
- [`architecture/process-supervisor-and-jobs.md`](architecture/process-supervisor-and-jobs.md)
- [`architecture/workspace-vcs-and-semantic-patch.md`](architecture/workspace-vcs-and-semantic-patch.md)
- [`architecture/vcs-provenance-and-attestations.md`](architecture/vcs-provenance-and-attestations.md)
- [`architecture/llm-router.md`](architecture/llm-router.md)
- [`architecture/prompt-runtime.md`](architecture/prompt-runtime.md)

### Architecture — V2 managed autonomy

- [`architecture/agent-pool-and-managed-agents.md`](architecture/agent-pool-and-managed-agents.md)
- [`architecture/agents-panel.md`](architecture/agents-panel.md)
- [`architecture/execution-handoff.md`](architecture/execution-handoff.md)
- [`architecture/human-agent-control-handoff.md`](architecture/human-agent-control-handoff.md)
- [`architecture/knowledge-registry.md`](architecture/knowledge-registry.md)
- [`architecture/playbooks-and-automations.md`](architecture/playbooks-and-automations.md)
- [`architecture/trajectory-learning-and-optimization.md`](architecture/trajectory-learning-and-optimization.md)
- [`architecture/session-insights.md`](architecture/session-insights.md)
- [`architecture/eval-and-agent-harness.md`](architecture/eval-and-agent-harness.md)

### Architecture — Computer Use and user surfaces

- [`architecture/computer-use.md`](architecture/computer-use.md) — browser + full desktop + TUI + mobile/remote surface model.
- [`architecture/mobile-simulator.md`](architecture/mobile-simulator.md)
- [`architecture/cli-tui.md`](architecture/cli-tui.md)
- [`architecture/ide.md`](architecture/ide.md)
- [`architecture/mcp-acp-and-sdk.md`](architecture/mcp-acp-and-sdk.md)

### Architecture — security/extensibility/operations

- [`architecture/security-scanners.md`](architecture/security-scanners.md)
- [`architecture/authentication-and-identity.md`](architecture/authentication-and-identity.md)
- [`architecture/plugins-skills-and-hooks.md`](architecture/plugins-skills-and-hooks.md)
- [`architecture/remote-workers-and-daemon.md`](architecture/remote-workers-and-daemon.md)
- [`architecture/artifact-store.md`](architecture/artifact-store.md)
- [`architecture/telemetry-and-observability.md`](architecture/telemetry-and-observability.md)
- [`architecture/project-and-config.md`](architecture/project-and-config.md)
- [`architecture/updates-and-release.md`](architecture/updates-and-release.md)

### Contracts, models and evaluation

- `api-contracts/` — versioned kernel/tool/policy/workspace/context/sandbox/managed-agent/handoff/Computer Use/trajectory contracts.
- `data-models/` — canonical domain, event, SQLite and configuration models.
- [`threat-model-and-security.md`](threat-model-and-security.md) — threat register and security requirements.
- `evaluation-specs/` — agent, context, security, Computer Use/mobile, managed-agent/handoff and long-horizon trajectory suites.
- [`requirements-traceability.md`](requirements-traceability.md) — requirement → design → task → verification mapping.
- `system-prompts/` — versioned runtime prompt library, including coordinator/background explorer/Computer Use/Session Insights prompts.
- `adrs/` — accepted architecture decisions including V2 ADRs 0013–0019.
- `operations/` — observability, testing, release/supply-chain and incident recovery.
- `reference/` — CLI commands, error codes, glossary and definition of done.

## 6. Normative hierarchy

`MUST`, `MUST NOT`, `SHOULD`, `SHOULD NOT`, and `MAY` are normative. If documents conflict, precedence is:

1. security invariants and explicit user requirements;
2. `01-PRD.md`;
3. accepted ADRs;
4. `02-SDD.md`;
5. module architecture documents;
6. API/data contracts;
7. task prompts.

An implementation agent that discovers a contradiction MUST stop that task with evidence and request a contract/ADR correction rather than creating a competing interface.

## 7. Baseline implementation topology

```text
rapidlm/
├── apps/rapid/                         # composition root / binary
├── crates/
│   ├── protocol/                       # stable IDs/events/contracts
│   ├── kernel/                         # lifecycle/orchestration
│   ├── event-ledger/                   # WAL/events/projections/replay
│   ├── agent-runtime/                  # turns/goals/workers/coordinator
│   ├── agent-pool/                     # persistent background-agent host
│   ├── scheduler/                      # DAG scheduling/budgets/fairness
│   ├── context-engine/                 # indexing/retrieval/read tracking
│   ├── knowledge/                      # trigger-scoped Knowledge Registry
│   ├── playbooks/                      # Playbooks + Automation runtime
│   ├── tool-gateway/                   # stable model-visible tools
│   ├── capability-broker/              # policy decisions/action leases
│   ├── process-supervisor/             # process groups/background jobs
│   ├── sandbox/                        # host/container/gVisor/remote/GUI
│   ├── workspace/                      # views/transactions/semantic patch
│   ├── vcs/                            # Git/provenance/attestation
│   ├── llm-router/                     # providers/routing/fallback/budgets
│   ├── handoff/                        # execution/control handoff protocol
│   ├── harness/                        # deterministic/live/replay evals
│   ├── trajectory/                     # governed observable trajectories
│   ├── insights/                       # deterministic session analysis
│   ├── computer-use/                   # browser/desktop/TUI/remote UI
│   ├── mobile-sim/                     # Android/iOS simulator supervision
│   ├── security/                       # scanners/security helpers
│   ├── auth/                           # credentials/identity/SecretHandle
│   ├── mcp/                            # MCP client/server bridge
│   ├── acp/                            # ACP bridge
│   ├── plugin-host/                    # WASM plugins/skills/hooks
│   ├── telemetry/                      # logs/metrics/traces/redaction
│   └── tui/                            # terminal projection/UI
├── sdk/typescript/
├── prompts/
├── evals/
└── docs/
```

## 8. Initial toolchain pins

For the implementation baseline dated 2026-08-14:

- Rust `1.97.1` pinned in `rust-toolchain.toml`.
- Node.js `24.19.0` LTS for SDK/tooling.
- pnpm `11.20.x`; pin exact patch at bootstrap.
- SQLite through the chosen Rust binding/bundled build with FTS5 enabled.

Dependencies are lockfile-pinned and governed by `operations/release-and-supply-chain.md`.

## 9. V2 definition of production-ready

V2 is production-ready only when all V1 gates still hold **and** the following are demonstrated by release suites:

- persistent background agents recover without unbounded private-context growth;
- clean-context workers cannot inherit raw parent transcript or live capability leases;
- parallel write-capable workers never share the same writable `WorkspaceView`;
- execution handoff fault tests prove at most one valid side-effect owner generation;
- human/agent takeover never produces concurrent input ownership and resumes only after fresh observation/reconciliation;
- Knowledge and Playbooks cannot grant capabilities or silently bypass policy;
- trajectory export is opt-in/policy-gated, redacted and contains no requirement to capture hidden chain-of-thought;
- held-out evals gate any prompt/router/context/harness variant promoted from trajectory experiments;
- 1h/4h/12h/24h endurance tiers and a >=1,000-tool-call fault scenario meet reliability budgets;
- Computer Use executes semantic-first, rejects stale visual/coordinate targets, redacts secrets, treats UI text as untrusted data and provides evidence-complete browser/desktop/mobile E2E runs;
- required sandbox/GUI isolation fails closed when unavailable;
- session replay, handoff, agent lineage, ControlLease history, evidence and cost remain inspectable from TUI/headless/SDK projections.

The engineering north-star is **verified engineering success per token, per unit cost, and per unit of privilege granted**.
