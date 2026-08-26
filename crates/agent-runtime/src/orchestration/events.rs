//! Orchestration events mapped onto the existing event-ledger kind registry.

use protocol::GoalId;
use serde::{Deserialize, Serialize};

/// Durable orchestration facts. Wire names match `EventKind` in event-ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationEventKind {
    TaskContractCreated,
    DiscoveryCompleted,
    PlanCreated,
    ImplementationCompleted,
    EvidenceCreated,
    CheckCompleted,
    VerificationCompleted,
    GapCreated,
    RepairStarted,
    StrategistInvoked,
    TaskVerified,
    TaskAccepted,
    TaskBlocked,
    TaskFailed,
    TaskCancelled,
}

/// Bounded event recorded by the supervisor and optionally appended to the ledger.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OrchestrationEvent {
    pub kind: OrchestrationEventKind,
    pub task_id: GoalId,
    pub round: u32,
    pub summary: String,
}

/// Host sink. Implementations must use the existing event ledger, not a second log.
pub trait OrchestrationEventSink {
    fn emit(&mut self, event: &OrchestrationEvent) -> Result<(), String>;
}

/// In-process recorder used by tests and as the supervisor's local buffer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MemoryEventSink {
    pub events: Vec<OrchestrationEvent>,
}

impl OrchestrationEventKind {
    pub const fn as_ledger_kind(self) -> &'static str {
        match self {
            Self::TaskContractCreated => "orchestration.task_contract_created",
            Self::DiscoveryCompleted => "orchestration.discovery_completed",
            Self::PlanCreated => "orchestration.plan_created",
            Self::ImplementationCompleted => "orchestration.implementation_completed",
            Self::EvidenceCreated => "orchestration.evidence_created",
            Self::CheckCompleted => "orchestration.check_completed",
            Self::VerificationCompleted => "orchestration.verification_completed",
            Self::GapCreated => "orchestration.gap_created",
            Self::RepairStarted => "orchestration.repair_started",
            Self::StrategistInvoked => "orchestration.strategist_invoked",
            Self::TaskVerified => "orchestration.task_verified",
            Self::TaskAccepted => "orchestration.task_accepted",
            Self::TaskBlocked => "orchestration.task_blocked",
            Self::TaskFailed => "orchestration.task_failed",
            Self::TaskCancelled => "orchestration.task_cancelled",
        }
    }
}

impl OrchestrationEventSink for MemoryEventSink {
    fn emit(&mut self, event: &OrchestrationEvent) -> Result<(), String> {
        self.events.push(event.clone());
        Ok(())
    }
}
