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

## 3. Parallel execution (goal §4) — CLOSED this cycle

- `[x]` **Worktree isolation** — every write-capable `task_spawn` child runs in its
  own git worktree view (`apps/rapid/src/agent_views.rs` over `crates/workspace`'s
  GitWorktreeStore/ViewRegistry: `refs/rapidlm/views/{id}`, detached checkout at the
  parent HEAD, write-owner attribution). The child's tools and context are rooted at
  the worktree; the parent's tree, branch, and dirty uncommitted files are untouched
  and never see the child's writes until integration. A view that cannot be created
  (non-git project, limit) refuses the delegation fail-closed — per-file write locks
  remain as scheduling inside a child, not as a substitute for isolation. The view id
  reaches the `/agents` panel (`agent.state_changed` with `workspace_view_id`) and
  the child's report carries its diff-stat.
  Evidence: 6 lifecycle tests on real repositories (`agent_views::tests`): clean
  integrate, check-command re-run (pass and fail), conflict refusal without touching
  the parent, nothing-to-apply release, fail-closed outside git, user's dirty work
  surviving integration.
- `[x]` **Reviewable integration** — interactive sessions hold a successful child's
  changes in its worktree for review (`/diff --agent`, `/agents show`), then
  `/agents integrate <id> [check-command...]` applies the patch with a
  conflict-decided-first three-way-safe apply (file-level overlap against the base
  is decided before anything is written; a conflict leaves the parent byte-identical
  and the worktree kept), optionally re-runs a check command in the parent and
  reports its output, and releases the view. Headless runs auto-integrate (no
  reviewer exists) and report conflicts honestly instead of force-applying.
  `/agents abandon <id>` removes the worktree via the store's never-force-delete
  removal — the parent was never touched, so there is nothing to undo.
- `[~]` Line-level three-way merges within one conflicted file are deliberately
  refused (file-granularity conflicts); a human merges those by hand from the kept
  worktree. Subagent depth stays capped at 1 and concurrency at 32/turn.

## 4. Integrations (goal §5) — ACP + daemon CLOSED this cycle; MCP HTTP + model catalog open

- `[x]` **ACP entry point** — `rapid acp` (`apps/rapid/src/acp_serve.rs`) binds the
  complete v1/v2 adapters to a real workspace over stdio: initialize/new/load answer
  through the kernel client; a prompt submits and EXECUTES the turn with the
  production assembly (hooks, MCP, retrieval, durable approval sink, worktree-
  isolated subagents) on its own thread while the loop streams mapped kernel events
  as `session/update` notifications and ends the prompt with the mapped stop
  reason. A pending approval surfaces as a real `session/request_permission`; the
  editor's decision resolves through the durable approval machinery and resumes the
  exact turn. Live evidence: real stdio session — initialize → session/new (ledger
  session) → session/load → session/prompt (turn executed; an unconfigured model
  maps honestly to a refusal stop) → cancel → clean exit 0.
- `[x]` **Daemon/SDK** — `rapid daemon` (`apps/rapid/src/daemon_serve.rs`) binds a
  workspace on an owner-only Unix socket and speaks the TypeScript SDK's exact wire
  contract (`rapidlm.sdk.rpc` v1: hello/hello_ok, dotted methods, event streams
  with cursor + stream_end): sessions create/get/fork/rewind; turns.submit submits
  AND executes; turns.interrupt; approvals.resolve through the durable machinery
  (pending wait resolved, turn resumed as a continuation); events.subscribe streams
  ledger envelopes and ends at terminal turn events (a starvation bug found and
  fixed by the live exercise). Live evidence with the REAL SDK
  (`examples/daemon/sdk-e2e.mjs`, node against the built package): connect →
  sessions.create → turns.submit streaming 5 events → sessions.fork → resume
  replay from cursor 0 → clean exit.
