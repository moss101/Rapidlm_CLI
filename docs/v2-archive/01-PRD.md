# Product Requirements Document — RapidLM CLI / TUI

**Architecture baseline:** V2.0 (2026-08-14)  
**V2 focus:** managed agents, execution handoff, Knowledge/Playbooks, trajectory learning, Session Insights, and full Computer Use.

## 1. Executive summary

RapidLM is a local-first agentic software-development platform exposed through a terminal UI, headless CLI, ACP, MCP, daemon API, and SDK. It coordinates one or more coding agents while preserving context efficiency, security boundaries, reproducibility, provenance, and user control.

The product succeeds if it lets an engineer delegate a repository-scale goal and answer, at any point:

- What is the agent trying to accomplish?
- What evidence has it gathered?
- Which files and commands did it touch?
- Why did it choose this model/tool?
- What is running in the background?
- What permissions were granted and for how long?
- How much context/token/cost budget remains?
- Can I safely pause, resume, fork, rewind, review, or apply the work?

## 2. Product principles

1. **Evidence before completion.** “Done” is a structured state supported by verification evidence.
2. **Least privilege by construction.** Permission checks happen before capabilities are materialized.
3. **Local-first, remote-capable.** Core workflows must work offline except provider calls and explicitly remote tools.
4. **Stable contracts over clever prompts.** Tool, event, API, and persistence contracts are versioned.
5. **Token efficiency is a product metric.** Measure useful task progress per input/output token, not just benchmark success.
6. **UI is a projection, not a control-plane implementation.** TUI/headless/ACP use the same kernel.
7. **No invisible autonomy.** Background or goal-driven work is visible, budgeted, interruptible, and recoverable.
8. **Git interoperability.** RapidLM may augment version control semantics but must work with ordinary Git repositories.
9. **Fail closed for privilege, fail recoverably for work.** Security uncertainty denies/asks; runtime failures park resumable work.
10. **Agent-friendly implementation.** Module boundaries, tests, and prompts are designed for parallel coding agents.
11. **Clean-context delegation.** Managed workers receive the minimum TaskEnvelope needed to succeed; parent chat history is not cloned by default.
12. **Human control is explicit ownership.** Taking over an agent transfers control of mutable surfaces rather than merely hiding UI output.
13. **Learning is governed.** Observable trajectories may improve the harness, prompts and specialist components only under explicit data policy; hidden chain-of-thought is never required.
14. **Visual proof is evidence.** Browser/desktop/mobile recordings and assertions are first-class evidence, not decorative artifacts.

## 3. Personas and jobs-to-be-done

### P1 — Interactive engineer

**Job:** “Understand and modify this codebase without constantly hand-feeding context or reviewing unsafe shell operations.”

Needs fast startup, trustworthy search/read/edit, clear diffs, lightweight approvals, context visibility, and reversible changes.

### P2 — Autonomous delegation user

**Job:** “Take this well-defined goal, work for 30–90 minutes, parallelize where safe, and tell me when evidence says it is finished or genuinely blocked.”

Needs goal lifecycle, budgets, subagents, background jobs, reliable resumption, evidence, and completion criteria.

### P3 — CI/platform integrator

**Job:** “Run the same agent behavior non-interactively with deterministic machine-readable output.”

Needs headless JSONL, stable exit codes, config-as-code, secrets isolation, policy-as-code, replay, and trace IDs.

### P4 — Security/enterprise administrator

**Job:** “Allow agents without giving repository content or infrastructure credentials unlimited ambient authority.”

Needs policy layers, organization restrictions, credential vaults, network allowlists, sandbox tiers, audit trails, and scanner gates.

### P5 — Agent/framework developer

**Job:** “Embed or extend RapidLM without forking the runtime.”

Needs SDK, MCP/ACP, WASM plugins, skills, hooks, documented contracts, and test harnesses.

## 4. Scope

### 4.1 v1 in scope

