# Benchmark: Qwen Code 0.22.2 vs Rapid CLI (`rapid exec`)

Date: 2026-08-28 · Host: macOS (arm64) · Model for both: `inclusionai/ling-3.0-flash-fin:free` via `https://openrouter.ai/api/v1`

## Method

Four prompts, byte-identical for both tools, run once each in clean scratch directories:

1. **p1 — generate**: Rust program printing balanced-bracket results for four fixed strings.
2. **p2 — debug**: find and fix a seeded bug in a Python binary search, print four probe results.
3. **p3 — explain**: what `Send`/`Sync` mean, at most 3 sentences.
4. **p4 — error handling**: `parse_port(s) -> Result<u32, String>` with empty / non-numeric / valid / out-of-range inputs and categorized reasons.

Invocations:

- rapid: `rapid exec "<prompt>"` (one-turn completion, no tools). Token total is the provider-reported sum across the turn's model steps, printed by the CLI as `tokens used: N` on stderr (added for this benchmark; stdout stays answer-only).
- qwen: `OPENAI_API_KEY=… OPENAI_BASE_URL=https://openrouter.ai/api/v1 OPENAI_MODEL=inclusionai/ling-3.0-flash-fin:free qwen -p "<prompt>" -o json` (agentic mode; may issue multiple API requests per prompt). Tokens are the JSON `result` event's `usage`.

Wall time via `/usr/bin/time -p`. Generated code was extracted from each tool's answer (qwen emitted fenced code in all cases; it wrote no files) and evaluated identically: `rustc --edition 2021 -D warnings` + execute, or `python3` + execute, compared against expected outputs.

## Results

| Prompt | rapid wall | rapid tokens | qwen wall | qwen tokens in/out/total | qwen API requests |
|---|---|---|---|---|---|
| p1 generate | 7.91 s | 1,289 | 27.78 s | 157,143 / 2,777 / 159,920 | 8 (7 turns) |
| p2 debug | 12.43 s | 2,193 | 12.01 s | 63,609 / 1,200 / 64,809 | 3 |
| p3 explain | 4.99 s * | 724 | 34.74 s | 70,636 / 4,643 / 75,279 | 6 |
| p4 errors | 8.69 s | 1,481 | 13.12 s | 62,980 / 1,292 / 64,272 | 3 |

\* p3-rapid failed once with a transient provider error (typed `Failed` status, exit 1, 0.46 s) and succeeded on an immediate retry; the failure is counted in the error-handling discussion below.

Baseline probe (`Reply with exactly one word: pong`): rapid 206 tokens total; qwen 28,133 tokens total across 2 API requests.

## Findings

**Completion time.** rapid was faster on all four prompts (4.99–12.43 s vs 12.01–34.74 s). qwen's time scales with its agent loop: p1 took 8 API requests even though the task needs one completion. For one-shot prompts the gap is 1.7×–7×.

**Code quality.** With the same model, outputs were equivalent; all six executable checks passed:

- p1: both correct, both compile clean under `-D warnings`, exact expected output `true false true false`. Style warts on both sides: rapid used an `unreachable!()` arm; qwen used a `VecDeque` where `Vec` suffices.
- p2: both produced byte-identical correct fixes (`return lo` → `return -1`), exact probe output `3 0 5 -1`.
- p4: functionally identical (empty → non-numeric → range checks, same messages), exact expected output.
- p3: both accurate and within the 3-sentence bound; qwen's answer adds markdown emphasis and a note about automatic trait derivation.

**Token consumption.** For identical tasks rapid used 724–2,193 tokens; qwen used 64k–160k — roughly 40–90× more. The dominant factor is qwen's per-request baseline: its system prompt plus tool schemas costs ~20–25k input tokens on every request, multiplied by its multi-request agent loop. Neither tool's *answer* tokens differ much (qwen's output tokens: 1.2k–4.6k). If the job is a single completion, the agent scaffolding is pure overhead; its value (file edits, tool use) went unused in these tasks.

**Error handling.** Matrix over three failure modes (bad API key, unreachable base URL, unknown model id):

| Case | rapid | qwen |
|---|---|---|
| Bad key | exit 1, stderr `agent turn failed: failed` | **exit 0**, JSON says `subtype: success`, `is_error: false`; error only in result text: `[API Error: 401 Missing Authentication header]` |
| Unreachable URL | exit 1, instant typed failure | **exit 0**, result text `[API Error: Connection error. (cause: fetch failed)]` |
| Malformed model id | exit 1 **before any network call**, precise typed message naming the invalid field and allowed alphabet | **exit 0**, result text `[API Error: 400 … is not a valid model ID]` (after a round trip) |

Neither tool echoed the API key in any output, and neither panicked.

Assessment: rapid is automation-safe (non-zero exit on every failure, fail-closed, config validated preflight) but its runtime failure message is opaque — `agent turn failed: failed` does not say *why* (the p3 transient would have been diagnosable only from the provider side). qwen's inline messages carry useful status codes, but a shell script cannot detect the failure: exit code is 0 and even `is_error` stays `false`, so callers must string-match the result text.

## Threats to validity

One run per prompt on a free-tier endpoint (p3 shows transients happen); qwen figures include its agent scaffolding by design; prompt set is small and code-only; model behavior is stochastic (p2's byte-identical fixes are a sampling coincidence, not a guarantee).

## Reproduce

Harness scripts and raw outputs: `/tmp/rapid-qwen-bench/` (ephemeral). `run_all.sh` runs the prompt pairs, `run_errors.sh` the error matrix, `eval.py` the extraction/compile/check pipeline.
