# Credentials Request — live comparative evaluation (goal §7)

`rapid eval --live` is implemented and dry-run validated (120 skips recorded with
the reason "model not usable"). To produce the actual comparative results the
goal requires, the live runs need one real model credential. This document is
the explicit request the goal calls for; until it is granted, the benchmark
reports skips, not numbers, and no comparison is claimed.

## What is requested

One of:

1. **An OpenAI-compatible endpoint + API key** (preferred — zero harness
   changes): set `OPENAI_API_KEY` (and optionally `OPENAI_BASE_URL`) in the
   environment `rapid eval --live` runs in, or grant a key usable against
   `https://api.openai.com/v1` with the model `gpt-4.1-mini`-class.

2. **A concrete spending limit** against the same endpoint: the harness runs
   each task once per agent, 40 tasks, single turn per task with tool calls —
   the offline gold trajectories measure ~10–20 tool calls per task, so a
   ceiling of **$20 per agent** (≈ 40 tasks × ≤ 500 tokens input / ≤ 1000
   output × $2.5/1M-class pricing) bounds the total spend at **≤ $60 for all
   three agents**, hard-enforced by `--max-wall-time` and per-run token
   ceilings already in the harness.

## What it unlocks

- `rapid eval --live --suite eval/suite` produces the comparative report:
  per-agent passed/failed/skipped, wall time, and tokens per task
  (`eval/results/live-*.json`), with identical repos, prompts, and judge.
- The pinned-competitor requirement is satisfied by recording the competitor
  CLI versions (`qwen --version`, `grok --version`) in the report; the harness
  already refuses to compare unpinned binaries.

## What stays true without it

- The offline mode (40/40) remains the mechanical validation; live numbers are
  additive and never replace it.
- All 120 live slots are recorded as `skipped: model not usable` — the report
  says exactly why nothing ran.
