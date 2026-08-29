# SWE repo-fix benchmark: Qwen Code 0.22.2 vs Rapid CLI (`rapid exec`)

Date: 2026-08-29 · Host: macOS (arm64) · Model for both: `inclusionai/ling-3.0-flash-fin:free` via `https://openrouter.ai/api/v1`

Third installment, after the [one-shot](./2026-08-29-qwen-code-vs-rapid.md) and [multi-part agentic](./2026-08-29-agentic-hard-task.md) runs. This time: one long SWE-style repository-repair task against a seeded real codebase, scored the way SWE-bench scores — by an independent test suite plus held-back hidden tests, never by the tool's own report.

## The task

A synthetic-but-realistic Python package (`textlib`, 5 modules, ~150 LOC) in a git repo, with **5 seeded bugs** that look like plausible regressions:

1. `lru.py` — `get()` returns the value but does not refresh recency, so eviction expels the wrong entry.
2. `csvlite.py` — a trailing-empty-field trim drops legitimate empty last fields.
3. `duration.py` — minutes computed from total seconds instead of the post-hour remainder (`1h 61m 1s`).
4. `prio.py` — heap tiebreaker sign flipped, so equal priorities pop LIFO instead of FIFO.
5. `tokenize.py` — word regex lost the intra-word hyphen join; compounds split apart.

The visible suite (`tests/`, stdlib unittest) has **33 tests, 13 failing** on the seeded state — more failures than bugs because two round-trip tests cascade from the duration bug. Diagnosis requires running the suite, reading code, and reasoning; there is no shortcut.

## Method and anti-cheating

- Identical fresh `git clone`s for both tools, same prompt (`swe_task.txt`): make the suite pass, do **not** touch `tests/`, no test-gaming, targeted fixes only, commit with the exact message `fix: make test suite green`, clean tree.
- Scoring by the harness, not the tools: visible suite, **18 hidden tests** (same contract, different data, kept outside the repo so neither tool could see them), a byte-level check that `tests/` is untouched vs the seed commit, the commit message, and tree cleanliness.
- **Solvability proof before the run:** the harness author's own reference fix passes all 33 visible + 18 hidden tests. One hidden test was deleted during design because it asserted `keys()` ordering — an implementation detail no visible test implies; hidden tests must test the visible contract, not internals.
- Same-model provider config as prior runs; rapid headless with project trust + `bypassPermissions`; qwen with `--approval-mode=yolo`. Runs sequential.

## Attempts — all of them

rapid (free-tier provider flaked repeatedly, as it did all day):

| Attempt | Outcome |
|---|---|
| 1 | exit 1 after 8.6 s — `model_failed (provider rejection)`, 6,323 tokens, repo untouched |
| 2 | exit 1 after 6.6 s — same rejection (backoff of 60 s applied before attempt 3) |
| 3 | repo **fully fixed and committed** in 46.7 s, 19 model steps, 147,859 tokens — but the turn then died on an empty model response: CLI exit 1 (`empty_response`), stdout empty |

qwen:

| Attempt | Outcome |
|---|---|
| 1 | exit 0 after 107.4 s — 10 API requests, 20 tool calls (11 read_file, 5 edit, 4 shell, 2 todo), 0 denials, 372,800 tokens (321,370 of them cache reads) |

## Independent verification

| Check | rapid (attempt 3 state) | qwen |
|---|---|---|
| Visible suite: 33/33 pass | PASS | PASS |
| Hidden suite: 18/18 pass | PASS | PASS |
| `tests/` byte-identical to seed | PASS | PASS |
| Commit message exactly `fix: make test suite green` | PASS | PASS |
| Working tree clean | PASS (harness logs left untracked) | PASS (harness logs committed into the fix commit) |
| **Outcome** | **Fixed** | **Fixed** |

Both tools produced **byte-identical library fixes** (`diff -r` between the two repos is empty): five minimal edits across the five files, functionally equal to the reference fix (duration and priority lines are identical to it; lru/csv/tokenize differ only in phrasing). Neither gamed the tests — the hidden suite proves the fixes generalize.

## Findings

**Both tools solved a real SWE-style repair task.** Five root-cause bugs, thirteen failing tests, one long tool-calling session each, and both ended with the same five-line-diff repair and a conforming commit.

**Cost and speed on real work:** rapid 147.9k tokens / 46.7 s vs qwen 372.8k tokens / 107.4 s — **2.5× tokens, 2.3× wall** in rapid's favor. The gap is narrower than the one-shot benchmark (29–181×) and consistent with the agentic run (3.4×): per-step overhead dominates for both; qwen's is just bigger per step (20.5k-token baseline, partially cache-served — 321k of its 363k input tokens were cache reads).

