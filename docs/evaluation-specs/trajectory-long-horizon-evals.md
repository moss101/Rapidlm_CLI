# Trajectory Learning and Long-Horizon Evaluation Specification

## Objectives

Measure whether RapidLM remains correct, efficient and recoverable over hours rather than only short benchmark tasks.

## Endurance tiers

| Tier | Minimum wall time | Minimum tool calls | Required injected faults |
|---|---:|---:|---|
| E1 | 1h | 100 | one compaction + provider retry |
| E2 | 4h | 300 | daemon restart + background-agent restart |
| E3 | 12h | 700 | remote handoff + sandbox restart + flaky tests |
| E4 | 24h | 1,000 | all above + multiple compactions/index refreshes |

## Hard gates

- zero policy bypasses;
- no required goal criterion forgotten or silently removed;
- no completion while required evidence missing;
- session projection/replay integrity after each restart;
- no two write owners across handoff;
- secrets absent from trajectory export.

## Efficiency metrics

- repeated unchanged read tokens over time;
- context tokens / useful evidence produced;
- tool retry amplification;
- agent spawn/churn;
- idle worker minutes;
- cost slope per hour;
- compaction-induced rediscovery;
- goal drift events;
- Computer Use semantic-target degradation over time.

## Trajectory ranking

Only runs that satisfy all hard gates can enter candidate ranking. Rank by multi-objective reward; retain baseline and candidate raw metric distributions.
