# newtask.md — Parity-then-Leapfrog Roadmap: RapidLM vs. Grok Build vs. Qwen Code

- **Date:** 2026-08-29
- **RapidLM baseline:** this workspace @ commit `687c745` (verified directly from source, not from `gaps.md`'s
  own tables — several of that document's "current state" rows were found stale during this pass; see
  the note in §0).
- **Competitor baselines:** Grok Build @ `9684fa3` (`gaps.md`, full source-grounded audit, cloned at
  `/Users/mohsin/grokbuild`); Qwen Code @ `265e7f1` (fresh source-grounded audit, this session, cloned
  shallow at `/private/tmp/claude-501/qwen-code-src`).
- **Third input — Modbit:** `/Users/mohsin/useful /Modbit_Feature_Inspiration_Provenance_2026-08-18_v2.md`
  (and the wider Modbit dossier at that path and at `/Users/mohsin/modbit`). Modbit is a separate, more
  mature product (an Electron/Code-OSS IDE fork with its own Rust context engine, policy kernel, change
  engine, verification plane, etc.) that the user has been designing across prior sessions. It is used
  here strictly as a **mechanism reference**: every item pulled from it below is named by its Modbit
  feature ID for traceability, but is to be **independently, natively reimplemented** against RapidLM's
  own crates — never ported as code, never treated as a dependency. This mirrors Modbit's own stated rule
  for its competitor research (`GOV-005`, "clean-room competitor adoption").
- **Severity scale:** `P0` = blocks credible parity · `P1` = closes a real competitive gap · `P2` = polish
  or a leapfrog bet · **Effort:** S / M / L, rough shape not a schedule commitment.

---

## 0a. Meta-finding from this implementation pass (read this before Phase 2 especially)

Nearly every gap this pass actually investigated in depth turned out to be smaller than written, for
the same underlying reason: **this codebase already contains mature, well-tested implementations of
most of the sophisticated primitives Phase 2 asks for — they are simply not wired into the exec loop
`apps/rapid` actually runs.** This is the exact same shape as the context-engine finding from commit
`687c745` (~92% of that crate was built and tested but unreachable from `rapid exec`), and it recurred
repeatedly during this pass:

- `crates/sandbox` — a full tiered `SandboxManager`/`SandboxBackend` system (HostRestricted/Container/
  Gvisor/RemoteWorker, ~11,400 lines) exists; `apps/rapid` doesn't depend on the crate at all (§1.1).
- `crates/capability-broker` — a complete `CapabilityLease`/policy/approval system exists (used
  correctly in test fixtures across several crates); no production code path in the whole repo mints a
  real lease today, confirmed while scoping §1.1's second correction.
- `crates/llm-router` — real per-request cost (`UsageCost::Reported`) and a full `ModelCatalog` with
  pricing/latency/context-limits already exist; the value is computed then dropped at one specific
  `apps/rapid` boundary before ever reaching the CLI (§1.5 row 17 correction).
- `crates/kernel::turn::guard::TurnSubmissionGuard` — exclusive per-session turn occupancy with
  optimistic `expected_seq` conflict detection already exists (165 kernel tests pass) — this is
  substantially what Phase 2 §2.6 (`SessionLease` + fencing) asks for. Not independently re-verified
  whether it's actually exercised by `apps/rapid`'s `resume`/`fork` paths, or only by kernel's own tests.
- `crates/security::scanners::secrets::FindingFingerprint` — a stable content-hash identity (digest
  over rule/path/range/match) for one `Finding` type already exists — substantially what Phase 2 §2.9
  (persisted, content-hash-keyed findings) asks for, at least for secrets scanning specifically.

**A second, opposite pattern also showed up, and matters just as much for anything touching the
interactive TUI specifically:** `run_interactive`'s `SessionLoop` (`apps/rapid/src/interactive.rs`) only
ever drives `InProcessKernelClient`'s session/turn *ledger bookkeeping* — `submit_turn_sync`
(`crates/kernel/src/client.rs:420`) validates a lease and appends a `TurnStarted` event, nothing more.
There is no live agent-turn/tool-dispatch loop in that path at all: `crates/kernel` has zero references
to `ToolDriver`/`PermissionLattice`/`ExecTools`/`AgentExecutor` and doesn't even depend on `agent-runtime`.
Confirmed while scoping §2.3's CAP-005 half, and it explains the `/compact` no-op §1.4/#13 already found
independently (`KernelApi::Dispatch => {}`) — both are the same root cause, not two unrelated bugs. Any
future TUI-touching item in this document should assume "the live chat session doesn't actually run
turns yet" as a starting fact, not verify it fresh each time.

**Practical consequence for whoever picks up a Phase 2 item below: spend 15 minutes grepping for the
primitive before writing new code.** The likely real task is "wire crate X into `apps/rapid`," not
"build X" — a smaller, safer, and differently-shaped piece of work than the item's original prose
describes. Phase 2 §2.1–§2.10 below were written before this pattern was discovered mid-pass and were
not individually re-audited against source with the same rigor as the Phase 1 corrections above (§2.6
and §2.9 got a quick spot-check, noted inline; §2.2–§2.5, §2.7, §2.8's `RouterDecisionRecord` half, and
§2.10 did not). Treat every remaining Phase 2/3 row as a hypothesis to verify, not a confirmed gap.

**Update, 2026-08-30: the pattern kept recurring outside `apps/rapid` too, in crates this document had
never named — worth a consolidated pointer so a future pass doesn't rediscover each one independently.**
`crates/kernel::recovery::RecoveryManager` (a full crash-recovery pipeline — checkpoint load, event
replay, stale in-flight interrupt), `crates/capability-broker::audit` (the whole audit-trail record
system for lease/approval decisions), `crates/handoff` (the entire foreground→daemon ownership-transfer
protocol, `SessionExecutionLease`/generation bumping), and `crates/vcs::provenance` (an append-only
provenance graph binding goal/evidence/agent/patch/commit identities) are each real, tested, and have
**zero callers anywhere outside their own crate.** Two (`RecoveryManager`, `handoff`) are downstream of
the same root cause the second meta-finding paragraph above already names — `apps/rapid`'s interactive
session loop doesn't run a live turn through the kernel at all, so nothing ever reaches the code paths
(resume, crash-recovery-on-open, foreground/daemon handoff) that would call them. The other two
(`capability-broker::audit`, `vcs::provenance`) hit the same wall as §2.10's Credential Broker note
already did: wiring either in for real needs a durable store and session/agent identity available at the
call site, which the one real call site that exists (`sandbox_exec.rs`'s lease-minting ceremony) doesn't
have — "premature before a real scenario exists," not a small wiring gap. None of these four were
implemented or scoped further this pass; flagging them here so effort isn't spent re-discovering the same
"tested but zero callers" fact independently for each one.

**Update, 2026-08-30: `crates/telemetry` (also zero callers outside its own crate) got the same "dormant"
survey, but turned up something different — a real bug in the code itself, not just an unwired crate,
fixed rather than only documented.** `crates/telemetry/src/lib.rs::is_forbidden_key` — the live redaction
gate every attribute passes through in `emit_span`/`emit_log`/`emit_metric` before a record reaches any
sink, including the network-capable `OtlpSink` — matches literal keys `"code"`/`"source_code"` and suffix
patterns for `_prompt`/`_secret`/`_token`, but had no `_code` suffix rule. `crates/telemetry/src/export.rs
::is_content_key` (the redaction list for the separate, local-only diagnostic-bundle exporter) has the
identical list *plus* `ends_with("_code")` — confirmed by direct comparison, not assumption. An attribute
keyed `"patch_code"`, `"diff_code"`, or `"generated_code"` would have been classified `FieldClass::Safe` by
the live gate and reached every sink unredacted, directly contradicting the module's own stated invariant
("Default fields exclude prompt, code... Redaction runs before any exporter sees a record", threat
`T-012`) — the same "check exists on one path, not the sibling path with the same effect" shape this
session already fixed twice elsewhere (the `git commit`/`git merge` gate, the team-memory secret gate).
**Fixed:** added the missing `|| normalized_key.ends_with("_code")` to `is_forbidden_key`, matching
`export.rs`'s list exactly. Extended the existing `protected_keys_are_omitted_not_exported` test with a
`"patch_code"` attribute and confirmed it reproduces the bug against the unfixed code first (the test's own
`omitted_attributes` count assertion failed, and the un-redacted value would have reached the exported
record) before confirming the fix closes it. Full `telemetry` crate suite (24 tests) and `cargo build
--workspace --tests` pass. Found via a general correctness-review pass over the four dormant crates above,
not the "gate bypass" pattern search specifically — worth noting since it means that review methodology is
also productive here, independent of whether the surrounding crate is even wired in yet.

**Same review pass, `crates/vcs::provenance`: `ProvenanceStore::append_all` contradicted its own "append a
bounded batch atomically... on cancel failure nothing is written" doc comment.** The mutation loop (once
the store's mutex was already locked and edges were being pushed into `inner.edges`/`by_from`/`by_to`)
re-checked cancellation every `CANCEL_STRIDE` (8) edges and returned early via `?` on a hit — but any edges
already pushed in that same call's earlier iterations stayed permanently in the append-only store. A
partial batch from `record_patch_attribution` (which can write up to 7+ edges per patch: agent/evidence/
goal/workspace/verification/commit/symbols) would leave a misleading trail — e.g. a patch with `ProducedBy`
recorded but `VerifiedBy` silently missing, which `lineage_for_patch` would then report as if complete.
**Fixed:** removed the mid-loop check entirely — cancellation is already checked in full *before* the lock
is taken (both an initial check and a full pass over the whole batch) and once more immediately after
locking, so by the time mutation starts, either the whole batch commits or the function has already
returned without touching `inner` at all. **Deliberately did not add a new test for the specific race this
fixes:** the old bug's window was a handful of cheap in-memory operations between `CANCEL_STRIDE`-aligned
checks — a genuinely single-digit-nanosecond race a concurrent test could almost never land inside
reliably (the exact "flaky test that proves nothing" trap this session's own CPU-ceiling test correction
already documented, just for a timing-race reason instead of a wrong-proxy-metric one). The existing
`cancelled_record_writes_nothing`/`bound_exceeded_is_atomic` tests already cover the *pre*-mutation
guarantees (pre-cancelled token, over-bound batch) and still pass unchanged; the fix's correctness for the
mid-mutation case rests on the code no longer having a mid-loop exit point at all, verifiable by inspection
rather than a race-dependent test. Full `vcs` crate suite (16 tests, unchanged) and `cargo build
--workspace --tests` pass.

**Same review pass, `handoff`: a real cross-process TOCTOU race that defeats the module's entire stated
purpose — documented, not attempted, given the fix needs real file locking.** The module doc comment
claims this protocol "enforces the single-writer invariant across processes via file-based persistence"
and names "duplicate-writer rejection: only one active writer per session" as an explicit guarantee. Both
`acquire()` and `accept()` do a plain check-then-write: `ledger.load()` (a plain `fs::read`) to see if an
owner already exists, then — if not — `ledger.save()` (write-tmp-then-rename) to claim ownership. There is
no advisory lock, no `O_EXCL`/`create_new` atomic-create, no synchronization spanning the gap between the
two calls (confirmed via grep — no locking primitive anywhere in this file or its dependencies). Two
processes racing `accept()` on the same bundle (a stale foreground CLI and a daemon, or two daemon
instances after a crash-restart) can both `load()` and see nothing, both pass the check, and both
`save()` — the loser's write is silently clobbered by the winner's `rename`, and *both* callers return
`Ok`, believing they hold exclusive ownership. That is exactly the duplicate-writer scenario this protocol
exists to make impossible. **Not attempted:** a real fix needs genuine cross-process mutual exclusion
(an advisory file lock via `flock`, or an atomic `O_EXCL` create as the actual ownership claim instead of
a separate load-then-save pair) — real, non-trivial systems work with real platform differences (Unix
`flock` vs. Windows locking semantics), not a small logic fix like the two just above. Latent today (zero
callers anywhere outside this crate, confirmed), but a genuine, demonstrable divergence between the stated
invariant and actual behavior for whenever `handoff` does get wired in — worth fixing before that happens,
not after.

**Fresh review pass, 2026-08-30, this time over `event-ledger`/`capability-broker` (actively used crates,
not dormant ones) — one confirmed bug, in `event-ledger::retention`, fixed; everything else checked
(checkpoint load/apply ordering, lease TTL boundary, single-use replay rejection, approval-resolution state
machine, cron claim/complete) held up against its own stated invariants.** `RetentionService::collect`'s
"delete unreferenced published blobs" loop checked every artifact's rootedness against a `BTreeSet`
snapshot (`rooted_ids()`) taken *once*, at the very top of the call — but `pin()` (the function that's
supposed to make a blob GC-immune) writes its own `artifact_refs` row independently, with no
synchronization against a concurrently-running `collect()`. A `pin()` call committing after the snapshot
was taken but before `collect()`'s loop reached that specific artifact was invisible to the stale
in-memory set, so the blob got deleted anyway — directly contradicting the module's own doc comment
("Missing roots fail closed: GC never deletes a blob that still has a catalog row pointing at it"), and
worse than merely "unenforced": it's a *dangling pin*, since the `artifact_refs` row survives pointing at
nothing. Not fully fixable atomically — `artifact_refs`/`checkpoints` are SQL tables in the ledger's own
SQLite database, but the published blobs `collect()` actually deletes are plain files under a completely
separate `ArtifactStore` (filesystem-only, no SQL at all), so there's no single transaction that could ever
span both the rootedness check and the delete. **Fixed the achievable half:** replaced the one-time
snapshot with a fresh, single-artifact `is_rooted()` query run immediately before each artifact's own
deletion, narrowing the race window from "however long the whole `collect()` sweep takes" down to "the gap
between one lookup and one delete for one specific artifact" — the standard mitigation for a TOCTOU that
can't be made fully atomic across two storage systems. **Deliberately did not force a concurrent
reproduction test**, the same call made for the `vcs::provenance` atomicity fix above: reliably landing a
background `pin()` inside the now-much-narrower per-artifact window is exactly the kind of race a test can
almost never hit deterministically without either flakiness or a test-only pause hook added to production
code. The three existing tests (`gc_keeps_pinned_and_deletes_unreferenced`, `checkpoint_artifact_is_a_gc_
root`, `unpin_then_gc_removes_blob`) already cover the non-racing behavior and pass unchanged; the fix's
correctness rests on the check now happening at the latest possible point before the irreversible action,
verifiable by inspection. Full `event-ledger` crate suite (87 tests, unchanged) and `cargo build
--workspace --tests` pass. `RetentionService` has zero callers anywhere outside its own tests today
(confirmed via grep), so this was latent, not actively firing — but a real bug in the module's own stated
contract, worth having fixed before it's ever wired into a scheduled GC.

**Fresh review pass, 2026-08-30, `crates/auth` and `crates/workspace` — both actively used (unlike the
mostly-dormant crates above), so these two are live, not latent.** Two real bugs, both fixed.

**1. `crates/auth/src/file_keychain.rs::write_owner_only` wrote secret plaintext world/group-readable
before chmod'ing it private.** `fs::write(path, bytes)` creates a file at the process's default,
umask-derived mode (typically `0644`) and only *after* the full secret was already on disk did a second
step chmod it to `0600` — a real window where a freshly-stored provider API key/OAuth token was readable
by any local reader that sampled the directory in between the two steps, on any host with a permissive
umask. `crates/auth/src/local_daemon.rs::create_private_file` already gets this right elsewhere in the same
crate (`OpenOptions::mode(TOKEN_FILE_MODE)` set *at creation time*, combined with `create_new(true)`),
which is what made the `file_keychain.rs` path stand out as the inconsistent one. **Fixed:** `write_owner_
only` now opens with `OpenOptions::new().write(true).create(true).truncate(true)` plus `.mode(0o600)` on
Unix — the file is created with the restrictive mode atomically, no window — while keeping `create` (not
`create_new`) so re-`put()`-ing an existing item (credential rotation) still overwrites rather than
erroring. Kept the post-write `chmod` too, as a defense-in-depth backstop for the case where the file
already existed from an older, less careful write (`mode()` at open time only applies when the call
actually creates the file). Extended the existing `put_get_round_trips_and_reports_available` test to
confirm re-`put`-ing the same item still overwrites correctly (including truncating a shorter replacement
secret). Full `auth` crate suite (71 tests) and `cargo build --workspace --tests` pass.

**2. `crates/workspace/src/checkpoint.rs::rollback_applied` was a guaranteed no-op on the one call path
that actually needs it, leaving a rewind-apply half-finished with no indication anything went wrong.**
`apply_plan`'s loop applies a plan's ops one at a time and, on any failure (including a cancellation
detected via `index_cancel`), calls `rollback_applied(source, &ops[..applied], cancel)` to undo the prefix
already written — but it passed the *same* `cancel` token that had just been used to detect the abort.
`rollback_applied`'s own loop began each iteration with `if cancel.is_cancelled() { return; }`, so when
triggered by cancellation specifically, it exited on its very first iteration having undone nothing; even
without that explicit guard, `DirectBackend::write`/`delete` (which the rollback loop calls) check
cancellation internally too, so every restore/delete would have failed closed against the same signal
regardless. A rewind-apply interrupted partway through a multi-file plan left whatever prefix had already
landed permanently mixed into the workspace — the caller only ever sees `CheckpointError::Cancelled`,
which this module's own "apply is refused when..." framing implies means nothing happened. **Fixed:**
`rollback_applied` no longer takes a `cancel` parameter at all — it constructs its own fresh, never-
cancelled token internally, so cleanup always runs to completion regardless of why the forward pass
stopped; rollback is a compensating action, not discretionary work that should itself be interruptible by
the same signal that triggered the need for it. New test `rollback_applied_always_restores_regardless_of_
cancellation_state`: builds a real two-file rewind plan via `plan_rewind`, manually applies the first op
(simulating "the forward pass got this far before stopping"), then confirms `rollback_applied` restores it
— verified to actually catch the old bug by temporarily reintroducing an already-cancelled token inside the
fixed function and confirming the same test fails exactly as predicted, before restoring the real fix. Full
`workspace` crate suite (171 tests, up from 170) and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-08-30, `crates/context-engine::compact_policy` — the crate's own fail-closed
hard-limit safety check was measuring the wrong thing, on the actively-used compaction path.**
`compact_with_policy`'s stated contract (its own doc comment) is: after compaction, verify the result via
`estimate_compacted_tokens` and return `StillOverHard` rather than silently hand back a context that still
blows the hard cap. But `estimate_compacted_tokens(compacted: &CompactedContext)` summed the byte-length of
`compacted.retained_locators()` — short block-ID strings like `"task"` (a handful of bytes each, `compact.rs`
confirms `retained_locators` is `block.locator().to_string()`, never content) — instead of the actual
mandatory/system block content that survives compaction untouched. `apps/rapid/src/host.rs::
LiveRecoveryController::recover_from_overflow`, the one real call site, rebuilds the final packet via
`build_packet(preserved, summary)` from the *original* mandatory content regardless of what
`CompactedContext` reported, so the safety check was verifying a handful of locator-label bytes while the
context actually handed back to the model could still be arbitrarily over the hard cap — the exact scenario
the check exists to catch. **Fixed:** `estimate_compacted_tokens` now takes `(packet: &ContextPacket,
compacted: &CompactedContext)` and sums real `estimated_tokens()` over every block that is `is_mandatory()`
or `source() == ContextSource::System` in the original packet, plus the summary's own byte-length estimate
— i.e., it measures what `recover_from_overflow` will actually rebuild, not a proxy for it. **Why the
existing `replacement_over_hard_fails_closed` test never caught this:** it used a degenerate `hard=2`-token
threshold, which the check exceeds trivially regardless of which quantity is being summed — it could never
have distinguished a correct estimate from a broken one. New test
`hard_check_measures_real_mandatory_content_not_locator_labels` uses a realistic `hard=50` threshold with a
45-token mandatory block and a 30-token optional block, and was verified to actually catch the old bug: with
the fix temporarily reverted to the old locator-summing logic, the test failed with `after_estimate_tokens:
21` against `before_tokens: 45` (the check passing when it should fail-closed), exactly as predicted; restoring
the fix makes it pass. Full `context-engine` crate suite (308 tests, up from 307) and `cargo build
--workspace --tests` pass.

**Fresh review pass, 2026-08-30, `crates/scheduler` — `GraphService::fan_out` deadlocked every shard it
created, against its own graph's stated readiness contract.** `EdgeKind::is_executable_dependency()`
(`kinds.rs`) included `DecomposesInto` alongside `DependsOn`/`ScheduledAfter`/`JoinsAt`, and `Graph::is_ready`
(`graph.rs`) gated a node's readiness on *every* `is_executable_dependency()` predecessor edge into it
having reached the required state (default `EdgeCondition::PredecessorSucceeded`). `fan_out` (`service.rs`)
creates shard nodes wired `parent --DecomposesInto--> shard`, and `orch.rs::GraphBackedRun::start` wires
`goal --DecomposesInto--> task` the same way — but a parent that has just decomposed into children is
`Pending`/`Running`, not `Succeeded` (a parent's own completion normally depends on its children finishing,
not the reverse), so every shard's `is_ready` check failed on its very first predecessor edge and could
never become ready by itself. **Confirmed directly** (not just from the earlier review agent's report) with
a new test added specifically to reproduce it before touching any fix code:
`fanned_out_shards_are_ready_immediately_not_deadlocked_on_parent` calls `fan_out` on a fresh graph and
asserts both shards appear in `ready_set()` — against the unfixed code this failed with `ready set was
[<root>]` (neither shard present), exactly the predicted deadlock. **Why the existing
`join_ready_requires_all_predecessors_and_fanout_creates_shards` test never caught this:** it calls
`fan_out` then immediately force-sets both shards to `Succeeded` via `set_state` directly, never once
checking whether the shards were actually `ready` first — it exercises the `Join` node's own predecessor
logic, not whether a freshly-fanned-out shard is schedulable at all. **Fixed:** added a new
`EdgeKind::gates_readiness()` (`DependsOn | ScheduledAfter | JoinsAt` — deliberately excluding
`DecomposesInto`) and switched `Graph::is_ready`'s gating loop (`graph.rs:264`) to use it instead of
`is_executable_dependency()`. Deliberately did **not** remove `DecomposesInto` from
`is_executable_dependency()` itself: that method is also `proposal.rs::has_executable_cycle`'s only input,
and a decomposition graph should still be acyclic (a task must not decompose into its own ancestor) even
though it shouldn't gate readiness — so the two concerns now have two correctly-scoped methods instead of
one overloaded one. Full `scheduler` crate suite (24 tests, up from 23) and `cargo build --workspace --tests`
pass. **Latent, not yet actively firing:** confirmed via grep that `apps/rapid` never calls `fan_out` or
touches graph readiness at all today (matches this document's own §0 note that `rapid graph` has no CLI
surface yet) — but a real, demonstrable bug in the scheduler's own contract, worth having fixed before
fan-out is ever wired into a real run.

**Fresh review pass, 2026-08-30, three separate check-command/hook/external-agent runners — all three read a
child's stdout/stderr only after the wait loop reported it exited, deadlocking on any output past the OS pipe
buffer and misreporting a finished process as timed out.** The shape: spawn a child with
`Stdio::piped()` stdout/stderr, poll `try_wait()` in a loop until it returns `Some` (or a deadline hits), and
only then read the pipes. A child that writes more than the OS pipe buffer (commonly 16-64 KiB, platform-
dependent) before exiting blocks inside its own `write(2)` call once that buffer fills — nobody is reading it
— so it never reaches exit, the poll loop never sees `Some`, and the deadline eventually fires and kills a
process that had already finished its real work. This is not a hypothetical edge case: 64 KiB is an entirely
ordinary amount of chatter for a compiler, test runner, or linter.

Three call sites had this shape:
1. **`apps/rapid/src/goal_claim.rs::run_check_command`** — backs `goal claim`'s deterministic-check
   acceptance evidence (module doc: "the host executes the checks for real"). A genuinely passing check
   command whose combined output exceeds 64 KiB (`MAX_CHECK_OUTPUT_BYTES`) would be misreported as
   `timed_out: true, passed: false`, blocking legitimate goal completion for up to `MAX_TIMEOUT_SECS` (600s).
2. **`apps/rapid/src/external_agents.rs::SupervisedCliRunner::run`** — a productive external CLI agent run
   past `MAX_AGENT_RESULT_BYTES` (256 KiB) would be misreported as failed/lost, discarding all its output.
3. **`crates/plugin-host/src/hooks.rs::run_command_hook`/`run_prepared`** — the module's own doc comment
   claims "**Timeout and nonzero exit stay distinct**"; a verbose lint/format hook past its `output_limit`
   (default 64 KiB, `DEFAULT_HOOK_OUTPUT_BYTES`) would be misreported as `HookRunStatus::TimedOut`, which
   (depending on `failure_policy`) can incorrectly `Block` the gated tool call.

**Confirmed directly, not just from the review agent's report:** added
`cancel::tests::plain_await_exit_deadlocks_on_output_past_the_pipe_buffer` to
`crates/process-supervisor` — spawns `/bin/dd if=/dev/zero bs=1024 count=200` (200 KiB, run through
`process_supervisor::spawn`/`await_exit` exactly like the three call sites) and confirms `await_exit` alone
reports `TerminalStatus::TimedOut` even though the child would exit near-instantly if drained. Then confirmed
the same command, run through the new fix, exits cleanly (below).

**Fixed with one reusable primitive plus three call-site updates, rather than three separate ad-hoc fixes:**
new `process_supervisor::cancel::await_exit_draining` spawns a background thread per stdout/stderr pipe
*before* calling the existing `await_exit`, each thread draining its pipe to EOF via a new `drain_capped`
helper. Two things distinguish this from simply "read on another thread": (1) both pipes must be taken and
handed to their reader threads *before* `await_exit` starts polling, since `await_exit` may call
`terminate_tree` (kill) on timeout/cancel — the pipes stay valid file descriptors independent of the `Child`
handle, so this is safe and matches the concurrent-drain pattern `apps/rapid/src/exec_tools.rs`'s own job
runner already uses correctly. (2) `drain_capped` deliberately never stops reading once it hits its byte cap
— it keeps consuming (and discarding) bytes to EOF. A drain that stopped at the cap would let the pipe fill
again for any output beyond it and reintroduce the identical deadlock, just at a larger threshold; a test
(`await_exit_draining_truncates_at_cap_but_still_drains_to_avoid_deadlock`) specifically pins this down with
a tiny 64-byte cap against the same 200 KiB `dd` command. `goal_claim.rs` doesn't use `process_supervisor`'s
`JobHandle` at all (raw `std::process::Command`), so it got a small local `drain_capped` copy of the same
shape rather than a forced dependency change. `plugin-host/src/hooks.rs`'s old `collect_output`/`read_capped`
(the post-wait, cap-stops-reading version) were deleted outright as dead code once both its call sites moved
to `await_exit_draining`. Every fix in this finding was verified with the standard temporary-revert cycle:
`goal_claim.rs`'s new test failed against the un-fixed body with a real 5-second timeout observed, then
passed once the real fix was restored. Full suites: `process-supervisor` (105 tests, up from 102),
`plugin-host` (117 tests, unchanged — no new plugin-host-specific test since the mechanism is already covered
by `process-supervisor`'s own tests and the fix is a straight swap to the same audited primitive), `rapid`
(327 tests, up from 326). `cargo build --workspace --tests` passes.

**Same finding family, a 4th crate: `crates/mobile-sim` had the identical read-after-wait pipe-deadlock bug
in all four of its hand-rolled host-process runners, unfixed because the crate doesn't depend on
`process-supervisor` at all.** `android::manager::run_bounded`, `android::action::run_adb`,
`android::snapshot::run_adb`, and `ios::simctl::run_xcrun` were four independent, byte-for-byte identical
copies (a shared copy-paste ancestor, same shape as this session's earlier Seatbelt-backend-parity finding):
spawn with `stdout(Stdio::piped())`, poll `try_wait()` to completion, read stdout only inside the
`Ok(Some(status))` arm. `ios::simctl.rs`'s own doc comment anticipates the risk directly — `pub const
MAX_HOST_OUTPUT_BYTES: usize = 64 * 1024;` sits right below `/// Maximum \`simctl list\` stdout retained.`,
already above macOS's fixed 16 KiB pipe buffer. Worse in `android::action::run_adb`: `HostAdbUi::dump()`
calls it for `TypedAdbCommand::Screenshot`, building `adb ... exec-out screencap -p` — a raw PNG piped over
stdout, essentially always well over the pipe buffer, so real device screenshots would deadlock and time out
on virtually every call, not an edge case.

**Fixed with one new shared module, `crates/mobile-sim/src/host_process.rs`** (`pub(crate)`,
`run_bounded_capturing_stdout` + `HostRunError`), mirroring `process-supervisor::cancel::await_exit_draining`
locally since this crate doesn't depend on that crate: takes the child's stdout pipe and spawns a drain-to-
EOF reader thread *before* the `try_wait` poll loop starts, same "never stop draining early at the cap"
discipline as every other fix in this family (a reader that stopped at the cap would let the pipe fill again
past it and reintroduce the identical deadlock at a larger threshold — pinned down by
`host_process::tests::truncation_at_a_small_cap_still_drains_without_deadlock`, cap=64 bytes against a 200 KiB
`dd` write). All four call sites now route through this one function, each mapping its generic `HostRunError`
back to its own local error enum (preserving each site's exact existing variant choices, including
`action.rs`'s pre-existing `HierarchyBound` vs `Backend` distinction — `OutputTooLarge` maps to
`HierarchyBound` there specifically, `Backend` everywhere else, matching what each file already did).
Deleted the now-dead per-file `HOST_POLL` constants and unused `Read`/`Command`/`Stdio`/`Instant` imports
in three of the four files (the fourth, `ios::simctl.rs`, still uses `Instant`/`HOST_POLL` elsewhere).
`host_process.rs`'s own tests directly reproduce the mechanism with the same `/bin/dd if=/dev/zero bs=1024
count=200` approach used for the other three sites in this finding family:
`output_past_the_pipe_buffer_does_not_deadlock` confirms 200 KiB drains cleanly without a false timeout;
`nonzero_exit_is_reported`/`zero_timeout_is_rejected_before_spawning`/
`already_cancelled_token_is_rejected_before_spawning` cover the surrounding contract. No additional
site-specific regression test was added at each of the four call sites — the shared primitive is the thing
that was actually broken and is now directly, thoroughly tested, and all four sites are a mechanical,
verified-by-compilation wire-up to it (the same call made for `vcs::provenance`'s atomicity fix earlier this
session: a well-tested shared primitive doesn't need re-proving at every call site). Full `mobile-sim` crate
suite (105 tests, up from 99: 6 new in `host_process.rs`, zero removed — confirmed via `#[test]` counts
against the pre-fix commit) and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-08-30, a different systemic bug: seven `str.truncate(N)`-after-byte-length-check
sites across seven files in six crates can panic the whole process on a multi-byte character straddling the
cut, contradicting each site's own "Maximum UTF-8 bytes" doc comment.** `String::truncate(new_len)` panics
unless `new_len` is a char boundary — checking `s.len() > max` first does not make `s.truncate(max)` safe,
since `max` can itself fall in the middle of a multi-byte character. Every site here guards a text field taken
from external/model-controlled input (a diff body, a tool title, streamed model output, an external CLI
agent's stdout, a repair-feedback model/tool name, collected trajectory text, a telemetry attribute value)
where non-ASCII is entirely normal, not an edge case. Confirmed via direct repro before touching any fix code
— `let mut s = "a".repeat(65535); s.push('é'); s.truncate(65536);` panics with `assertion failed:
self.is_char_boundary(new_len)` — and via the standard temporary-revert cycle on one representative site
(`crates/acp/src/v1.rs::tool_title`): reverting to raw `title.truncate(MAX_TOOL_TITLE_BYTES)` made the new
test panic with that exact assertion, confirming the test genuinely catches the bug before the real fix was
restored.

**Fixed with the same small `truncate_to_char_boundary(s: &mut String, max: usize)` helper duplicated locally
in each affected file** (walks back from `max` to the nearest char boundary before calling `s.truncate`) —
duplicated rather than shared cross-crate since these six crates don't share a common low-level dependency for
this, the same call made for `crates/mobile-sim`'s local `drain_capped` copy earlier in this document. Sites
fixed, each with its own straddling-boundary regression test:
- `crates/acp/src/v1.rs`: `FileDiff::new`/`file_edit_update` (`MAX_DIFF_BYTES`), `tool_title`
  (`MAX_TOOL_TITLE_BYTES`), `text_from_payload` (`MAX_UPDATE_TEXT_BYTES`) — the highest-severity instance,
  since this is a JSON-RPC handler shared by every ACP session; one bad event (an oversized diff/title/model-
  output chunk with an unlucky UTF-8 boundary) would have killed the process for all sessions, not just the
  offending one. 5 new tests.
- `crates/security/src/scanners/external.rs::sanitize_text` (`MAX_REMEDIATION_BYTES`) — reachable because the
  char-accumulation loop preceding the truncate only checks its own, different byte cap (`MAX_MESSAGE_BYTES`)
  *before* pushing each char, so the loop can overshoot by up to 3 bytes past where the final truncate cuts.
  1 new test.
- `crates/tool-gateway/src/repair.rs`: `build_feedback` (model/tool name is caller-controlled, not a fixed
  literal) and `feedback` (currently only called with fixed literals, fixed anyway since it's a general-purpose
  private function). 1 new test on the reachable path.
- `crates/trajectory/src/lib.rs::TrajectoryCollector::record` (`MAX_TEXT_BYTES`) — `text` is arbitrary observed
  event content. 1 new test.
- `crates/telemetry/src/lib.rs::RedactionPipeline::redact_text` (`MAX_ATTRIBUTE_VALUE_BYTES`) — subtler than
  the others: the *original* text is already bound-checked before redaction, but canary substitution can
  **grow** the string (the `[REDACTED:secret:<16 hex>]` placeholder is longer than a short needle), pushing an
  untouched multi-byte character in the tail across the cap even though the pre-redaction text never came
  close to it. 1 new test.
- `apps/rapid/src/external_agents.rs::normalize_result` (`MAX_AGENT_RESULT_BYTES`) — `raw_text` is an external
  CLI agent's own stdout (already passed through `String::from_utf8_lossy`, which guarantees valid UTF-8
  overall but not that any given byte offset is a char boundary). 1 new test.
- `crates/tui/src/panels/agents.rs::sanitize_preview` (`MAX_AGENT_TEXT_BYTES`) — fixed defensively but **not
  currently reachable**: `MAX_PREVIEW_CHARS` (24) chars encode to at most 96 bytes, comfortably under
  `MAX_AGENT_TEXT_BYTES` (128), so the truncate can never actually fire today. Fixed anyway since it's a one-
  line change and removes a latent trap tied to two constants staying in this exact relationship — if either
  changes later without someone re-deriving this bound, the trap goes live silently. 1 new test (of the helper
  directly, since the call site itself isn't reachable).

Full suites, each up by exactly its one new test: `acp` (46, up from 41), `security` (169, up from 168),
`tui` (211, up from 210), `tool-gateway` (53, up from 52), `trajectory` (3, up from 2), `telemetry` (25, up
from 24), `rapid` (328, up from 327). `cargo build --workspace --tests` passes.

**Fresh review pass, 2026-08-30, `crates/context-engine/src/read.rs::slice_text` — a limit that can't admit
even the first considered line returns a continuation cursor that points back at the exact same line,
contradicting the module's entire reason for existing.** The doc comment at the top of the file (`read.rs:3`)
states: "Truncation always returns a structured continuation cursor and the reason." — the whole point of
`ReadCursor`/`next_line` (`read.rs:56`: "Resume point after a bounded read") is that a caller can retry with
`start_line = cursor.next_line()` and make forward progress. But `end_line` (the local variable the cursor's
`next_line` is computed from) is only advanced at `read.rs:439-442`, *after* the `max_bytes` check
(`read.rs:412-415`) and the `max_tokens` check (`read.rs:425-437`) — both of which can `break` the loop
before that update ever runs, specifically when the very first line considered in the call (`taken == 0`)
already exceeds the limit by itself (e.g. a single line longer than `max_bytes`, or one whose token estimate
alone exceeds `max_tokens`). When that happens, `end_line` is still at its pre-loop value
(`start_line.saturating_sub(1)`, `read.rs:379`), so `cursor.next_line = end_line + 1 = start_line` —
identical to the line just requested. A caller that follows the documented "resume point" contract (as
`read_repo`'s own cursor-to-`start_line` wiring at `read.rs:346-350` does) retries the exact same
`start_line` forever, gets the exact same empty-`text` response and identical cursor every time. Confirmed
directly with a reproduction test before touching any fix code: `ReadLimits::new().max_bytes(1)` against a
2-line file returns `cursor.next_line() == 1` (same as the requested `start_line`) — verified via the
standard temporary-revert cycle (reverting the `MaxBytes` branch's new one-line fix made the test fail with
exactly `left: 1, right: 2`, confirming the test catches the bug, before the fix was restored). **Fixed:**
when either break fires with `taken == 0`, also set `end_line = line_no` so the cursor skips past the single
line that couldn't fit rather than repeating it — the caller loses that one line's content (unavoidable under
the given limits) but the cursor now always makes monotonic forward progress. Applied the same guard to the
`LineWindow` break too (`taken >= max_lines` with `max_lines == 0`) for defensive completeness, even though
`validate_limits` (`read.rs:552-561`) already rejects `max_lines == 0` before `slice_text` ever runs, so that
specific path isn't reachable via the public `read_repo` entry point today — matches this session's
`crates/tui::sanitize_preview` precedent of fixing a currently-unreachable instance of the same shape rather
than leaving a latent trap. Two new tests, one per genuinely reachable reason (`MaxBytes`, `TokenBudget`).
**Latent, not yet actively firing:** confirmed via grep that `read_repo` has zero callers anywhere outside its
own tests — `apps/rapid`'s real `repo_read` tool (`apps/rapid/src/exec_tools.rs::execute_repo_read`) has its
own, entirely separate implementation and does not call this module at all, the same "fully-built, well-
tested, currently unwired" shape already documented repeatedly in this file (context-engine retrieval,
`crates/sandbox`, `crates/capability-broker`) — but a real, demonstrable bug in the module's own contract,
worth having fixed before anything wires a real `repo.read` continuation loop into it. Full `context-engine`
crate suite (310 tests, up from 308) and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-08-30, `apps/rapid/src/p9_commands.rs::read_bounded_file` — a stat-then-read TOCTOU
contradicting its own doc comment.** The doc comment reads: "Read a file whose size is checked before the
read so an oversized input is rejected without buffering it." The implementation was a plain `fs::metadata`
size check followed by a separate `fs::read` call — a file that grows between those two syscalls (a local
edit, a symlink swap) can pass the size check and still be fully buffered by the subsequent unconditional
`fs::read`, exactly contradicting "rejected without buffering it." Lower severity than the TOCTOU bugs fixed
earlier this session (`resolve_in_root`, `RetentionService::collect`, etc.): every caller passes a local,
CLI-operator-supplied path (plugin manifests, hook-spec documents, fixture events — not adversarial network
input), so the race window is narrow and the caller is generally trusted. Fixed anyway since it's a one-line-
shape change and directly contradicts a doc-commented guarantee. **Fixed:** replaced the stat-then-read pair
with a single bounded read — `File::open` then `.take(max_bytes + 1).read_to_end(&mut buf)`, rejecting only
after checking the buffer's actual length. This closes the TOCTOU gap (the size check and the read are no
longer two separate syscalls with a window between them) and also more faithfully satisfies the doc comment's
"without buffering it" intent: memory use is now capped at `max_bytes + 1` regardless of how large the file
actually is, rather than trusting a stale stat result. **Deliberately did not attempt a race-reproduction
test** — the same call made for this session's `vcs::provenance` atomicity fix: reliably growing a file in the
single-digit-millisecond window between two syscalls is not a race a test can land deterministically without
flakiness or a test-only pause hook. Added two boundary-behavior tests instead (`read_bounded_file_accepts_
at_the_limit_and_rejects_one_byte_over`, `read_bounded_file_never_buffers_past_the_cap_even_for_a_much_larger_
file`) — confirmed both also pass unchanged against the old stat-then-read code, precisely because they don't
exercise the race; the fix's TOCTOU-closing property rests on the read now being a single bounded operation,
verifiable by inspection rather than a race-dependent test. Full `rapid` crate suite (330 tests, up from 328)
and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-08-30, `crates/tui/src/panels/trace_jobs.rs::view_artifact` — on the last locally-
cached page, the returned cursor pointed back at the start of that page instead of past everything already
captured.** The doc comment reads: "Cursor the kernel ArtifactReader should use for the next remote page."
Both `Jobs`/`Traces` arms computed the offset as `page_offset(&row.excerpt_lines, self.selection.log_page)` —
the byte offset of the *start* of the currently selected local page — even when that page is the last one
locally cached and `more` is true because the backing artifact genuinely has more remote content
(`remote_more`, driven by `row.truncated` or `artifact.bytes > row.cursor`). `row.cursor` — the real "bytes
already captured from the backing artifact" tracker, already used correctly elsewhere in this file
(`view_logs`'s `page_from_lines` call) — was never consulted when building the `ArtifactCursor`. Confirmed
directly with the file's own fixture (`fixture_jobs()`: 40-line excerpt, 3 local pages of 16/16/8,
`row.cursor == 40`, `truncated == true`) via the standard temporary-revert cycle: selecting the last page (2)
and calling `view_artifact` returned offset `448` (the byte start of page 2) before the fix, `40` (the real
captured-cursor) after — the reverted version was confirmed to produce the wrong value first, then the fix
restored. A real kernel `ArtifactReader` handed offset 448 would re-serve bytes already rendered on screen
instead of the genuinely new remote content the `more: true` flag promises. **Fixed:** new `next_view_offset`
helper — while there is still a locally-cached page ahead, keeps returning `page_offset` (so re-deriving a
specific already-cached page's bytes stays consistent with what's on screen, preserving the existing test's
asserted behavior for non-terminal pages); once the selection is on the last cached page, returns `row.cursor`
instead. Scoped to the `Jobs` arm only, which is the only one with the underlying data: `SpanObservation`/
`SpanRowView` have no `cursor`/`truncated` fields at all (confirmed by struct definition, `trace_jobs.rs`
~lines 133-143 vs. 147-156), an asymmetry in the data model this fix does not attempt to correct — the
`Traces` arm still uses the original `page_offset`-only logic, left as a separate, more foundational gap
(spans can carry an artifact via `SpanObservation::with_artifact` but have no way to track how much of it has
been captured, so the "resume past the cache" case can't be computed correctly for spans without first adding
that tracking). New test `view_artifact_on_the_last_local_page_resumes_past_captured_bytes_not_the_page_start`
covers the `Jobs` case; the existing `viewing_uses_artifact_cursor_not_pid` test (which only exercised pages 0
and 1, never the terminal page) still passes unchanged, confirming the fix didn't disturb the non-terminal-
page behavior it already locked in. **Latent, not yet actively firing:** confirmed via grep that
`TraceJobsViewModel` has zero production callers anywhere in `apps/` — only re-exported from `crates/tui`'s
own `lib.rs` — the same "fully-built, unwired seam" shape documented repeatedly in this file, worth having
fixed before a real kernel-client consumer starts calling `view_artifact` to page through live remote logs.
Full `tui` crate suite (212 tests, up from 211) and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-08-30, `apps/rapid/src/p9_commands.rs::run_release_manifest` — a stale doc comment
claiming a security feature that was never built, corrected rather than rushed.** The doc comment claimed:
"build a release manifest with content digests, an HMAC signature over the digest list, and rollback recovery
fields; verification is fail-closed." The implementation genuinely computes real content digests
(`protocol::ArtifactId::from_bytes`) and emits rollback metadata, but never computes an HMAC, never emits a
`signature` field, and there is no `verify` subcommand or verification path anywhere — confirmed via
`grep -rn "signature\|hmac"` across the whole repo, which turns up nothing outside this one doc comment. A
manifest's `artifacts` list (path/digest pairs) is not tamper-evident at all today: anyone can edit a digest
or path in the emitted JSON with nothing to catch it. **Deliberately not implemented, not a quick fix:**
signing needs a real key-management decision first — a release signing key held by CI/maintainers is
structurally not something a local `rapid` binary distributed to end users can hold or check itself (unlike
`capability-broker`'s existing `hmac_sha256`/`LeaseIssuer::from_key`, which signs with a key the *caller*
already possesses at the point of use). Bolting on a plausible-looking HMAC using some locally-generated or
binary-embedded key would be actively worse than the honest current state: it would create the appearance of
tamper-evidence without a real answer to "who holds the verification key and how do they know it's the
legitimate one" — the same category of judgment call as this session's other documented not-attempted items
(`handoff`'s cross-process locking, the capability-broker audit trail's durable-store decision). **Fixed the
part that was actually safe to fix:** the doc comment now accurately describes what's implemented (digests +
rollback metadata) and explicitly states signing is unimplemented pending that key-management decision, so a
future reader auditing release-integrity guarantees isn't misled into believing tamper-evidence exists. Full
`rapid` crate suite unaffected (doc-only change) and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-08-30, `crates/agent-runtime/src/orchestration/supervisor.rs::check_budget` — 4 of
7 documented budget caps were silent no-ops; fixed the one genuinely self-contained dimension, documented the
other three as needing plumbing that doesn't exist in this Supervisor's model at all yet.**
`OrchestrationBudget`'s own doc comment: "Caps that force Blocked rather than an infinite repair loop." Only
`max_verification_rounds`, `max_repair_rounds`, and `max_strategist_calls` were ever actually checked.
`max_tokens` was checked but dead anyway — `tokens_used` (`OrchestrationSnapshot`) is initialized to `0` and
never incremented anywhere in the workspace (confirmed via grep). `max_agent_spawns` and `max_tool_executions`
were never checked at all, and — more fundamentally — `Supervisor` has no method anywhere that represents "an
agent was spawned" or "a tool was executed"; there's no event source to count in the first place, confirmed
via `grep -n "pub fn " supervisor.rs`. `max_wall_clock_ms` was likewise never checked, and
`OrchestrationSnapshot` had no timestamp field to check it against. This is real and reachable in production:
`apps/rapid/src/goal_claim.rs` constructs a live `OrchestrationBudget` (`max_wall_clock_ms: 600_000`, a real
10-minute cap evidently intended to bound the run) and drives the `Supervisor` through repeated `advance()`
calls, each of which calls `check_budget()` first — the configured 10-minute backstop was a complete no-op
regardless of how long the loop actually ran.

**Fixed the one dimension that's genuinely self-contained: `max_wall_clock_ms`.** Added `started_at: Instant`
directly on `Supervisor` (not on `OrchestrationSnapshot`, whose own doc comment — "Serializable orchestration
snapshot for resume" — signals intent to eventually persist it, and `Instant` can never survive that; putting
a non-resumable timestamp there would misrepresent what's actually resumable), set at both `start()` and
`resume()`, checked in `check_budget()` via `started_at.elapsed() > Duration::from_millis(max_wall_clock_ms)`.
**Deliberate, narrower limitation, stated in the field's own doc comment:** `resume()` restarts this window
rather than resuming the original run's elapsed wall-clock time, since `Instant` values from a prior process
don't survive a restart — full cross-process persistence would need an absolute wall-clock timestamp instead
and a decision about how that interacts with resume semantics, which this fix doesn't attempt. Verified via
the standard temporary-revert cycle: the new test failed with `Ok(Discovering)` instead of
`Err(BudgetExceeded)` against the reverted check, confirming it fails closed only because the real check ran.

**Deliberately not attempted, three dimensions, each needing real design/plumbing work, not a quick fix:**
- `max_agent_spawns` / `max_tool_executions`: `Supervisor` has no concept of either event in its current state
  machine at all — these aren't miscounted, there's nothing counting them because nothing in this crate emits
  such an event. Wiring this correctly needs a decision about where the count comes from: today's host-owned
  drivers (`goal_claim.rs`'s `HostPhases`) don't spawn agents or invoke arbitrary tools through this
  Supervisor at all (matches this document's own note: "Model-backed planner/implementer/verifier drivers do
  not exist yet, so every injected driver is host-owned"), so there's no real occurrence to count yet either.
- `max_tokens`: same shape — no LLM call currently happens inside this Supervisor's own round-driving methods
  (`run_checks`/`verify`/`repair`/`strategize` are all host-deterministic today), so there's no real token-
  usage event to report. Wiring this needs the same model-backed-driver work as the two above, not a small
  local fix.

Bolting on fake counters for these three (e.g., incrementing on every `advance()` call regardless of what
actually happened) would create the same false-safety-net problem as this document's `run_release_manifest`
finding just above: it would look like enforcement without actually bounding the thing the doc comment
promises to bound. New test `wall_clock_budget_forces_blocked_not_an_unbounded_run`. Full `agent-runtime`
crate suite (270 tests, up from 269) and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-08-30, `crates/protocol/src/error.rs::ApiError`'s `Deserialize` impl trusted a
forged `retryable` bit instead of deriving it from `code`.** The doc comment on `ApiError::new`:
"`retryable` follows [`ErrorCode::is_retryable`]." All three in-process constructors (`new`,
`new_with_details`, `from_unknown`) correctly set `retryable: code.is_retryable()` — but the fourth
construction path, `Deserialize` (the one actually used for wire/cross-process data, per this module's own
"Wire form matches the domain-model public error object"), took `retryable` straight off the wire with no
recomputation or cross-check against `code`. `ErrorCode::is_retryable()` is a deliberately small allowlist
("Conservative default: only clearly transient codes are retryable") that excludes `PolicyDenied` — a
fail-closed error a client should never auto-retry. Confirmed directly with the standard temporary-revert
cycle: `{"code":"policy.denied",...,"retryable":true,...}` decodes successfully and reports
`retryable() == true` against the un-fixed code (reverting the fix made the new test fail exactly as
predicted), even though `ErrorCode::PolicyDenied.is_retryable() == false` — a forged or stale peer response
could mark a policy-denial as safe to auto-retry, or the reverse (mark a genuinely transient error as
permanently failed), and nothing in the deserialization path would catch either direction. **Fixed:** the
wire `retryable` field is still required (unchanged schema — `missing_field` still fires if it's absent, so
no wire-format break), but its *value* is discarded and replaced with `code.is_retryable()` after `code`
itself is parsed — the same derivation the three in-process constructors already use, now applied uniformly
on all four paths. New test `deserialize_never_trusts_a_forged_retryable_bit` covers both forgery directions
(a non-retryable code force-marked retryable, and a retryable code force-marked non-retryable). Full
`protocol` crate suite (71 tests, up from 70) and `cargo build --workspace --tests` pass — significant given
`protocol` is a dependency of nearly every other crate in the workspace.

**Fresh review pass, 2026-08-30, `crates/handoff/src/lib.rs` — the module's own "who is expected to accept"
and single-writer invariants were never actually checked in `accept()`/`detach()`.** Two bugs in the same
"one identity field validated, its sibling identity field silently trusted" family already documented
earlier in this file (this crate's *other*, separately-documented finding — the cross-process TOCTOU in
`acquire()`/`accept()` needing real file locking — is a different, distinct issue; both remain latent, this
crate still has zero callers anywhere in the repo, confirmed via grep).

**Bug 1: `accept()` never checked the caller-supplied `new_owner` against `bundle.new_owner`.**
`HandoffBundle::new_owner`'s own field doc: "Who is expected to accept." But `accept()` took a `new_owner: &str`
parameter, validated only that it was a *well-formed* owner string (`valid_owner`), and used it directly to
construct the new `ExecutionOwnership` — never comparing it to `bundle.new_owner` at all. Since `detach()`
clears the ledger before the real acceptor calls `accept()`, any caller who obtains the bundle can call
`accept(&ledger, &bundle, "attacker")` in that empty-ledger window and take ownership under an arbitrary
string, contradicting "who is expected to accept" and this crate's "single-writer invariant" framing.
**Fixed:** added `if new_owner != bundle.new_owner { return Err(InvalidOwner) }`, placed *after* the existing
`AlreadyOwned` check (not before) specifically to preserve an existing test's precedence expectation — when
a slot is already occupied, that's reported as `AlreadyOwned` regardless of which wrong owner string the
caller also happened to supply; ordering doesn't affect the security property either way (both checks must
still pass), only which error variant surfaces when multiple problems apply at once.

**Bug 2: `detach()` never checked the caller-supplied `session_id` against the ledger's actual owned session.**
`detach()` validated `ownership.owner != previous_owner` but never `ownership.session_id != session_id` —
the caller-supplied `session_id` flowed straight into the returned `HandoffBundle` unchecked. A caller that
supplies the correct `previous_owner` string but an unrelated `session_id` got back a bundle whose
`session_id` never matched the session whose ownership record was actually validated. **Fixed:** added
`if ownership.session_id != session_id { return Err(InvalidOwner) }` right alongside the existing owner
check.

Both verified with the standard temporary-revert cycle: reverting each check independently made its
corresponding new test (`accept_rejects_a_caller_that_is_not_the_bundles_expected_new_owner`,
`detach_rejects_a_session_id_that_does_not_match_the_owned_session`) fail with exactly the predicted
assertion, confirming each test genuinely catches its bug, before both fixes were restored. Full `handoff`
crate suite (10 tests, up from 8) and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-08-30, `apps/rapid/src/context_retrieval.rs::retrieve_inner` leaked its timeout-
watcher thread on every error path.** `TimeoutWatcher`'s own doc comment: "Fires `cancel.cancel()` after
`timeout` unless `stop()` is called first." `retrieve_inner` spawned the watcher at its own top, then had
four `?`-early-return points (`build_manifest`, `IndexPipeline::open`, `InformationNeed::new`, `scout`)
before its only `watcher.stop()` call, which sat right before the final `Ok(blocks)` — any of those four
failing skipped `stop()` entirely, leaving the spawned thread sleeping in 20ms increments for up to the full
`RETRIEVAL_TIMEOUT` (8 seconds) before firing `cancel.cancel()` into an already-abandoned token. Reachable via
the module's own existing test scenario: `retrieve(root, ...)` on a nonexistent root trips `build_manifest`'s
`?` immediately, so every failed retrieval (broken workspace root, index-open failure, malformed need, scout
failure) leaked a live thread for up to 8 seconds — in a tight loop of repeated `rapid exec` turns against a
workspace whose indexing keeps failing, these could pile up concurrently before self-terminating. The sibling
function one block below in the same file, `ripple_advisory`, already gets this right: it spawns the watcher
in the outer function, calls the fallible `_inner` version, then calls `watcher.stop()` unconditionally
*before* branching on the result — `retrieve_inner` just never followed its own neighbor's pattern. **Fixed:**
restructured `retrieve()`/`retrieve_inner` to match `ripple_advisory`'s exact shape — spawn the watcher and
call `retrieve_inner` (now taking `cancel: &CancellationToken` as a parameter instead of owning it) from the
outer `retrieve()`, call `watcher.stop()` unconditionally on the very next line before matching on the
`Result`, so there is no longer any early-return point between spawning the watcher and stopping it. New test
`timeout_watcher_is_stopped_even_when_retrieve_inner_errors_early` directly exercises the watcher/stop
mechanism (spawn with a short test timeout, call `retrieve_inner` against a nonexistent root, call `stop()`,
then sleep past the timeout and confirm `cancel` was never tripped) — it can't observe `retrieve()`'s own
internal thread lifecycle from outside the deliberately black-box public API (`Vec`/`Option` return, no
thread handle exposed), so `retrieve()`'s own "no early-return between spawn and stop" guarantee is verified
by inspection instead, the same discipline used for a few other hard-to-black-box-test fixes this session —
the diff shows a straight-line `let result = retrieve_inner(...); watcher.stop(); match result { ... }` with
no `?` in between, mirroring `ripple_advisory`'s already-trusted structure exactly. Full `rapid` crate suite
(331 tests, up from 330) and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-08-30, `crates/agent-runtime/src/agent/spawn.rs::spawn_agent` — the scheduler
cleanup pattern used for two error paths was silently skipped for four others in between.** The function
consistently rolls back the scheduler's record on failure at the start (`Spawned`-emit failure: `scheduler.
cancel(child_id)`) and at the end (turn-execution `Cancelled`/`Failed` arms: `scheduler.cancel`/`finish`) —
but the stretch in between (`check_cancel`, `scheduler.start()`, two `agent.transition()` calls, the
`Started`-emit) used bare `?` with no equivalent cleanup, even though by the time any of these can fail the
scheduler record has already moved past `Queued` (via `enqueue()`) or `Running` (via `start()`), with no
other path back to a terminal state. Confirmed reachable, not just a contrived mock: the built-in
`SpawnEventSink for Vec<SpawnEvent>` is bounded at `MAX_SPAWN_EVENTS = 32` and fails once already holding 32
— a shared lifecycle sink naturally accumulates 2 events per successful spawn, so the 17th spawn in a session
fills it to exactly 32 on its own `Spawned` emit, then its `Started` emit fails at precisely the point cleanup
was skipped. Verified directly with the standard temporary-revert cycle: reverting just the `Started`-emit
error handling back to bare `?` made the new test fail with `left: Running, right: Cancelled` — the
scheduler record left stuck at `Running` forever, no terminal state ever recorded, exactly as predicted —
before the fix was restored. **Fixed:** added a `cancel_on_err` closure (`|err| { scheduler.cancel(child_id);
err }`) and applied `.map_err(cancel_on_err)` uniformly across all four previously-bare `?` points, matching
the same cleanup the function already performs at its other two failure sites. New test `started_event_
failure_still_cancels_the_scheduler_record` (a `RejectSecondEventSink` that accepts `Spawned` but rejects
`Started`, confirming the scheduler record ends up `Cancelled` rather than stuck at `Running`). **Latent, not
yet actively firing:** confirmed via grep that `spawn_agent` has zero callers anywhere in `apps/` — this
matches the session's own earlier note that "there is no write-scoped, narrowly-leased child run path at all"
wired into `task_spawn` yet — but a real, demonstrable resource-leak bug in the function's own error handling,
worth having fixed before this becomes the real subagent-spawn path. Full `agent-runtime` crate suite (271
tests, up from 270) and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-08-30, `crates/agent-runtime/src/agent/result.rs::ResultStore::complete` — a
genuine concurrent race could strand a completed child agent with no stored result and no way to retry.**
The file's own header claims "`complete_agent` records the typed summary, evidence, and view." `complete`
checked the duplicate/bound conditions under one lock acquisition, released the lock, then called
`agent.complete(result, cancel)` — an **irreversible** mutation (`Agent::complete` sets a terminal
`AgentState`, and `validate_transition` rejects any transition once `from.is_terminal()`, so a terminal agent
can never be completed again) — before re-acquiring the lock and re-checking the *same* conditions right
before the insert. Two threads racing to complete two *different* children against a shared, bounded
`ResultStore` could both pass the first (pre-mutation) check before either inserted, both call
`agent.complete()` (both now terminal), and then only one would win the second check and actually get
inserted — the loser returns `Err(BoundExceeded)` with its agent permanently `Succeeded`/`Failed` but no
corresponding record in the store: `inspect_result` returns `AgentNotFound` forever, and retrying `complete()`
now fails with `InvalidTransition` since the agent is already terminal. Confirmed directly with a genuine
2-thread test (not a synthetic single-threaded sequence — a sequential call never lands in the race window,
since the *first* call's own pre-check already sees the store full before any mutation happens, which is
exactly why the existing `store_bound_and_shared_view_fail_closed` test never caught this) and the standard
temporary-revert cycle: run 20 times against the reverted two-separate-locks code, the new test failed ~75%
of the time with the loser's agent left `Succeeded` instead of `Running`; restored the fix and reran 20
times, passed every time — deterministic now, since the race window no longer exists. **Fixed:** merged the
two separately-acquired lock blocks into one critical section that holds the lock across the whole
check → `agent.complete()` → insert sequence, so a losing thread now fails the bound/duplicate check *before*
ever calling `agent.complete()`, leaving its agent's state untouched and safely retryable. New test
`concurrent_completions_never_strand_the_losing_agent_when_the_store_is_full`. **Latent, not yet actively
firing:** confirmed via grep that `ResultStore`/`complete_agent` have zero callers anywhere in `apps/` —
same not-yet-wired-in subagent-spawn subsystem as the `spawn.rs::spawn_agent` finding just above, and the
same reasoning applies: a real, demonstrable bug worth having fixed before this becomes the live child-
completion path. Full `agent-runtime` crate suite (272 tests, up from 271) and `cargo build --workspace
--tests` pass.

**Fresh review pass, 2026-08-30, `crates/agent-runtime/src/specialist.rs::PersistentSpecialist::deliver` —
the sibling path to `mark_observed` had no generation gate at all.** `mark_observed`'s own doc comment: "A
generation change invalidates observations from older generations so a specialist never serves stale
repository state." That guarantee is enforced entirely by `mark_observed`'s `retain()` call, which sweeps the
mailbox using `message_generation_is_current`. But `deliver()` — the only other function that touches the
mailbox — accepted every message unconditionally, with no check against `self.observed_generation` at all.
Concretely: `mark_observed(5)` then `deliver(Observe { generation: 2, .. })` succeeded and queued the stale
message, which `pop_mail()` would hand back exactly as if it were current — directly contradicting "a
specialist never serves stale repository state." A second symptom followed from the same gap: since
`mark_observed`'s `retain()` only runs when the generation argument *changes* (`if generation !=
self.observed_generation`), a redundant re-confirmation at the same generation skipped the sweep entirely,
though this specific symptom becomes moot once `deliver()` itself gates on entry (below), since nothing stale
can get in for a same-generation re-confirmation to need sweeping. **Fixed:** `deliver()` now applies the
same `message_generation_is_current(self.observed_generation)` predicate `mark_observed` already uses, and
silently drops (returns `Ok(())`, not an error) a message that's already stale at delivery time — matching
`retain()`'s own framing of staleness as invalidation, not a caller error. Verified with the standard
temporary-revert cycle: reverting the new gate made the test fail with the mailbox containing the stale
message, restoring it passes. New test `deliver_drops_a_message_already_stale_at_delivery_time`. **Latent,
not yet actively firing:** confirmed via grep that `PersistentSpecialist`/`SpecialistPool` have zero callers
anywhere outside this file's own tests — this control-plane component isn't wired into a host yet — but a
real, demonstrable contract violation in the public API against its own documented invariant, worth having
fixed before it is. Full `agent-runtime` crate suite (273 tests, up from 272) and `cargo build --workspace
--tests` pass.

**Fresh review pass, 2026-08-31 — the standing autonomous loop hit an account-wide weekly rate limit on
background review subagents mid-cycle; this and the following findings were investigated directly rather
than via a dispatched agent, same verification discipline throughout.** `crates/agent-runtime/src/agent_defs.
rs::load_directory` had the exact stat-then-read TOCTOU shape already fixed once this session in
`apps/rapid/src/p9_commands.rs::read_bounded_file`, in a different module: `MAX_DEF_FILE_BYTES`'s own doc
comment ("Maximum UTF-8 bytes in a definition file") was enforced by checking `meta.len()` from a
`symlink_metadata` call taken *before* `fs::read_to_string(&path)` actually read the file — a file that grows
between the two syscalls could pass the size check and still be fully buffered by the unconditional read.
Unlike several other findings this session, this one is **live and reachable today**: confirmed via grep that
`apps/rapid/src/p9_commands.rs` calls `agent_runtime::agent_defs::{full_inventory, load_directory}` directly
from a real CLI command that scans `<project>/.rapidlm/agents/*.toml`. **Fixed** the same way as the earlier
instance: replaced the stat-then-read pair with `File::open` + `.take(MAX_DEF_FILE_BYTES + 1).
read_to_string(&mut buf)`, checking the actual buffered length instead of a stale stat result — caps memory
at `MAX_DEF_FILE_BYTES + 1` regardless of the file's real size and closes the TOCTOU gap in the same step.
New test `load_directory_rejects_a_file_over_the_byte_cap_without_buffering_it_in_full` (confirmed, via the
standard temporary-revert cycle, that it also passes against the un-fixed stat-then-read code — it doesn't
exercise the race itself, the same testability ceiling documented for the original `read_bounded_file` fix;
the TOCTOU-closing property rests on the read now being a single bounded operation, verifiable by inspection).
Full `agent-runtime` crate suite (274 tests, up from 273) and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-08-31, `apps/rapid/src/web_fetch.rs::is_private_ip` — the IPv6 arm was missing the
link-local check its own IPv4 sibling has, a real SSRF bypass in the live, model-callable `web_fetch` tool.**
The module's own doc comment: "loopback/private/link-local are refused by default." The `IpAddr::V4` arm
correctly checks `is_loopback() || is_private() || is_link_local() || is_unspecified() || is_broadcast()`, but
the `IpAddr::V6` arm only checked `is_loopback() || is_unspecified() || <manual fc00::/7 ULA bitmask>` —
`fe80::/10` (IPv6 link-local, the direct sibling of the IPv4 check `169.254.0.0/16` catches) was never
checked at all. Confirmed and reproduced standalone before touching any fix code:
`is_private_ip("fe80::1".parse().unwrap())` returns `false` against the un-fixed code, and
`classify_fetch("http://[fe80::1]/x", &[])` returns `Ok(())` — the SSRF guard would let the live `web_fetch`
tool (confirmed via `apps/rapid/src/exec_tools.rs::WEB_FETCH_TOOL` → `execute_web_fetch` →
`crate::web_fetch::fetch_page` → `classify_fetch`, one of the 15 real model-callable tools) fetch an
IPv6 link-local address with no refusal, exactly the class of address the module doc promises is blocked by
default. Verified via the standard temporary-revert cycle: the new test failed with `unwrap_err()` called on
an `Ok` value against the reverted code, confirming the test genuinely catches the gap, before the fix was
restored. **Fixed:** added `v6.is_unicast_link_local()` (the direct IPv6 counterpart to `Ipv4Addr::
is_link_local()`) to the V6 match arm. New test `classify_refuses_ipv6_link_local_without_allowlist`. This is
the highest-severity, most clearly *live* finding from this document's stat-then-read/gate-asymmetry sweep —
not a latent, unwired module, but an actively-shipped security guard on a real tool a model can call today.
Full `rapid` crate suite (332 tests, up from 331) and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-09-01, `crates/plugin-host/src/wasm.rs::PluginInstance::call_inner` — the
`max_stack_frames` sandbox limit never actually bounded guest recursion, because the only check comparing
against it counted the wrong thing.** The module's own framing: "Capability-isolated... explicit fuel,
wall-clock, and memory bounds," backed by `ResourceLimits::max_stack_frames` / `HARD_MAX_STACK_FRAMES` and a
dedicated `WasmError::StackOverflow` variant that implies guest recursion fails closed once it's exceeded. The
`Op::Call` handler instead checked `labels.len() + 1 > self.limits.max_stack_frames` — `labels` is the
Block/Loop/If structured-control-nesting stack local to one `call_inner` activation, freshly (re-)declared
`Vec::new()` on *every* call, including the recursive one, so it can never reflect depth accumulated across
calls. A guest function that does nothing but call itself (`call $self` with no surrounding block/loop/if)
keeps `labels` permanently empty at the check site — the condition `0 + 1 > max_stack_frames` is never true, no
matter how deep the real native recursion goes — so the interpreter recurses on the actual OS thread stack
until it exhausts it: an uncatchable process abort (`SIGABRT` via Rust's stack-overflow guard-page handler),
not a contained `Trap`, directly defeating the sandbox's own stated purpose of bounding untrusted guest
execution. Reproduced concretely before touching any fix code: a minimal hand-built WASM module (single
exported function, body = `call 0; end`, i.e. it calls itself) run under `tight_limits()`
(`max_stack_frames: 32`) crashed the entire test binary — `thread ... has overflowed its stack` / `fatal
runtime error: stack overflow, aborting` / `signal: 6, SIGABRT` — confirming the check never fires for this
shape of recursion. **Fixed:** threaded a real `depth: usize` parameter through `call_inner` (incremented by
`call_inner`'s own recursive call site, seeded at `0` from `run_func`'s outer call) and changed the `Op::Call`
guard to `if depth + 1 > self.limits.max_stack_frames { return Err(WasmError::StackOverflow) }` — a counter
that actually reflects native call-stack depth across activations, unlike `labels`. New test
`bare_self_recursion_is_stopped_by_max_stack_frames`, verified via the standard temporary-revert cycle: with
the guard reverted back to `labels.len() + 1`, the test run genuinely reproduces the crash described above
(`cargo test` process aborts with `SIGABRT`, not a graceful test failure); restoring the fix makes the same
test pass cleanly (`Err(WasmError::StackOverflow)`) in under a millisecond. Full `plugin-host` crate suite
(118 tests, up from 117) and `cargo build --workspace --tests` pass. **Latent, not yet actively firing:**
confirmed via `grep -rln "PluginInstance::\|wasm::.*::call\b\|plugin_host::wasm" apps/` (zero matches) that
this WASM execution path has no caller anywhere in `apps/rapid` today — the same "mechanism is broken but not
yet wired into a real command" shape this document has already found repeatedly (§0a's `crates/sandbox`/
`crates/capability-broker` notes above) — but a genuine, reproducible denial-of-service against the *host*
process (not just the guest) in a component whose entire purpose is running untrusted code safely, worth
having fixed before anything wires a real plugin-install command up to it.

**Fresh review pass, 2026-09-02, `crates/sandbox/src/backends/seatbelt.rs::read_capped` — the only one of
this crate's four near-identical `read_capped` helpers that doesn't drain the pipe past the output cap.**
`host_restricted.rs:889-912`, `container.rs`, and `gvisor.rs` all implement the same "cap the buffered bytes,
then keep reading (and discarding) until EOF" shape once the cap is hit — `seatbelt.rs`'s version instead
`return`ed the moment `buf.len() >= cap`, dropping `pipe` (the child's `ChildStdout`/`ChildStderr`) immediately
and never reading anything the peer still had queued. Reproduced directly, without any sandbox/process
machinery, by unit-testing the free function itself: a `UnixStream::pair()` with one side spawned to
`write_all` a 4MiB payload (chosen to exceed both the 4096-byte cap and any realistic OS socket buffer) while
the other side is handed to `read_capped`. Against the unfixed code, `read_capped` returns after the first
~8KB read, dropping the reader half of the pair — the writer thread's still-in-flight `write_all` immediately
fails with `Os { code: 32, kind: BrokenPipe, .. }` instead of completing, confirmed by running the new test
against the reverted code before restoring the fix. (The originally-hypothesized failure mode for the
equivalent real-child-process case was an infinite `write(2)` block rather than a broken pipe — not
independently re-verified here since it depends on pipe-vs-socket and signal-disposition specifics the test
doesn't need to settle; either way, the underlying defect is the same: the reader stops servicing the pipe
while the peer still has output queued, and normal completion breaks.) **Fixed:** ported the same
drain-to-EOF loop the three sibling backends already use. New test
`read_capped_drains_the_pipe_past_the_cap_so_the_writer_never_blocks`. Full `sandbox` crate suite (98 tests,
up from 97) and `cargo build --workspace --tests` pass. **Latent, not yet actively firing:** confirmed via
`grep -rln "SeatbeltBackend"` that this backend has zero callers outside the crate's own tests — its own
module doc says explicitly that wiring `apps/rapid` to prefer it over the existing job-based Seatbelt path in
`apps/rapid/src/exec_tools.rs` is separate follow-up work not attempted here — but a real, demonstrable defect
in output handling worth having fixed before that wiring happens. (Found via a background review agent
targeting `crates/sandbox` — a crate not previously reviewed this session — dispatched with the same
recurring-bug-shape checklist used throughout this document; the same pass also flagged the
`require_proc_lease` gap fixed in the next entry below, and a follow-up deep pass on `remote.rs` separately
flagged a self-referential worker-identity check in `RemoteWorkLease::verify`'s only call site, not yet acted
on as of this entry.)

**Fresh review pass, 2026-09-02, `crates/sandbox/src/backends/remote.rs::require_proc_lease` — the sole
backend, of five, that never checked a `CapabilityLease`'s expiry or remaining-uses before honoring it.**
The identically-named, identically-purposed helper in all four sibling backends
(`host_restricted.rs:1146-1154`, `container.rs`, `gvisor.rs`, `seatbelt.rs`) checks
`lease.is_expired(Instant::now()) || lease.remaining_uses() == 0` in addition to the capability-type match;
`remote.rs`'s version checked only `lease.capability() != Capability::ProcExec`, silently accepting a lease
whose authorization window had already closed. `RemoteBackend` is also, per `IsolationStrength::of_tier`, the
*strongest* isolation tier (`SandboxTier::RemoteWorker`/`MicroVm`) of the five — the one exception is on the
highest-privilege path, not a lower one. Note `remote.rs` separately tracks its own `RemoteWorkLease`'s expiry
(`spec.expires_at_unix_ms`) — that gates the remote-worker *assignment* `prepare()` mints, a distinct object
from the caller-supplied `CapabilityLease` authorization grant this fix addresses; a fresh `RemoteWorkLease`
can be minted from a `CapabilityLease` that is itself already expired. Reproduced via the standard
temporary-revert cycle without any real waiting: `capability_broker::lease::issue`'s own signature takes an
explicit `now: Instant` parameter (the same injection point `capability-broker`'s own
`expired_lease_is_rejected` test uses), so a lease built with `now = Instant::now() - 120s` has an
already-past `expires_at` (default TTL is a fixed 60s, confirmed via `LeaseConstraints::DEFAULT_MAX_TTL_SECS`
— not independently overridable through the policy TOML schema, so this injection is the only fast way to get
a genuinely expired lease here) — `backend.prepare()` with this lease succeeded against the reverted code
(panicking the test with a live `SandboxHandle` instead of the expected error) and correctly returned
`SandboxError::LeaseInvalid` once the fix was restored. **Fixed:** added the same
`is_expired`/`remaining_uses` check the other four backends already use. New test
`require_proc_lease_rejects_an_already_expired_lease`. (The `remaining_uses() == 0` half is, by inspection of
`capability_broker::lease`, unreachable through the real issuance path today — `remaining_uses` is a fixed
snapshot of `constraints.max_uses()` set once at `issue()` and never decremented on the `CapabilityLease`
object itself, and `issue()` itself rejects `max_uses() == 0` — but it's added for parity with the other four
backends' identical check and as a defensive backstop against a future lease representation that does
decrement it.) Full `sandbox` crate suite (99 tests, up from 98) and `cargo build --workspace --tests` pass.
**Latent, not yet actively firing:** confirmed via `grep -rln "RemoteBackend\|SandboxTier::RemoteWorker"
apps/` (zero matches) that this backend is exercised only by its own crate's tests today; `apps/rapid/src/
sandbox_exec.rs` registers only `HostRestrictedBackend`.

**Fresh review pass, 2026-09-02, `crates/sandbox/src/backends/remote.rs::RemoteBackend::exec` — the
worker-identity half of `RemoteWorkLease::verify` was a tautology at its only call site.** `verify`'s own
parameter shape (`controller_id: ControllerId, worker_id: WorkerId`) exists to check the lease's embedded
identity against an independently-known "expected" identity — the `controller_id` side of the same call
already does this correctly, passing `self.controller_id` (the backend's own authoritative field). But the
`worker_id` argument was `work_lease.worker_id` — the lease's own field, read off the very value being
checked and handed right back to itself. `self.worker_id != worker_id` inside `verify` (lease.rs:837)
collapses to `work_lease.worker_id != work_lease.worker_id`, unconditionally `false`; the check could never
fail regardless of which worker is actually attached to the backend when `exec()` runs. Confirmed via the
standard temporary-revert cycle: with a real, executable reproduction — `prepare()` a sandbox against
`ready_profile()`, `attach_profile(other_ready_profile())` to swap in a different `WorkerId` before calling
`exec()` on the same handle — the reverted code's `exec()` still reached its final `Err(SandboxError::
HealthFailed)` fallthrough line (the module's own "no real Firecracker transport" stub) because `verify()`
silently passed despite the worker swap, and the test failed asserting `HealthFailed == LeaseInvalid`;
restoring the fix made `verify()` correctly reject with `LeaseInvalid` before ever reaching that fallthrough.
**Fixed:** replaced `work_lease.worker_id` with `self.profile.as_ref().ok_or(TierUnavailable)?.id` — the
backend's actual currently-attached worker, the same field `prepare()` itself uses when first minting the
work lease. New test `exec_rejects_a_lease_bound_to_a_worker_no_longer_attached`. Full `sandbox` crate suite
(100 tests, up from 99) and `cargo build --workspace --tests` pass. **Currently masked, not latent in the
usual "unwired" sense:** this bug lives inside the same `RemoteBackend`/`SandboxTier::RemoteWorker` path the
entry above already establishes has zero callers under `apps/` — but unlike that entry, even the crate's own
`exec()` can't observe the consequence yet, since the very next line unconditionally fails closed with
`HealthFailed` before any real dispatch happens. The check was dead code with no externally visible effect
today, and would have stayed silently broken the moment a real Firecracker dispatch got wired in after that
stub — worth fixing now while it's cheap and isolated, rather than after that wiring lands. (Found by the same
background review agent's deep follow-up pass on `remote.rs`, dispatched after its first pass flagged the
`require_proc_lease` gap fixed in the entry above; that same follow-up pass also flagged a lower-confidence,
architectural note — `RemoteSandboxSpec::from_sandbox_spec`'s "required attestations" and `issue_work_lease`'s
"presented attestations" are both read from the same `self.profile.attestations` field with nothing external
in between, making `verify_required_attestations` tautological along this one wiring too, though the
verification machinery itself is not broken — not acted on this pass; flagging here so it isn't
independently rediscovered.)

**Fresh review pass, 2026-09-02, `crates/vcs/src/provenance.rs::PatchAttribution` — `symbols` had no bound of
its own, unlike its sibling field `evidence`, letting it silently overflow the lineage read API's truncation
cap with no error signal.** `record_patch_attribution` (line 594) checks `attribution.evidence.len() >
MAX_EVIDENCE_REFS` (32) and rejects with `BoundExceeded`, but had no equivalent check for `attribution.
symbols`, the only other `Vec`-shaped field on `PatchAttribution` (every other field is `Option<T>`, capped at
one). `MAX_LINEAGE_NODES` (64) bounds how many nodes of one kind `lineage_for_patch`'s `push_unique` helper
will collect before silently dropping the rest with no error — `evidence`'s 32-cap keeps it safely under that
ceiling, but nothing kept `symbols` under it. Concretely: a `PatchAttribution` built with 70 `with_symbols(...)`
entries (a plausible large refactor touching 70 functions) is accepted by `record_patch_attribution` and all
70 `ModifiesSymbol` edges are correctly appended to the graph — but `lineage_for_patch`, the crate's own
documented "walk patch edges" read API, silently returns only 64 of them, with no truncation flag, giving a
caller an incomplete answer for a query the API's own doc comment ("Walk patch... edges back to agent and
evidence") frames as authoritative. Verified via the standard temporary-revert cycle: the new test failed with
`too many symbol refs: 0` (i.e. no error at all) against the reverted code, confirming the gap, before the fix
was restored. **Fixed:** added `MAX_PATCH_SYMBOLS: usize = 32` (mirroring `MAX_EVIDENCE_REFS`'s value and
framing) and the matching bound check in `record_patch_attribution`. New test `symbol_ref_bound`. Full `vcs`
crate suite (17 tests, up from 16) and `cargo build --workspace --tests` pass. **Latent, not yet actively
firing:** confirmed via `grep -rln "vcs::" .` (outside `crates/vcs`) that this crate has zero callers anywhere
in the workspace, including `apps/rapid` — the same "mechanism is broken but not yet wired into a real
command" shape this document keeps finding — but a genuine logic defect in the crate's own primary read API,
worth having fixed before anything depends on `lineage_for_patch` for a complete answer. (Found by a
background review agent doing this session's first pass over `crates/vcs`; the same pass also flagged that
`RecordedAt`'s derived `Ord`/`PartialOrd` — plain byte-wise comparison of the wrapped RFC3339 string — is not
chronologically correct once fractional-second timestamps are compared against whole-second ones, e.g.
`"2026-08-15T12:00:00.500000000Z" < "2026-08-15T12:00:00Z"` under derived `Ord` despite being half a second
*later*; confirmed by direct string comparison, not currently exercised by any sort/`BTreeSet`/`.cmp()` call
anywhere in the crate or its callers, so left undocumented as a known caveat rather than fixed this pass — a
real fix needs a parsed-timestamp representation, not a bigger change than this entry's scope.)

**Fresh review pass, 2026-09-02 — a background review agent's `crates/event-ledger` finding was investigated
and found to be a false positive; corrected here rather than silently dropped.** The agent flagged
`CronStore::claim_due` (`crates/event-ledger/src/cron.rs`, then lines 311-312) as the sole method opening a
bare `Connection::open(&self.path)` instead of going through `self.connect()`, which — every other method in
the file does — calls `MigrationRunner::apply`, which sets `conn.busy_timeout(BUSY_TIMEOUT)` (5000ms). The
claimed consequence: two concurrent `rapid cron poll` invocations racing for the same `BEGIN IMMEDIATE`
transaction would have the loser fail immediately with `SQLITE_BUSY` instead of waiting out the winner,
contradicting `claim_due`'s own doc comment ("two concurrent pollers can never claim the same row"). This is
a real, live, reachable path (`apps/rapid/src/p9_commands.rs`'s `run_cron`'s `"poll"` arm →
`scheduler::PromptCron::poll` → `CronStore::claim_due`), so it was investigated directly rather than deferred.
**Investigation found the premise wrong:** a diagnostic test (two `rusqlite::Connection::open` handles against
the same file, one holding an uncommitted `BEGIN IMMEDIATE`, timing the second's `BEGIN IMMEDIATE` attempt)
measured a ~5.2s wait before `SQLITE_BUSY`, not an immediate failure — and grepping this workspace's pinned
rusqlite source directly (`~/.cargo/registry/src/.../rusqlite-0.32.1/src/inner_connection.rs:119`) confirms why:
`InnerConnection::open_with_flags` unconditionally calls `ffi::sqlite3_busy_timeout(db, 5000)` on every
connection `Connection::open` creates, regardless of any application-level `.busy_timeout()` call — the exact
same 5000ms `connect()`'s explicit call also sets. `claim_due`'s bare `Connection::open()` already had the
identical busy-wait behavior every sibling method has; the gap the agent found is real (one method skips an
explicit call the others make) but has no behavioral effect, because rusqlite's own default already closes it.
**Action taken:** added a code comment at the call site recording this (so a future reader doesn't rediscover
and "fix" the same non-bug), added a regression test `claim_due_waits_for_a_concurrent_writer_instead_of_failing_busy`
that exercises the real, still-worth-protecting invariant directly (a concurrent writer holding the lock
doesn't cause `claim_due` to fail busy) since it's a genuinely important behavior for a live, concurrently-called
API regardless of which mechanism guarantees it — but did **not** change `claim_due` to call `self.connect()`,
since there was no bug to fix and swapping it in would have been change for its own sake. Full `event-ledger`
crate suite (88 tests, up from 87) and `cargo build --workspace --tests` pass. This is the session's own
established practice applied to a rare case where it was needed: verify every finding, including ones a
background agent already framed with citations and a plausible mechanism, before writing a fix — this one
looked exactly like the session's other confirmed "sibling method skips a check" bugs until direct measurement
disproved the mechanism.

**Fresh review pass, 2026-09-02 — `crates/capability-broker` got a direct internal-correctness review (its
lease/policy/approval API had only been read piecemeal via caller-side bugs found earlier this session); it
came back clean.** Every stated invariant was traced against the code and the crate's own 137-plus-adversarial
test suite (all passing): the MAC comparison (`ct_eq`) is genuinely non-short-circuiting, `LeaseValidator::
validate_use` performs its check-decrement-write entirely inside one lock acquisition (no TOCTOU), policy-
revision re-checking happens at use time, not just issuance, and `ActionFingerprint`'s binding bytes are
injective (no delimiter-collision path lets two different principal/session/action bindings hash the same).
One candidate — `ApprovalRequest::resolve` having no consumed-flag, so a shared `&ApprovalRequest` could in
principle be resolved more than once — was correctly *not* reported as a bug: no doc comment claims single-
resolve, the enforced guarantee is the resulting lease's `max_uses` (which genuinely is atomic), and no real
call site anywhere in the workspace keeps a pending `ApprovalRequest` around across concurrent calls. Recorded
here only because "reviewed, found nothing" is itself useful signal against re-litigating this crate.

**Fresh review pass, 2026-09-02, `crates/mcp/src/transport.rs::StdioTransport` — a live, reachable hang: the
transport's blocking pipe read ignored both `CancellationToken` and its own configured `IoBounds::timeout`.**
The trait doc (`transport.rs:156`, then): "Byte-level MCP transport. Implementations must honor cancel and
frame caps." `read_newline_frame` only checked `cancel.is_cancelled()` *between* `Read::read` calls
(`CANCEL_STRIDE`-gated); once inside a real blocked `read()` syscall on an actual pipe, nothing could interrupt
it, and `IoBounds.timeout` was never consulted for stdio at all — confirmed via `grep -n "bounds.timeout"
transport.rs` showing exactly one hit, inside the *HTTP* transport's `exchange()`, not stdio's. **Concrete,
live consequence:** `apps/rapid/src/exec_tools.rs::execute_mcp_tool` calls `session.tools_call(...)` directly
on a real `StdioTransport<ChildStdout, ChildStdin>` and spawns its own 30s watchdog thread specifically to
bound that call via `cancel.cancel()` — a watchdog that turns out to be a no-op against a genuinely hung MCP
stdio server, since the cancellation flag it sets is never actually checked while the read is blocked.
Reproduced directly: a test using a real `UnixStream::pair()` (write end held open but silent, exactly
mirroring a hung server's pipe) hung the test process itself past a 20s bound when run against the unfixed
code — confirmed via the standard temporary-revert cycle (stash the fix, re-add just the new tests to the
original code, observe the genuine hang, restore the fix) rather than only reasoning about it. **Fixed:**
`StdioTransport` now spawns a dedicated background thread at construction (`spawn_frame_reader`) that owns the
reader and runs the framing loop internally, pushing each parsed frame (or terminal error) onto an
`mpsc::Receiver`; `recv_frame` polls that channel with `recv_timeout` in short (`RECV_POLL_INTERVAL` = 50ms)
slices bounded by `IoBounds::timeout`, checking cancellation every slice, so it now returns promptly on either
signal instead of blocking on the syscall itself. This is safe against the higher-layer `exchange_skipping_
notifications` loop (`transport.rs:946`), which already discards any frame whose JSON-RPC `id` doesn't match
the expected request — confirmed by reading it before starting the redesign, specifically to rule out a
"stale response from an abandoned request gets delivered to a later, unrelated call" correctness risk before
implementing this. New tests `stdio_recv_frame_times_out_when_the_peer_never_writes` and `stdio_recv_frame_
is_interrupted_by_cancellation_when_the_peer_never_writes`. Full `mcp` crate suite (80 tests, up from 78),
`apps/rapid`'s three real MCP integration tests (`mcp_stdio_servers_register_and_dispatch_through_the_session`
included — a real spawned-child end-to-end path), and `cargo build --workspace --tests` all pass. **Known,
deliberately out-of-scope sibling gap:** `send_frame`'s blocking write (`write_all_cancellable`) has the exact
same structural limitation (cancellation only checked between syscalls) but was not fixed this pass — the
confirmed reproduction and the live watchdog-bypass consequence were specifically on the *receive* side;
fixing the write side too would be a natural follow-up but wasn't independently demonstrated here, so it's
named rather than assumed-and-fixed by extension.

**Same review pass — `crates/mcp/src/gateway.rs`'s `McpGateway`/`McpTrustStore`/`McpCatalogCache` are
unreachable: the real caller bypasses the entire trust/policy/lease gate this crate exists to provide.** This
is a restatement, not a new discovery: §0a above already established that "no production code path in the
whole repo mints a real lease today" (the `crates/capability-broker` note). `McpGateway::invoke` requires
exactly that missing prerequisite as an input (`ExternalCallRequest.lease: &CapabilityLease`), so this is that
same already-documented, deliberately-not-attempted structural gap surfacing again at a new call site, not an
independent problem needing its own remediation decision. Concretely: `gateway.rs`'s own doc comment ("Catalog
revision, trust, policy, and the capability lease are checked before any server I/O") and `trust.rs`'s
("Project-configured servers start disabled. Trust is an explicit grant.") are both true of the *gateway* type
in isolation — its own tests confirm every check fires correctly — but `apps/rapid/src/exec_tools.rs` never
constructs or calls through `McpGateway` at all; `register_mcp_servers` spawns every configured server
directly off `.rapidlm/settings.json`'s `mcpServers` table once the coarser, unrelated workspace-trust check
passes, and `execute_mcp_tool` calls `McpSession::tools_call` directly — no `McpTrustStore::authorize_connect`/
`authorize_tool`, no policy evaluation, no lease consumption, for any MCP tool call in production today.
**Deliberately not attempted:** wiring `McpGateway::invoke` in at that call site needs the same missing
prerequisite every other "wire crate X into `apps/rapid`" item in this document is blocked on — a real
approval/policy-stack/lease-issuance flow reachable from the live tool-dispatch loop, which doesn't exist
anywhere in `apps/rapid` yet (confirmed, not assumed: no call site in `apps/rapid` constructs a `PolicyStack`
+ `LeaseValidator` + issues a `CapabilityLease` for any tool category, MCP or otherwise). This is exactly the
"needs a design decision this pass can't responsibly guess at" category, not a small wiring gap — flagging it
precisely rather than either rushing a partial/wrong integration or silently leaving it unrecorded.

**Fresh review pass, 2026-09-02, `crates/auth/src/secret.rs::SecretValue::expose` — two independent
implementations of "does this ref name the same secret" had silently diverged, so `open()` succeeding could
still be followed by `expose()` wrongly denying it.** `crates/auth/src/store.rs`'s `MemoryTable::get` (and the
in-memory/ephemeral legs of the keychain adapters that route through it) deliberately does *loose* matching —
`refs_match` treats a shared `id` **or** a shared `alias` as "the same handle" even if the two refs otherwise
differ, and `get` returns the *stored* ref, not the caller's query ref. But `SecretValue::expose` compared
`token.secret_ref != self.refer` with plain `PartialEq` — *strict* structural equality. `SecretBroker::open`'s
own doc comment: "Fails closed if `scoped` was issued for a different environment, and fails if the store no
longer holds the handle." A secret stored under `SecretRef::from_id_and_alias(id, alias)` but issued as a
`ScopedSecret` from `SecretRef::from_alias(alias)` alone made `open()` succeed (correctly proving the store
holds it) and then `expose()` fail with `ExposeUnauthorized` anyway, because `{id: None, alias}` != `{id:
Some(_), alias}` — contradicting `open()`'s own doc-promised guarantee that success there means the caller can
proceed. Verified via the standard temporary-revert cycle: the new test failed with exactly `expose:
ExposeUnauthorized` against the reverted strict-equality check, confirming the gap, before the fix was
restored. **Fixed:** added `SecretRef::matches` — the single canonical definition of "same handle" (exact
equality, or shared id, or shared alias) — and made both `store.rs::refs_match` and `SecretValue::expose` call
it, so the two can never independently diverge again. New test
`expose_succeeds_when_the_issued_ref_is_narrower_than_the_stored_one`. Full `auth` crate suite (72 tests, up
from 71) and `cargo build --workspace --tests` pass. **Latent, not yet actively firing:** confirmed via
`grep -rln "SecretBroker"` (outside `crates/auth`) that this broker has zero callers anywhere in the
workspace — its own `#[allow(dead_code)]` comment on `SecretBrokerToken::issue` says it's "minted by
SecretBroker / store paths in later auth tasks" — but a real, demonstrable contract violation in the public
API worth having fixed before any caller mints a `ScopedSecret` from a ref shape narrower than how the secret
happens to be stored.

**Same review pass, `crates/auth/src/local_daemon.rs::persist_token` — a write/sync failure while issuing a
daemon token left an un-wiped, owner-only secret file on disk indefinitely.** Every other failure path in this
function (mode-setting, rename) cleans up the temp file with `let _ = fs::remove_file(&tmp);` before
returning, but the `file.write_all(&encoded)`/`file.sync_all()` calls used a bare `.map_err(..)?` with no
cleanup — sibling-path asymmetry, the same shape this session has fixed repeatedly elsewhere. Concretely: if
`write_all`/`sync_all` fails (e.g. `ENOSPC` from a full disk, or any transient I/O error) while `DaemonAuth::
issue()` persists a freshly-generated token, the `0600`-mode temp file at `<runtime_dir>/daemon.token.tmp`
survives on disk, containing whatever partial/complete token bytes were flushed before the failure, until
`issue()` happens to be called again (the only existing cleanup is defensive, at the *next* call's own
`if tmp.exists() { remove_file }` guard at the top of the function) — inconsistent with the rest of this
crate's otherwise scrupulous secret-hygiene discipline (`wipe_array`/`wipe_vec` zero every in-memory secret
buffer on every drop/error path). Reachability: live — `DaemonAuth::issue()` is exercised directly by
`crates/kernel`'s IPC server auth handshake (the same subsystem this session already fixed a real bug in:
the per-request re-authorization gap, commit `72149ea`). **Fixed:**
the write-then-sync sequence now shares the same catch-and-clean-up pattern the mode-set and rename paths
already use. **Verification note:** not exercised by a new test — forcing a genuine, portable `write_all`/
`sync_all` failure (a full disk, a closed fd) without a platform-specific device (`/dev/full` isn't available
on macOS, where this pass ran) or an unsafe fd-manipulation trick isn't achievable safely here; the fix is a
direct, minimal-diff structural match to the two already-tested sibling cleanup blocks in the same function,
verified by inspection and by the full crate suite's continued pass (72 tests, no regressions) plus
`cargo build --workspace --tests`.

**Fresh review pass, 2026-09-02, `crates/llm-router/src/providers/openai_compatible.rs::http_get` — the
single most severe, most clearly *live* finding of this whole session: the live `web_fetch` tool's HTTP
response reader silently truncated response bodies to whatever bytes happened to arrive in the same low-level
socket read as the header terminator, often to nothing at all, for essentially any real remote server.**
`http_get`'s own doc comment: "the same SSRF guards, TLS root set, and **response caps** as the provider
transport." The real provider transport's reader, `read_http_response` (used identically by both the
OpenAI-compatible and Anthropic adapters), correctly continues reading after the initial header-focused read —
looping on `Content-Length` until satisfied, decoding `Transfer-Encoding: chunked`, or reading to EOF/deadline
when neither is present. `http_get`'s own reader, `read_http_response_opts`, did **none of this**: it read
until it found `\r\n\r\n` anywhere in the accumulated buffer and returned immediately, treating whatever bytes
happened to be in that same buffer as "the body" — no Content-Length continuation, no chunked decoding, full
stop. Any real server that delivers headers and body across separate TCP reads or TLS records (essentially
every server on a real network, as opposed to a `Cursor`-backed unit-test fixture) would have its body
silently cut down to a few hundred/thousand bytes or to nothing — returned as `Ok(...)`, not an error, and
`apps/rapid/src/web_fetch.rs::fetch_page`'s own truncation-marker logic (`if body.len() >= max_bytes { ...
FETCH_TRUNCATION_MARKER }`) never fires either, since the returned length is almost always far below
`max_bytes` — so the model receives a tiny or empty snippet of a page with **no indication it was cut off at
all**. Verified via the standard temporary-revert cycle with a real TCP fixture: a server that flushes headers,
sleeps briefly, then dribbles a 5,000-byte body across ten separate 500-byte writes (deliberately forcing
headers and body across separate low-level reads, the exact shape that defeats the header-focused-only reader)
made the new test fail with an **empty body** against the reverted code; restoring the fix returned the full,
correct 5,000 bytes. **Fixed:** unified `read_http_response`/`read_http_response_opts` into one
`read_http_response_impl(..., truncate: bool)` sharing the same Content-Length/chunked/EOF continuation logic
— `truncate=false` (provider path) keeps its existing fail-closed-past-`max_body` behavior unchanged (verified
by diffing the new code against the original line-for-line: identical when `truncate=false`), `truncate=true`
(tool-fetch path) now genuinely reads the real body up to `max_bytes` before returning, capping rather than
abandoning it early. `decode_chunked` gained the same `truncate` parameter for the chunked-body case (a
capped-but-incomplete final chunk is read up to the cap and returned as success rather than erroring). Also
removed `read_until_limit`, a now-dead thin wrapper the refactor obsoleted. New test `http_get_reads_the_full_
body_when_it_arrives_after_the_headers`; the six existing `decode_chunked` unit tests updated to pass their
prior, unchanged `truncate=false` expectation explicitly. Full `llm-router` crate suite (140 tests, up from
139, confirmed stable across 15 repeated runs — this crate's real-TCP-fixture tests carry some inherent
timing-based flakiness unrelated to this change, confirmed identical on the pre-fix code before concluding
so), `apps/rapid`'s 10 real `web_fetch` tests, and `cargo build --workspace --tests` all pass. **Live and
reachable, not latent:** confirmed via `grep -rln "llm_router::" apps/` that `apps/rapid/src/web_fetch.rs`
imports `http_get` directly and `apps/rapid/src/exec_tools.rs:2229` wires it into the real, model-callable
`web_fetch` tool — this bug has been silently degrading (in most real-world cases, silently *breaking*) one of
the product's core tools for arbitrary internet fetches.

**Fresh review pass, 2026-09-02, `crates/computer-use` — a dedicated review of desktop automation and, on
follow-up, browser automation, found five findings: two fixed, one lower-confidence code-smell noted only,
and two genuinely large gaps documented and deliberately not rushed.**

**Fixed — `crates/computer-use/src/desktop/backend.rs::DesktopActor::act` computed a sensitive-target flag and
never enforced it.** The threat model's T-CU-01 control ("UI content is untrusted... sensitive UI classifier
plus deterministic policy") is implemented on the observe/redaction side — `ResolvedDesktopTarget::is_sensitive
()` correctly flags macOS secure text fields and system-permission apps, Windows password elements and secure-
desktop apps, and Linux password roles and auth apps — but `act()` only ever checked `resolved.interactive`,
never `resolved.is_sensitive()`; the flag was computed and then never consulted for gating. A `Click` or
`TypeText` against a flagged target (a password field, a login-window control) reached the OS-level `perform()`
call unmodified. Verified via the standard temporary-revert cycle. **Fixed with one deliberate refinement over
the naive "deny everything sensitive" version**, discovered by the fix's own first attempt breaking two
existing, deliberately-designed tests (`secret_handle_stays_opaque` on macOS and Windows): T-CU-03's own stated
design lets an agent type an *opaque* `SecretHandle` into a secure field without ever seeing the plaintext —
that's the intended credential-injection path, not an attack, and remains allowed. What's denied is any
`Click` on a sensitive target, and any `TypeText` carrying a model-visible `SecretAwareString::Literal` value.
Two pre-existing tests (`linux.rs`, `windows.rs`) that typed literal text into the password field purely to
verify `type_text`'s action-kind mapping (not testing security semantics) were updated to use a secret handle
instead, preserving their original intent. New `DesktopError::TargetSensitive` variant (mirrors `TargetNot
Interactive`'s `ErrorCode::ToolInvalidArguments` mapping) and new test `sensitive_target_rejects_click_and_
literal_text_but_not_a_secret_handle`. Full `computer-use` crate suite (181 tests, up from 179 net after the
two test updates) and `cargo build --workspace --tests` pass. **Latent:** confirmed via `grep -rln "DesktopActor
|MacosDesktopBackend|WindowsDesktopBackend|LinuxDesktopBackend" apps/` (zero matches) that desktop automation
has no caller in `apps/rapid` at all today — `computer_runtime.rs` only imports `computer_use::browser::*`.

**Fixed — `crates/computer-use/src/browser/session.rs::BrowserManager::create` leaked freshly-allocated
session directories on every error path after allocation.** `allocate_dirs` creates `downloads`/`tmp` (and, for
an Ephemeral profile, `profile`) directories under `sessions/<id>/` before `reserve_slot`'s `TooManySessions`/
`SessionConflict` check, the backend launch, context creation, or `commit_session` can fail — none of those
five early-return paths removed the just-created directories, unlike the normal-close path (`ManagerInner::
cleanup`), which correctly does. Verified via the standard temporary-revert cycle with a real, non-synthetic
reproduction: filling `MAX_LIVE_SESSIONS` (32) live sessions, then one more `create()` call that fails with
`TooManySessions` — 33 directories remained on disk against the reverted code, confirmed by directly counting
`sessions/`'s entries, before the fix reduced it back to 32. **Fixed** with an RAII guard (`SessionDirsCleanup
Guard`) armed right after `allocate_dirs` and defused only once `commit_session` succeeds, rather than adding
cleanup at each of the five separate early-return sites by hand — the same shape of oversight ("forgot one of
several early-return paths") that caused the bug in the first place. The guard mirrors `cleanup()`'s own
`persist` exemption exactly: a `Persistent` profile's directory is a long-lived, possibly-reused-across-
sessions directory and is never deleted, even on this failure path — confirmed by re-reading `cleanup()`'s
identical `if snapshot.persist { false } else { remove_dir_if_exists(...) }` pattern before writing the guard,
since deleting a shared persistent profile on a transient failure would be strictly worse than the leak being
fixed. New test `create_failure_after_dir_allocation_does_not_leak_session_directories`. Full `computer-use`
crate suite and `cargo build --workspace --tests` pass. **Latent:** same reachability note as above —
`apps/rapid`'s `computer_runtime.rs` doesn't call `BrowserManager::create` yet.

**Documented, deliberately not rushed — `crates/computer-use/src/browser/action.rs::BrowserActor::act_with`
never calls the crate's own sensitive-action classifier, so a single `browser.navigate` lease can execute
actions the security module's own doc comments say must always be denied.** `security.rs`'s own doc comment:
"FileUpload, AuthSecurityAccount, Destructive, and Clipboard are Exclusive (deny-by-default). A `browser.
navigate` lease cannot authorize them." But `act_with`'s non-`Navigate` arm authorizes solely via `authorize_
page_action` → `require_browser_lease`, which checks only lease expiry/remaining-uses/capability-match/origin
— it never calls `classify_browser_action`/`authorize_browser_action`/`authorize_intents` from `security.rs`
at all. This is not speculative: the crate's own existing, unmodified, currently-passing test suite contains
two tests that directly contradict each other on the identical `(click "Sign in", BrowserNavigate lease)`
pair — `action.rs`'s `click_type_key_scroll_use_semantic_targets` asserts this click succeeds via `act()`,
while `security.rs`'s `navigate_lease_cannot_authorize_upload_auth_or_destructive` asserts the same click with
the same lease type is `SecurityError::PolicyDenied` via `authorize_browser_action` directly — both pass today
because they exercise two different, disconnected code paths for the same real decision. **Why this pass
didn't attempt a fix:** `act`/`act_with`'s public signature takes a single `observation_id: ObservationId`
(used only for staleness re-validation) and never receives the actual `Observation` value `classify_browser_
action`/`authorize_browser_action` require as an argument — `BrowserObserver` deliberately exposes no method to
re-fetch a previously-issued `Observation` by ID (only `require_current`, which validates staleness and returns
nothing). Wiring the classifier in correctly needs a real design decision: either widen `act`/`act_with`'s
public API to accept an `Observation` parameter (a breaking change touching every existing call site in this
crate's own tests, `fixtures.rs`, and any future `apps/rapid` integration), or add new internal machinery to
safely retrieve/reconstruct one from the observer's ledger without risking exactly the kind of stale-observation
bug this crate's own T-CU-02 threat class is about. Guessing at either under time pressure risks introducing a
new, subtly wrong security boundary rather than closing the one that exists — this is the "needs a design
decision this pass can't responsibly guess at" category, same as this document's other declined items, not a
small wiring gap. **Latent, matching the reachability pattern above:** confirmed via `grep -rln "computer_use::"`
that `apps/rapid/src/computer_runtime.rs` is the only consumer, and it does not call `act`/`BrowserActor::act`/
`authorize_browser_action` today — but this is exactly the mechanism that will fire the moment anything wires
real action dispatch through the obvious, already-public entry point.

**Documented, deliberately not rushed — two more desktop-module gaps, both requiring a design decision this
pass didn't attempt to guess at, found by the same sub-review:** (1) `DesktopActor::act` has no `CapabilityLease`
parameter or concept at all — compare `BrowserActor::act`, which requires one and checks it — so there is
currently no way to scope which desktop actions an agent may perform even in principle; designing that scoping
scheme from scratch (what capability/resource shape maps to Click vs. TypeText vs. LaunchApp, etc.) is a real
design task, not a bug fix, especially with zero real callers yet to validate the design against. (2)
`DesktopAction`'s bound checks (`MAX_CLICK_COUNT`, scroll magnitude, chord key count, `DisplayGeometry`'s
width/height) are enforced only inside the enum's own smart-constructor functions, not re-checked in `act()`
or any backend's `perform()` — because `DesktopAction` is a `pub` enum, Rust cannot restrict its variant fields
to be narrower than the enum itself, so `DesktopAction::ResizeWindow { width: 50_000, height: 50_000, .. }` can
be constructed directly, bypassing the bound entirely. Closing this properly means either re-validating bounds
defensively inside every backend's `perform()` (duplicated logic, easy to miss a variant) or restructuring the
public API around private fields and fallible constructors only (a breaking change to a still-unstable, not-
yet-wired-in public type) — a real API design tradeoff, not attempted this pass. Both latent, same reachability
as the other desktop findings above.

**Fresh review pass, 2026-09-02, `crates/agent-pool/src/lib.rs::ResourcePool::acquire` — a provisioner that
returns a duplicate id let one caller's release silently steal or free another caller's active lease.** The
module doc's own framing: the pool "tracks each environment's phase (warm / in-use / quarantined)," implying
each id maps to exactly one live lease at a time. `acquire`'s miss-fallback path called `provisioner.provision
(backend)` and inserted the result straight into `in_use` with no check against the ids already tracked in
`warm`/`in_use`/`quarantined` — a second provision returning an id already held elsewhere silently overwrote
that entry via `BTreeMap::insert`. Reproduced (and, before applying any fix, empirically confirmed against the
real crate in a standalone throwaway binary by a background review agent) with a `Provisioner` that derives its
id from the backend alone with no per-call uniqueness — exactly the shape `apps/rapid/src/host_runtime.rs`'s
own `FakeProvisioner` already uses: alice acquires `Container` (misses, provisions `"env-container"`), bob
acquires `Container` before alice releases (misses again, provisioner returns the *same* id, silently
overwriting alice's `in_use` entry — `in_use_count()` stays `1` even though two acquires were granted), bob
releases cleanly, carol acquires the now-warm `"env-container"`, and alice's eventual release removes *carol's*
still-active entry instead of her own. Verified via the standard temporary-revert cycle: the new test failed
with bob's acquire succeeding (returning `owner: Some("bob")` for what should have been a rejected duplicate)
against the reverted code. **Fixed:** added `ResourcePool::is_tracked` (checks all three sets) and reject the
freshly-provisioned id with a new `PoolError::DuplicateId` if it's already tracked anywhere, calling `provisioner
.destroy(&id)` first so the just-created backing resource isn't itself leaked. New test `duplicate_provisioner_
id_fails_closed_instead_of_stealing_a_lease`. **Same pass, second finding and fix — `quarantine()`'s
`QuarantineLimit` failure path dropped the lease from every tracked set, unlike its sibling `sanitize()`'s
identical failure shape**, which explicitly re-inserts the lease with the comment "a failed sanitize does not
lose the environment" (`lib.rs:302`). `quarantine()`'s equivalent branch had no such restoration, silently
decrementing the pool's real resource count with no `destroy()` call — the classic "cleanup/restoration present
on one error path, missing on the analogous sibling" shape this document has fixed repeatedly. Confirmed
reachable in isolation only via a total()-desyncing bug (this pass's own Finding 1, now fixed, was exactly such
a mechanism) or a future relaxation of the `MAX_ENVIRONMENTS` gate — a latent defensive-code bug, not a live
one, but worth having correct regardless. **Fixed:** `quarantine()` now re-inserts the lease into `in_use` on
the limit failure, mirroring `sanitize()`'s contract exactly. New test `quarantine_failure_returns_the_lease_
to_in_use_not_lost` (constructs the boundary directly via 256 filler entries rather than exercising 256 real
quarantine cycles). Full `agent-pool` crate suite (17 tests, up from 15) and `cargo build --workspace --tests`
pass. **Latent, not yet actively firing:** confirmed via `grep -rln "HostRuntime\b" apps/rapid/src` (only the
module declaration, no real call site) that `apps/rapid`'s `HostRuntime` wrapper around this pool has no
production caller, and no real (non-test) `Provisioner` implementation exists anywhere in the workspace yet —
but this is a genuine, demonstrable defect in the pool's own logic, not a caller-misuse issue: the pool's own
doc-claimed exclusivity guarantee should hold regardless of what ids a `Provisioner` implementation happens to
return, and the one example implementation already in the tree is exactly the naive shape that triggers it.

**Fresh review pass, 2026-09-02, `crates/knowledge/src/lib.rs::PreferenceStore::merge` — two compounding bugs:
the wrong entry was returned when the merge target wasn't the vec's last element, and a rejection was replayed
as a confirmation.** The module's own framing: "Preference store with contradiction handling across
candidates," and `merge`'s own doc comment: "same proposition confirms/contradicts in place." **Bug 1:** the
function mutated `existing` (found via `iter_mut().find(...)`, which can be any index) but always *returned*
`self.candidates.last().unwrap()` — correct only when the merge target happened to be the vec's last entry.
The crate's own pre-existing test never caught this because it only ever exercised a single-entry store, where
`last()` trivially equals the merged entry. **Bug 2:** the function *always* called `existing.confirm()` before
additionally replaying `candidate.contradictions` contradictions — so merging a Reject-sourced candidate
(built via `PreferenceCandidate::from_feedback`, which carries `observations: 0, contradictions: 1` for a
rejection) still called `confirm()` once, spuriously incrementing `observations` and partially offsetting the
contradiction's confidence drop — the store ends up *more* confident in a preference than it should be right
after being told the user rejected it, the opposite of `contradict`'s own doc comment ("Contradiction reduces
confidence"). Verified via the standard temporary-revert cycle for both: the new tests failed with the exact
predicted wrong values (`"prop-b"` returned instead of `"prop-a"`; `observations` incremented to 2 instead of
staying at 1) against the reverted code. **Fixed:** replaced the `iter_mut().find()`/`last()` pattern with an
index-based lookup (`iter().position()` then index into `self.candidates[index]`) so the function can return a
reference to the actual merged entry regardless of position, and replays `candidate.observations` confirms
before `candidate.contradictions` contradicts — generalizing correctly to both Accept/Edit-sourced (1
observation, 0 contradictions) and Reject-sourced (0 observations, 1 contradiction) candidates alike, instead
of hardcoding one confirm every time. New tests `merge_returns_the_actual_merged_entry_even_when_not_last` and
`merge_replays_a_rejection_as_a_contradiction_not_a_confirmation`. **Same pass, third and lowest-severity
finding — `PreferenceCandidate::decay`'s `days_idle as i32` cast wraps negative past `i32::MAX`, flipping
`0.98f64.powi(...)` from decaying toward the 0.05 floor to diverging toward infinity** — directly contradicting
the function's own doc comment ("recency keeps preferences honest"). Requires an unrealistic `days_idle` (~5.9
million years) to trigger directly via the public `u32` parameter, but is exactly the kind of footgun a
timestamp-subtraction-derived `days_idle` could hit on an underflow before the cast; fixed with a `.min(i32::
MAX as u32)` clamp before casting, verified via the same temporary-revert cycle (`confidence.is_finite()`
failed against the reverted code). New test `decay_never_blows_up_past_i32_max_days_idle`. Full `knowledge`
crate suite (7 tests, up from 4) and `cargo build --workspace --tests` pass. **Latent, not yet actively
firing:** confirmed via `grep -rn "PreferenceStore|PreferenceCandidate|FeedbackEvent|ExperimentRegistry"
--include="*.rs" .` that the only matches outside this crate's own file are its `Cargo.toml` dependency
declaration in `apps/rapid` — no file under `apps/rapid/src` actually imports or calls into `knowledge::*` yet.
The crate is fully dead code from the rest of the workspace's perspective today, but these bugs would be live
the moment the feedback/preference-learning pipeline its own module doc references (P11-020..027) gets wired
in — worth having correct before that happens.

**Fresh review pass, 2026-09-02, `crates/workspace/src/backends/git_worktree.rs::GitWorktreeStore::
rollback_create` — the live, reachable finding of this pass: a failed `create_view` could permanently orphan a
worktree directory with no record left pointing at it.** The module's own doc comment: "Cleanup never uses
`--force`; a dirty worktree is left intact and reported as `GitWorktreeError::CleanupFailed`." `remove_view`
honors this precisely (checks the removal result and the directory's continued existence; on either failure,
marks the persisted record `CleanupFailed` and returns an error *without* deleting the ref or metadata).
`rollback_create` — invoked from `create_view`'s error branch when `finish_create` fails after the worktree may
already be partially created — did the opposite: it discarded `git worktree remove`'s result and unconditionally
deleted both the ref and the metadata record regardless of whether the directory removal actually succeeded.
Concretely: `finish_create` runs `git worktree add` then checks the user's HEAD hasn't moved, failing with
`GitFailed` if it has (or on a timeout/cancellation mid-checkout, already a real, tested failure mode via `git_
timeout_is_enforced`) — a real worktree directory exists on disk by that point. A non-forced `git worktree
remove` can legitimately fail against a dirty/partial checkout; `rollback_create` swallowed that failure and
deleted the metadata anyway, leaving the directory (and git's own `.git/worktrees/<id>` registration)
permanently unreachable — `list_views`/`metadata_ids` only enumerate `views/*.json`, so nothing could ever find
or clean it up again. Verified via the standard temporary-revert cycle: the new test failed on `assert!(ref_
exists(...))` against the reverted code. **Fixed:** `rollback_create` now mirrors `remove_view`'s exact
contract — on a removal failure or the directory still existing afterward, it re-reads the still-persisted
record, marks it `CleanupFailed`, and persists that instead of deleting anything further. New helper `mark_
cleanup_failed`, new test `rollback_create_preserves_the_record_when_cleanup_cannot_remove_the_worktree`. Full
`workspace` crate suite (173 tests, up from 171) and `cargo build --workspace --tests` pass. **Live and
reachable, not latent:** confirmed via `grep -rn "create_view"` that `apps/rapid/src/shadow_diagnostics.rs` and
`crates/agent-runtime/src/agent/spawn.rs` (which explicitly maps `GitWorktreeError::Timeout` to `SpawnError::
Timeout` — timeout-during-create is an anticipated, already-handled outcome on the real subagent-spawn path)
both call `create_view` directly, and `apps/rapid` depends on both crates.

**Same review pass, `crates/workspace/src/backends/{overlay,remote}.rs` — a lost-update race in both `Overlay
Backend`/`RemoteSnapshotBackend`'s `write`/`delete`: two separate lock acquisitions (one to read the current
state via `visible()`, a second to mutate) let a concurrent writer's release-then-remutate silently overwrite
another writer's just-completed change with no error, defeating the `expected`/`PreimageMismatch` optimistic-
concurrency contract those parameters exist to enforce.** The sibling `DirectBackend::write` already gets this
right — one lock held across the whole read-decide-mutate sequence — making this an asymmetry between
implementations of the same `WorkspaceBackend` contract, not a design choice. Concretely: two threads both call
`write(path, bytes, expected: None)` for a not-yet-existing path; both see `visible() == None` under their own,
separately-acquired-and-released lock, both pass the `(None, None) => {}` arm, and both then acquire a second,
fresh lock to `slots.insert(...)` — the second insert silently replaces the first, and both calls return `Ok
(())` even though only one write should have won. Verified via a genuine multi-threaded reproduction (not
simulated): an unsynchronized 2-thread version of this test needed hundreds of tries to land inside the actual
race window (a few in-memory operations) and mostly didn't, so the real test uses 16 threads synchronized to a
`Barrier` all racing to create the same path — against the reverted two-lock code this reliably produced
multiple `Ok(())` results (2 successes out of 16 in the run that confirmed it) for what should be exactly one
winner; restoring the fix reduced it to exactly 1 every time across repeated runs. **Fixed:** both backends now
acquire the lock once and hold it across the whole decide-then-mutate sequence, via a new `visible_locked` that
takes an already-held `&Inner` instead of re-locking (the now-fully-redundant `OverlayBackend::visible`
convenience wrapper was removed as genuinely dead code once both its only two callers were switched). New test
`concurrent_writes_to_a_new_path_never_both_report_success` in `overlay.rs`. Full `workspace` crate suite and
`cargo build --workspace --tests` pass. **Latent, not yet actively firing:** confirmed via `grep -rln
"OverlayBackend::\|RemoteSnapshotBackend::"` that neither type is constructed anywhere outside the crate's own
tests — real, demonstrable defects in dormant public API, worth having correct before either backend is wired
into `apps/rapid`.

**Same pass, two further findings documented but not fixed — both real, both currently dormant, both larger
than a scoped bug fix given the time already spent on this pass's two live/higher-confidence fixes above:**
(1) `crates/workspace/src/checkpoint.rs::CheckpointManager::rewind` bypasses the manager's own `max_files`/
`max_bytes` caps that `checkpoint()` enforces — `plan_rewind`'s signature takes neither parameter at all, so a
manager configured with tight caps (e.g. `max_files=1`) still processes every journal entry since the checkpoint
(up to `MAX_JOURNAL_ENTRIES` = 4096, each up to the crate-wide 8 MiB ceiling) with zero enforcement from its own
configured limits during `rewind`. Closing this means deciding what `plan_rewind` should actually do when a
rewind's natural scope exceeds the manager's caps (truncate the rewind set? fail closed? a different cap
entirely from checkpoint's?) — a real design question, not a mechanical fix, and `grep -rln "CheckpointManager"`
confirms zero instantiations outside the crate's own tests today. (2) `crates/workspace/src/patch/apply.rs::
MAX_OVERLAY_FILES`'s own doc comment says "Maximum **present** files retained in one overlay," but `check_
overlay_bounds` counts `slots.len()` — present entries *and* delete tombstones, which are never pruned — so a
`StagingOverlay` reused across many sequential patches (an explicitly supported, tested usage pattern) can hit
the 4096 cap from accumulated deletes alone and reject a legitimate `CreateFile` for a brand-new file even with
zero present-file bytes held. Fixing this needs a decision on the actual intended tombstone lifetime (prune on
some schedule? cap tombstones separately from present files? count only `Present` slots against `MAX_OVERLAY_
FILES` and add a distinct tombstone bound?) rather than a one-line change; `grep -rln StagingOverlay` outside
the crate confirms zero external callers, and the crate's own internal caller creates a fresh overlay per
transaction, so it isn't triggered by current wiring either. Both flagged precisely rather than guessed at.

**Fresh review pass, 2026-09-02, `crates/insights/src/lib.rs::analyze` — the last crate in the workspace to get
a dedicated review this session, and a live one: two of the four signal families `analyze`'s own doc comment
promises were never implemented.** Doc comment: "Session insights over exported ledger records: tool usage,
approvals, goal completion, error pressure." The implementation only ever produced `tool_usage`, `denials`
(narrowly `kind == "tool.denied"`, not the broader "error pressure" the doc promises), `goal_completion`, and
an undocumented fifth `secret_touches`. Concretely: a session with real `ApprovalRequested`/`ApprovalResolved`/
`ApprovalExpired` events (`crates/event-ledger`'s actual Approval-family kinds — confirmed present in its
closed `define_event_kinds!` registry) or `ToolFailed`/`TurnFailed`/`ModelFailed` events produced no `approvals`
or `error_pressure` insight at all — `rapid insights <session-id>` silently omitted exactly the two signal
categories its own doc comment says it surfaces. Separately, `secret_touches` (`kind.contains("secret")`) was
confirmed genuinely dead code for any real session: `crates/event-ledger/src/event.rs`'s wire-string registry
is closed and fully enumerated (~90 literal kind strings), and none contains the substring `"secret"` — the
crate's own test only exercised this branch with a synthetic `"secret.used"` string production code can never
construct (`EventSummary` is built exclusively from real `ExportedEvent.kind` values). Verified via the
standard temporary-revert cycle: the new test failed on `kinds.contains(&"approvals")` against the reverted
code. **Fixed:** implemented `approvals` (matches the real `approval.*` kinds plus the tool-side `tool.
approval_required` gate that triggers them) and `error_pressure` (matches `turn.failed`/`model.failed`/`tool.
failed`) using the exact same filter-and-count pattern the three existing insights already establish in this
same function — not a new mechanism, just extending an established local pattern with kinds that already exist
in the ledger's own registry. Removed the confirmed-dead `secret_touches` branch (and its test coverage, which
only ever exercised the synthetic string) rather than leave demonstrably unreachable production logic that
looks like it does something. New tests `analyzer_surfaces_tools_denials_completion_approvals_and_errors` and
`analyzer_emits_no_approvals_or_error_pressure_when_absent`. Full `insights` crate suite (5 tests, was 4 before
removing the old combined test and adding two) and `cargo build --workspace --tests` pass. **Live and
reachable:** confirmed via `grep -rn "secret_touches"` (zero hits anywhere, safe to remove) and via `apps/
rapid/src/interactive.rs:349`/`p9_commands.rs:1817-1847` that `rapid insights <session-id>` is a real, wired CLI
subcommand exercising exactly this function against real exported ledger events today — `apps/rapid`'s own
`insights_command_analyzes_real_exported_ledger_events` integration test still passes. This closes out the
session's crate-by-crate sweep: every crate in the workspace has now had at least one dedicated review pass.

**Fresh review pass, 2026-09-02, `apps/rapid/src/exec_tools.rs` — the shipped binary's own tool-dispatch code
finally got a dedicated pass (previously only touched indirectly via `crates/*` fixes), and found the exact
same live "child-output deadlock" bug family already fixed three times elsewhere this session, plus a real
panic and a real unbounded-memory gap, all in the primary model-facing tool surface.**

**Fix 1 — `execute_shell`'s foreground `shell_exec` path deadlocks on ordinary-sized output.** It polled
`child.try_wait()` in a loop without ever reading the child's piped stdout/stderr until `wait_with_output()`
*after* the loop — a child writing more than one OS pipe buffer's worth of combined output before exiting
blocks in `write(2)`, `try_wait()` never observes the exit, and the call spins for the full timeout (default
60s) before being force-killed. The sibling background-job path (`JobRegistry::start`) already gets this right
— it spawns reader threads that drain each pipe concurrently with its own poll loop — making this the same
"one path has the fix, the sibling reimplements the deadlock" asymmetry this session's own `git log` shows
already fixed three times (commit `e0236b5`, "fix: drain child stdout/stderr concurrently with wait, not
after, across 3 call sites" — `external_agents.rs`, `goal_claim.rs`, `crates/plugin-host/src/hooks.rs`); this
fourth call site was simply missed. Verified via the standard temporary-revert cycle with a real child (`dd
if=/dev/zero bs=1024 count=300`, ~300 KiB — comfortably past any common pipe buffer): the reverted code took
the full 5000ms test timeout and had to be force-killed; the fix completes in under 50ms. **Fixed:** ported
the same concurrent-reader-thread pattern `JobRegistry::start` already uses, into a shared, capped buffer (this
also fixes stdout/stderr's *relative* ordering going from "grouped" to real chronological interleaving — a
minor, arguably-improved behavior change the existing test only asserts `contains(...)` against, not exact
ordering). New test `shell_exec_drains_output_concurrently_and_does_not_deadlock`.

**Fix 2 — `pdf_page_count` panics on a crafted file as small as 10 bytes.** `&bytes[at + 6..]` assumed 6 bytes
always follow a `"/Type"` match, but `find_bytes` only guarantees the 5-byte match itself fits — a match
ending exactly at the buffer's end panics with an out-of-range slice. Reachable in two model tool calls:
`workspace_write` a file containing `"%PDF-/Type"`, then `workspace_read` it. `batch_dispatch`'s own panic
containment (each call runs in a `thread::scope` worker, joined and converted to a typed failure) keeps this
from crashing the whole process, but it still means a bare, noisy internal panic instead of the clean, typed,
model-correctable failure every other malformed-input branch in this file produces. Verified via the standard
temporary-revert cycle. **Fixed:** `bytes.get(at + 6..).unwrap_or(&[])` instead of a direct slice, treating "no
bytes follow" as "nothing left to inspect" rather than panicking; `cursor` is likewise clamped to `bytes.len()`
so the next loop iteration's slice can't overrun either. New test
`workspace_read_pdf_with_type_at_buffer_end_does_not_panic`.

**Fix 3 — `workspace_read`, `repo_read`, `workspace_patch`, and `repo_search` all read files fully into memory
with no bound, unlike `workspace_write`'s input, which is genuinely capped before any I/O happens.** All four
called plain `fs::read`/equivalent, allocating and copying a file's *entire* size regardless of how much of it
ever reaches the model — `workspace_read`/`repo_read` only truncate the *output* afterward; `repo_search`'s
`walk_text_files` does this for every one of up to 2000 walked files, and even reads a file fully before its
own binary/NUL-byte heuristic (which only inspects the first 8 KiB) gets a chance to skip it. This repo already
has the exact right fix pattern twice over — `crates/workspace::backends::external_mutation::read_confined`
and, in this very file's own sibling module, `p9_commands::read_bounded_file`, whose doc comment states the
rationale almost verbatim: "Reads through a `max_bytes + 1` cap rather than trusting a preceding `fs::metadata`
size check... Capping the read itself means at most `max_bytes + 1` bytes are ever buffered, regardless of how
large the file actually is." Concretely: a model calling `repo_search` (or `workspace_read`) on a workspace
containing a large checked-in file (a dataset, media asset, log, or database dump) allocates memory
proportional to that file's full size to serve what should be a cheap, bounded response. Verified via the
standard temporary-revert cycle: an 8 MiB+1 file made `workspace_read` succeed (fully buffering it) against the
reverted code; the fix rejects it as a handled, typed failure instead. **Fixed:** new shared helper
`read_file_bounded` (mirrors `read_bounded_file`'s exact technique) and a new `MAX_FILE_READ_BYTES` (8 MiB,
matching the same "one file" ceiling already established by `crates/workspace`'s `MAX_DIRECT_FILE_BYTES` and
`crates/llm-router`'s `MAX_HTTP_RESPONSE_BYTES`) wired into all four call sites — `workspace_patch` in
particular needs the file's *real* content to patch correctly, so it fails closed above the cap rather than
silently truncating, unlike the read-preview tools, which already truncate their *output* separately and
unaffected by this change for any realistically-sized file. New test
`workspace_read_rejects_a_file_past_the_read_bound_instead_of_buffering_it`.

**Bonus fix found during verification, `apps/rapid/src/context_retrieval.rs::TimeoutWatcher`** — while
confirming the three fixes above didn't destabilize the full `apps/rapid` suite, one unrelated test
(`timeout_watcher_is_stopped_even_when_retrieve_inner_errors_early`) failed intermittently; confirmed via
`git stash` that it already failed ~75% of the time on the *original*, untouched code, ruling out a regression
from this pass's own changes and pointing at a real, pre-existing race. The watcher's poll loop only checks its
stop flag *inside* the `while started.elapsed() < timeout` loop body — if `stop()` is called after the loop's
last flag-check but before its next `elapsed() < timeout` re-evaluation trips false, the loop falls straight
through to `cancel.cancel()` with no further chance to observe the stop request at all, exactly the "the check
exists on the way in, not on the way out" shape this document has found repeatedly elsewhere. **Fixed:** added
one more stop-flag check immediately before the fall-through `cancel.cancel()`. Confirmed via repeated runs:
15/15 clean afterward, versus roughly 1-in-4 passing before.

Full `rapid` crate suite (335 tests, up from 332) and `cargo build --workspace --tests` pass for all of the
above. **Live and reachable, not latent:** `SHELL_EXEC_TOOL`, `WORKSPACE_READ_TOOL`, `REPO_READ_TOOL`,
`WORKSPACE_PATCH_TOOL`, and `REPO_SEARCH_TOOL` are all registered, real, model-callable tools dispatched
directly from `execute_call_traced`'s match table — this is the primary tool surface a model actually drives on
every turn, not a dormant or unwired mechanism like most of this session's other findings.

**Fresh review pass, 2026-09-03, `apps/rapid/src/host.rs::load_memory_index`/`load_todos_index` — the same
unbounded-read pattern just fixed in `exec_tools.rs`, recurring one file over.** `load_memory_index`'s own doc
comment: "a bounded, always-loaded pointer file... oversized content is truncated to the line and byte bounds
rather than dropped entirely." Both loaders called raw `fs::read_to_string`/`fs::read` — bounding only the
*output* after the whole file was already buffered, not the read itself. `.rapidlm/MEMORY.md` is explicitly
documented elsewhere in this binary as "git-committed and team-shared," i.e. it arrives via `git clone`, not a
write this binary controls or bounds; a bloated or malicious repo's `MEMORY.md`/`todos.json` forces a full-file
allocation on every single turn, not once at startup. Verified via the standard temporary-revert cycle, with a
specifically-designed reproduction for each: `load_memory_index`'s new test constructs valid multi-line content
whose truncated OUTPUT would be identical whether or not the read itself was capped, so it was extracted into a
directly-testable `read_memory_index_bounded` helper and asserted on the raw pre-truncation length (100KB
buffered on the reverted code vs. the intended 25.6KB cap); `load_todos_index`'s new test uses genuinely valid,
complete JSON padded past the cap with a large trailing field — an unbounded read parses the whole thing
successfully, while a read capped short of the file's real size truncates mid-value, making the bytes actually
read invalid JSON. Both distinguishing mechanisms were necessary because a merely "oversized-and-garbage" or
"oversized programmatically-truncated" fixture would return the same result regardless of whether the read was
bounded, which wouldn't actually prove the fix. **Fixed:** widened `exec_tools::read_file_bounded`/
`BoundedReadError` to `pub(crate)` and reused it for `load_todos_index` (oversized → treated as corrupt,
matching its own documented fail-open contract); `load_memory_index` needed its own bounded-read helper instead,
since unlike `read_file_bounded` it must still yield truncated content on an oversized file, not `None`. New
tests `read_memory_index_bounded_never_buffers_past_the_cap`, `load_memory_index_truncates_an_oversized_
multiline_file_instead_of_dropping_it`, `load_todos_index_treats_a_file_past_the_bound_as_corrupt_even_if_it_
is_valid_json`. Full `host` test module (50 tests, up from 44) and `cargo build --workspace --tests` pass.
**Live and reachable:** both loaders are called from `apps/rapid/src/interactive.rs`'s `exec_turn`, the real
entry point for every `rapidlm exec` invocation and every `rapid cron poll` cycle — an attacker-controlled cron
prompt combined with a bloated `MEMORY.md` in the target repo re-triggers this on every unattended poll.
(Same review pass, background agent's Finding 2 — `host.rs::diag_host_from_base_url` truncates by `.chars()`
count instead of the `MAX_DIAG_HOST_BYTES` its own doc comment promises, so an internationalized hostname could
yield a diagnostic label up to ~4x the documented byte cap — noted but not fixed this pass: it's a `--verbose`-
only diagnostic line, not a memory/security boundary, and the fix is genuinely trivial should anyone hit it.)

**Same review sweep, `apps/rapid/src/permissions.rs::ToolPattern::matches` — a real security bypass: domain
`deny`/`ask` rules, and even the admin `denied_tools` ceiling documented as un-overridable by any setting, were
case-sensitive, so a case change in the request URL's host slipped past them entirely.** `glob_match` is a
plain per-`char` comparator with no case folding, and `rule_subject` (`exec_tools.rs`) builds the `web_fetch`
match subject as `format!("domain:{host}")` from the raw, un-lowercased URL host. The *sibling* mechanism for
the identical conceptual question — "is this host on my list?" — is explicitly documented and implemented as
case-insensitive one file over: `web_fetch::classify_fetch`'s own doc comment says "exact, case-insensitive
match," via `eq_ignore_ascii_case`. Concretely: a settings rule `{"deny": ["web_fetch(domain:evil.example.com)"]}`
(or an admin policy `denied_tools` entry, which per its own doc "no setting can re-enable") never matches a
call to `https://EVIL.EXAMPLE.COM/...` — `glob_match` returns `false` on the first case mismatch, no ask/allow
rule matches either, `web_fetch` classifies as `ToolClass::ReadOnly`, and the call auto-allows and actually
executes. Verified via the standard temporary-revert cycle: the new test failed against the reverted matcher —
not with the naively-predicted "the fetch actually executes" (the fictional test domain doesn't resolve
regardless of case, so the reverted code's *attempted* fetch itself fails on an unrelated DNS error), but the
security-relevant fact it proves is unaffected either way — the deny rule did not fire and the call was not
`Denied`, exactly the bypass this fix closes; for a real, resolvable malicious domain the attempted fetch would
have gone through. **Fixed:** `ToolPattern::matches` now compares `"domain:"`-prefixed subjects/patterns
case-insensitively (both sides lowercased before `glob_match`), leaving every other subject shape (paths, shell
argv) exact-case as before, matching real filesystem/shell semantics — this is scoped precisely to the one
subject shape that's unambiguously case-insensitive by spec, not a blanket case-insensitive glob change. New
test `web_fetch_deny_rule_matches_the_urls_domain_regardless_of_letter_case`. Full `permissions` (21 tests) and
`web_fetch`-related `exec_tools` (11 tests) suites, plus `cargo build --workspace --tests`, all pass. **Live
and reachable, security-relevant:** `permission_for`/`ToolPattern::matches` gate every real tool call via
`execute_call_traced`, the actual dispatch path a model drives on every turn — this is the one bug this
session's `apps/rapid` sweep found that directly defeats a security control on the live tool surface, rather
than a latent mechanism or a memory/correctness issue. (Same pass, Finding 2 from the same background agent —
`parse_grants`'s per-project cap checks *after* pushing, allowing 129 entries into a 128-entry `Vec` before
breaking — confirmed genuinely inert: the sole reader, `PermissionLattice::with_grants`, re-caps via
`.take(MAX_GRANTS - self.grants.len())` before consulting any grant, and nothing ever persists `PermissionGrants`
back to disk, so the extra element never has an observable effect. Not fixed this pass since there is nothing
to demonstrably fix a bug in, but noted for completeness rather than silently dropped.)

**Fresh review pass, 2026-09-03, `apps/rapid/src/interactive.rs`'s fallback-chain wiring — the admin `min_
reasoning_effort` floor applied to the primary model but not to `[models] fallback` alternates.** `managed_
config.rs`'s own module doc frames every one of its gates as a hard invariant that "only ever restrict[s],
never widen[s]," and `resolve_gated` correctly raises the *primary* model's effort to the floor. But the
fallback-chain wiring in `interactive.rs` already re-applies the managed `allowed_providers` allowlist to each
`[models] fallback` candidate — with the comment "a fallback entry is never let through a restriction the
primary itself has to honor" — and simply never got the equivalent line for `min_reasoning_effort`: the
developer clearly reasoned about parity for one gate and missed it for the sibling one. Concretely: an admin
sets `min_reasoning_effort = "high"`; the user's primary model is correctly raised to `"high"`, but a
configured `[models] fallback` entry with no (or a lower) `reasoning_effort` runs at its own unraised value the
moment the primary fails over — a normal, real trigger (rate limit, outage, auth hiccup), not an edge case.
Verified via the standard temporary-revert cycle: the new test failed with the fallback candidate staying at
`Low` instead of being raised to `High`. **Fixed:** extracted the floor comparison into a shared `below_floor`
predicate `resolve_gated` and the fallback wiring both call (so the two paths can't silently drift apart again
the way they just had), and further extracted the *whole* per-candidate policy application (allowlist filter +
effort raise) into `apply_to_fallback_candidate`, replacing the fallback loop's own hand-rolled, partial
version — this also simplified `interactive.rs`'s call site. New tests `below_floor_matches_resolve_gated_own_
raise_condition` and `fallback_candidates_are_raised_to_the_effort_floor_and_filtered_by_allowlist`. Full
`managed_config`/`interactive` test modules and `cargo build --workspace --tests` pass. **Live and reachable:**
`[models] fallback` is a real, documented, tested user-facing feature, and `FallbackController`/
`FallbackChainModel` is live production code wired at `interactive.rs`'s startup, not test-only scaffolding.

**Same review pass, `managed_config.rs::load_policy` and `user_config.rs::read_config_file` — the same
unbounded-read-before-bound-check pattern already fixed twice this session (`exec_tools.rs`, `host.rs`), found
in the two files that define and load the byte/count ceilings the rest of this gating layer enforces on
everything else.** `load_policy` read the managed policy document via `fs::read_to_string` with no cap at all;
`read_config_file` read the user config via `fs::read` and only checked `bytes.len() > MAX_USER_CONFIG_BYTES`
after the whole file was already buffered. **Fixed:** reused `exec_tools::read_file_bounded` (already widened
to `pub(crate)` earlier this session) for both, mapping its `TooLarge`/`Io` outcomes onto each function's
existing, unchanged error variants — `read_config_file`'s new test confirms the observable `TooLarge` contract
survives the rewiring (verified via the revert cycle that this specific test does *not* discriminate old from
new code, since both correctly reject an oversized file the same way — its value is guarding the wiring, not
proving boundedness, which is `read_file_bounded`'s own separately-tested guarantee). New constant `MAX_
MANAGED_POLICY_BYTES` (256 KiB, matching `MAX_USER_CONFIG_BYTES`). Full `managed_config`/`user_config` test
modules and `cargo build --workspace --tests` pass. **Lower severity than the effort-floor gap above:** both
paths are ordinarily admin/user-controlled files, not attacker-reachable input, so this is a defensive-
consistency fix rather than a live exploit path — flagged by the same background review agent at lower
confidence for exactly this reason, and fixed anyway since the established bounded-read helper was a direct,
low-risk drop-in.

**Fresh review pass, 2026-09-03, `apps/rapid/src/model.rs::fold_stream` — parallel tool-call proposals were
silently reordered into lexical call-id order, contradicting `agent-runtime::turn`'s own documented dispatch
contract.** `fold_stream`'s own doc comment: "tool-call deltas form proposed calls (the tool driver decides
validity downstream)" — implying the calls it hands back are the ones the model actually proposed, in that
order. It collected them into a `BTreeMap<String, (String, String)>` keyed on the provider's opaque `call_id`
(OpenAI's `call_XXXXXXXXXXXX`, Anthropic's `toolu_XXXXXXXXXXXX` — confirmed via direct citation of both
provider adapters, not assumed), then handed back `.into_iter()`'s **lexicographic byte order of the key**,
not the order `ToolCallStart` events actually arrived in. `crates/agent-runtime/src/turn.rs`'s own doc comment
is explicit: "Phase 1: per-call gates and validation, in **proposal order**. The first refusal is recorded;
calls accepted ahead of it still dispatch" — the per-call budget gate and the loop detector both act on
whatever sequence `fold_stream` hands them. Concretely: a model proposes `workspace_write` (save a file) then
`shell_exec` (run tests) in that order; if their call ids happen to sort the other way (entirely a function of
each provider's opaque id generation, unrelated to proposal order), the turn loop sees `shell_exec` first — if
the turn's remaining tool-call budget only allows one more call, the *test run* could be accepted while the
*file save it's supposed to check* is refused as over-budget, the inverse of what the model asked for. Parallel
tool calls are a normal feature of both provider APIs, not an edge case; the existing test suite never
exercised more than one concurrent `ToolCallStart`, which is exactly why this went uncaught. Verified via the
standard temporary-revert cycle: the new test (deliberately using call ids that sort opposite of proposal
order) failed with the calls reversed against the reverted `BTreeMap` code. **Fixed:** replaced the map with an
insertion-order-preserving `Vec<(String, String, String)>`, using a short linear scan (real per-turn tool-call
counts are small, typically single digits) to find the in-progress call an arguments-delta belongs to instead
of a keyed lookup. New test `fold_stream_preserves_the_providers_proposal_order_for_parallel_tool_calls`. Full
`model` test module (19 tests, up from 18) and `cargo build --workspace --tests` pass. **Live and reachable,
not latent:** `fold_stream` runs inside `ConfiguredModel::step`, the sole `LiveModelCall` implementation wired
to both the primary model and every `FallbackChainModel` backend (`interactive.rs`) — this fires on every real
request that returns more than one tool call in a step, not a hypothetical or an unwired mechanism.

**Fresh review pass, 2026-09-04, `apps/rapid/src/goal_host.rs::GoalHost::load` (line 250) and `::load_evidence`
(line 310) — both buffered the entire file into memory via plain `std::fs::read` before any JSON parsing,
the same unbounded-read-before-size-check anti-pattern already fixed 6+ times this session** (`exec_tools.rs`'s
`workspace_read`/`repo_read`/`workspace_patch`/`repo_search`, `host.rs`'s `load_memory_index`/`load_todos_index`,
`managed_config.rs`'s `load_policy`, `user_config.rs`'s `read_config_file`). `goal.json` and `goal-evidence.json`
are normally self-written by this same host, but corruption, a bad merge, or a file arriving via a cloned repo
can still make either arbitrarily large, and both functions are unconditionally live: `GoalHost::load` runs on
every TUI startup via `sync_persisted_goal` (`interactive.rs:287`, called from `interactive.rs:2172`) and on
every real `rapid goal create|show|pause|resume|cancel|complete` invocation via `run_goal_command`
(`interactive.rs:365`); `load_evidence` runs from the same command handler (`interactive.rs:388`). **Fixed:**
both now go through the crate's existing `read_file_bounded` primitive (`exec_tools.rs`, widened from private to
`pub(crate)` earlier this session for exactly this kind of reuse), with two new caps: `MAX_GOAL_FILE_BYTES`
(1 MiB) and `MAX_EVIDENCE_FILE_BYTES` (8 MiB). Both caps were deliberately set well above the schema's own
legitimate ceiling rather than picked arbitrarily — checked directly against `crates/agent-runtime/src/goal/
state.rs` (`MAX_GOAL_STATEMENT_BYTES` 16 KiB + `MAX_CRITERIA` 64 × `MAX_CRITERION_TEXT_BYTES` 4 KiB, a legitimate
`goal.json` ceiling around 272 KiB before JSON overhead) and `crates/agent-runtime/src/evidence.rs`
(`MAX_EVIDENCE_RECORDS` 256 records, each with several 4 KiB-capped string fields, a legitimate evidence-file
ceiling around 6 MiB) — the first-chosen caps (256 KiB / 4 MiB) were actually *below* those legitimate ceilings
and would have rejected real, schema-compliant files as "too large," caught and corrected before committing.
`BoundedReadError::Io`'s `NotFound` case still maps to the existing `Ok(None)`/`Ok(0)` "no file yet" behavior;
any other bounded-read error (including `TooLarge`) maps to the existing `GoalPersistError::Io` variant. New
test `load_bounds_the_read_instead_of_buffering_an_oversized_file`, using a garbage fixture one byte over
`MAX_GOAL_FILE_BYTES` and asserting the specific `Err(GoalPersistError::Io)` variant — verified via the standard
temporary-revert cycle that this genuinely discriminates: reverted to plain `fs::read`, the same fixture no
longer produces `Err(Io)` (the old code buffers the whole garbage file and only then fails to parse it as JSON,
a different error path), confirming the test fails against the original bug before confirming it passes against
the fix. `GoalPersistError` gained `Eq, PartialEq` to support the match-based assertion (`Result::expect_err`
would have required `GoalHost: Debug`, which it isn't). Full `goal_host` test module (9 tests, up from 8) and
`cargo build --workspace --tests` pass.

**Fresh review pass, 2026-09-04, `apps/rapid/src/findings_store.rs::FindingsStore::load` (line 42, at the time
of the fix) — the last outlier of this session's unbounded-read sweep, and the most directly attacker-reachable
one.** `load` called plain `std::fs::read(root.join(FINDINGS_STORE_PATH))`, buffering `.rapidlm/findings.json`
in full before any size check or parsing, the same shape already fixed 7+ times this session. What makes this
one notably more exposed than the others: `FindingsStore::load` isn't just invoked from the explicit `rapid
findings list|dismiss` subcommand (`run_findings`, `p9_commands.rs:780`) — it's called from
`exec_tools.rs::scan_for_secrets_advisory`/`scan_command_advisory`/`scan_patch_advisory` (`exec_tools.rs:3024`,
`:3089`, `:3134`), which run automatically on the ordinary `workspace_write`/`shell_exec`/`workspace_patch` tool
paths (`exec_tools.rs:2649`, `:1721`, `:1751`, `:1774`, `:1783`, `:2883`, `:2886`). Concretely: simply operating
on a cloned/untrusted workspace whose `.rapidlm/findings.json` is oversized (corrupted, appended-to over time,
or planted in a malicious repo) triggers a full unbounded allocation on the agent's very first file write or
shell command in that session — no explicit `rapid findings` invocation needed. The module's own doc comment
already commits to a fail-open contract ("a missing or corrupt store is treated as empty... matching
`exec_tools.rs::load_todos`'s established fail-open convention"), so an oversized file should already have been
in scope for that same bounded-read treatment. **Fixed:** `load` now goes through `read_file_bounded` with a new
`MAX_FINDINGS_STORE_BYTES` (256 KiB, matching `host.rs::MAX_TODOS_INDEX_BYTES` — the module doc already names
that function as this store's convention-sibling). New test
`load_bounds_the_read_instead_of_buffering_an_oversized_file`: since `load`'s contract fails open on *any* error
(missing, corrupt, or too-large all collapse to `Self::default()`), a garbage-oversized fixture would pass
against both old and new code alike and prove nothing — so the fixture instead is **valid, fully-parseable
JSON** with one `reason` field padded past the cap. An unbounded read parses it whole and yields one dismissed
entry; a bounded read rejects it before parsing and yields an empty store — two genuinely different outcomes.
Verified via the standard temporary-revert cycle: reverted to plain `fs::read`, the test failed with "got 1
entries" exactly as predicted, confirming the fixture discriminates before confirming the fix closes it. Full
`findings_store` test module (5 tests, up from 4) and `cargo build --workspace --tests` pass. This closes the
`read_file_bounded` sweep across every currently-reachable `apps/rapid` file-loader found this session;
`preview.rs`'s equivalent gaps were also surfaced by the same review pass but are not live — `PreviewSupervisor`
has zero callers anywhere in the binary (confirmed via grep across `apps/rapid/src`), so left undocumented here
pending it actually being wired up.

**Fresh review pass, 2026-09-04, `apps/rapid/src/hooks.rs::run_hook_once` — two independent bugs in the same
function, both contradicting the module's own stated timeout contract.** Module doc: "Hook commands... are
bounded by a timeout — a hung hook denies rather than hangs the turn." Both `pre_tool_use` and `post_tool_use`
hooks run through `run_hook_once` on every real tool call whenever a project configures either
(`exec_tools.rs:1119`/`:1212`), and `session_start`/`session_end`/`subagent_start`/`subagent_stop` hooks
(`interactive.rs:1247`/`:1716`, `exec_tools.rs:2349`/`:2362`) share the same function.

1. **The stdin write blocked the parent thread with no timeout, before the timeout clock even started.**
`run_hook_once` wrote the hook's stdin JSON via a plain, synchronous `stdin.write_all()` prior to capturing
`started = Instant::now()`. `write_all` on a piped child stdin blocks once the OS pipe buffer fills, until the
child either reads or dies — and the overwhelming majority of hooks (a linter, a notifier, a one-line format
check) never touch stdin at all. `run_pre_tool_hooks`'s `arguments` is the raw tool-call arguments value
(`exec_tools.rs:1122`, e.g. a `workspace_write`'s full file content) and `run_post_tool_hooks`'s `summary` can
carry up to `MAX_SHELL_OUTPUT_BYTES` (16 KiB) of raw shell output, which `serde_json`'s per-control-byte
`\u00XX` escaping can inflate several-fold — either is large enough to fill a typical OS pipe buffer outright.
Verified via the revert cycle with a hook that never reads stdin (`sleep 30`) and a 4 MB stdin payload: the
reverted code's write blocked for the full 30 s until the child exited on its own, at which point
`try_wait()` immediately saw a successful exit and returned `PreHookOutcome::Allowed` — a hung/ignorant hook
being silently **allowed** rather than denied, the opposite of the doc's own contract, not just a slow denial.
**Fixed:** the stdin write now happens on a detached thread, started before the timeout clock; if the hook is
later killed on timeout, its stdin fd closes and unblocks the writer thread with a broken-pipe error, which is
ignored (matching the pre-existing "output to a temp file, never a pipe" rationale already used for the read
side of this same function). New test
`a_large_stdin_payload_does_not_block_past_the_hook_timeout`.

2. **`std::fs::read_to_string` on the captured output silently discarded the entire output when any single byte
anywhere in it was invalid UTF-8** — not just an unbounded-read concern (shape already fixed 8+ times this
session — see `read_capped_bytes` below), but a correctness bug independent of size: `read_to_string` requires
the *whole* buffer to validate as UTF-8, so one stray non-UTF-8 byte from a hook's raw stdout/stderr (common
for any tool emitting binary-ish diagnostic output) zeroed out `output` entirely via
`.unwrap_or_default()`, before the `truncate()` helper that already correctly trims to the last valid UTF-8
boundary ever got a chance to run on the real bytes. **Fixed, alongside the unbounded-read problem in one
motion:** new `apps/rapid/src/exec_tools.rs::read_capped_bytes` (distinct from `read_file_bounded` — this one
never errors on an oversized file, since captured hook/diagnostics output is always going to be truncated to a
hard byte cap regardless; it exists purely to avoid ever buffering more than that cap). `run_hook_once` now
reads through it and passes raw bytes straight to `truncate()`, both bounding the read and preserving valid
output that happens to be followed by a stray invalid byte. New test
`hook_output_survives_a_trailing_invalid_utf8_byte`, using a hook that `printf`s a valid prefix immediately
followed by a raw `\xFF` byte — verified via the revert cycle that reverting just this part (independent of
fix 1) reproduces exactly the predicted failure (`got ""` instead of the valid prefix).

**Same review pass, `apps/rapid/src/shadow_diagnostics.rs::run_diagnostics_once` — the identical
`read_to_string`-discards-valid-output-on-any-invalid-byte bug** (this file already redirects stdio to a temp
file rather than a pipe with no stdin write at all, so bug 1 above doesn't apply here). Same fix
(`read_capped_bytes` + passing raw bytes to the existing `from_utf8_lossy`-based `truncate`), same verification
approach: new test `diagnostics_tail_survives_a_trailing_invalid_utf8_byte` (a `printf`-based diagnostics
command whose output is a valid prefix plus a trailing `\xFF`), confirmed via the revert cycle to fail with an
empty tail against the original `read_to_string` code before confirming it passes against the fix. Live and
reachable: `run_diagnostics_once` backs `verify_candidate`, which runs on every `workspace_write` call when
`shadow_diagnostics` is configured (`exec_tools.rs:1267`).

Full `hooks` (9 tests, up from 7) and `shadow_diagnostics` (8 tests, up from 7) modules — 17 total across both,
up from 14 — and `cargo build --workspace --tests` pass.

**Fresh review pass, 2026-09-04, `apps/rapid/src/context_retrieval.rs::ripple_advisory_inner` (line 218) —
the Next-Edit-Ripple advisory's self-exclusion filter compared the graph's normalized path against the raw,
unnormalized caller-supplied path, so an edited file could wrongly advise about referencing itself.**
`ripple_advisory_inner` correctly normalizes its input up front (`let target = RepoPath::parse(path).ok()?`,
line 192) and uses `target` for the walk match and `symbols_at_path` lookup — but the loop that builds the
"impacted files" set filtered with `impacted_path.as_str() != path` (the *original*, unnormalized string),
not `target.as_str()`. `impacted_path` always comes from the graph's own `symbol_label`, which is always in
canonical `RepoPath` form (no `./`, no redundant separators) — so the comparison only worked by coincidence,
when the caller happened to already pass a canonical path. `RepoPath::parse` drops `.` components
(`crates/protocol/src/repo_path.rs:60-63`), and `checked_relative` (`exec_tools.rs:2672`) explicitly permits
`Component::CurDir`, so a `./`-prefixed path — a common form for LLM-issued tool calls — reaches this function
unnormalized via `args.path` (never itself rewritten, only resolved into a separate `PathBuf`) and
`append_write_advisories` (`exec_tools.rs:1288/1306/1316/1594/1658`) into `ripple_advisory`
(`exec_tools.rs:3019`). **Concrete failure scenario:** a file with a same-file helper→target call
(`fn helper() { target(); } fn target() {}`) edited via a `./`-prefixed path produces a nonsensical advisory —
`"advisory: editing ./same_file.rs may affect code that references it in: same_file.rs — verify they still
work"` — the file naming itself; worse, near the 8-path `MAX_RIPPLE_PATHS_LISTED` truncation boundary, the
spurious self-entry can silently displace a real impacted file from the reported (alphabetically-sorted)
list. Live and reachable: `ripple_advisory` runs on every successful `workspace_write`/`workspace_patch` call
via `append_write_advisories`, not a hypothetical or dormant path. **Fixed:** one-line change, comparing
against `target.as_str()` instead of the raw `path`. New test
`ripple_advisory_excludes_the_edited_file_itself_even_with_a_dot_slash_path` (a same-file caller edited via a
`./`-prefixed path, asserting `None` since there's no other real caller) — verified via the revert cycle to
fail with exactly the predicted self-referential advisory text against the original comparison before
confirming the fix closes it. Full `context_retrieval` test module (7 tests, up from 6) and
`cargo build --workspace --tests` pass. Advisory-only (never blocks a write), so the blast radius is a
confusing/wrong message rather than data loss — but easily triggered given how common both `./`-prefixed
tool-call paths and same-file helper→target patterns are.

**Fresh review pass, 2026-09-04, `apps/rapid/src/web_fetch.rs::is_private_ip` (line 90) — the SSRF guard's
IPv6 arm never unmapped IPv4-mapped IPv6 addresses, a well-known SSRF-filter bypass.** Module doc: "every
resolved address for the URL's host must be public unless the host is on the settings allowlist." An address
in `::ffff:0:0/96` encodes an IPv4 address and is routed by any dual-stack network stack to that embedded
IPv4 destination — but `is_private_ip`'s `IpAddr::V6` arm only checked `is_loopback`/`is_unspecified`/
`is_unicast_link_local`/the ULA range, never calling `to_ipv4_mapped()` first. **Concrete failure:** an
attacker-controlled domain publishing an `AAAA` record for `::ffff:127.0.0.1` (or `::ffff:169.254.169.254`
for cloud instance metadata, or any RFC1918 address) sailed straight through `classify_fetch`, which is
called on every real `web_fetch` tool invocation (`exec_tools.rs:2302`) with an attacker/model-choosable URL.
**Fixed:** both `is_private_ip`'s V4 and V6 arms now share a `is_private_ipv4` helper, and the V6 arm calls
`v6.to_ipv4_mapped()` first, judging a mapped address by its embedded IPv4 rules before falling through to
the existing IPv6-specific checks. New test `classify_refuses_ipv4_mapped_ipv6_addresses` (three IPv4-mapped
literals: loopback, link-local metadata, and RFC1918) — verified via the revert cycle that the original code
returned `Ok(())` for `[::ffff:127.0.0.1]` exactly as predicted, before confirming the fix rejects it. Full
`web_fetch` test module (7 tests, up from 6) and `cargo build --workspace --tests` pass.

**Same review pass, investigated but declined: `crates/llm-router`'s `ip_is_blocked`/`host_is_blocked` have
the identical IPv4-mapped gap, and `http_get`'s IPv4 arm never checks `is_loopback()`/`is_private()` at all —
directly contradicting `allow_private`'s own doc comment ("opts loopback/private targets back in," implying
`false` blocks them).** `web_fetch.rs::fetch_page` always calls `http_get(url, ..., false, ...)`, with its own
comment explaining this second, independent resolution exists specifically to close a DNS-rebind TOCTOU
window (a short-TTL attacker domain answering with a public IP on `classify_fetch`'s lookup and a
private/loopback IP on `http_get`'s lookup immediately before connecting) — a window that in fact stays open
today for every class `ip_is_blocked` doesn't check. Attempted the equivalent fix (broadening `ip_is_blocked`'s
IPv4 arm to `is_loopback`/`is_private`/`is_link_local`, unmapping IPv4-mapped IPv6 first) and it **failed 20
existing tests** across `providers::anthropic` and `providers::openai_compatible` — `cancellation_is_not_
swallowed`, `auth_failure_is_not_remapped_to_transient`, several streaming/normalization tests, all of which
bind a real `TcpListener` on `127.0.0.1` to exercise the actual provider transport. `ip_is_blocked`/
`host_is_blocked` are shared by both `http_get` (the one tool-facing caller with a genuine untrusted-URL SSRF
concern) *and* the main provider connection path, which by design must be able to reach a self-hosted/local
OpenAI-compatible endpoint (LM Studio, Ollama, an on-prem gateway) — for that path, blocking loopback/RFC1918
outright would be a regression, not a fix, since a configured `base_url` pointing at a local server is the
intended use, not an attacker-controlled URL. Reverted the `llm-router` change entirely (`git diff` confirms
zero delta) and kept only the self-contained `web_fetch.rs` fix above, which fully closes the *first*-resolution
gap `classify_fetch` owns. **Flagging for a future pass, not fixing now:** closing the residual DNS-rebind
window on the `http_get` side needs either a second, stricter blocklist function used only by `http_get`'s
`allow_private: false` path (distinct from the shared provider-transport one), or some other way to give
`fetch_page`'s specific untrusted-URL threat model a check that doesn't also constrain legitimate self-hosted
provider endpoints — a real design decision, not a mechanical one-line fix, so declining to rush it.

**Same review pass, `apps/rapid/src/pdf_text.rs::extract_pdf_text` (line 39) — a single corrupted or
chain-filtered stream discarded every page of text already extracted from earlier streams in the same PDF.**
Doc comment: "Returns `None` when the input is not a PDF or contains no harvestable text operators." The loop
scans every `stream...endstream` object in the file, pushing each one's harvested text into `collected` — but
`inflate(&bytes[data_start..data_end])?` propagated a `None` from a single failed `FlateDecode` (truncated
data, bit-flip corruption, or a chained filter like `[/ASCII85Decode /FlateDecode]` that the substring-based
`is_flate` detection doesn't account for) straight out of the *whole* function via `?`, discarding every
page already pushed into `collected` and never scanning any later object either. **Concrete failure:** a
3-stream PDF where streams 1 and 3 are valid FlateDecode with real text and stream 2 is corrupted returns
`None` for the entire file — reported to the model as "no extractable text (scanned or encoded content)" for
a document that is in fact mostly text-extractable. Live and reachable: `extract_pdf_text` runs on every
`.pdf` read through `workspace_read`/`repo_read` (`exec_tools.rs:1352`). Checked this file for the same
unchecked-byte-offset panic shape already fixed once this session in `pdf_page_count` — traced every slice
operation by hand and found none recur here (all offsets are `find()`-derived or bounds-checked), so this is
an isolated finding, not a sibling of that one. **Fixed:** `inflate(...).unwrap_or_default()` instead of `?`
— a failed stream contributes empty content (skipped by the existing `page_text.trim().is_empty()` check)
rather than aborting the file. New test
`a_corrupted_stream_does_not_discard_text_already_extracted_from_earlier_streams` (3-stream PDF, middle one
garbage) — verified via the revert cycle to fail (whole-file `None`) against the original `?`-based code
before confirming the fix preserves both surrounding pages' text. Full `pdf_text` test module (6 tests, up
from 5) and `cargo build --workspace --tests` pass.

**Same review pass, `apps/rapid/src/external_agents.rs::MAX_AGENT_PROMPT_BYTES` (256 KiB) silently exceeded
the transport's real, hard-enforced limit (`process_supervisor::MAX_STDIN_BYTES`, 64 KiB), so any prompt in
between constructed successfully only to fail deterministically later.** `ExternalAgentTask::new` validates
`prompt.len() > MAX_AGENT_PROMPT_BYTES`, but `SupervisedCliRunner::prepare` sends the prompt as the child's
stdin via `StdinSpec::Bytes`, and `ExecSpec::build`'s own validation (`crates/process-supervisor/src/
spawn.rs:336`) hard-rejects anything over `MAX_STDIN_BYTES` with `SpawnError::StdinTooLarge` — which
`prepare()` then collapses into the generic `ExternalAgentError::Supervised("spec".into())`, losing the real
cause entirely. **Concrete failure:** any `rapid agent-cli <prompt> -- <argv>` invocation (real command,
`p9_commands.rs:286`, wired in `interactive.rs:339`) with a prompt between 64 KiB and 256 KiB — an entirely
ordinary size, e.g. pasting a sizeable code excerpt — passes construction (`ExternalAgentTask::new` says it's
fine) and then always fails at `prepare()` with a generic, unhelpful error that gives no hint the real limit
is 4x smaller than advertised. **Fixed:** `MAX_AGENT_PROMPT_BYTES` now aliases `process_supervisor::
MAX_STDIN_BYTES` directly instead of a separately-chosen, larger number — nothing in the 64–256 KiB range
ever succeeded before this fix either, so this is a pure improvement (an immediate, specific
`PromptTooLarge` at construction instead of a deferred, generic failure), not a capability regression. New
test `new_rejects_a_prompt_too_large_for_the_stdin_transport` (a `MAX_STDIN_BYTES + 1`-byte prompt) — verified
via the revert cycle that it passed construction under the old 256 KiB cap exactly as predicted, before
confirming the fix rejects it. Full `external_agents` test module (9 tests, up from 8), including the real
`supervised_cli_runner_drives_real_child_through_broker_lease` integration test, and
`cargo build --workspace --tests` pass.

**Same review pass, investigated but declined: `crates/process-supervisor::spawn()` writes a spawned child's
stdin synchronously (`write_stdin`, blocking `pipe.write_all`) before returning, with no timeout and nothing
yet draining the child's stdout/stderr concurrently — a mirror-image of the exact stdin-write-deadlock shape
just fixed in `apps/rapid/src/hooks.rs::run_hook_once` this session, but here in a shared crate underlying
every `process_supervisor::spawn()` caller, not a single self-contained file.** A child that emits any
startup output before fully draining stdin (banners, verbose logging — common) combined with a stdin payload
near or over the OS pipe buffer size can deadlock both sides before `await_exit_draining`'s timeout/grace
logic ever begins, since that logic only starts after `spawn()` returns. `external_agents.rs`'s
`DEFAULT_AGENT_TIMEOUT` (600s) would never actually bound such a hang. Declining to fix this pass: unlike the
`hooks.rs` fix (one self-contained function in one file), `process_supervisor::spawn()` is a shared primitive
used by `external_agents.rs` and `plugin-host::hooks.rs` alike, and the llm-router revert above already showed
this session's own risk-assessment intuition for "should be safe" changes to shared crates isn't reliable
without the full test suite — the right fix here likely mirrors `hooks.rs`'s detached-writer-thread pattern,
but should be scoped and reviewed as its own pass across every real caller rather than folded into this batch.

## 0. Where RapidLM actually stands today (read this before the tables below)

`gaps.md` is a living document and parts of it are now stale. Commit `ac66e8a` ("Wire the gaps.md parity
core into the exec loop", 28 Aug) already closed several items that document's own tables still list as
open: per-call tool-result messages, parallel batch dispatch, the six-mode permission lattice, and
`AGENTS.md`/`.claude`/`.cursor` rule discovery. Commit `687c745` (29 Aug, this session's baseline) then
wired context-engine retrieval into the exec loop. **Do not re-derive priorities from `gaps.md`'s
executive-summary table without cross-checking current source** — this document's Phase 1 has already
done that cross-check. Confirmed current state, direct from source:

- 15 model-callable tools (`apps/rapid/src/exec_tools.rs`: `workspace.write`, `workspace.read`,
  `repo.read`, `repo.search`, `workspace.patch`, `repo.glob`, `todo_write`, `plan_enter`, `plan_exit`,
  `job_status`, `job_output`, `task_spawn`, `ask_user`, `web_fetch`, `shell_exec`).
- Batched concurrent dispatch (reads parallel, same-path writes serialize) and per-call tool-result
  messages — closed in `ac66e8a`.
- Six-mode permission lattice (`apps/rapid/src/permissions.rs`: `Default | Plan | AcceptEdits | Auto |
  DontAsk | BypassPermissions`).
- `shell_exec` has opt-in macOS Seatbelt sandboxing (`find_sandbox_exec()`, `sandbox-exec -f <profile>`) —
  **macOS only**, no Linux backend.
- `task_spawn` exists; read-only "specialist" subagents cannot inherit the parent's capability lease
  (`agent_runtime::specialist::PersistentSpecialist::inherits_parent_lease()` hardcoded `false`) — but
  there is **no write-scoped, narrowly-leased child run path at all** (`PersistentSpecialist::new()`
  rejects any non-read-only role outright).
- Context-engine retrieval (FTS + code graph + Context Scout) is now reachable from `rapid exec` for
  trusted workspaces (`apps/rapid/src/context_retrieval.rs`, wired `687c745`) — previously ~92% of that
  crate was dead weight.
- Shadow-verified writes exist for `workspace_write` only (`apps/rapid/src/shadow_diagnostics.rs`,
  `02a583b`) — `workspace_patch` is not covered yet.
- `NodeState::Paused` exists at the graph-scheduler layer (`GraphService::pause`/`resume`,
  `crates/scheduler/src/service.rs`, `02a583b`) — no OS-level process suspension, and `rapid graph` has no
  CLI surface at all despite being listed in `00-README.md`.
- No public distribution: build from source only.

---

## Phase 1 — Close parity against Grok Build and Qwen Code

Items are grouped by dimension. Each row states the gap, which competitor(s) already close it, and where
it lands in RapidLM.

### 1.1 Sandboxing breadth

**Correction (2026-08-29, during implementation):** the original framing of this row was wrong. It is
not "no native Linux sandbox" — `crates/sandbox` already contains a mature, tiered, cross-platform
implementation (`backend.rs`'s `SandboxManager`/`SandboxBackend` trait plus four backends:
`HostRestrictedBackend` — process-group + resource limits, unix/windows via `cfg` —, `ContainerBackend`,
`GvisorBackend` (shells to `runsc`, genuinely stronger isolation than raw Landlock — syscall mediation, not
just filesystem policy), and `RemoteBackend`; ~11,400 lines total). **The real gap is identical in shape
to the context-engine finding from commit `687c745`: the crate is fully built and unwired.**
`apps/rapid` does not depend on the `sandbox` crate at all (absent from `Cargo.toml`, zero `use sandbox::`
call sites). `shell_exec`'s `sandbox: true` path (`apps/rapid/src/exec_tools.rs`, `find_sandbox_exec()`)
is a small, separate, macOS-only inline `sandbox-exec` (Seatbelt) invocation that predates and never
touches `SandboxManager`'s tier system. Neither implementation currently knows the other exists.

| # | Gap | Grok Build | Qwen Code | Where it lands | Sev | Effort |
|---|---|---|---|---|---|---|
| 1 | ~~Second correction: wiring `SandboxManager` in turned out to be blocked on a deeper, crate-level gap~~ **Output-capture half implemented 2026-08-29.** `SandboxExecResult` gained an `output: Vec<u8>` field (sibling to `exit`, not folded into `ResourceUsage` — that struct's "counters only" doc comment is specifically about itself, not its parent). `HostRestrictedBackend`, `ContainerBackend`, and `GvisorBackend` (all three actually reachable — `RemoteBackend::exec` is an intentional stub that always returns `Err(HealthFailed)`, "No Firecracker transport in this protocol task," so it had nothing to fix) each had an internal `join_output`/`wait_child` pair that already read the child's stdout+stderr into a real `Vec<u8>` buffer via `read_capped`, then threw the buffer away and returned only its byte *count* — all three shared byte-for-byte identical logic (a copy-paste common ancestor), so the same fix applied cleanly to each: `join_output` now returns the combined, already-capped bytes instead of a length; `WaitOutcome`'s variants carry `output: Vec<u8>` instead of `output_bytes: u64`; the existing `ResourceUsage` byte counter is now derived from `output.len()` at the one point it's still needed, keeping that struct's own "counters only" property intact. `cargo build -p sandbox --tests` and the crate's full test suite both pass with the real byte-for-byte identical fix applied three times. ~~Still not done, and this is genuinely the app-level wiring task now, not a hidden crate gap:~~ **Wiring implemented 2026-08-30.** New `apps/rapid/src/sandbox_exec.rs` module: `sandbox` added as an `apps/rapid` dependency; `SandboxManager` registers `HostRestrictedBackend` only (deliberately — `ContainerBackend`/`GvisorBackend` need a real tier-selection/degrade policy, tracked as its own follow-up, not guessed here); `run_sandboxed(root, argv, timeout, output_limit)` builds a `SandboxSpec` (mounting `root` read-write at a fixed `workspace` `RepoPath` target and setting `cwd` to that same target — this is the `cwd`/`mount` relationship flagged as unresolved in the reconnaissance note below, now resolved), mints a real single-use `Capability::ProcExec` lease via the full capability-broker ceremony (`PolicyDocument::parse_toml` → `PolicyStack` → `ActionRequest` → `evaluate` → `request_approval` → `resolve(Approve(Once))` → `issue`, using `LeaseIssuer::ephemeral()` — the first production code path in the repo to mint a real `CapabilityLease`; previously only test fixtures did), then runs `prepare`/`exec`/`destroy`. `exec_tools.rs`'s `execute_shell` routes `sandbox: true` through it on the non-macOS-or-no-`sandbox-exec` fallback path, synchronously, returning exit code / timeout / captured combined output. Hit and fixed one real bug along the way, not a crate gap: `HostRestrictedBackend` deliberately rejects a relative or bare `argv[0]` (`relative_executable_is_rejected` is an existing, intentional test) — unlike `std::process::Command`, it does no implicit `$PATH` search — but real `shell_exec` calls pass bare/relative names (`"ls"`, `"./script.sh"`, matching `exec_tools.rs`'s own existing tests), so `sandbox_exec.rs` now resolves `argv[0]` itself (absolute passthrough, root-relative for a path containing `/`, `$PATH` search otherwise) before handing the request to the sandbox crate. All 5 of `sandbox_exec`'s own tests, the full `sandbox` crate suite (83 tests), and the full `rapid` lib suite (254 tests) pass. **Deliberately not done:** `ContainerBackend`/`GvisorBackend` registration (tier-selection policy, item #2 below is the closest open slot for that decision), unifying this synchronous path with the macOS Seatbelt path's async job-based one (see the module's own doc comment).
**Reconnaissance done this pass** (dependency added, backends registered, then reverted rather than leave
an unused half-wired dependency — see below): `HostRestrictedBackend::new()` / `ContainerBackend::new()` /
`GvisorBackend::new()` all take no arguments and can be registered unconditionally — `SandboxManager::select`
already health-checks each and only picks an available one, so no platform-conditional registration logic
is needed. The real remaining unknown is `SandboxSpec`'s `cwd`/`mount` model: `cwd` takes a `RepoPath`
(*repo-relative*, not a host path) and `SandboxSpecBuilder::build()` requires it (`SandboxError::Empty`
otherwise); the actual host directory is supplied separately via `.mount(SandboxMount { source:
CanonicalHostPath, target: RepoPath, mode })` — getting the `cwd`/mount relationship right for a real
project root (as opposed to the existing tests' temp-workspace fixtures) needs to be worked out carefully,
not guessed, before wiring `shell_exec` through it. | Landlock + Seatbelt + child seccomp, named profiles | Docker/Podman only (also no native Linux sandbox — this is a real RapidLM opportunity: `GvisorBackend`'s syscall mediation is a stronger property than Qwen's container-only story) | `crates/sandbox/src/backend.rs` (`SandboxExecResult`, done) + `apps/rapid` (wiring, done) | P0 (done) | L |
| 2 | ~~No macOS backend registered with `SandboxManager` at all~~ **Backend implemented and tested 2026-08-30; `apps/rapid` wiring deliberately not attempted yet — see below.** New `crates/sandbox/src/backends/seatbelt.rs::SeatbeltBackend: SandboxBackend`, registered at `SandboxTier::HostRestricted` exactly as this row anticipated (`IsolationStrength` auto-derives from tier, so no `protocol::SandboxTier` widening was needed at all — mechanical, not a judgment call). Reuses `host_restricted.rs`'s own already-tested mount/cwd-resolution and forbidden-host-source logic (`resolve_cwd`, `resolve_existing_dir`, `is_forbidden_host_source`, all made `pub(crate)` for this) rather than risk a second, subtly different copy of security-relevant path validation — this is genuinely different from this session's usual "duplicate a trivial 3-line helper" pattern precisely because this logic isn't trivial (docker-socket/home-dir/`.ssh`/`.aws`/keychain prefix checks, symlink-safe canonicalization). Builds its own `.sb` profile per prepare (temp file, cleaned up in `destroy`): deny-all-writes then allow only the resolved read-write mount roots plus `/dev/`/`/private/tmp/`, matching `exec_tools.rs::seatbelt_profile`'s existing shape — but only `SandboxNetwork::None` is accepted, and for that case the profile also adds `(deny network*)`, a real capability neither the existing job-based Seatbelt path nor `HostRestrictedBackend`'s process-policy-only isolation has today. This rule ordering (`deny` before a trailing `(allow default)`) was verified empirically against the real `sandbox-exec` binary on this dev machine before writing any Rust — a `(deny network*)`/narrow `(allow file-write* (subpath ...))` earlier in the profile is NOT undone by `(allow default)` later, confirmed via direct `sandbox-exec` invocations (curl DNS failure with the deny present vs. HTTP 200 without it; a real write outside the allowed subpath returns `Operation not permitted`). Tested with 11 new tests including two real, non-mocked security-property tests only possible because this dev environment is macOS: `prepare_exec_destroy_confines_writes_to_the_mounted_root` (a write inside the mount succeeds, a real write to a path outside it is denied by the OS, not by any of this backend's own bookkeeping) and `network_is_genuinely_denied_not_just_unrequested` (a real `curl` to a real host fails under the profile). Full `sandbox` crate suite (94 tests, up from 83) and `cargo build --workspace --tests` both pass. **Deliberately not attempted:** wiring `apps/rapid` to actually use this backend instead of `exec_tools.rs`'s existing async job-based Seatbelt path — that would mean giving up the background-job/poll-via-`job_status` model for sandboxed `shell_exec` on macOS in favor of this backend's synchronous contract, a real user-visible behavior change (not just an internal refactor) that deserves its own scoping and test-migration pass, not a rider on this one; `apps/rapid/src/sandbox_exec.rs`'s own doc comment already flagged this exact split as deliberately deferred. `ContainerBackend`/`GvisorBackend` registration (tier-selection policy) also remains untouched, per item #1's own note. | Landlock + Seatbelt + child seccomp, named profiles | Docker/Podman + macOS Seatbelt (six `.sb` profiles) | `crates/sandbox/src/backends/seatbelt.rs` (backend, done) + `apps/rapid` (wiring into `execute_shell`, not done) | ~~P1~~ P1 (partial) | M |
| 3 | Sandbox profile selection isn't policy-coupled to permission mode | Auto-allow keying between sandbox profile and approval mode | Partial (YOLO does **not** imply sandboxing — documented explicitly; don't copy this gap) | `capability-broker` + `sandbox` — gated on #1/#2 landing first | P1 | S |
- **Sharper consequence of row 2's "paths deliberately left unmerged" note, found 2026-08-30 while giving
  `SeatbeltBackend` full CPU/memory/pid-count parity (§2.10's own entries below) — the two paths don't just
  differ in ergonomics (sync vs. background-job/poll), they differ in what safety properties actually
  apply.** Traced `exec_tools.rs::execute_shell`'s `args.sandbox` branch (macOS, the *default* path
  whenever `find_sandbox_exec()` succeeds — `crates/sandbox::run_sandboxed` is reached only on non-macOS or
  a missing `sandbox-exec` binary) all the way through `JobRegistry::start`'s supervisor thread
  (`exec_tools.rs:224-380`, the same function `args.background` jobs use): its loop checks exactly three
  things — child exit, cancellation, and wall-clock `timeout` — and nothing else. No CPU, no memory, no
  process-count check anywhere in that path. **This means the CPU/memory/pid-count enforcement this whole
  session built out to full parity across all four `crates/sandbox` backends (§1.1 row 1, §2.10 below)
  provides zero real protection for the actual default sandboxed `shell_exec` path on the platform most
  contributors and CI likely run on.** The Seatbelt *profile* (filesystem/network confinement) still
  applies on this job-based path — only the resource-ceiling half is absent, on top of the wall-clock
  timeout and `MAX_JOB_OUTPUT_BYTES` output cap that already exist. Not attempted here: porting equivalent
  monitoring into `JobRegistry::start` would need `crates/sandbox`'s process-group sampling helpers
  (`sample_process_group`/`terminate_process_group`, currently `pub(crate)` within that crate) exposed as
  real public API, or a second, non-trivial copy of that same shell-out-to-`ps`/`pgrep` logic — either is a
  real design/API-surface decision, not a rider on the parity work that surfaced it, and row 2's own note
  already correctly scoped "unify the two paths" as separate, dedicated follow-up rather than something to
  rush alongside other work.

### 1.2 Multi-agent / subagents

**Correction (2026-08-29, during implementation):** row 4 below was wrong as originally written. It
conflated two unrelated constructs. `agent_runtime::specialist::PersistentSpecialist` (a long-lived,
read-only background *observer* — "Context Curator," §3 of the agent-harness design, never a `task_spawn`
target) is correctly read-only-or-reject; that part of `gaps.md` finding #8 is accurate for *that* type.
But `task_spawn`'s actual runtime (`apps/rapid/src/interactive.rs`'s `LiveSubagentRunner::run`) is a
**separate, write-capable path that already exists**: any `agent_type` other than `"explore"`/`"plan"`
gets `ExecTools::workspace_with_permissions` (full write access), and — this is the real gap — it is
constructed with `self.permissions.clone()`, i.e. **the spawned child gets a byte-for-byte copy of the
parent's entire permission lattice** (mode, rules, persisted grants), not a narrowed one. The problem
isn't "no write-scoped child run exists" — it's "the one that exists doesn't scope down at all."

| # | Gap | Grok Build | Qwen Code | Where it lands | Sev | Effort |
|---|---|---|---|---|---|---|
| 4 | ~~Write-capable `task_spawn` children inherit the parent's full `PermissionLattice` unmodified~~ **Implemented 2026-08-29.** `PermissionLattice::for_subagent()` (`apps/rapid/src/permissions.rs`) caps `BypassPermissions` down to `AcceptEdits` for a spawned child — the one mode with no `Ask` step at all — while carrying rules/grants over unchanged; every other mode already denies non-file-edit calls for a subagent (headless-style execution, no interactive channel, `Ask` renders as denial per `evaluate`'s doc comment), so only that one mode needed a ceiling. Wired into `LiveSubagentRunner::run` (`interactive.rs`). New test `subagent_lattice_caps_bypass_but_leaves_every_other_mode_and_rules_alone`. | Subagents in-loop, `general-purpose`/`explore`/`plan` naming (narrowing depth not fully profiled here) | `general-purpose`/`Explore`/`review-agent`/`fork` subagent types, tool-set hard-restricted per role **at runtime**, not just by prompt | `apps/rapid/src/interactive.rs::LiveSubagentRunner::run` + `crate::permissions::PermissionLattice` | ~~P0~~ done | M |
| 5 | ~~Subagent results return as flat text~~ **Implemented 2026-08-29.** New `SubagentReport` struct (`summary`, `status`, `tool_calls`, `tokens`, `stop_reason`) carried across the `SubagentRunner` trait boundary instead of flattening to a `String` inside `LiveSubagentRunner::run`; `execute_task_spawn` renders the final text from the typed fields. Deliberately only carries what `ExecOutcome` actually produces today — no invented "touched files"/"proposed patches" fields nothing populates (a fuller Modbit-style `AgentResultEnvelope` is still future work, this is the trait-boundary half of it). Test mock and assertions updated to check the structured fields reach the rendered summary, not just the free-text body. | Not established | Structured completion via `functionResponse`/tool-result parts, but no dedicated child-result schema either | `apps/rapid/src/exec_tools.rs` (`SubagentRunner` trait, `SubagentReport`) + `interactive.rs` (`LiveSubagentRunner`) | ~~P1~~ done | M |
| 6 | No competitive/parallel multi-model execution mode | None | **Agent Arena**: 2–5 models race in isolated `git worktree`s, automated `git apply` merge-back of the winner | See Phase 3 §3.1 — do not build this until the Change Engine hardening in Phase 2 §2.1 lands; an automated merge without a real `MergeTransaction` primitive is how you corrupt a tree | P2 | — (gated on §2.1) |

### 1.3 Extensibility

| # | Gap | Grok Build | Qwen Code | Where it lands | Sev | Effort |
|---|---|---|---|---|---|---|
| 7 | ~~Hooks cover only `pre_tool_use`/`post_tool_use`~~ **Implemented 2026-08-29.** Added `session_start` (fires once per `rapid exec` run, right after settings load), `session_end` (fires on *every* exit path of `exec_turn` — early `?`-propagated error, explicit early return, or falling off the end — via a `SessionEndHookGuard` whose `Drop` impl fires exactly once regardless of which path was taken; this was initially skipped as "too risky to intercept every return point" and then actually implemented once the `Drop`-guard approach was worked out, rather than left half-done), `subagent_start`/`subagent_stop` (fire around `task_spawn`) — all four via a new notification-style `run_notify_hooks` (fire-and-collect, never gates, unlike `pre_tool_use`). 6 events total now, up from 2. Still far short of Grok Build/Qwen's ~15 events, no `http`/`function`/`prompt` executor types (command-only, matching the existing `pre_tool_use`/`post_tool_use` shape). | 15 events, blocking semantics | ~15 events across 4 executor types (`command`/`http`/`function`/LLM-judged `prompt`), parallel by default | `apps/rapid/src/hooks.rs`, `interactive.rs` (`SessionEndHookGuard`) | ~~P1~~ done (partial breadth) | M |
| 8 | ~~No cross-CLI config/plugin import~~ **Partially implemented 2026-08-29.** Found and fixed a real inconsistency while scoping this: `exec_permission_lattice` already merged permission rules across *both* `.rapidlm/settings.json` and `.claude/settings.json` (`PROJECT_SETTINGS_FILES`), but the separate block loading hooks/MCP-servers/shadow-diagnostics/fetch-allowlist only ever read `.rapidlm/settings.json` — a `.claude/settings.json`-only project silently lost all four. Extracted into `load_project_integrations()`, which merges list-shaped config (allowlist, each hook stage, MCP servers) across every settings file the same way permission rules already do, and uses first-file-wins for the one single-value config (shadow-diagnostics), matching `exec_permission_mode`'s own precedence. **Still not done:** full plugin-manifest/marketplace import (Claude Code Marketplace plugins, Gemini CLI extensions) — this only closes the settings-file compat gap, not a plugin-package importer; that remains real, separate, larger work requiring an authoritative (not guessed) plugin manifest schema. | Reads `.claude/settings.json` for permission rules only | Converts and installs **Claude Code Marketplace plugins**, Gemini CLI extensions, Qoder plugins; `/import-config claude-code` | `apps/rapid/src/interactive.rs` (`load_project_integrations`, `ProjectIntegrations`) | P1 (partial) | M |
| 9 | ~~MCP tool schemas are not lazily hydrated~~ **Partially implemented 2026-08-30: the 20 KB cap half.** `apps/rapid/src/exec_tools.rs::full_surface_impl` unconditionally appended *every* MCP-registered tool's full schema to the model-facing surface (confirmed: no size check anywhere in the loop) — a misconfigured or adversarial MCP server could advertise arbitrarily many tools with arbitrarily large schemas, re-sent on every model request for the rest of the turn. Added `MAX_MCP_TOOL_SURFACE_BYTES` (20 KB, matching Qwen's own cited number), enforced first-registered-wins: once the cumulative name+description+schema size crosses the cap, later registrations are skipped (a `--verbose`-gated stderr note reports how many). **Not attempted: the actual lazy-hydration redesign** (`search_tool`/`use_tool` meta-tools replacing eager injection with on-demand discovery) — that's a real, separate protocol change to how the model discovers and invokes MCP tools at all, not a bounding fix; scoping it well needs a real decision about the discovery-tool contract and whether directly-advertised tools coexist with indirected ones, not attempted here. | Meta-tools (`search_tool`/`use_tool`) | Same idea, plus 20 KB cap on eager injection | `apps/rapid/src/exec_tools.rs` (cap, done) + `mcp` crate (lazy hydration, not done) | P2 (partial) | S |
| 10 | ~~No SDK-style typed tool-schema export~~ **Implemented 2026-08-29.** New `rapid tools` subcommand (`apps/rapid/src/p9_commands.rs::run_tools_schema`) dumps the model-facing tool surface — every tool's `name`/`description`/`parameters` (already-existing JSON Schema per tool, confirmed via the pre-existing `tool_surface_advertises_all_sixteen_tools_with_json_schemas` test) — as a single versioned JSON document (`rapidlm.tool_surface` schema), analogous to Claude Code's `sdk-tools.d.ts` or Grok Build's protobuf tool API. `--read-only` dumps the narrower surface a subagent's `explore`/`plan` scope gets instead of the full one; `--root <path>` points it at a real project (introspective only, never writes). Note this reuses `ExecTools::tool_surface()`, which already existed — the actual gap was purely "no CLI command exposes it," not missing schema data. | Protobuf tool API (`xai-grok-tools-api`) | `sdk-tools.d.ts`-equivalent not present either | `apps/rapid/src/p9_commands.rs` (`run_tools_schema`) | ~~P2~~ done | S |

### 1.4 Memory & context

**Correction (2026-08-29):** rows 13 and 14 below are narrower than first written — spot-checked
against source during this pass (not as thoroughly as the sandboxing/cost-accounting corrections
above, flagging as lower-confidence rather than re-scoping in full):
- `crates/context-engine/src/compact.rs::compact_packet` already has a `CompactMethod::Deterministic`
  path used as the built-in fallback whenever no model summarizer is supplied or one fails — i.e. a
  mechanical, no-model-call compaction mode already exists at the context-engine layer. What's
  unconfirmed is whether it's reachable as an explicit, user-requested "always mechanical, skip the
  summarizer" mode (row 13's actual ask) rather than only as an automatic fallback.
- A form of session export already exists: `rapid inspect export <session> <file>`
  (`apps/rapid/src/p9_commands.rs::run_inspect_export`) writes raw ledger events as JSONL. Row 14's
  real gap is narrower than "no export exists" — it's the richer `html`/`md` rendered-transcript
  formats, not the JSONL case.
Re-verify both before implementing rather than trusting the original row text.

| # | Gap | Grok Build | Qwen Code | Where it lands | Sev | Effort |
|---|---|---|---|---|---|---|
| 11 | No background memory-consolidation pass | Not established | **"Dream"**: LLM-planned dedup/cleanup over saved memories, daily or on-demand | `context-engine::memory` (crate exists per `gaps.md` remediation item 15 — extend, don't replace) | P1 | M |. **Correction (2026-08-30): confirmed gated on the same "crate is real, `apps/rapid` never calls it" foundation gap found repeatedly elsewhere in this document, checked directly rather than assumed.** `context_engine::memory::MemoryStore` is a real, complete, tested SQLite-backed store (`MemoryWrite`/`MemoryRecord`/`MemoryQuery`, TTL/confidence/provenance fields) — but `grep -rl "context_engine::memory" apps/rapid/src` returns nothing: zero call sites. `apps/rapid` only ever persists memory as the flat `.rapidlm/MEMORY.md` pointer file (`host.rs::load_memory_index`, §1.4 row 12's own git-committed-team-memory correction). A consolidation *pass* over memories that are never actually written to the structured store this item names has nothing to consolidate — this is the identical shape to §3.3's `GoalUsage`/`GoalDriver` finding (a real mechanism, a real crate, and a turn loop that never calls into it), not a new, independent gap. Wiring `context_engine::memory` into `apps/rapid`'s actual write/read paths is itself the real prerequisite, undocumented as its own item anywhere in this file; re-scoping this row's true blocker accordingly rather than leaving the M-effort estimate implying "just add a consolidation job" is the missing piece.
| 12 | ~~No git-committed team-shared memory tier~~ **Partially implemented 2026-08-30: the "mandatory secret scanner" half only.** `.rapidlm/MEMORY.md` (`apps/rapid/src/host.rs::load_memory_index`) turned out to already BE this codebase's git-committed, team-shared memory tier — confirmed not gitignored (`git check-ignore` on it returns nothing), always loaded into every turn's context. What it lacked was Qwen's "mandatory," i.e. blocking, secret scan: `workspace_write` already ran the secrets scanner over every write, but only ever advisory (append a note, never refuse). Added `exec_tools.rs::is_team_memory_path` + a gate at the top of `execute_write`: when the target is exactly `.rapidlm/MEMORY.md`, a non-dismissed finding now returns `ToolStepResult::Failed` (write never touches disk) instead of an advisory note — verified before writing, same "never touch the real tree on a failed check" discipline shadow diagnostics already uses. A `rapid findings dismiss`-ed fingerprint still unblocks the write, reusing the exact same `FindingsStore` every other scanner in this file shares. Every other file keeps the advisory-only behavior; this is a narrow escalation for one specific path, not a policy change to `workspace_write` generally. **Explicitly not attempted:** the actual `.qwen/team-memory/`-style directory tier (multiple files, not one pointer file), and `context-engine::memory`'s SQLite-backed store remains completely unwired from `apps/rapid` (zero call sites, confirmed by grep) — this fix operates entirely at the `.rapidlm/MEMORY.md` text-file layer, not that structured store. | Not established | `.qwen/team-memory/` with a mandatory secret scanner before commit | `context-engine::memory` + `security::scanners::secrets` (a `Finding`-lifecycle type already exists for secrets — reuse it, per `gaps.md` finding #4) | ~~P2~~ P2 (partial) | M |
| 13 | No *user-requested* mechanical (non-model-call) compaction fast-path — the deterministic path already exists as an automatic fallback (see correction above), just maybe not as an explicit mode | Not established | `/compress-fast`: strips old tool output/thinking with no model call | `context-engine::compact` (already has the primitive) + wherever compaction is user-triggered | P2 | S |. **Correction (2026-08-30): "wherever compaction is user-triggered" turned out to be nowhere real, and the gap is bigger than S.** The TUI already advertises a `/compact` slash command (`crates/tui/src/commands.rs:532`, dispatches `KernelAction::CompactSession`) and there's already a `SessionLifecycleIntent::Compact`/`CompactSessionIntent` type for it (`crates/tui/src/session_actions.rs`) — traced the whole path expecting to find *some* existing hook into `compact_packet` to expose the `Deterministic` method on. There isn't one: `CompactSession` isn't specially handled anywhere; `KernelAction::kernel_api()` (`crates/tui/src/commands.rs:827`) falls through its catch-all to `KernelApi::Dispatch` for it, and `apply_kernel_action`'s match on that (`apps/rapid/src/interactive.rs:2015`) is `KernelApi::Approve | KernelApi::Dispatch => {}` — a silent no-op, not even a stub message. Typing `/compact` today does literally nothing. Deeper still: `crates/context-engine::compact::compact_packet(packet: &ContextPacket, ...)` operates on a `ContextPacket`, and grepping confirms `ContextPacket` has zero references anywhere in `crates/tui` or the interactive TUI session loop in `apps/rapid/src/interactive.rs` — the *only* place in the whole app that builds one is `exec_turn`'s one-shot `build_live_context` (used for `rapid exec`, a single non-interactive turn, not the persistent TUI session). So making `/compact` do anything real isn't "expose an existing primitive as an explicit mode" (the original S-effort framing) — it first needs the TUI's live session loop to track a `ContextPacket`-equivalent at all, which today it structurally does not. That's real, separate architecture work (state the persistent session's context blocks somewhere `apply_kernel_action` can reach), not a small wiring fix. Re-scoping to M/L and flagging the `/compact` no-op as its own concrete, verified bug independent of this item's original ask.
- **Real gate bypass found and fixed 2026-08-30, row 12 above: the "narrow escalation for one specific
  path" turned out to only cover half of that one path.** The mandatory gate was only ever checked in
  `execute_write` — `workspace_patch` edits the exact same file's content through a completely separate
  function (`execute_patch`, two tiers) and never called `is_team_memory_path` at all. A model could
  bypass the mandatory gate trivially: write a clean `.rapidlm/MEMORY.md` via `workspace_write` (passes,
  nothing to flag), then splice a secret in via `workspace_patch` on the same file — confirmed with a real
  reproduction before fixing: the patch succeeded with only an advisory note
  (`"replaced 1 occurrence(s)... advisory: possible secrets detected..."`) and the secret genuinely landed
  in the git-committed, team-shared file. Same "mandatory gate exists on the more obvious of two entry
  points, not both" shape as this session's `git commit`/`git merge` background-bypass fix (§2.9) and the
  workspace-symlink-escape fix before that (§2.9 also) — a real recurring lesson this pass, not three
  unrelated bugs. **Fixed:** extracted the write-side check into a shared `team_memory_gate(root, path,
  content, action)` helper (parameterized by `"write"`/`"patch"` for the error message) and called it from
  `execute_write` (unchanged position/behavior) and from *both* of `execute_patch`'s tiers, right before
  each tier's own `fs::write`, checking the tier's own computed `updated` content rather than the
  original file content. New test `patching_a_likely_secret_into_team_memory_is_blocked_not_advisory`:
  reproduced the bypass against the pre-fix code first (confirmed the exact advisory-only success and the
  secret landing on disk), then confirmed the fix blocks the patch identically to a direct write. Full
  `-p rapid` suite (326 lib tests) and `cargo build --workspace --tests` pass.
- **Correction (2026-08-30): the `/compact` no-op turned out to be one symptom of a much bigger, TUI-wide
  gap, not an isolated bug — checked by enumerating the whole `KernelAction` enum against
  `apply_kernel_action`'s match, not assumed from one variant.** `KernelAction` has 34 variants.
  `kernel_api()` (`crates/tui/src/commands.rs:827`) routes them into four buckets: `Interrupt` (3 variants:
  `CancelAgent`/`TerminateAgent`/`CancelJob`), `SubmitTurn` (`StartGoal`), `ForkSession`, `Rewind`
  (`RewindSession`) — 6 variants genuinely wired, real client calls in `apply_kernel_action`
  (`apps/rapid/src/interactive.rs`) — and `Approve` (16 variants: `ApplyChangeSet`, `Rollback`,
  `RunPlaybook`, `ApproveKnowledge`, `RejectKnowledge`, `EditKnowledge`, `AddMcp`, `RemoveMcp`, `AuthMcp`,
  `InstallPlugin`, `RemovePlugin`, `SetPluginPermissions`, `Handoff`, `Takeover`, `ComputerObserve`,
  `ComputerRecord`, `ComputerTest`) plus the `Dispatch` catch-all (the remaining 14: `PauseGoal`,
  `ResumeGoal`, `CancelGoal`, `ShowGoalBudget`, `PauseAgent`, `ResumeAgent`, `SleepAgent`, `ReindexContext`,
  `SuggestKnowledge`, `ValidatePlaybook`, `SelectModel`, `ResumeSession`, `CompactSession`,
  `ControlReturn`) — both of which hit the exact same `KernelApi::Approve | KernelApi::Dispatch => {}` arm.
  **30 of 34 `KernelAction` variants (essentially every TUI slash-command that isn't goal-start/fork/rewind/
  agent-or-job-interrupt) are silent no-ops today**, not just `/compact`. `requires_approval()`
  (`commands.rs:803`) even has a doc comment naming the intended design — "marks for
  `kernel::KernelClient::approve`" — confirming `KernelClient::approve` was meant to be called from the
  `Approve` arm and never was (grepped the whole workspace: `requires_approval`/`KernelApi::Approve` have
  zero call sites outside `commands.rs`'s own definition and tests). This reframes the TUI's kernel-action
  layer itself as the real gap, not any one command: it reads as an early scaffold with the command
  parsing, `KernelAction` typing, and approval-classification metadata all built, but the actual dispatch
  to `KernelClient` largely unwired behind it — the mirror image of `apps/rapid`'s headless `exec` path,
  which this whole document has repeatedly found to be the mature, heavily-tested side of this codebase.
  **Deliberately not attempted:** implementing any of the 30 — each of `Approve`'s 16 needs its own real
  subsystem behavior (MCP add/remove, plugin install, handoff, takeover, computer control, rollback,
  changeset apply, playbook run, knowledge review — genuinely separate features, not one fix), and even a
  narrower "stop pretending it worked" fix (surface a visible not-yet-implemented notice instead of silent
  `{}`) needs a real TUI user-notification mechanism this pass didn't locate with confidence, in a crate
  (`crates/tui`) this session has no prior tested familiarity with, unlike the `apps/rapid` core this
  session's other fixes are grounded in. Documenting the true scope precisely, rather than take a guess at
  a partial fix in unfamiliar territory, is the honest output of this pass.

**Fresh review pass, 2026-08-31, `crates/kernel/src/ipc/server.rs::handle_connection` — an authenticated
daemon only ever checked the client's grant once, at connect time, never again for the life of the
connection.** `auth::local_daemon::SessionApiKind`'s own doc comment: "Session APIs that must not run before
a live `ClientGrant`." `DaemonAuth::authorize_session_api`'s own doc comment: "Reject missing/wrong grants
before any session API, including enumeration." But `handle_connection` ran the challenge/response handshake
exactly once, then discarded the resulting grant (`let _ = &grant;`) and never referenced it again — every
subsequent request on that connection went straight to `dispatch_method` with no re-check at all. Confirmed
`authorize_session_api` and `enumerate_sessions` have zero callers anywhere in the workspace outside
`local_daemon.rs`'s own tests before this fix — the function documented to gate "any session API" gated
none of them in practice. **Concrete consequence:** `DaemonAuth::issue()` (token rotation — a normal,
documented, callable-while-serving operation per its own `issue_rotates_token_and_invalidates_prior_
challenges` test) is meant to invalidate prior credentials, but an already-open, already-authenticated
connection kept full session-API access indefinitely regardless of rotation, since nothing on the request
path ever re-consulted the daemon's current token state. Verified via the standard temporary-revert cycle:
the new test failed at "must be rejected after rotation" against the reverted code (grant checked once,
never again), confirming the test genuinely catches the gap, before the fix was restored. **Fixed:**
`authorize_session_api` already re-validates a passed-in grant against the daemon's *current* token
(`grant_mac` comparison inside `self.lock()`), so the fix is exactly "call it again per request" — no new
crypto or state needed. Added `session_api_kind(method) -> Option<SessionApiKind>` (a clean 1:1 mapping for
8 of the 9 wire methods `dispatch_method` actually handles; `Enumerate` has no corresponding wire method in
this dispatch table at all) and threaded `auth`/`grant` through `handle_request` so every dispatched call
re-authorizes against the live token before running, failing closed with the same `auth.required` response
a rejected handshake already uses. New test `rotating_the_token_revokes_an_already_open_connections_session_
apis`: authenticates a real connection via the full challenge/response protocol, confirms a call succeeds,
rotates the token, confirms the *same* connection's next call is now rejected. **Latent, not yet actively
firing:** confirmed via grep that `IpcServer::with_auth` and `kernel::ipc::client::DaemonClient` both have
zero callers anywhere in `apps/` outside their own crate's tests — authenticated IPC isn't wired into any
real daemon startup path today. **A second, related, deliberately-not-attempted gap surfaced investigating
this:** `DaemonClient` (`crates/kernel/src/ipc/client.rs`, the only production IPC client in the workspace)
implements no side of the auth challenge/response protocol at all — confirmed via `grep -i auth` returning
zero matches in the whole file. Pairing an authenticated `IpcServer` with the real `DaemonClient` today fails
every call (the client sends a request frame where the server expects a proof frame; the server's `auth.
challenge` frame doesn't match any reply shape the client's decoder accepts) — a functional interop gap, not
a security hole (fails closed, not open), but it means the fix above is not yet reachable through the only
shipped client either. **Not attempted:** wiring real handshake support into `DaemonClient` needs a design
decision this pass didn't make — the client's constructor has no notion of "the local daemon's identity/
runtime dir" to construct an `auth::LocalDaemonClient` for proving, and disambiguating "does this server
require auth at all" from the client side needs either a caller-supplied flag or a protocol-level signal,
neither of which exists today. Full `kernel` crate suite (167 tests, up from 166) and `cargo build
--workspace --tests` pass.
- **Confirmed 2026-08-31, directly, why the "narrower" fix isn't actually narrow — the naive version of it
  would be a regression, not an improvement.** Traced the caller chain from `apply_kernel_action`
  (`interactive.rs:2358`) up: `dispatch_slash` propagates its error via `?` (`interactive.rs:2349`),
  `handle_input` propagates via `?` in turn, and `SessionLoop::run`'s own main loop (`interactive.rs:2253`)
  calls `self.handle_input(input)?` — a bare `?` with no catch. So simply changing the silent `{}` arm to
  `return Err(InteractiveError::NotImplemented)` (the obvious "quick" version of "surface a notice") would
  propagate all the way up and **terminate the entire interactive TUI session** on the very first
  unimplemented slash command a user types — replacing a silent no-op with a hard crash, strictly worse for
  the user. Also checked whether the already-established `apps/rapid::exec_diag::stderr_line` pattern (used
  for headless-exec diagnostics elsewhere in this codebase) could be reused here instead: no — that pattern
  assumes plain stdout/stderr, but interactive mode owns the whole terminal via `crates/tui`'s rendering, so
  a raw stderr write would interleave with and corrupt the live TUI frame instead of showing a clean message.
  Both of these confirm, rather than overturn, the judgment already recorded above: a real fix needs an
  actual non-fatal, TUI-native notification path (a new `LocalUiEvent` variant reduced into `AppState` and
  rendered somewhere in the frame, with its own display/dismissal lifecycle) — genuine UI/UX design work in
  an unfamiliar crate, not a one-line "return the right thing" fix. Leaving unattempted, now with concrete
  evidence for why, instead of a guess.
| 14 | ~~No rendered (`html`/`md`) export format~~ **Implemented 2026-08-29, completed 2026-08-30.** `rapid inspect-export <session> <file> --format md` renders a chronological Markdown list (`- **kind** (seq N, timestamp) — \`payload\``) alongside the existing (now-default, unchanged-behavior) `--format jsonl`. Deliberately generic, not per-event-kind prose: `EventKind` has dozens of variants across session/turn/model/tool/... families, and rendering each one's payload into readable sentences is real, separate work this doesn't attempt — the value here is a readable, chronological skim of a transcript without guessing at semantics this function doesn't actually know. **`--format html` added 2026-08-30:** same generic chronological-list rendering as `md`, as a minimal static page (`<!doctype html>` + a `<ul>` of `<li>` entries); every field (event kind, timestamp, JSON payload) goes through a new `html_escape()` — ledger payloads are untrusted-origin text (tool output, model text) and this file may be opened in a real browser, so escaping isn't optional even though it's a local file, not a network-facing surface. Caught and fixed one real regression while adding this: an existing test, `inspect_export_rejects_an_unknown_format`, used `"html"` as its example of a format that *should* be rejected — updated to `"xml"` instead, since `html` is now valid. | Not established | `/export {html,md,json,jsonl}` | `apps/rapid/src/p9_commands.rs::run_inspect_export` | ~~P2~~ done | S |

### 1.5 Headless / scripting contract

| # | Gap | Grok Build | Qwen Code | Where it lands | Sev | Effort |
|---|---|---|---|---|---|---|
| 15 | ~~Exit codes are undifferentiated (fail-closed but opaque: `agent turn failed: failed`)~~ **Correction + fix, 2026-08-30.** The premise was half-wrong: `apps/rapid/src/headless/jsonl.rs` already has a real typed taxonomy, `JsonlExitCode` (`Success=0, Usage=2, Policy=3, Provider=4, Runtime=5, GoalIncomplete=6, Sandbox=7, ResourceExhausted=8, Interrupted=130`), derived from `ErrorCode`/`RapidErrorClass`, and — despite its doc comment saying "for `rapid run --jsonl`" — it was already reused by `InteractiveOutcome::exit_code()` for the plain (non-JSONL) `rapid exec`/TUI path too, not JSONL-gated as the doc comment implied. **What was actually still undifferentiated:** 17 separate call sites across `interactive.rs` (`goal` subcommand family, model-configuration errors, a permission-lattice setup failure) and 2 in `p9_commands.rs` (`cron remove`, `agents` inventory) returned a bare, untyped `Ok(1)` instead of routing through `JsonlExitCode` — including the two richest cases: a `Cancelled` (Ctrl-C) turn and a tool-failure stop both exited `1`, identical to every other failure, losing exactly the SIGINT-vs-failure distinction Grok Build's own taxonomy calls out. Fixed by mapping every site to the existing taxonomy (no new variants): "no active goal"/"no evidence recorded"/bad model override → `Usage`; a rejected goal-completion claim or an unmet `complete` precondition → `GoalIncomplete` (a perfect existing fit, not a new bucket); ledger/goal-file IO failures → `Runtime`; a permission-lattice setup failure → `Policy`; a `Cancelled` terminal status → `Interrupted`; a provider-classified `FailureCause` (`Auth`/`Connection`/`Rejected`/`Transient`) → `Provider`; everything else (tool-failure stops, `FailureCause::Unspecified`) → `Runtime`. New `exec_turn_exit_code()` helper in `interactive.rs`; the `Err(AgentExecutionError)` arm now reuses its existing `error_code()` method rather than a second hand-rolled mapping. Three integration tests (`configured_model_integration.rs`, `exec_diagnosability.rs`) were asserting the old flat `1` and were updated to the new, more precise codes — a genuine behavior change (a CI script keyed on exit code 1 for these cases needs updating), not a test-only fix. **Not attempted:** a finer-grained taxonomy matching Grok Build's exact numbering (separate `auth`/`config`/`turn-limit`/`budget` codes) — RapidLM's existing buckets are coarser by design (`Provider` covers auth+connection+rejection+transient; `Runtime` covers tool-exec+unspecified) and widening them is a bigger, separate call, not bundled into this fix. | Not profiled in depth | Structured taxonomy: 41 auth · 42 input · 44 sandbox · 52 config · 53 turn-limit · 54 tool-exec · 55 budget · 130 SIGINT | `apps/rapid/src/headless/jsonl.rs` (taxonomy, pre-existing) + `apps/rapid/src/interactive.rs`, `apps/rapid/src/p9_commands.rs` (wiring, done) | ~~P0~~ done | S |
| 16 | ~~No constrained structured-output mode~~ **Implemented 2026-08-30.** `rapid exec --json-schema <path>` reads and compiles the schema up front (a bad path or malformed schema fails typed as `JsonlExitCode::Usage`, before any model call), then wraps the turn's `ToolDriver` with a new `apps/rapid/src/structured_output.rs::StructuredOutputTools<T>` decorator — adds one synthetic tool (`emit_structured_result`, `parameters` = the caller's schema verbatim) to the advertised surface and delegates every other tool call to the wrapped driver unchanged (including its own `execute_batch` override, so the inner driver's real concurrency for its own calls isn't lost). A call to the synthetic tool is validated against the compiled schema (via the `jsonschema` crate, new dependency, `default-features = false` — the default features pull in `reqwest` for remote `$ref` resolution, which this never needs since the schema is always a fully-inline caller-supplied document, not fetched); a mismatch is a `ToolStepResult::Failed` the model can see and correct, never a silent pass. On a successful call, the validated JSON is captured and printed to stdout in place of the model's own text summary. If the turn finishes without ever producing a valid call, that's a typed `JsonlExitCode::Runtime` failure, not a silent 0 — the constrained-output contract wasn't met. **Not attempted:** the `tool-gateway` crate mentioned in the original "where it lands" — this is `rapid exec`-only (one-shot headless), not wired into the interactive TUI or MCP-server-exposed tool surface. | Not established | `--json-schema` registers a synthetic tool, Ajv-validated against a caller-supplied schema | `apps/rapid/src/structured_output.rs`, `interactive.rs` (`exec_turn`) | ~~P1~~ done | M |
| 17 | ~~No cost accounting anywhere~~ **Implemented 2026-08-29.** Correction chain, then a full fix: "no cost accounting" was wrong from the start — `llm-router` already computed real per-request cost (`UsageCost::Reported`, from real `openai_compatible`/`anthropic` provider responses) and a full `ModelCatalog` with pricing. The value was silently dropped at `apps/rapid/src/model.rs::fold_stream`, which read `NormalizedUsage` but only extracted token count before constructing `ModelStepOutput` (`tokens: u64` only, no cost field). Initially deferred as too large — widening `ModelStepOutput` touches a shared `agent-runtime` type across (checked precisely) 40+ construction/match expressions, and a side-channel workaround doesn't work because `SelectedModel`/`FallbackChainModel` dispatch polymorphically. **Done anyway**, once the risk was reassessed: adding a struct field makes the compiler enumerate every missed site as a compile error, not a silent bug — `cargo build --workspace --tests` was used as the authoritative fix-list rather than tracking sites by hand. `ModelStepOutput::{Terminal,ToolCalls}` now carry `cost_usd_micros: Option<u64>`; `fold_stream` reads the real value via a new `usage_cost_micros()` (no fabricated estimate the way tokens has one — a guessed dollar figure is a lie, not an estimate); a new `CostAccumulator` (`host.rs`, mirrors the existing token counter) sums it across steps, tracking whether *any* step ever reported one so "unknown" never reads back as "confirmed zero"; `ExecOutcome` and `SubagentReport` both carry the total; the `--verbose` diagnostic line and the `tokens used:` stderr line both surface it (`format_usd_micros`, 6 decimals). Still not done: `RouterDecisionRecord` (§2.8 below). **Correction (2026-08-30):** `session_finished` now does have a real call site (`rapid exec --jsonl`, §2.8's own 2026-08-30 note below) — but cost still isn't surfaced there: neither `JsonlRecord::session_finished`'s `data` nor the new `assistant_message` carry `cost_usd_micros`, only `stderr`'s `tokens used:` line does. Widening `session_finished`'s `data` to include it (a golden-JSON-breaking change, deliberately not bundled into the `--jsonl` wiring pass) is real, separate follow-up. **Follow-up implemented 2026-08-30:** `JsonlRecord::session_finished` now takes a `cost_usd_micros: Option<u64>` parameter and always emits the key (`null` when no step ever reported a real cost, never a fabricated `0` — same discipline `CostAccumulator` itself already uses). `interactive.rs`'s one call site widened its `(text, code)` match result to a `(text, code, cost_usd_micros)` triple, reading `outcome.cost_usd_micros` from the same `ExecOutcome` the `tokens used:` stderr line already reads (`Err` arms — no `ExecOutcome` to read from — pass `None`). Confirmed as the anticipated golden-JSON-breaking change: `GOLDEN_SESSION_FINISHED` updated (`cost_usd_micros` sorts before `exit_code` alphabetically, matching the `router_decision` golden test's own already-documented key-order lesson); the two integration-test assertions in `exec_diagnosability.rs` index into `data.exit_code` directly rather than comparing the whole object, so they needed no changes. `assistant_message` still doesn't carry cost (it never claimed to — cost belongs to the turn, not the text) and remains untouched. | Usage/cost fields present (`xai-grok-pager/src/headless/cli.rs`) | Token usage only, no pricing table, no `cost_usd` field | `apps/rapid/src/model.rs`, `host.rs` (`CostAccumulator`, `ExecOutcome`), `exec_tools.rs` (`SubagentReport`), `interactive.rs`; `crates/agent-runtime/src/turn.rs` (`ModelStepOutput`) | ~~P1~~ done | M |

### 1.6 Distribution (product/packaging, not architecture — tracked here for completeness, not gated on it)

| # | Gap | Grok Build | Qwen Code | Sev | Effort |
|---|---|---|---|---|---|
| 18 | No public install path | curl installer + self-updater + DotSlash | npm, Homebrew, Docker, VS Code/Zed extensions, Tauri desktop | P1 | L (packaging/release-eng work, separate track from this roadmap) |

---

## Phase 2 — Leapfrog: Modbit-informed hardening (clean-room reimplementation only)

Every item below cites its Modbit feature ID for traceability. **None of this is Modbit's code or a
Modbit dependency** — RapidLM already has the adjacent primitive in most cases; the task is to harden it
to the rigor Modbit's own architecture decided on, using RapidLM's own crates and data models.

### 2.1 Change Engine hardening + `MergeTransaction`

Modbit: `CHG-001` (Change Engine + Write Barrier, tagged MOAT — single typed, base-revision-bound, atomic,
provenance-recorded change path; no alternate raw filesystem mutation for agents), `CHG-003` (deterministic
edit-match ladder: exact → whitespace-insensitive with offset remap → context suggestion → ambiguity
error — never guess), `CHG-012` (`MergeTransaction`: source/target/base revisions, pre-merge state,
conflicts, resolutions, validation evidence, committed/rolled-back state — the merge itself is reversible
and auditable), `CHG-008`–`CHG-010` (typed `UndoAction`/`UndoPlan`, optimistic-concurrency revert checking
expected hash before reversal, `ReversibilityClass` for irreversible/compensatable actions).

- **Where it lands:** `workspace` crate. `workspace.patch`'s match ladder should be audited against
  `CHG-003`'s ordering (this is also what `gaps.md` remediation item 4 already asked for, citing Grok
  Build's `search_replace`). `CheckpointManager` (already exists, `workspace::checkpoint`) is the natural
  home for typed undo actions.
- **Why this comes first:** every subsequent Phase 2/3 item that touches concurrent or competitive
  execution (`MergeTransaction` above, Arena-style execution in Phase 3) is unsafe without this. Build the
  primitive before anything that calls it.
- **Sev/Effort:** P0 / L.
- **Correction (2026-08-30): the "audit workspace.patch's match ladder" pointer targeted the wrong
  crate, and the match-ladder half is now fixed independently of the L-effort transaction migration.**
  `crates/workspace/src/transaction.rs` already implements almost exactly `MergeTransaction`
  (`WorkspaceTransaction`/`TransactionManager`, base-revision staleness checks, `rollback`/
  `rollback_on_failure`, pluggable `VerificationHook`s) and is wired into `agent-runtime`'s
  `MergeHandoff`/`ResultStore`, `event-ledger`, `kernel::session::fork`, and `acp::v1` — but **none of it
  reaches `apps/rapid`** (zero `use workspace::` in `exec_tools.rs`). The actual match ladder Grok Build's
  `search_replace` compares against lives entirely in `apps/rapid/src/exec_tools.rs::execute_patch`
  (`crates/workspace/src/patch/model.rs`'s `SemanticPatch`/`PatchOp` is byte-range-based, not text-search,
  so it has no ladder to audit — a different mechanism from what `WORKSPACE_PATCH_TOOL` actually uses).
  **Implemented the two missing tiers in `execute_patch` itself:** tier 2, whitespace-insensitive fallback
  (`find_whitespace_insensitive`/`lines_match_loosely`) — when the exact substring match finds zero
  occurrences, retries line-by-line with each line's whitespace-split tokens compared instead of its raw
  text, tolerant of reindentation/reflowed spacing but never of an actual content difference; applies the
  same unique-vs-ambiguous-without-`replace_all` policy as the exact tier, and splices `new` in verbatim
  at the located byte range (deliberately never reindents the replacement to match the matched region's
  real indentation — that is a second, harder, separately-risky problem this doesn't attempt). Tier 3,
  context-suggestion (`suggest_closest_line`, advisory only) — when even the loose tier finds nothing,
  names the single existing line most similar to `old`'s first line by shared-token overlap, so the model
  has something concrete to correct on retry instead of a bare "not found". **Still not done:** migrating
  `apps/rapid`'s actual writes onto the already-built, already-wired-elsewhere `WorkspaceTransaction` (the
  genuinely L-effort, atomicity/provenance/multi-file half of this item) and the `UndoAction`/`UndoPlan`/
  `ReversibilityClass` types (`CheckpointManager`/`RewindOp`/`RewindPreview` in `workspace::checkpoint`
  already give optimistic-concurrency-checked, previewable, reversible undo, unnamed as such — a
  wiring/naming task, not a build-from-scratch one, but not attempted in this pass).

### 2.2 `AgentExecutionCapsule` + `AgentResultEnvelope` + write-scoped narrow leases

Modbit: `AGT-005` (`AgentExecutionCapsule` — per-agent tools/model policy/permissions/token budget/private
context; share only authorized workspace/evidence/artifacts, never a hidden shared transcript), `AGT-011`
(`AgentResultEnvelope` — typed child result: summary, artifacts, proposed patches, tests, evidence IDs,
assumptions, conflicts, decisions, touched files/symbols, next action — "do not merge children through
prose alone"), `CAP-008` (detached-agent permission ceiling: background agents may consume existing grants
but cannot interrupt for new ones — escalate via the foreground parent).

- **Where it lands:** directly extends the gap Phase 1 §1.2 row 4 named. `capability_broker::lease`
  already has `LeaseIssuer`/`CapabilityLease` with single-use, validated consumption
  (`validator::{LeaseUseGuard, ConsumedLeaseUse}`) — extend it with a **narrow, write-scoped** lease
  variant instead of `PersistentSpecialist`'s current binary read-only/reject-everything-else gate.
  `task_spawn`'s return path should carry a typed envelope, not a text blob — this closes Phase 1 §1.2
  row 5 at the same time.
- **Guardrail (Modbit `AGT-010`, bounded recursive delegation):** nested delegation off by default; an
  explicit max-depth profile only, never unbounded. Also see `REJ-007` below.
- **Sev/Effort:** P0 / M.
- **`AGT-010` audited and fixed 2026-08-30 — real, exploitable unbounded nesting found, not hypothetical.**
  Checked whether this guardrail already held given how much of §2.2 had already landed. It only half did:
  `WorkspaceTools::open_read_only` (the `explore`/`plan` agent-type path) already excludes `task_spawn`
  from its surface as a side effect of filtering to `ToolKind::Read` tools — confirmed by an existing test,
  `depth 1 enforced`. But `task_spawn`'s *other* branch — any non-`explore`/`plan` agent type, which gets
  a full write-capable `WorkspaceTools` via `open_with_permissions` — kept `task_spawn` in its own surface,
  and `open_with_permissions` always builds a *fresh* `Arc::new(AtomicU64::new(0))` for
  `subagent_spawns` (the `MAX_SUBAGENT_SPAWNS_PER_TURN` counter from §2.10's own correction) rather than
  inheriting the parent's. Combined, this meant: a write-capable subagent could itself call `task_spawn`,
  its child could too, and so on — unbounded in *depth*, with each level getting its own fresh budget of
  32 (`32^depth` total possible spawns, not 32), the exact shape `AGT-010` explicitly names ("never
  unbounded"). **Fixed:** new `WorkspaceTools::disable_nested_spawn()` (mirrors `read_only`'s own
  surface-plus-execution double guard exactly — unadvertised in `tool_surface()`, refused in
  `execute_call_traced()` if called anyway) called on every subagent child's own tools in
  `LiveSubagentRunner::run` (`interactive.rs`), for both the read-only and write-capable branches alike —
  the read-only branch already got this for free from its `ToolKind::Read` filter, but calling it there
  too costs nothing and removes the asymmetry. This closes only the "off by default" half of `AGT-010`;
  the "explicit max-depth profile" half (an opt-in, configurable way to allow depth > 1) does not exist
  and was not attempted — the default is now correctly bounded, but there is no profile mechanism to widen
  it deliberately if a future use case needs to. New test
  `write_capable_subagent_children_cannot_spawn_further_subagents`: confirms `task_spawn` is present
  before `disable_nested_spawn()` (proving the bug was real, not already-impossible), absent after, and
  that a call attempted anyway under `BypassPermissions` (chosen specifically so no *other* gate would
  have denied it first) is refused with an `AGT-010`-labeled reason. Full `-p rapid` suite (304 lib tests)
  and `cargo build --workspace --tests` pass.
- **A third instance of the same "child starts fresh instead of inheriting" shape, found while checking
  what else a subagent's tools default to that the parent's don't: project hooks.** `WorkspaceTools::
  open_with_permissions` defaults `hooks: HooksConfig::default()` (empty), and — same as the disk/network
  budgets — nothing in `LiveSubagentRunner::run` ever called `set_hooks` on the child. A configured
  `pre_tool_use` hook (a real policy-enforcement surface: a security scanner, an approval webhook, a
  linter gate) protects the parent's own tool calls but was silently bypassable by asking a subagent to
  make the same call instead — delegation as a policy-evasion vector, not merely a resource-accounting
  gap like the previous two instances. **Fixed:** new `WorkspaceTools::hooks_config()` (clones the
  configured `HooksConfig`, mirroring `turn_budget_handles()`'s shape) read from the parent right where
  `LiveSubagentRunner` is constructed, stored on it, and applied via the *already-existing* `set_hooks`
  on every child `LiveSubagentRunner::run` builds — no new setter needed, unlike the budget fix. New test
  `subagent_children_inherit_the_parents_policy_hooks`: a `pre_tool_use` hook that denies every call,
  confirmed to *not* gate an unshared child's own tools (the vulnerability was real) and confirmed to gate
  a child with the parent's hooks propagated (fixed). Full `-p rapid` suite (306 lib tests) and
  `cargo build --workspace --tests` pass. `session_start`/`session_end` hooks are deliberately not
  propagated — those fire once per `rapid exec` process, not per tool call, so a subagent (which runs
  inside the same process, not a new one) firing them again would be a duplicate, not a fix.
- **A fourth instance, found by checking the rest of `WorkspaceTools`'s per-instance config for the same
  shape: the shadow-diagnostics quality gate.** Same defect as hooks, one config field over:
  `shadow_diagnostics: None` by default, only ever set on the parent (`tools.set_shadow_diagnostics(shadow)`
  in `interactive.rs`), never propagated by `LiveSubagentRunner::run`. A subagent's `workspace_write` calls
  silently skipped the verify-in-an-isolated-worktree check the parent's own matching writes went through
  — lower severity than the hooks bypass (a quality gate, not a security control, and shadow diagnostics
  already fails open by design on its own misconfiguration), but the same "delegation quietly drops a
  policy the parent had" shape. **Fixed** with the same technique: new `WorkspaceTools::
  shadow_diagnostics_config()` (clone, mirrors `hooks_config()`), read from the parent alongside `hooks`
  right where `LiveSubagentRunner` is constructed, applied via the already-existing `set_shadow_diagnostics`
  in `LiveSubagentRunner::run`. New test `subagent_children_inherit_the_parents_shadow_diagnostics_gate`:
  a `grep -q MARKER {path}` gate that fails a markerless write, confirmed to *not* fire for an unshared
  child (the gap was real) and confirmed to fire once propagated (fixed). Full `-p rapid` suite (307 lib
  tests) and `cargo build --workspace --tests` pass. **Checked and deliberately left alone:**
  `fetch_allowlist` (private-host allowlist for `web_fetch`) has the identical "parent-only" shape, but
  fixing it would *widen* a subagent's capabilities to match the parent's, cutting against this
  codebase's established direction of narrowing subagent scope (write-scope confinement, permission-mode
  capping, nested-spawn denial) — an unshared, empty allowlist makes a child strictly *more* restricted
  than the parent, which is the safe direction to leave a gap in, not one that needs closing.
- **Fifth and final instance of this shape, completing an exhaustive field-by-field pass over
  `WorkspaceTools`'s every subagent-relevant field: `trace_calls`.** Headless `rapid exec` unconditionally
  enables per-tool-call stderr tracing on its own tools (`exec_turn`, `interactive.rs`) but every subagent
  child defaulted to `false` — a user watching a headless run's stderr saw every top-level tool call
  traced and every delegated subagent's tool calls completely silent, the same call just invisible once
  routed through `task_spawn`. Lower stakes than the previous four (a debug/observability convenience,
  not a security or quality control), but zero-cost to close with the same pattern: new
  `WorkspaceTools::trace_calls_enabled()` (mirrors the other three getters), read from the parent and
  applied via the already-existing `set_trace_calls` in `LiveSubagentRunner::run`. New unit test
  `trace_calls_enabled_reflects_set_trace_calls_for_subagent_propagation` covers the getter/setter
  roundtrip directly (verifying the actual stderr output of a live subagent turn would need a heavier
  integration harness this pass didn't build). Full `-p rapid` suite (308 lib tests) and
  `cargo build --workspace --tests` pass. **This closes the audit**: every field on `WorkspaceTools` that
  a subagent's tools carry (`permissions`, `read_only`, `jobs`, `trace_calls`, `subagents`,
  `fetch_allowlist`, `hooks`, `shadow_diagnostics`, `ask_stdin`, `mcp`/`mcp_surface`, `subagent_spawns`,
  `bytes_written`/`fetch_bytes`, `nested_spawn_allowed`) has now been individually checked for this shape,
  not just "some fields, spot-checked."
- **Correction + partial fix (2026-08-30):** `AgentResultEnvelope`'s exact field list already exists —
  `crates/agent-runtime/src/agent/model.rs`'s `AgentResult` carries `summary, evidence, workspace_view,
  patch_summary, artifacts, claims, open_questions, blockers, context_lineage` (all with accessors) —
  but `apps/rapid/src/interactive.rs`'s `LiveSubagentRunner::run` only ever read `.summary()` and
  `.status()` before constructing `SubagentReport`, silently discarding claims/blockers/open_questions/
  patch_summary before they ever reached the parent model's tool-result text. **Fixed the discard, not
  the type shape:** `SubagentReport` (`apps/rapid/src/exec_tools.rs`) gained `claims: Vec<String>`,
  `blockers: Vec<String>`, `open_questions: Vec<String>`, `patch_summary: Option<String>` — pre-rendered
  as text lines (`"{criterion}: {text} ({result})"`, `"[{kind}] {summary}"`, etc.) at construction time,
  matching this codebase's existing convention of flattening typed enums to `String` at the tool-result
  boundary (e.g. `stop_reason`) rather than smuggling a second JSON-typed channel through a text-only
  tool result. `execute_task_spawn` now appends non-empty ones to the returned summary (`\nclaim: ...`,
  `\nblocker: ...`, etc.), so a parent model actually sees what a subagent asserted, was blocked by, or
  left open instead of only its prose summary. **Deliberately not done:** `artifacts()`
  (`Vec<ArtifactRef>`, content-addressed blob refs with a `RedactionClass`) — surfacing these needs a
  real decision about redaction-aware rendering (a `Secret`-class artifact ref probably shouldn't even
  be named in plain text) that a straight `.to_string()` would get wrong by default, so left untouched
  rather than guessed at.
- **`artifacts()` closed 2026-08-30 — checked `protocol::ArtifactRef`'s actual shape and the redaction
  worry doesn't apply.** `ArtifactRef` is `{ id: ArtifactId, media_type: String, bytes: u64, redaction:
  RedactionClass }` — a content-addressed SHA-256 hash, a generic media type, a byte count, and the class
  itself; there is no name/path/locator field at all, so no field carries the artifact's actual content or
  anything content-derived beyond its hash. Naming a `Secret`-class ref this way discloses nothing the
  reference architecture wasn't already designed to disclose (the point of a content-addressed reference
  is that a party can hold and pass it along without ever seeing the payload). `SubagentReport` gained
  `artifacts: Vec<String>`, rendered uniformly across every `RedactionClass` as `"{id} ({media_type},
  {bytes}B, {redaction})"` — the class itself is shown, not hidden, so a parent model reading a subagent's
  result can see *that* a secret artifact exists and its size without ever seeing what it contains.
  `execute_task_spawn` appends `\nartifact: ...` lines the same way claims/blockers do. New assertion in
  `task_spawn_report_surfaces_claims_blockers_questions_and_patch_summary` covers a `secret`-class
  artifact by name, confirming the display choice explicitly rather than leaving it implicit. The **narrow
  write-scoped lease variant** and replacing `PersistentSpecialist`
  are both still entirely open — this pass only closed the "typed data exists but gets thrown away"
  half, not the permission-ceiling half.
- **Narrow write scope implemented 2026-08-30 — at the `PermissionLattice` layer, not as a
  `capability_broker::lease` variant.** Traced the intended lease-level design first: it would mean
  routing the *entire* subagent tool-dispatch path through lease validation, which doesn't happen at all
  today (subagents dispatch via `ExecTools::workspace_with_permissions`/`PermissionLattice`, never
  through a capability-broker lease check) — a much bigger integration than this item's own framing
  suggested, since it's not "add a lease variant" but "make lease validation the actual gate for
  subagent tool calls at all." Built the pragmatic, safe equivalent at the layer that *does* already gate
  every subagent tool call: `PermissionLattice` gained `write_scope: Option<String>` (a workspace-relative
  path prefix) and `with_write_scope()`, checked in `evaluate()` **before** every rule/grant/mode —
  including `bypassPermissions`, which otherwise allows everything unconditionally — so a scope ceiling
  can only be narrowed further, never widened, by anything downstream. Scoped deliberately to
  `ToolClass::FileEdit` only: `shell_exec`'s `subject` is joined argv, not a workspace path, so applying
  a path-prefix check to it would silently misfire. `task_spawn` gained an optional `write_scope`
  argument (validated through the same `checked_relative` fail-closed path check every other tool
  argument uses — an escaping `../` path is a handled, model-visible refusal, not a panic or a silent
  no-op), threaded through the widened `SubagentRunner::run` trait to `LiveSubagentRunner::run`, which
  applies it via `.with_write_scope()` on the child's lattice. **Not attempted:** the actual
  capability-broker lease integration this item originally specified, and replacing `PersistentSpecialist`
  — both remain real, separate, larger work; what's implemented here is a genuine safety improvement
  (a subagent confined this way structurally cannot write outside its scope) using the mechanism this
  codebase already has, not a renamed placeholder.

### 2.3 `CapabilitySnapshot` / `AuthorizationEpoch` + Policy Compiler

**Verified genuinely absent (2026-08-29):** a targeted grep for `CapabilitySnapshot`/`AuthorizationEpoch`/
a policy-compiler type across `capability-broker` found nothing — unlike most of this section, this one
really is missing, not just unwired. **Narrower nuance found while scoping this:** `exec_permission_lattice()`
(`apps/rapid/src/interactive.rs`) has exactly one call site, inside headless `exec_turn` — the lattice
loads once per process invocation and is fixed for the whole turn, which is already the core property
`CAP-005` asks for (an immutable per-round snapshot, no mid-turn retroactive policy change) for
`rapid exec`'s single-turn case specifically. Whether the interactive TUI's longer-lived, potentially
multi-turn session reloads settings between turns (and so needs an explicit snapshot/epoch type to get
the same guarantee) was not confirmed — its lattice-construction call site wasn't traced in this pass.
Worth checking before building a full `CapabilitySnapshot` type: the exec case may already need only a
name for a property it already has, while the TUI case is the part that might still be genuinely open.

Modbit: `CAP-005` (immutable per-model-round capability snapshot carried through the model event, tool
call, and run step — mid-round policy changes apply next round, never retroactively), `CAP-001` (Policy
Compiler: hard invariants → enterprise/admin → execution profile → user → project → agent, merged into one
monotonic authority where lower-trust layers may only restrict, never widen), `CAP-009` (an optional fast
classifier for ambiguous approvals sits **beneath** deterministic allow/ask/deny rules and can never
override them — contrast with Qwen Code's `auto` mode, which makes its LLM classifier the factory
default; adopt the classifier-as-safety-net idea, not the classifier-as-default posture).

- **Where it lands:** `capability-broker`. RapidLM's 6-mode lattice already has the right shape; this adds
  the missing piece — a frozen, auditable snapshot per turn, and an explicit merge order so a project's
  `.rapidlm/settings.json` can never widen what a user or admin policy already restricted.
- **Sev/Effort:** P1 / M.
- **CAP-005 half resolved by verification, 2026-08-30 — and it surfaced a much bigger finding.** Traced
  the TUI's lattice-construction call site this pass names as unconfirmed: `run_interactive` →
  `run_started_session` builds one `ServiceGraph` service, `KernelRuntime`, and its `SessionLoop` only
  ever calls `CreateSession`/`SubmitTurn`/`Interrupt`/`ForkSession`/`RewindSession`/`SubscribeEvents`
  against `InProcessKernelClient`. `submit_turn_sync` (`crates/kernel/src/client.rs:420`) does nothing
  but validate a turn-lease, append a `TurnStarted` ledger event, and store the lease — no LLM call, no
  tool call, no `ExecTools`/`AgentExecutor`/`PermissionLattice` anywhere in it, and `crates/kernel` has
  zero references to any of those types at all (confirmed by grep), doesn't even depend on `agent-runtime`
  in its `Cargo.toml`. **The interactive TUI has no live agent-turn/tool-dispatch loop yet at all** — typing
  a prompt into the live chat records `TurnStarted` and returns; nothing in-process invokes a model or
  runs a tool for that turn. This is the same shape as the `/compact` no-op found in §1.4/#13's correction
  (dispatched by the TUI, landing on a silent no-op because the underlying mechanism doesn't exist yet) —
  a real, load-bearing architecture gap, bigger than and separate from CAP-005 itself. Given this, CAP-005
  genuinely doesn't apply to the TUI yet: there's no turn execution there for a snapshot to protect.
  **For the one path that does exist (`rapid exec`), checked whether a `CapabilitySnapshot` wrapper would
  add anything real:** `PermissionLattice` (`apps/rapid/src/permissions.rs`) has zero `&mut self` methods
  — no mutation API exists to guard against in the first place, so the "immutable per-round snapshot"
  property already holds unconditionally, not just by convention. A wrapper type here would rename an
  already-total invariant, not enforce a new one — declining to build it, per this document's own standing
  rule against ceremony with no load-bearing behavior behind it.
- **Policy Compiler (`CAP-001`) half: a real instance of the exact pattern already existed one file
  over, extended to the permission-mode domain, 2026-08-30.** `apps/rapid/src/managed_config.rs`
  (`RAPIDLM_MANAGED_CONFIG`) already implements CAP-001's "hard invariants merge, lower-trust layers may
  only restrict, never widen" shape — just scoped to model configuration (`locked_default`,
  `allowed_providers`, `min_reasoning_effort`, each enforced managed > env > user > default). But
  `exec_permission_lattice()` (the actual tool-approval mode/rule resolution) had zero connection to it:
  a project's `.rapidlm/settings.json` or `RAPIDLM_PERMISSION_MODE` could set `bypassPermissions` freely,
  with no admin ceiling at all. **Implemented:** a new `max_permission_mode` field on `ManagedPolicy` plus
  `gate_permission_mode()`, wired into `exec_permission_lattice()` right after mode resolution — narrows
  the resolved mode down to the managed ceiling when it's exceeded (reported via a new
  `GateReportEntry`, `eprintln!`'d as a warning), passes through untouched otherwise, mirroring
  `min_reasoning_effort`'s existing silent-enforcement shape rather than hard-refusing the whole run
  (becoming *more* restrictive is always safe; a model misconfiguration hard-refusal is not the same
  risk). Needed a real permissiveness ranking to compare modes at all — added
  `PermissionMode::permissiveness_rank()`; confirmed from `evaluate()`'s own logic (not guessed) that
  `Plan` is the strictest of all six (denies every write-classified call outright, `PlanModeDeny`), not a
  position in the enum's declaration order (which only matches `MODE_NAMES`'s lookup table). A configured-
  but-unreadable `RAPIDLM_MANAGED_CONFIG` now fails `exec_permission_lattice` closed too, matching
  `load_policy`'s own already-documented invariant that it must never silently become "no policy" — this
  path previously didn't call `load_policy` at all, so the invariant had nothing to apply to. **Not
  attempted, and this is the bulk of `CAP-001` and all of `AuthorizationEpoch`:** the full hard-invariants
  → admin → execution-profile → user → project → agent merge *order* across multiple settings sources
  (today's merge is a flat union of `.rapidlm/settings.json` + `.claude/settings.json` rules, with no
  concept of which layer "wins" a conflict beyond the two special-cased fields above) — real, separate
  design work.
- **A second `CAP-001` dimension implemented 2026-08-30: `denied_tools` — an admin-level tool ban nothing
  downstream can widen past.** `PermissionLattice` gained `denied_tools: Vec<ToolPattern>` (reusing the
  existing `ToolPattern` type project/user deny rules already use — `Name` or `Name(arg-glob)`, same
  parser) checked in `evaluate()` as step *-1*, before even the `write_scope` ceiling and every rule/grant/
  mode including `bypassPermissions` — mirroring `write_scope`'s own precedent exactly (a lattice-level
  ceiling checked first, not a rule appended to a list where insertion order would matter). Unlike
  `max_permission_mode`, this needed no "gate" comparison function: a pure ban has no lower-trust value to
  widen against, so `managed_config.rs`'s new `denied_tools: Option<Vec<ToolPattern>>` field (parsed from
  a `policy.denied_tools = ["tool", "tool(arg-glob)"]` TOML array, validated against `ToolPattern::parse`
  at load time) is applied unconditionally in `exec_permission_lattice` once loaded — no merge order to
  get wrong, since there's no competing "allow_tools" layer to reconcile against. `for_subagent()` carries
  `denied_tools` over unchanged, same as `write_scope`, so a banned tool stays banned for delegated work
  too. New tests: `admin_denied_tools_win_over_bypass_permissions_and_allow_rules` (an explicit allow rule
  plus `bypassPermissions` both present, the ban still wins), `admin_denied_tools_survive_for_subagent_
  narrowing`, `admin_denied_tools_respect_their_own_arg_glob` (a pattern with an arg glob bans only
  matching arguments, same semantics as an ordinary deny rule's glob) in `permissions.rs`, plus
  `managed_config.rs` parse/validation tests (rejects an empty array, rejects an invalid pattern string,
  reads a valid one back). Full `-p rapid` suite (313 lib tests) and `cargo build --workspace --tests`
  pass. Still not attempted: the full multi-layer merge order noted above — this is one more absolute
  ceiling added to the same "managed policy, applied once, never re-checked against a competing layer"
  shape `max_permission_mode` already established, not the general merge-order primitive `CAP-001`/
  `AuthorizationEpoch` actually ask for.
- **A third `CAP-001` dimension, `confine_writes_to` (an admin-level, deployment-wide write-scope
  ceiling), implemented 2026-08-30 — after finding and deliberately avoiding a real bypass the naive
  version would have had.** The obvious implementation — reuse `PermissionLattice::with_write_scope`,
  the exact mechanism `task_spawn`'s own narrower scope already uses — turns out to be actively unsafe
  for this purpose: `with_write_scope` *overwrites* rather than intersects (correct for its actual use,
  where each `task_spawn` call sets its own child's scope from scratch), so a subagent's own `write_scope`
  argument would silently replace an admin's confinement instead of narrowing within it — a real
  bypass a managed deployment would have no way to detect. **Fixed by not reusing the field**: new,
  fully independent `PermissionLattice::admin_write_scope: Option<String>` /
  `with_admin_write_scope()` / `DecisionReason::AdminWriteScopeViolation`, checked in `evaluate()`
  *before* the per-`task_spawn` `write_scope` (both must hold when both are set — an admin ceiling and a
  subagent's own narrower scope compose as an intersection, never an override). `managed_config.rs` gained
  `confine_writes_to: Option<String>`, validated through `protocol::RepoPath::parse` (the same relative/
  no-traversal check every other workspace-relative path in this codebase already goes through, not a
  second, possibly-divergent one) and applied unconditionally in `exec_permission_lattice`, same
  no-merge-order-needed shape as `denied_tools`. New test
  `admin_write_scope_wins_over_bypass_permissions_and_write_scope_never_overwrites_it` constructs exactly
  the bypass scenario above (a lattice with `admin_write_scope("src")`, then `.with_write_scope("docs")`
  layered on top as `task_spawn` would) and confirms the admin ceiling still holds *and* the subagent's
  own narrower scope still independently applies too — both constraints active at once, neither silently
  dropped. Full `-p rapid` suite (316 lib tests) and `cargo build --workspace --tests` pass.
- **A fourth `CAP-001`/`WRK-017` dimension, 2026-08-30: `max_write_bytes_per_turn`/`max_fetch_bytes_per_turn`
  — admin-lowerable disk/network per-turn ceilings.** `WorkspaceTools`'s `MAX_TOTAL_WRITE_BYTES_PER_TURN`/
  `MAX_TOTAL_FETCH_BYTES_PER_TURN` (§2.10) were fixed constants; a managed deployment wanting a stricter
  budget than the 64 MB/16 MB built-in defaults had no way to configure one. New `max_write_bytes`/
  `max_fetch_bytes: u64` fields (defaulting to the existing constants) replace the constants in
  `reserve_write_budget`/`reserve_fetch_budget`'s comparison; new `narrow_write_ceiling`/
  `narrow_fetch_ceiling` setters take the *minimum* of the current value and the requested one, so a
  managed policy can only ever lower the ceiling, never raise it past the built-in default even if called
  with a larger number by mistake — verified directly (`narrow_write_ceiling_only_ever_lowers_never_raises`
  calls it once with a small value, once with the original constant, and confirms the second call is a
  no-op). Propagated to subagent children via the same `LiveSubagentRunner` mechanism as the other three
  dimensions (`turn_ceilings()`/`narrow_*_ceiling()` alongside `hooks_config()`/`shadow_diagnostics_config()`
  — a managed ceiling must bound a delegated subagent's writes too, not just the parent's). Applied in
  `exec_turn` by re-loading the managed policy rather than threading it out of `exec_permission_lattice`'s
  return type — that function already fails the whole turn closed on an unreadable policy, so a load
  failure at this second call site is treated as "skip narrowing" rather than a second independent
  failure point, since the ceiling is non-security-critical compared to the permission lattice itself.
  `managed_config.rs` gained a shared `parse_positive_integer` helper (rejects zero/negative — a "ceiling"
  of zero would fail every real turn, almost certainly a policy-authoring mistake, not an intended
  ultra-strict setting). Full `-p rapid` suite (318 lib tests) and `cargo build --workspace --tests` pass.
  `CAP-001`'s Policy Compiler now has four real, independently-tested admin dimensions (mode ceiling, tool
  ban, write confinement, resource ceilings); the full multi-layer merge-order primitive across arbitrary
  settings sources remains the one part of `CAP-001`/`AuthorizationEpoch` genuinely unbuilt.
- **A fifth `CAP-001`/`WRK-017` dimension, 2026-08-30: `max_subagent_spawns_per_turn` — an admin-lowerable
  ceiling on how many `task_spawn` calls one turn may make.** Same narrow-only shape as the byte ceilings:
  `WorkspaceTools` gained a `max_subagent_spawns: u64` field (defaulting to the existing
  `MAX_SUBAGENT_SPAWNS_PER_TURN` constant) that `execute_task_spawn`'s budget check now compares against
  instead of the constant directly, plus a `narrow_subagent_spawn_ceiling` setter that takes the minimum of
  the current value and the requested one — verified by
  `narrow_subagent_spawn_ceiling_only_ever_lowers_never_raises`, which narrows the ceiling to 2, confirms a
  call with the original (larger) constant is a no-op, then drives two real `task_spawn` calls through a
  `FakeRunner` and confirms a third is refused with the interpolated ceiling in the error message.
  Deliberately **not** propagated to subagent children the way the byte ceilings are: `disable_nested_spawn`
  already makes `task_spawn` unreachable from a child entirely, so a child's own copy of this field would be
  dead data, not a gap — no `LiveSubagentRunner` field was added for it. `managed_config.rs` gained
  `max_subagent_spawns_per_turn: Option<u64>`, parsed through the same `parse_positive_integer` helper as
  the byte ceilings (rejecting zero, since a ceiling of zero would refuse every `task_spawn` call), and
  applied in `exec_turn` alongside `max_write_bytes_per_turn`/`max_fetch_bytes_per_turn` in the same
  re-loaded-policy block. Full `-p rapid` suite (319 lib tests), `cargo test -p rapid --tests` (all
  integration binaries), and `cargo build --workspace --tests` pass. `CAP-001`'s Policy Compiler now has
  five real, independently-tested admin dimensions; the full multi-layer merge-order primitive across
  arbitrary settings sources remains the one part of `CAP-001`/`AuthorizationEpoch` genuinely unbuilt.

### 2.4 `CompletionContract` (tri-state) + `VerificationPlane`

Modbit: `AGT-025`/`VER-004` (model proposes completion; deterministic/evidence/semantic verifiers return
`VERIFIED | REJECTED | INDETERMINATE`; `INDETERMINATE` never equals success), `VER-002` (`VerificationPlane`
— `DeterministicVerifier`, `EvidenceVerifier`, `ChangeVerifier`, `EnvironmentVerifier`, `SemanticVerifier`,
`GoalVerifier` under one orchestration contract; the verifier model itself is never the authority),
`REJ-006` (explicit anti-pattern: fail-open completion verification is rejected outright — a verifier
error, timeout, or invalid output must yield `INDETERMINATE`, never silent success).

- **Where it lands:** this is the single closest fit in the whole document — `gaps.md`'s own finding #7
  already noted RapidLM's goal-completion gate (V3 invariant #8, `Verification`/`Evidence` as first-class
  `NodeKind`s, `rapid evidence show|verify|export`) is structurally the deepest of any of the three
  products reviewed, but flagged that it was unconfirmed whether the verdict model is genuinely tri-state
  today. **First task here is not new code — it's reading `event-ledger`/`harness`'s actual verdict type**
  to confirm or fix the tri-state shape, then auditing every verifier call site for fail-closed behavior
  per `REJ-006`.
- **Sev/Effort:** P0 / S (audit) → M (if the fix is real, not confirmatory).
- **Audited 2026-08-29 (commit `6ded0d6`) — this note was missing from the file even though that commit's
  own message pointed here; corrected 2026-08-30 to actually say what was found and decided.**
  `crates/agent-runtime/src/evidence.rs::CriterionEvaluator` is the real goal-completion gate (`harness::
  assertions::Verdict` is a different, unrelated binary type for test-harness assertions over exported
  ledger records — not this gate). Confirmed: `CriterionVerdict.satisfied` is a plain `bool`, not tri-
  state, and every error/cancellation path in `evaluate_with_backing` already collapses to `satisfied:
  false` (fail-closed, matching `REJ-006`'s safety bar) rather than ever defaulting to success. **Decision:
  a literal `VERIFIED | REJECTED | INDETERMINATE` enum was deliberately NOT built.** Forcing one in would
  have meant either (a) letting `INDETERMINATE` NOT block completion — violating `REJ-006` outright — or
  (b) making it block identically to `REJECTED`, at which point it's a relabeling with no behavioral
  difference, not a real tri-state gate. Instead: `CriterionUnsatisfied` gained three new reasons
  (`LedgerEventNotFound`/`LedgerEventWrongKind`/`LedgerUnavailable`, replacing one collapsed
  `UnbackedEvidence` for agent-cited ledger references) plus a `retryable()` classifier — `true` only for
  `UnavailableStatus`/`ErrorStatus`/`LedgerUnavailable` (the verifier itself couldn't produce a definite
  result and a rerun with zero new evidence might succeed), `false` for every other reason (`FailedStatus`,
  `MissingEvidence`, etc. — a genuinely new observation is required). The gate's own behavior is
  unchanged either way (`satisfied` stays `false` for both); `retryable` is a caller-facing "try again" vs.
  "this is final" signal layered on top, not a second axis the gate itself consults.
  **Wiring gap found and closed 2026-08-30:** `retryable()` had zero call sites outside its own unit tests
  in `evidence.rs` — the exact "built but unwired" pattern from this document's own §0a meta-finding.
  `apps/rapid/src/goal_host.rs::export`'s per-criterion JSON now includes `"retryable"` (`null` only when
  the criterion is satisfied and has no reason at all); `apps/rapid/src/interactive.rs`'s `goal verify`
  text output now appends `, retryable` to an unsatisfied criterion's line when true. New test
  (`export_emits_snapshot_verdicts_and_attestation`, extended) asserts `retryable: false` for a
  `MissingEvidence` criterion — the `true` branch itself is already covered by `evidence.rs`'s own
  `retryable()` unit tests; constructing a genuine `LedgerUnavailable` scenario through `GoalHost`'s public
  API would need real ledger-resolver-failure plumbing this pass didn't build.
- **Turn-level rollup closed 2026-08-30.** New `CriterionVerdicts::retry_advisable()` (`crates/agent-
  runtime/src/evidence.rs`): true only when completion is currently blocked *and* every blocking
  criterion's reason is retryable — false the moment even one blocker needs a genuinely new observation
  (a single non-retryable reason sinks the whole rollup, deliberately not "any criterion is retryable"),
  and false when nothing is blocked at all (an already-satisfied goal has nothing to retry). Also
  explicitly guards the degenerate empty-verdicts case (`allowed()` reads `false` for zero criteria, which
  would otherwise vacuously satisfy an "all blockers are retryable" check with no blockers to check).
  Wired into both existing surfaces: `goal_host.rs::export`'s JSON gained a top-level `"retry_advisable"`
  key alongside the per-criterion `"retryable"` fields; `interactive.rs`'s `goal verify` text output prints
  `retry advisable: <bool>` once, after the per-criterion lines, only when `complete: false` (printing it
  for an already-complete goal would be meaningless). Three new tests in `evidence.rs`: all-satisfied →
  `false`; a single resolver-outage blocker → `true`, the same blocker as a structural rejection
  (`NotFound`) → `false`; and the actual crux case, two criteria where one is resolver-outage-blocked
  (retryable) and the other has zero evidence recorded at all (`MissingEvidence`, not retryable) →
  `false`, confirming one bad blocker overrides an otherwise-retryable one rather than a naive "any
  retryable" implementation reading `true`. `export_emits_snapshot_verdicts_and_attestation` extended with
  a `retry_advisable: false` assertion for its existing `MissingEvidence` fixture. Full `agent-runtime`
  suite (269 lib tests), full `-p rapid` suite (321 lib tests + all integration binaries), and
  `cargo build --workspace --tests` pass. This closes the "Still open" gap `AGT-025`/`VER-002`'s tri-state
  audit left behind; the "literal `VERIFIED\|REJECTED\|INDETERMINATE` enum deliberately not built" decision
  two paragraphs above still stands unchanged.

### 2.5 Structured `PlanGraph`/`TodoState` outside the transcript + stall detection

**Stall detection implemented 2026-08-29** (the `PlanGraph`/`TodoState` half below is not). New
`detect_stall()` (`apps/rapid/src/host.rs`) scans the last `STALL_WINDOW` (6) tool exchanges for an
identical `(tool, arguments)` call repeated at least `STALL_REPEAT_THRESHOLD` (3) times — a model stuck
re-reading the same file with no distinct progress — and, when found, logs a `--verbose` diagnostic
line via the existing `StepDiag` mechanism on `SupervisedModel::step`. Tests confirm exact-repeat
detection, that varied arguments (real progress) don't false-positive even with the same tool name
repeating, that two repeats stays under threshold, and that an old repetition outside the window doesn't
count against a turn that moved on. **Update 2026-08-30: the warning is no longer diagnostic-only** —
see the dated correction below, which wires it into the model's own context too.

Modbit: `AGT-016` (plan nodes carry status, dependencies, owner, evidence requirements, attempts, and
blockers as durable state outside the transcript — compaction cannot silently change task truth),
`AGT-017` (detect repeated read/edit cycles with no progress and surface the known state plus the
blocker, rather than looping autonomously).

- **Where it lands:** `scheduler` already models this at the graph-node level (`NodeState` now includes
  `Paused` per `02a583b`) — the gap is a **model-visible projection** of plan/todo state that survives
  compaction, not a new state machine. `todo_write` tool exists; wire its state to the graph rather than
  keeping it prompt-only.
- **Sev/Effort:** P1 / M.
- **Model-visible projection implemented 2026-08-30 — the exact "survives compaction, not prompt-only"
  half this item's "where it lands" note asked for.** `todo_write` already persisted durably to
  `.rapidlm/todos.json`, but nothing ever read it back into context — the model only ever saw its own
  todos through the transcript, so compaction (or a fresh `rapid exec` invocation in the same project)
  could lose track of them entirely. Found the fix by tracing how `.rapidlm/MEMORY.md` already achieves
  exactly this for the memory index: `PreservedLiveContext::with_memory_index` feeds a plain string into
  `build_packet` via `CompileInput::new("memory/index", text)` — `CompileInput`, not the closed
  `context_engine::compile::ContextBlock` (an internal-only type the compile pipeline constructs from
  `CompileInput`, with no public constructor of its own — this was the exact wall the stall-detection
  injection hit and correctly declined to force through; todos needed no such wall, since this injection
  point was already open and proven). Mirrored the pattern exactly: new `host::load_todos_index(root)`
  reads and renders `.rapidlm/todos.json` (fail-open on missing/corrupt — persisted plan state is
  advisory context, not something a turn should fail to start over; a malformed individual entry is
  skipped, not fatal to the whole projection), `PreservedLiveContext::with_todos_index`/`todos_index()`
  mirror `with_memory_index`/`memory_index()` field-for-field, and `build_packet` compiles it as a
  `"plan/todos"` system block whenever present. **Deliberately not attempted:** the `scheduler::NodeState`
  graph half — this is the durable, *readable* projection into context, not a structured state machine
  with dependencies/owner/evidence-requirements/attempts that `todo_write`'s flat id/content/status shape
  doesn't carry at all.
- **`AGT-017`'s other still-open half — wiring the stall-detection warning into context — closed
  2026-08-30, at the layer this item's own note above said needed a decision.** Found the right layer by
  tracing exactly why `SupervisedModel::step` (where `detect_stall` already ran, diagnostic-only) is too
  late: it receives `blocks: &[ContextBlock]` already extracted from `live.packet().blocks()` one layer up,
  in `LiveContextModelDriver::step` — by the time `SupervisedModel` sees them, the packet for that step is
  already fixed. `LiveContextModelDriver::step` is the one layer that holds both `input` (so it can call
  `detect_stall(input.history())` itself) and the shared `Rc<RefCell<LiveContext>>` (so it can rebuild the
  packet in place) — exactly the "before it's compiled" seam the todos/memory case had and this one didn't.
  **Implemented:** a new `PreservedLiveContext::stall_warning`/`with_stall_warning` field (module-private
  setter — set only by the driver itself, never a public caller), compiled into a `"diagnostics/stall"`
  system block by `build_packet` (`"Stall detected: {warning}"`), and `LiveContextModelDriver::step` now
  recomputes `detect_stall` on every step and rebuilds the packet in place whenever the result changes —
  a new stall appearing, or a prior one resolving once the model makes distinct progress — so the warning
  is visible on the very same step it's first detected, not one step late, and disappears again once it's
  no longer true. Best-effort: a rebuild failure here is silently skipped, never fails the turn. **One real
  wrinkle found and fixed along the way:** rebuilding the packet needs the current compaction summary too
  (`build_packet(preserved, summary)`), but `summary` lived only on `LiveRecoveryController`, a sibling
  struct the driver has no handle to. Rather than thread it through, moved `summary: Option<String>` onto
  `LiveContext` itself (shared by both, since both now need to rebuild the same packet) — safe because
  `LiveRecoveryController::summary()` was verified dead code (grepped the whole workspace, zero callers)
  before being removed, not just assumed unused. New tests
  `stall_warning_is_compiled_into_the_packet_as_a_system_block` (bounds-checked like every other block, via
  a new `MAX_STALL_WARNING_BYTES`) and `live_context_model_driver_injects_and_clears_the_stall_block_
  across_steps` (drives a real `LiveContextModelDriver` through three steps — no stall, a real stall,
  resolved progress — and asserts the block appears and disappears in `live.packet()` itself, not just in
  `build_packet`'s output in isolation). Full `-p rapid` suite (321 lib tests), `cargo test -p rapid --tests`
  (all integration binaries), and `cargo build --workspace --tests` pass — including the pre-existing
  compaction-recovery tests (`overflow_rebuilds_live_context_and_recovers`, `repeated_overflow_fails_
  closed_with_typed_limit`), confirming the `summary` relocation didn't change compaction behavior. This
  closes `AGT-017` for this item; the `scheduler::NodeState` structured-graph half of `AGT-016` (dependencies/
  owner/evidence-requirements/attempts) remains the one genuinely open piece of §2.5.

### 2.6 `SessionLease` + fencing generation

Modbit: `AGT-018` — a single active mutation owner for a session, with a lease generation counter that
rejects a stale writer. Named explicitly to "prevent desktop/CLI/cloud dual-resume corruption."

- **Where it lands:** directly relevant to RapidLM's daemon/handoff ambitions (`handoff` crate,
  `crates/kernel`'s daemon/IPC). Build this before `rapid resume`/`rapid fork` are exposed across more
  than one concurrent surface — otherwise two clients resuming the same session is a real corruption path,
  not a hypothetical one.
- **Sev/Effort:** P1 / M.
- **Audited 2026-08-30: the actual safety property `AGT-018` asks for already exists, durably and
  cross-process, traced end to end rather than assumed from the type name alone.** Two layers exist, and
  it matters which one is doing the real work. Layer one, `crates/kernel/src/turn/guard.rs`'s
  `TurnSubmissionGuard`/`TurnLease` — `session_id`/`turn_id`/`expected_seq`/`generation` fields, a single
  `occupied: Mutex<HashMap<SessionId, u64>>` admitting one lease per session, a `next_generation:
  AtomicU64` — looks exactly like `SessionLease` by name and shape, but its own doc comment says
  "in-process," and it genuinely is: `apps/rapid` opens a fresh `InProcessKernelClient` per CLI invocation
  (`interactive.rs:2838`), so two separate `rapid` processes (desktop app, CLI terminal, cloud daemon) each
  get their own empty `occupied` map and never see each other through this layer alone. Layer two is the
  one that actually matters here: `submit_turn_sync` (`crates/kernel/src/client.rs:440`) appends
  `EventKind::TurnStarted` via `EventLedger::append` with `AppendOptions { expected_seq: Some(req.
  expected_seq), .. }`, and `append` (`crates/event-ledger/src/ledger.rs:190`) runs inside a SQLite
  `TransactionBehavior::Immediate` transaction that re-reads the durable `last_seq` and returns
  `LedgerError::SequenceConflict` on a mismatch *before* inserting the row — a real, durable,
  cross-process compare-and-append, enforced by SQLite's own transactional file locking on the shared
  ledger file every surface points at, not by anything in-memory. `SequenceConflict` maps to a typed
  `ErrorCode::SessionConflict` (`client.rs:914`), not a generic/opaque failure, so a stale writer gets a
  specific, actionable error. **This is functionally the fencing property `AGT-018` asks for** — a second,
  stale writer's turn-start is durably rejected — just implemented as "the ledger's own `seq` is the
  fencing token, checked transactionally" rather than a dedicated named lease-generation field visible at
  the API layer. `TurnSubmissionGuard`'s in-process layer is a genuine but secondary optimization on top
  (an immediate local `Conflict` without a wasted DB round-trip within one process), not the actual
  safety boundary. `fork_session`/`rewind` don't need the same analysis: fork always targets a brand-new
  session id (no contention with the source session), and `rewind_sync` is read-only replay (no ledger
  write at all). **Re-scoping, not closing:** the core "prevent dual-resume corruption" property is real
  and already shipped; what's still genuinely open is polish, not safety — e.g. whether a `SessionConflict`
  surfaces to an end user as "another surface is actively using this session" (actionable) versus a bare
  typed-but-generic conflict message, and whether `TurnSubmissionGuard`'s in-process generation counter is
  worth exposing at all now that the real fencing lives one layer down. Neither was investigated further
  this pass — genuinely minor compared to the safety property itself, which is what this item was actually
  named for. Downgrading from P1/M ("build this") to P3/S ("polish the conflict message if it ever comes
  up as a real UX complaint") given the hard safety guarantee already holds.
- **The conflict-message half of that P3/S polish closed 2026-08-30 — but narrower than a first read of
  "Session conflict" suggests, checked by grepping every occurrence in the crate before touching any of
  them.** `"Session conflict"` as a literal string appears six times across `crates/kernel`
  (`client.rs` ×3, `turn/guard.rs`, `session/service.rs`, and transitively via `recovery`), but five of
  those six genuinely describe the same real-world situation — an occupied lease, a stale `expected_seq`,
  or a seq-out-of-range subscribe request are all "somebody else already moved this session, retry" from
  a caller's point of view, and using one consistent message for them is correct, not a bug. **Only
  `ledger_api`'s combination of `LedgerError::SessionExists` and `LedgerError::SequenceConflict` was a
  real conflation** — a client-side id collision on session *creation* has nothing to do with the
  cross-process dual-writer race `AGT-018`'s fencing exists to catch, and both fell into "Session
  conflict" the same way. Split into `"A session with this id already exists"` /
  `"Another writer already advanced this session past the expected sequence"`, same `ErrorCode::
  SessionConflict` for both (severity is genuinely equal — this is a message-clarity fix, not a
  reclassification). New test `ledger_api_gives_session_exists_and_sequence_conflict_distinct_messages`
  constructs both `LedgerError` variants directly (no real session/ledger needed) and asserts the codes
  match but the messages don't. Confirmed no existing test depended on the old shared string (every
  existing assertion on this path checks `.code()`, never `.message()`). Full `-p kernel` suite (166 lib
  tests), full `-p rapid` suite (323 lib tests + integration binaries), and `cargo build --workspace
  --tests` all pass. `TurnSubmissionGuard`'s in-process generation counter (this note's other named
  "polish" item) remains untouched — genuinely lower value now that the real fencing is known to live at
  the ledger layer, not something to chase without a concrete reason to.
- **New finding, 2026-08-30: a whole crash-recovery pipeline exists, is real and tested, and is never
  invoked — but fixing the wiring wouldn't currently do anything observable, because the feature that
  would need it is itself one of the already-documented no-op `KernelAction` variants.**
  `crates/kernel/src/recovery/mod.rs::RecoveryManager::recover_session` (module doc: "verifies storage,
  loads the newest compatible checkpoint... replays later committed events, applies interrupt/pause
  actions... idempotent across repeated startup") is exactly the mechanism that should clear a session's
  `active_turn` marker after a crash mid-turn. It has zero callers anywhere outside `crates/kernel`'s own
  tests and `crates/kernel/tests/crash_recovery.rs`. Traced the actual consequence: `TurnSubmissionGuard::
  validate_seq` (§2.6's own subject above) refuses `submit_turn` whenever `snapshot.active_turn().is_some
  ()` — durably true forever once a turn starts, cleared only by that turn completing/failing/being
  interrupted through the normal path. A process killed mid-turn leaves `active_turn` durably `Some` with
  no live process left to ever clear it; without `recover_session` (or equivalent) running on the next
  open, that session can never submit another turn again — every future attempt hits `SessionConflict`
  permanently. **But this has no current observable path to a user**, because the only way to reconnect to
  an existing session at all is `KernelAction::ResumeSession`, and the earlier audit of `apply_kernel_
  action` (this same section's TUI-dispatch finding under Phase 1 §1.4 item 13) already confirmed
  `ResumeSession` is one of the 30 variants that hits the silent `KernelApi::Approve | KernelApi::Dispatch
  => {}` no-op arm — resume itself doesn't work yet, crashed or not. Wiring `recover_session` in without
  also building real resume dispatch would fix nothing a user could exercise, and building real resume
  dispatch is itself real, separate feature work (this document's own TUI finding already flagged it as
  needing dedicated attention, not a quick pass) — so this is disclosed as a real, latent bug that will
  matter the moment `ResumeSession` gets properly implemented, not attempted as its own fix now. Not
  confused with two other things named "recovery" in this codebase, checked directly: `host.rs`'s
  `LiveRecoveryController` is unrelated context-overflow/compaction recovery (§2.7), and `GoalCommand::
  Pause`'s `process_recovered` is an in-memory goal-pause flag, not this ledger-level pipeline.

**Correction to the correction (2026-08-29):** the "verified genuinely absent" note directly below was
itself wrong — a methodology bug, not a re-check of source: the grep only covered
`crates/context-engine/src/*.rs` (one directory level), missing `crates/context-engine/src/index/
graph.rs` entirely. **`CodeGraph::impact()` already exists there** — an incoming-only BFS over
callers/importers of a given symbol, hop- and result-bounded, fully implemented — which is exactly
Next-Edit-Ripple. Like nearly everything else in this document, it's unused outside its own file (grep
for `.impact(` across `apps/`/`crates/*/src` confirms zero external call sites). The real remaining work
is: resolve a file-level edit to the `SymbolLocator`(s) `impact()` needs (it takes a symbol, not a file
path), and expose the result as a new model-facing tool or an automatic post-edit annotation — genuine,
moderate-sized wiring work, not a "build a traversal algorithm" task as the paragraph below still assumes.
Lesson for future passes: a "not found" grep result is only as good as its glob — check subdirectories
before writing "genuinely absent" anywhere in this document.

Also newly confirmed: `context-engine::compact`'s whole compaction system (`compact.rs` + `compact_policy.rs`) is
itself unwired from `apps/rapid` — `compact_packet` is only ever called from within
`compact_policy.rs`, in the same crate, never from the exec loop. So Phase 1 §1.4 row 13's "explicit
fast-path" framing undersells it: compaction isn't reachable *at all* today, deterministic or model-based.
**Correction (2026-08-30): this specific "not reachable at all" claim is now stale — checked directly
against `apps/rapid/src/host.rs`, not re-assumed.** `LiveRecoveryController::recover_from_overflow` calls
`compact_with_policy(live.packet(), &policy, None, &CeCancel::new())` on every real context overflow (a
long-lived automatic path with its own dedicated tests, e.g. `overflow_rebuilds_live_context_and_recovers`
— this predates the current implementation pass, not something added by it), and `compact_with_policy`
itself calls `compact_packet` internally (`compact_policy.rs:221`). So the narrow literal claim (`compact_
packet` itself has exactly one call site, inside `compact_policy.rs`) still holds, but "compaction isn't
reachable at all today" does not — the deterministic fallback path runs, automatically, whenever a real
turn overflows its context budget. This actually matches §1.4 item 13's own more careful framing exactly
(automatic fallback already existed; only an *explicit, user-requested* mode was ever the real gap) —
that item's framing was right and this paragraph's stronger claim was the one that needed correcting. The
explicit-mode gap folds into the TUI kernel-dispatch finding under item 13 itself: `/compact` is one of the
30 `KernelAction` variants confirmed silently no-op in `apply_kernel_action`, not a separate unwired-crate
problem the way this paragraph originally framed it.

**Next-Edit-Ripple implemented 2026-08-30 — the "genuine, moderate-sized wiring work" the note above
anticipated, not the traversal algorithm (already existed).** Two small, additive `context-engine` reads
closed the "resolve a file to symbols, resolve a symbol back to a file" gap `CodeGraph::impact()` itself
never needed for its own BFS: `CodeGraph::symbols_at_path(repo_id, path)` (a direct indexed query against
`context_graph_symbols`'s existing `path` column — no schema change) and `CodeGraph::symbol_label(locator)`
(resolves one `impact()` edge endpoint back to a `(fq_name, RepoPath)` for display, `None` on any lookup
miss — advisory, never a hard error). New `apps/rapid/src/context_retrieval.rs::ripple_advisory(root, path)`
sits alongside the existing `retrieve()` (same module, same persistent `.rapidlm/index/`, same fail-open/
bounded-timeout philosophy): after a successful `workspace_write`/`workspace_patch`, it walks (stat/
eligibility only, no parsing — bounded by `MAX_RIPPLE_WALK_FILES`) looking specifically for the one
just-written path, indexes only that match, then calls `impact()` on that file's symbols and renders the
callers it finds as an advisory note appended to the tool's own summary — the same shape every other
scanner advisory in `exec_tools.rs` already uses. Deliberately does **not** re-walk and re-index the whole
repo per write the way `retrieve()` does once per turn (a multi-edit turn would otherwise pay a full walk
per edit) — indexing is scoped to the one changed file only. **Important, explicitly documented
limitation:** a caller only shows up if it was already indexed by an earlier `retrieve()` call this
session (persisted across turns, but genuinely absent on a fresh project or if retrieval was skipped) —
this is "what the graph already knows," not an on-demand full dependency audit; verified this precisely
with a test that fails without an earlier `retrieve()` call and passes with one. New tests: two in
`context-engine::index::graph` (`symbols_at_path_finds_only_that_files_symbols`,
`symbol_label_resolves_next_edit_ripple_end_to_end`, the latter exercising the whole
resolve→impact→resolve chain against a real two-file call graph) and one in `apps/rapid`
(`ripple_advisory_names_the_file_that_calls_the_edited_function`, covering the positive case, an
uncalled function producing no advisory, and a nonexistent path failing open). Full `context-engine`
suite (307 tests, up from 305) and full `rapid` lib+integration suite (295 lib tests) both pass, plus
`cargo build --workspace --tests`. **Still not attempted:** `CTX-013`/`CTX-014`'s full Context Pack
Compiler/Workspace Capsule and `CTX-003`'s retrieval-before-edit guardrail (surfacing *inadequate*
context as a distinct condition) — this closes only the Next-Edit-Ripple half of this section.

Modbit: `CTX-013`/`CTX-014` (a bounded, provenance-carrying, task-specific context package for execution
and handoff — "a context package grants no permissions"), `CTX-017` (tagged MOAT — graph-driven affected
files/symbols/tests/config as change-impact follow-up after an edit, revision/evidence bound), `CTX-003`
(tagged MOAT — retrieval-before-edit: a material edit requires adequate relevant context to already be
retrieved; surface *inadequate context* as a distinct condition rather than editing blind).

- **Where it lands:** RapidLM's V3 invariant #12 ("subagents receive minimal typed task/context envelopes,
  not the full parent transcript") already states this goal — `apps/rapid/src/host.rs`'s
  `PreservedLiveContext` (not `context_retrieval.rs` — corrected 2026-08-30; it's the live context
  envelope used throughout `host.rs`/`interactive.rs`/`exec_tools.rs`/`model.rs`) is the concrete
  substrate to formalize into a named, reusable capsule. Next-Edit-Ripple builds directly on
  `context_engine::index::graph::CodeGraph`, which `gaps.md` finding #5 already confirmed exists and is
  scout-reachable but unused for call-site validation.
- **Sev/Effort:** P1 / M.

### 2.8 Model router upgrade: capability catalog + `RouterDecisionRecord` + real cost accounting

**Correction (2026-08-29):** the capability-catalog half of this is already substantially built, and the
cost-plumbing half is now **implemented** — see Phase 1 §1.5 row 17 above for both. What's still
genuinely missing: `RouterDecisionRecord` (no auditable routing-decision record exists), and surfacing
cost in the headless JSONL contract specifically — `apps/rapid/src/headless/jsonl.rs::session_finished`
has no call sites at all outside its own tests, so it isn't wired into `rapid run --jsonl` yet regardless
of cost; extending it is real, separate work once it's wired up at all.

**Correction + partial fix (2026-08-30): `session_finished` now has a real call site, closing half of
the "isn't wired up at all" gap above — but not via `rapid run`.** Tracing where `--jsonl` was actually
supposed to land surfaced a bigger, adjacent finding: `CLI_USAGE`'s own "Target V3 command surface" text
lists dozens of subcommands (`run`, `resume`, `fork`, `rewind`, `daemon`, `acp`, `graph`, `context`,
`evidence`, `process`, `computer`, `mcp`, top-level `hooks`/`plugins`/`skills`, `eval`, top-level
`sandbox`) that `run_subcommand`'s actual match has no arm for at all — `rapid run <goal/playbook>`, the
JSONL contract's own documented home ("`rapid run --jsonl` writes only protocol records to stdout"), is
one of these: entirely unbuilt, not merely unwired. Building the real "durable graph run" feature is
large, separate work. What genuinely was tractable: `rapid exec` — a real, working, already-tested
one-shot flow — reusing the *same* JSONL contract (`headless::jsonl`) for the same underlying purpose
(machine-parseable output + a typed exit code), without pretending this is the full durable-graph `run`
command. **Implemented:** `rapid exec --jsonl` writes `rapid.schema`, then (on the turn's outcome) an
`assistant.message` record carrying the same text the plain path would print (the `--json-schema`
captured JSON when present, else the model's own summary) *only when the turn produced one*, then
`session.finished` with the real typed exit code — reusing `exec_turn_exit_code`/`AgentExecutionError`'s
own `error_code()` mapping built for the plain-text path, so the two paths can never silently disagree
on what code a given failure gets. New `JsonlRecord::assistant_message` constructor (mirrors the existing
`EventKind::ModelCompleted → "assistant.message"` mapping `from_event` already used, byte-identical wire
shape, golden-JSON-tested) and `now_rfc3339()` (new `time` crate dependency — already resolved
transitively in `Cargo.lock`, so this promotes an existing dependency to direct rather than adding a new
one to the supply chain; `formatting`-only, no `parsing`, since this only ever stamps "now"). **Narrower
than full correctness, disclosed rather than silently gapped:** `--jsonl` only covers the turn-execution
outcome — a pre-flight setup failure (bad model config, bad `--json-schema` document, a permission-
lattice error) still exits with the correct typed code but without a `session.finished` record, since no
JSONL writer or session id exists yet at that point in `exec_turn`. Extending every one of those ~15
early-return sites to also finish the JSONL sequence is real, separate follow-up work, not attempted here
to keep this change reviewable. Streaming individual `assistant.delta`/`tool.*` events as the turn runs
(rather than one `assistant.message` at the end) is also not attempted — `exec_turn` only collects
`agent_runtime::TurnEvent`s in-memory, not through a real committed `EventLedger` with durable per-event
seq numbers, so `JsonlRecord::from_event`'s intended ledger-backed path doesn't apply to this one-shot,
non-durable flow at all.

Modbit: `MOD-003` (a model capability catalog — context window, tool/parallel/vision/reasoning/
structured-output support, latency, cost, health — "do not route by model name alone"), `MOD-004`
(`ModelCapabilityVector` + `TaskFingerprint`: hard constraints first, then soft optimization for predicted
quality/latency/cost/reliability), `MOD-005` (`RouterDecisionRecord` — requested model, resolved/fallback
model, routing reason, policy version, estimated vs. actual cost — "routing must be auditable"),
`AGT-028` (exact per-run/per-step cost and token accounting).

- **Why this is worth prioritizing over other Phase 2 items:** it's a clean, uncontested lead. Neither
  Grok Build (single provider, no router) nor Qwen Code (5 providers, real adapters, but no dollar-cost
  field anywhere in its own headless contract — confirmed in this session's Qwen Code audit) has this.
  RapidLM's `llm-router` crate already has the filter/score/fallback shape neither competitor matches;
  this closes the one piece it's missing.
- **Where it lands:** `llm-router` (catalog file per `gaps.md` remediation item 20, citing Grok Build's
  `xai-grok-models/default_models.json` as the closest open reference for the catalog *shape*, not its
  content) + `headless` (surface `cost_usd` in the JSON contract, closing Phase 1 §1.5 row 17).
- **Sev/Effort:** P1 / M.
- **`RouterDecisionRecord` implemented 2026-08-30.** `apps/rapid/src/host.rs::FallbackChainModel::step`
  already computed exactly the shape `MOD-005` asks for at every retry/fallback/stop decision — it just
  threw the information away into an ephemeral `--verbose`-gated diagnostic string (`diag_line`) instead
  of a structured, queryable record. Added `RouterDecisionRecord` (`requested_model`, `resolved_model`,
  `reason: RouterDecisionReason` — `RetrySame`/`FallbackTo`/`Stop(reason)`) and `RouterDecisionLog`, an
  `Arc<Mutex<Vec<_>>>` handle mirroring `CostAccumulator`'s existing shape: cloned out of the
  `FallbackChainModel` before it's moved into `SelectedModel`/`run_live_exec`, read back in
  `interactive.rs::exec_turn` once the turn resolves. The common case — a turn that never needed to retry
  or fall back — produces zero records, not a record saying so; absence *is* the "used the requested
  model, no incident" signal. Surfaced two ways: unconditionally on stderr (a mid-turn model switch is
  operationally significant enough to show without `--verbose`) and, in `--jsonl` mode, one new
  `router.decision` record per entry (`requested_model`/`resolved_model`/`reason`, the last a short
  machine-stable tag: `"retry_same"`/`"fallback_to"`/a `StopReason::as_str()` value) written after
  `rapid.schema` and before the outcome records, with `seq` kept monotonic across all of them. **Not
  attempted:** `policy_version` and `estimated vs. actual cost` per decision (`MOD-005`'s full field list)
  — no policy-versioning concept exists anywhere yet to cite, and per-decision cost attribution would need
  threading `CostAccumulator`'s per-step values back into whichever attempt they belonged to, not just the
  turn-level total this codebase currently tracks; both are real, separate follow-up.
- **Unrelated security fix found while auditing this crate, 2026-08-30: an integer overflow in chunked
  HTTP decoding let any configured provider crash the process.** `crates/llm-router/src/providers/
  openai_compatible.rs::decode_chunked` (shared by both the OpenAI-compatible and Anthropic adapters via
  `HttpTransport`) parses each `Transfer-Encoding: chunked` chunk-size line as a hex `usize` with only the
  *line's byte length* bounded (`MAX_HEADER_LINE_BYTES`, 8 KiB) — nothing bounds the *value* it can
  encode, so a malicious or compromised provider can send a chunk size up to `usize::MAX`. The running
  total (`body.len() + len`) and the resize target (`start + len`) both used raw, unchecked addition: a
  second chunk sized to wrap the sum past `usize::MAX` back to a small number silently passed the
  `> max_body` bound check, then wrapped the resize target too, truncating the buffer while `start`
  stayed put — `&mut body[start..]` then panicked on an out-of-range slice (or, in a debug build, the
  addition itself panics first with "attempt to add with overflow" — either way, one crafted response
  crashes the process, not just a bad completion). **Fixed:** both additions now go through `body.len()
  .checked_add(len)`, mapping the overflow case itself to `ProviderError::BoundExceeded` — exact, not an
  approximation, since no legitimate chunk size ever needs to overflow a 64-bit sum against any real
  `max_body`. New test `a_chunk_size_that_would_overflow_the_running_total_fails_closed_not_a_panic`:
  confirmed it reproduces the real panic against the unfixed code first (`attempt to add with overflow`
  at the exact line), then confirmed the fix turns it into a clean `Err(BoundExceeded)`. The existing
  `truncated_or_oversized_chunked_bodies_fail_closed` test never combined a nonzero prior body length with
  a wrap-inducing size, which is why this wasn't caught before. Full `-p llm-router` suite (139 lib
  tests), full `-p rapid` suite (323 lib tests + integration binaries), and `cargo build --workspace
  --tests` all pass.
- **Second unrelated security fix, 2026-08-30, same audit sweep: a symlinked intermediate directory could
  get real directories created outside the workspace root before the escape was ever detected.**
  `apps/rapid/src/exec_tools.rs::WorkspaceTools::resolve_in_root` already correctly refused a symlinked
  *leaf* (`ln -s /etc/passwd leak.txt`, tested by the existing `symlinked_leaf_escape_is_refused_on_read_
  and_write`) — but its handling of the containing *directory* had a real ordering flaw: it called
  `fs::create_dir_all(parent)` on the whole parent path first, then canonicalized and rejected it only
  afterward if it resolved outside the root. If an intermediate component were a pre-existing symlink into
  a directory outside the workspace (`ln -s /tmp/evil root/link`, then a write to `link/sub/file.txt`),
  `create_dir_all` would follow that symlink and create `sub` inside `/tmp/evil` *before* the
  canonicalize-and-reject check ever ran — the final file write was still correctly refused, but the
  escape (a real directory created outside the root) had already happened on disk. Confirmed with a real
  reproduction before fixing: a new test failed against the unfixed code exactly this way. **Fixed:**
  `resolve_in_root` now walks the path one component at a time — for each intermediate level, check first
  (`symlink_metadata` + canonicalize + `starts_with(root)`) and only create that single level
  (`fs::create_dir`, not `create_dir_all`) if it's genuinely missing, before ever stepping into the next
  component. Nothing is created or entered past the point a symlink is found to lead outside the root,
  closing the exact race the old "create everything, then check" ordering had. New test
  `symlinked_intermediate_directory_creates_nothing_outside_the_root`: seeds a directory symlink, attempts
  a nested write through it, confirms the write is refused *and* that no directory was created outside the
  root — reproduced against the unfixed code first (failed exactly as predicted), then confirmed passing
  after the fix. Full `-p rapid` suite (324 lib tests, both symlink tests passing together) and
  `cargo build --workspace --tests` pass — including every existing nested-directory write test, so the
  common (non-symlinked) case is unaffected.

### 2.9 Persisted, content-hash-keyed review findings + `PatchPolicyGate`

Modbit: `VER-007`/`VER-008` (a structured `Finding` model — category, severity, confidence, rationale,
rule, evidence provenance — where dismissed/resolved findings survive reruns keyed by content hash, so a
rerun doesn't resurface something already triaged, but changed/new findings are never hidden), `VER-009`
(`PatchPolicyGate` — security/license/attribution/secret/static/test gates run before commit/merge, and
their results become evidence, not just a console warning).

- **Where it lands:** `security` crate already has a working `Finding`-lifecycle type for secrets scanning
  (`security::scanners::secrets::Finding`, cited in `gaps.md` finding #4) — extend its shape to cover
  general review findings and key persistence by content hash rather than by line number (line numbers
  shift; content hashes don't).
- **Sev/Effort:** P2 / M.
- **Correction + partial fix (2026-08-30): the secrets scanner is more built than the doc credits, and
  now has its first production call site — advisory-only, not the `PatchPolicyGate` this item actually
  asks for.** Beyond `security::scanners::secrets::Finding`/`FindingFingerprint` (a real, mature,
  prefix/pattern-based scanner: AWS keys, GitHub/Slack tokens, private keys, connection strings, high-
  entropy assignments), there are three more parallel typed `Finding`+content-hash-fingerprint scanners —
  `PatchFinding`, `CommandFinding`, `ExternalFinding` (`scanners/patch.rs`, `command.rs`, `external.rs`)
  — all already content-hash-keyed (`VER-007`'s actual ask), all with zero call sites anywhere in
  `apps/rapid`, entirely dormant. **Implemented:** `apps/rapid/src/exec_tools.rs::execute_write`'s plain
  (non-shadow-diagnostics) write path now runs the secrets scanner over newly-written content and appends
  an advisory note to the tool's own success summary when it finds something (`"advisory: possible
  secret(s) detected (rule_id, ...) — verify before committing"`) — deliberately never blocking the
  write: a scanner false positive (e.g. a high-entropy test fixture) must never break a legitimate
  workflow, matching this codebase's model-correctable-not-fatal philosophy (the model sees the note in
  its own tool result and can redact/rewrite if the flag is real). **Explicitly not `PatchPolicyGate`:**
  this is a notification, not a gate — nothing here blocks a commit/merge, nothing becomes durable
  evidence, and the other three scanners (patch/command/external) and the dismiss/resolve/triage
  persistence store this item's title actually names (confirmed: no dismiss/resolved/triage persistence
  exists anywhere in the crate) remain entirely unattempted. Also not covered: `execute_write`'s shadow-
  diagnostics branches and `execute_patch`'s written content — scoped to the one plain-write path to keep
  this change reviewable, not a signal that those paths are exempt from the same risk.
- **Persistence half of `VER-007` implemented 2026-08-30 — the "dismissed findings survive reruns keyed
  by content hash" part specifically, not the full `PatchPolicyGate`.** New
  `apps/rapid/src/findings_store.rs::FindingsStore`: a project-local `.rapidlm/findings.json` (alongside
  `.rapidlm/todos.json` — project-local, not per-user, since a team's triage decisions are project facts)
  mapping a `FindingFingerprint`'s hex straight to a dismissal reason. `scan_for_secrets_advisory` now
  loads it and filters out already-dismissed fingerprints before building its advisory note, so a
  dismissed finding never resurfaces on a rerun of the *same* content — but a changed finding at the same
  location gets a different fingerprint (the hash covers rule/path/byte-range/match-content) and is never
  silently hidden by an old dismissal, exactly `VER-007`'s stated invariant. New `rapid findings
  list|dismiss <fingerprint> --reason <text> [--root <path>]` CLI surface (`p9_commands.rs::run_findings`)
  makes it actually usable, not just inert plumbing — every advisory note itself now also prints the
  fingerprint so there's something to pass to `dismiss`. Caught a real bug in this pass, not just in the
  new code: the advisory message's own wording, `"possible secret(s) detected"`, put an unrelated `(` (in
  `"secret(s)"`) before the fingerprint's own parenthesis — a naive fingerprint-extraction test failed
  silently on the wrong substring until traced; fixed by rewording to `"possible secrets detected"`
  rather than papering over it in the test. **Still not attempted, and this is the bulk of `PatchPolicyGate`
  (`VER-009`) itself:** an actual gate (something that can block a commit/merge, not just advise), the
  three other dormant scanners (patch/command/external) wired the same way, and any concept of "results
  become evidence" (a durable, queryable record of what was checked and why it passed/failed) — a
  dismissal file recording what a human decided is not the same as the gate deciding anything itself.
- **Checked whether `CommandFinding` (the `shell_exec` scanner) is as quick a wire-up as `secrets` was,
  2026-08-30 — it is not, and here is exactly why, for whoever picks this up next.**
  `security::scanners::command::CommandRiskScanner::scan` takes a
  `capability_broker::CanonicalCommand`, not a raw argv `Vec<String>` — and `CanonicalCommand` has no
  public constructor; it's only built via `normalize_exec()` against a real `Resolver` impl (executable
  PATH resolution + cwd canonicalization, the exact ceremony `apps/rapid/src/sandbox_exec.rs::
  resolve_program` already implements standalone for a different purpose). `execute_shell`
  (`exec_tools.rs`) has none of this machinery today — no `Resolver`, no `normalize_exec` call anywhere
  in `apps/rapid`. Wiring this scanner in is thus a real, self-contained integration on the same order as
  building `sandbox_exec.rs` was (a `Resolver` impl, `normalize_exec` call, `ShellMode::Argv` vs.
  `ShellString` handling), not a copy of `scan_for_secrets_advisory`'s shape — noted precisely rather than
  forced through under time pressure. `PatchFinding`/`ExternalFinding` weren't checked with the same
  rigor and may or may not have the same shape; verify each independently before assuming either way.
- **Correction — `CommandFinding` WAS wired in after all, 2026-08-30, reversing the assessment above once
  the integration was precisely scoped.** The "real, self-contained integration on the same order as
  building `sandbox_exec.rs`" turned out to be reusable rather than duplicable: `sandbox_exec::
  resolve_program` (executable PATH/root-relative resolution) was made `pub(crate)` and reused as-is, and
  the `Resolver` ceremony needed only a trivial "already resolved, just validate" impl — copied from
  `p9_commands.rs`'s existing `FrozenPathResolver` as `exec_tools.rs::AlreadyResolvedPathResolver`. New
  `exec_tools.rs::scan_command_advisory(root, argv)`: resolves `argv[0]` via `resolve_program`, builds an
  `ExecIntent`/`CanonicalCommand` via `normalize_exec`, runs `security::CommandRiskScanner::scan`, and
  filters findings through the same `FindingsStore` from the secrets correction above (so `rapid findings
  dismiss <fingerprint>` works uniformly across both scanners — `FindingsStore` is keyed by fingerprint hex
  string, not a scanner-specific type, exactly because it was built generic). Wired into `execute_shell`'s
  plain synchronous path only (not the sandboxed or background paths), appending an advisory note to the
  success summary the same way `scan_for_secrets_advisory` does — never blocking, matching the same
  model-correctable-not-fatal philosophy. Verified with a new `#[cfg(unix)]` test
  (`shell_exec_flags_a_dangerous_command_but_never_blocks_it`, using `rm -rf <path>` to trigger
  `command.rm_destructive`) plus the full `-p rapid` lib+integration suite (292 lib tests, all integration
  binaries) and a full `cargo build --workspace --tests`, all green. **Still not covered:** the sandboxed
  and background `shell_exec` paths (same rationale as secrets' shadow-diagnostics gap — scoped narrow to
  keep the change reviewable), and `PatchFinding`/`ExternalFinding` remain entirely unattempted.
- **Correction — `PatchFinding` wired in too, 2026-08-30, and it needed none of `CommandFinding`'s
  ceremony.** Checked independently as flagged above: `security::PatchScanTarget::create` takes a
  `protocol::RepoPath` directly, no `Resolver`/`normalize_exec` involved — the simplest of the three
  dormant scanners to wire. New `exec_tools.rs::scan_patch_advisory(root, path, content)`: parses
  `args.path` as a `RepoPath` (already relative/traversal-free from `checked_relative`), builds a
  `PatchScanTarget::create(.., executable: false)` unconditionally (this write path has no chmod
  capability, so a target it produces is never actually executable, and the content-scanning rules that
  can fire here — credential paths/material, CI-release paths, sudoers, hook paths — don't distinguish
  `Create` from `Replace`; only `Delete`/`Move` do, and this path never produces either), runs
  `security::PatchScanner`, and filters through the same `FindingsStore` the other two scanners use.
  Wired into `execute_write`'s plain path alongside (not instead of) the secrets scan — a single write can
  now carry both advisory notes. Verified with a new test
  (`workspace_write_flags_a_patch_policy_issue_but_never_blocks_the_write`, writing a `write-all` GitHub
  Actions workflow to trigger `patch.ci_permissions_broaden`) plus the full `-p rapid` lib+integration
  suite (293 lib tests) and `cargo build --workspace --tests`, all green. **Still unattempted:**
  `ExternalFinding`, and the same shadow-diagnostics/`execute_patch` paths the secrets scanner also skips.
- **`CommandFinding`'s "not covered" background/sandboxed paths closed 2026-08-30.** `scan_command_advisory`
  had only ever been wired into `execute_shell`'s plain synchronous path; the macOS Seatbelt job path, the
  non-macOS `sandbox_exec::run_sandboxed` path, and the plain background-job path (`self.jobs.start`) all
  ran a command's real argv unscanned. All three now call the same `scan_command_advisory(self.root(),
  &args.argv)` (the real argv, not the Seatbelt-wrapped `sandboxed` vec that prepends `sandbox-exec -f
  <profile>`) and append the note to their own success summary — `"started sandboxed job ..."`, `"sandboxed
  exit ..."`, `"started background job ..."` all now carry the advisory the same way the plain path's
  `"exit 0 ..."` already did. New `#[cfg(unix)]` test
  (`shell_exec_flags_a_dangerous_command_on_the_background_and_sandboxed_paths_too`) exercises both the
  background and sandboxed paths with a real `rm -rf` against a harmless nonexistent target, on this
  actual macOS dev machine (so the Seatbelt branch, not the `run_sandboxed` fallback, is what's really
  covered here — `run_sandboxed`'s own branch gets no direct test since `find_sandbox_exec()` always
  succeeds on macOS). Full `-p rapid` suite (296 lib tests) and `cargo build --workspace --tests` pass.
  `ExternalFinding` and `execute_patch`'s equivalent command-adjacent surfaces remain the only pieces of
  this section still untouched.
- **`execute_patch`'s own "not covered" gap closed 2026-08-30 too — secrets, patch-policy, and ripple all
  now scan the patched content, not just fresh writes.** Both success tiers of `execute_patch` (exact-match
  and whitespace-insensitive) now run `scan_for_secrets_advisory`/`scan_patch_advisory`/`ripple_advisory`
  against `updated` (the post-patch file content) before returning, in the same order `execute_write`
  already uses. This was flagged as a gap independently in both the secrets correction ("not covered:
  ... `execute_patch`'s written content") and the patch-policy correction ("still unattempted: ... the
  same shadow-diagnostics/`execute_patch` paths") — both close with this one change, since both scanners
  take the same `(root, path, content: &[u8])` shape regardless of which tool produced the bytes. New test
  `workspace_patch_scans_the_resulting_content_like_workspace_write_does` exercises both success tiers:
  a secret introduced via an exact-match patch, and a `patch.ci_permissions_broaden` finding reached only
  through the whitespace-insensitive fallback (confirming the scan runs on that tier too, not just the
  exact-match one). Full `-p rapid` suite (297 lib tests) and `cargo build --workspace --tests` pass.
- **`execute_write`'s shadow-diagnostics branches closed too, 2026-08-30 — every write/patch path in
  `exec_tools.rs` now runs the same three scans.** Rather than repeat the three `if let Some(note) = ...`
  blocks a fifth and sixth time, extracted `append_write_advisories(summary, root, path, content)` — the
  one place all three scanners (secrets, patch-policy, ripple) are called from now, used by
  `execute_write`'s plain path, both its shadow-diagnostics outcomes (`Passed`/`Skipped`), and both
  `execute_patch` match tiers alike, instead of five near-identical call sites drifting independently.
  Scans run *after* shadow diagnostics has already decided to apply the write (still advisory-only,
  appended to the same success summary) — not *before* the shadow-diagnostics candidate is accepted, which
  would be a real, different verify-then-apply semantics change and wasn't attempted. Extended
  `shadow_diagnostics_applies_a_passing_write_for_real` to also assert a secret introduced through that
  path is flagged, not just the plain path. Full `-p rapid` suite (297 lib tests, unchanged count since
  this extended an existing test rather than adding a new one) and `cargo build --workspace --tests` pass.
- **The actual `PatchPolicyGate` (`VER-009`) — the one piece of this whole section repeatedly flagged as
  "the bulk of it remains unattempted" across every prior correction — implemented 2026-08-30 for its
  single most meaningful boundary: `git commit`.** Every scanner wired this session (secrets, patch-
  policy) was advisory-only by design — a note the model sees and can act on, never a block. This item's
  actual ask was different: a real gate that blocks the commit boundary itself. New `exec_tools.rs::
  scan_git_commit_gate(root, argv)`: when `shell_exec`'s plain path is about to run a plain `["git",
  "commit", ...]` call, it reads `git diff --cached --name-only` for the staged file list, `git show
  :<path>` for each file's staged content (not working-tree content — the two can differ), and runs
  `scan_for_secrets_advisory`/`scan_patch_advisory` on each — the exact same scans and the exact same
  `FindingsStore` dismiss mechanism every other scanner in this file already uses, reused verbatim rather
  than reimplemented. Any non-dismissed finding refuses the commit outright (`ToolStepResult::Failed`,
  handled, model-visible) *before the `git commit` process is even spawned* — the real, qualitative
  difference from every other scanner call site, which only ever appends a note to an already-succeeded
  result. Fails open on anything that isn't a real, introspectable git repo with staged changes (a repo
  `git diff --cached` can't run against must not be blocked by a check that can't run) — this can only
  ever narrow which commits succeed, never widen what's allowed. Caught a real bug in the message wording
  while writing the test, the same class as the earlier `"secret(s)"` bug this session already fixed once:
  `"unresolved findings (PatchPolicyGate):"` put a confusable `(` before the finding's own `(fingerprint)`
  parenthesis; reworded to `"blocked by the PatchPolicyGate: staged changes have unresolved findings:"`
  rather than patching around it in the test. Two new tests:
  `git_commit_is_blocked_by_an_unresolved_secret_in_staged_content` (a real git repo, a real staged secret,
  a real blocked commit confirmed via `git log`, then a real successful commit after `rapid findings
  dismiss`) and `git_commit_with_no_findings_is_never_gated` (a clean staged file commits normally). Full
  `-p rapid` suite (301 lib tests) and `cargo build --workspace --tests` pass. **Explicitly not covered:**
  `git commit` wrapped in a shell string (`["sh", "-c", "git commit ..."]`, undetectable since `shell_exec`
  never interprets shell strings) is not gated; and "results become evidence" (a durable, queryable record
  of what was checked, distinct from a dismissal file recording a human's decision) remains the one part
  of `VER-009` genuinely unaddressed — the gate now decides something real, but that decision isn't
  recorded anywhere durable beyond the commit either succeeding or being refused.
- **`git merge` gate implemented 2026-08-30, the same day — the "separate, similarly-shaped follow-up"
  flagged above.** New `scan_git_merge_gate(root, argv)`, sharing the actual scanning (`collect_content_
  findings`, extracted from what was `scan_git_commit_gate`'s inline loop) with the commit gate rather
  than duplicating it — the only real difference between the two boundaries is *which* files count as
  "about to become permanent." A merge has no single "staged" set to read the way a commit does, so the
  target ref(s) are read straight from `argv` (every trailing token after `git merge` that doesn't start
  with `-`; a flag-only invocation like `git merge --continue` has no such token and is correctly left
  alone), the changed-file list comes from `git diff --name-only HEAD <ref>`, and each file's *incoming*
  content is read via `git show <ref>:<path>` — deliberately never the working tree, which an unmerged
  branch hasn't touched yet. Both gates are checked at the same `execute_shell` call site
  (`scan_git_commit_gate(...).or_else(|| scan_git_merge_gate(...))`). New test
  `git_merge_is_blocked_by_an_unresolved_secret_in_the_incoming_branch`: a real second branch with a real
  staged-then-committed secret, a real blocked merge (confirmed by the target file never landing on disk),
  then a real successful merge after `rapid findings dismiss`. Full `-p rapid` suite (302 lib tests) and
  `cargo build --workspace --tests` pass. Both named `PatchPolicyGate` boundaries (`VER-009`: "before
  commit/merge") are now real gates, not advisories — only the durable-evidence half of `VER-009` and a
  shell-string-wrapped invocation of either command remain open.
- **Durable-evidence half of `VER-009` implemented 2026-08-30, the same day — "results become evidence,
  not just a console warning."** New `record_gate_decision(root, boundary, blocked, findings)`: appends
  one JSON object per line to `.rapidlm/gate_log.jsonl` (`{schema, time, boundary: "commit"|"merge",
  blocked, findings}`) after *every* gate check that actually ran — a clean pass as much as a block,
  since "what was checked and why it passed" is as much evidence as "what was checked and why it failed."
  Deliberately minimal rather than a full ledger integration: no new `event-ledger` event kind, no
  `EvidenceRecord`/`EvidenceService` involvement (that system is goal-criterion-scoped, and conflating a
  general security-gate audit trail with goal-completion evidence would blur two genuinely different
  concepts) — just a durable, append-only, greppable/`jq`-able file, matching the same "a plain file is
  the honest tool for this" choice `FindingsStore` itself already made for dismissals. A write failure
  here never affects the gate's own decision — recording is advisory to the gate, not a second gate. New
  test `git_commit_gate_decisions_are_recorded_durably_blocked_and_clean_alike`: a real blocked commit, a
  real dismissal, a real clean commit, then both `.rapidlm/gate_log.jsonl` lines parsed and asserted on
  (`blocked: true` with a non-empty findings array for the first, `blocked: false` with an empty one for
  the second — confirming a dismissed finding doesn't silently resurface in its own evidence record
  either). Full `-p rapid` suite (303 lib tests) and `cargo build --workspace --tests` pass. **What
  remains of `VER-009`:** only a shell-string-wrapped `git commit`/`git merge` invocation, which
  `shell_exec`'s own "no shell string is ever interpreted" design makes structurally undetectable at this
  layer without a much bigger change to how commands are parsed.
- **Real gate bypass found and fixed 2026-08-30, more serious than the shell-string-wrapping limitation
  above: `background: true`/`sandbox: true` skipped the gate entirely, not merely a documented edge case.**
  `execute_shell` (`exec_tools.rs`) branches on `args.sandbox`/`args.background` *before* reaching the
  `scan_git_commit_gate`/`scan_git_merge_gate` check — both branches `return` early (starting a real
  background/sandboxed job via `self.jobs.start`) without ever routing through the gate, which only sat on
  the plain synchronous path below both branches. A model could bypass the entire secret-scanning
  pre-commit/pre-merge block simply by adding `"background": true` to the same `shell_exec` call —
  confirmed with a real reproduction: `{"argv":["git","commit","-m","..."],"background":true}` against a
  repo with a staged secret returned `Succeeded { summary: "started background job job-1: git commit ..." }`
  and the commit genuinely landed, where the identical call without `background` was correctly blocked.
  This is the same "validate after the side effect is already running" shape as the `resolve_in_root`
  symlink-escape fix earlier this session, just for a process-spawn side effect instead of a filesystem
  one. **Fixed:** moved the gate check to run once, unconditionally, immediately after parsing `args` and
  before any of the three branches (`sandbox`/`background`/plain) — the old check at the bottom of the
  plain path is now dead code given the new one already covers it and was removed rather than left
  redundant. New test `background_and_sandbox_shell_exec_cannot_bypass_the_git_commit_gate`: reproduced the
  bypass against the pre-fix code first (confirmed the exact `"started background job"` success and a real
  second commit landing), then confirmed the fix gates `background: true` identically to the plain path
  (blocked, and `git log` shows only the seed commit — the job never starts at all). Full `-p rapid` suite
  (325 lib tests) and `cargo build --workspace --tests` pass.
- **Correction (2026-08-30): the gap is narrower and more concrete than "shell-string-wrapped" alone —
  checked both gates' exact match condition directly.** `scan_git_commit_gate`/`scan_git_merge_gate`
  require `argv[1]` to be exactly `"commit"`/`"merge"`. A plain `git -C <path> commit ...` or
  `git -c key=value commit ...` (git's own global flags before the subcommand — not shell-string wrapping,
  a completely ordinary argv shape) has `argv[1]` be `"-C"`/`"-c"`, not `"commit"`, and skips the gate
  entirely. Considered and rejected two fixes rather than force one through: (a) a shell-string heuristic
  scanning `["sh"/"bash", "-c", "<script>"]` for `git commit`/`git merge` — rejected because a heuristic
  extraction subtly wrong about what the script actually runs would give *false confidence* that
  shell-wrapped commits are caught, worse than the honestly-documented gap; (b) a git-global-flag skip
  parser (walk past `-C <path>`, `-c <k>=<v>`, etc. to find the real subcommand) — rejected because git
  has enough flags that consume a following argument that getting the skip count wrong risks silently
  checking the wrong token as the subcommand, another false-confidence failure mode, for a case
  (`execute_shell` already runs every command with `current_dir(self.root())`, so `-C` is rarely needed in
  practice) unlikely to be common. Left as a precisely documented boundary rather than a fix with a hidden
  failure mode: the gate covers the ordinary, by-far-most-common `git commit`/`git merge` invocation shape
  a model would actually produce, not every syntactically valid one.
- **Why `ExternalFinding` specifically has stayed unattempted across every prior correction in this
  section, checked directly against its source 2026-08-30 rather than left as an unexplained gap.**
  `crates/security/src/scanners/external.rs` isn't a pure-content scanner like the other three
  (`security::scanners::secrets`/`CommandFinding`/`PatchFinding` all take bytes already in hand and need
  no external state) — it's a real "External SAST/SCA scanner adapter": it shells out to a *configured,
  externally-installed* scanner binary (Semgrep/Trivy-shaped, `ExternalScannerKind::{Sast,Sca,Container}`)
  inside a supervised sandbox, then normalizes that tool's SARIF 2.1.0 output into RapidLM findings.
  Wiring it into `apps/rapid` isn't the same shape as the other three at all: it needs a real, new
  configuration surface (which scanner binary/argv is configured, per project or globally — nothing in
  `.rapidlm/settings.json` today has a slot for this) and a policy decision about *when* it runs (unlike
  the always-on, free, in-process secrets/patch/command scans, invoking a real external process on every
  write would be slow and often simply `Unavailable` when no scanner is installed — this likely wants an
  explicit `rapid scan` entry point, not a hook on every tool call). This is new user-facing feature and
  config-surface work, not a wiring task the way `CommandFinding`/`PatchFinding` were — correctly left
  alone this whole section rather than a gap anyone missed.

### 2.10 Scoped Credential Broker + Resource Governor

Modbit: `WRK-016` (mint short-TTL, audience/run/workspace-scoped credentials for authorized tools/workers
— hosted provider API keys never leave the gateway), `WRK-017` (CPU/RAM/disk/network/token/cost/
concurrency ceilings — and explicitly: "budgets cannot convert failed verification into success").

**Correction (2026-08-29): the Credential Broker half is already built.** `crates/auth/src/broker.rs`'s
`SecretBroker` already does exactly this — opaque `ScopedSecret` handles (never plaintext), resolution
only at the executor/provider boundary via `SecretBroker::open`, one-use tokens that can't be replayed,
never written to the event ledger/traces/telemetry/logs. Not used anywhere in `apps/rapid` today (which
talks to `auth::InMemoryCredentialStore` directly for the single-provider-credential case it currently
has) — another wiring gap, not a missing feature, and lower priority than the others in this document
since `apps/rapid` doesn't yet have a scenario (MCP server secrets, multi-tenant credential sharing)
that actually needs the scoping `SecretBroker` provides. **The Resource Governor half was genuinely
absent, and still mostly is** — no CPU/RAM/disk/network/concurrency ceiling type exists anywhere in the
workspace (only `GoalBudget`'s narrower turn/token/time budget for one goal, `agent-runtime`, unrelated).
Building a full one is real, standalone systems work needing actual OS-level resource monitoring, not
attempted here. **Implemented 2026-08-29, the one ceiling that doesn't need OS-level monitoring:**
`rapid exec --max-wall-time <seconds>` (`apps/rapid/src/interactive.rs`, `spawn_wall_time_watchdog`) —
a background thread that cancels the turn's existing `CancellationToken` (the same cooperative signal
Ctrl-C already sends, checked by every model step and tool call) once the deadline passes. CPU/RAM/disk/
network ceilings remain real, separate future work needing actual OS-level monitoring.

**Correction (2026-08-30): the concurrency axis was already narrower than claimed, and is now fully
covered for this codebase's actual shape.** `apps/rapid/src/exec_tools.rs::MAX_BACKGROUND_JOBS` (16) was
already a real, pre-existing concurrency ceiling on live `shell_exec background: true` jobs — the "no
concurrency ceiling type exists anywhere" claim above was wrong for that one axis. The genuinely open gap
was `task_spawn`: unbounded, no cap on how many subagents one turn could start — a runaway or adversarial
loop could burn real tokens/cost/wall-time with nothing stopping it. **Implemented 2026-08-30:** a new
`MAX_SUBAGENT_SPAWNS_PER_TURN` (32) enforced via a `subagent_spawns: Arc<AtomicU64>` counter on
`WorkspaceTools`, checked before every `task_spawn` call; once exhausted, the call is a typed, handled,
model-visible failure (`"task_spawn budget exhausted"`), never a hard kill — the runner is never even
invoked past the cap. Named a *total-per-turn* cap rather than "concurrency" deliberately:
`execute_task_spawn` already runs synchronously (blocks until the child turn finishes before the tool
call returns), so there is no actual concurrent-subagent risk in this codebase's design to cap — only an
unbounded-sequential-total one, which is what WRK-017's spirit ("budgets can't buy a fake pass" aside)
is really protecting against here. CPU/RAM/disk/network ceilings are still real, separate, OS-level work.

**Correction (2026-08-30): the CPU/RAM claim was wrong too — real OS-level enforcement already existed
for one path, was silently misconfigured, and is now fixed, not just documented.** While auditing this
section, checked `crates/sandbox/src/backends/host_restricted.rs` (already wired into `apps/rapid` since
an earlier session via `sandbox_exec::run_sandboxed`, the non-macOS `shell_exec(sandbox: true)` path) and
found it already does genuine OS-level CPU (`ulimit -t` via `require_cpu_rlimit`) and memory
(`/proc`-based RSS sampling against `plan.memory_mb`, killing the process group on breach) enforcement —
this is real, not a stub. The bug: `apps/rapid/src/sandbox_exec.rs::build_spec` never set `cpu_millis`/
`memory_mb` on the `SandboxSpec` it built, so every sandboxed `shell_exec` call on that path silently ran
under `SandboxSpecBuilder`'s generic crate-wide defaults — `cpu_millis: 1_000` (**one CPU-second**) and
`memory_mb: 256` — values sized for nothing in particular, certainly not a general-purpose shell command.
**Verified empirically, not assumed:** a real `sh` loop doing ~200,000,000 iterations of trivial
arithmetic was killed by `SIGXCPU` after ~2.4 real seconds with **zero output** and no informative error
— `execute_shell` only ever reported "sandboxed no exit code (signalled)", identical to any other signal
death, giving no hint that a resource ceiling (not a crash, not a kill request) was the cause. This is a
real correctness bug for anyone actually relying on `shell_exec(sandbox: true)` on Linux/Windows: a
legitimate CPU-bound script (a build, a data-processing loop, anything nontrivial) would silently die with
no explanation, not just a "runaway process" edge case. **Fixed 2026-08-30:** `sandbox_exec.rs` now sets
explicit, intentional `SANDBOX_CPU_MILLIS = 30_000` (30 CPU-seconds) and `SANDBOX_MEMORY_MB = 1024`
(1 GiB) on every spec it builds, replacing the accidental generic defaults. Also surfaced the previously-
discarded `SandboxExit::signal()` through `SandboxRunOutcome` (new `signal: Option<i32>` field) and added
`exec_tools.rs::sandboxed_status_line()` (extracted, directly unit-tested) so a future CPU-limit kill
reports `"killed: sandbox CPU-time limit exceeded (SIGXCPU)"` by name instead of the same opaque
"no exit code (signalled)" for every signal death. New tests: `run_sandboxed_survives_a_moderately_cpu_
heavy_command` (a real, wall-clock-bounded ~3-second busy loop that would have died under the old 1-
second default and now completes) and `sandboxed_status_line_names_the_cpu_limit_specifically` (pure
unit test over the four status-line cases). Full `-p rapid` suite (299 lib tests) and
`cargo build --workspace --tests` pass. **What this does not close:** the macOS Seatbelt path (this
session's own new `SeatbeltBackend`, `crates/sandbox/src/backends/seatbelt.rs`) has no CPU/memory
enforcement at all yet — only timeout/cancel — a real, separate gap in that backend specifically, not
attempted here; and disk/network ceilings for the sandboxed exec path itself (distinct from the per-turn
disk/network budgets in §2.10's earlier paragraph, which cover `workspace_write`/`web_fetch`, not
sandboxed shell commands) remain unaddressed.
- **The OOM/pid-limit half of this same "named instead of generic" gap closed 2026-08-30 — found while
  auditing this section for other small, precisely-scoped follow-ups.** The CPU-limit naming fix above
  only handled the one axis that surfaces as a signal number (`SIGXCPU`). `HostRestrictedBackend`'s other
  two kill paths — `WaitOutcome::Oom` (the memory ceiling) and `WaitOutcome::PidsExceeded` (the process-
  count ceiling) — report with `SandboxExit::signal()` as `None`, not a signal number at all
  (`crates/sandbox/src/backends/host_restricted.rs:705-721`), so both fell through to the exact same
  generic `"no exit code (signalled)"` the CPU fix was written to eliminate — `SandboxExit` already had
  working, tested `oom()`/`policy_violation()` accessors for exactly this, `apps/rapid/src/sandbox_exec.rs
  ::run_sandboxed` just never read them into `SandboxRunOutcome`. **Fixed:** `SandboxRunOutcome` gained
  `oom: bool`/`policy_violation: bool` fields (populated from those two accessors); `exec_tools.rs::
  sandboxed_status_line` gained two more named cases — `"killed: sandbox memory limit exceeded (OOM)"`
  and `"killed: sandbox process-count limit exceeded"` — checked before falling through to the generic
  signal-number cases, mirroring the `SIGXCPU` arm exactly. New test `sandboxed_status_line_names_the_
  memory_and_process_count_limits_specifically`; the existing `sandboxed_status_line_names_the_cpu_limit_
  specifically` updated for the two new parameters. Full `-p rapid` suite (323 lib tests) and
  `cargo build --workspace --tests` pass. This is purely a diagnostics/reporting fix — the sandbox already
  enforced both ceilings correctly before this; a command that hit either one was already killed, it just
  reported an unhelpfully generic reason. `SeatbeltBackend`'s own missing RSS enforcement (next paragraph)
  remains the one real *enforcement* gap this pass didn't touch.
- **`SeatbeltBackend`'s own CPU gap closed too, 2026-08-30, same session as the backend itself.** Reused
  `host_restricted.rs`'s exact `sh -c 'ulimit -t "$1" || exit 125; shift; exec "$@"'` wrapper technique
  (duplicated rather than shared — a fixed three-line script, unlike the mount/path-validation logic this
  module already reuses from `host_restricted.rs`), applied to the whole `sandbox-exec` invocation:
  `exec` replaces the process image and the rlimit survives it, while `current_dir` (set on the same
  `Command`) is unaffected since `exec` never changes cwd. `SeatbeltPlan` gained a `cpu_millis` field
  captured from `spec.cpu_millis()` in `prepare`. New `cpu_ceiling_kills_a_command_that_exceeds_it_
  before_the_wall_clock_timeout` test, calibrated the same careful way as the `apps/rapid` fix's own test
  (see next paragraph) — a real, measured shell loop that reliably exceeds a 1-second CPU ceiling.
  Memory (RSS) monitoring remains unenforced for this backend — `host_restricted.rs`'s own memory
  enforcement needs a background sampling thread this backend's simpler `wait_child` loop doesn't have;
  not attempted here, a real, separate gap.
- **Memory (RSS) enforcement closed for this backend too, 2026-08-30.** Turned out to need no background
  sampling thread after all — `host_restricted.rs`'s own poll loop doesn't use one either; it just samples
  inline on the same 10ms cadence it already checks `try_wait`/the deadline on. Reused that exact idea,
  simplified for this backend's single-process model: `sh -c '...; exec sandbox-exec ...'` execs straight
  through to the final target program, so (unlike `host_restricted.rs`'s process-group tree) the whole
  chain shares one pid and there's no group to sum RSS across — `host_restricted.rs::pid_rss_kb` (made
  `pub(crate)` for this, same reuse-not-duplicate rationale as the mount/cwd helpers this module already
  shares) is exact here, not an approximation. `SeatbeltPlan` gained a `memory_mb` field from `spec.
  memory_mb()`; `wait_child`'s poll loop now checks `pid_rss_kb(pid)` against it alongside the existing
  cancel/deadline checks, killing and returning a new `WaitOutcome::Oom` (mapped to `SandboxExit::oom() ==
  true`) on breach. New test `memory_ceiling_kills_a_command_that_exceeds_it_before_the_wall_clock_timeout`:
  `/usr/bin/python3 -c "... bytearray(200 * 1024 * 1024) ..."` allocates and holds a real 200 MB buffer
  against a 64 MB ceiling — chosen deliberately over a `dd`/`yes`-style stream, which would never show up
  in RSS the way a held allocation does, the identical "measure the real thing" lesson the CPU test above
  already learned the hard way. **Verified the test isn't vacuous, the same discipline that correction
  demands:** temporarily raised the ceiling to 4096 MB and re-ran — the test correctly failed, running the
  full 30-second timeout with no kill, confirming the check is genuinely load-bearing before restoring the
  real 64 MB ceiling. Full `sandbox` crate suite (96 tests, up from 95) and `cargo build --workspace
  --tests` pass.
- **Correction, same day: the "matching `HostRestrictedBackend`'s coverage exactly" claim just above was
  wrong on two counts, caught while checking whether the other two backends (`container.rs`/`gvisor.rs`)
  had the same memory gap Seatbelt did — they didn't; both already had full CPU+memory+pid-count
  enforcement, confirming Seatbelt was genuinely the one outlier, not a symptom of a wider pattern.**
  First: the single-pid RSS check above only tracked the one pid the `sh -c '...; exec sandbox-exec ...'`
  chain hands back — a target program that forks its own children would have those children's memory go
  completely uncounted, unlike `HostRestrictedBackend`'s process-group-wide sampling. Second: Seatbelt still
  had no pid-*count* ceiling at all, a fourth dimension `host_restricted.rs`/`container.rs`/`gvisor.rs` all
  already enforce (`WaitOutcome::PidsExceeded`) that this backend simply never gained. **Fixed properly
  instead of leaving the overstated claim standing:** `run_seatbelt`'s spawned command now calls
  `isolate_process_group` (made `pub(crate)` on `host_restricted.rs`, reused rather than duplicated) —
  process-group membership survives every `exec` in the chain (the CPU-rlimit wrapper, then `sandbox-exec`'s
  own exec of the target) exactly the way the CPU rlimit itself does, so the target's own forked children
  land in the same group. `wait_child` now calls `host_restricted.rs::sample_process_group` (also made
  `pub(crate)`) instead of the single-pid `pid_rss_kb`, checking *both* memory and pid-count against
  `SeatbeltPlan`'s `memory_mb`/new `pids` field every poll, and uses the reused `terminate_process_group`
  (also `pub(crate)` now) for every termination path (timeout/cancel/oom/pids-exceeded) instead of a bare
  `child.kill()` — killing the whole group, not just the one tracked pid. New test `pid_count_ceiling_
  kills_a_command_that_forks_past_it`: the same reproduction shape as `host_restricted.rs`'s own
  `advertised_pids_bound_is_enforced` (a shell script backgrounds two `sleep` children against a ceiling of
  1), verified not vacuous the identical way (temporarily raised to 64, confirmed it then ran the full
  timeout with no kill, before restoring 1). Confirmed no regression to existing timeout/cancel semantics
  from switching to `terminate_process_group`'s graceful TERM-then-KILL sequence: all 13 pre-existing
  Seatbelt tests still pass, and the full 97-test `sandbox` suite still completes in ~2.4 seconds. Full
  `-p rapid` suite (326 lib tests) and `cargo build --workspace --tests` pass too. `SeatbeltBackend` now
  genuinely matches its three siblings on all three resource dimensions (CPU/memory/pid-count); only the
  still-separate execution paths in `apps/rapid` (job-based vs. this synchronous backend) remain unmerged,
  per this section's own earlier note.
- **Correction, same day: the first version of both new CPU tests was itself broken, and silently proved
  nothing — worth recording exactly why, since it's a real testing-methodology trap.** Both this fix's
  first regression test and the `SeatbeltBackend` CPU test originally used a *wall-clock-bounded* busy
  loop (`end=$(($(date +%s)+3)); while [ $(date +%s) -lt $end ]; do :; done`) chosen for "predictable
  duration regardless of shell-arithmetic throughput." That reasoning was wrong: `date +%s` forks a new
  subprocess every iteration, and `RLIMIT_CPU` only counts the CPU time of the *one process it's set on*
  — a parent shell that spends nearly all its wall-clock time blocked in `fork`/`wait` on child processes
  accumulates almost no CPU time of its own, so this loop could run for any number of real seconds
  without ever approaching even a 1-second CPU ceiling. Both tests "passed" under the buggy 1-second
  default too, meaning they verified nothing about the actual fix — caught only by deliberately re-running
  each test against the reverted (buggy) default and noticing it *still* passed, which should never happen
  for a real regression test. Also discovered mid-investigation: a naive large iteration count for the
  *positive* case (200,000,000, guessed from how long it took a `ulimit -t 1`-killed run to receive
  `SIGXCPU`, which is not the same as how long the loop takes to actually finish) turned out to need
  **over 120 real seconds** to complete — confirmed by directly timing `/bin/sh -c '...'` outside any
  sandbox at all. Fixed by measuring real throughput directly (`time /bin/sh -c 'i=0; while [ $i -lt N ];
  do i=$((i+1)); done'` at a few values of `N`) and picking `N = 1,500,000` (~4 real/CPU seconds,
  confirmed by direct timing), a pure shell-builtin loop with no subprocess forking. Re-verified both
  fixed tests fail under the reverted 1-second default and pass under the real one before trusting them.
  Lesson for future sandbox/rlimit tests in this codebase: never trust a resource-ceiling test that
  hasn't been run against a deliberately-broken version of the fix it claims to verify.

**Disk axis, 2026-08-30: a real per-turn ceiling landed without needing any OS-level monitoring —
the same "count what's already flowing through a chokepoint" trick as the concurrency fix above,
not the OS-level work the "still real, separate" note above assumed disk required.** Every
`workspace_write`/`workspace_patch` call already bounds its own content size
(`MAX_WRITE_BYTES` = 64 KB), but nothing bounded the *count* of calls — a runaway loop writing
max-size files repeatedly could consume unbounded disk with no single call ever exceeding its own cap,
exactly the same shape as the `task_spawn` gap already fixed. **Implemented:** `MAX_TOTAL_WRITE_BYTES_PER_TURN`
(64 MB, 1024x the per-call cap — generous enough for any real coding task, tight enough to stop a
genuinely pathological loop) enforced via a `bytes_written: Arc<AtomicU64>` counter on `WorkspaceTools`
and a `reserve_write_budget()` helper (atomic reserve-then-rollback-if-over, safe against two concurrent
near-the-limit writes on different paths racing each other — same-path writes already serialize via
`write_group_key`), checked in both `execute_write` (all three of its write points: the two shadow-
diagnostics branches and the plain path) and both of `execute_patch`'s write points before the bytes
ever reach `fs::write`, so a refused write never touches disk. CPU/RAM/network ceilings still remain
genuine OS-level work — this is disk specifically, and specifically the "unbounded call count" shape,
not a byte-accurate disk-usage monitor (a single `fs::write` beyond the file's own existing size isn't
separately accounted for, e.g. overwriting a large file with a similarly large one; the ceiling is on
cumulative *written* bytes this turn, not net disk delta).
- **Explicit scope note, 2026-08-30, added after checking whether this axis' "checked in `execute_write`/
  `execute_patch`" language could be misread as covering more than it does.** `shell_exec` can write
  arbitrary bytes to disk inside the workspace via ordinary shell commands (`dd`, `curl -o`, redirection,
  a heredoc) — none of `execute_shell`'s three branches (plain, background, sandboxed) call `reserve_
  write_budget`, so a shell-driven write is completely outside `MAX_TOTAL_WRITE_BYTES_PER_TURN`'s
  accounting. Confirmed this isn't the same "settled, tested, documented exclusion" shape as `write_scope`
  deliberately not covering `shell_exec` (that one has both an explanatory comment and a dedicated test;
  this one has neither anywhere near `execute_shell` or `reserve_write_budget`). It also isn't a small
  wiring gap the way the `git commit`/`git merge` gate and the team-memory patch gate were, both fixed
  earlier this session: those were the *same* check, trivially applicable to a sibling code path that was
  just never wired to it. This one is structurally different — `reserve_write_budget` pre-reserves a
  *known* byte count before a structured `workspace_write`/`workspace_patch` call runs; a shell command's
  eventual disk footprint isn't known until (if ever) it finishes, so pre-reservation doesn't apply the
  same way, and accounting for it after the fact would need real OS-level disk-usage monitoring of the
  child process — the same category of work this section already calls out as an open, separate gap for
  CPU and RAM, just not previously named for disk specifically. Not attempted here; recorded as a precise
  scope boundary on the "disk axis... implemented" claim above rather than left to be misread as
  comprehensive.

**Network axis, 2026-08-30: same shape, applied to `web_fetch`.** `MAX_TOTAL_FETCH_BYTES_PER_TURN`
(16 MB) enforced via a `fetch_bytes: Arc<AtomicU64>` counter and `reserve_fetch_budget()` (identical
reserve-then-rollback shape to the disk one), reserved against each call's own requested `max_bytes`
*before* the network round trip — a conservative worst-case, since the actual response size isn't known
until after the request. **CPU and RAM remain the two axes with no shortcut available**: unlike
concurrency/disk/network, there's no existing per-call counter or size argument to chokepoint against —
bounding either genuinely needs real OS-level resource monitoring (rlimits, cgroups, or platform-specific
APIs), which is real, separate, and was correctly identified as the hard part of this item from the start.

**Correction, 2026-08-30: the disk/network ceilings just above bounded one tool *instance*, not the
turn — found while auditing §2.2's `AGT-010` nested-delegation fix for the same "fresh counter per
child" shape and checking whether it also applied here.** It did: `WorkspaceTools::open_with_permissions`
builds a brand-new `Arc::new(AtomicU64::new(0))` for `bytes_written`/`fetch_bytes` every time it's
called — including every subagent child `LiveSubagentRunner::run` constructs. Since a turn can spawn up
to `MAX_SUBAGENT_SPAWNS_PER_TURN` (32) subagents (each now capped at depth 1, so this is a *bounded*
multiplier, not the unbounded shape the nested-delegation bug had), the real aggregate ceiling for one
turn was `(1 + 32) × 64 MB` disk and `(1 + 32) × 16 MB` network — 33x either constant's name, not the ~1x
"per turn" implies. **Fixed:** new `WorkspaceTools::turn_budget_handles()`/`share_turn_budgets()` — the
parent clones its own `bytes_written`/`fetch_bytes` `Arc`s and hands them to `LiveSubagentRunner`, which
now calls `share_turn_budgets` on every child's tools (right alongside `disable_nested_spawn`) instead of
letting `open_with_permissions` hand it a fresh pair. `subagent_spawns` itself needed no equivalent fix:
now that nested spawn is disabled by default, a child can never reach `execute_task_spawn` at all, so
its own fresh (and now unreachable) counter is moot. New test
`subagent_children_share_the_parents_per_turn_disk_budget`: a parent pre-loaded near the disk ceiling, a
child sharing its handles refused for hitting the *shared* budget, and a genuinely separate unshared
child's own write succeeding normally — confirming the refusal came from sharing, not from some other
cause. Full `-p rapid` suite (305 lib tests) and `cargo build --workspace --tests` pass.

- **Where it lands:** Credential Broker: wire `SecretBroker` into `apps/rapid`'s credential path once a
  real multi-secret scenario exists — premature before that. Resource Governor: new work, `sandbox` or a
  new small crate; encode its "budgets can't buy a fake pass" anti-pattern into whatever implements
  `CompletionContract` in §2.4.
- **Sev/Effort:** P2 / M (broker wiring) + L (governor, new feature).
- **New finding, 2026-08-30: a companion to the Credential Broker gap above — `capability-broker`'s own
  audit-trail subsystem is fully built and tested but has zero callers anywhere, including from the one
  real lease-minting call site this codebase now has.** `crates/capability-broker/src/audit.rs`
  (`audit_decision`, `audit_approval_requested`/`resolved`/`expired`, `audit_lease_issued`/`used`/
  `expired`/`rejected`, plus `MemoryAuditStore`/`LedgerAuditStore`) is real, tested, durable-record
  machinery for exactly `WRK-016`'s own intent ("mint short-TTL, audience/run/workspace-scoped
  credentials... "), but grep confirms none of it is called from anywhere outside its own crate. Per this
  section's own 2026-08-30 entry above, `apps/rapid/src/sandbox_exec.rs::run_sandboxed` is "the first
  production code path in the repo to mint a real `CapabilityLease`" (the full `evaluate` → `request_
  approval` → `resolve` → `issue` ceremony) — and that call site never calls any `audit_*` function, so
  the one real capability decision happening in production today leaves no audit record despite the
  recording machinery being ready. **Checked why this isn't a small wiring fix before flagging it as
  one:** every `audit_*` function takes a `store: &impl CapabilityAuditStore`, and `CapabilityAuditRecord`
  requires a real `session_id: SessionId` (`AuditContext` carries `agent_id`/`trace_id` too) — `sandbox_
  exec.rs`'s call site has none of these readily available (it's a one-shot tool-execution path, not
  session-scoped), and the only store that would make the audit trail durably meaningful,
  `LedgerAuditStore`, needs a real ledger to write to that this call site has no access to (the kernel's
  session ledger is a separate subsystem entirely, established elsewhere in this document). Wiring this in
  for real needs the same kind of session/store plumbing decision this section's Credential Broker note
  already correctly deferred as "premature before a real scenario exists" — not attempted here for the
  same reason, but named precisely rather than left as an unexplained dormant module.

---

## Phase 3 — Bets that put RapidLM ahead of both, not just even

These are gated on Phase 2 landing first — each one is unsafe or hollow without the primitive it depends
on, noted inline.

### 3.1 Governed competitive multi-model execution (RapidLM's answer to Agent Arena)

Qwen Code's Agent Arena (2–5 models racing in isolated `git worktree`s, automated merge-back of the
winner) has no equivalent in either Rust tool and is a genuinely useful idea — but Qwen's own
implementation applies the winning diff via `git apply` with no typed transaction, conflict record, or
rollback evidence. Modbit's `CHG-012` `MergeTransaction` (§2.1) is the harder version of the same
primitive. **Once §2.1 lands**, RapidLM can offer the same competitive-execution UX with an actually
auditable, reversible merge — something neither existing implementation has. Bound it per Modbit
`REJ-007` (no unbounded parallel agents — competitive execution is earned by explicit task separability
and a fixed, small agent count, not a knob users crank up).

- **Sev/Effort:** P2 / L. **Do not start before §2.1 and §2.2 both land.**

### 3.2 Governed two-tier background automation

`gaps.md`'s own translated-findings section (§20, finding #4) already flagged this as a real gap: RapidLM's
`rapid cron` runs durable background prompts, but nothing distinguishes a "propose, never auto-apply"
tier from full execution. Modbit's `AGT-008` (agent parking — a parent interruption parks child work
rather than cancelling it) and `CAP-008` (§2.2 above) are the supporting primitives. Combine with the
existing `security::scanners::secrets::Finding` pattern (§2.9) to give background automation a native
"reviewable suggestion" output shape instead of inventing a new one per feature.

- **Sev/Effort:** P2 / M. Depends on §2.2.
- **Correction (2026-08-30): the premise overstates what `rapid cron` actually does — it's not "runs
  durable background prompts" yet, only "durably schedules them."** Traced `rapid cron poll`
  (`p9_commands.rs::run_cron`, `"poll"` arm) end to end: it calls `scheduler::PromptCron::poll()`, which
  does real claim-lease-firing job-lifecycle bookkeeping (due-time tracking, requeue, quarantine after
  repeated failures — all genuinely implemented, tested at the `scheduler` crate level), and the
  fired jobs are just **printed** (`id=... session=... prompt=...`) — confirmed via grep that neither
  `p9_commands.rs` nor `crates/scheduler` contains a single reference to `run_live_exec`,
  `AgentExecutionRequest`, or any other real turn-execution entry point. **A fired cron job never
  actually runs its prompt through a model or a tool call at all** — the whole background-automation
  surface this item wants to add a "propose, don't auto-apply" tier *to* doesn't exist as an executing
  system yet; there's nothing behind "poll" but a scheduler and a print statement. This is a bigger,
  more foundational gap than the tiering distinction §3.2 itself asks for, and it changes the shape of
  the real task: building execution-on-fire at all, with a deliberately read-only/propose-only mode as
  the *first* mode it supports (never a mode added after a full-execution one already shipped), rather
  than adding a tier to something that runs. Deliberately not attempted in this pass — wiring real turn
  execution into a background/unattended path carries genuine safety weight (getting a "propose-only,
  never applies" guarantee subtly wrong here is a very different risk than a CLI flag defaulting wrong)
  and deserves dedicated design attention, not a rushed pass alongside unrelated work.
- **Execution-on-fire implemented 2026-08-30, exactly the "propose-only as the first mode" shape the
  correction above called for — riding on an already-proven safety primitive rather than inventing a new
  one.** `evaluate()` (`apps/rapid/src/permissions.rs`) already denies every non-read-only tool call
  unconditionally in `PermissionMode::Plan`, before the mode table is even consulted for anything else,
  and read-only calls stay allowed in every mode — a real, independently-tested "can explore, can never
  mutate" guarantee that already existed for the interactive `plan` mode. Reusing it (rather than building
  a parallel "propose-only" concept from scratch) is what makes this safe to ship in one pass: the hard
  safety property comes from code this session did not need to write or newly trust. **Implemented:**
  `exec_turn` (`interactive.rs`) gained a `forced_mode: Option<PermissionMode>` parameter that
  unconditionally overrides env/project-settings/Claude-compat mode resolution (composes safely with the
  managed-policy ceiling regardless of order, since `Plan` is already the least permissive of all six
  modes — gating it can only ever be a no-op); `rapid cron poll`'s fired-job loop (`p9_commands.rs`) now
  calls `exec_turn(&[due.prompt], Some(PermissionMode::Plan))` for each due job instead of only printing
  it, reporting `outcome=exit:<code>` or `outcome=error:<detail>` per job. **Deliberately not attempted,
  disclosed rather than silently gapped:** no failure-count integration with `PromptCron::quarantine` —
  `poll()`'s own quarantine logic already covers unparseable schedules; wiring repeated *execution*
  failures into it needs a real policy decision (how many consecutive failures, quarantine vs. just
  keep retrying) this pass didn't make, so a job that fails every time will keep firing and failing
  forever rather than being caught by quarantine, a known, named limitation, not silently
  auto-handled; session continuity (a cron job's `session_id` is still just an opaque string, never
  threaded into real session/ledger state) also remains out of scope, unchanged from before. **Testing
  note, disclosed for the same reason:** `rapid cron`'s schedule grammar has a one-minute floor (rejects
  sub-minute schedules), so a fast, deterministic, real-binary end-to-end test isn't possible without
  either waiting up to 60 real seconds or widening internal visibility purely for testability — neither
  worth it for one test. New `binary_cron_poll_runs_the_fired_job_in_plan_mode_and_denies_the_patch`
  (`configured_model_integration.rs`) is a genuine, real, non-mocked-safety-property end-to-end test —
  loopback model server, real `rapid cron add`/`rapid cron poll` subprocess invocations, `RAPIDLM_
  PERMISSION_MODE=acceptEdits` deliberately set in the environment to prove the forced mode overrides an
  *explicit* permissive setting, not just an absent one — but marked `#[ignore]` (the first and only use
  of that attribute in this codebase) with a doc comment explaining exactly why, runnable on demand via
  `cargo test -- --ignored`, and **actually run once during this pass, confirmed passing** (fired=1,
  outcome=exit:0, file unmodified, ≥2 requests reached the loopback server) before being committed — not
  merely written and trusted to compile. A second, fast unit test,
  `forced_mode_overrides_env_and_settings_resolution` (`interactive.rs`), covers the parameter-threading
  behavior on every normal `cargo test` run by requesting two different forced modes and confirming both
  distinctly come back out (proving the parameter, not some fixed ambient default, decides the outcome).
  Full `-p rapid` suite (322 lib tests), `cargo test -p rapid --tests` (the new binary-level test correctly
  sits at 1 ignored, not slowing the normal suite), and `cargo build --workspace --tests` all pass.

### 3.3 Verified-success-per-token as a tracked, reported metric

RapidLM's own product thesis (`00-README.md`: "improve verified task success per token") is already,
independently, the same idea as Modbit's `CTX-002` Context Economy Engine ("optimize task-relevant
information per model token and verified outcome... do not sacrifice correctness for compression"). Right
now this is a stated goal with no instrumentation. Once §2.4 (tri-state completion) and §2.8 (real cost/
token accounting) both land, this becomes a computable number RapidLM can actually report per run — turning
a slogan already in the README into a real, differentiating metric neither Grok Build nor Qwen Code
publishes.

- **Sev/Effort:** P2 / S once §2.4 and §2.8 land.
- **Correction (2026-08-30): §2.4 and §2.8 have now both landed, but this item's own "then it's just S"
  claim turns out to rest on a third, previously-undocumented precondition that hasn't — checked while
  scoping this as the next tractable pick.** §2.8's cost/token accounting (`ExecOutcome.tokens`/
  `cost_usd_micros`, `apps/rapid/src/host.rs`) and §2.4's verified-completion gate
  (`CriterionEvaluator`/`GoalHost::can_complete`) are both real, but they live on two objects that never
  meet: `ExecOutcome` is per-`rapid exec`-invocation and forgotten once that process exits, while
  "verified" (a goal's evidence gate passing) is a property of a *goal*, not a single exec call — and a
  goal's own usage tracking is a fourth, separate, still-entirely-dormant subsystem. `crates/agent-runtime/
  src/goal/state.rs::GoalUsage` (`turns`/`tokens`/`active_ms`/`cost` — exactly the shape this metric
  needs) and the full `GoalBudgetGuard`/`GoalDriver` machinery that accrues and enforces it
  (`crates/agent-runtime/src/goal/driver.rs::GoalDriver::next`, `crates/agent-runtime/src/goal/budget.rs`,
  both with real, passing tests — e.g. `active_usage_accrues_and_writes_back`) have **zero call sites
  anywhere outside `agent-runtime` itself** (confirmed by grep across `apps/rapid` and every other crate) —
  the same "mature, tested, fully unwired" shape this document keeps finding elsewhere (context-engine,
  `sandbox`, now `agent-runtime`'s own goal driver). `apps/rapid/src/goal_host.rs` never references
  `GoalUsage`/`GoalDriver` at all: a goal's `usage` field stays `GoalUsage::default()` (all zeros) for its
  entire life regardless of how many real tokens/dollars `rapid exec` actually spends against it, and
  `GoalBudgetGuard`'s ceiling enforcement (the actual mechanism behind `GoalCommand::Block { budget_exhausted:
  true }`) is consequently never exercised by anything real either — a goal today can never budget-exhaust
  in production, only in `agent-runtime`'s own unit tests. **This means the metric's real blocker isn't
  §2.4/§2.8 (both done) — it's wiring per-turn usage into the active goal at all**, and `GoalDriver::next`'s
  shape (generic over the model/tool driver, i.e. designed as an orchestration loop of its own) suggests
  that isn't a small "call this after your own turn" addition: it would mean either routing `apps/rapid`'s
  existing turn loop (`interactive.rs`/`host.rs`'s fallback-chain/retry/stall-detection machinery) through
  `GoalDriver` instead, or building a narrower usage-only update path alongside it — a real design decision,
  not confirmed or attempted here. Re-scoping this item's effort to M (goal-usage wiring) → S (the metric
  itself, once usage is real) rather than the S this row currently claims.
- **Checked the "narrower usage-only update path" half of that fork directly, 2026-08-30, rather than
  leaving both options equally open — one of them is a real trap.** The obvious version of "narrower path":
  skip `GoalCommand`/`GoalStateMachine::apply` entirely and just call `GoalStateMachine::from_snapshot
  (snapshot.with_usage(new_usage))` directly from `apps/rapid`'s `GoalHost` after each turn — no new
  command variant, minimal code. This is unsafe to ship as-is: every other `GoalHost` mutation either goes
  through `apply(GoalCommand, actor, cancel)` (a real lifecycle transition, ledger-recorded) or
  `record_evidence` (a genuinely separate, dedicated store, `EvidenceService`, not a `GoalSnapshot` field
  swap). `from_snapshot` has no such precedent for *incremental* mutation of an already-active goal — it
  exists to load a persisted snapshot wholesale, not to patch one field on a live one. Swapping the
  `machine` field's snapshot on every turn would silently skip whatever ledger-event recording
  `apply`'s callers rely on for reconstructability, an architectural property (event-sourcing) this pass
  didn't have enough context on `agent-runtime::goal`'s actual guarantees to safely bypass. The
  architecturally consistent version — a new `GoalCommand::RecordUsage { .. }` variant handled in
  `apply()`'s match, so a usage bump *is* a real, ledger-recorded lifecycle event like every other mutation
  — is real, additional design/implementation work on `agent-runtime::goal::state`'s core enum, not
  attempted here. Both halves of the original fork remain open; this narrows *which* narrower path is
  actually safe, rather than leaving "just mutate the snapshot" looking like the easy option it initially
  appears to be.
- **Checked whether `GoalCommand::RecordUsage` — the "architecturally consistent" path the note above
  left open — is actually consistent, 2026-08-30, before starting to implement it. It isn't, and a third
  option doesn't obviously fit either.** `GoalCommand`'s own doc comment (`crates/agent-runtime/src/goal/
  state.rs:113`) states the contract directly: "Create/replace/pause/resume/block/complete/cancel. Field
  edits are not a lifecycle edge; a new contract is atomically substituted via `Replace`." A usage bump is
  exactly a field edit, not a lifecycle transition (the goal's `state` doesn't change) — adding `RecordUsage`
  would be the first variant in this enum that violates its own documented rule, not a natural extension
  of it. It also isn't contained: `GoalCommandKind` mirrors `GoalCommand` 1:1 (used for typed error
  reporting, `InvalidTransition { command: GoalCommandKind }`), and `GoalEffect.event: GoalEventKind` is a
  third enum in the same family — a new command needs a new entry in both, plus whatever ledger-event
  replay/projection logic reconstructs a `GoalSnapshot` from persisted history (not traced fully this
  pass), so this is a ripple through a small family of tightly-coupled wire-relevant enums, not a
  contained one-file addition. **Considered a third option** — model usage accrual as a separate
  host-level store the way `record_evidence` does (`apps/rapid/src/goal_host.rs::GoalHost::record_evidence`
  writes into `self.evidence: EvidenceService`, entirely outside `GoalCommand`/`apply()`/the ledger-recorded
  lifecycle machine). This doesn't obviously fit either: `EvidenceService` holds structured, potentially-
  numerous records where "a separate store" is a natural shape, while `GoalUsage` is already, by design, a
  plain field *on* `GoalSnapshot` itself (`turns`/`tokens`/`active_ms`/`cost`, defaulting to zero) — moving
  it to a side-store would mean `GoalSnapshot.usage` permanently reads zero while the real numbers live
  somewhere else, an odd split for a field that's supposed to round-trip through `save`/`load`/`export`
  alongside everything else on the same snapshot. **No safe path was found and none was attempted.** All
  three options this document has now considered (direct snapshot mutation, a new lifecycle command, a
  side-store) have a real, specific problem; the actual fix needs someone with full context on
  `agent-runtime::goal`'s event-sourcing guarantees to decide the right shape, not a guess made while
  scoping an unrelated metric this document was trying to close out. `GoalDriver`/`GoalUsage` wiring stays
  the genuinely open blocker for this item and for §3.3, unchanged from the prior correction.

---

## Explicit guardrails — do not adopt these, from either competitor or from Modbit's own rejected list

- **No fail-open completion verification** (Modbit `REJ-006`). A verifier error, timeout, or invalid
  output is `INDETERMINATE`, never a silent pass. This directly shapes §2.4.
- **No unbounded parallel/recursive agents** (Modbit `REJ-007`, `AGT-010`). Depth and fan-out are explicit,
  bounded, off-by-default settings — not a dial users are encouraged to max out. This shapes §2.2 and §3.1.
- **Don't make an LLM-judged auto-approval classifier the factory default.** Qwen Code ships `auto` as its
  out-of-box posture; Modbit's own design (`CAP-009`) explicitly keeps a similar classifier subordinate to
  deterministic policy as a fallback, never the default gate. RapidLM's default should stay `Default`
  (ask), with the classifier idea (if built at all) arriving strictly under §2.3 as an optional layer
  beneath the existing deterministic rules — never replacing them.
- **No scope creep into a general IDE/desktop product.** Most of Modbit's `IDE-*`, `WEB-*` (beyond
  existing computer-use ambitions already in `00-README.md`), and backend-control-plane items (`DAT-*`,
  `IDN-*`, `CTL-*`) describe a different product shape (an Electron/Code-OSS fork with a hosted control
  plane) and are out of scope for a CLI/TUI/daemon tool. They're excluded from this document on purpose,
  not by oversight.
- **No local SLM dependency of any kind** (Modbit `SLM-001`–`SLM-003`, cancelled in Modbit's own roadmap
  for good reason — it doesn't survive contact with real provider-neutral routing). RapidLM's
  provider-neutral `llm-router` is already the right shape; don't regress it by baking in a local-model
  special case.

---

## Follow-up-task diagnostic notes

**`computer-use` fixtures hang (spawned task `task_81037fda`):** ran all 8 tests individually with
`--test-threads=1` (all pass, ~0.00–0.01s each), then all 8 together with default parallelism via
`cargo test -p computer-use --test fixtures --quiet` (also passes, 0.01s total) — reproduced cleanly
twice. This rules out a deterministic concurrency bug in the fixtures file itself with reasonable
confidence: every individual test is fast and correct, and the whole file passes together in isolation.
The original hang only manifested inside a full `cargo test --workspace` run, which spawns many test
*binaries* concurrently across every crate on a machine already under heavy, sustained background CPU
load (several long-running, unrelated processes pinning multiple cores for days — see the note in this
document's own commit history about compile times). The likely cause is resource contention/exhaustion
under that combined load, not a bug in this specific test file. Whoever picks up that task should
prioritize reproducing it via a full `cargo test --workspace` run (accepting the 10-20+ minute cost) over
further scrutiny of `fixtures.rs` in isolation, and consider whether the fix belongs in test
infrastructure (bounded parallelism, e.g. `cargo test --workspace -- --test-threads=N`) rather than in
`computer-use`'s own code.

**Confirmation, 2026-08-30: a full `cargo test --workspace` run (accepting the ~minutes-long cost, exactly
as recommended above) completed cleanly — no hang, no failures, anywhere.** 73 test binaries (lib +
integration + doc-tests across every crate), every one `test result: ok`, zero `FAILED`/panicked lines in
the full log. `computer-use`'s own `fixtures.rs` ran twice in this one workspace run (it's exercised by
two separate test binaries) — both times all 8 (then 6) tests passed with no hang, consistent with the
resource-contention hypothesis above rather than a real bug: on a machine not under that same sustained
background load, the full suite — including the specific file that hung before — runs clean. Not
conclusive proof the hang can never recur under contention, but a real, full-cost reproduction attempt
that found nothing wrong, which is itself the evidence this note asked the next person to go gather.

## Source ledger

- This session's three-way parity research (RapidLM vs. Grok Build vs. Qwen Code), published as an
  artifact this conversation, and the fresh Qwen Code source audit behind it (clone at `265e7f1`).
- `gaps.md` (this repo) — source-grounded Grok Build audit at `9684fa3`, cross-checked against current
  RapidLM source rather than trusted at face value (see §0).
- `docs/benchmarks/2026-08-28-qwen-code-vs-rapid.md`, `docs/research/feature-inspiration-matrix.md`,
  `docs/v2-archive/research/competitive-analysis.md` — prior internal research, lower resolution than the
  above two but consistent with them.
- `/Users/mohsin/useful /Modbit_Feature_Inspiration_Provenance_2026-08-18_v2.md` — Modbit's own
  feature-by-feature provenance matrix (~280 IDs, each tagged LOCKED / PROVISIONAL / EXPERIMENT /
  REJECTED / DEFERRED against a named inspiration source). This document mines the CLI/kernel-relevant
  subset only; the wider Modbit dossier (`Modbit_Goal_Runtime_V2_COMPLETE.md`,
  `Modbit-Consolidated-Architecture-Decision-Record-2026-08-08(2).md`, `Modbit_Lite_TASKS_v5.md`, and the
  `/Users/mohsin/modbit` source tree itself) was not read for this pass and may contain further
  implementation-level detail worth a follow-up mining pass if any Phase 2 item above needs more precision
  than its Modbit ID alone provides.