- terminal TUI and headless CLI;
- multi-provider LLM router;
- repository read/search/edit/shell tools;
- context indexing with Tree-sitter, lexical search, optional embeddings, LSP enrichment;
- multi-repo workspace manifests;
- durable sessions and event ledger;
- goals, budgets, evidence, pause/resume/block/complete;
- subagents with isolated workspace views and concurrency limits;
- capability broker, policy engine, approvals, audit records;
- host and sandboxed process execution;
- browser computer use;
- desktop computer-use abstraction with OS adapters;
- Android emulator and macOS iOS Simulator orchestration;
- security scanning pipeline;
- MCP client/server, ACP server, TypeScript SDK;
- skills, hooks, WASM plugin host;
- local daemon, background jobs, cron-like scheduled jobs while daemon runs;
- observability and evaluation harness;
- signed cross-platform binary releases.

### 4.2 V2 in scope

- persistent session-long read-only background AgentPool;
- coordinator-managed clean-context worker fleet with per-agent budgets;
- provider-neutral local↔remote execution handoff with anti-split-brain ownership;
- human/agent takeover and resume for workspace, terminal, desktop and mobile control;
- trigger-scoped Knowledge Registry distinct from memory/rules/skills/policy;
- versioned Playbooks and scheduled/event-triggered Automations;
- Session Insights with evidence-linked improvement proposals;
- training-ready observable trajectories and harness optimization/data flywheel;
- 1h/4h/12h/24h endurance suites including >=1,000-tool-call cases;
- full Computer Use across browser, desktop, native/Electron apps, TUI surfaces and emulator windows;
- annotated video/screenshot/DOM/accessibility evidence tied to Goal criteria;
- diff-aware UI test planning, secure SecretHandle entry and human takeover for MFA/CAPTCHA.

### 4.3 Post-V2 / staged

- hosted control plane and fleet manager;
- always-on cloud goal execution;
- organization SSO/SCIM management UI;
- graph-native VCS replacing Git internals;
- collaborative real-time multi-user sessions;
- Windows desktop automation beyond UI Automation baseline;
- physical mobile-device farms.

### 4.4 Explicit non-goals

- replacing full graphical IDEs;
- inventing a new programming-language parser instead of using Tree-sitter/LSP;
- granting plugins unrestricted in-process native execution;
- silently uploading full repositories for indexing;
- automatically enabling outbound network access in sandboxes;
- making completion claims solely from model natural language.

## 5. Functional requirements

### FR-CLI — CLI/TUI

- **FR-CLI-001** `rapid` with no subcommand MUST start the interactive TUI in the detected project.
- **FR-CLI-002** startup to first interactive frame SHOULD be <150 ms p50 on a warm local install, excluding migration/index rebuild.
- **FR-CLI-003** the TUI MUST expose transcript, composer, status line, goal/progress, agent panel, diff review, approvals, context inspector, model selector, memory inspector, trace inspector, and background jobs.
- **FR-CLI-004** `rapid run --jsonl` MUST emit newline-delimited versioned events and never mix human prose on stdout; diagnostics go to stderr.
- **FR-CLI-005** TUI, headless, ACP, and SDK MUST operate on the same session model.
- **FR-CLI-006** user MUST be able to interrupt current model/tool work; interruption parks active goals as paused.
- **FR-CLI-007** `/rewind` MUST restore workspace transaction state and session projection to a selected checkpoint when safe.
- **FR-CLI-008** all destructive apply/rollback operations MUST preview affected paths and conflicts.

### FR-CTX — Context Engine

