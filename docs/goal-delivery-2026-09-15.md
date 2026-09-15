# Goal delivery — evaluation, agent runtime, product completeness (2026-09-15)

Baseline: `docs/codebase-assessment-2026-09-15.md` (checkout `7816caa`).
Delivery commits, in order: `b6d1c02`, `7978614`, `bffedf2`, `dee578f`, plus
the validation/docs commit carrying this file.

Grading version for all new evaluation evidence: **2** (`GRADER_VERSION = "2"`).

## 1. Acceptance checklist (criterion → evidence)

Every claim below links a commit plus a concrete artifact (test name, results
file, or command output retained in-repo).

### 1.1 Evaluation trustworthiness

| Criterion | Status | Evidence |
|---|---|---|
| Authoritative verification outside the agent-editable workspace | done | Task data (`verify`, `protected`, `mutants`) lives in `eval/suite*/`, executed by the harness process, never materialized into the scratch. Tests: `eval_serve::tests::grading_positive_control_gold_submission_passes`, `load_suite_parses_protected_and_mutants`. Commit `b6d1c02`. |
| Prohibited modifications detected | done | Protected-file integrity (byte-identical, deletion counts). Tests: `tampering_with_a_protected_test_file_is_rejected`, `hard_coding_a_protected_runner_is_rejected`. |
| Test-writing tasks execute meaningful assertions | done | Per-function mutation matrix: empty test files, assertion-free scripts, weakened assertion sets all rejected. Tests: `empty_test_file_is_rejected_by_mutation_check`, `hard_coded_assertion_free_output_is_rejected`, `weakened_assertions_are_rejected`. |
| Mutation checks prove tests detect defects | done | `mutants` applied alone; submission must fail each. Gold kills all mutants — proven at generation (`eval/gen-suite.py`, `eval/gen-repo-suite.py` self-checks) and end-to-end (offline runs below). |
| Workflow structure validated, not just compilation | done | `workflow-*` verify = `playbook-compile` + inline structural check (exactly `build` task + `check` verification depending on build with a command). |
| Negative controls: invalid submissions rejected | done | The six tests above plus `timeouts_and_agent_failures_are_failures_never_skips`. |
| Pre-change failure check retained; broken environment distinguished | done | `prechange_check` + `verify_unrunnable` (sh 126/127 → `infrastructure`). Test: `vacuous_tasks_and_broken_environments_are_distinguished`. |
| Correct solutions pass | done | Offline gold runs: representative 12/12, held-out 4/4, smoke 40/40 (`eval/results/offline-1789458191.json`, `-8194.json`, `-7005.json`). |

### 1.2 Subprocess handling

| Criterion | Status | Evidence |
|---|---|---|
| Concurrent stdout/stderr drain for the whole lifetime | done | `run_supervised` per-pipe reader threads. Tests: `verbose_output_cannot_manufacture_a_timeout` (4 MiB to each stream), `stdin_is_delivered_without_blocking_the_runner`. |
| Bounded retention while draining excess | done | `BoundedTail` (tail retention + drop counters). Test: `retained_output_is_bounded_and_keeps_the_tail`. |
| Timeout/cancel kills and reaps the whole process group | done | `process_signal::isolate_process_group` + `terminate_process_group` (the proven supervisor/sandbox primitive). Test: `timeout_kills_the_whole_process_group` (grandchild survivor check via pgrep). |
| Bounded diagnostics, exit reasons, partial usage, failure artifacts | done | Usage collected on every exit path incl. timeouts (`run_live_agent`); typed `RunFailure::kind()` per attempt in results JSON. |
| Proven production primitives reused | done | `process-signal` crate shared with supervisor/sandbox/plugin-host (`apps/rapid/Cargo.toml` dependency note). |
| Applied to preflight, live execution, verification, helpers | done | `run_verify`, `run_live_agent`, `probe_agent_health` all use `run_supervised`; generator self-checks cover the offline helper path. |

