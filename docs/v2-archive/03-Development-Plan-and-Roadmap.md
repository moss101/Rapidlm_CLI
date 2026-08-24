# Development Plan and Phased Roadmap

**V2 architecture baseline:** 2026-08-14. Phases 0–9 preserve the V1 foundation; Phases 10–13 are normative V2 delivery phases.

## 1. Delivery strategy

Build vertical slices that prove the invariants early. Do not start with the full TUI or multi-agent UX. The order is: **contracts → durability → policy → single-agent loop → workspace execution → context → TUI → isolation → baseline parallelism/browser/mobile → extensibility/remote → managed-agent mesh → execution/control handoff + Knowledge/Playbooks → full Computer Use V2 → trajectory optimization/Session Insights/long-horizon hardening**.

Each phase has an exit gate. A later phase MUST NOT rely on a mocked invariant from an earlier phase.

## 2. Team topology

Recommended parallel streams after Phase 1:

- Runtime/Kernel
- Context/Retrieval
- Security/Policy/Sandbox
- Agent/LLM/Prompting
- TUI/UX
- Workspace/VCS
- Browser/Desktop/Mobile Computer Use
- Managed Agents/Scheduler
- Handoff/Knowledge/Playbooks
- Trajectory/Insights/Eval
- Eval/Quality
- Extensibility/SDK
- Release/Observability

Agents working in parallel use the task dependencies in `prompts.md`; no stream may change shared contracts without an ADR.

## Phase 0 — Architecture bootstrap (1–2 weeks)

**Deliverables**

- Rust workspace and SDK workspace;
- formatting/lint/test/release CI;
- canonical IDs/errors/config types;
- ADR process;
- schema codegen approach;
- threat-model baseline;
- eval harness skeleton.

**Exit gates**

- `cargo test --workspace` and SDK tests green on macOS/Linux/Windows;
- dependency and license scan configured;
- no circular crate dependencies;
- generated contract compatibility check in CI.

## Phase 1 — Durable kernel and single-agent headless loop (2–4 weeks)

**Deliverables**

- event ledger + SQLite migrations;
- kernel lifecycle/service graph;
- session create/resume/list;
- LLM provider trait + one reference provider + mock provider;
- stable Tool Gateway with read/search/status no side effects;
- JSONL frontend;
- cancellation and crash recovery.

**Exit gates**

- deterministic replay reconstructs identical session projection;
- kill-at-every-event-boundary recovery tests pass;
- JSONL stdout contains only schema-valid records;
- mock model can execute a scripted multi-step turn.

## Phase 2 — Policy, workspace transactions, shell execution (3–5 weeks)

**Deliverables**

- policy engine and project trust;
- capability broker/leases;
- workspace direct view + git-worktree view;
- semantic patch engine;
- process supervisor/background command primitives;
- host-restricted and container sandbox backends;
- approval events/headless policy behavior.

**Exit gates**

- all write/exec/net paths have broker contract tests;
- lease replay/expiry/action mismatch tests pass;
- shell descendant cancellation proven on supported OSes;
- workspace rollback restores byte-identical preimage in test corpus.

## Phase 3 — Context engine and token-efficiency baseline (4–6 weeks)

**Deliverables**

- incremental file inventory and hashing;
- Tree-sitter Tier-1 parsers;
- symbol graph;
- SQLite FTS5;
- optional embedding interface + local/remote providers;
- vector index cache;
- hybrid ranking + MMR;
- read tracking;
- context compiler + token budget inspector;
- multi-repo manifest.

**Exit gates**

- context eval dataset meets precision/recall thresholds;
- incremental indexing has no full-repo scan on single-file changes;
- offline lexical/structural mode passes core coding tasks;
- token regression test shows bounded context construction and no duplicate unchanged chunks.

## Phase 4 — Interactive TUI v1 (3–5 weeks)

**Deliverables**

- transcript/composer/status;
- streaming tool rendering;
- diff viewer/apply/rollback;
- approval modal;
- context/model/memory inspectors;
- session picker/resume/rewind;
- command palette/slash commands;
- accessibility/plain mode.

**Exit gates**

- terminal golden tests at 80/120/200 columns;
- no TUI state diverges from kernel projection in reconnect tests;
- keyboard-only end-to-end task works;
- p95 render loop target met on 10k-event session projection.

## Phase 5 — Goals, subagents, Agents Panel (4–6 weeks)

**Deliverables**