- **FR-CTX-001** index source files incrementally using content hashes and file watcher events.
- **FR-CTX-002** support Tree-sitter symbol extraction for Tier-1 languages: Rust, TypeScript/JavaScript, Python, Go, Java, C/C++, C#, Kotlin, Swift, Ruby, Bash, JSON/YAML/TOML/Markdown.
- **FR-CTX-003** support LSP enrichment when a configured server is available; failure MUST degrade to syntax/lexical retrieval.
- **FR-CTX-004** store FTS, symbol graph, chunk metadata, repository identity, and freshness locally.
- **FR-CTX-005** embeddings MUST be optional. Offline mode MUST remain functional with lexical + structural retrieval.
- **FR-CTX-006** hybrid retrieval MUST combine lexical, semantic, symbol/graph, explicit references, current diff, read-set, and recency signals.
- **FR-CTX-007** MMR or equivalent diversity ranking MUST prevent redundant chunks from consuming the context budget.
- **FR-CTX-008** track exact read ranges and content hashes per session so unchanged content is not needlessly resent.
- **FR-CTX-009** context compiler MUST assign every item a token estimate, source, freshness, and reason-for-inclusion.
- **FR-CTX-010** user can inspect and pin/unpin context items.
- **FR-CTX-011** workspace manifest can attach multiple repositories with aliases and read/write policy.
- **FR-CTX-012** memory writes MUST include scope, source/provenance, confidence, and optional expiry.

### FR-AGT — Agent runtime and Agents Panel

- **FR-AGT-001** each agent has immutable ID, parent ID, task scope, workspace view, model policy, budget, and state.
- **FR-AGT-002** main agent MAY spawn subagents only within scheduler limits and policy.
- **FR-AGT-003** subagents MUST NOT mutate the main goal lifecycle or organization policy.
- **FR-AGT-004** parallel write-capable agents MUST use separate workspace views.
- **FR-AGT-005** Agents Panel MUST display state, elapsed time, tokens/cost, current action, workspace view, diff size, blockers, and last evidence.
- **FR-AGT-006** scheduler MUST support cancellation, pause, priority, max concurrency, per-provider concurrency, and global cost budget.
- **FR-AGT-007** agent outputs are merged through explicit result/evidence contracts; no hidden shared chat context.

### FR-GOAL — Goal and evidence runtime

- **FR-GOAL-001** a main session has at most one actively pursued top-level goal; goal DAG may contain child goals/tasks.
- **FR-GOAL-002** goal states: `active`, `paused`, `blocked`; `complete` is terminal event then active snapshot is cleared/archive-addressable.
- **FR-GOAL-003** cancel clears the active goal; cancellation is an event, not a resumable state.
- **FR-GOAL-004** active goal after process crash/restart MUST restore as `paused`, never auto-run.
- **FR-GOAL-005** budgets MAY include turns, tokens, wall-clock active time, and cost; unspecified budgets remain unset.
- **FR-GOAL-006** budget exhaustion results in `blocked(reason=budget_exhausted)`.
- **FR-GOAL-007** completion criteria MUST map to evidence requirements. Missing required evidence prevents completion.
- **FR-GOAL-008** each change MAY be linked to goal/evidence nodes; applied changes MUST have attribution.

### FR-KRN — Kernel/process/session

- **FR-KRN-001** kernel owns session lifecycle, service graph, cancellation, event ordering, process supervision, and crash recovery.
- **FR-KRN-002** every state mutation MUST emit a typed event with monotonically increasing session sequence.
- **FR-KRN-003** ledger writes and critical state transitions MUST be transactionally durable before acknowledging success.
- **FR-KRN-004** embedded and daemon transports share the same service traits.
- **FR-KRN-005** background jobs survive TUI disconnect when daemon mode is enabled.
- **FR-KRN-006** orphaned child processes MUST be terminated or re-adopted according to job policy after crash.

### FR-POL — Capability broker/policy

- **FR-POL-001** all side-effecting tools MUST request capabilities through the broker.
- **FR-POL-002** policy decision is one of `allow`, `ask`, `deny`; deny is final at a higher-trust layer.
- **FR-POL-003** approvals issue scoped leases with action fingerprint, resource constraints, expiry, and max uses.
- **FR-POL-004** project files may restrict but MUST NOT broaden privileges beyond user/org policy.
- **FR-POL-005** secrets are referenced by opaque handles; model-visible transcripts MUST NOT contain secret values unless user explicitly asks and policy allows.
- **FR-POL-006** network is default-deny in isolated sandboxes; per-domain/port leases are supported.
- **FR-POL-007** approval UI MUST display normalized action, reason, risk class, requested scope, and narrower alternatives.

