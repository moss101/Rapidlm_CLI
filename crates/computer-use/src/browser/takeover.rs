//! Takeover reconciliation state machine (P8-032).
//!
//! Tracks the lifecycle of human-control takeover and return-control
//! reconciliation. When a human takes over from the agent, all prior
//! observations, coordinates, and expected postconditions become stale.
//! On return, a mandatory fresh observation must be taken before the agent
//! can resume; reconciliation compares expected vs actual state to decide
//! whether to resume, replan, or block.

use std::fmt;

/// Takeover reconciliation lifecycle states.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum ReconciliationState {
    AgentControl,
    TakeoverRequested,
    HumanControl,
    ReturnRequested,
    Reobserving,
    Reconciling,
    Resumed,
    ReplanRequired,
    Blocked,
}

/// What changed during human control.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceChange {
    None,
    Navigated { new_url: String },
    ElementDisappeared { selector: String },
    AuthStateChanged,
    ViewportChanged { old: (u32, u32), new: (u32, u32) },
    ContentChanged,
}

impl fmt::Display for SurfaceChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "no_change",
            Self::Navigated { .. } => "navigated",
            Self::ElementDisappeared { .. } => "element_disappeared",
            Self::AuthStateChanged => "auth_state_changed",
            Self::ViewportChanged { .. } => "viewport_changed",
            Self::ContentChanged => "content_changed",
        })
    }
}

/// Outcome of reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum ReconciliationOutcome {
    Compatible,
    Changed,
    Unsafe,
}

/// Typed reconciliation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReconciliationError {
    InvalidTransition {
        from: ReconciliationState,
        event: &'static str,
    },
    StaleObservation,
}

impl fmt::Display for ReconciliationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition { from, event } => {
                write!(f, "invalid transition from {from:?} on {event}")
            }
            Self::StaleObservation => f.write_str("observation is stale"),
        }
    }
}

impl Error for ReconciliationError {}

use std::error::Error;

/// The takeover reconciliation state machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TakeoverReconciler {
    state: ReconciliationState,
    changes: Vec<SurfaceChange>,
}

impl Default for TakeoverReconciler {
    fn default() -> Self {
        Self::new()
    }
}

impl TakeoverReconciler {
    pub fn new() -> Self {
        Self {
            state: ReconciliationState::AgentControl,
            changes: Vec::new(),
        }
    }

    pub fn state(&self) -> ReconciliationState {
        self.state
    }
    pub fn changes(&self) -> &[SurfaceChange] {
        &self.changes
    }

    pub fn request_takeover(&mut self) -> Result<(), ReconciliationError> {
        if self.state != ReconciliationState::AgentControl {
            return Err(ReconciliationError::InvalidTransition {
                from: self.state,
                event: "TAKEOVER_REQUESTED",
            });
        }
        self.state = ReconciliationState::TakeoverRequested;
        Ok(())
    }

    pub fn confirm_human_control(&mut self) -> Result<(), ReconciliationError> {
        if self.state != ReconciliationState::TakeoverRequested {
            return Err(ReconciliationError::InvalidTransition {
                from: self.state,
                event: "HUMAN_CONTROL",
            });
        }
        self.changes.clear();
        self.state = ReconciliationState::HumanControl;
        Ok(())
    }

    pub fn request_return(&mut self) -> Result<(), ReconciliationError> {
        if self.state != ReconciliationState::HumanControl {
            return Err(ReconciliationError::InvalidTransition {
                from: self.state,
                event: "RETURN_CONTROL",
            });
        }
        self.state = ReconciliationState::ReturnRequested;
        Ok(())
    }

    pub fn reobserve(&mut self) -> Result<(), ReconciliationError> {
        if self.state != ReconciliationState::ReturnRequested {
            return Err(ReconciliationError::InvalidTransition {
                from: self.state,
                event: "REOBSERVING",
            });
        }
        self.state = ReconciliationState::Reobserving;
        Ok(())
    }

    /// Record what changed during human control.
    pub fn record_change(&mut self, change: SurfaceChange) {
        self.changes.push(change);
    }

