# Product Requirements Document — RapidLM CLI / TUI V3

## 1. Vision

RapidLM is a local-first, graph-native software-engineering execution platform. It must let a developer delegate work from a one-line fix through a multi-hour repository goal while retaining answerability: **what is happening, why, with what context, authority, evidence, cost and recovery path?**

The north-star metric is **verified autonomous task throughput per 1M total tokens**, segmented by task complexity, repository size and model class.

## 2. Product principles

1. Evidence before completion.
2. Host-owned orchestration; model-owned reasoning only within bounded tasks.
3. Token efficiency is a first-class product metric.
4. Context is compiled, provenance-aware and invalidatable—not transcript stuffing.
5. Fail closed for privilege; fail recoverably for work.
6. Parallelism requires isolation and attribution.
7. Every external effect is journaled for recovery.
8. Human control is explicit ownership, not a cosmetic pause button.
9. Small/local models get smaller, clearer tools and more harness assistance.
10. Extensibility never implies privilege escalation.
11. Source/tests/config are implementation truth; docs describe intent.
12. Every major behavior is inspectable through CLI/TUI and machine-readable protocols.

## 3. Personas

- **Interactive engineer:** fast TUI, code understanding, edit/test/diff, reversible actions.
- **Autonomous delegation user:** durable goals, budgets, parallel execution, independent verification.
- **Staff/principal engineer:** multi-repo graph/context, architecture-aware refactors, provenance.
- **CI/platform integrator:** deterministic JSONL, policy-as-code, stable exit semantics, trace bundles.
- **Security administrator:** restrictive org policy, sandbox tiers, credentials, audit and egress controls.
- **Agent/eval engineer:** model routing, scripted/replay/live harness, trajectories and experiment gates.
- **Local-model user:** low schema/context overhead, repairable calls, bounded context and offline indexing.

## 4. Functional requirements

### FR-GRAPH — Graph orchestration
- **FR-GRAPH-001:** Runtime Graph MUST be the canonical orchestration state for non-trivial autonomous runs.
- **FR-GRAPH-002:** graph nodes/edges MUST be typed and schema-versioned.
- **FR-GRAPH-003:** model-proposed graph mutations MUST be host validated.
- **FR-GRAPH-004:** graph revisions MUST be immutable and diffable.
- **FR-GRAPH-005:** scheduler MUST support parallel dependency waves, joins, barriers, shards and resource constraints.
- **FR-GRAPH-006:** repair/replan MUST add/supersede/invalidate graph elements without erasing history.
- **FR-GRAPH-007:** node state MUST support pending, ready, running, waiting, blocked, succeeded, failed, cancelled, superseded and invalidated.
- **FR-GRAPH-008:** process, approval, user-input and external-condition nodes MUST suspend durably without LLM polling.
- **FR-GRAPH-009:** graph checkpoints MUST resume after crash with effect reconciliation.
- **FR-GRAPH-010:** graph inspector MUST explain readiness, blocking, dependencies and invalidation.

### FR-CTX — Context engineering
- **FR-CTX-001:** exact path/symbol/read-set lookup MUST precede expensive semantic retrieval when it can answer the request.
- **FR-CTX-002:** lexical `rg`/FTS, Tree-sitter, LSP, git/manifests/tests, code graph and optional vector retrieval MUST compose behind one Context Fabric.
- **FR-CTX-003:** retrieval MUST apply scope/trust/freshness filters before model exposure.
- **FR-CTX-004:** MMR/equivalent diversity MUST prevent redundant chunks consuming budget.
- **FR-CTX-005:** every `ContextItem` MUST carry source revision/hash, token estimate, reason, retrieval method and trust class.
- **FR-CTX-006:** Context Scout reports MUST include searched scope, exhaustive references where required, checked negative findings and open questions.
- **FR-CTX-007:** zero-hit searches MUST broaden before absence is claimed.
- **FR-CTX-008:** writes MUST invalidate affected context/evidence/test-selection records.
- **FR-CTX-009:** dedup MUST be based on current context visibility generation, not “ever read”.
- **FR-CTX-010:** read tools MUST enforce line, byte, per-line and token ceilings and return continuation cursors.
- **FR-CTX-011:** compaction MUST preserve active goal/criteria, current graph state, rules, unresolved blockers, evidence anchors and recent causal context.
- **FR-CTX-012:** `/context` MUST explain token allocation and why each block is present.

### FR-HARNESS — Agent/tool harness
- **FR-HARNESS-001:** subagents MUST receive typed task envelopes and isolated contexts.
- **FR-HARNESS-002:** write-capable subagents MUST receive isolated mutable workspace views.
- **FR-HARNESS-003:** persistent specialists MUST be read-only by default and maintain bounded summaries/read sets.
- **FR-HARNESS-004:** tool calls MUST validate as-is before any repair.
- **FR-HARNESS-005:** repairs MUST be localized to validator issue paths, bounded, recorded and revalidated.
- **FR-HARNESS-006:** semantic ambiguity MUST fail instead of being guessed.
- **FR-HARNESS-007:** cross-tool invariants MUST be validated against execution state (e.g. read revision before write).
- **FR-HARNESS-008:** ToolOutcome MUST distinguish success, recovered, partial, retryable, denied and failed.
- **FR-HARNESS-009:** model/tool repair rates MUST be measurable by model family and schema version.
- **FR-HARNESS-010:** tool visibility MUST be projected by role, graph node, mode, policy and model capability.
- **FR-HARNESS-011:** background task completion MUST wake dependent graph nodes rather than require model polling.
- **FR-HARNESS-012:** agent loop detectors MUST stop repeated tool arguments, repeated messages and empty-response loops within budgets.