### FR-SBX — Sandbox

- **FR-SBX-001** execution backends: `host-restricted`, `container`, `gvisor` where supported, `remote-worker`; optional Firecracker backend for hosted Linux.
- **FR-SBX-002** filesystem mounts, network, environment, CPU, memory, pids, timeout, and output limits are explicit in `SandboxSpec`.
- **FR-SBX-003** secrets enter sandbox through broker-managed ephemeral injection and are not persisted in images/snapshots.
- **FR-SBX-004** sandbox MUST expose structured exit reason and resource usage.
- **FR-SBX-005** tool stdout/stderr are capped and spool to artifact storage with digest; model receives bounded excerpts.

### FR-LLM — LLM Router

- **FR-LLM-001** providers implement a common streaming request contract and normalized usage/error schema.
- **FR-LLM-002** route constraints include model capability, context size, tool support, data policy, region, budget, latency SLO, and user pin.
- **FR-LLM-003** fallback MUST NOT violate user/provider policy or silently switch to a materially weaker capability class for a task requiring it.
- **FR-LLM-004** router records route decision features without recording hidden reasoning.
- **FR-LLM-005** model-visible static prefix and tool schema SHOULD remain stable across turns to preserve cacheability.
- **FR-LLM-006** per-model prompt adapters are versioned and eval-gated.

### FR-EVAL — Eval and Agent Harness

- **FR-EVAL-001** harness can run deterministic tool doubles, real sandbox scenarios, model-live scenarios, and trace replay.
- **FR-EVAL-002** scenario spec defines setup, prompt/goal, permissions, fixtures, expected evidence, expected diff properties, and metrics.
- **FR-EVAL-003** every critical module has contract tests and failure-injection tests.
- **FR-EVAL-004** release gates include task success, security violations, token efficiency, cost, latency, flake rate, and regression delta.
- **FR-EVAL-005** prompt/router changes require matched baseline evaluation before merge.

### FR-CU — Computer Use

- **FR-CU-001** browser adapter uses structured DOM/accessibility observations first and screenshot vision as fallback.
- **FR-CU-002** browser sessions are isolated and ephemeral by default; persistent profiles require explicit selection.
- **FR-CU-003** action contract is `observe -> act -> verify` with evidence per action.
- **FR-CU-004** downloads/uploads, clipboard, password fields, payment/auth flows, and external side effects have elevated risk classes.
- **FR-CU-005** browser traces can include screenshots, DOM snapshots, network metadata, and Playwright traces with redaction.
- **FR-CU-006** desktop adapter uses native accessibility APIs before coordinate-based fallback.

### FR-MOB — Mobile simulator

- **FR-MOB-001** Android backend manages AVD discovery/boot/snapshot/reset/install/launch/adb/logcat/screenshot/recording.
- **FR-MOB-002** iOS backend is macOS-only locally and uses `simctl`; non-mac hosts may route to a remote macOS worker.
- **FR-MOB-003** simulator actions normalize into the same observe/act/verify model used by Computer Use.
- **FR-MOB-004** simulator pools must be reset to known state between eval runs.
- **FR-MOB-005** app binaries and captured artifacts receive content digests and provenance.

### FR-SEC — Security scanning

- **FR-SEC-001** normalize findings into SARIF-compatible model with severity, confidence, location, rule, evidence, remediation, and source scanner.
- **FR-SEC-002** provide native secret/policy/dangerous-command checks and adapters for external SAST/SCA scanners.
- **FR-SEC-003** pre-action scanner can block/ask before dangerous execution; post-change scanner gates apply/completion.
- **FR-SEC-004** scanner failure is visible and does not count as a clean scan.

