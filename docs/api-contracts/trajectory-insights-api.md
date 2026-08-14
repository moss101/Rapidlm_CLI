# Trajectory and Session Insights API Contract

```rust
pub struct TrajectoryExportRequest {
    pub source: SourceRef,
    pub purpose: TrajectoryPurpose, // Replay | Eval | TrainingCandidate
    pub data_policy: DataPolicyRef,
}

pub struct RewardVector {
    pub correctness: Score,
    pub verification: Score,
    pub security: Score,
    pub patch_quality: Score,
    pub context_precision: Score,
    pub token_efficiency: Score,
    pub tool_efficiency: Score,
    pub latency: Score,
    pub autonomy: Score,
    pub cost_efficiency: Score,
}

pub struct SessionInsightReport {
    pub session_id: SessionId,
    pub outcome: OutcomeLabel,
    pub findings: Vec<InsightFinding>,
    pub improvement_candidates: Vec<ImprovementCandidateRef>,
    pub supporting_events: Vec<EventRef>,
    pub generated_at: DateTime<Utc>,
}
```

No API field requests or stores hidden chain-of-thought. Observable model messages are subject to provider/user data policy. Missing privacy classification causes export failure.
