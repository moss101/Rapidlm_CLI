//! Explicit orchestration state machine. Invalid edges fail closed.

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Host-owned orchestration lifecycle. Wire form is snake_case.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OrchestrationState {
    Created,
    Contracting,
    Discovering,
    Planning,
    Retrieving,
    ReadyToImplement,
    Implementing,
    CollectingEvidence,
    RunningChecks,
    AwaitingVerification,
    Verifying,
    Refuted,
    Repairing,
    Strategizing,
    Reverifying,
    Verified,
    Accepted,
    Blocked,
    Failed,
    Cancelled,
}

/// Named transition applied by the host, never by model text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum OrchestrationTransition {
    BeginContract,
    BeginDiscovery,
    BeginPlanning,
    BeginRetrieval,
    MarkReady,
    BeginImplementation,
    CollectEvidence,
    RunChecks,
    AwaitVerification,
    BeginVerification,
    MarkVerified,
    MarkRefuted,
    BeginRepair,
    BeginStrategist,
    BeginReverification,
    Accept,
    Block,
    Fail,
    Cancel,
}

/// Why a transition was rejected. Display never echoes task text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TransitionError {
    InvalidTransition,
    Terminal,
}

impl OrchestrationState {
    pub const ALL: &'static [Self] = &[
        Self::Created,
        Self::Contracting,
        Self::Discovering,
        Self::Planning,
        Self::Retrieving,
        Self::ReadyToImplement,
        Self::Implementing,
        Self::CollectingEvidence,
        Self::RunningChecks,
        Self::AwaitingVerification,
        Self::Verifying,
        Self::Refuted,
        Self::Repairing,
        Self::Strategizing,
        Self::Reverifying,
        Self::Verified,
        Self::Accepted,
        Self::Blocked,
        Self::Failed,
        Self::Cancelled,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Contracting => "contracting",
            Self::Discovering => "discovering",
            Self::Planning => "planning",
            Self::Retrieving => "retrieving",
            Self::ReadyToImplement => "ready_to_implement",
            Self::Implementing => "implementing",
            Self::CollectingEvidence => "collecting_evidence",
            Self::RunningChecks => "running_checks",
            Self::AwaitingVerification => "awaiting_verification",
            Self::Verifying => "verifying",
            Self::Refuted => "refuted",
            Self::Repairing => "repairing",
            Self::Strategizing => "strategizing",
            Self::Reverifying => "reverifying",
            Self::Verified => "verified",
            Self::Accepted => "accepted",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Accepted | Self::Blocked | Self::Failed | Self::Cancelled
        )
    }
}

impl OrchestrationTransition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BeginContract => "begin_contract",
            Self::BeginDiscovery => "begin_discovery",
            Self::BeginPlanning => "begin_planning",
            Self::BeginRetrieval => "begin_retrieval",
            Self::MarkReady => "mark_ready",
            Self::BeginImplementation => "begin_implementation",
            Self::CollectEvidence => "collect_evidence",
            Self::RunChecks => "run_checks",
            Self::AwaitVerification => "await_verification",
            Self::BeginVerification => "begin_verification",
            Self::MarkVerified => "mark_verified",
            Self::MarkRefuted => "mark_refuted",
            Self::BeginRepair => "begin_repair",
            Self::BeginStrategist => "begin_strategist",
            Self::BeginReverification => "begin_reverification",
            Self::Accept => "accept",
            Self::Block => "block",
            Self::Fail => "fail",
            Self::Cancel => "cancel",
        }
    }
}

