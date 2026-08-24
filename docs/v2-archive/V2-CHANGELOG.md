# RapidLM CLI / TUI — V2 Architecture Revision

**Revision:** v2.0 architecture baseline  
**Date:** 2026-08-14  
**Compatibility:** V2 preserves the V1 kernel/event/capability/workspace foundations and adds managed-agent, handoff, trajectory-learning, knowledge/playbook, and full Computer Use subsystems.

## Why V2 exists

V1 established a strong local-first agent runtime: event-sourced sessions, stable gateway tools, capability leases, transactional workspace views, a hybrid Context Engine, goal/evidence completion, a sandbox ladder, model routing, and a deterministic evaluation harness. The V2 review identified two important underrepresented reference systems—**Meta Muse Code** and **Cognition Devin**—whose public architecture/product patterns materially strengthen long-horizon autonomy and engineering operations.

V2 does not replace the V1 architecture. It extends it in five directions:

1. **Managed Agent Mesh** — session-long background agents plus clean-slate isolated managed workers coordinated by a parent agent.
2. **Execution Handoff and Human Takeover** — move a durable session between local/remote workers and safely transfer control between agent and human without split-brain writes.
3. **Knowledge + Playbooks** — separate durable trigger-scoped organizational knowledge from episodic memory, reusable skills, orchestration playbooks, and security policy.
4. **Trajectory Learning & Session Insights** — turn the harness into a replayable optimization/data flywheel while preserving privacy and never requiring hidden chain-of-thought capture.
5. **Computer Use V2** — full browser + desktop + TUI + mobile visual interaction, diff-aware E2E testing, annotated video evidence, accessibility-first targeting, secure credential injection, and human takeover.

## Primary inspiration added in V2

### Muse Code / Muse Spark 1.2

Official Meta material published 2026-08-05 describes:

- persistent async background agents that remain active throughout a session;
- a local append-only event log covering model calls, tool runs, approvals and edits, enabling replay-exact crash recovery;
- bundled `/plan`, `/grill`, and `/goal` workflows;
- co-training the coding model and harness using rejection-sampled harness trajectories and optimizations for goals, compaction, subagents and tool usage;
- long-horizon training across whole-repository projects and auto-research;
- self-generated coding environments and candidate grading;
- kernel-optimization evaluations exceeding 1,000 tool calls and running up to 24 hours.

Source: https://research.meta.ai/blog/introducing-muse-code-and-muse-spark-1-2

### Devin

Official Cognition material documents:

- a coordinator that scopes work and delegates to managed Devins running in isolated VMs with clean contexts;
- parent access to child trajectories, per-child compute visibility and lifecycle control;
- `/handoff` from a local CLI to an independent cloud VM with repo/branch, context, and uncommitted changes;
- full-desktop Computer Use using mouse/keyboard/screenshots beyond browser-only automation;
- human takeover of the IDE/terminal/browser, followed by resumption;
- trigger-scoped Knowledge, reusable Skills, automations, Session Insights and improved-prompt suggestions;
- end-to-end testing with video recordings; Android emulator use through both `adb` and visual desktop Computer Use.

Sources:

- https://cognition.ai/blog/devin-can-now-manage-devins
- https://docs.devin.ai/cli/handoff
- https://docs.devin.ai/work-with-devin/computer-use
- https://docs.devin.ai/work-with-devin/devin-session-tools
- https://docs.devin.ai/product-guides/knowledge
- https://docs.devin.ai/product-guides/session-insights
- https://docs.devin.ai/work-with-devin/testing-and-recordings
- https://docs.devin.ai/onboard-devin/environment/android-emulation

## Architectural additions

| V2 subsystem | New primary document |
|---|---|
| Managed agent mesh / persistent background pool | `architecture/agent-pool-and-managed-agents.md` |
| Local/remote execution migration | `architecture/execution-handoff.md` |
| Human/agent exclusive control transfer | `architecture/human-agent-control-handoff.md` |
| Trigger-scoped organizational knowledge | `architecture/knowledge-registry.md` |
| Repeatable workflows and event/schedule automation | `architecture/playbooks-and-automations.md` |
| Training-ready trajectories and harness optimization | `architecture/trajectory-learning-and-optimization.md` |
| Post-session diagnosis and improvement suggestions | `architecture/session-insights.md` |
| Full browser/desktop/TUI visual interaction | `architecture/computer-use.md` (rewritten for V2) |
| Harness as evaluator + data factory | `architecture/eval-and-agent-harness.md` (rewritten for V2) |

## V2 compatibility rules

- Existing V1 event IDs and wire schemas remain readable; V2 uses additive schema evolution unless an ADR explicitly approves a break.
- Capability leases never cross a handoff boundary. The target runtime re-evaluates policy and issues new leases.
- An execution handoff never resumes two write owners. Source session execution is parked before the target can acquire the `SessionExecutionLease`.
- Persistent background agents are **read-only by default**. Write-capable work is delegated to an isolated managed worker or explicit transactional view.
- Trajectory collection stores observable inputs/actions/results, compact state and metrics. It MUST NOT require or infer hidden chain-of-thought.
- Proprietary user code/trajectories are excluded from external training by default; any export requires an explicit data policy and redaction pass.
- Computer Use never receives ambient permission to type secrets, upload files, publish externally, approve OS security dialogs, or execute financial/destructive actions. These remain separately capability-gated.