### FR-EXT — MCP/ACP/plugins/skills/hooks/SDK

- **FR-EXT-001** MCP support targets specification `2026-07-28` with compatibility shim for prior common revision when feasible.
- **FR-EXT-002** ACP server supports v1 and v2 negotiation while v2 adoption stabilizes.
- **FR-EXT-003** third-party plugins run as WASM/WASI components by default and declare capabilities.
- **FR-EXT-004** native hooks execute out of process, are time bounded, receive redacted event payloads, and cannot grant permissions.
- **FR-EXT-005** skills are instruction/resources bundles with explicit activation metadata; skills do not grant tools.
- **FR-EXT-006** TypeScript SDK mirrors stable kernel session/event/tool abstractions without exposing internal crate types.


## 5A. V2 functional requirements

### FR-POOL — Persistent background agents and managed workers

- **FR-POOL-001** the runtime MUST distinguish persistent background agents from bounded managed workers; each class has an explicit lifecycle and default capability profile.
- **FR-POOL-002** persistent background agents MUST be read-only by default and MUST NOT mutate the top-level goal, policy, approvals, or capability leases.
- **FR-POOL-003** managed workers MUST be created from a typed `TaskEnvelope` containing objective, criteria, selected context/knowledge, workspace access, capability ceiling, model policy, budget, and expected result schema; cloning the entire parent transcript is forbidden by default.
- **FR-POOL-004** sibling write-capable managed workers MUST receive separate writable WorkspaceViews and sandbox/process ownership.
- **FR-POOL-005** a coordinator MUST be able to inspect child observable trajectory summaries, evidence, usage and blockers without requiring hidden chain-of-thought.
- **FR-POOL-006** scheduler MUST enforce per-agent token/cost/time/tool budgets, global budget, provider concurrency, worker capacity, task dependencies and priority.
- **FR-POOL-007** persistent agents MUST communicate through bounded typed mailboxes with evidence/artifact references; mailbox content is untrusted data.
- **FR-POOL-008** default persistent-agent roles MUST be eval-gated and MAY be parked when measured benefit does not justify ongoing cost.

### FR-HOF — Execution handoff

- **FR-HOF-001** RapidLM MUST support moving session execution between compatible local/daemon/remote workers through a versioned `HandoffBundle`.
- **FR-HOF-002** the handoff bundle MUST include ledger checkpoint/tail references, goal/task/agent state, repository revisions, uncommitted ChangeSets, context/read state, Knowledge refs, artifact manifest and required capability intents.
- **FR-HOF-003** capability leases and plaintext secret values MUST NOT be transferred; target policy MUST re-evaluate intents and issue target-local leases.
- **FR-HOF-004** a generation-based `SessionExecutionLease` MUST guarantee at most one write-capable execution owner across source and target.
- **FR-HOF-005** source runtime MUST quiesce new write/tool dispatch before target ownership can commit; handoff failure MUST leave a recoverable source or a clearly blocked session, never two writers.
- **FR-HOF-006** target MUST verify bundle signature/digest/expiry, worker compatibility, repository preimages and artifact hashes before ownership transfer.
- **FR-HOF-007** Computer Use surfaces are recreated on the target; target MUST obtain a fresh Observation before any UI action.

### FR-HUM — Human/agent control handoff

- **FR-HUM-001** the kernel MUST represent interactive mutable ownership using a generation-based `ControlLease` scoped to workspace write, terminal input, desktop input and mobile input domains.
- **FR-HUM-002** human takeover MUST pause conflicting agent write/input actions before the human becomes the owner.
- **FR-HUM-003** human edits/commands/actions MUST remain visible in the same Event Ledger with `actor=human` or `actor=external_human` attribution.
- **FR-HUM-004** agent resumption after human takeover MUST detect workspace mutations and MUST re-observe browser/desktop/mobile state.
- **FR-HUM-005** CAPTCHA and MFA flows MUST support human takeover and MUST NOT be bypassed or solved by an agent in violation of the challenge/security mechanism.
- **FR-HUM-006** control ownership MUST NOT confer security capability; both human-mediated automated actions and agent actions remain subject to applicable platform/policy controls.

