# Performance and Token Efficiency Evals

Benchmark first frame, local kernel dispatch, lexical/symbol retrieval, hybrid ContextPacket compile, graph scheduling, 10k event replay, TUI sustained stream, process output spooling and resource acquisition. Token metrics: tool schema tokens, duplicated context, repeated reads, model turns, repair turns, verifier tokens and total tokens per verified success. Compare cold/warm and local/cloud model classes.
## Common gate
Run against production contracts. Prefer deterministic assertions before model judges. Record exact fixture/version/model/prompt/context/tool schemas. A security/data-loss/duplicate-effect/false-completion regression is a hard failure regardless of aggregate score.
