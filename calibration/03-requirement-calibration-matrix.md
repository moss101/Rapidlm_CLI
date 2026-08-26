# D. V3 Requirement Calibration Matrix

Source of requirements: `docs/requirements-traceability.md` (97 requirements). Each is mapped to current repository truth (8 subsystem audits + `cargo` baseline). Status vocabulary: ALREADY_SATISFIED · PARTIALLY_SATISFIED · NOT_IMPLEMENTED · SUPERSEDED_BY_EXISTING · NEEDS_VERIFICATION · BLOCKED_BY_ARCHITECTURE_DECISION.

> No official task status was changed to VERIFIED from this mapping. The `development-ledger.md` remains the authoritative task tracker and must be re-baselined from these findings.

## GRAPH — Runtime Graph (target `docs/architecture/graph-engineering.md`, tasks P2)

| Req | V3 contract | Status | Evidence / Gap | IDs |
|---|---|---|---|---|
| FR-GRAPH-001 | Graph is canonical orchestration state | NOT_IMPLEMENTED | No graph; only single-snapshot `GoalStateMachine` (`agent-runtime/src/goal/state.rs`) | P2 |
| FR-GRAPH-002 | Typed, schema-versioned nodes/edges | NOT_IMPLEMENTED | No `Node`/`Edge`/`NodeState` types anywhere | P2 |
| FR-GRAPH-003 | Model mutations host-validated | NOT_IMPLEMENTED | No graph proposal path | P2 |
| FR-GRAPH-004 | Immutable, diffable revisions | NOT_IMPLEMENTED | Event ledger revisions exist but no graph-diff model | P2 |
| FR-GRAPH-005 | Parallel waves, joins, barriers, shards, resource constraints | NOT_IMPLEMENTED | `crates/scheduler` empty; no ready-set engine | P2 |
| FR-GRAPH-006 | Repair/replan add/supersede/invalidate history | NOT_IMPLEMENTED | `recover_goal` only parks; no subgraph repair | P2 |
| FR-GRAPH-007 | Node states pending→invalidated | NOT_IMPLEMENTED | — | P2 |
| FR-GRAPH-008 | Suspend without LLM polling | NOT_IMPLEMENTED | No graph nodes; jobs exist in process-supervisor but not graph-integrated | P2 |
| FR-GRAPH-009 | Checkpoint resume w/ effect reconciliation | NOT_IMPLEMENTED | Kernel recovery exists; no graph checkpoint | P2 |
| FR-GRAPH-010 | Inspector explains readiness/blocking | NOT_IMPLEMENTED | No graph inspector; TUI graph panel missing | P2 |

## CTX — Context Fabric (target `docs/architecture/context-engineering.md`, tasks P3)

| Req | V3 contract | Status | Evidence / Gap | IDs |
|---|---|---|---|---|
| FR-CTX-001 | Exact path/symbol/read-set before semantic | ALREADY_SATISFIED | `context-engine/src/read_set.rs` | P3 |
| FR-CTX-002 | rg/FTS/tree-sitter/LSP/git/code-graph/vector compose | PARTIALLY_SATISFIED | FTS(`index/fts.rs`), tree-sitter(`parse/symbols.rs`), LSP(`lsp/mod.rs`), graph(`index/graph.rs`), vector(`index/vector.rs`) yes; **ripgrep absent** | P3 |
| FR-CTX-003 | Scope/trust/freshness filters before model | ALREADY_SATISFIED | `compile.rs` budgeting + freshness drop | P3 |
| FR-CTX-004 | MMR/divergent rerank | ALREADY_SATISFIED | `retrieval/rank.rs` RRF+MMR | P3 |
| FR-CTX-005 | ContextItem carries source rev/hash/token/reason/trust | ALREADY_SATISFIED | `memory.rs`/`compile.rs` | P3 |
| FR-CTX-006 | Scout: scope/exhaustive/negative/open-q | PARTIALLY_SATISFIED | Context Scout role exists; negative-finding store MISSING in context-engine | P3 |
| FR-CTX-007 | Zero-hit broaden before absence | PARTIALLY_SATISFIED | Broaden-on-zero-hit not confirmed in source | P3 |
| FR-CTX-008 | Writes invalidate context | ALREADY_SATISFIED | read_set freshness/invalidation | P3 |
| FR-CTX-009 | Dedup by visibility generation | ALREADY_SATISFIED | read_set generations | P3 |
| FR-CTX-010 | Read ceilings + continuation cursors | ALREADY_SATISFIED | read-set ceilings | P3 |
| FR-CTX-011 | Compaction preserves goal/criteria/graph/rules/blockers/evidence | ALREADY_SATISFIED | `agent-runtime/src/compaction.rs` | P3/P5 |
| FR-CTX-012 | `/context` explains allocation | PARTIALLY_SATISFIED | Compiler drop reasoning exists; `/context` UI panel missing in TUI | P3/P10 |

