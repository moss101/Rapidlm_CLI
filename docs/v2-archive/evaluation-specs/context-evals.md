# Context Engine Evaluation Specification

## Gold queries

Maintain repository-specific query sets mapping a task/query to relevant files, symbols and line ranges. Include cross-repo references, renamed symbols, generated code, large monorepos and stale-index cases.

## Metrics

- Recall@5/10/20 for relevant chunks and files;
- MRR/nDCG for first useful evidence;
- redundant-token ratio after MMR;
- stale-context rate;
- context precision by token;
- retrieval/compile p50/p95 latency;
- index update latency;
- bytes/index and RAM;
- downstream task success and tokens versus `rg-only`, `vector-only`, and full-file baselines.

## Token-efficiency metric

`context_efficiency = verified_task_success / model_input_tokens`. Compare at equal task set and model. A context-engine change cannot be accepted solely because recall rises if it materially increases tokens without downstream benefit.

## Fault tests

Delete/corrupt vector index, stop LSP, rename repository root, change file without mtime resolution assumptions, create 100k watcher events, modify a previously-read range, and force token-budget pressure. Lexical/structural retrieval must remain functional.
