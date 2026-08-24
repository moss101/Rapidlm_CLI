# Tool Contract and Repair Evals

For each model×tool schema test valid calls unchanged, optional-null, stringified collections, singleton collection, safe scalar coercions, aliases, nested issue paths, ambiguous/destructive fields, relational invariant violations and repeated repair loops. Required metric: valid-input mutation rate = zero. Measure invalid-call rate, repair success, extra turns, semantic corruption escape and model improvement after repair feedback.
## Common gate
Run against production contracts. Prefer deterministic assertions before model judges. Record exact fixture/version/model/prompt/context/tool schemas. A security/data-loss/duplicate-effect/false-completion regression is a hard failure regardless of aggregate score.
