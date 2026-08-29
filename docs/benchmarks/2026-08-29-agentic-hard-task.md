# Agentic benchmark: Qwen Code 0.22.2 vs Rapid CLI (`rapid exec`) — one hard multi-part task

Date: 2026-08-29 · Host: macOS (arm64) · Model for both: `inclusionai/ling-3.0-flash-fin:free` via `https://openrouter.ai/api/v1`

Companion to [2026-08-29-qwen-code-vs-rapid.md](./2026-08-29-qwen-code-vs-rapid.md) (one-shot prompts). This run gives both tools a single hard task requiring many diverse tool actions, and every claim below was re-derived from the end state on disk by an independent verifier — the tools' own success reports were never trusted.

## Task

One prompt (byte-identical for both, `task.txt` in the harness dir): build a polyglot project in an empty scratch repo —

1. `src/primegen.py` printing the first N primes (must be correct to N=100);
2. fix a seeded off-by-one bug in an existing `tools/stats.py` (self-test fails until fixed; minimal in-place fix required);
3. `doubler/src/main.rs` (stdin stdin-integers → doubled stdout), compiled with `rustc --edition 2021 -O`;
4. `inventory.json` (exactly 5 well-formed items) + `src/report.py` printing `TOTAL: <price*qty sum>`;
5. executable `build.sh` chaining (selftest → primes piped through doubler → report), failing on any step failure;
6. run it, commit everything to git with the exact message `init: inventory project`, clean tree (artifacts gitignored).

Six subtasks, six languages/tools touched (Python, Rust, shell, JSON, filesystem, git), and one trap: the bug must actually be found by running the self-test, not guessed.

## Fairness setup

- Identical scratch dirs (`/tmp/rapid-qwen-agent/{rapid,qwen}`): same seed file bytes, `git init` + local identity pre-set (so `git commit` identity is not a failure mode), nothing else.
- rapid: `RAPIDLM_CONFIG` pinned to the same OpenRouter model, `RAPIDLM_PERMISSION_MODE=bypassPermissions`, project dir explicitly marked trusted in `~/.rapidlm/project-trust.json` (headless exec refuses all workspace tools for untrusted projects — by design, fail-closed).
- qwen: same `OPENAI_*` env, `--approval-mode=yolo` (last run's permission denials avoided; this time 0 denied calls).
- Runs were sequential to avoid free-tier rate-limit cross-talk.

## Attempts (all of them)

- rapid attempt 1: exit 1 after 5.4 s — `agent turn failed: tool_failed`, opaque cause; the planning tool call (todo_write) persisted, then a subsequent call failed with no dispatch logged. Most consistent with a malformed tool-call from the free-tier model; not confirmed.
- rapid attempt 2 (verbose, scratch dir still contained my earlier diagnostic probe files): exit 1 after 17.9 s and 44,440 tokens — `model_failed (provider rejection)`. Tools demonstrably worked (shell `cat`, selftest run) before the provider rejected a request. Run discarded; dir reset to pristine seed.
- rapid attempt 3 (clean state): **success**, exit 0, ≈38.3 s (runner-measured; `/usr/bin/time` was accidentally omitted on this attempt), 84,505 tokens, 16 model steps.
- qwen: **success on its single attempt**, exit 0, 49.74 s, 288,586 tokens (284,498 in / 4,088 out; 253,437 of the input tokens were cache reads), 10 API requests, 23 tool calls (6 write_file, 8 run_shell_command, 5 read_file, 3 todo_write, 1 edit), 0 denials.

## Independent verification (run by the harness, not the tools)

| Check | rapid | qwen |
|---|---|---|
| 1 primegen correct for N=1, 10, 100 | PASS | PASS |
| 2 stats bug actually fixed (module imported and re-checked; gutted selftest would fail this) | PASS | PASS |
| 3 doubler binary doubles `1 2 -3` → `2 4 -6` | PASS | PASS |
| 4 inventory valid (5 items, types/ranges) and report total equals harness-recomputed total | PASS (215.97) | PASS (91.50) |
| 5 build.sh executable, exit 0, shows PASS + `4 6 10 14 22` + TOTAL | PASS | PASS |
| 6 git: exact message, clean residue, all six files tracked | PASS | PASS |
| **Total** | **6/6** | **6/6** |

The seeded bug fix came out byte-identical from both tools (`s[mid + 1]` → `s[mid - 1]`); both wrote the same one-line `.gitignore` (`doubler/doubler`); both produced a single commit with the exact required message. rapid's `build.sh` uses `set -euo pipefail`, qwen's `set -e` — both satisfy the fail-on-step-failure requirement (verified by exit-code propagation through the pipeline).

## Findings

**Both tools completed a genuinely multi-step agentic task.** Six diverse subtasks, all independently verified. The one-shot token gap (29–181× in the companion benchmark) collapses to **3.4× (84.5k vs 288.6k)** here: when the work is tool-driven, both tools pay per-step input overhead, and rapid's own per-step cost (~5–7k tokens/step with tool schemas) is no longer trivial. qwen's cache reads (253k of 284k input tokens) show most of its overhead is repeated system-prompt/tool-schema text, which providers bill at cache rates even when the headline token count looks large.

**Reliability is the differentiator, not capability.** qwen finished first try; rapid needed three attempts — one opaque `tool_failed`, one provider rejection. Both failures are visible in rapid's typed failure output and non-zero exit codes; a script calling rapid would have known to retry. Also notable: a harness mistake (my diagnostic files left in rapid's scratch dir) visibly derailed attempt 2 — leftover-state sensitivity is real for both tools.

**Wall time is roughly comparable for real work** (≈38 s vs 50 s here, vs 1.8–3.4× for one-shot prompts): agent loops, not answer generation, dominate.

**Headless tool authorization:** rapid requires explicit per-project trust plus a permission mode before it will touch anything headlessly (two commands of setup, then full autonomy); qwen needs `--approval-mode=yolo`. Both are fail-closed by default, which is the right default — but benchmarkers must configure both deliberately or they measure setup errors instead of capability.

## Threats to validity

One successful run per tool (plus rapid's two failed attempts) on a free-tier endpoint; the model is small and flaky (two provider-side failures in ~1 hour); rapid attempt 3's wall time is runner-measured (time -p omitted); the task is verified by end-state checks, so mid-run wastefulness (e.g. rapid's 16 steps vs qwen's 10) is visible only in token counts; both tools saw the same prompt, but step counts differ, so per-step pricing would change the cost picture.

## Reproduce

Harness: [`2026-08-29-harness/`](./) — `task.txt` (the prompt), `agent_run.sh` (per-tool runner), `verify.py` (independent end-state verifier), plus the one-shot scripts from the companion run. Raw artifacts: `/tmp/rapid-qwen-agent/` (ephemeral).