### FR-KNW — Knowledge Registry

- **FR-KNW-001** Knowledge MUST be modeled separately from episodic Memory, Rules, Skills, Playbooks and security Policy.
- **FR-KNW-002** a Knowledge item MUST include scope, triggers, body, owner, status, provenance/evidence, freshness/review metadata and optional supersession.
- **FR-KNW-003** Context Engine MUST retrieve Knowledge only after hard scope/status/data-policy filters and MUST provide trigger reasons and independent token budget accounting.
- **FR-KNW-004** untrusted repository/web/tool content MUST NOT directly create approved shared Knowledge; it may at most create a candidate requiring governance.
- **FR-KNW-005** Session Insights and explicit user corrections MAY propose Knowledge candidates, but shared/org publication MUST require configured owner/human approval.
- **FR-KNW-006** secret plaintext MUST NOT be stored in Knowledge; use SecretHandle references where a secret dependency must be documented.
- **FR-KNW-007** deprecated/stale/conflicting Knowledge MUST be surfaced and excluded or down-ranked according to explicit freshness policy.

### FR-PLY — Playbooks and Automations

- **FR-PLY-001** Playbooks MUST be immutable versioned orchestration graphs composed of steps, dependencies, agent/tool roles, inputs, evidence gates and capability intents.
- **FR-PLY-002** Playbooks MUST NOT grant permissions; each runtime action remains broker-authorized.
- **FR-PLY-003** Automations MUST bind a Playbook to a manual, schedule, repository/CI or connector event trigger with durable idempotency/cursor state.
- **FR-PLY-004** event payloads MUST be treated as untrusted data; public/comment triggers require explicit high-risk policy and narrow filters.
- **FR-PLY-005** non-interactive automation encountering an unresolved `ask` decision MUST block/notify instead of auto-approving.
- **FR-PLY-006** successful recurring runs MAY update bounded AutomationState but MUST NOT accumulate an unbounded conversation transcript.

### FR-TRJ — Trajectory learning and Session Insights

- **FR-TRJ-001** the harness MUST be able to emit a versioned observable `TrainingTrajectory` containing environment, prompt/version refs, context selections, model/tool boundary events, patches, verification, metrics, outcome and data-policy label.
- **FR-TRJ-002** trajectory collection MUST NOT require, infer or reconstruct hidden chain-of-thought.
- **FR-TRJ-003** production/user trajectories MUST be excluded from external training/export by default; export requires explicit data policy, redaction and secret scanning.
- **FR-TRJ-004** the harness MUST support candidate ranking/rejection sampling using hard correctness/security gates followed by multi-objective efficiency metrics.
- **FR-TRJ-005** synthetic task-environment generation MUST keep generated/training suites separate from held-out production-quality evaluation suites.
- **FR-TRJ-006** endurance evaluation MUST support 1h, 4h, 12h and 24h tiers and at least one >=1,000-tool-call scenario with restart/compaction/handoff faults.
- **FR-TRJ-007** Session Insights MUST derive evidence-linked findings from the Event Ledger and deterministic metrics before optional LLM summarization.
- **FR-TRJ-008** Session Insights MAY propose Knowledge, Playbook, prompt, router or Context Engine changes but MUST NOT silently apply or promote them without normal review/eval gates.

### FR-CU2 — Full Computer Use V2

