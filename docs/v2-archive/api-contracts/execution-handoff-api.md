# Execution Handoff API Contract

```rust
pub struct PrepareHandoff {
    pub session_id: SessionId,
    pub target: WorkerSelector,
    pub auto_resume: bool,
    pub include_uncommitted: bool,
}

pub struct HandoffBundleHeader {
    pub schema_version: u32,
    pub handoff_id: HandoffId,
    pub session_id: SessionId,
    pub source_generation: u64,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub manifest_digest: Digest,
    pub signature: Signature,
}

pub struct RestorePlan {
    pub restore_id: RestoreId,
    pub target_worker: WorkerId,
    pub required_downloads: Vec<ArtifactRef>,
    pub policy_gaps: Vec<CapabilityIntent>,
    pub compatibility: CompatibilityResult,
}
```

Protocol states:

`requested → source_parking → bundle_ready → target_restoring → target_ready → lease_committed → complete`

Failure/abort states are durable events. CapabilityLeases and secret values never cross the boundary. Target always re-evaluates policy and creates a new Computer Use Observation before any UI action.
