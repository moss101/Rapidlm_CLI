# P5 Calibration — Agent Harness, Prompts, Models & Skills (source-backed)

Calibration of every P5 task (P5-001..P5-032) against actual repository code, tests and
the V3 target (`docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`).
Manifest `NOT_STARTED` does **not** mean the code is missing; most P5 work already exists and
is KEEP/ADAPT. No subsystem is duplicated; the existing agent-runtime is the single harness
authority (no second scheduler/supervisor is introduced).

## Subsystem dispositions

| Subsystem | Crate | Disposition | Notes |
|---|---|---|---|
| Turn loop / engine | `agent-runtime` (`turn.rs`) | KEEP | `run_turn`, `ModelDriver`/`ToolDriver`, `TurnBudget`, `TurnStopReason`. Extended with loop guard (P5-022). |
| Agent identity/lifecycle | `agent-runtime` (`agent/model.rs`) | KEEP | `AgentRole`, `AgentSpec`, `AgentResult`, `AgentState`, budgets, `validate_transition`. |
| Subagent scheduler | `agent-runtime` (`agent/scheduler.rs`) | KEEP | `Scheduler`, `SpawnAgent`, priorities, provider occupancy, global cost budget. |
| Subagent spawn | `agent-runtime` (`agent/spawn.rs`) | KEEP | `spawn_agent`, isolated child view, lifecycle events, detached, cancellation. |
| Orchestration | `agent-runtime` (`orchestration/*`) | KEEP | Supervisor, TaskContract, evidence, verification; already reconciled with host graph. |
| Compaction | `agent-runtime` (`compaction.rs`) + `context-engine` | KEEP | Structured session compaction + durable artifact/event. |
| Prompt composition | `agent-runtime` (`prompt.rs`) | KEEP | Versioned/hashed compiled prompt; AGENTS.md hierarchy already read. |
| Loop robustness | `agent-runtime` (`loop_guard.rs`) | ADAPT (new) | Pure `ToolCallLoopDetector` wired into `run_tool_steps`; `MessageLoopDetector` ready. |
| Model routing | `llm-router` (`route/`, `catalog.rs`, `fallback.rs`) | KEEP | `RouteDecision`, hard capability filters, safe retry/fallback. |
| Provider capabilities | `llm-router` (`provider.rs`) | KEEP | `ProviderCapabilities`/`ModelCapabilities` hard filters. |
| Skills | `plugin-host` (`skills.rs`) | KEEP | `SkillDescriptor`, `SkillLoader`, `SkillPromptData`, resources. |
| Context fabric | `context-engine` | KEEP | read/read-set/scout/compact; knowledge is separate from skill prompts. |
| P-032 deferred | `auth` (`store.rs`/`file_keychain.rs`) | ADAPT | Durable file-backed `PlatformKeychain`; OS-keychain deferred (carry-forward). |

## Per-task classification

Class legend: `VE`=VERIFIED_EXISTING, `PS`=PARTIALLY_SATISFIED, `NI`=NOT_IMPLEMENTED, `NV`=NEEDS_VERIFICATION.

