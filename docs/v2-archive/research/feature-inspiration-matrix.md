# RapidLM V2 Feature Inspiration Matrix

This matrix answers two questions for implementers: **where did the design idea come from, and what is RapidLM's own extension?** Inspiration is architectural, not code copying. Implementation MUST respect source licenses and RapidLM's own contracts.

| RapidLM V2 feature | Primary inspiration | Source-backed pattern | RapidLM extension / decision |
|---|---|---|---|
| Rust-first TUI/kernel | Grok Build | Rust TUI/runtime/tools/workspace separation | One kernel contract also serves daemon/headless/ACP/SDK/remote |
| Structured goal lifecycle | Kimi Code supplied goal design | active/paused/blocked, budgets, crash parks active goal | Goal DAG + evidence gates + attestations |
| Stable model-visible tools | Codex + MCP cache guidance | stable loop/tool prefixes improve cacheability | small gateway tool set routed through Capability Broker |
| Hybrid Context Engine | Augment + Cursor | indexed/semantic multi-repo context | local FTS/Tree-sitter/LSP/graph/vector/MMR/read tracking/token compiler |
| Parallel isolated writes | Qwen Arena + worktree patterns | independent Git worktrees | transactional WorkspaceViews + semantic patch + merge evidence |
| Provenance graph | Atomic | causal change/provenance/attestation | Git-compatible goal→evidence→change→verification graph |
| Persistent background agents | **Muse Code** | specialized async agents persist through session | read-only AgentPool with typed mailboxes and bounded state |
| Replay-exact session log | **Muse Code** | append every model/tool/approval/edit for restart | Event Ledger + side-effect idempotency + digest-based replay |
| Harness learning/data flywheel | **Muse Code** | rejection-sampled harness trajectories; co-training | privacy-governed trajectories for prompt/router/context/specialist-model optimization |
| Long-horizon endurance | **Muse Code** | 1,000+ tool calls / up to 24h case | mandatory 1h/4h/12h/24h resilience suites |
| Planner→critic→executor | Muse `/plan` + `/grill` + `/goal` | explicit planning/stress-testing/goal execution | risk-triggered planner/critic, not forced on trivial tasks |
| Coordinator-managed worker fleet | **Devin Manage Devins** | coordinator delegates isolated VM workers | TaskEnvelope, clean contexts, AgentResult, evidence-aware merge |
| Child trajectory visibility | **Devin** | parent can inspect managed Devin trajectories | summary by default; raw observable trace on demand |
| Per-agent budgets/lifecycle | **Devin** | compute visibility, sleep/terminate child | tokens/$/time/tool budget and priority in scheduler |
| Execution handoff | **Devin `/handoff`** | local context/branch/uncommitted changes → cloud VM | generalized local↔remote HandoffBundle + anti-split-brain execution lease |
| Human takeover | **Devin IDE/Desktop** | pause agent, user edits/runs/tests, resume | explicit ControlLease + mutation reconciliation |
| Knowledge Registry | **Devin Knowledge** | trigger descriptions retrieve scoped small knowledge | owner/evidence/freshness/repo/org governance; distinct from memory |
| Session Insights | **Devin** | analyze completed session and improve prompt | inefficiency/security/context analysis + governed improvement proposals |
| Full desktop Computer Use | **Devin Computer Use** | screenshot→mouse/keyboard→screenshot across GUI apps | accessibility-first semantics, vision fallback, policy pre/postconditions |
| Video testing evidence | **Devin Testing & Recordings** | run application and send recording as proof | annotated video + action/evidence timeline + Goal criterion binding |
| Android visual testing | **Devin Android emulator** | `adb` plus visual desktop interaction | normalized mobile SurfaceRef, clean snapshot pools, remote iOS/mac workers |
| Sandbox/approval separation | Codex + Devin CLI sandbox | OS restriction is distinct from permission decision; fail closed | capability lease validated by executor + host/container/gVisor/microVM tiers |
| Skills/hooks/plugins | Kimi/Grok/Claude/Devin | reusable procedures/extensions | skills never grant privilege; WASM plugin default |
| Provider/model routing | Kimi/Qwen + multi-model ecosystems | provider-neutral model access | measured task-class router, fallback/circuit breaker, quality/cost/latency/privacy policy |

## RapidLM-only synthesis

No single source above provides the following full combination:

```text
Goal/Evidence DAG
    ↓
Coordinator + Persistent AgentPool
    ↓
Clean TaskEnvelope → Isolated Managed Worker
    ↓
Token-budgeted Context + Triggered Knowledge
    ↓
Stable Tool Gateway
    ↓
Capability Broker → CapabilityLease
    ↓
Transactional Workspace + Tiered Sandbox / Computer Surface
    ↓
Verification + Video/Test/Security Evidence
    ↓
Provenance + Attestation
    ↓
Replayable Trajectory → Session Insights → Eval/Learning Flywheel
```

That composition is the intended RapidLM V2 product identity.
