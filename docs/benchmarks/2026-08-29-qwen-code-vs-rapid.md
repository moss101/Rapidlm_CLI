# Benchmark: Qwen Code 0.22.2 vs Rapid CLI (`rapid exec`) — re-run

Date: 2026-08-29 · Host: macOS (arm64) · Model for both: `inclusionai/ling-3.0-flash-fin:free` via `https://openrouter.ai/api/v1`

Re-run of the [2026-08-28 benchmark](./2026-08-28-qwen-code-vs-rapid.md) with the same four prompts, the same invocations, and the same evaluation pipeline. One config change since the previous run: `~/.rapidlm/config.toml` now defaults rapid to a different provider, so rapid was pinned to the `openrouter` entry via `RAPIDLM_CONFIG` to keep the same-model comparison.

## Method

Identical to the previous run. Four byte-identical prompts, one run each, in clean scratch directories:

1. **p1 — generate**: Rust program printing balanced-bracket results for four fixed strings.
2. **p2 — debug**: find and fix a seeded bug in a Python binary search, print four probe results.
3. **p3 — explain**: what `Send`/`Sync` mean, at most 3 sentences.
4. **p4 — error handling**: `parse_port(s) -> Result<u32, String>` with empty / non-numeric / valid / out-of-range inputs and categorized reasons.

Invocations:

- rapid: `RAPIDLM_CONFIG=<openrouter config> rapid exec "<prompt>"` (one-turn completion, no tools). Token total is the provider-reported sum across the turn's model steps, printed by the CLI as `tokens used: N` on stderr.
- qwen: `OPENAI_API_KEY=… OPENAI_BASE_URL=https://openrouter.ai/api/v1 OPENAI_MODEL=inclusionai/ling-3.0-flash-fin:free qwen -p "<prompt>" -o json` (agentic mode). Tokens are the JSON `result` event's `usage`; request counts from `stats.models[*].api.totalRequests`.

Wall time via `/usr/bin/time -p`. Generated code was extracted from each tool's answer (qwen's file writes and shell commands were permission-denied, as in the previous run, so answers came from result text) and evaluated identically: `rustc --edition 2021 -D warnings` + execute, or `python3` + execute, compared against expected outputs.

## Results

| Prompt | rapid wall | rapid tokens | qwen wall | qwen tokens in/out/total | qwen API requests |
|---|---|---|---|---|---|
| p1 generate | 5.06 s | 749 | 17.30 s | 62,782 / 1,404 / 64,186 | 3 |
| p2 debug | 13.74 s | 2,287 | 21.26 s | 64,494 / 1,781 / 66,275 | 3 |
| p3 explain | 2.14 s | 304 | 6.43 s | 27,761 / 246 / 28,007 | 2 (1 main + 1 memory-extractor) |
| p4 errors | 6.78 s | 599 | 22.68 s | 106,590 / 1,691 / 108,281 | 5 |

Baseline probe (`Reply with exactly one word: pong`): rapid 322 tokens in 1.96 s; qwen 69,228 tokens in 38.07 s across 6 API requests — only 1 was the main completion, the other 5 came from a `managed-auto-memory-extractor` subsystem making its own calls.

## Findings

**Completion time.** rapid was faster on all four prompts, this time by 1.8×–3.4× (5.06–13.74 s vs 6.43–22.68 s). Every rapid run also stayed inside one API request.

**Code quality.** With the same model, outputs were again equivalent; all six executable checks passed:

- p1: both correct, both compile clean under `-D warnings`, exact expected output `true false true false`. rapid's stack-based solution was clean this time (`Vec`, no `unreachable!()` arm — last run's wart did not recur); qwen again reached for `VecDeque` where `Vec` suffices.
- p2: both produced byte-identical correct fixes (`return lo` → `return -1`), exact probe output `3 0 5 -1` — same sampling coincidence as last run.
- p4: functionally identical, same categorized messages in the same order, exact expected output.
- p3: both accurate and within the 3-sentence bound (rapid used 3 sentences, qwen 2).