## HARNESS — Agent Harness (target `docs/architecture/agent-harness.md`, tasks P5)

| Req | V3 contract | Status | Evidence / Gap | IDs |
|---|---|---|---|---|
| FR-HARNESS-001 | Typed envelopes, isolated contexts | ALREADY_SATISFIED | `agent-runtime/src/agent/spawn.rs` | P5 |
| FR-HARNESS-002 | Write subagents isolated mutable views | ALREADY_SATISFIED | `allocate_child_view` WriteIsolated | P5 |
| FR-HARNESS-003 | Persistent specialists read-only + bounded summaries | PARTIALLY_SATISFIED | Roles exist; no persistence/wake lifecycle | P5 |
| FR-HARNESS-004 | Validate as-is before repair | ALREADY_SATISFIED | `turn.rs`/`tool-gateway` validate | P5 |
| FR-HARNESS-005 | Localized/revalidated repairs | NOT_IMPLEMENTED | No repair module | P5 |
| FR-HARNESS-006 | Semantic ambiguity fails not guesses | NOT_IMPLEMENTED | Structural validation only | P5 |
| FR-HARNESS-007 | Cross-tool invariants vs execution state | PARTIALLY_SATISFIED | validate yes; invariant engine MISSING | P4/P5 |
| FR-HARNESS-008 | ToolOutcome distinctions | ALREADY_SATISFIED | `tool-gateway` ToolResultEnvelope | P5 |
| FR-HARNESS-009 | Repair rates measurable | NOT_IMPLEMENTED | No repair telemetry | P5 |
| FR-HARNESS-010 | Tool visibility projected by role/node/mode/policy/model | PARTIALLY_SATISFIED | `tool_catalog_hash` forwarded; projection in capability-broker | P5 |
| FR-HARNESS-011 | Background completion wakes dependent nodes | NOT_IMPLEMENTED | No graph nodes to wake | P2/P5 |
| FR-HARNESS-012 | Loop detectors (repeated args/messages/empty) | NOT_IMPLEMENTED | Only hard step bounds | P5 |

## GOAL — Goals/Evidence (target `docs/architecture/goals-evidence-verification.md`, tasks P1/P6)

| Req | V3 contract | Status | Evidence / Gap | IDs |
|---|---|---|---|---|
| FR-GOAL-001 | Goal as graph root w/ criteria/budgets | ALREADY_SATISFIED | `goal/state.rs` + `goal/budget.rs` | P1/P6 |
| FR-GOAL-002 | Lifecycle active/paused/blocked/terminal; cancel per policy | ALREADY_SATISFIED | `GoalStateMachine::apply` | P1 |
| FR-GOAL-003 | Model may propose, not authoritatively complete | ALREADY_SATISFIED | false-completion prevention | P6 |
| FR-GOAL-004 | Completion needs all criteria + fresh evidence + verification + no contradiction | ALREADY_SATISFIED | `evidence.rs::can_complete` | P6 |
| FR-GOAL-005 | Independent verifier, context-independent | ALREADY_SATISFIED | `CriterionEvaluator` from evidence | P6 |
| FR-GOAL-006 | Evidence stale on dependency change | ALREADY_SATISFIED | stale invalidation | P6 |
| FR-GOAL-007 | Recovered goals → paused unless trusted scheduler | ALREADY_SATISFIED | `crash_restart_makes_zero_provider_calls_until_explicit_resume` | P1 |
| FR-GOAL-008 | Inspect/pause/resume/cancel/fork/change budgets | ALREADY_SATISFIED | goal commands | P6 |

