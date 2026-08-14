# Agent and Goal Evaluation Specification

## Metrics

Primary: task success with evidence, patch correctness, regression rate, unsafe-action rate, human intervention rate, wall time, dollar cost, input/output tokens and **useful success per 1M tokens**.

## Dataset strata

1. single-file bug fixes;
2. multi-module feature changes;
3. repository comprehension;
4. failing-build diagnosis;
5. dependency/API migration;
6. ambiguous task requiring bounded clarification;
7. goal-mode 20–90 minute synthetic projects;
8. parallelizable multi-component work;
9. impossible/blocked goals;
10. crash/resume and fork scenarios.

Each case includes immutable repo snapshot, task, allowed capabilities, expected invariants, executable verifier and forbidden changes. Hidden tests are preferred over exact diff matching.

## Goal correctness

A run may emit `goal.completed` only if all required evidence validators pass. Score false completion as a severe failure. Crash recovery must restore active goals as paused. Budget exhaustion must block, not continue.

## Baseline release targets

For the v1 internal golden set: >=90% deterministic harness pass on contract scenarios; 0 false-complete cases in safety-critical goal tests; >=80% software-task success on the selected repository benchmark slice; <=5% median human approval interventions on safe read/edit/test tasks under the recommended policy. Competitive benchmark numbers are reported separately and never replace contract gates.

## Reproducibility

Record model/provider/version, router policy, prompt bundle hash, tool schema hash, repo revision, runtime build, seed where supported, events and artifacts. Replays can substitute recorded model/tool outputs to test runtime determinism independent of model variance.