### 1.3 Accounting and classification

| Criterion | Status | Evidence |
|---|---|---|
| Total measured usage across ALL attempts / verified successes | done | `agent_usage_metrics`. Test: `accounting_counts_failed_attempts_in_the_spend_denominator_is_successes`. |
| Successful-attempt averages reported separately | done | `successful_attempts_only_average` field + test. |
| Missing usage unknown; coverage + lower bound exposed | done | `attempts_missing_usage`, `complete_coverage`, `lower_bound`. Tests: `accounting_with_no_measurements_reports_unknown_not_zero`, lower-bound assertions. |
| Estimated cost separate from measured | done | Separate blocks + test: `accounting_keeps_measured_cost_and_estimate_separate_and_includes_skipped_arms`. |
| Verified success from grading, never `turn.completed` | done | Eval: pass verdicts only from the grading pipeline. Harness: `MetricCollector::VerifiedSuccessPerToken` now takes assertion verdicts; test `metrics_include_verified_success_per_token_and_approval_rate` (turn.completed alone earns nothing). |
| Typed infrastructure errors + dedicated health probe | done | `InfraError` enum + `probe_agent_health` (trivial no-tools turn). Live run evidence: rapid arm typed as credential-blocked, NOT misclassified (`eval/results/live-1789458229.json`). |
| Every requested competitor in results | done | `plan_arms` includes absent/mismatched arms; `skip_all` records typed skips. Test: `every_requested_arm_is_planned_even_when_absent`; live run shows all four arms. |
| Empty/all-skipped/incomplete runs cannot pass the gate | done | `eval_exit_code`. Test: `exit_gate_fails_empty_failed_or_skipped_runs`. |

### 1.4 Representative evaluation program

| Criterion | Status | Evidence |
|---|---|---|
| Independent tasks beyond smoke | done | `eval/suite/`: 12 tasks — navigate (3), refactor (4, multi-file gold), context (1, contracts across 10 files), errors (2), integration (2). Generators: `eval/gen-repo-suite.py`. |
| Smoke retained, separately labelled | done | `eval/suite-smoke/` (40 tasks); labelled in `EVAL_USAGE`, `eval/README.md`, report `kind` field. |
| Held-out tasks not used for tuning | done | `eval/suite-heldout/` (4 tasks, `heldout-*`). |
| Repeated trials + variation | done | `--trials <n>`; per-task pass counts, flaky/never-passed lists. Test: `repeated_trials_report_variation_and_flag_flaky_tasks`. |
| Provenance recorded | done | `build_provenance`: commit, dirty, runner+grading versions, suite SHA-256 + task ids, model identity, permissions, budgets, environment, arm pins/observed versions. Visible in every results file. |
| Historical results preserved | done | `eval/results/` append-only; 2026-09-14 findings retained as superseded context, not rewritten. |
| Pinned Kimi and Qwen runners alongside Grok | done | `RECIPES` pins (2026-09-15): grok 1.0.30, kimi 0.43.0, qwen 0.22.2. Kimi absent → typed skip; qwen present-but-unauthenticated → typed skip at probe. Never fabricated. |
| Same-model vs default-model comparisons separated | done | Documented in `eval/README.md`; the recorded live run is a default-configuration comparison (models recorded per arm). The same-model rerun is credential-blocked (below). |

### 1.5 Agent harness

