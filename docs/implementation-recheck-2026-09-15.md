# Implementation recheck — 2026-09-15

Reviewed HEAD: `bc87a89`. Previous assessment baseline: `7816caa`.

## Verdict

**Substantial remediation has landed, but the goal is not fully implemented or verified.** Some originally missing capabilities now have working production paths. Important acceptance criteria remain open, and this review found residual evaluation defects and a detached-agent cleanup bug.

This review inspected the changed source, reran targeted checks, downloaded CI logs, and inspected retained evaluation results. It did not run new live model comparisons or modify application source.

## Implemented improvements

| Area | Current evidence | Assessment |
|---|---|---|
| Test grading | Protected-file checks, mutation checks, and negative controls in `eval_serve::grade_submission`; 24 evaluation tests passed locally | Material improvement; remaining false-positive judge below |
| Process output handling | `run_supervised` drains stdout/stderr concurrently and uses process-group termination on timeout | Original pipe-blocking path corrected |
| Usage accounting | `agent_usage_metrics` includes failed attempts and exposes missing measurement coverage | Original success-only token aggregation corrected; model pricing remains incomplete |
| Evaluation exit gate | Empty, failed, and skipped results produce nonzero exit | Correct for arms that reach the result list; unknown requested arms still disappear |
| Suite breadth | 12 representative tasks, 4 held-out tasks, separately labelled 40-case smoke suite; repeated-trial support | Improved; several requested evaluation axes still absent |
| Detached agents | Background spawn, bounded concurrency, job status/output, cancellation | Implemented, but normal-completion cleanup is defective and continuation is a fresh child |
| ACP modes | Production `SessionModeControl` wired into ACP serve and permission resolution | Implemented |
| Anthropic streaming | Model adapter uses `invoke_sync_streaming` when a delta sink is present | Implemented |
| Interactive model selection | `KernelAction::SelectModel` reaches the existing session selection backend | Implemented |
| Interactive plugin controls | Manifest registration, revocation, and permission reporting reach the trust catalog | Implemented at this scope; registration alone is not a complete package marketplace/install lifecycle |
| SDK and formatting | All four SDK test files included; 28 tests and typecheck passed; format check passed | Original validation defects corrected |

## Findings still preventing sign-off

### P1 — A representative grader still accepts a broken pipeline

`eval/suite/errors-deploy-pipeline.json:19` verifies with `sh deploy.sh | grep DEPLOY-OK`. The pipeline reports the status of `grep`, not the deployment command. There are no mutation checks for this case.

**Local negative probe:** seeded the task in a temporary directory and changed only `tools/migrate.py` to print `DEPLOY-OK` and exit 1. All protected files remained unchanged. The actual deployment exited 1, migration/seed output files were absent, but the judge exited 0. This submission therefore satisfies every current post-run grading check while failing the task.

Fix the judge to require successful execution and validate resulting artifacts/behavior independently. Add this invalid submission as a negative control. The 24 passing evaluator tests do not cover this case.

### P1 — Detached agents leak threads after ordinary completion

`apps/rapid/src/exec_tools.rs:3868` starts a watchdog that exits only when a cancellation token or job flag is set. The ordinary success/failure path marks the job terminal and then calls `watchdog.join()` at line 3939 without stopping that watchdog.

`ChildLifecycle::finish` merely unregisters the child; it does not cancel its token. `stop_if_running` refuses cancellation after a job becomes terminal. Consequently an ordinary completed child leaves its watchdog running and its worker blocked joining it. The concurrency slot is already released, so repeated tasks can accumulate threads despite the four-agent concurrency limit.

This is a source-traced lifecycle defect. The three existing detached-agent tests pass because they verify reported completion and slot release, not termination of the worker and watchdog. Add an explicit watchdog stop signal with guaranteed cleanup and a lifecycle regression test.

### P1 — Evidence invalidation after edits remains unconnected

`crates/agent-runtime/src/evidence.rs` implements `invalidate_subject`, but searches found no production invocation on workspace writes. The delivery record explicitly acknowledges this at `docs/goal-delivery-2026-09-15.md:149`.

The requested guarantee that verification evidence becomes stale when relevant code changes is therefore not closed. Wiring and production-level regression coverage remain necessary.

### P1 — No complete live computer-use workflow