- top-level goal lifecycle + budgets;
- goal DAG and evidence nodes;
- scheduler and subagent spawning;
- isolated worktree views;
- result/evidence merge;
- Agents Panel;
- model router scoring/fallback;
- goal completion verifier.

**Exit gates**

- crash turns active goal into paused;
- subagent cannot update top-level goal directly;
- parallel write agents never share a view;
- budget hard-stop tests pass;
- benchmark demonstrates parallel speedup on decomposable tasks without higher conflict rate than threshold.

## Phase 6 — Strong isolation and security scanners (3–5 weeks)

**Deliverables**

- gVisor backend on Linux;
- network policy proxy/egress enforcement;
- secret handles and injection;
- scanner pipeline and SARIF normalization;
- pre-action command/URL/path scanner;
- post-diff security gate;
- supply-chain trust for project plugins/hooks/MCP.

**Exit gates**

- sandbox escape simulation suite has zero policy bypasses;
- secret exfiltration tests blocked/redacted;
- prompt-injection corpus produces no direct privilege escalation;
- scanner failure cannot be reported as pass.

## Phase 7 — Computer Use and Mobile Simulator (4–7 weeks)

**Deliverables**

- Playwright browser supervisor;
- accessibility-first observe/act/verify;
- trace/screenshot/HAR artifacts;
- desktop accessibility adapters baseline;
- Android emulator pool;
- iOS Simulator backend on macOS;
- remote-mac routing hook;
- browser/mobile eval suites.

**Exit gates**

- browser state isolation verified across sessions;
- action verification catches stale selectors/screens;
- Android snapshot/reset deterministic across eval runs;
- iOS unsupported-host behavior is explicit and non-destructive.

## Phase 8 — MCP/ACP/SDK/plugins/daemon (4–6 weeks)

**Deliverables**

- MCP 2026-07-28 client/server;
- ACP v1/v2 server;
- TypeScript SDK;
- WASM plugin host;
- skills/hooks;
- daemon IPC and background goals/jobs;
- JSONL/API version policy.

**Exit gates**

- compatibility integration tests with at least two MCP clients/servers and two ACP clients;
- malicious plugin cannot exceed manifest capability;
- daemon restart recovery tests pass;
- SDK can reproduce session lifecycle and event stream without TUI.

## Phase 9 — Remote workers, hardening, GA (4–8 weeks)

**Deliverables**

- mTLS worker enrollment and work leases;
- artifact CAS transfer;
- Firecracker reference backend for Linux workers;
- release signing/update mechanism;
- telemetry opt-in/admin policy;
- performance/security hardening;
- migration and disaster-recovery docs.

**GA gates**

- all v1 PRD requirements satisfied or explicitly re-scoped by ADR;
- 30-day internal dogfood with no unrecoverable session corruption;
- security eval pass = 100% for critical escape/exfiltration scenarios;
- task success/token-efficiency metrics beat chosen internal baseline by agreed margin;
- signed binaries and SBOM verified in release pipeline.



## Phase 10 — V2 Managed Agent Mesh (3–5 weeks)

**Goal:** add persistent background AgentPool and Devin-style clean isolated managed workers without destabilizing V1 agent contracts.

Deliverables:

- `AgentPoolService`, typed mailbox and durable bounded background state;
- `TaskEnvelope` and V2 `AgentResult` contracts;
- coordinator decomposition and dependency scheduler extensions;
- per-agent token/cost/time/tool budgets;
- clean-context managed worker spawn and isolated WorkspaceView allocation;
- parent-visible child trajectory summaries;
- Agents Panel engineering-operations view;
- deterministic managed-agent eval suite.

**Exit gates**

- sibling write agents never share writable view;
- malicious child result cannot mutate top-level goal/capabilities;
- persistent Explorer reduces repeated discovery tokens by measured threshold on held-out suite before default enablement;
- crash/restart rebuilds coordinator/worker topology.

## Phase 11 — Handoff, Human Control, Knowledge and Playbooks (4–6 weeks)

Deliverables:

- signed/hashed `HandoffBundle`;
- generation-based `SessionExecutionLease` and local↔remote transfer state machine;
- remote→local return path;
- `ControlLease` for workspace/terminal/desktop/mobile ownership;
- Human takeover/resume UI and mutation reconciliation;
- Knowledge Registry persistence, trigger matcher, governance and Context Engine integration;
- Playbook schema/registry/runner and Automation trigger/cursor integration;
- `/handoff`, `/takeover`, `/knowledge`, `/playbook` command families.

**Exit gates**

