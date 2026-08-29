//! Typed Runtime Graph node/edge kinds and states.

use serde::{Deserialize, Serialize};

/// Catalog node kinds (`docs/reference/graph-node-edge-catalog.md`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Goal,
    Criterion,
    Plan,
    Task,
    Agent,
    ContextQuery,
    ContextPacket,
    ToolInvocation,
    Process,
    Monitor,
    ResourceAcquire,
    WorkspaceTransaction,
    ComputerObservation,
    ComputerAction,
    Preview,
    Approval,
    AskUser,
    Trigger,
    Artifact,
    Claim,
    Evidence,
    Verification,
    Handoff,
    HumanControl,
    Join,
    Barrier,
}

/// Host-owned node lifecycle. Models cannot set this directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Pending,
    Ready,
    Running,
    Waiting,
    Blocked,
    /// Explicitly suspended by the host or a human, distinct from `Waiting`
    /// (blocked on an external dependency) and `Cancelled` (terminal, never
    /// resumes): a `Paused` node keeps its progress and resumes back to
    /// `Running` on an explicit [`crate::service::GraphService::resume`]
    /// call. Never entered by the scheduler itself.
    Paused,
    Succeeded,
    Failed,
    Cancelled,
    Superseded,
    Invalidated,
}

/// Catalog edge kinds. Executable `DependsOn` edges must stay acyclic.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    DecomposesInto,
    DependsOn,
    Blocks,
    ScheduledAfter,
    JoinsAt,
    ProvidesContextTo,
    Reads,
    Writes,
    Mutates,
    Produces,
    Supports,
    Contradicts,
    Verifies,
    RequiresApproval,
    DelegatedTo,
    Supersedes,
    Invalidates,
    TriggeredBy,
}

impl NodeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Goal => "goal",
            Self::Criterion => "criterion",
            Self::Plan => "plan",
            Self::Task => "task",
            Self::Agent => "agent",
            Self::ContextQuery => "context_query",
            Self::ContextPacket => "context_packet",
            Self::ToolInvocation => "tool_invocation",
            Self::Process => "process",
            Self::Monitor => "monitor",
            Self::ResourceAcquire => "resource_acquire",
            Self::WorkspaceTransaction => "workspace_transaction",
            Self::ComputerObservation => "computer_observation",
            Self::ComputerAction => "computer_action",
            Self::Preview => "preview",
            Self::Approval => "approval",
            Self::AskUser => "ask_user",
            Self::Trigger => "trigger",
            Self::Artifact => "artifact",
            Self::Claim => "claim",
            Self::Evidence => "evidence",
            Self::Verification => "verification",
            Self::Handoff => "handoff",
            Self::HumanControl => "human_control",
            Self::Join => "join",
            Self::Barrier => "barrier",
        }
    }
}

impl NodeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Blocked => "blocked",
            Self::Paused => "paused",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
            Self::Invalidated => "invalidated",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Superseded | Self::Invalidated
        )
    }
}

impl EdgeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DecomposesInto => "decomposes_into",
            Self::DependsOn => "depends_on",
            Self::Blocks => "blocks",
            Self::ScheduledAfter => "scheduled_after",
            Self::JoinsAt => "joins_at",
            Self::ProvidesContextTo => "provides_context_to",
            Self::Reads => "reads",
            Self::Writes => "writes",
            Self::Mutates => "mutates",
            Self::Produces => "produces",
            Self::Supports => "supports",
            Self::Contradicts => "contradicts",
            Self::Verifies => "verifies",
            Self::RequiresApproval => "requires_approval",
            Self::DelegatedTo => "delegated_to",
            Self::Supersedes => "supersedes",
            Self::Invalidates => "invalidates",
            Self::TriggeredBy => "triggered_by",
        }
    }

    pub const fn is_executable_dependency(self) -> bool {
        matches!(
            self,
            Self::DependsOn | Self::ScheduledAfter | Self::JoinsAt | Self::DecomposesInto
        )
    }
}