### FR-GOAL — Goals/evidence/verification
- **FR-GOAL-001:** Goal MUST be a graph root with explicit end state, criteria, boundaries, proof requirements and optional budgets.
- **FR-GOAL-002:** lifecycle MUST support active, paused, blocked and terminal completion; cancellation clears/terminates according to run policy.
- **FR-GOAL-003:** model may propose completion but MUST NOT authoritatively complete a proof-required goal.
- **FR-GOAL-004:** completion MUST require all mandatory criteria satisfied, fresh required evidence, passing verification edges and no blocking contradiction.
- **FR-GOAL-005:** qualifying goals MUST run an independent completion verifier with a context independent enough to avoid circular self-confirmation.
- **FR-GOAL-006:** evidence MUST track source revision and become stale when dependencies change.
- **FR-GOAL-007:** active goals recovered after process restart MUST become paused unless a trusted scheduler initiates a new run.
- **FR-GOAL-008:** user MUST be able to inspect, pause, resume, cancel, fork and change explicit budgets.

### FR-WS — Workspace/VCS
- **FR-WS-001:** WorkspaceView backends MUST include direct, git-worktree, overlay and remote/content-addressed views where supported.
- **FR-WS-002:** first-party edits MUST use preimage-aware transactional patches.
- **FR-WS-003:** shell/external mutations MUST be detected and attributed or explicitly marked external/unattributed.
- **FR-WS-004:** child integration MUST stage, conflict-check, verify and then commit/apply.
- **FR-WS-005:** semantic change attribution MUST link symbols/intent/evidence to textual patches without sacrificing normal Git interoperability.
- **FR-WS-006:** session rewind MUST support conversation/graph/workspace restoration or safe fork from a checkpoint without rewriting Git history.

### FR-SEC — Policy/security/sandbox
- **FR-SEC-001:** compiled invariants > organization > user > trusted project > session approvals > agent request; lower layers may only narrow.
- **FR-SEC-002:** every privileged action MUST pass Capability Broker and executor-side lease validation.
- **FR-SEC-003:** automation MUST support a noninteractive `dont-ask` mode that fails rather than hangs.
- **FR-SEC-004:** project-controlled hooks/plugins/MCP/executable config MUST remain inactive before project trust.
- **FR-SEC-005:** all web/MCP/process/external-agent/browser content MUST be labeled untrusted before prompt inclusion.
- **FR-SEC-006:** secrets SHOULD be represented by opaque handles and resolved only at executor boundary.
- **FR-SEC-007:** short-lived provider/cloud credentials MUST be identity+environment scoped; caches MUST include environment identity.
- **FR-SEC-008:** sandbox ladder MUST support host-restricted, rootless/container, stronger kernel sandbox/microVM and remote dedicated worker modes where platform permits.
- **FR-SEC-009:** required isolation MUST fail closed; no silent downgrade.
- **FR-SEC-010:** external effect attempts SHOULD emit hash-linked egress/operation receipts for audit.

### FR-PROC — Processes/jobs/resources
- **FR-PROC-001:** long-running commands MUST be supervisor-owned and survive TUI disconnection in daemon mode.
- **FR-PROC-002:** output MUST be bounded in model context with complete logs stored as artifacts.
- **FR-PROC-003:** monitors MUST support event predicates and wake dependent graph nodes.
- **FR-PROC-004:** process-tree cancellation MUST kill owned groups/trees without touching unrelated user processes.
- **FR-PROC-005:** cron/background jobs MUST use durable generation/lease/idempotency semantics and deterministic jitter.
- **FR-PROC-006:** Resource Pool MUST support acquire/release of warm containers, microVMs, browser desktops, mobile simulators and remote workers.
- **FR-PROC-007:** resource acquisition MUST expose phase telemetry and fallback provisioning when allowed.

### FR-CU — Computer/browser/mobile/preview
- **FR-CU-001:** targeting order SHOULD be DOM/test-id → accessibility/native → TUI semantic → bounded vision → coordinate fallback.
- **FR-CU-002:** every action MUST reference a fresh `Observation`/surface generation and optional expected postcondition.
- **FR-CU-003:** executor MUST normalize coordinates between model/API and physical/display frames and reject incompatible aspect transforms.
- **FR-CU-004:** actions MAY batch when safe; state-changing actions MUST settle/reobserve before dependent input.
- **FR-CU-005:** failure MUST capture bounded diagnostic screenshot/state automatically when policy permits.
- **FR-CU-006:** Computer Use commands MUST include observe, click, double-click, move, down/up, drag, scroll, type, key/chord, wait, screenshot, cursor and app/window operations.
- **FR-CU-007:** Browser MUST provide structured DOM/AX/CDP/network/console paths before raw pixels.
- **FR-CU-008:** Preview Supervisor MUST track build/dev-server/HTTP/console/network/HMR health and feed failures into graph repair.
- **FR-CU-009:** Android/ADB and iOS/simctl on eligible macOS workers MUST use normalized mobile contracts.
- **FR-CU-010:** human takeover MUST transfer `ControlLease`, pause conflicting agent input, reconcile mutations and reobserve before resume.