Interactive observe/test commands now reach the existing policy/backend stack, but `LiveMacosAxHost::probe` hardcodes accessibility trust false, and its `snapshot` and `act` methods return errors (`crates/computer-use/src/desktop/macos.rs:446`). The only implementation found for `PlaywrightBackend` is `FakePlaywright`.

A real observe → authorize → act → verify → recover workflow is still unavailable. Correctly reporting an unavailable backend is useful, but it does not meet the goal's live-workflow criterion.

### P2 — Evaluation classification, pricing, and provenance are still partial

- `run_live_agent` converts **every** `prechange_check` error into `RunFailure::Vacuous` (`apps/rapid/src/eval_serve.rs:1541`). Missing commands and pre-change timeouts therefore lose their infrastructure classification in the actual live path, despite the helper distinguishing them.
- Cost estimation always uses the hardcoded Grok-4.6 rate tuple, regardless of the actual selected model (`eval_serve.rs:1254`, called for live results at line 1490). Use model-specific prices with an explicit version/basis, or report the estimate unavailable.
- `--arms` filters known recipes by the requested names without rejecting unknown names (`eval_serve.rs:2075`). A typo alongside a valid arm is silently omitted, so the resulting gate cannot guarantee that every requested arm was tested.
- Provenance records the RapidLM active model name and arm versions, but not resolved competitor model identities, complete sampling/configuration data, exact invocation arguments, or all model/tool budgets. The latest live artifact has `dirty: true` without a snapshot of those changes. This is insufficient for exact experiment reconstruction.

### P2 — Continuation and evaluation breadth remain limited

The detached-agent implementation explicitly says a child cannot resume mid-run and another spawn creates a fresh child (`exec_tools.rs:3817`). Durable child-session continuation is not delivered.

The new 12+4 suite does not include the requested live-model tasks for provider interruption/restart, approval suspension, concurrent-subagent merge conflicts, or MCP/ACP/streaming workflows. Runtime unit/integration coverage is valuable but is a different kind of evidence. The delivery record acknowledges the distinction at lines 156–163.

Tool auto-repair remains a library primitive with no production caller; smart phase routing remains limited. Correcting the documentation is a valid outcome where the goal allowed that option, but these should not be counted as newly implemented features.

## Verification and remaining empirical gaps

### Fresh local checks

- `cargo test -p rapid --lib --locked eval_serve::tests`: **24 passed**.
- `cargo test -p rapid --lib --locked detached_spawn`: **3 passed**.
- `pnpm test`: **28 passed**, all four test files.
- `pnpm typecheck`: **passed**.
- `cargo fmt --all -- --check`: **passed**.
- Broken-deployment negative probe: **incorrect submission accepted**, as detailed above.

### CI checked directly

[CI run 34975308875](https://github.com/moss101/Rapidlm_CLI/actions/runs/34975308875) tested `f66930d`. Only `docs/goal-delivery-2026-09-15.md` changed between that commit and reviewed HEAD, so this CI run covers the current application source.

- Linux and macOS: successful Rust/SDK-related CI gates; downloaded logs show no Rust test failures for these platforms.
- Windows: logs report **45 test failures** across four completed test groups (27 + 11 + 3 + 4). The workflow deliberately sets `continue-on-error` on Windows tests (`.github/workflows/ci.yml:67`), so a green job is not proof of Windows runtime correctness.
- Latest successful release-matrix run found is [34904042578](https://github.com/moss101/Rapidlm_CLI/actions/runs/34904042578), for the old baseline `7816caa`. The new implementation has not been validated by that release matrix.

### Live comparison evidence

The newest retained live run, `eval/results/live-1789458229.json`, records **Grok 12 passes; RapidLM 12 skips; Kimi 12 skips; Qwen 12 skips**. These are historical availability failures, not a fresh determination of today's credentials or installations. No new successful RapidLM representative-suite run or complete same-model comparison is present in that artifact. Full competitor parity remains unmeasured.

## Completion order

1. Fix the broken-pipeline judge and detached-worker cleanup bug.
2. Connect evidence invalidation to actual workspace mutations.
3. Finish the required live computer-use workflow and supported child continuation.
4. Close remaining evaluator classification, pricing, requested-arm, and provenance gaps.
5. Expand the missing evaluation axes and rerun representative comparisons with working, pinned arms.
6. Run the release matrix for the new implementation and preserve the explicit Windows support limitation until runtime tests pass.

The previous untracked assessment and pre-existing modified `.rapidlm/goal.json` were preserved.
