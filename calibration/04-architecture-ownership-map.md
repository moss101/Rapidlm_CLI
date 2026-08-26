# E. Architecture Ownership Map

For each authority domain: **current owner** (crate, from source), **V3 target owner** (from dossier), and whether a **duplicate/competing authority** exists. "Conflict" rows need resolution before the next goal.

| Domain | Current authority (crate) | V3 target authority | Duplicate / competing? |
|---|---|---|---|
| **Orchestration (run state)** | `agent-runtime` `GoalStateMachine` (single-snapshot goal) + `goal/driver.rs` | Runtime Graph (`scheduler` crate as graph scheduler; `kernel` GraphService) | **CONFLICT** — three "graph"/"scheduler" names: empty `scheduler` crate (designated home), `agent-runtime::agent::scheduler` (agent concurrency only), `process-supervisor::schedule` (cron only). No authoritative runtime-graph owner exists. |
| **Durability / event log** | `event-ledger` (single authoritative log) | `event-ledger` + graph-revision events | Clean — no competitor. "Operation Journal" (ADR-0003) = the event ledger itself. |
| **Persistence (SQLite)** | `event-ledger::migrations` (schema v2), `context-engine` memory, `vcs` provenance | same + durable attestation | Clean; durable attestation/signature not yet issued. |
| **Context** | `context-engine` (FTS/LSP/graph/vector/compiler/memory) | Context Fabric | Clean; but durable **memory** lives in `context-engine::memory` while V3 P5 assigns memory/knowledge/preferences to the (empty) `knowledge` crate → latent ownership ambiguity. |
| **Tools** | `tool-gateway` (registry/schema/validate/bounded output) | `tool-gateway` + repair/cross-tool invariants | Clean; **tool repair** module absent (P4 gap). |
| **Policy / security** | `capability-broker` (policy/lease/approval/audit) + `security` (scanners/redaction/network/doctor/gate) | same | Clean; `dont-ask` Ask→Deny transform not present in either crate (orchestrator gap). |
| **Workspace mutations** | `workspace` (patches/transactions/backends) + `vcs` (provenance) | `workspace` + `vcs` attestation | **DUPLICATE** — attribution split: `workspace::external_mutation::MutationAttribution` (runtime) vs `vcs::ProvenanceStore` (durable graph). Complementary but overlapping; no single attribution owner documented. |
| **Evidence** | `agent-runtime::evidence` (store + evaluator) | `agent-runtime` evidence fabric | Clean; no separate `VerificationRecord`/`Claim` type (modeled via `criterion_id` + `CriterionVerdicts`). |
| **Verification** | `agent-runtime::evidence::CriterionEvaluator` (evidence-based, fail-closed) | Independent verifier | Clean; no separate verifier crate/subsystem. |
| **Process execution** | `process-supervisor` (jobs/cron/recovery/cancel) | `process-supervisor` + kernel graph nodes | **POTENTIAL DUPLICATE** — background/scheduling authority may also live in `kernel` (graph nodes) and `agent-runtime` once graph lands; triggers/wake-on-event/monitors not in process-supervisor. |
| **Sandbox** | `sandbox` (4 backends, no-downgrade select) | `sandbox` + warm ResourcePool | Clean; warm pool absent. |
| **Computer Use** | `computer-use` (logic) + `mobile-sim` (adb/xcrun) | `computer-use` + Preview Supervisor | **CONFLICT (execution)** — desktop/browser **live drivers fail-closed**; only `Scripted*`/`Fake*` execute. OS-framework linking blocked by `#![forbid(unsafe_code)]`. |
| **Remote execution / handoff** | `kernel` (ControlLease id, fork quiesce), `workspace` (quiesce/merge), `protocol` (remote_worker, lease sig), `event-ledger` (HandoffBundleReady), `sandbox` (remote tier) | `handoff` crate (HandoffBundle/SessionExecutionLease/split-brain) | **CONFLICT** — `handoff` crate is **empty**; handoff logic is scattered across kernel/workspace/protocol/event-ledger. No authoritative handoff owner. |
| **Protocols (MCP/ACP)** | `mcp` (gateway+server+transport+trust), `acp` (stdio/v1/v2) | adapters, not authorities (ADR-0018) | Clean; ACP v2 wraps v1; no conflict. |
| **UI projections** | `tui` (panels/state) + `apps/rapid` (loop) | TUI/CLI + headless | **CONFLICT** — `tui::commands::dispatch` resolves to `KernelAction` but `apps/rapid::apply_kernel_action` collapses to `{}`; panels render in tests but binary never paints (no ratatui dep). Declared vs executed contract diverge. |
| **Provider / model routing** | `llm-router` (adapters/catalog/routing/fallback) | `llm-router` + TLS transport | **BLOCKER** — no TLS transport; cloud providers unreachable. |
| **Eval / learning** | none (empty `harness`, `trajectory`, `insights`) | `harness` (Flight Simulator) + `trajectory` + `insights` | **MISSING** — no owner exists; all three crates empty. |
| **Knowledge / preferences** | `context-engine::memory` (data only) | `knowledge` crate (registry/rules/prefs) | **CONFLICT** — empty `knowledge` crate vs functional memory in `context-engine`; ownership boundary undefined. |

## Duplicate-authority resolution required before P2/P9/P10

1. **Runtime-graph scheduler**: decide whether `scheduler` crate becomes the real graph scheduler or is deleted in favor of `kernel` GraphService + `agent-runtime`. Currently three unrelated "scheduler/graph" names coexist.
2. **Handoff authority**: either implement `handoff` crate as the owner, or formally assign the scattered logic to `kernel`/`workspace` and delete the empty crate.
3. **Attribution owner**: designate `vcs::ProvenanceStore` (durable) as the single attribution authority; keep `workspace::external_mutation` as the runtime detector feeding it.
4. **Memory/Knowledge**: reconcile `context-engine::memory` vs `knowledge` crate; decide if `knowledge` is built or `context-engine` absorbs P5 memory/preference/rule authority.
5. **Daemon authority**: compose `kernel::ipc::IpcServer` with `auth::local_daemon` (currently uncomposed); the shipped binary uses in-process client, so neither daemon path is exercised.
6. **TUI dispatch contract**: align `tui::commands::dispatch` with `apps/rapid::apply_kernel_action` so dispatched commands actually execute/paint.
7. **CancellationToken**: consolidate `agent::model::CancellationToken` and `workspace::CancellationToken` into one shared type.
8. **Persona/source-of-truth**: `prompt.rs::CORE_SYSTEM_V1` (hardcoded, hash-pinned) vs `system-prompts/*.md` — reconcile so runtime prompts match documented personas.