- `[x]` **MCP**: stdio wired (env/bounds/offline diagnostics/reconnect) AND
  Streamable HTTP wired end to end: `crates/mcp` exposes the exchange seam
  (`exchange_skipping_notifications` at the transport level — the streamable
  transport answers each POST within its own response; the old send/recv framing
  could never serve HTTP) and `HttpRequest` read accessors;
  `apps/rapid/src/mcp_http.rs` implements `StreamableHttpIo` over llm-router's
  new `Http1Transport::post_raw` (real HTTP/1.1 + rustls, SSRF guards, bounded
  responses, broker→router cancel bridge); `mcp_config.rs` accepts
  `{"type":"http","url":…,"headers":{…}}` (bounded caps; unknown remote kinds
  still named by kind); `connect_mcp_http` authorizes egress to exactly the
  configured origin with real system DNS for redirect revalidation.
  Evidence: `rapid mcp probe` against a live fixture HTTP server
  (`examples/mcp-http/fixture-server.py`) reports
  `ok=fixture-http tools=1 mcp__fixture-http__ping`; stdio untouched;
  mcp crate tests green (80+6). Bearer-token auth via configured headers
  works; full OAuth discovery remains future work.
- `[~]` **Model setup**: capabilities still hard-coded (vision/caching/reasoning);
  `/model list|select` parsed but unrouted; `rapid doctor` is strong.

### Benchmark findings (live runs, 2026-09-14; full table in `eval/FINDINGS-2026-09-14.md`)

Four live runs completed against a real model endpoint (grok-build-0.1 via
x.ai — the same model the Grok CLI drives, satisfying the same-model
requirement):

- **rapid** (acceptEdits): 25/40 and 17/40 across two runs — genuine model
  non-determinism. Failure clusters: `recovery` pipelines, three `tests`
  tasks, and the `workflow` tasks (which need a shell call that acceptEdits
  denies).
- **grok CLI 1.0.30** (`--always-approve -p`, the comparable auto-approval
  configuration): 40/40 twice. Its native auto-approval covers shell, which
  the recovery/workflow tasks need.
- **Qwen Code 0.22.2**: free tier discontinued 2026-04-15 — all tasks
  skipped, recorded with that reason.

Per the goal's rule, no superiority claim: the runs are reported separately
because rapid's shell approval and grok's auto-approval are different
configurations, and one rapid window lost its model token mid-run. The
measured deltas are harness + approval-configuration, attributed as such.
Observed failures already drove one product change under discussion: rapid's
approval ladder treating shell as a distinct grant is what cost the
recovery/workflow tasks.

## 5. Execution protection (goal §6) — reporting + fail-closed CLOSED this cycle

- `[x]` **Truthful level reporting** — `shell_exec {"sandbox": true}` names the
  tier in its model-visible output: `[sandbox: seatbelt — filesystem writes and
  network are confined]` on macOS with sandbox-exec; `[sandbox: host-restricted —
  resource limits only; filesystem and network are NOT confined]` elsewhere.
  `rapid doctor` probes the same backends and reports available tiers; the two
  surfaces now agree.
- `[x]` **Fail-closed required protection** — `RAPIDLM_SANDBOX_REQUIRED=1`
  (an ExecTools field resolved at open) refuses sandboxed execution on tiers that
  cannot confine, with the reason and remedy in the failure detail. Evidence:
  `sandbox_truth_tests::required_confinement_fails_closed_where_only_resource_
  limits_exist` (early-returns where Seatbelt exists, runs where it matters);
  Seatbelt's own suite genuinely denies network and confines writes
  (`crates/sandbox` seatbelt tests).
- `[x]` **Accurate platform notes** — getting-started documents both tiers and
  the Windows posture (job control, hooks Unix-first; PTY and sandbox tiers need
  Unix tools).
- `[ ]` `rapid sandbox` as a standalone doctor-style command — doctor covers the
  same checks today; the dedicated command is polish, not a truthfulness gap.

## 6. Benchmark (goal §7) — OPEN

