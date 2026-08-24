# Graph Runtime Evals

Scenarios: deterministic ready-set, cycle rejection, joins/barriers/shards, revision conflict, retry attempt history, localized replan, cancellation propagation, wait/resume, crash at every node transition, resource starvation/fairness, 10k/100k-node replay, invalidation propagation. Properties: historical revisions immutable; no node becomes READY with unsatisfied hard dependency; uncertain external effects never auto-replay.
## Common gate
Run against production contracts. Prefer deterministic assertions before model judges. Record exact fixture/version/model/prompt/context/tool schemas. A security/data-loss/duplicate-effect/false-completion regression is a hard failure regardless of aggregate score.