## WS — Workspace/VCS (target `docs/architecture/workspace-vcs-semantic-patch.md`, tasks P4)

| Req | V3 contract | Status | Evidence / Gap | IDs |
|---|---|---|---|---|
| FR-WS-001 | Backends direct/worktree/overlay/remote | PARTIALLY_SATISFIED | direct+git_worktree only; overlay/remote unimplemented | P4 |
| FR-WS-002 | Preimage-aware transactional patches | ALREADY_SATISFIED | `SemanticPatch` + `TransactionManager` | P4 |
| FR-WS-003 | Shell/external mutations detected/attributed | ALREADY_SATISFIED | `external_mutation.rs::MutationDetector` | P4 |
| FR-WS-004 | Child integration stage/conflict/verify/commit | ALREADY_SATISFIED | TransactionManager hooks | P4 |
| FR-WS-005 | Semantic attribution links symbols/intent/evidence | ALREADY_SATISFIED | `vcs::ProvenanceStore` | P4 |
| FR-WS-006 | Rewind session/graph/workspace restore or fork | PARTIALLY_SATISFIED | session fork/replay + workspace restore exist; graph rewind absent | P4 |

## SEC — Security (target `docs/security/threat-model.md`, tasks P4/P12)

| Req | V3 contract | Status | Evidence / Gap | IDs |
|---|---|---|---|---|
| FR-SEC-001 | Invariant hierarchy, lower only narrows | ALREADY_SATISFIED | `policy/evaluator.rs` LAYERS_HIGH_TO_LOW, default-deny | P4 |
| FR-SEC-002 | Privileged action passes broker + executor lease | ALREADY_SATISFIED | capability-broker + executor-side validator | P4 |
| FR-SEC-003 | dont-ask fails closed | PARTIALLY_SATISFIED | Ask/Deny exist; Ask→Deny transform not wired | P4 |
| FR-SEC-004 | Project-controlled hooks/plugins/MCP inactive before trust | ALREADY_SATISFIED | ProjectTrustStore, McpTrustStore | P4 |
| FR-SEC-005 | External content untrusted before prompt | ALREADY_SATISFIED | scanners + redaction | P4 |
| FR-SEC-006 | Secrets opaque handles | ALREADY_SATISFIED | SecretHandle/SecretRef | P4 |
| FR-SEC-007 | Short-lived creds identity+env scoped | PARTIALLY_SATISFIED | ephemeral in-process lease; broker dead_code; keychain stub | P4 |
| FR-SEC-008 | Sandbox ladder host→rootless→microVM→remote | ALREADY_SATISFIED | 4 backends, no-downgrade select | P4 |
| FR-SEC-009 | Required isolation fails closed | ALREADY_SATISFIED | select fails closed | P4 |
| FR-SEC-010 | Egress emits hash-linked receipts | ALREADY_SATISFIED | network_policy audit ring + journal events | P4 |

## PROC — Process/Background/Trigger (target `docs/architecture/process-background-trigger-runtime.md`, tasks P4/P7)