| ID | Title | Class | Basis / gap |
|---|---|---|---|
| P5-001 | AgentExecutionContext and AgentResult | VE | **Complete** — canonical `AgentResult` contract with claims/open_questions/blockers/context_lineage/tool_repair_stats (typed, bounded, serialized, no-leak); `AgentSpec` is the execution context; 3 in-repo tests. |
| P5-002 | node AgentExecutor interface | VE | **Complete** — canonical host-owned `AgentExecutor` (`AgentExecutionRequest` + trait + `TurnAgentExecutor`) runs `run_turn` and assembles a canonical `AgentResult` from host state; `TurnResult` exposes bounded `terminal_output`; authoritative subagent path `spawn_agent` runs through `TurnAgentExecutor`. SEE VERIFIED ledger P5-002. |
| P5-003 | role registry and capability profiles | VE | **New** `agent_runtime::role_profile::{RoleRegistry,RoleProfile,RoleToolSurface,RoleModelPolicy}` metadata-only per-role registry (tool surface, model tier, read-only/can-delegate); ADAPTs existing `AgentRole`; 4 in-repo tests. |
| P5-004 | main agent role | VE | `AgentRole::Main`; exercised by scheduler/spawn. |
| P5-005 | Context Scout role | VE | `AgentRole::{Explorer,ContextCurator}` + `context-engine::scout`. |
| P5-006 | planner/architect role | VE | `AgentRole::Planner`. |
| P5-007 | coder/debugger roles | VE | **ADAPT** — added `AgentRole::Debugger` + `Coder` profile via `RoleProfile`. |
| P5-008 | reviewer/tester roles | VE | **ADAPT** — added `AgentRole::Tester`; refined `Reviewer` (read-only workspace, may run checks). |
| P5-009 | independent verifier role | VE | `AgentRole::Verifier` + orchestration verification. |
| P5-010 | security/perf reviewer roles | VE | **ADAPT** — added `AgentRole::PerformanceReviewer`; refined `SecurityReviewer` (read-only scanner with Exec). |
| P5-011 | browser/computer operator role | VE | **ADAPT** — added `AgentRole::BrowserOperator` (Read+Write+Browser+Mobile). |
| P5-012 | release-manager role | VE | **ADAPT** — added `AgentRole::ReleaseManager` (Read+Write+Exec+Git, delegating). |
| P5-013 | clean-context TaskEnvelope | VE | **Closed** — `SpawnRequest` (task + role/profile + view + budgets) + bounded `with_task_context` delimited as untrusted data; parent transcript structurally excluded; tests `child_prompt_carries_only_explicitly_selected_state`, `selected_context_is_delimited_untrusted_data_not_instructions`. |
| P5-014 | typed agent mailbox/result refs | VE | `agent/result.rs` typed `AgentResult` + result/workspace/artifact refs, parent-inspect. |
| P5-015 | isolated subagent lifecycle | VE | `spawn_agent` isolated child view + lifecycle events + detached. |
| P5-016 | persistent read-only specialist lifecycle | VE | `agent_runtime::specialist::PersistentSpecialist` + `SpecialistPool` (bounded, read-only-only admission, message/generation routing, cancel_all) — 8 in-repo tests. |
| P5-017 | background specialist bounded summaries | VE | `PersistentSpecialist::retain_summary` (bounded latest) + bounded mailbox + `SpecialistPool` routing — tested. |
| P5-018 | delegation utility scoring | VE | **New** `agent_runtime::delegation::evaluate_delegation` + `recommend` (scoring-only, 0..100, deterministic, NaN-free); 4 in-repo tests. |
| P5-019 | nested delegation depth/budget bounds | VE | **New** `agent_runtime::delegation::{DelegationPolicy,enforce_delegation}` host enforcement (one canonical `DELEGATION_MAX_DEPTH`; depth→budget→safety→scoring); 3 in-repo tests incl. depth-cannot-be-bypassed. |
| P5-020 | agent cancellation/timeouts | VE | `spawn_agent`/`scheduler.cancel`/cancellation tokens + `TurnBudget`. |
| P5-021 | empty-response retry guard | NI | No empty-response handling in `run_model_step`. |
| P5-022 | repeated tool-call loop detector | VE | **New** `loop_guard::ToolCallLoopDetector` wired into `run_tool_steps`; turn fails with `TurnStopReason::RepeatedToolCall`. |
| P5-023 | repeated-message/stream loop detector | VE | **New** `loop_guard::MessageLoopDetector` wired at the goal-continuation boundary (`TurnResult.terminal_hash` + `GoalDriver::next` → `GoalDriverStop::RepeatedMessage`); production-path tests `repeated_terminal_message_stops_the_goal_loop`, `alternating_messages_do_not_stop_the_goal`. |
| P5-024 | compact-before-context-overflow retry | VE | **Complete + production-reachable**: `apps/rapid` `LiveContextHost<B>` owns a live `context_engine::ContextPacket`, rebuilds it via `compact_packet`+`compile` on overflow, couples a `LiveContextModelDriver` with a `LiveRecoveryController`, preserves goal/criteria/AGENTS/skills/workspace/evidence/artifact refs + output reserve, and runs `execute_with_context_recovery` (bounded retry, no effect replay, cancellation, provider-failure no-compaction, lineage). CLI `rapid exec`/`goal` entry wired; `run_live_exec` + `UnconfiguredModel` seam. SEE VERIFIED ledger P5-024. |
| P5-025 | per-subtask model routing | VE | `llm-router::route` `RouteDecision` per request. |
| P5-026 | provider/model capability profiles | VE | `ProviderCapabilities`/`ModelCapabilities` hard filters. |
| P5-027 | routing fallback/circuit breakers | VE | `llm-router::fallback` safe retry/fallback state machine. |
| P5-028 | PromptRegistry version/hash model | VE | `prompt.rs` versioned + hash-bound compiled prompt; system-prompts directory. |
| P5-029 | layered PromptComposer | VE | `prompt.rs` assembles core/role/project/goal/context in fixed precedence. |
| P5-030 | AGENTS hierarchy loader | VE | **New** `agent_runtime::rules_loader::{load_agents,RulesBundle}` deterministic hierarchy loader (root→nested→scope, sibling isolation, fail-closed on non-UTF-8/escape, bounded); 5 in-repo tests. |
| P5-031 | skill metadata registry | VE | `plugin-host::skills::{SkillDescriptor,SkillManifest,SkillLoader}`. |
| P5-032 | skill progressive disclosure | VE | `SkillPromptData`/`SkillResource` bounded progressive disclosure. |

## Dependency-ready execution order

Dependency-ready (no blocking intra-P5 dep beyond the already-met P3/P4 seam), first = **P5-018** (delegation
utility scoring — pure, isolated) or **P5-022** (already implemented this slice). Recommended order:

1. **P5-022** repeated tool-call loop detector — DONE this slice (implemented + wired + tested).
2. **P5-018** delegation utility scoring — pure function, no IO.
3. **P5-021** empty-response retry guard — turn-loop, small.
4. **P5-023** message loop detector wiring — pure detector exists; wire into stream.
5. **P5-019**/P5-024 nested depth + compact-before-overflow — scheduler/compaction seams.
6. P5-001/P5-002/P5-003/P5-013 result/executor/envelope contract completion.
7. Role additions (P5-007/008/010/011/012) — extend `AgentRole`.
8. P5-016/P5-017 specialist/background lifecycles.
9. P5-030 RulesLoader, P5-029/028 prompt registry hardening.
10. Re-verify P5-025/026/027/031/032 (already VE) with a gate audit.

## Fidelity note

Only **P5-022** was executed end-to-end this slice (implemented gap, real in-repo turn test,
ledger/manifest updated). P5-021 and P5-023 are tracked as PS/NI respectively; the remaining
tasks are mostly VE/PS and should be verified (not re-implemented) during the P5 loop.
