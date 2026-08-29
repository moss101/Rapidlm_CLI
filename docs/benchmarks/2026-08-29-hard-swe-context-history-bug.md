# Hard benchmark: 8-module SWE-repo-fix finds a 100%-reproducible context bug

Date: 2026-08-29 · Host: macOS (arm64) · Model: `deepseek-v4-flash-vision-exp` via `https://api.b.ai/v1` (the account's default configured model — no new API key needed)

Harness and seed repo were built for this run in a scratch directory outside the repository and deleted afterward, per instruction — nothing benchmark-related is committed except this report and the code fix it led to.

## What was run

**Baseline health check** (already-committed opt-in suites, no changes):
- `real_model_bench.rs` (S1–S5): single write, cross-step memory, multi-file rename + shell verify, subagent spawn, live `web_fetch`. **5/5 passed.**
- `tool_call_bench.rs` (scripted, no API calls): dispatch, 16-call batch cap, pagination, patch payload, verify-pattern, drained-notification delivery. **6/6 passed.**

**Hard benchmark** (built for this run, not committed): a Python package `textlib` with **8 modules** — 7 seeded bugs (LRU recency, CSV trailing-empty-field trim, duration minutes rollover, priority-queue tie order, tokenizer hyphen join, trie prefix search, semver numeric compare) plus **one correctness distractor** (`redact.py`, intentionally unusual-looking but bug-free, to check whether the agent "fixes" code that isn't broken). 49 visible tests (17 failing on the seed), scored independently against a held-back hidden suite (25 more tests, never given to the agent) plus anti-cheat checks: `tests/` must be byte-identical to the seed, exact commit message, clean tree. Solvability was proven first with a hand-written reference fix passing all 74 visible+hidden tests.

Task prompt and rules mirrored the existing `2026-08-29-swe-repo-fix` benchmark (don't touch `tests/`, don't game tests, single commit with an exact message, clean tree).

## Result: 0/3 → root cause → fix → 1/1

| | Before fix | After fix |
|---|---|---|
| Attempts | 3 (independent, fresh clones) | 1 |
| Successes | **0** | **1** |
| Files edited (any attempt) | **0** — confirmed via `git diff --stat HEAD` and `git status --porcelain` after every attempt | 7/7 correct, `redact.py` left untouched |
| Visible+hidden tests passing at end | 32/74 failing every time (identical count to the untouched seed) | 74/74 |
| Outcome | `turn outcome=failed reason=budget_exhausted`, empty stdout, exit 1/1/timeout | exit 0, exact commit message, clean tree, 180.6s |

All three pre-fix attempts hit **exactly 32/32 model steps** (the hard per-turn ceiling) or the 600s wall-clock deadline, made **zero `workspace_write` calls**, and ended with the identical 32-test failure count as the pristine, untouched seed — i.e. the CLI never once attempted an edit.

## Root cause

The full (untruncated) stderr transcript of a diagnostic single-attempt run showed the agent doing nothing but re-reading the same ~9 files and re-running the visible test suite, over and over, for all 32 steps:

```
workspace_read calls: 113   (across 32 model steps, ~9 files)
shell_exec calls:     16    (same `python3 -m unittest discover` command, repeated)
workspace_write calls: 0
```

`apps/rapid/src/model.rs` replays the turn's tool-call history into every model request (this is the model's only memory of prior tool results — RapidLM does not use the provider's own conversation state). That replay is capped by two constants:

```rust
const MAX_TOOL_HISTORY_EXCHANGES: usize = 12;
const MAX_TOOL_HISTORY_BYTES: usize = 24 * 1024;   // 24 KiB
```

Once either bound is hit, `kept_history()` drops the **oldest exchanges wholesale** — silently, with no notice to the model. With 9 files' worth of read results plus a few verbose `python3 -m unittest -v` runs, the transcript blew past 24 KiB / 12 exchanges within the first handful of steps. Every subsequent request to the model was missing the file contents it had already read (and the exchange carrying `I already read file X` was gone too, not just summarized) — so from the model's perspective those files were unread, and it read them again. This is a pure eviction, not real compaction: the codebase already has a working context-fabric summarization path (`compact_packet`, wired through `LiveRecoveryController`) for genuine provider-side overflow, but this pre-emptive 24 KiB cap fires long before that path is ever needed, and produces no summary at all.

This did not show up in the existing benchmark suite because none of those scenarios touch more than 3 files or run a verbose command more than once — the cap is generous enough for small tasks and catastrophic for anything wider.

## Fix

`apps/rapid/src/model.rs`: raised the two constants (sized against the model's `DEFAULT_CONTEXT_WINDOW` of 32,768 tokens, ~128 KiB) —

```rust
const MAX_TOOL_HISTORY_EXCHANGES: usize = 48;   // was 12
const MAX_TOOL_HISTORY_BYTES: usize = 96 * 1024; // was 24 KiB
```

Genuine overflow on smaller-context models or longer sessions still recovers correctly through the existing `compact_packet` semantic-summarization path — this only removes the premature, silent, un-summarized eviction that was firing well below any real limit.

Added two regression tests (`kept_history_keeps_many_small_file_reads_within_the_new_budget`, `kept_history_still_prunes_oldest_once_the_byte_budget_is_exceeded`) — there was no existing unit coverage of `kept_history`'s threshold behavior at all.

**Verification:** same task, same model, fresh clone, one attempt: **exit 0, 74/74 tests passing, `tests/` byte-identical to seed, exact commit message, clean tree, 180.6s.** The model's own bug list matched the reference fix on all 7 bugs and correctly left the `redact.py` distractor alone. Full lib suite (202 tests), the mechanical `tool_call_bench` (6/6), and the real-model `real_model_bench` baseline (5/5) all still pass after the change.

## Follow-up fixes (same day, after this report's first draft)

Two items flagged above as "not fixed" were root-caused and fixed on request:

- **`tokens=0` in every diagnostic line — fixed.** Root cause: `api.b.ai` streams a `usage` object, but its `prompt_tokens`/`completion_tokens` fields are null, so `NormalizedUsage` resolves to a real `Some(...)` whose total is 0 — indistinguishable in the old code from "usage was never reported." A real completed exchange never actually costs 0 tokens, so `apps/rapid/src/model.rs` now falls back to a byte-derived estimate (~4 bytes/token, from the request's message text plus the response text/tool-call arguments) whenever the resolved total is 0; any real reported total still wins. Live-verified: the same `hi.txt`-write smoke task that previously logged `tokens=0` on every line now logs `tokens=138`, `tokens=144`, `turn outcome=succeeded tokens=282`. Two new unit tests cover the fallback and the "usage event present but empty" shape; the existing `fold_stream_collects_tool_calls_with_arguments` test's `tokens == 0` expectation was updated to `tokens > 0` since it was asserting the exact bug.
- **Hallucinated tool names recur under load — mitigated.** The "unknown tool" error previously said only "it is not part of this session's tool surface; use one of the tool names given in the tool surface" — a pointer back at the structured schema, not a concrete correction. `apps/rapid/src/exec_tools.rs` now lists the real tool names directly in the failure text (kept compact against the 256-byte detail cap — a first attempt at this that used a wordier prefix silently truncated `workspace_read`/`workspace_write` off the end of the list, which would have made the fix worse than the original message for the two tools most often confused). Not independently re-benchmarked against a fresh hallucination episode (none recurred in the post-fix verification run), so this is a live-verified error-text fix, not a live-verified reduction in hallucination rate. The existing `ToolCallLoopDetector` (exact repeated tool+arguments, 3-in-a-row) is unaffected and still the only structural loop-breaker; a model varying its arguments across repeated attempts at the same nonexistent tool name still would not trip it — noted, not addressed here.

All fixes verified: full lib suite (203 tests), mechanical `tool_call_bench` (6/6), and live single-scenario re-runs, all green. Working tree after this session: only `apps/rapid/src/model.rs`, `apps/rapid/src/exec_tools.rs`, and this report — no benchmark scaffolding.

## Provider flakiness (observed, not a defect)

9 `outcome=failed:connection` retries occurred in a single 32-step run against `api.b.ai` during the original hard-benchmark run; the retry-then-succeed path (from `270ce2c`) worked correctly every time. Not itself a bug, but a real cost — consistent with the flakiness noted in the prior `2026-08-29-swe-repo-fix` report.

## Threats to validity

One clean before/after comparison (3 failing attempts, 1 fix, 1 successful re-run) on one free/small provider model; a stronger model might have compensated for the truncated history by re-deriving fixes from memory or hitting the bug less often. The distractor module and reference fix were authored by the same person who wrote the bugs. The secondary observations above are single-run signals, not independently re-verified.

## Reproduce

Not checked in (per instruction). The harness was: an 8-module `textlib` seed (5 modules reused from [`2026-08-29-harness/swe-seed`](./2026-08-29-harness/) at the pre-revert commit, plus 2 new bugs in `trie.py`/`version.py` and the `redact.py` distractor), a held-back hidden-test directory, and a temporary `apps/rapid/tests/hard_bench.rs` integration test (same pattern as the existing `real_model_bench.rs`: isolated `RAPIDLM_HOME`, programmatic project-trust grant, `RAPIDLM_PERMISSION_MODE=bypassPermissions`, real `rapid exec` against the user's configured model) that git-inits the seed, runs the real binary, and scores the result against visible + hidden tests plus the anti-cheat checks. Deleted after this run; regenerate from this description to replay.
