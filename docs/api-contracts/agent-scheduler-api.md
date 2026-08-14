# Agent Scheduler Contract

```rust
pub struct SpawnAgent {
    pub parent: AgentId,
    pub role: AgentRole,
    pub task: String,
    pub access: WorkspaceAccess, // ReadOnly | WriteIsolated
    pub budget: AgentBudget,
    pub model_policy: ModelPolicyRef,
    pub expected_result: ResultSchema,
}

pub struct AgentResult {
    pub agent_id: AgentId,
    pub status: AgentTerminalStatus,
    pub summary: String,
    pub evidence: Vec<EvidenceId>,
    pub workspace_view: Option<WorkspaceViewId>,
    pub patch_summary: Option<PatchSummary>,
    pub artifacts: Vec<ArtifactRef>,
}
```

Scheduler enforces global, per-provider and write-agent concurrency. Write-capable siblings never share a writable view. Cancellation propagates parent→child unless child was explicitly detached as a daemon job. Main-goal lifecycle mutations from subagents are rejected by the goal service.