| Criterion | Status | Evidence |
|---|---|---|
| Detached subagents (parent continues; bounded concurrency; status; retrieval; cancel) | done | `task_spawn` `background: true`; `MAX_DETACHED_SUBAGENTS = 4` session-wide; report via `job_status`/`job_output` + completion notifications; cancel via `/agents cancel` and `/jobs cancel` (watchdog). Tests: `detached_spawn_returns_immediately_and_the_report_lands_in_the_job`, `detached_spawn_is_cancellable_through_the_registry_and_the_job_flag`, `detached_spawn_enforces_the_session_concurrency_bound`. Commit `bffedf2`. |
| Worktree isolation, narrowed permissions, hooks, budgets preserved | done | Detached path runs the SAME `SubagentRunner` (`LiveSubagentRunner`) — worktrees, lattice, hook propagation and shared budgets are inherited by construction; existing subagent tests unchanged and green. |
| Restart behavior for children and retained worktrees defined | done | Documented in `execute_task_spawn_detached` + tool schema: a re-invoked `task_spawn` is a fresh child; state lives in the retained worktree until `/agents integrate\|abandon`. |
| Tool-repair connected or claims corrected | corrected | `crates/tool-gateway/src/repair.rs` module doc now states it is a library primitive with NO production caller; not advertised as delivered behavior. Production dispatch keeps handled argument errors. |
| Context recovery preserves requirements/state | verified (pre-existing) | `LiveRecoveryController::recover_from_overflow` + `PreservedLiveContext`; unchanged this window, covered by host tests. |
| Turn completion distinct from verified completion | done | Goal layer requires ledger-backed evidence; harness collector fixed (§1.3). |
| Evidence invalid when workspace state changes | partial | Typed mechanism exists (`EvidenceStore::invalidate_subject`, freshness in `can_complete`; in-crate tests) but NO production write-hook invalidates on workspace writes — recorded as a remaining gap, not claimed done. |

### 1.6 Product gaps

| Item | Status | Evidence |
|---|---|---|
| ACP session mode switching with truthful advertisement | done | `SessionModeControl` seam; without it `session/set_mode` stays METHOD_NOT_FOUND (original tests unchanged); with it `session/new`/`session/load` advertise `modes`+`currentMode` and switches change the next prompt's lattice (override > env > settings, still narrowed by the managed ceiling). Wired in `acp_serve.rs` (+ SDK daemon cell). Tests: `modes_are_advertised_and_set_mode_switches_for_subsequent_prompts`, `session_new_omits_mode_fields_without_mode_control`, `session_mode_override_wins_over_env_and_settings_but_not_forced`. acp 48+6 green. Commit `bffedf2`. |
| Progressive Anthropic streaming | done | `AnthropicSseTextDeltaParser` + `invoke_sync_streaming`; wired whenever a delta sink exists; canonical parse unchanged. Tests: `streaming_forwards_text_deltas_progressively_and_canonical_parse_holds`, `delta_parser_ignores_non_text_frames_and_survives_split_frames`. llm-router 144 green. Commit `dee578f`. |
| Interactive model switching | done | `KernelAction::SelectModel` drives the same `select_model` backend as `/model select`. Test: `kernel_action_select_model_switches_the_session_override`. Commit `dee578f`. |
| Interactive plugin install/trust | done | Install registers UNTRUSTED (executables disabled) through the same ledger; revoke narrows; permissions renders policy+grants+elevation argv. Elevation never happens from a slash command. Tests: `interactive_plugin_flow_registers_untrusted_then_revokes`, `interactive_plugin_install_rejects_a_privileged_manifest`. Commit `dee578f`. |
| Complete browser/computer-use workflow | partial | `/computer observe` + `/computer test` run the production stack (`ComputerUseRuntime` fence/JS-gate over `DesktopActor` over the platform AX host) through the real interactive loop, and fail CLOSED with typed reasons (this build never links ApplicationServices). Tests: `computer_observe_reports_the_typed_platform_gate_not_a_stub`, `computer_selftest_executes_the_production_policies`. NO live OS/browser driver exists (`LiveMacosAxHost`/Playwright boundary are fail-closed stubs), so no live end-to-end workflow is claimed. Commit `dee578f`. |

### 1.7 Validation

