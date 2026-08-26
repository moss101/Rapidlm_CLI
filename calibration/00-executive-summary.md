# A. Executive Calibration Summary — RapidLM V3 Repository Audit

**Audit date:** 2026-08-24
**Repository:** `/Users/mohsin/projects/RapidLM CLI`
**Audited commit:** `c28572d` (HEAD, branch `main`, upstream `origin/main`)
**Working-tree state (precise):** HEAD is `c28572d`. Against HEAD the tree is dirty: 49 tracked files modified and ~200 untracked source files. **This dirty tree PRE-EXISTED the calibration goal.** The conversation's initial `git_status` snapshot (before any calibration tool call) already listed `M .grok/workflows/rapidlm-v2-implement.rhai`, `M Cargo.lock`, `M apps/rapid/*`, `M crates/*/src/lib.rs` for every crate, plus `??` for the full uncommitted V3 implementation (`apps/rapid/src/{lib,interactive,headless}`, crate modules, `sdk/typescript/src`, `.github/`). The user, in the prior turn, explicitly chose to commit only the docs migration and leave that implementation uncommitted. This goal **did not produce** those 49 modified files, did not produce the workflow `capability_mode` change, and did not produce the untracked source. This goal **added only** the untracked `calibration/` directory (audit artifacts). Reverting the dirty tree would destroy the user's pre-existing in-progress V3 implementation, which the goal forbids (`Do NOT checkout over the user's current work`).
**Audit type:** READ-ONLY calibration. This goal made no source, prompt, policy, sandbox, tool-contract, dependency, or workflow changes. All 8 audit subagents ran with `capability_mode: "read-only"`. The workflow tool was never invoked.

---

## Headline finding

The `docs/development-ledger.md` marks **every** V3 task (including the audit itself, P0) as `NOT_STARTED`. **This is false against repository truth.** The workspace contains ~29 crates of substantial, tested Rust implementation. `cargo check --workspace` and `cargo test --workspace --no-run` both pass (exit 0). Eight audit subagents, reading source directly, found **zero production `todo!`/`unimplemented!` stubs** in the implemented crates — every `panic!`/`unreachable!` is inside `#[cfg(test)]` assertions.

The ledger must be recalibrated from source before any V3 implementation begins.

## Topology audited

29 workspace members (28 crates + `apps/rapid` binary) plus `sdk/typescript`:
`protocol, kernel, event-ledger, agent-runtime, agent-pool, scheduler, context-engine, knowledge, playbooks, tool-gateway, capability-broker, process-supervisor, sandbox, workspace, vcs, llm-router, handoff, harness, trajectory, insights, computer-use, mobile-sim, security, auth, mcp, acp, plugin-host, telemetry, tui`.

## Capability classification counts (approximate, from 8 subsystem audits)

| Status | Approx. count | Notes |
|---|---|---|
| VERIFIED_EXISTING | ~80 | impl + source/test evidence |
| PRESENT_UNVERIFIED | ~15 | impl present, no observed test pass / no production test captured |
| PARTIAL | ~25 | material requirements absent |
| STUB | 8 crates + CU drivers | `harness, agent-pool, knowledge, trajectory, scheduler, handoff, insights, playbooks` are empty `#![forbid(unsafe_code)]` shells; computer-use `Live*` desktop/browser drivers fail-closed |
| MISSING | ~28 | no impl after search (incl. entire Runtime Graph) |
| CONFLICTING | ~6 | see Architecture Ownership Map |

## Largest existing assets worth preserving (KEEP/ADAPT)

- **Kernel + Event Ledger**: transactional SQLite ledger, durable sessions/replay/recovery, CAS artifact store, capability-broker policy/lease engine, sandbox (host/container/gVisor/remote), process-supervisor jobs/cron/leases, agent-runtime harness core (turn loop, subagents, isolated views, budgets, cancellation, goal lifecycle, evidence/verifier with fail-closed completion), context-engine (FTS/BM25, tree-sitter, LSP, vector/hybrid, MMR/rerank, context compiler, durable memory), tool-gateway (schema validation, bounded output), MCP/ACP/plugin-host, TS SDK, telemetry.

