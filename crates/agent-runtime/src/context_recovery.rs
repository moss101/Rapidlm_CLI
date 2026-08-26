//! Host-owned live-context recovery seam (agent-harness §6 / P5-024).
//!
//! `run_turn` *detects* a context/bound overflow (it already surfaces
//! `TurnStopReason::ContextBoundExceeded`). The *host/context owner* repairs it.
//! This module provides the typed seam between the two:
//!
//! - [`ContextOverflow`] — the typed request handed to the host;
//! - [`ContextController`] — the host-implemented recovery contract (compact or
//!   rebuild live model input via the Context Fabric; never a callback smuggled
//!   into `TurnSpec`);
//! - [`ContextRecoveryDecision`] + [`ContextRetryPolicy`] — the bounded retry /
//!   terminal decision, applied by the host executor.
//!
//! Context Fabric ownership is NOT moved into `run_turn`; the controller keeps
//! compaction/rebuild authority outside provider-specific model code.

use std::error::Error;
use std::fmt;

use protocol::TurnId;

use crate::agent::model::ContextRevision;

/// Typed overflow request handed to the host/context controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextOverflow {
    turn_id: TurnId,
    attempt: u32,
}

impl ContextOverflow {
    pub const fn new(turn_id: TurnId, attempt: u32) -> Self {
        Self { turn_id, attempt }
    }

    pub const fn turn_id(self) -> TurnId {
        self.turn_id
    }

    pub const fn attempt(self) -> u32 {
        self.attempt
    }
}

/// Host decision after attempting live-context compaction/rebuild.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextRecoveryDecision {
    /// Context was compacted/rebuilt; a bounded re-run may proceed.
    Recovered,
    /// The overflow is not recoverable through compaction.
    NotRecoverable,
    /// The model/goal budget is exhausted; no further retry.
    BudgetExceeded,
    /// Cancellation was observed during recovery.
    Cancelled,
}

/// Host-implemented recovery contract. The host owns live model messages/context,
/// the Context Fabric, and compaction; it decides whether the overflow can be
/// repaired. `run_turn` never owns or consults this directly.
pub trait ContextController {
    fn recover_from_overflow(&mut self, request: ContextOverflow) -> ContextRecoveryDecision;

    /// The rebuilt context revision after the most recent successful recovery.
    ///
    /// The controller owns compaction/rebuild, so it alone knows the identity of
    /// the rebuilt context. It is consumed by the executor to record genuine
    /// [`ContextRevision`] lineage (never raw context contents). The default is
    /// `None`, in which case the executor records only that a recovery occurred.
    fn rebuilt_context(&self) -> Option<ContextRevision> {
        None
    }
}

/// Bounded retry policy for an overflow that was judged recoverable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextRetryPolicy {
    max_attempts: u32,
}

impl ContextRetryPolicy {
    pub fn new(max_attempts: u32) -> Self {
        let max_attempts = if max_attempts == 0 { 1 } else { max_attempts };
        Self { max_attempts }
    }

    pub const fn max_attempts(self) -> u32 {
        self.max_attempts
    }
}

impl Default for ContextRetryPolicy {
    fn default() -> Self {
        Self::new(2)
    }
}

/// Result of assessing one recovery decision against the retry bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryOutcome {
    Retry,
    Stop,
}

/// Whether the host may re-run `run_turn` after a recovery decision.
///
/// `Recovered` retries only within `policy.max_attempts`; every non-recovered
/// decision stops immediately. This is deterministic and never loops unbounded.
pub fn should_retry(
    attempt: u32,
    decision: ContextRecoveryDecision,
    policy: &ContextRetryPolicy,
) -> RetryOutcome {
    match decision {
        ContextRecoveryDecision::Recovered if attempt < policy.max_attempts => RetryOutcome::Retry,
        ContextRecoveryDecision::Recovered => RetryOutcome::Stop,
        ContextRecoveryDecision::NotRecoverable
        | ContextRecoveryDecision::BudgetExceeded
        | ContextRecoveryDecision::Cancelled => RetryOutcome::Stop,
    }
}