| Criterion | Status | Evidence |
|---|---|---|
| SDK test command executes all intended test files | done | `pnpm test` now runs all four files: 28 tests, 4 suites, 0 failures (was 8 tests / 1 file). `sdk/typescript/package.json`. |
| Formatting clean | done | `cargo fmt --all -- --check` clean (fixed eval runner, interactive, model, mcp transport, tui state). |
| Lint clean (`clippy -D warnings`) | done | `cargo clippy --workspace --all-targets --locked -- -D warnings` exits 0 after mechanical fixes (collapsible ifs, unused imports/vars, redundant closures, while-let, complex types, duplicated attributes). Commit `82a6510`. Notable: ci.yml's lint/test steps had ALREADY been failing on main before this window (the 09-14 "green" runs were release-matrix build smokes, not ci.yml) — restoring it to green is part of this delivery. |
| Full supported-platform Rust suite | done (CI, GREEN) | Local full-workspace run stalls at 0% CPU (the assessment's documented local startup pathology), so the equivalent evidence is CI: **run `34975308875` — conclusion `success` on ubuntu-latest, macos-latest, windows-latest (fmt + clippy `-D warnings` + `cargo test --workspace --locked --no-fail-fast` + protocol schema fixtures) and the SDK job** (`generate:check`, `typecheck`, all four test files). Getting there surfaced and fixed the pre-existing red ci.yml had been carrying on main: 2 stale mcp_cli expectations (streamable-HTTP servers are first-class now), 1 sandbox truth test (approval gate preempted the confinement refusal on Linux), 1 help-marker test needing the new wired-command markers, 3 timing races in this window's own new tests, and 4 rounds of Windows-only `cfg(unix)` import/helper fall-out in `daemon_serve.rs`. rapid lib locally: 815 passed / 0 failed. |
| Typecheck + schema checks | done | `pnpm typecheck` 0 errors; `pnpm generate:check` no drift. |
| Release smoke | via CI | `release-matrix.yml` (macos/ubuntu/windows build + binary smokes + clean-env smoke) — dispatch/tag-driven; the retained evidence is the 2026-09-14 green run (34888814182) plus this window's local smokes (`rapid eval --offline` invocations, trust-grant, health probes). |
| Unsupported vs supported platforms distinguished | done (pre-existing + kept) | Windows cargo test `continue-on-error` is explicit and documented in `ci.yml`; the daemon's Unix-socket path compiles to a typed refusal on non-Unix. |
| Local startup delays investigated | done | Local `cargo test` binary startup stalls sample as `_dyld_start` (dynamic linker), before test execution — OS-level, not application code; no OS protections weakened; CI provides the equivalent stable environment. |
| No assertions weakened / gates relaxed | done | All changes add checks; the only gate change makes the eval exit STRICTER (skips now fail). |

## 2. Parity matrix

Legend: **verified** = implemented + regression-tested through production entry points; **partial** = implemented with a recorded gap; **unsupported** = absent by design on this build; **unmeasured** = no live run evidence this window.

| Capability | RapidLM status | Notes / evidence |
|---|---|---|
| Terminal + headless coding agent | verified | exec/TUI/eval; offline + live runs retained. |
| Evaluation harness (grading v2) | verified | §1.1; suite hashes + provenance in every result. |
| Detached subagents | verified | §1.5; bounded, retrievable, cancellable. |
| Worktree-isolated write children | verified (pre-existing) | `agent_views.rs`; exercised via subagent tests. |
| Durable goals + evidence-gated completion | verified (pre-existing) | `goal_host.rs` gates; claim verifier runs real commands. |
| Context recovery after overflow | verified (pre-existing) | `host.rs` recovery folds; crash/restart test in `kernel`. |
| MCP (stdio) | verified (pre-existing) | Registration + dynamic tools; probe/doctor surfaces. |
| ACP serve v1/v2 | verified | Serve loop + adapters; mode switching added. |
| Progressive Anthropic streaming | verified | §1.6; deltas live, canonical parse unchanged. |
| Progressive OpenAI-compatible streaming | verified (pre-existing) | `invoke_sync_streaming` + delta sink in production. |
| Interactive model switching | verified | Same backend as `/model select`. |
| Plugin trust management (interactive) | verified | Fail-closed install/revoke/report; elevation stays explicit. |
| Background shell jobs | verified (pre-existing) | `JobRegistry`; session-scoped, bounded. |
| ACP session mode switching | verified | Truthful advertisement; consistent permission behavior. |
| Browser automation (live) | unsupported | Playwright boundary exists but only an in-process stand-in implements it; no real driver shipped. |
| Desktop computer use (live) | partial | Production observe/test entry points run and fail closed typed; no OS AX driver linkage (`forbid(unsafe_code)`); no live workflow claimed. |
| Session recording (`/computer record`) | unsupported | No capture sink; stated in-command. |
| Tool-call auto-repair in production | unsupported | Library primitive only; claim corrected. |
| Model routing beyond chat/compaction purposes | partial | Purpose enum + compaction route exist; live requests build as Chat (recorded, not advertised as smart routing). |
| Competitor live comparisons (same model) | unmeasured | Credential-blocked this window (below). |

## 3. Evaluation outcomes (retained evidence)

- **Offline (mechanical, grading v2):** representative 12/12, held-out 4/4,
  smoke 40/40 — gold trajectories through the real executor pass integrity,
  verification, and every mutation check
  (`eval/results/offline-1789458191.json`, `-8194.json`, `-7005.json`).
- **Live (default configurations, representative suite, equal shell surface):**
  grok CLI 1.0.30 (grok-4.6) **12/12 passed**; 154,069 measured tokens per
  verified success, $0.0828 published-rate cost estimate per success,
  complete usage coverage (`eval/results/live-1789458229.json`, provenance:
  commit `7978614`, suite hash `c56100b8…`, grant_shell=true).
  rapid arm: all 12 skipped — TYPED infrastructure failure: both configured
  models rejected the health probe (`deepseek-v4-flash-vision-exp`: provider
  rejection; `openrouter` fallback: authentication). kimi: not installed.
  qwen 0.22.2: present, not authenticated. The gate correctly refused to
  pass an incomplete comparison.
- **Not measured:** same-model (grok-4.6 vs rapid-on-grok-4.6) comparison —
  no x.ai credential available in this window; this is the exact blocked
  criterion. No parity percentage is claimed from a one-sided run.

## 4. Remaining limitations

1. **Evidence invalidation hook** (§1.5): typed mechanism exists; production
   workspace-write trigger not wired.
2. **Live browser/desktop drivers** (§1.6): fail-closed stubs only; live
   workflows unsupported on this build.
3. **Same-model competitor comparison**: blocked on x.ai credentials.
4. **Qwen authentication / kimi installation**: typed skips; rerun
   `rapid eval --live --arms rapid,qwen` once credentials/drivers exist.
5. **Scale and suite axes**: the representative suite is 12+4 tasks with
   repeated-trial support — far smaller than SWE-bench-scale programs;
   results are directional, not certification. Of the goal's task axes,
   provider-interruption/cancellation/restart, approval suspension,
   concurrent-subagent merge conflicts, and MCP/ACP/streaming workflows are
   covered at the RUNTIME level (crash-recovery tests, detached-subagent
   tests, ACP adapter suites, eval typed timeouts) rather than as suite
   tasks — extending the suite to those axes is future work.
6. **Release-matrix CI** for this exact commit: `ci.yml` runs on the push of
   this commit; `release-matrix.yml` remains dispatch/tag-driven.
7. **ci.yml had been red on main before this window** — the pre-existing
   failures (stale mcp_cli "stdio only" expectations from before
   streamable-HTTP support, the sandbox truth test's approval-gate
   preemption on Linux, and the help-marker drift) are fixed here;
   `docs/reference/cli-command-reference.md`'s "actually dispatches" note
   was the only doc-sync contract affected and still passes its source
   check.
