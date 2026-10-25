# Agent Harness — Production Architecture

## 1. Definition

The production Agent Harness is the layer that executes Agent nodes using a model, prompt, bounded context, projected tool surface, workspace and capability ceiling. It does not own the Runtime Graph, policy or workspace truth.

## 2. AgentExecutionContext

Contains `NodeId/AttemptId`, role, objective, acceptance criteria, ContextPacket, applicable rules/skills, tool projection, workspace view, model route, token/cost/time/tool budgets, parent graph refs and evidence obligations. Parent transcript is excluded unless a small explicitly selected excerpt is necessary.

## 3. Roles

Built-ins: main, context-scout, planner/architect, coder, debugger, reviewer, tester, independent-verifier, security-reviewer, performance-reviewer, browser/computer operator, release-manager, session-insights. Persistent specialists: Explorer, Context Curator, Test/Dependency watcher; read-only by default.

### Agent definitions

A role is a base. A project, a user or a shared directory can define a named agent on top of one — `<id>.md` (front matter, then the instructions as the body) or `<id>.toml` — and the definition is **a request to narrow, never a grant**: its tools are the role's classes less the ones it leaves out, then `allow_tools`/`deny_tools` patterns; its `permission` rules can only refuse or ask; `max_steps` bounds a run; `mode` says whether the agent is offered to `task_spawn` (`subagent`), is a session's main agent (`primary`) or both; `spawn` holds a main agent's delegation to a list; an `allow_tools` pattern starting with `mcp__` gives a child those MCP tools from the session's connections. A session runs as one such agent when `--agent` or `[agents] default_primary` says so — fixed for the session, recorded as `agent.selected`, re-applied on resume. The same validator reads both syntaxes; the host's lattice and the managed policy sit above every definition. See `docs/reference/agent-definitions.md` and ADR 0026.

## 4. Capability Projection

```mermaid
flowchart TB
  TR[Full Tool Registry] --> CP[Capability Projection]
  ROLE[Role] --> CP
  NODE[Graph node] --> CP
  MODE[plan/code/review/headless] --> CP
  POL[Effective policy] --> CP
  MOD[Model capability profile] --> CP
  CP --> SURF[Small stable model tool surface]
  SURF --> GW[Tool Gateway]
  GW --> CB[Capability Broker]
```

Schema projection reduces tokens and bad calls but is not a security boundary; execution still rechecks policy/leases.

## 5. Delegation

Delegation is a graph proposal. Host decides model, workspace, resources, tool projection and scheduling. Utility score can consider expected quality/latency gain, context duplication, spawn cost, merge-conflict risk and verification independence. Nested delegation is depth/budget bounded; built-in Context Scouts are read-only and do not recursively spawn unless explicitly allowed.

## 6. Loop robustness

Per Agent attempt implement: empty-response retry within budget, repeated exact tool-call detector, repeated streaming/message detector, provider truncation → compact then retry, malformed tool repair feedback, budget convergence prompt, cancellation. None may loop indefinitely.

## 7. Result contract

`AgentResult { outcome, summary, claims, evidence_refs, artifact_refs, workspace_transaction, open_questions, blockers, usage, context_lineage, tool_repair_stats }`. Results are observations/proposals until host accepts required workspace/evidence actions.

## 8. System prompt philosophy

Prompts synthesize Muse-style evidence/verification discipline, Claude-style concise layered instruction/tool boundaries, and Augment-style exhaustive scoped context gathering. Prompt text is newly authored for RapidLM; competitor wording is not copied. See `system-prompts/`.

## 9. Model routing

Each role/node can route separately. Cheap capable models should handle retrieval/scouting/classification where evals show parity; high-reasoning models reserved for planning, hard coding or verification as configured. Routing includes measured tool-repair reliability rather than benchmark quality alone.

## 10. Harness observability

Track per attempt: prompt hash, model route, ContextPacket, tools exposed/called, validation/repairs, policy decisions, tokens/cost, wall time, output/claims, evidence, stop reason, graph mutation proposals and final host verdict. No hidden chain-of-thought is required.