- **FR-CU2-001** Computer Use MUST support normalized `BrowserPage`, `Desktop`, `Window`, `Tui`, `AndroidEmulator`, `IosSimulator` and `RemoteDesktop` surfaces where platform support exists.
- **FR-CU2-002** target resolution MUST prefer DOM/test-id/accessibility/native control/TUI semantic targets before vision and raw coordinates.
- **FR-CU2-003** coordinate/visual targets MUST be bound to an Observation ID, surface generation and geometry; stale coordinate actions MUST be rejected.
- **FR-CU2-004** desktop adapters MUST normalize macOS Accessibility, Windows UI Automation and Linux AT-SPI semantics into a common tree; unavailable semantics MUST be an explicit degraded state.
- **FR-CU2-005** full screenshots MUST NOT be the default token path when structured observations or visual deltas suffice; metrics MUST track vision tokens and screenshot count.
- **FR-CU2-006** Computer Use MUST support click, drag, scroll, text, key/chord, window focus/resize, app launch, navigation, approved upload and staged download actions.
- **FR-CU2-007** SecretHandle-backed UI entry MUST resolve plaintext only at the final executor boundary and MUST mask the target region from recording/event payloads.
- **FR-CU2-008** auth, MFA, CAPTCHA, payment, external publication, destructive operations, OS security prompts, upload/download and secret/clipboard access MUST be separate high-risk capability classes.
- **FR-CU2-009** Computer Use MUST never treat page/window/app text as permission authority; UI prompt injection remains untrusted data.
- **FR-CU2-010** a diff/goal-aware `UiTestPlanner` MUST be able to produce a focused test plan with explicit assertions and evidence requirements.
- **FR-CU2-011** browser/desktop/mobile E2E runs MUST be able to capture annotated video, before/after screenshots, DOM/AX snapshots, application logs and action timestamps according to evidence policy.
- **FR-CU2-012** video evidence MUST be bindable to Goal criteria but MUST NOT replace deterministic assertions when deterministic evidence is available.
- **FR-CU2-013** Computer Use MUST integrate Human Control Handoff for MFA/CAPTCHA, ambiguous security dialogs and user-requested takeover.
- **FR-CU2-014** GUI execution requiring isolation MUST run inside an explicit GUI sandbox/worker profile with app/file/network/clipboard bounds and MUST fail closed if that required isolation cannot be provided.

### FR-EVAL2 — V2 harness gates

- **FR-EVAL2-001** eval scenarios MUST support persistent background agents, managed workers, execution handoff, human takeover and Computer Use surfaces using the same production contracts.
- **FR-EVAL2-002** release suites MUST measure cross-agent duplicated context, parallel efficiency, merge-conflict cost, per-agent idle time and coordinator overhead.
- **FR-EVAL2-003** Computer Use release suites MUST measure semantic-target rate, coordinate fallback, stale-action rejection, visual tokens, sensitive-action violations and evidence completeness.
- **FR-EVAL2-004** handoff fault-injection suites MUST prove the maximum simultaneously valid write-owner count is one.
- **FR-EVAL2-005** all runtime/prompt/router/context variants promoted from trajectory experiments MUST pass matched held-out baseline evaluation and security hard gates.


## 6. Non-functional requirements

### Performance

- TUI input-to-render p95 <16 ms under normal transcript size.
- file metadata scan of 100k files <5 s warm cache, excluding parsing/embeddings.
- incremental update for one changed source file p95 <250 ms for syntax/FTS path.
- retrieval query p95 <150 ms local for repositories with <=1M indexed chunks, excluding remote embedding call.
- event append p95 <10 ms local SSD for ordinary session events.
- headless event delivery p95 <50 ms after internal event commit.

### Reliability

- no acknowledged file mutation without rollback metadata or isolated view.
- session event sequence has no duplicates and no gaps from the perspective of committed events.
- crash at any injected persistence boundary must recover to a valid projection.
- background job cancellation propagates to descendants.

### Security

- zero ambient network in isolated execution unless policy grants it.
- project-controlled config cannot execute before trust decision.
- secret values are redacted at logging boundary.
- plugin/hook tool escalation is impossible without broker lease.
- untrusted tool/MCP/browser output is labeled as data, never promoted to system instructions.

### Portability

