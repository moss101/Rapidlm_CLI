# H. Calibration Handoff (for the NEXT implementation goal)

**Repository revision:** `c28572d` on `main` (`origin/main`), audited 2026-08-24.
**Audit type:** READ-ONLY calibration. This goal made no source/prompts/policy/sandbox/tool-contract/dependency/workflow changes. All 8 audit subagents used `capability_mode: "read-only"`; the workflow tool was never invoked.
**Working-tree provenance:** Against HEAD the tree is dirty (49 tracked files + ~200 untracked source files). That dirty tree **pre-existed this goal** (listed in the conversation's initial `git_status` before any calibration tool call; the user previously chose to leave it uncommitted). This goal added only untracked `calibration/` artifacts. Do **not** revert the dirty tree — it is the user's in-progress V3 implementation. See `calibration/00-executive-summary.md` for the full provenance record.
**Full evidence set:** `calibration/00..07-*.md` plus `calibration/v2-historical-evidence.md`.

## Verified existing architecture (preserve — KEEP/ADAPT)
- **Kernel + Event Ledger** (`crates/kernel`, `crates/event-ledger`): durable sessions/replay/recovery, transactional SQLite ledger (schema v2), CAS artifact store, capability-broker policy/lease engine — all VERIFIED with tests. `cargo check --workspace` = exit 0.
- **Agent runtime** (`crates/agent-runtime`): turn loop, subagents, isolated views, budgets, cancellation, goal lifecycle, evidence/verifier (fail-closed completion), compaction — VERIFIED.
- **Context engine** (`crates/context-engine`): FTS/BM25, tree-sitter, LSP enricher, code graph, vector/hybrid, MMR, Context Compiler, durable memory — VERIFIED (~148 tests).
- **Policy/Security** (`crates/capability-broker`, `crates/security`): policy engine, broker, leases, executor-side validation, scanners, redaction, network policy, doctor, gate — VERIFIED.
- **Sandbox / Process** (`crates/sandbox`, `crates/process-supervisor`): 4 sandbox backends, jobs/cron/leases/recovery — VERIFIED (gaps: warm pool, monitors, wake-on-event, triggers, jitter).
- **Tools / Protocols / Plugins** (`crates/tool-gateway`, `crates/mcp`, `crates/acp`, `crates/plugin-host`): VERIFIED. WASM is a hand-rolled interpreter (INVESTIGATE correctness).
- **TUI / Telemetry / SDK / Workspace / VCS / Mobile / Auth / Protocol**: VERIFIED (with partial gaps noted).

## Disposition decisions (summary)
- **KEEP/ADAPT** (evolve in place): kernel, event-ledger, protocol, auth, agent-runtime, context-engine, workspace, vcs, tool-gateway, capability-broker, sandbox, process-supervisor, llm-router (ADAPT for TLS), mcp, acp, plugin-host, tui, telemetry, mobile-sim, apps/rapid, sdk/typescript, build/CI.
- **BUILD / INVESTIGATE** (currently empty shells): `scheduler` (runtime-graph scheduler), `handoff`, `agent-pool` (or DELETE/merge), `trajectory`, `knowledge`, `insights`, `playbooks`, `harness`.
- **No REPLACE/DELETE** recommended. Do not delete working primitives merely because a V3 name differs.

## Unresolved questions (decide in next goal)
1. Where does the Runtime Graph scheduler live — `scheduler` crate, `kernel` GraphService, or `agent-runtime`? (Three "scheduler/graph" names coexist.)
2. Who owns handoff logic — implement empty `handoff` crate, or assign to `kernel`/`workspace` and delete it?
3. Memory vs Knowledge — build `knowledge` crate, or fold P5 memory/preference/rule authority into `context-engine::memory`?
4. WASM — keep bespoke interpreter or adopt `wasmtime`? (correctness/portability risk either way)
5. How to link OS Computer-Use frameworks without violating `#![forbid(unsafe_code)]`? (safe-binding crates vs scoped `unsafe`)

## Failing baseline gates (recorded, NOT repaired)
- **`kernel::ipc::client::tests::stream_disconnect_resumes_by_cursor_and_dedups`** FAILS (panic at `crates/kernel/src/ipc/client.rs:1559`, assertion `0 == 1`, resume/dedup count mismatch). All other sampled crate tests pass.
- Cargo check + test-compile: PASS (exit 0).
- `UnimplementedPlatformKeychain` (auth): durable OS-keychain credential storage is a no-op stub → credentials persist only in-memory unless encrypted-file fallback configured.

## Ordered next task IDs (see `06-work-queue.md`)
P0 → re-baseline ledger + resolve duplicate authorities → migrate/preserve working primitives → P2 Runtime Graph (biggest gap) → provider TLS (P8, critical) → Computer Use live drivers (P6) → tool repair/loop-detection (P4/P5) → dont-ask/credential wiring → workspace overlay/remote + VCS attestation → background-trigger hardening → eval/learning stack (P10/P11) → build/release supply chain (P11) → wire interactive TUI.

## Evidence references
- `calibration/00-executive-summary.md` — headline + counts + risks
- `calibration/01-v2-historical-evidence.md` — git/ledger evidence (only 2 commits; V2 validation is doc-consistency only)
- `calibration/02-disposition-matrix.md` — per-component KEEP/ADAPT/BUILD + gaps
- `calibration/03-requirement-calibration-matrix.md` — all 97 requirements → status
- `calibration/04-architecture-ownership-map.md` — current/target authority + 8 duplicate-authority conflicts
- `calibration/05-test-eval-baseline.md` — baseline commands/results
- `calibration/06-work-queue.md` — ordered backlog
- `docs/development-ledger.md` — MUST be re-baselined from above (currently false `NOT_STARTED`)
- `docs/research/migration-v2-to-v3.md` — all V2 concepts KEEP/EXTEND (drives disposition)
