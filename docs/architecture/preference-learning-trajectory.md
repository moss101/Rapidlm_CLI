# Architecture — Preference Learning, Trajectory Optimization and Experiment Promotion

## 1. Responsibility
Learn soft user/team/repo tendencies from observable feedback and use production trajectories to scientifically improve prompts/context/router/harness.

## 2. Non-negotiable design rules
- No hidden chain-of-thought collection is required.
- Inferred preferences remain soft, scoped, provenance-bearing and decaying.
- Production behavior changes only through versioned experiment promotion.

## 3. Components
- **FeedbackCollector** — accept/reject/edit/review events
- **SignalExtractor** — candidate preference propositions
- **PreferenceStore** — scope/confidence/support/contradiction/decay
- **TrajectoryStore** — observable run records
- **ExperimentRegistry** — candidate/baseline/held-out promotion

## 4. Canonical contracts
`PreferenceCandidate`, `Preference`, `TrainingTrajectory`, `Experiment`, `PromotionDecision`.

## 5. Failure and recovery
Contradictory signals reduce confidence rather than overwrite history. Model-assisted extraction outputs candidates requiring schema validation. Failed experiments remain historical.

## 6. Security and trust
Data boundary/classification controls export and retention. Sensitive/secret content excluded or redacted. Preference can never grant capability or override rules.

## 7. Implementation notes
Evaluate user/team preference benefits separately to prevent overfitting; use minimum-support thresholds and time decay.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