- exhaustive handoff fault injection yields max valid write owner count = 1;
- target rejects all source capability leases;
- untrusted repo content cannot self-approve Knowledge;
- scheduled playbook blocks rather than auto-approves unresolved permission requests.

## Phase 12 — Full Computer Use V2 (5–8 weeks)

Deliverables:

- normalized browser/desktop/window/TUI/mobile Surface Registry;
- Linux/Windows/macOS accessibility normalizers;
- semantic target resolver and guarded vision/coordinate fallback;
- desktop session/virtual display lifecycle and GUI sandbox profile;
- full mouse/keyboard/drag/window action set;
- SecretHandle UI injection + recording redaction;
- sensitive UI classifier and human takeover bridge;
- diff-aware UI Test Planner;
- annotated video recording/evidence pipeline;
- Android ADB + visual bridge and iOS/macOS worker bridge;
- TUI Computer Use testing backend;
- Browser/Desktop/Mobile deterministic and safety eval corpora.

**Exit gates**

- >=95% semantic target rate on semantically addressable held-out fixtures;
- stale coordinate actions rejected 100%;
- zero unauthorized sensitive actions and zero secret leaks in release corpus;
- video/assertion evidence binds to Goal criteria;
- MFA/CAPTCHA fixture requires safe human takeover.

## Phase 13 — Trajectory Learning, Session Insights and Long-Horizon GA (4–8 weeks)

Deliverables:

- observable trajectory collector/redactor/data-policy gate;
- deterministic reward vector and candidate ranker;
- experiment registry and matched baseline comparator;
- synthetic task-environment generator isolated from held-out suites;
- Session Insights analyzers and improvement proposal workflow;
- 1h/4h/12h/24h endurance runner;
- >=1,000-tool-call release scenario;
- restart/compaction/handoff/sandbox/provider fault schedules;
- V2 release gate aggregation and migration compatibility.

**Exit gates**

- no hidden chain-of-thought required in trajectory schema;
- privacy-disallowed session cannot export training data;
- 24h endurance preserves all mandatory goal constraints through injected recovery events;
- prompt/router/context improvement cannot promote without held-out security/correctness gate;
- V2 traceability and deterministic replay checks pass.


## 3. Release trains

- **0.1** headless single-agent kernel
- **0.2** safe editing/shell + policy
- **0.3** context engine
- **0.4** TUI
- **0.5** goals/subagents
- **0.6** strong sandbox/security
- **0.7** browser/mobile
- **0.8** extensibility/daemon
- **0.9** remote workers/GA hardening
- **1.0** stable public contracts

## 4. Critical path

`protocol → ledger → kernel → policy/broker → workspace/process → agent loop → context compiler → goal/evidence → scheduler → TUI/ACP/SDK`.

Browser/mobile and remote-worker streams can proceed behind interfaces after their dependent contracts stabilize.

## 5. Risk register

| Risk | Impact | Mitigation |
|---|---|---|
| Context index becomes large/slow | product loses token/latency advantage | rebuildable tiered indexes, measured chunk budgets, incremental hashing |
| Sandbox incompatibility with dev toolchains | user disables safety | backend ladder, compatibility telemetry, per-task tier selection |
| Tool schemas grow with features | token/cache regression | fixed gateway tools + capability broker |
| Multi-agent merge conflicts | parallelism harms throughput | isolated views, dependency-aware scheduler, semantic preconditions |
| Plugin/MCP supply-chain attack | host compromise/data loss | project trust, signed manifests where available, leases, sandboxing |
| Event schema churn | broken sessions/SDK | schema versioning, golden replay fixtures, migration tests |
| Router optimizes cost over quality | task regressions | hard capability constraints, eval-gated policy versions, user pins |
| iOS automation portability | inconsistent support | local macOS-only contract + remote mac worker abstraction |
| Model prompt injection through repo/web | privilege escalation | untrusted-data labeling, policy outside model, no prompt-granted capabilities |


## V2 release train

| Milestone | Scope | Required proof |
|---|---|---|
| V2-alpha | AgentPool + managed workers + internal handoff | deterministic managed-agent/handoff suites |
| V2-beta | Knowledge/Playbooks + full Computer Use | computer-use safety/evidence suite, human takeover |
| V2-RC | trajectory/insights + long-horizon | 12h endurance, held-out evals, migration/replay |
| V2-GA | all V2 requirements | 24h/>=1k tool suite, zero security hard-gate violations, traceability/manifest integrity |