/// Typed recovery-policy failure. Display never includes context text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextRecoveryError {
    Cancelled,
    NotRecoverable,
    BudgetExceeded,
}

impl ContextRecoveryError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "context recovery cancelled",
            Self::NotRecoverable => "context overflow is not recoverable",
            Self::BudgetExceeded => "context recovery budget exceeded",
        }
    }
}

impl fmt::Display for ContextRecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ContextRecoveryError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn overflow(attempt: u32) -> ContextOverflow {
        ContextOverflow::new(TurnId::new(), attempt)
    }

    struct StubController {
        outcome: ContextRecoveryDecision,
    }

    impl ContextController for StubController {
        fn recover_from_overflow(&mut self, _request: ContextOverflow) -> ContextRecoveryDecision {
            self.outcome
        }
    }

    #[test]
    fn recovered_within_budget_retries_then_stops_at_bound() {
        let policy = ContextRetryPolicy::new(2);
        assert_eq!(
            should_retry(0, ContextRecoveryDecision::Recovered, &policy),
            RetryOutcome::Retry
        );
        assert_eq!(
            should_retry(1, ContextRecoveryDecision::Recovered, &policy),
            RetryOutcome::Retry
        );
        // At the retry bound a recovered overflow is a terminal bounded stop.
        assert_eq!(
            should_retry(2, ContextRecoveryDecision::Recovered, &policy),
            RetryOutcome::Stop
        );
    }

    #[test]
    fn non_recovered_decisions_stop_immediately() {
        let policy = ContextRetryPolicy::new(10);
        for decision in [
            ContextRecoveryDecision::NotRecoverable,
            ContextRecoveryDecision::BudgetExceeded,
            ContextRecoveryDecision::Cancelled,
        ] {
            assert_eq!(should_retry(0, decision, &policy), RetryOutcome::Stop);
        }
    }

    #[test]
    fn controller_contract_is_host_implementable() {
        let mut controller = StubController {
            outcome: ContextRecoveryDecision::Recovered,
        };
        assert_eq!(
            controller.recover_from_overflow(overflow(0)),
            ContextRecoveryDecision::Recovered
        );
        controller.outcome = ContextRecoveryDecision::Cancelled;
        assert_eq!(
            controller.recover_from_overflow(overflow(0)),
            ContextRecoveryDecision::Cancelled
        );
    }

    #[test]
    fn policy_is_deterministic_and_carries_bounds() {
        let policy = ContextRetryPolicy::new(2);
        assert_eq!(policy.max_attempts(), 2);
        assert_eq!(
            should_retry(0, ContextRecoveryDecision::Recovered, &policy),
            should_retry(0, ContextRecoveryDecision::Recovered, &policy)
        );
    }

    #[test]
    fn controller_defaults_to_no_rebuilt_context() {
        let mut controller = StubController {
            outcome: ContextRecoveryDecision::Recovered,
        };
        assert!(controller.rebuilt_context().is_none());
        // The decision contract is unchanged; rebuilt_context is an optional hook.
        assert_eq!(
            controller.recover_from_overflow(overflow(0)),
            ContextRecoveryDecision::Recovered
        );
    }

    #[test]
    fn controller_reports_rebuilt_context_for_genuine_lineage() {
        let mut controller = ReportingController {
            revision: ContextRevision::new("context/overflow-recovery", None),
        };
        assert_eq!(
            controller.recover_from_overflow(overflow(0)),
            ContextRecoveryDecision::Recovered
        );
        let rev = controller.rebuilt_context().expect("rebuilt context");
        assert_eq!(rev.source(), "context/overflow-recovery");
        // Lineage never carries raw context contents.
        assert!(rev.source().contains("context"));
    }

    struct ReportingController {
        revision: ContextRevision,
    }

    impl ContextController for ReportingController {
        fn recover_from_overflow(&mut self, _request: ContextOverflow) -> ContextRecoveryDecision {
            ContextRecoveryDecision::Recovered
        }

        fn rebuilt_context(&self) -> Option<ContextRevision> {
            Some(self.revision.clone())
        }
    }
}