**Scriptability has nuance worth knowing.** rapid's successful run exited 1 — the provider returned an empty response while the model was writing its final summary, after the fixes were already committed. Judged by exit code alone, a script would mark failure and retry; judged by repository state (the SWE-bench way), it is a full success. qwen's exit 0 meant exactly what it claimed. Neither tool's stdout was a reliable report of the repo state in rapid's case — verification has to be external.

**Provider flakiness is the real tax.** Two of rapid's three attempts (and one earlier today) died on `model_failed (provider rejection)` within seconds of each other. On a free-tier endpoint, per-attempt cost matters: rapid's failed attempts burned ~6k tokens each; the same flakiness would have cost qwen ~30k+ per rejection given its per-request baseline.

**Harness artifacts leak.** My runner wrote its log files inside the project directories; rapid left them untracked, qwen swept them into its fix commit. Both behaviors are defensible readings of "commit all files / clean tree"; the leak is the harness's fault, and it previews a real issue for agent benchmarks: instrumentation inside the workspace becomes part of the task.

## Re-run after the diagnosability fixes (same day, ~2h later)

The six deficiencies found above were fixed in commit `270ce2c` (tool-failure detail payload, per-call stderr lines, provider-rejection/empty-response retry, exit-code fidelity, withheld-tools warning, `exec --help`). The benchmark was re-run from fresh pristine clones against the fixed binary.

| | rapid (fixed) | qwen |
|---|---|---|
| Attempts to success | **1** (exit 0) | 1 (exit 0) |
| Wall time | 33.9 s | 53.0 s |
| Tokens | 52,044 (7 model steps, from verbose per-step lines) | 361,382 (11 requests, 19 tool calls) |
| Visible suite | 33/33 OK | 33/33 OK |
| Hidden suite | 18/18 OK | 18/18 OK |
| tests/ untouched, exact commit message | PASS | PASS |
| Library diff vs reference | identical shape (5 files, +4/−5) | same, except tokenizer also joins `+` (untested extrapolation beyond spec) |

The new per-call stderr trace was visible throughout rapid's run (24 `tool …: ok/failed` lines), including a live demonstration of its value: the model's `git commit --format=%s` mistake and its exit-129 error appeared right in the stream. No provider rejection occurred in this run, so the retry path was exercised only by the scripted tests, not in the wild.

**Mid-run stderr silence — root-caused and fixed:** in rapid's successful run, stderr stopped receiving output at 05:27:03 — the last line is the argv trace of a `git add -A` call — while stdout kept working (final summary written, process exited 0 ten seconds later); the committed copy of the stderr log inside that very commit proves nothing more was ever written, so `turn outcome=`, `tokens used:` and time's `real` line were lost. rapid's token total above is reconstructed from the verbose per-step lines. Two structural loss mechanisms were found in the code and removed in `270ce2c`'s follow-up: (1) `eprintln!` panics on a failed stderr write, and inside a `batch_dispatch` worker thread that panic killed the thread quietly while `if let Ok(handle.join())` then dropped the whole group's outcomes; (2) the same fire-and-forget writes made every diagnostic line lossy-by-design. All exec diagnostics now flow through one error-resilient writer (`exec_diag::stderr_line`, never panics), a dead batch worker surfaces its calls as typed per-call failures instead of vanishing, and a deterministic regression test (`stderr_stays_complete_when_the_agent_git_adds_the_log_file`) drives the real binary through the triggering shape — the agent `git add`s and commits its own stderr log mid-run — asserting every diagnostic line survives to process exit (10 stress iterations, all green). Follow-up re-verification attempts on the live provider were then hard-rejected for 45 minutes straight (free-tier window closed; five typed-failure runs, each with a complete stderr tail — the loss did not recur on the live path), so the plan-sanctioned scripted stand-in stands as the final re-verification evidence; rapid's earlier live success in this re-run (33.9 s, first try) plus the scripted invariant tests cover the fix. Everything the benchmark scores was unaffected by the anomaly.

**Bottom line:** with the fixes in, both tools again solved the repo repair on identical terms — and rapid now does it first-try, cheaper, faster, and with its behavior visible line-by-line on stderr.

## Threats to validity

One successful run per tool; the underlying free model is small and flaky (3 provider rejections across the day); the package is small enough that a strong model might fix it from the test names alone without running anything (both tools did run the suite); hidden tests are written by the same author as the bugs and favor the reference fix's semantics; qwen's committed-harness-files artifact was scored leniently.

## Reproduce

Everything is checked in: [`2026-08-29-harness/`](./) — `swe_task.txt` (prompt), `swe_run.sh` (runner), `swe-seed/` (the seeded repo: `textlib/`, `tests/`, `README.md`), `swe-hidden-tests/` (the 18 hidden tests). Raw artifacts in `/tmp/swe-bench/` (ephemeral). To replay: `git init` a copy of `swe-seed/`, commit it as the seed, clone per tool, run, then score with the visible suite + hidden tests + `git diff <seed> HEAD -- tests/`.
