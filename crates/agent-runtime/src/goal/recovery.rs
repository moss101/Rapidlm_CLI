//! Session-recovery hook for top-level goals.
//!
//! Formerly `active` goals become `paused` with `process_recovered`.
//! Paused and blocked snapshots are preserved. Recovery never starts a turn
//! or invokes a provider; continuation requires an explicit resume.

use std::error::Error;
use std::fmt;

use protocol::{AgentId, ArtifactId, ErrorCode, SessionId};

use crate::agent::model::CancellationToken;
use crate::evidence::EvidenceService;
use crate::goal::driver::GoalDriver;
use crate::goal::state::{
    GoalActor, GoalCommand, GoalEffect, GoalEventKind, GoalSnapshot, GoalState, GoalStateError,
    GoalStateMachine,
};
use crate::turn::TurnBudget;

/// Result of [`recover_goal`]: the parked-or-preserved snapshot and optional pause.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalRecovery {
    snapshot: GoalSnapshot,
    effect: Option<GoalEffect>,
}

/// Typed recovery failure. Display never echoes goal statement text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalRecoveryError {
    Cancelled,
    State(GoalStateError),
}

/// Session-recovery hook. Parks a formerly active goal; never auto-continues.
pub fn recover_goal(snapshot: GoalSnapshot) -> Result<GoalRecovery, GoalRecoveryError> {
    recover_goal_with_cancel(snapshot, &CancellationToken::new())
}

/// [`recover_goal`] that observes an explicit cancellation token.
pub fn recover_goal_with_cancel(
    snapshot: GoalSnapshot,
    cancel: &CancellationToken,
) -> Result<GoalRecovery, GoalRecoveryError> {
    if cancel.is_cancelled() {
        return Err(GoalRecoveryError::Cancelled);
    }
    match snapshot.state() {
        GoalState::Paused | GoalState::Blocked => Ok(GoalRecovery {
            snapshot,
            effect: None,
        }),
        GoalState::Active => park_active(snapshot, cancel),
    }
}

fn park_active(
    snapshot: GoalSnapshot,
    cancel: &CancellationToken,
) -> Result<GoalRecovery, GoalRecoveryError> {
    let goal_id = snapshot.id();
    let mut machine = GoalStateMachine::from_snapshot(snapshot);
    let effect = machine
        .apply_with_cancel(
            GoalCommand::Pause {
                goal_id,
                process_recovered: true,
            },
            &GoalActor::System,
            cancel,
        )
        .map_err(GoalRecoveryError::from)?;
    let Some(recovered) = effect.snapshot().cloned() else {
        return Err(GoalRecoveryError::State(GoalStateError::NotActive));
    };
    Ok(GoalRecovery {
        snapshot: recovered,
        effect: Some(effect),
    })
}

impl GoalRecovery {
    pub fn snapshot(&self) -> &GoalSnapshot {
        &self.snapshot
    }

    /// Pause event is present only when the pre-recovery snapshot was active.
    pub fn effect(&self) -> Option<&GoalEffect> {
        self.effect.as_ref()
    }

    pub fn pause_event(&self) -> Option<GoalEventKind> {
        self.effect.as_ref().map(GoalEffect::event)
    }

    /// Bind a driver to the recovered snapshot. Does not start a turn.
    pub fn into_driver(
        self,
        session_id: SessionId,
        agent_id: AgentId,
        actor: GoalActor,
        tool_catalog_hash: ArtifactId,
        turn_budget: TurnBudget,
    ) -> GoalDriver {
        GoalDriver::from_machine(
            session_id,
            agent_id,
            actor,
            tool_catalog_hash,
            turn_budget,
            GoalStateMachine::from_snapshot(self.snapshot),
            EvidenceService::new(),
        )
    }
}

impl GoalRecoveryError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "goal recovery cancelled",
            Self::State(_) => "goal recovery rejected the lifecycle command",
        }
    }

    /// Public error code when this failure has a wire mapping.
    ///
    /// [`GoalRecoveryError::Cancelled`] has no public code.
    pub const fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::State(err) => err.code(),
        }
    }
}

impl From<GoalStateError> for GoalRecoveryError {
    fn from(err: GoalStateError) -> Self {
        match err {
            GoalStateError::Cancelled => Self::Cancelled,
            other => Self::State(other),
        }
    }
}

