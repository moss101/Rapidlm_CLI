# ADR 0023 — Inbox edges: durable messages to running and finished subagents, with three delivery modes

**Status:** Accepted for `SEAMS-RAPIDLM-01` (Phase 0; governs `SEAM-04`, used by `SEAM-12` `/aside`)  
**Date:** 2026-09-20  
**Baseline audit:** [SEAM-00-1](../goals/seams-phase0-baseline-2026-09-20.md) §3 (SEAM-04)  
**Builds on:** ADR 0002 (event-sourced durable state), 0004 (graph-native orchestration), 0012/0013 (evidence, verifier), 0021 (graph replay); invariants 1, 3, 7, 12, 13; `SEAMS §3.2 S1, S5, S6`

## Context (what exists at the baseline)

- A subagent is the model-invoked `task_spawn` tool (`apps/rapid/src/exec_tools.rs:3846–4085`): arguments `prompt, type ∈ {general-purpose, explore, plan}, description, write_scope, background`. Inline children run on the parent's thread inside the tool call; `background: true` runs the child on its own thread with the report in the job spool (`job_status`/`job_output`). `LiveSubagentRunner::run` (`interactive.rs:2902–3129`) builds a fresh model, fresh `ExecTools` (rooted at an isolated worktree when write-capable) and drives `run_live_exec`. The child is always `AgentRole::Coder` with the `subagent` permission profile; `RoleRegistry` and the `.rapidlm/agents/*.toml` definitions (`agent_runtime::agent_defs`) are not consulted.
- Nothing can reach a running child: `SubagentRegistry` holds `(AgentId, CancellationToken)` pairs only. A finished child cannot be continued (`exec_tools.rs:3976–3980` says so). The detached ceiling `MAX_DETACHED_SUBAGENTS = 4` rejects the fifth spawn rather than queueing it.
- The ledger already declares `agent.mail.sent` / `agent.mail.dropped` (`crates/event-ledger/src/event.rs`) with **no emitter**; `agent_runtime::specialist::PersistentSpecialist` has a bounded typed mailbox with no caller. The graph has `DelegatedTo`, `Supersedes`, `TriggeredBy` edges and no message edge.

## Decision

1. **An inbox message is a ledger event first.** `agent.mail.sent` gets its first producer: payload `{ record: "rapidlm.agent.mail/v1", message_id, to: AgentId, from: AgentId|"user", delivery: interject|steer|queue, body (bounded 16 KiB, same as `prompt`), turn_id }`. `agent.mail.dropped` records a message that could not be delivered (`reason: bounded | terminal_without_continue | unknown_agent`). A message that is not durable is not sent (append before enqueue, invariant 4).
2. **One session-shared `Inbox`, drained at three points by the child's own turn loop.** The runner passes the child an `InboxHandle` (per `AgentId`); the agent-runtime turn loop (`crates/agent-runtime/src/turn.rs`) gains a `MessageSource` it consults: **interject** — checked while the child waits on a tool result or a model stream (the same cancellation-token poll the child already runs); the current wait is pre-empted with a typed `Interrupted { by_message }` result and the message becomes the next user-role message; **steer** — checked at the top of each model step, appended as a user-role message before the next request; **queue** — held until the child's run completes, then delivered by decision 3. Every delivery appends `agent.mail.delivered { message_id, at: wait|turn_boundary|after_completion }` and the transcript labels it `[<mode> from <from> → <agent>]`.
3. **Continuing a finished child is a new turn in the same lineage.** A message to a terminal child (or a `queue` delivery arriving after completion) starts a new `LiveSubagentRunner` turn for the same `AgentId`, seeded with the child's recorded exchange history (the `SuspendedHistory` shape `approvals.rs` already serialises) plus the message. The graph records it as a superseding revision of the child's Agent node (`GraphProposal.supersede` + a new node with a `Supersedes` edge) when the session runs under the graph; otherwise the ledger's `agent.state_changed { continued_from }` carries the lineage. The original report is immutable (invariant 3).
4. **Admission is a bounded FIFO, not a rejection.** `SubagentRegistry` gains a queue: a spawn above the concurrency ceiling waits in order for a slot (the parent's turn sees `queued (position n)` in the tool result and the `/agents` row), with the wait bounded by the turn's wall-time ceiling. The ceiling is `min(built-in, managed.max_concurrent_subagents)` — narrow-only (S5). The per-turn spawn *budget* stays as it is.
5. **`agent_type` resolves through the definitions.** `type` is looked up in `agent_defs::full_inventory` (builtins + `.rapidlm/agents/` (project, trust-gated) + `~/.rapidlm/agents/` (user; lower precedence than project, cannot shadow builtins)). Definitions gain `base_role`, `instructions`, `model`, `reasoning_effort`, `inputs`, `outputs`; validation stays narrow-only (`validate_grants`); the resolved definition sets the child's role, tool surface, model and effort, and its `instructions` are the child's system context. Unknown `type` is a typed refusal listing the known ids. No `task_spawn` parameter can widen the surface (there is none today; the rule is stated so none is added).
6. **Declared outputs are checked.** A definition's `outputs` are names the child's `AgentResult` artifacts/claims must carry; a missing one is `SubagentEnd::IntegrationFailed { missing_outputs }`, never a silent success.
7. **Wait ceiling.** Inline children inherit the parent turn's ceiling; detached children get `subagent.wait_ceiling` (default one hour). Expiry is reported as `still running` (the child is not killed), and `/agents` shows it.

## What is deliberately not done

- No new mailbox type: `PersistentSpecialist`'s `SpecialistMessage` stays for read-only specialists; the inbox carries user/parent text, which is a different contract, and the two are not merged in this program.
- No graph-level message *edge kind* is added until the session itself runs under the graph; the event is the record (S1: "a node, an event or a gate"). When ADR 0021's work brings live sessions onto the graph, the event maps onto an edge without a migration.
- No cross-process delivery through the daemon beyond what the ledger gives: a reconnecting client sends mail by appending the event; the process that owns the child drains it.

## Consequences

- `SEAM-04`'s slices: (a) definitions gain fields and the spawn path resolves `type` through them; (b) inbox events + `MessageSource` in the turn loop + three delivery tests; (c) continuation; (d) admission queue + managed ceiling; (e) outputs check + wait ceiling.
- `/aside` (`SEAM-12`) is a read-only child with no inbox and no parent-context delivery; its answer is never appended to the parent's history.
- `docs/reference/event-catalog.md` and the SDK wire catalog gain `agent.mail.delivered`; `agent.mail.sent/dropped` gain their payload shape.