/// Validate `from --transition--> to`. Does not apply the change.
pub fn validate_transition(
    from: OrchestrationState,
    transition: OrchestrationTransition,
) -> Result<OrchestrationState, TransitionError> {
    if from.is_terminal() && transition != OrchestrationTransition::Cancel {
        return Err(TransitionError::Terminal);
    }
    let to = match (from, transition) {
        (OrchestrationState::Created, OrchestrationTransition::BeginContract) => {
            OrchestrationState::Contracting
        }
        (OrchestrationState::Contracting, OrchestrationTransition::BeginDiscovery) => {
            OrchestrationState::Discovering
        }
        (OrchestrationState::Contracting, OrchestrationTransition::BeginPlanning) => {
            OrchestrationState::Planning
        }
        (OrchestrationState::Discovering, OrchestrationTransition::BeginPlanning) => {
            OrchestrationState::Planning
        }
        (OrchestrationState::Discovering, OrchestrationTransition::BeginRetrieval) => {
            OrchestrationState::Retrieving
        }
        (OrchestrationState::Planning, OrchestrationTransition::BeginRetrieval) => {
            OrchestrationState::Retrieving
        }
        (OrchestrationState::Planning, OrchestrationTransition::MarkReady) => {
            OrchestrationState::ReadyToImplement
        }
        (OrchestrationState::Retrieving, OrchestrationTransition::MarkReady) => {
            OrchestrationState::ReadyToImplement
        }
        (OrchestrationState::ReadyToImplement, OrchestrationTransition::BeginImplementation) => {
            OrchestrationState::Implementing
        }
        (OrchestrationState::Implementing, OrchestrationTransition::CollectEvidence) => {
            OrchestrationState::CollectingEvidence
        }
        (OrchestrationState::Repairing, OrchestrationTransition::CollectEvidence) => {
            OrchestrationState::CollectingEvidence
        }
        (OrchestrationState::CollectingEvidence, OrchestrationTransition::RunChecks) => {
            OrchestrationState::RunningChecks
        }
        (OrchestrationState::RunningChecks, OrchestrationTransition::AwaitVerification) => {
            OrchestrationState::AwaitingVerification
        }
        (OrchestrationState::AwaitingVerification, OrchestrationTransition::BeginVerification) => {
            OrchestrationState::Verifying
        }
        (
            OrchestrationState::Verifying | OrchestrationState::Reverifying,
            OrchestrationTransition::MarkVerified,
        ) => OrchestrationState::Verified,
        (
            OrchestrationState::Verifying | OrchestrationState::Reverifying,
            OrchestrationTransition::MarkRefuted,
        ) => OrchestrationState::Refuted,
        (OrchestrationState::Refuted, OrchestrationTransition::BeginRepair) => {
            OrchestrationState::Repairing
        }
        (
            OrchestrationState::Refuted | OrchestrationState::Repairing,
            OrchestrationTransition::BeginStrategist,
        ) => OrchestrationState::Strategizing,
        (OrchestrationState::Strategizing, OrchestrationTransition::BeginRepair) => {
            OrchestrationState::Repairing
        }
        (OrchestrationState::Strategizing, OrchestrationTransition::MarkReady) => {
            OrchestrationState::ReadyToImplement
        }
        (OrchestrationState::Verified, OrchestrationTransition::Accept) => {
            OrchestrationState::Accepted
        }
        (
            OrchestrationState::AwaitingVerification,
            OrchestrationTransition::BeginReverification,
        ) => OrchestrationState::Reverifying,
        (_, OrchestrationTransition::Block) if !from.is_terminal() => OrchestrationState::Blocked,
        (_, OrchestrationTransition::Fail) if !from.is_terminal() => OrchestrationState::Failed,
        (_, OrchestrationTransition::Cancel) if !from.is_terminal() => {
            OrchestrationState::Cancelled
        }
        _ => return Err(TransitionError::InvalidTransition),
    };
    Ok(to)
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidTransition => "invalid orchestration transition",
            Self::Terminal => "orchestration task is terminal",
        })
    }
}

impl Error for TransitionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implementer_claim_has_no_accept_edge() {
        for state in OrchestrationState::ALL {
            if *state == OrchestrationState::Verified {
                continue;
            }
            assert_eq!(
                validate_transition(*state, OrchestrationTransition::Accept),
                Err(if state.is_terminal() {
                    TransitionError::Terminal
                } else {
                    TransitionError::InvalidTransition
                })
            );
        }
        assert_eq!(
            validate_transition(
                OrchestrationState::Verified,
                OrchestrationTransition::Accept
            )
            .unwrap(),
            OrchestrationState::Accepted
        );
    }

    #[test]
    fn cancel_from_non_terminal_succeeds() {
        assert_eq!(
            validate_transition(
                OrchestrationState::Implementing,
                OrchestrationTransition::Cancel
            )
            .unwrap(),
            OrchestrationState::Cancelled
        );
        assert_eq!(
            validate_transition(
                OrchestrationState::Accepted,
                OrchestrationTransition::Cancel
            ),
            Err(TransitionError::InvalidTransition)
        );
    }

    #[test]
    fn unknown_edge_fails_closed() {
        assert_eq!(
            validate_transition(OrchestrationState::Created, OrchestrationTransition::Accept),
            Err(TransitionError::InvalidTransition)
        );
    }
}
