# Gap-to-Delivery Baseline — 2026-09-13 (HEAD `b60b01f`)

Revalidation of `gap_analysis.md` (2026-09-01, commit `ad54349`) and `calibration/` (2026-08-24,
commit `c28572d`) against current HEAD. Both earlier reports contain stale claims; where a claim
no longer holds it is marked FIXED below and not tracked as work. This document is the working
checklist for the delivery goal: each item records current behavior + real entry point, the
user-visible acceptance bar, where the work goes, and evidence status.

Legend: `[ ]` open · `[~]` partial · `[x]` closed (with evidence noted inline).

## What the earlier reports got wrong at HEAD (do not re-do)

- FIXED — TUI never sends prompt text: `SubmitTurn` now carries `text` (`crates/kernel/src/client.rs:74`); interactive turns run on the same setup as headless (hooks, MCP, retrieval, reminders, fallback chain, subagents — commit `6b0ce30`).
- FIXED — context budget hard-coded 8192: `context_budget_for` derives from the resolved model (`apps/rapid/src/interactive.rs:3077`); tests assert the old constants are gone.
- FIXED — trust un-grantable: `rapid trust grant|status|revoke` (`apps/rapid/src/interactive.rs:977`) writes `ProjectTrustStore`; untrusted ⇒ all tools refused, fail-closed.
- FIXED — CLI help fiction: help is generated from the same `SUBCOMMANDS` table that dispatches (`apps/rapid/src/interactive.rs:566-721`); 22 advertised = 22 dispatched, test-enforced.
- FIXED — `rapid doctor` stub: real diagnosis incl. production model resolution + live sandbox probe naming the backend (`apps/rapid/src/doctor.rs`).
- FIXED — `rapid exec --resume/--continue` (commit `3982049`), `/compact` (commit `b0700b9`), session transcript replay on resume, `/agents cancel` (commit `11c7266`), Windows CI.
- STALE — "tool-gateway is an orphaned parallel authority": still true that apps/rapid defines its own 15-tool surface (`exec_tools.rs`); tool-gateway remains unused by the product (kept as schema/repair library). Disposition: do not migrate the exec loop onto GatewayTool this cycle; it is not a user-visible gap.

## 1. Interactive dependability (goal §2)

- `[ ]` **Approval workflow** — Today an `Ask` decision is a typed denial on every surface ("no surface in this build can prompt for an approval", `apps/rapid/src/permissions.rs:364`, characterization test `apps/rapid/src/interactive.rs:9325`). Storage and RPC already exist unused: durable `approvals` wait table (`crates/event-ledger/src/journal.rs:652-729`), `ResolveApproval` + `ApprovalDecision` (`crates/kernel/src/client.rs:138`), IPC handler (`crates/kernel/src/ipc/server.rs:1029`), SDK `approvals.resolve` (`sdk/typescript/src/client.ts:352`), broker approval domain (`crates/capability-broker/src/approval.rs`), defensive `TurnStopReason::ApprovalRequired` (`crates/agent-runtime/src/turn.rs:1573`).
  Acceptance: pending tool call surfaced with action/scope/diff; approve-once / scoped remembered / deny; exact pending action resumes without repeating completed side effects; pending approvals survive restart; no exclusive lease or active-work budget held while waiting; one policy engine behind TUI, headless, external clients; managed policy + explicit deny preserved.
  Location: `apps/rapid/src/exec_tools.rs` (decision enforcement), `apps/rapid/src/interactive.rs` (turn loop + TUI surface), `crates/kernel/src/client.rs` + `crates/event-ledger/src/journal.rs` (durable waits), `crates/tui` (panel).
- `[ ]` **Queued messages** — submission while a turn runs is silently dropped by design (`apps/rapid/src/interactive.rs:6005-6013`). Acceptance: never silently discarded; durable with visible state; cancellable/editable; explicit follow-up/interruption semantics.
- `[~]` **Progressive streaming** — providers stream but deltas are folded before any surface sees them (`apps/rapid/src/model.rs:670`); TUI updates per tool-step (50 ms event drain), exec prints only the final answer (`interactive.rs:4327`). `ModelStreamDelta` exists in protocol/JSONL remap/recovery but no production emitter. Acceptance: model output streams progressively in TUI and `exec --jsonl`.
- `[~]` **Cancellation** — propagates to tools, child agents, jobs, wall-clock; ONE gap: in-flight provider HTTP uses a fresh never-cancelled token (`apps/rapid/src/model.rs:316-320`).
- `[ ]` **Clarification** — `ask_user` with no source stops the turn with `ContextRequired`; TUI answers via a normal follow-up message; headless exits `NeedsContext` (`apps/rapid/src/exec_tools.rs:3266-3322`, `interactive.rs:4249`). Acceptance: surface question (with options), accept answer, continue the pending work on the durable-wait machinery.
- `[x]` **Restart recovery (turns)** — `exec --resume <id>|--continue`, TUI `rapid resume` + `/resume` rebuild transcript from ledger; compaction summaries survive. Approval/queue recovery rides the approval/queue work above.

## 2. Workflows + verified completion (goal §3)