## Largest V3 gaps (highest-risk)

1. **Runtime Graph is entirely absent** (P2). No `Node`/`Edge`/`NodeState`, no ready-set scheduler, no fan-out/barriers/joins, no planner, no graph revision/repair. The `scheduler`, `handoff`, `agent-pool` crates that were to hold this are **empty shells**. `agent-runtime` only has a single-snapshot `GoalStateMachine`, not a goal/task DAG.
2. **No TLS transport in `llm-router`** — `Http1Transport` rejects non-`Http` schemes, no `rustls`/`reqwest`. The Anthropic/OpenAI adapters target `https://` and **cannot complete a real call**; only plaintext-local servers work. Vision image bytes and structured-output schema are never transmitted to providers. This blocks all live model execution end-to-end.
3. **Computer Use live drivers are stubs**: all `Live*` macOS AX / Win UIA / Linux AT-SPI / Playwright actors fail-closed (`#![forbid(unsafe_code)]` blocks linking the OS frameworks). Only `Scripted*`/`Fake*` test fakes execute.
4. **Interactive runtime is non-functional end-to-end**: TUI panels render (golden-tested) but `apps/rapid` never calls a paint/`render` and has no `ratatui` dependency; most kernel-mutating slash commands are no-ops (`apply_kernel_action` collapses to `{}`). Headless JSONL emitter is golden-tested but `run()` rejects all subcommands.
5. **Empty crates for full V3 subsystems**: `harness` (eval), `trajectory` (learning), `knowledge` (registry/prefs), `insights` (session insights), `playbooks` (engine) — all unwritten.
6. **Build/release supply chain absent**: no packaging, signing, SBOM, provenance/attestation, release channels, `rapid doctor` CLI, or security scanning in CI.

## Critical migration risks

- **Doc drift / false-ledger**: planning docs claim `NOT_STARTED` everywhere; reality is mixed. Next goal must re-baseline the ledger from source, not trust it.
- **Duplicate/competing authorities** (must be resolved before P2): three "scheduler" names (empty crate, agent-runtime agent-scheduler, process-supervisor cron), three "graph" names (absent runtime graph, kernel `ServiceGraph`, context `CodeGraph`), split daemon authority (IPC server vs `auth::local_daemon`, uncomposed), split attribution (workspace `external_mutation` vs `vcs::ProvenanceStore`), duplicate `CancellationToken` types, and persona divergence (hardcoded `CORE_SYSTEM_V1` vs `system-prompts/*.md`).
- **Wiring gaps**: daemon IPC↔token auth not composed; `SecretBrokerToken::issue` is `#[allow(dead_code)]`; `dont-ask` (`Ask→Deny`) transform not present in any scoped crate; tool repair API absent; workspace `overlay`/`remote` backends declared but unimplemented.
- **TLS blocker (above)** is a hard external-execution blocker, not just a gap.

## Disposition summary (detail in §C)

- **KEEP/ADAPT** (preserve + evolve): kernel, event-ledger, protocol, auth, agent-runtime, context-engine, workspace, vcs, tool-gateway, capability-broker, sandbox, process-supervisor, llm-router, mcp, acp, plugin-host, tui, telemetry, sdk/typescript, apps/rapid.
- **BUILD / INVESTIGATE** (currently empty shells): scheduler (runtime-graph scheduler home), handoff, agent-pool, trajectory, knowledge, insights, playbooks, harness.
- **No DELETE recommended** without the next goal's decision; do not delete working primitives merely because V3 names differ.

## Completion criteria for this goal

All 12 acceptance points from the plan are met: topology inspected; V2/historical evidence recovered; claims reconciled to source; every major subsystem classified; every existing subsystem assigned a disposition; V3 requirements calibrated; read-only baseline executed; failures recorded; duplicate authorities flagged; ordered backlog produced; handoff produced; no production implementation performed.