- Tier-1: macOS arm64/x64, Linux x64/arm64, Windows x64.
- gVisor backend: Linux where supported.
- iOS simulator: macOS only; remote mac worker is the portability path.
- Firecracker: Linux/KVM hosted/remote worker only.

### Accessibility and UX

- keyboard-only complete operation.
- color is never sole state indicator.
- screen-reader friendly plain transcript mode.
- `NO_COLOR` respected.
- terminal width down to 80 columns remains usable; under 100 columns panels collapse into tabs.

## 7. Success metrics

### North-star

**Verified autonomous task throughput per 1M total tokens**, segmented by task complexity and repository size.

### Supporting metrics

- verified task success rate;
- median tokens per successful task;
- retrieval precision@k / evidence recall;
- percentage of repeated unchanged code resent to model;
- approval frequency per successful task;
- rollback/recovery success rate;
- policy violation rate (target 0 in release suite);
- uncontained process rate (target 0);
- multi-agent merge conflict rate;
- cost and wall-clock time per verified task;
- user intervention count for autonomous goals.

## 8. Product acceptance scenarios

### Scenario A — Repository-scale refactor

User asks: “Migrate the auth package to the new token interface, update all callers, and prove tests and static checks pass.” RapidLM indexes dependencies, creates goal criteria, delegates search/tests to scoped agents, edits in isolated view, runs checks, links evidence, presents a single attributable diff, and marks complete only when required evidence passes.

### Scenario B — Untrusted repository

A cloned repository contains project hooks/MCP config and a prompt-injection string inside a README. RapidLM loads the text only as untrusted data, does not execute hooks or start MCP servers before trust, and requires capability approval for any sensitive action.

### Scenario C — Headless CI review

`rapid run --jsonl --mode review --policy ci.toml "Review this PR"` emits only versioned events, never prompts interactively, exits non-zero on policy/scanner gate failure, and attaches a trace bundle.

### Scenario D — Browser reproduction

Agent launches a local web app in sandbox, opens isolated Chromium context, reproduces a UI bug using accessibility selectors, records trace/screenshot evidence, patches code, reruns test, and links before/after evidence to the goal.

### Scenario E — Mobile bug

Agent boots an Android emulator from a clean snapshot, installs debug APK, reproduces issue, captures logcat/screenshot, patches code, builds and reinstalls, verifies fix, and preserves artifacts. On iOS, same workflow is available only on macOS/remote mac worker.

### Scenario F — Managed agent fleet

A repository-scale migration is decomposed into read-only exploration, two isolated implementation workers and a verifier. The coordinator sees child evidence/usage and integrates only verified ChangeSets. The parent transcript is not copied wholesale into any child.

### Scenario G — Local to remote handoff

A user starts locally, accumulates uncommitted changes, then runs `/handoff remote`. RapidLM parks local writes, signs/transfers a HandoffBundle, restores a fresh remote sandbox, verifies preimages, commits a new execution generation and continues without transferring local capability leases/secrets.

### Scenario H — Human takeover during Computer Use

An E2E test reaches MFA. RapidLM pauses desktop input, grants human ControlLease, records human-attributed interaction without exposing secret text, then re-observes the browser and lets the agent continue verification.

### Scenario I — Visual desktop application verification

Agent launches an Electron/native fixture, generates a focused UI test plan, uses accessibility-first targets, performs drag/keyboard/window interactions, records annotated video, correlates application logs, and attaches passing assertion evidence to goal criteria.

### Scenario J — Session learning loop

A completed session shows repeated source re-reading and unnecessary worker spawning. Session Insights produces evidence-linked findings and proposes a Context Engine experiment plus Knowledge candidate. Nothing is changed until the candidate/experiment passes its normal approval and evaluation workflow.

## 9. Release criteria

A release candidate MUST satisfy the gates in `03-Development-Plan-and-Roadmap.md`, `threat-model-and-security.md`, and `evaluation-specs/*`. No feature is considered shipped solely because it is reachable in the TUI; headless and event contracts must be stable for features marked public.
