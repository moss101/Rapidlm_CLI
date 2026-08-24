# Agent Harness Evals

Scripted scenarios cover clean TaskEnvelope, no parent-transcript leakage, role tool projections, read-only specialists, isolated writer views, delegation utility, cancellation/timeouts, empty response recovery, repeated-call/message loop guards, compact-before-retry and model route/fallback. Compare same model with/without harness features to quantify verified success per token.
## Common gate
Run against production contracts. Prefer deterministic assertions before model judges. Record exact fixture/version/model/prompt/context/tool schemas. A security/data-loss/duplicate-effect/false-completion regression is a hard failure regardless of aggregate score.
