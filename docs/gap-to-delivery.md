# Gap-to-Delivery Baseline — 2026-09-13 (HEAD `b60b01f` + delivery commits)

Revalidation of `gap_analysis.md` (2026-09-01, commit `ad54349`) and `calibration/`
(2026-08-24, commit `c28572d`) against current HEAD, updated with delivery evidence
from this cycle. Both earlier reports contain stale claims; items marked FIXED are
closed and not re-tracked. This is the working checklist: current behavior + entry
point, acceptance bar, location, evidence, and — explicitly — what remains open.

Legend: `[x]` closed with evidence · `[~]` partial (named limitation) · `[ ]` open.

## What the earlier reports got wrong at HEAD (do not re-do)

- FIXED — TUI never sends prompt text (`SubmitTurn` now carries `text`), context budget no longer hard-coded 8192, trust grantable (`rapid trust`), CLI help generated from the dispatch table (test-enforced), `rapid doctor` real, `exec --resume/--continue`, `/compact`, `/agents cancel`, Windows CI.

## 1. Interactive dependability (goal §2) — CLOSED this cycle

- `[x]` **Approval workflow** — an `Ask` decision on a surface that can reach a human
  now records a durable pending approval (action summary, scope, bounded diff) via
  `approval.requested` + the approvals wait table (`apps/rapid/src/approvals.rs`,
  kernel `RecordApproval`/`ResolveApproval`/`pending_approvals`), and the turn
  suspends carrying a `TurnSuspension` (every completed exchange + the pending
  placeholder — `agent-runtime/src/turn.rs`). The turn finishes as
  `TurnOutcome::Waiting`, releasing the lease — nothing holds the session or budgets
  while a human decides. `/approvals` lists / approves once / approves-and-remembers
  (scoped persisted grant; never offered for `shell_exec`) / denies / answers; the
  TUI modal renders the action (`crates/tui` approval projection). A restarted
  process re-offers pendings at session start. The continuation turn replays the
  suspension and executes the approved call exactly once via `execute_preapproved`
  (deny rules + managed ceilings re-checked, so approval never widens policy).
  Evidence: `apps/rapid/tests/approvals_flow.rs` (6 integration tests: suspend +
  record + diff, restart-visible pending, duplicate-resolution fails closed,
  deny-continues-with-typed-denial, no-sink fail-closed denial, managed-policy deny
  beats the surface, suspension wire round-trip, clarification rides the same
  machinery); live TUI modal rendering tests in `crates/tui`.
- `[x]` **Queued messages** — a submission while a turn runs is queued, journaled at
  acceptance (`message.queued`/`message.state`), restored after restart, and
  `/queue` lists/cancels/edits/runs. Auto-dequeue waits for an idle slot AND an
  empty pending-approval set. Evidence: rewritten test
  `second_submission_while_a_turn_is_in_flight_is_queued_durably_not_dropped`.
- `[~]` **Progressive streaming** — the llm-router transport reads the full response
  body before parsing, so token-level streaming requires an incremental transport
  that does not exist yet. The TUI already renders per-step progress (model step +
  tool events, 50 ms drain); a `ModelStreamDelta` event kind exists end-to-end for
  the moment an incremental transport lands. **Named limitation, not done.**
- `[x]` **Cancellation into provider requests** — `ProviderCancelWatch`
  (`apps/rapid/src/model.rs`) bridges the turn's cancel token into the router's, so
  Ctrl-C / wall-clock aborts an in-flight provider request at the transport's read
  checks (previously a fresh never-cancelled token ran the request to its timeout).
- `[x]` **Clarification** — `ask_user`'s ContextRequired suspension records the
  question as a pending on the same machinery; the answer resumes the turn with a
  one-shot answer source. Evidence: `a_clarification_is_pending_until_answered_then_continues`.

## 2. Workflows + verified completion (goal §3) — CORE CLOSED this cycle

- `[x]` **`rapid run`** (`apps/rapid/src/workflow.rs`, `p9_commands::run_run_command`):
  load + validate through the scheduler compiler (one authority: bounds, unique
  keys, acyclic deps, connected entry); dependent/independent steps with bounded
  parallelism (`--parallel`, default 4); agent steps run real turns via the
  production assembly; verification/process steps run commands (bounded, timeout);
  approval/ask_user steps pause on the durable wait machinery; monitor steps poll
  bounded commands. Run state is an atomically-saved `.rapidlm/runs/<id>.json`.
  Retry: failed steps retry in place with a persistent attempts counter; completed
  steps are NEVER replayed (at-most-once guard against repeated external effects);
  `--retry` resets one failed step. Evidence invalidation: steps with `watch` globs
  record an evidence digest; a resume over changed files invalidates the step and
  its dependents.
  Evidence: 6 module tests (validation, diamond parallelism, retry-then-succeed +
  no-replay-on-resume, pause/resume, watch invalidation, glob semantics) plus a
  LIVE smoke: fresh run → paused exit 10 → `--resolve approve` in a second process →
  `--resume` in a third → `"verified": true`, exit 0; `--status` reports per-step
  state and freshness.
