# G. Missing / Partial Work Queue (ordered backlog for the NEXT goal)

Ordered by dependency. The next goal should consume `calibration/` (this set) and begin implementation. Do NOT implement during the calibration goal (already respected).

## P0 — Prerequisite (must do before/with P2)
1. **Re-baseline `docs/development-ledger.md`** from source. Current ledger falsely marks everything `NOT_STARTED`. Replace with the source-derived statuses in `03-requirement-calibration-matrix.md` and `02-disposition-matrix.md`.
2. **Resolve duplicate authorities (see `04-architecture-ownership-map.md` §resolution)** — specifically: runtime-graph scheduler owner, handoff owner, attribution owner, memory/knowledge owner, daemon IPC↔auth composition, TUI dispatch↔execute contract, single `CancellationToken`, persona source-of-truth. These block clean P2/P9/P10 work.

## Must preserve / migrate first (do not break)
3. **Preserve working primitives**: kernel, event-ledger, context-engine, capability-broker, sandbox, process-supervisor, tool-gateway, agent-runtime (harness+goals), mcp/acp, plugin-host, tui, telemetry, sdk/typescript. Per `migration-v2-to-v3.md`, all are KEEP/EXTEND — evolve in place; do not rewrite.
4. **Wire (don't rebuild) the daemon**: compose `kernel::ipc::IpcServer` with `auth::local_daemon` token check; currently uncomposed and bypassed by `InProcessKernelClient`.

## Architecture foundation (P2 — the biggest gap)
5. **Runtime Graph**: implement `Node`/`Edge`/`NodeState`, graph revision persistence + immutable diff, host graph mutation service, deterministic ready-set, fan-out/fan-in, joins/barriers, retries, checkpoints/resume, repair/replan, supersession, invalidation engine. (`scheduler` crate is the designated home — build it or fold into `kernel` GraphService; decide per resolution #2.)
6. **Goal DAG / task DAG**: extend `agent-runtime::goal` single-snapshot state into a multi-node graph root with dependency edges.
7. **Planner**: emit `GraphProposal` (nodes/edges) from requirements/intent.
8. **Handoff**: implement `HandoffBundle`/`SessionExecutionLease`/split-brain prevention (currently scattered across kernel/workspace/protocol/event-ledger; `handoff` crate empty).
9. **Fix the failing kernel IPC test**: `stream_disconnect_resumes_by_cursor_and_dedups` (`ipc/client.rs:1559`) — resume/dedup count mismatch.

## Functional implementation (by phase)
10. **Provider TLS (P8, CRITICAL)**: add TLS transport to `llm-router` (no `rustls`/`reqwest` today). Without it, Anthropic/OpenAI adapters cannot complete real calls. Also embed vision image bytes and transmit structured-output schema.
11. **Computer Use live drivers (P6)**: link macOS AX / Win UIA / Linux AT-SPI / CDP-Playwright actors (currently fail-closed due to `#![forbid(unsafe_code)]`; use safe bindings, not `unsafe`, or relax per-module). Real `Live*` hosts required for production.
12. **Tool repair (P4)**: add validate→repair→revalidate + cross-tool invariant engine to `tool-gateway`.
13. **Loop detection + tool-call repair (P5)**: repeated-tool-call / repeated-read / empty-response detectors + repair feedback in `agent-runtime::turn`.
14. **dont-ask + credential broker wiring (P4)**: implement `Ask→Deny` transform; wire `SecretBrokerToken::issue` to a `SecretBroker`; add durable OS-keychain backend (replace `UnimplementedPlatformKeychain`).
15. **Workspace overlay/remote backends (P4)** + textual diff renderer.
16. **VCS durable attestation/signature issuance (P4)**.

## Hardening
17. **Background trigger runtime (P4/P7)**: monitors, wake-on-event, triggers, deterministic jitter, warm ResourcePool in `process-supervisor`/`sandbox`.
18. **Context gaps (P3)**: ripgrep integration (or document FTS-as-substitute), negative-findings store, test-selection mapping; real embedding provider injection.
19. **WASM runtime correctness (P9)**: the hand-rolled interpreter (`plugin-host/wasm.rs`) needs an audit/comparison vs `wasmtime` before trusting guest modules.

## Eval / learning (P10/P11) — entire stack missing
20. **Build the eval harness (`harness` crate)**: ScriptedModel/ReplayProvider/LiveProvider, KernelRunner, FaultInjector, AssertionEngine, MetricCollector, deterministic graders, TrajectoryCollector, ExperimentRegistry.
21. **Build `trajectory` (experiment registry / learning flywheel) and `insights` (Session Insights engine).**
22. **Build `knowledge` (registry/rules/preferences) or formally fold into `context-engine::memory`.**
23. **Build `playbooks` engine (compile-to-graph).**

## Release (P11)
24. **Build/release supply chain**: packaging (cargo-dist/installer), code signing, SBOM, provenance/attestation, release channels, `rapid doctor` CLI, `cargo-audit`/security scanning in CI. Current CI only does fmt/clippy/test/typecheck.
25. **Wire the interactive TUI**: paint panels (add `ratatui` dep), execute dispatched kernel commands, add graph + evidence inspector panels, wire headless JSONL entry (`run --jsonl`), and `ResumeSession`/`CompactSession` commands.