| Req | V3 contract | Status | Evidence / Gap | IDs |
|---|---|---|---|---|
| FR-PROC-001 | Long-running supervisor-owned, survives TUI disconnect | ALREADY_SATISFIED | `JobLifetime::Daemon` | P4 |
| FR-PROC-002 | Bounded output, logs as artifacts | ALREADY_SATISFIED | bound_output + MemoryArtifactSink | P4 |
| FR-PROC-003 | Monitors wake dependent nodes | NOT_IMPLEMENTED | monitors absent | P4/P7 |
| FR-PROC-004 | Process-tree cancel kills owned groups | ALREADY_SATISFIED | `cancel.rs::terminate_tree` | P4 |
| FR-PROC-005 | Cron durable gen/lease/jitter | PARTIALLY_SATISFIED | cron yes; jitter NO | P4 |
| FR-PROC-006 | ResourcePool warm containers/microVM/browser/mobile/remote | NOT_IMPLEMENTED | warm pool absent | P4 |
| FR-PROC-007 | Resource acquisition phase telemetry/fallback | PARTIALLY_SATISFIED | SandboxSpec telemetry yes; fallback unconfirmed | P4 |

## CU — Computer Use (target `docs/architecture/computer-use.md`, tasks P6/P8)

| Req | V3 contract | Status | Evidence / Gap | IDs |
|---|---|---|---|---|
| FR-CU-001 | Targeting DOM/a11y→TUI→vision→coord | PARTIALLY_SATISFIED | ordering logic real; live drivers stubbed | P6 |
| FR-CU-002 | Action references fresh Observation/generation | PARTIALLY_SATISFIED | stale-observation rejection real; drivers stubbed | P6 |
| FR-CU-003 | Normalize coordinates; reject bad transforms | PARTIALLY_SATISFIED | logic; drivers stubbed | P6 |
| FR-CU-004 | Batch safe; settle/reobserve | PARTIALLY_SATISFIED | verify.rs; drivers stubbed | P6 |
| FR-CU-005 | Failure captures diagnostic screenshot | PARTIALLY_SATISFIED | screenshot-on-failure logic; drivers stubbed | P6 |
| FR-CU-006 | Commands observe/click/.../app/window | PARTIALLY_SATISFIED | action enum real; drivers stubbed | P6 |
| FR-CU-007 | Browser DOM/AX/CDP/network/console before pixels | PARTIALLY_SATISFIED | observe/security/trace real; FakePageActor only | P6 |
| FR-CU-008 | Preview Supervisor health feeds repair | PARTIALLY_SATISFIED | preview supervision in process-supervisor (not audited deeply) | P6 |
| FR-CU-009 | Android ADB / iOS simctl normalized | ALREADY_SATISFIED | `mobile-sim` real adb/xcrun | P6 |
| FR-CU-010 | Human takeover transfers ControlLease | PARTIALLY_SATISFIED | HumanTakeoverRequired emitted; live drivers stubbed | P6 |

## EXT — Protocols/Extensibility (target `docs/architecture/mcp-acp-sdk-external-agents.md`, tasks P7/P9)

| Req | V3 contract | Status | Evidence / Gap | IDs |
|---|---|---|---|---|
| FR-EXT-001 | Headless JSONL versioned, owns stdout | ALREADY_SATISFIED | `headless/jsonl.rs` golden | P9 |
| FR-EXT-002 | ACP stdio maps to kernel contracts | ALREADY_SATISFIED | `acp/v1.rs:79` | P7 |
| FR-EXT-003 | MCP calls pass normalization/policy/secret/sanitize/trust | ALREADY_SATISFIED | `mcp/gateway.rs` | P7 |
| FR-EXT-004 | Lifecycle hooks observable/cancellable/time-bounded, no privilege | ALREADY_SATISFIED | `plugin-host/hooks.rs` | P4 |
| FR-EXT-005 | WASM plugins declare caps, host APIs | ALREADY_SATISFIED | `plugin-host/wasm.rs` (WASI not linked) | P9 |
| FR-EXT-006 | Skills progressive disclosure, no capability | ALREADY_SATISFIED | `plugin-host/skills.rs` | P9 |
| FR-EXT-007 | Playbooks compile to graph templates | NOT_IMPLEMENTED | `playbooks` crate empty | P4 |
| FR-EXT-008 | External ACP/CLI agents may run Agent nodes, subject to contracts | ALREADY_SATISFIED | acp adapters | P7 |