### FR-EXT — Protocols/extensions
- **FR-EXT-001:** headless JSONL MUST be versioned and own stdout strictly.
- **FR-EXT-002:** ACP stdio adapter MUST map session/diff/permission/progress to kernel contracts without bypass.
- **FR-EXT-003:** MCP client/server calls MUST pass schema normalization, policy, secret handling, output sanitization and trust labeling.
- **FR-EXT-004:** lifecycle hooks MUST be observable, cancellable/time-bounded and incapable of granting privilege.
- **FR-EXT-005:** WASM component plugins MUST declare capabilities and use host APIs rather than ambient OS access by default.
- **FR-EXT-006:** skills MUST be progressively disclosed and never grant capability.
- **FR-EXT-007:** playbooks MUST compile to initial graph templates; runtime may extend them.
- **FR-EXT-008:** external ACP/CLI agent adapters MAY execute Agent nodes but remain subject to RapidLM graph, workspace, policy and evidence contracts.

### FR-EVAL — Evaluation/learning
- **FR-EVAL-001:** Eval Harness MUST instantiate production graph/kernel contracts.
- **FR-EVAL-002:** suites MUST support ScriptedModel, ReplayProvider and LiveProvider.
- **FR-EVAL-003:** FaultInjector MUST cover process crash, provider error/throttle, sandbox/worker loss, persistence fault, network partition, stale credentials, preview failure and handoff interruption.
- **FR-EVAL-004:** deterministic graders MUST precede optional LLM judges when possible.
- **FR-EVAL-005:** long-horizon release suites MUST cover 1h/4h/12h/24h and at least one >=1,000-tool-call scenario.
- **FR-EVAL-006:** metrics MUST include verified success, tokens/success, repeated-read ratio, context duplication, tool repair rate, polling avoided, false-completion rate, policy violations, graph repair success and recovery success.
- **FR-EVAL-007:** observable trajectory storage MUST never require hidden chain-of-thought.
- **FR-EVAL-008:** Preference Fabric MAY learn from explicit accept/reject/edit signals but MUST preserve provenance/confidence/scope/decay and never override policy or explicit instructions.
- **FR-EVAL-009:** candidate prompt/router/context/harness changes MUST pass held-out regression/security gates before promotion.

### FR-UX — CLI/TUI
- **FR-UX-001:** TUI MUST remain keyboard complete, `NO_COLOR` compatible and usable at 80 columns with adaptive panels.
- **FR-UX-002:** core inspectors MUST include graph, context, agents, diff, terminal/process, approvals, evidence, trace, memory/knowledge, resources and computer preview.
- **FR-UX-003:** first usable frame SHOULD target <=100ms on reference warmed local hardware; heavyweight indexing MUST not block it.
- **FR-UX-004:** streaming UI MUST never own durable business state.
- **FR-UX-005:** interactive and headless modes MUST use the same kernel contracts.

## 5. Acceptance scenarios

1. **Repository migration:** map every caller of an auth interface, split safe work across isolated agents, integrate transactionally, run repository-native tests and verify every criterion.
2. **Untrusted repo:** malicious README/MCP/hook content remains data; no project code executes until trust; denied operations cannot be rerouted around policy.
3. **Open-model task:** malformed optional/null/array tool arguments are safely repaired only where validator evidence permits and the task completes without corrupting valid inputs.
4. **Crash during external effect:** operation journal resolves committed/uncertain state, never double-sends/republishes silently.
5. **Long background test:** process runs under daemon supervisor; TUI disconnect/reconnect works; monitor wakes verifier without polling turns.
6. **Visual bug:** preview starts, browser structured observation reproduces bug, patch is applied, deterministic test + visual evidence both pass.
7. **Human MFA takeover:** input control transfers to human, agent cannot type concurrently, surface is reobserved after return.
8. **Remote handoff:** exactly one session writer generation remains valid; secrets/capability leases are reissued, not transferred.
9. **Context staleness:** a write invalidates old file evidence and context; verifier cannot accept proof from previous content hash.
10. **Rewind/fork:** user forks from a prior graph/workspace checkpoint without destructive history rewrite.

## 6. Release criteria

- zero known policy/sandbox bypasses in release security suite;
- all mandatory PRD requirements mapped to design + task + verification;
- deterministic replay and graph resume pass crash/recovery suites;
- no unresolved schema compatibility blocker;
- no task marked VERIFIED without named current evidence;
- cross-platform packaging verified on supported targets;
- performance/token-efficiency baselines captured and regression thresholds enforced.