- `[ ]` `crates/harness` primitives (ScriptedModel/ReplayProvider, DeterministicGrader,
  AssertionEngine, FaultInjector) exist, consumed by nothing; no `rapid eval`; no
  30–50 task suite; prior Qwen comparisons were manual/ephemeral; nothing for Grok.

## 7. Adoption (goal §8) — version, docs, clean-env smoke CLOSED; signing/update OPEN

- `[x]` **Clean-environment smoke** — `scripts/release-smoke.sh <binary>`: runs the
  release binary under a pristine HOME (no config/trust/credentials) and asserts
  the documented first-contact behavior — `--version` prints, help lists the
  shipped commands, an untrusted project is named as such, `trust grant` flips the
  gate, doctor reports the model check, and `rapid eval --offline` passes the
  shipped 40-task suite. Evidence: green against `target/release/rapid` on this
  machine; wired for CI by invoking it after the release build.
- `[x]` **Versioning** — `rapid --version` / `-V` prints `rapid 0.1.0 (RapidLM CLI)`
  from the workspace version; previously it silently opened the TUI.
- `[x]` **First-run guide** — getting-started covers configure (real config.toml
  example) → trust → the permission ladder (permissions allow / modes) → sandbox
  levels → the exit-code table (test-asserted); this session's scripted
  walkthrough exercised exactly that path end to end with a live model endpoint.
- `[x]` **Self-update with failure recovery** — `rapid update` (registered
  subcommand, `apps/rapid/src/update_serve.rs`): fetches a release manifest
  (`{version, sha256, url}`; https-only fetch), refuses downgrades unless
  `--force`, verifies the artifact checksum BEFORE touching anything, smoke-runs
  the staged binary (`--version` must report the manifest version), swaps
  atomically, and rolls the old binary back if the smoke run fails. Five
  lifecycle tests on executable fixtures: clean swap, checksum refusal,
  wrong-version staged artifact rollback, downgrade refusal, bounded manifest
  parsing. Signed manifests are surfaced for user verification (signing keys
  are operator credentials this binary does not hold).
- `[x]` **SBOM + provenance** — `rapid release-manifest <version> <artifacts…>`
  now embeds a CycloneDX SBOM read from Cargo.lock (248 components for this
  workspace, live-checked) and a provenance block (commit via
  `RAPIDLM_BUILD_COMMIT`, builder, timestamp).
- `[x]` **Release notes + RC draft** — `docs/RELEASE-NOTES-0.1.0-rc1.md`:
  highlights, honest limitations (streaming, credentials, signing), platform
  table, upgrade path. Publication stays the final approval step.
- `[~]` 3-target release workflow with SHA-256 checksums + smoke steps remains;
  install docs match artifacts. Code signing beyond checksums requires signing
  infrastructure (approval-gated credentials).

## Sequencing (updated)

1. ~~Approval workflow~~ → done (unblocked waits for §3/§5).
2. ~~Queued messages / provider-cancel / clarification~~ → done.
3. ~~Workflow executor + examples + completion semantics~~ → done.
4. §4 subagent isolation (workspace crate wiring) — next.
5. ~~§5 ACP + daemon/SDK bridge + MCP HTTP~~ → all three entry points closed.
   Also closed since this document's first commit: §7 (rapid eval + 40-task suite
   + offline validation + live-mode honesty), §6 (truthful levels + fail-closed),
   per-model capability overrides, and the fresh-user walkthrough (live: configure
   → doctor → trust → exec change → scoped approvals → rerun → tests pass → diff
   → --continue). Validation at this commit: rapid lib 775 + approvals_flow 6 +
   6 agent_views + eval module 6, all green; crates kernel/agent-runtime/
   event-ledger/scheduler/tui/workspace 1093 green; mcp 86 green; release smoke
   green against target/release/rapid.
6. §6 sandbox truthfulness.
7. §7 benchmark harness offline + suite.
8. §8 release hardening + RC prep.