    /// Reconcile expected vs actual surface state after fresh observation.
    pub fn reconcile(
        &mut self,
        expected_url: Option<&str>,
        actual_url: Option<&str>,
        expected_elements: &[String],
        actual_elements: &[String],
    ) -> Result<ReconciliationOutcome, ReconciliationError> {
        if self.state != ReconciliationState::Reobserving {
            return Err(ReconciliationError::InvalidTransition {
                from: self.state,
                event: "RECONCILE",
            });
        }
        self.state = ReconciliationState::Reconciling;
        // URL changed → navigation → replan required.
        if let (Some(expected), Some(actual)) = (expected_url, actual_url)
            && expected != actual
        {
            self.record_change(SurfaceChange::Navigated {
                new_url: actual.to_owned(),
            });
            self.state = ReconciliationState::ReplanRequired;
            return Ok(ReconciliationOutcome::Changed);
        }
        // Auth state changed → unsafe → block.
        if self
            .changes
            .iter()
            .any(|c| matches!(c, SurfaceChange::AuthStateChanged))
        {
            self.state = ReconciliationState::Blocked;
            return Ok(ReconciliationOutcome::Unsafe);
        }
        // Expected elements disappeared → replan required.
        for el in expected_elements {
            if !actual_elements.contains(el) {
                self.record_change(SurfaceChange::ElementDisappeared {
                    selector: el.to_owned(),
                });
            }
        }
        if !self.changes.is_empty() {
            self.state = ReconciliationState::ReplanRequired;
            return Ok(ReconciliationOutcome::Changed);
        }
        // Safe: no changes detected.
        self.state = ReconciliationState::Resumed;
        Ok(ReconciliationOutcome::Compatible)
    }

    /// Resume after successful reconciliation.
    pub fn resume(&mut self) -> Result<(), ReconciliationError> {
        if self.state != ReconciliationState::Resumed {
            return Err(ReconciliationError::InvalidTransition {
                from: self.state,
                event: "RESUME",
            });
        }
        self.state = ReconciliationState::AgentControl;
        self.changes.clear();
        Ok(())
    }

    /// Block after unsafe reconciliation.
    pub fn block(&mut self) {
        self.state = ReconciliationState::Blocked;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_lifecycle_safe_return() {
        let mut rec = TakeoverReconciler::new();
        assert_eq!(rec.state(), ReconciliationState::AgentControl);
        rec.request_takeover().unwrap();
        rec.confirm_human_control().unwrap();
        assert_eq!(rec.state(), ReconciliationState::HumanControl);
        // No changes during human control.
        rec.request_return().unwrap();
        rec.reobserve().unwrap();
        let outcome = rec
            .reconcile(
                Some("https://app.com/page"),
                Some("https://app.com/page"),
                &[],
                &[],
            )
            .unwrap();
        assert_eq!(outcome, ReconciliationOutcome::Compatible);
        rec.resume().unwrap();
        assert_eq!(rec.state(), ReconciliationState::AgentControl);
    }

    #[test]
    fn human_navigation_causes_replan() {
        let mut rec = TakeoverReconciler::new();
        rec.request_takeover().unwrap();
        rec.confirm_human_control().unwrap();
        rec.request_return().unwrap();
        rec.reobserve().unwrap();
        let outcome = rec
            .reconcile(
                Some("https://app.com/original"),
                Some("https://other.com/navigated"),
                &[],
                &[],
            )
            .unwrap();
        assert_eq!(outcome, ReconciliationOutcome::Changed);
        assert!(
            rec.changes()
                .iter()
                .any(|c| matches!(c, SurfaceChange::Navigated { .. }))
        );
    }

    #[test]
    fn element_disappearance_triggers_replan() {
        let mut rec = TakeoverReconciler::new();
        rec.request_takeover().unwrap();
        rec.confirm_human_control().unwrap();
        rec.request_return().unwrap();
        rec.reobserve().unwrap();
        let outcome = rec
            .reconcile(
                Some("https://app.com/page"),
                Some("https://app.com/page"),
                &["btn-submit".to_owned(), "input-email".to_owned()],
                &["btn-submit".to_owned()],
            )
            .unwrap();
        assert_eq!(outcome, ReconciliationOutcome::Changed);
        assert!(rec
            .changes()
            .iter()
            .any(|c| matches!(c, SurfaceChange::ElementDisappeared { selector } if selector == "input-email")));
    }

    #[test]
    fn auth_state_change_blocks_reconciliation() {
        let mut rec = TakeoverReconciler::new();
        rec.request_takeover().unwrap();
        rec.confirm_human_control().unwrap();
        rec.record_change(SurfaceChange::AuthStateChanged);
        rec.request_return().unwrap();
        rec.reobserve().unwrap();
        let outcome = rec
            .reconcile(
                Some("https://app.com/page"),
                Some("https://app.com/page"),
                &[],
                &[],
            )
            .unwrap();
        assert_eq!(outcome, ReconciliationOutcome::Unsafe);
        assert_eq!(rec.state(), ReconciliationState::Blocked);
    }

    #[test]
    fn invalid_transition_fails_closed() {
        let mut rec = TakeoverReconciler::new();
        // Cannot confirm human control without requesting takeover first.
        assert!(rec.confirm_human_control().is_err());
        assert!(matches!(
            rec.confirm_human_control(),
            Err(ReconciliationError::InvalidTransition { .. })
        ));
    }
}
