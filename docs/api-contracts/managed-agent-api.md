# Managed Agent / AgentPool API Contract

```rust
pub struct TaskEnvelope {
    pub task_id: TaskId,
    pub parent_agent_id: AgentId,
    pub goal_node_id: GoalNodeId,
    pub objective: String,
    pub acceptance_criteria: Vec<CriterionRef>,
    pub context_packet: ContextPacketRef,
    pub knowledge_refs: Vec<KnowledgeId>,
    pub workspace_access: WorkspaceAccess,
    pub capability_ceiling: CapabilitySet,
    pub model_policy: ModelPolicyRef,
    pub budget: AgentBudget,
    pub result_schema: ResultSchema,
}

pub struct AgentResult {
    pub agent_id: AgentId,
    pub status: AgentTerminalStatus,
    pub summary: String,
    pub evidence: Vec<EvidenceId>,
    pub changeset: Option<ChangeSetRef>,
    pub artifacts: Vec<ArtifactRef>,
    pub blockers: Vec<Blocker>,
    pub usage: AgentUsage,
    pub trajectory_summary: Option<ArtifactRef>,
}

pub struct AgentMessage {
    pub message_id: MessageId,
    pub from: AgentId,
    pub to: AgentRecipient,
    pub topic: MailboxTopic,
    pub body: String,
    pub evidence: Vec<EvidenceId>,
    pub artifacts: Vec<ArtifactRef>,
    pub freshness: DateTime<Utc>,
}
```

Rules:

- child context is created from `TaskEnvelope`; raw parent transcript cloning is not an API;
- write-capable sibling agents must have distinct writable WorkspaceViews;
- message bodies are bounded; large payloads use ArtifactRef;
- messages/results cannot contain transferable CapabilityLeases;
- `AgentResult` cannot mutate top-level goal state; coordinator/verifier does so through Goal API.