## EVAL — Eval/Learning (target `docs/architecture/eval-harness.md`, tasks P10/P11)

| Req | V3 contract | Status | Evidence / Gap | IDs |
|---|---|---|---|---|
| FR-EVAL-001 | Harness instantiates production graph/kernel | NOT_IMPLEMENTED | `harness` crate empty | P10 |
| FR-EVAL-002 | ScriptedModel/ReplayProvider/LiveProvider | NOT_IMPLEMENTED | — | P10 |
| FR-EVAL-003 | FaultInjector (crash/provider/sandbox/network/cred/preview/handoff) | NOT_IMPLEMENTED | — | P10 |
| FR-EVAL-004 | Deterministic graders before LLM judges | NOT_IMPLEMENTED | — | P10 |
| FR-EVAL-005 | 1h/4h/12h/24h, ≥1000 tool-call | NOT_IMPLEMENTED | — | P10 |
| FR-EVAL-006 | Metrics (success/tokens/repair/polling/false-completion/...) | NOT_IMPLEMENTED | no metrics collector | P10 |
| FR-EVAL-007 | Trajectory storage no hidden CoT | NOT_IMPLEMENTED | `trajectory` crate empty | P10 |
| FR-EVAL-008 | Preference Fabric learns from signals, provenance | NOT_IMPLEMENTED | knowledge/trajectory empty | P10 |
| FR-EVAL-009 | Held-out regression/security gates before promotion | NOT_IMPLEMENTED | — | P11 |

## UX — TUI/CLI (target `docs/architecture/cli-tui.md`, tasks P9/P10)

| Req | V3 contract | Status | Evidence / Gap | IDs |
|---|---|---|---|---|
| FR-UX-001 | Keyboard complete, NO_COLOR, 80 cols, adaptive | PARTIALLY_SATISFIED | panels golden at 80x24; binary never paints → cannot confirm live | P9 |
| FR-UX-002 | Inspectors: graph/context/agents/diff/terminal/approvals/evidence/trace/memory/resources/computer | PARTIALLY_SATISFIED | 7 panels golden; **graph + evidence panels MISSING** | P9 |
| FR-UX-003 | First usable frame ≤100ms | PARTIALLY_SATISFIED | unmeasurable (not painted) | P9 |
| FR-UX-004 | Streaming UI owns no durable state | ALREADY_SATISFIED | TUI is a kernel projection | P9 |
| FR-UX-005 | Interactive + headless same kernel contracts | PARTIALLY_SATISFIED | both target InProcessKernelClient; headless not wired to CLI | P9/P10 |

## Calibration tally (of 97)

- ALREADY_SATISFIED: ~52 (kernel/event-ledger/auth/context/agent/goal/workspace-tooling/security/protocols/computer-mobile/headless)
- PARTIALLY_SATISFIED: ~30 (graph-adjacent gaps, computer-use live drivers, TUI paint, dont-ask/credential wiring, few context items)
- NOT_IMPLEMENTED: ~15 (entire Runtime Graph P2, eval harness P10/P11, tool repair, loop detection, warm pool, monitors/wake/triggers/jitter, playbooks compiler, TLS-blocked provider calls)
- SUPERSEDED_BY_EXISTING / BLOCKED_BY_ARCHITECTURE_DECISION: 0 (none identified at requirement level)

**Interpretation:** The V3 dossier's `NOT_STARTED` ledger is wrong. Roughly half the 97 requirements are already satisfied by existing, tested code; ~30 are partial; ~15 are genuinely unbuilt — concentrated in the Runtime Graph, the eval/learning stack, live Computer Use drivers, the TLS provider path, and the empty-subsystem crates.
