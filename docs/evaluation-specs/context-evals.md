# Context Engineering Evals

Corpus includes symbol lookup, architecture exploration, exhaustive caller/test enumeration, zero-hit negatives, multi-repo scope, stale post-write data, huge/minified files, Unicode/path aliases, generated/vendor code. Metrics: evidence recall, precision, exhaustive-reference recall, false negative claim, duplicate/stale token rate, read repetition, token budget, compile latency, visibility-dedup correctness, post-write invalidation, tokens per verified success.
## Common gate
Run against production contracts. Prefer deterministic assertions before model judges. Record exact fixture/version/model/prompt/context/tool schemas. A security/data-loss/duplicate-effect/false-completion regression is a hard failure regardless of aggregate score.