**Token consumption.** rapid used 304–2,287 tokens; qwen used 28k–108k total — 29×–181× more (probe: 215×). As before, the dominant factor is qwen's per-request baseline (~20.5k input tokens for system prompt plus tool schemas) multiplied by its multi-request loop. New observation this run: much of qwen's input is cache reads (p4: 84,473 of 106,590 input tokens were `cache_read_input_tokens`), and its auto-memory-extractor issues extra full API requests even when the task needs none (probe: 5 of 6 requests).

**Error handling.** Matrix over three failure modes (bad API key, unreachable base URL, unknown model id):

| Case | rapid | qwen |
|---|---|---|
| Bad key | exit 1, `agent turn failed: model_failed (authentication; check the configured API credential)` | **exit 0**, `subtype: success`, `is_error: false`; error only in result text: `[API Error: 401 Missing Authentication header]` |
| Unreachable URL | exit 1, `agent turn failed: model_failed (connection; check network reachability and the configured base_url)` | **exit 0**, result text `[API Error: Connection error. (cause: fetch failed)]` |
| Malformed model id | exit 1 **before any network call**, precise typed message naming the invalid field and allowed alphabet | **exit 0**, result text `[API Error: 400 … is not a valid model ID]` (after a round trip) |

Improvement over the previous run: rapid's runtime failure messages are no longer opaque (`agent turn failed: failed`); they now carry a typed cause — authentication vs connection — making them diagnosable from the CLI alone. The malformed-model case was already precise preflight validation and is unchanged. qwen's behavior is unchanged: exit 0 and `is_error: false` on every failure mode, so callers must string-match result text. Neither tool echoed the API key, and neither panicked.

## Comparison with the 2026-08-28 run

| Metric | 2026-08-28 | 2026-08-29 |
|---|---|---|
| rapid wall (p1–p4) | 7.91 / 12.43 / 4.99 / 8.69 s | 5.06 / 13.74 / 2.14 / 6.78 s |
| qwen wall (p1–p4) | 27.78 / 12.01 / 34.74 / 13.12 s | 17.30 / 21.26 / 6.43 / 22.68 s |
| rapid tokens (p1–p4) | 1,289 / 2,193 / 724 / 1,481 | 749 / 2,287 / 304 / 599 |
| qwen total tokens (p1–p4) | 160k / 65k / 75k / 64k | 64k / 66k / 28k / 108k |
| qwen API requests (p1–p4) | 8 / 3 / 6 / 3 | 3 / 3 / 2 / 5 |
| Transient failures | 1 (p3 rapid, retried OK) | 0 |

The headline conclusion is unchanged: same-model answers are equivalent, rapid is faster and one order of magnitude cheaper in tokens for one-shot tasks, and only rapid is scriptable (typed failures, non-zero exit). The per-prompt numbers are not stable for qwen — its wall time and request count swing with the agent loop (p3: 34.7 s → 6.4 s; p4: 13.1 s → 22.7 s) — so treat any single-qwen-run figure as one sample of a high-variance distribution.

## Threats to validity

One run per prompt on a free-tier endpoint; qwen figures include its agent scaffolding by design; prompt set is small and code-only; model behavior is stochastic (p2's byte-identical fixes recurred but remain a coincidence, not a guarantee); qwen's memory-extractor traffic varies per run and is included in its totals.

## Reproduce

Harness scripts are checked in this time (the previous run's lived in `/tmp` and were ephemeral): [`2026-08-29-harness/`](./2026-08-29-harness/) — `run_all.sh` (prompt pairs + probe), `run_errors.sh` (error matrix), `eval.py` (extraction/compile/check), `summarize.py` (tokens/wall table). Raw outputs from this run remain in `/tmp/rapid-qwen-bench/`.