impl fmt::Display for GoalRecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for GoalRecoveryError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::str::FromStr;

    use protocol::{ArtifactId, GoalId};

    use crate::goal::driver::{GoalDriverError, GoalDriverStop, GoalSession};
    use crate::goal::state::{
        Criterion, EvidenceRequirement, GoalBudget, GoalCommand, GoalSpec, GoalStopReason,
        GoalUsage,
    };
    use crate::turn::{
        ModelDriver, ModelStepError, ModelStepInput, ModelStepOutput, ProposedToolCall, ToolDriver,
        ToolStepError, ToolStepResult, TurnBudget, TurnEvent, TurnEventKind, ValidatedToolCall,
    };

    const GOAL_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const AGENT_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";
    const SESSION_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ac";

    struct ScriptedModel {
        outputs: VecDeque<Result<ModelStepOutput, ModelStepError>>,
        seen: u32,
    }

    struct ScriptedTools;

    impl ScriptedModel {
        fn new(outputs: Vec<Result<ModelStepOutput, ModelStepError>>) -> Self {
            Self {
                outputs: outputs.into(),
                seen: 0,
            }
        }
    }

    impl ModelDriver for ScriptedModel {
        fn step(
            &mut self,
            _input: &ModelStepInput<'_>,
            cancel: &CancellationToken,
        ) -> Result<ModelStepOutput, ModelStepError> {
            if cancel.is_cancelled() {
                return Err(ModelStepError::Cancelled);
            }
            self.seen = self.seen.saturating_add(1);
            self.outputs
                .pop_front()
                .unwrap_or(Err(ModelStepError::Failed))
        }
    }

    impl ToolDriver for ScriptedTools {
        fn validate(
            &mut self,
            call: &ProposedToolCall,
            cancel: &CancellationToken,
        ) -> Result<ValidatedToolCall, ToolStepError> {
            if cancel.is_cancelled() {
                return Err(ToolStepError::Cancelled);
            }
            Ok(ValidatedToolCall::from_proposed(call))
        }

        fn execute(
            &mut self,
            call: &ValidatedToolCall,
            cancel: &CancellationToken,
        ) -> Result<ToolStepResult, ToolStepError> {
            if cancel.is_cancelled() {
                return Err(ToolStepError::Cancelled);
            }
            Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: "ok".to_owned(),
            })
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn parse_id<T: FromStr>(raw: &str) -> T
    where
        T::Err: std::fmt::Debug,
    {
        raw.parse().expect("id")
    }

    fn goal_id() -> GoalId {
        parse_id(GOAL_ID)
    }

    fn agent_id() -> AgentId {
        parse_id(AGENT_ID)
    }

    fn session_id() -> SessionId {
        parse_id(SESSION_ID)
    }

    fn catalog() -> ArtifactId {
        ArtifactId::from_bytes(b"rapidlm-goal-recovery-catalog")
    }

    fn spec() -> GoalSpec {
        GoalSpec::new(
            goal_id(),
            "ship auth",
            vec![Criterion::new("c1", "tests pass").expect("criterion")],
            GoalBudget::new(Some(10), Some(100_000), None, None),
            vec![EvidenceRequirement::new("c1", vec!["test".to_owned()]).expect("req")],
        )
        .expect("spec")
    }

    fn create_active() -> GoalSnapshot {
        let mut machine = GoalStateMachine::new();
        machine
            .apply(GoalCommand::Create(spec()), &GoalActor::Human)
            .expect("create");
        machine
            .snapshot()
            .expect("active")
            .clone()
            .with_usage(GoalUsage::new(2, 40, 1_000, 3))
    }

    fn pause(snapshot: GoalSnapshot, process_recovered: bool) -> GoalSnapshot {
        let mut machine = GoalStateMachine::from_snapshot(snapshot);
        machine
            .apply(
                GoalCommand::Pause {
                    goal_id: goal_id(),
                    process_recovered,
                },
                &GoalActor::Human,
            )
            .expect("pause");
        machine.snapshot().expect("paused").clone()
    }

    fn block(snapshot: GoalSnapshot) -> GoalSnapshot {
        let mut machine = GoalStateMachine::from_snapshot(snapshot);
        machine
            .apply(
                GoalCommand::Block {
                    goal_id: goal_id(),
                    budget_exhausted: true,
                },
                &GoalActor::System,
            )
            .expect("block");
        machine.snapshot().expect("blocked").clone()
    }

    fn next_step(
        driver: &mut GoalDriver,
        model: &mut ScriptedModel,
        tools: &mut ScriptedTools,
        events: &mut Vec<TurnEvent>,
        cancel: &CancellationToken,
    ) -> Result<crate::goal::driver::GoalDriverOutcome, GoalDriverError> {
        let mut session = GoalSession::new(model, tools, events, cancel);
        driver.next(&mut session)
    }

    fn turn_starts(events: &[TurnEvent]) -> usize {
        events
            .iter()
            .filter(|event| event.kind() == TurnEventKind::TurnStarted)
            .count()
    }

    #[test]
    fn recover_goal_returns_pause_event_only_for_formerly_active_goal() {
        let recovery = recover_goal(create_active()).expect("recover active");
        assert_eq!(recovery.pause_event(), Some(GoalEventKind::Paused));
        assert_eq!(recovery.snapshot().state(), GoalState::Paused);
        assert_eq!(
            recovery.snapshot().stop_reason(),
            Some(GoalStopReason::ProcessRecovered)
        );
        assert_eq!(
            recovery.effect().expect("pause").event(),
            GoalEventKind::Paused
        );
        assert_eq!(recovery.snapshot().usage(), GoalUsage::new(2, 40, 1_000, 3));

        let paused = recover_goal(pause(create_active(), false)).expect("recover paused");
        assert!(paused.effect().is_none());
        assert!(paused.pause_event().is_none());

        let blocked = recover_goal(block(create_active())).expect("recover blocked");
        assert!(blocked.effect().is_none());
        assert!(blocked.pause_event().is_none());
    }

    #[test]
    fn paused_and_blocked_goals_remain_preserved() {
        let paused_in = pause(create_active(), false);
        let paused = recover_goal(paused_in.clone()).expect("paused");
        assert_eq!(paused.snapshot(), &paused_in);
        assert_eq!(paused.snapshot().state(), GoalState::Paused);
        assert!(paused.snapshot().stop_reason().is_none());

        let recovered_in = pause(create_active(), true);
        let recovered = recover_goal(recovered_in.clone()).expect("already recovered");
        assert_eq!(recovered.snapshot(), &recovered_in);
        assert_eq!(
            recovered.snapshot().stop_reason(),
            Some(GoalStopReason::ProcessRecovered)
        );

        let blocked_in = block(create_active());
        let blocked = recover_goal(blocked_in.clone()).expect("blocked");
        assert_eq!(blocked.snapshot(), &blocked_in);
        assert_eq!(blocked.snapshot().state(), GoalState::Blocked);
        assert_eq!(
            blocked.snapshot().stop_reason(),
            Some(GoalStopReason::BudgetExhausted)
        );
        assert_eq!(blocked.snapshot().usage(), GoalUsage::new(2, 40, 1_000, 3));
    }

    #[test]
    fn crash_restart_makes_zero_provider_calls_until_explicit_resume() {
        let recovery = recover_goal(create_active()).expect("recover after crash");
        assert_eq!(
            recovery.snapshot().stop_reason(),
            Some(GoalStopReason::ProcessRecovered)
        );

        let mut driver = recovery.into_driver(
            session_id(),
            agent_id(),
            GoalActor::MainAgent {
                agent_id: agent_id(),
            },
            catalog(),
            TurnBudget::unlimited_steps(),
        );
        let mut model = ScriptedModel::new(vec![Ok(ModelStepOutput::Terminal {
            text: "should not run until resume".to_owned(),
            tokens: 1,
            cost_usd_micros: None,
        })]);
        let mut tools = ScriptedTools;
        let mut events = Vec::new();

        let parked = next_step(&mut driver, &mut model, &mut tools, &mut events, &live())
            .expect("parked next");
        assert!(!parked.continues());
        assert_eq!(parked.stop_reason(), Some(GoalDriverStop::Paused));
        assert!(parked.turn().is_none());
        assert_eq!(turn_starts(&events), 0);
        assert_eq!(model.seen, 0, "restart must not call the provider");
        assert_eq!(
            driver.snapshot().expect("parked").state(),
            GoalState::Paused
        );

        driver
            .apply(GoalCommand::Resume { goal_id: goal_id() }, &live())
            .expect("explicit resume");

        let resumed = next_step(&mut driver, &mut model, &mut tools, &mut events, &live())
            .expect("resumed next");
        assert!(resumed.continues());
        assert_eq!(model.seen, 1);
        assert_eq!(turn_starts(&events), 1);
        assert_eq!(
            driver.snapshot().expect("active").state(),
            GoalState::Active
        );
    }

    #[test]
    fn recover_goal_observes_cancellation() {
        let cancel = live();
        cancel.cancel();
        let err = recover_goal_with_cancel(create_active(), &cancel).expect_err("cancelled");
        assert_eq!(err, GoalRecoveryError::Cancelled);
        assert!(err.code().is_none());
        assert_eq!(err.to_string(), "goal recovery cancelled");
        assert!(!format!("{err}").contains("ship"));
    }
}