- `[ ]` **Public execution path** — scheduler has graph IR, `GraphService` (ledger-backed create/propose/state/wait/retry/invalidate/fan-out), playbook compile, `GraphBackedRun`+`Supervisor` — but no executor loop, no node executors, no ledger rebuild on restart (`crates/scheduler/src/service.rs:121` reads memory only), and nothing reachable from the binary (`playbook-compile` prints JSON; `rapid run`/`rapid graph` do not exist). Acceptance: load+validate, dependent/independent steps, bounded parallelism, approval/clarification waits, background-process + external-condition waits, status/progress/failure/cancel, restart recovery, retry without replaying external effects, evidence invalidation on code change.
- `[ ]` **Runnable examples** — none exist (only an embedded test template). Need 4: bugfix flow, feature flow, parallel-isolated flow, interrupt/restart/recovery flow.
- `[~]` **Completion semantics** — goal surface is evidence-gated (`rapid goal claim/verify`, `GoalHost.complete` gated by `evidence.can_complete`, `apps/rapid/src/goal_host.rs:343`); plain `rapid exec` completion = "turn ended", no verified signal or acceptance criteria. Acceptance: finished-vs-verified distinction in exec output/exit statuses; denied actions/unmet criteria/failed checks in structured output.

## 3. Parallel execution (goal §4)

- `[~]` **Isolation** — write-capable subagents run in the parent tree with per-path write locks only (`apps/rapid/src/exec_tools.rs:1256`); bounded concurrency (≤32/turn, depth 1) and narrowed lattice exist. `crates/workspace` has the full machinery (GitWorktreeStore, ViewRegistry/scope, overlay, `MergePreview` with machine-readable conflicts, transactions with verification hooks) — used only by shadow diagnostics. Acceptance: each write child gets isolated working state + bounded perms/resources + cancellation + attribution; reviewable integration (show changes+evidence, detect conflicts, integrate deliberately, re-run checks, preserve user changes, abandon safely).
- `[~]` **Review surface** — `/agents` panel shows state incl. merge-state columns but `pause/resume/apply` intents are parsed-then-unsupported (`crates/tui/src/commands.rs:879`, `command_help.rs:63`); `/diff --agent` works; no event carries view/patch payloads (`interactive.rs:6664`).

## 4. Integrations (goal §5)

- `[ ]` **ACP entry point** — agent-side v1+v2 adapters complete (`crates/acp/src/v1.rs`, `v2.rs`, stdio framing) but no serve loop and no `rapid acp`.
- `[ ]` **Daemon/SDK** — `IpcServer` complete (challenge auth, subscribe, submit_turn with text, approve, fork, rewind) but bound by nothing; SDK ↔ kernel protocol mismatch (dotted method names + `hello_ok` vs underscore names + `auth.challenge`). Acceptance: real client session — connect, submit, stream progress, approve, cancel, reconnect, resume.
- `[~]` **MCP** — stdio wired with env/bounds/offline pseudo-tools/per-turn reconnect (`apps/rapid/src/exec_tools.rs:5264`); `rapid mcp add/list/remove/probe` works; `StreamableHttpTransport` implemented in-crate and unwired; no url/headers config; no OAuth. Acceptance: Streamable HTTP + auth where required, timeouts/reconnects/clear diagnostics, trust/credential boundaries, stdio preserved.
- `[~]` **Model setup** — resolution + fallback chain + doctor are strong; capabilities hard-coded `vision=false, caching=false, reasoning=None` (`apps/rapid/src/model.rs:166`); `/model list|select|doctor` parsed but unrouted/unsupported (`crates/tui/src/commands.rs:943,1102`). Auth = inline key or env var only.

## 5. Execution protection (goal §6)

- `[~]` **Sandbox** — reachable tiers: Seatbelt (macOS, real FS+network confinement; tests genuinely deny network) and host-restricted (resource limits only; FS writes and network accepted-but-NOT-enforced, `crates/sandbox/src/backends/host_restricted.rs:68-81`). Container/gVisor/remote backends real but instantiated nowhere in the product. Active level is not reported in tool output; no required-protection fail-closed knob; `rapid sandbox` does not exist; doctor reports tiers truthfully but only as a warning. Acceptance: per-platform active level reported; advertised restrictions enforced; fail-closed when required protection unavailable; real allow/deny tests; accurate Windows notes.

## 6. Benchmark (goal §7)

- `[ ]` — `crates/harness` primitives (ScriptedModel/ReplayProvider, DeterministicGrader, AssertionEngine, FaultInjector, metrics) exist and are consumed by nothing; no `rapid eval`; no task suite; prior Qwen comparisons were manual + ephemeral (`docs/benchmarks/2026-08-2*.md`); nothing for Grok Build. Acceptance: reproducible harness, 30–50 repo tasks, pinned competitor versions, limits, external grading, honest skipped/failed reporting; offline (scripted-model) validation first.

## 7. Adoption (goal §8)

- `[~]` — 3-target release workflow with SHA-256 checksums + smoke steps (`.github/workflows/release-matrix.yml`); install docs match real artifacts (`docs/getting-started.md`, test-asserted exit-code table); gaps: no `rapid --version`, no self-update, no signing beyond checksums, no SBOM/provenance, no clean-environment smoke harness, no first-run guide beyond getting-started. Publication stays an explicit approval step.

## Sequencing

1. Approval workflow (unblocks §2, §3 waits, §5 ACP/SDK approval round-trips).
2. Queued messages + streaming + provider-cancel + clarification.
3. Workflow executor + examples + completion semantics (rides approval waits).
4. Subagent isolation + integration path.
5. ACP + daemon/SDK + MCP HTTP (ride approval machinery).
6. Sandbox truthfulness.
7. Benchmark harness offline + suite.
8. Release hardening + RC prep.