- `[x]` **Runnable examples** — `examples/playbooks/`: bugfix-flow (diagnose→patch→
  regression test→verify→approval), feature-flow (plan→implement→tests→verify→diff
  review), parallel-investigation (goal→two parallel surveys→integrate→verify),
  long-running-with-recovery (prepare→approval gate→migrate→verify).
- `[x]` **Completion semantics** — finished ≠ verified: exit 0 requires every
  verification step to have run fresh in the final invocation; finished-but-stale
  or unmet exits 6 with the unmet steps named in a structured JSON report;
  failed runs name the step and how to retry; paused runs exit 10 with
  resolve/resume instructions (new `JsonlExitCode::NeedsApproval = 10`, documented
  in getting-started's asserted table).
- `[ ]` **`rapid run` remaining depth** — background-process steps (`process` kind
  with long-lived supervision) and external-condition monitors poll synchronously
  with a ceiling rather than supervising across pause/resume; run-level progress in
  the TUI (runs are currently CLI-first). Open, scoped.

## 3. Parallel execution (goal §4) — OPEN

- `[ ]` Write-capable subagents still run in the parent tree with per-path write
  locks; the workspace crate's GitWorktreeStore/ViewRegistry/MergePreview/transaction
  machinery (conflict detection, verification hooks, rollback) is built and tested
  in `crates/workspace` but not wired into `task_spawn` or the `/agents` panel's
  parsed-but-unsupported `apply/pause/resume` intents. Concurrency (≤32/turn, depth
  1, narrowed lattice) and attribution (`PatchSummary`, workspace-changes events)
  exist. The design is ready (isolate child → attributable view → `/agents
  integrate` via `commit_transaction` with re-run checks → `abandon` via the store's
  non-destructive cleanup) but is not implemented.

## 4. Integrations (goal §5) — OPEN (machinery exists, entry points do not)

- `[ ]` **ACP**: agent-side v1+v2 adapters complete in `crates/acp`; no `rapid acp`
  serve loop. The approval round-trip it needs now exists (§2).
- `[ ]` **Daemon/SDK**: `IpcServer` complete (challenge auth, subscribe,
  submit_turn with prompt, approve, fork, rewind) and unbound; SDK speaks dotted
  method names + hello handshake vs the kernel's underscore names + auth.challenge —
  a translation layer is needed. `approvals.resolve` on the SDK side now has a real
  producer behind it.
- `[~]` **MCP**: stdio wired (env/bounds/offline diagnostics/reconnect); `crates/mcp`
  has StreamableHttpTransport — unwired; no url/headers config; no OAuth.
- `[~]` **Model setup**: capabilities still hard-coded (vision/caching/reasoning);
  `/model list|select` parsed but unrouted; `rapid doctor` is strong.

## 5. Execution protection (goal §6) — OPEN

- `[ ]` Seatbelt (macOS) enforces for real (tests genuinely deny network);
  host-restricted enforces resources only and its fs/network non-enforcement is not
  surfaced in tool output; no required-protection fail-closed knob; `rapid sandbox`
  does not exist; doctor reports tiers truthfully but only as a warning.

## 6. Benchmark (goal §7) — OPEN

- `[ ]` `crates/harness` primitives (ScriptedModel/ReplayProvider, DeterministicGrader,
  AssertionEngine, FaultInjector) exist, consumed by nothing; no `rapid eval`; no
  30–50 task suite; prior Qwen comparisons were manual/ephemeral; nothing for Grok.

## 7. Adoption (goal §8) — OPEN

- `[~]` 3-target release workflow with checksums + smoke steps; install docs match
  artifacts; missing: `rapid --version`, self-update, signing beyond checksums,
  SBOM/provenance, clean-environment smoke harness, first-run guide depth.

## Sequencing (updated)

1. ~~Approval workflow~~ → done (unblocked waits for §3/§5).
2. ~~Queued messages / provider-cancel / clarification~~ → done.
3. ~~Workflow executor + examples + completion semantics~~ → done.
4. §4 subagent isolation (workspace crate wiring) — next.
5. §5 ACP + daemon/SDK bridge + MCP HTTP (ride the approval machinery).
6. §6 sandbox truthfulness.
7. §7 benchmark harness offline + suite.
8. §8 release hardening + RC prep.
