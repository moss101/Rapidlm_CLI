# Post-fix re-verification: full benchmark suite re-run

Date: 2026-08-29 (same day, after landing [46c854c](https://github.com/moss101/Rapidlm_CLI/commit/46c854c) and [02a583b](https://github.com/moss101/Rapidlm_CLI/commit/02a583b)) · Host: macOS (arm64) · Model: `deepseek-v4-flash-vision-exp` via `https://api.b.ai/v1`

A second, independent run of the full benchmark suite from [2026-08-29-hard-swe-context-history-bug.md](./2026-08-29-hard-swe-context-history-bug.md), against the binary with all of that report's fixes plus the two new features (`gaps.md` §20 findings #1/#2) landed. Same rule as before: harness built outside the repo, deleted after the run; only this report and the two temporary test files' *results* persist.

## Results

| Suite | Before (same day) | This run |
|---|---|---|
| Mechanical `tool_call_bench` (no API) | 6/6 | **6/6** |
| Real-model baseline `real_model_bench` (S1–S5) | 5/5 | **5/5** (S3 needed one retry — `model_failed`, provider-side transient, passed clean on retry at 149.8s; not a code issue) |
| Hard SWE-repo-fix (8 modules, 7 bugs, hidden tests) | 0/3 → fixed → 1/1 at 180.6s | **1/1 on the first attempt, 59.4s** — under a third of the post-fix verification run's time |
| Shadow-diagnostics live smoke (new) | not yet benchmarked | **pass**, 5.4s |

## What's different from the first post-fix run

**Real token accounting, live.** Every `model step` line in this run carries genuine per-step numbers (`tokens=8327`, `7813`, `8622`, …, turn total `61124`) instead of the flat `0` from before the `tokens=0` fix. This wasn't just a unit-test claim — it's now visible on an actual production-shaped task.

**Faster, cleaner solve.** 59.4s vs. 180.6s on the same task class. The model reached for `workspace_patch` (surgical edits) rather than full-file `workspace_write` rewrites this time, and needed only 9 model steps total — well under the old 32-step ceiling that used to be exhausted purely on re-reading. This is consistent with, not proof of, the context-history fix (model behavior varies run to run) but the shape matches what the fix predicts: no repeated re-reads of already-seen files.

**Shadow diagnostics confirmed live**, not just in the 10 unit/integration tests from implementation. Asked the real model to write `add.py` with a `.rapidlm/settings.json` configuring `python3 -m py_compile {path}` gated on `*.py`: the tool result read back `wrote 32 bytes to add.py (shadow diagnostics: ok)`, and the file landed correctly. Full round trip through settings parsing, git-repo detection, isolated worktree creation, the diagnostics command, and the real write — all exercised by an actual model turn, not a scripted one.

**S3 (multi-file rename + shell verify) failing once on `model_failed`** and passing clean on immediate retry is the same class of provider flakiness noted in every benchmark report so far today (`2026-08-29-hard-swe-context-history-bug.md`'s "Provider flakiness" section, the original `2026-08-29-swe-repo-fix.md`'s three same-day rejections). Not re-diagnosed here — it's an established, external characteristic of the free/budget `api.b.ai` endpoint, not a RapidLM defect.

## Bottom line

Nothing regressed. The two fixes from earlier today (context-history eviction, token accounting) hold up on an independent re-run with a visibly better result (first-try success, real numbers, less than a third of the wall-clock), and the two new features shipped this session (`NodeState::Paused`/`GraphService::pause`/`resume`, shadow-verified writes) don't appear in the mechanical/baseline suites (they're not on `rapid exec`'s critical path for those scenarios) but shadow diagnostics is now confirmed working end-to-end against the live model, not just in isolation.

## Not covered by this run

- `NodeState::Paused`/`GraphService::pause`/`resume` has no live benchmark here — it's reached through `rapid run`/durable graph execution, not `rapid exec`, and none of today's benchmark scenarios exercise that path. Its 2 tests in `crates/scheduler/src/lib.rs` are the only verification so far.
- `workspace_patch` is not covered by shadow diagnostics yet (noted as a known follow-up in `gaps.md` §20 #1); this run's hard-SWE task happened to use `workspace_patch` for its fixes, so those specific edits were not shadow-verified even in a project with `shadow_diagnostics` configured (this project wasn't configured with it — only the dedicated live smoke test was).
