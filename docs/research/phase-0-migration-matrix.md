# Phase-0 V2→V3 Migration Matrix

**Date:** 2026-08-24  
**Method:** source + tests (`cargo check --workspace`, `cargo test` on kernel/agent-runtime/context-engine/capability-broker/tool-gateway). Not README names.  
**Full calibration:** `calibration/00-executive-summary.md`, `calibration/02-disposition-matrix.md`.

Negative claims (MISSING/STUB) cite search: empty `src/lib.rs` (`#![forbid(unsafe_code)]` only) for `scheduler` (pre-this-change), `handoff`, `agent-pool`, `trajectory`, `knowledge`, `insights`, `playbooks`.

| Crate | Disposition | Evidence class | V3 owner | Notes |
|---|---|---|---|---|
| kernel | KEEP/ADAPT | CODE-VERIFIED + TEST-VERIFIED | GraphService host | add graph authority; daemon uncomposed |
| event-ledger | KEEP/EXTEND | CODE-VERIFIED + TEST-VERIFIED | graph revision events | no second log |
| protocol | KEEP | CODE-VERIFIED + TEST-VERIFIED | wire | GraphId/NodeId added for P2 |
| auth | KEEP/ADAPT | CODE-VERIFIED | secrets | OS keychain stub |
| agent-runtime | KEEP/EXTEND | CODE-VERIFIED + TEST-VERIFIED | AgentNodeExecutor + verified orchestration | KEEP supervisor; do not fork |
| context-engine | KEEP/EXTEND | CODE-VERIFIED + TEST-VERIFIED | Context Fabric | no ripgrep subprocess |
| workspace | KEEP/EXTEND | CODE-VERIFIED + TEST-VERIFIED | views/tx | overlay/remote backends unimplemented |
| vcs | KEEP/EXTEND | CODE-VERIFIED | provenance | in-memory |
| tool-gateway | KEEP/EXTEND | CODE-VERIFIED + TEST-VERIFIED | tools | repair module absent |
| capability-broker | KEEP/EXTEND | CODE-VERIFIED + TEST-VERIFIED | policy/leases | dont-ask transform unwired |
| sandbox | KEEP/EXTEND | CODE-VERIFIED + TEST-VERIFIED | isolation | no warm pool |
| process-supervisor | KEEP/EXTEND | CODE-VERIFIED + TEST-VERIFIED | jobs/cron | no wake-on-event |
| llm-router | ADAPT | CODE-VERIFIED | providers | no TLS to cloud |
| mcp / acp / plugin-host | KEEP | CODE-VERIFIED + TEST-VERIFIED | adapters | WASM interpreter INVESTIGATE |
| tui / apps/rapid | ADAPT | CODE-VERIFIED + TEST-VERIFIED | projection | paint/no-op commands PARTIAL |
| telemetry / sdk | KEEP | CODE-VERIFIED + TEST-VERIFIED | export/SDK | |
| computer-use / mobile-sim | ADAPT/KEEP | CODE-VERIFIED + TEST-VERIFIED (fakes) | CU | Live* drivers fail-closed |
| scheduler | BUILD (was empty stub) | CODE-VERIFIED after P2 | Runtime Graph | this change implements host GraphService |
| harness | ADAPT | CODE-VERIFIED | eval surface | re-exports supervisor + ledger sink |
| handoff, agent-pool, trajectory, knowledge, insights, playbooks | INVESTIGATE/BUILD | CODE-VERIFIED empty | as named | do not DELETE working primitives elsewhere |

**Status update (2026-08-27):** the empty-crate observations above are point-in-time (2026-08-24) and have since been resolved: `handoff`, `trajectory`, `knowledge`, `insights`, and `agent-pool` are implemented and tested (P7–P13 gates VERIFIED), and the still-empty `playbooks` crate was removed from the workspace — the PlaybookCompiler lives in `crates/scheduler/src/playbook.rs`. The matrix text is retained as the phase-0 record.

**REPLACE/DELETE:** none. `agent-pool` remains INVESTIGATE (overlap with `agent-runtime` scheduler).
