//! Autonomous goal driver. One ordinary turn per iteration while the goal is active.
//!
//! Goal snapshot and budget-hint injection happen at turn boundaries only.
//! Pause, block, complete, and cancel stop continuation. Model text cannot
//! mark a goal complete; missing required evidence rejects completion.

use std::error::Error;
use std::fmt;

use protocol::{AgentId, ArtifactId, ErrorCode, SessionId, TurnId};

use crate::agent::model::{AgentRole, CancellationToken};
use crate::evidence::{EvidenceError, EvidenceService};
use crate::goal::budget::{ConvergenceHint, GoalBudgetError, GoalBudgetGuard};
use crate::goal::state::{
    GoalActor, GoalCommand, GoalEffect, GoalEventKind, GoalSnapshot, GoalState, GoalStateError,
    GoalStateMachine,
};
use crate::loop_guard::MessageLoopDetector;
use crate::prompt::{PromptBundle, PromptCompiler, PromptError, PromptInputs, PromptLimits};
use crate::turn::{
    ModelDriver, ProposedToolCall, ToolDriver, ToolStepError, ToolStepResult, TurnBudget,
    TurnError, TurnEventSink, TurnResult, TurnSpec, TurnStatus, ValidatedToolCall, run_turn,
};

/// Structured tool that requests runtime-validated completion.
pub const GOAL_COMPLETE_TOOL: &str = "goal.complete";

/// Structured tool that parks an active goal.
pub const GOAL_PAUSE_TOOL: &str = "goal.pause";

/// Structured tool that records an external/user/policy blocker.
pub const GOAL_BLOCK_TOOL: &str = "goal.block";

/// Structured tool that clears the active goal.
pub const GOAL_CANCEL_TOOL: &str = "goal.cancel";

/// Collaborators for one [`GoalDriver::next`] iteration.
pub struct GoalSession<'a, M, T, E> {
    model: &'a mut M,
    tools: &'a mut T,
    events: &'a mut E,
    cancel: &'a CancellationToken,
}

/// Session-scoped autonomous continuation. Holds the goal, evidence, and budgets.
#[derive(Clone, Debug)]
pub struct GoalDriver {
    session_id: SessionId,
    agent_id: AgentId,
    actor: GoalActor,
    machine: GoalStateMachine,
    evidence: EvidenceService,
    tool_catalog_hash: ArtifactId,
    turn_budget: TurnBudget,
    active_ms: u64,
    message_loop: MessageLoopDetector,
}

/// Result of one driver iteration.
// `Continued` legitimately carries a full turn's payloads while `Stopped`
// carries none; boxing the large variant would only add indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalDriverOutcome {
    /// Goal remains active. The caller may invoke [`GoalDriver::next`] again.
    Continued {
        turn: TurnResult,
        prompt: PromptBundle,
        snapshot: GoalSnapshot,
        hint: Option<ConvergenceHint>,
        rejected_complete: bool,
    },
    /// Pause, block, complete, cancel, or missing goal. No further turn is started.
    Stopped {
        reason: GoalDriverStop,
        turn: Option<TurnResult>,
        prompt: Option<PromptBundle>,
        snapshot: Option<GoalSnapshot>,
        hint: Option<ConvergenceHint>,
        effect: Option<GoalEffect>,
    },
}

/// Why [`GoalDriver::next`] did not continue the goal.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum GoalDriverStop {
    NoGoal,
    Paused,
    Blocked,
    Completed,
    Cancelled,
    RepeatedMessage,
}

/// Typed driver failure. Display never echoes goal statement or model text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalDriverError {
    Cancelled,
    EvidenceMissing,
    State(GoalStateError),
    Evidence(EvidenceError),
    Turn(TurnError),
    Prompt(PromptError),
}

struct GoalLifecycleTools<'a, T> {
    inner: &'a mut T,
    machine: &'a mut GoalStateMachine,
    evidence: &'a EvidenceService,
    actor: GoalActor,
    applied: &'a mut Option<GoalEffect>,
    rejected_complete: &'a mut bool,
    cancel: &'a CancellationToken,
}

impl<'a, M, T, E> GoalSession<'a, M, T, E> {
    pub fn new(
        model: &'a mut M,
        tools: &'a mut T,
        events: &'a mut E,
        cancel: &'a CancellationToken,
    ) -> Self {
        Self {
            model,
            tools,
            events,
            cancel,
        }
    }

    pub fn cancel(&self) -> &CancellationToken {
        self.cancel
    }
}

impl GoalDriver {
    pub fn new(
        session_id: SessionId,
        agent_id: AgentId,
        actor: GoalActor,
        tool_catalog_hash: ArtifactId,
        turn_budget: TurnBudget,
    ) -> Self {
        Self {
            session_id,
            agent_id,
            actor,
            machine: GoalStateMachine::new(),
            evidence: EvidenceService::new(),
            tool_catalog_hash,
            turn_budget,
            active_ms: 0,
            message_loop: MessageLoopDetector::new(),
        }
    }

    pub fn from_machine(
        session_id: SessionId,
        agent_id: AgentId,
        actor: GoalActor,
        tool_catalog_hash: ArtifactId,
        turn_budget: TurnBudget,
        machine: GoalStateMachine,
        evidence: EvidenceService,
    ) -> Self {
        Self {
            session_id,
            agent_id,
            actor,
            machine,
            evidence,
            tool_catalog_hash,
            turn_budget,
            active_ms: 0,
            message_loop: MessageLoopDetector::new(),
        }
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    pub const fn actor(&self) -> GoalActor {
        self.actor
    }

    pub fn snapshot(&self) -> Option<&GoalSnapshot> {
        self.machine.snapshot()
    }

    pub fn evidence(&self) -> &EvidenceService {
        &self.evidence
    }

    pub fn evidence_mut(&mut self) -> &mut EvidenceService {
        &mut self.evidence
    }

    pub const fn turn_budget(&self) -> TurnBudget {
        self.turn_budget
    }

    pub fn set_active_ms(&mut self, active_ms: u64) {
        self.active_ms = active_ms;
    }

    /// Apply a lifecycle command. Complete is rejected without required evidence.
    pub fn apply(
        &mut self,
        command: GoalCommand,
        cancel: &CancellationToken,
    ) -> Result<GoalEffect, GoalDriverError> {
        apply_lifecycle(
            &mut self.machine,
            &self.evidence,
            command,
            &self.actor,
            cancel,
        )
    }

    /// Start exactly one ordinary turn when the top-level goal is still active.
    pub fn next<M, T, E>(
        &mut self,
        session: &mut GoalSession<'_, M, T, E>,
    ) -> Result<GoalDriverOutcome, GoalDriverError>
    where
        M: ModelDriver,
        T: ToolDriver,
        E: TurnEventSink,
    {
        check_cancel(session.cancel)?;
        if let Some(reason) = stop_without_turn(self.machine.snapshot()) {
            return Ok(GoalDriverOutcome::stopped(reason, self.machine.snapshot()));
        }

        let hint = match self.budget_before(session.cancel)? {
            BudgetGate::Continue(hint) => hint,
            BudgetGate::Blocked => {
                return Ok(GoalDriverOutcome::stopped(
                    GoalDriverStop::Blocked,
                    self.machine.snapshot(),
                ));
            }
        };

        let snapshot = match self.machine.snapshot() {
            Some(snapshot) if snapshot.state() == GoalState::Active => snapshot.clone(),
            _ => {
                return Ok(GoalDriverOutcome::stopped(
                    GoalDriverStop::NoGoal,
                    self.machine.snapshot(),
                ));
            }
        };
        let prompt =
            compile_boundary_prompt(snapshot, hint, self.tool_catalog_hash, session.cancel)?;

        let mut applied = None;
        let mut rejected_complete = false;
        let turn = {
            let mut tools = GoalLifecycleTools {
                inner: session.tools,
                machine: &mut self.machine,
                evidence: &self.evidence,
                actor: self.actor,
                applied: &mut applied,
                rejected_complete: &mut rejected_complete,
                cancel: session.cancel,
            };
            let spec = TurnSpec::new(
                TurnId::new(),
                self.session_id,
                self.agent_id,
                self.turn_budget,
                session.model,
                &mut tools,
                session.events,
            );
            match run_turn(spec, session.cancel) {
                Ok(turn) => turn,
                Err(TurnError::Cancelled) => return Err(GoalDriverError::Cancelled),
                Err(err) => {
                    let _ = self.pause_if_active(session.cancel);
                    return Err(GoalDriverError::Turn(err));
                }
            }
        };

        if turn.status() != TurnStatus::Completed {
            let _ = self.pause_if_active(session.cancel);
        }

        let post_hint = self.accrue_after_turn(turn.usage().tokens(), session.cancel)?;
        let hint = post_hint.or(hint);

        if let Some(reason) = stop_after_turn(self.machine.snapshot(), applied.as_ref()) {
            return Ok(GoalDriverOutcome::stopped_after(
                reason,
                Some(turn),
                Some(prompt),
                self.machine.snapshot().cloned(),
                hint,
                applied,
            ));
        }

        // Repeated completed assistant message across goal-continuation turns is
        // a loop. Only the content hash is observed (never the raw text); it is
        // bounded, deterministic, and reset at the goal/attempt boundary (a
        // fresh detector per GoalDriver). It cannot create an infinite retry:
        // it stops the goal rather than continuing. Budget accrual and lifecycle
        // take precedence, so a loop can never mask an exhausted budget.
        if let Some(hash) = turn.terminal_hash() {
            self.message_loop.observe_hash(hash);
            if self.message_loop.is_looping() {
                let _ = self.pause_if_active(session.cancel);
                return Ok(GoalDriverOutcome::stopped_after(
                    GoalDriverStop::RepeatedMessage,
                    Some(turn),
                    Some(prompt),
                    self.machine.snapshot().cloned(),
                    None,
                    None,
                ));
            }
        }

        let snapshot = match self.machine.snapshot() {
            Some(snapshot) if snapshot.state() == GoalState::Active => snapshot.clone(),
            Some(snapshot) => {
                return Ok(GoalDriverOutcome::stopped_after(
                    GoalDriverStop::from_state(snapshot.state()),
                    Some(turn),
                    Some(prompt),
                    Some(snapshot.clone()),
                    hint,
                    applied,
                ));
            }
            None => {
                return Ok(GoalDriverOutcome::stopped_after(
                    GoalDriverStop::NoGoal,
                    Some(turn),
                    Some(prompt),
                    None,
                    hint,
                    applied,
                ));
            }
        };

        Ok(GoalDriverOutcome::Continued {
            turn,
            prompt,
            snapshot,
            hint,
            rejected_complete,
        })
    }

    fn budget_before(&mut self, cancel: &CancellationToken) -> Result<BudgetGate, GoalDriverError> {
        let Some(snapshot) = self.machine.snapshot().cloned() else {
            return Ok(BudgetGate::Continue(None));
        };
        let mut guard = GoalBudgetGuard::from_snapshot(&snapshot);
        let outcome = guard.before_turn(cancel).map_err(GoalDriverError::from)?;
        if outcome.is_exhausted() {
            apply_lifecycle(
                &mut self.machine,
                &self.evidence,
                GoalCommand::Block {
                    goal_id: snapshot.id(),
                    budget_exhausted: true,
                },
                &self.actor,
                cancel,
            )?;
            return Ok(BudgetGate::Blocked);
        }
        Ok(BudgetGate::Continue(outcome.hint()))
    }

    fn accrue_after_turn(
        &mut self,
        tokens: u64,
        cancel: &CancellationToken,
    ) -> Result<Option<ConvergenceHint>, GoalDriverError> {
        let Some(snapshot) = self.machine.snapshot().cloned() else {
            return Ok(None);
        };
        if snapshot.state() != GoalState::Active {
            return Ok(None);
        }
        let mut guard = GoalBudgetGuard::from_snapshot(&snapshot);
        guard
            .after_model(tokens, 0, cancel)
            .map_err(GoalDriverError::from)?;
        let outcome = guard
            .after_turn(self.active_ms, cancel)
            .map_err(GoalDriverError::from)?;
        let updated = guard.apply(snapshot);
        let goal_id = updated.id();
        self.machine = GoalStateMachine::from_snapshot(updated);
        if outcome.is_exhausted() {
            apply_lifecycle(
                &mut self.machine,
                &self.evidence,
                GoalCommand::Block {
                    goal_id,
                    budget_exhausted: true,
                },
                &self.actor,
                cancel,
            )?;
        }
        Ok(outcome.hint())
    }

    fn pause_if_active(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Option<GoalEffect>, GoalDriverError> {
        let Some(snapshot) = self.machine.snapshot() else {
            return Ok(None);
        };
        if snapshot.state() != GoalState::Active {
            return Ok(None);
        }
        let goal_id = snapshot.id();
        apply_lifecycle(
            &mut self.machine,
            &self.evidence,
            GoalCommand::Pause {
                goal_id,
                process_recovered: false,
            },
            &self.actor,
            cancel,
        )
        .map(Some)
    }
}

enum BudgetGate {
    Continue(Option<ConvergenceHint>),
    Blocked,
}

impl GoalDriverOutcome {
    fn stopped(reason: GoalDriverStop, snapshot: Option<&GoalSnapshot>) -> Self {
        Self::Stopped {
            reason,
            turn: None,
            prompt: None,
            snapshot: snapshot.cloned(),
            hint: None,
            effect: None,
        }
    }

    fn stopped_after(
        reason: GoalDriverStop,
        turn: Option<TurnResult>,
        prompt: Option<PromptBundle>,
        snapshot: Option<GoalSnapshot>,
        hint: Option<ConvergenceHint>,
        effect: Option<GoalEffect>,
    ) -> Self {
        Self::Stopped {
            reason,
            turn,
            prompt,
            snapshot,
            hint,
            effect,
        }
    }

    pub const fn continues(&self) -> bool {
        matches!(self, Self::Continued { .. })
    }

    pub fn turn(&self) -> Option<TurnResult> {
        match self {
            Self::Continued { turn, .. } => Some(turn.clone()),
            Self::Stopped { turn, .. } => turn.clone(),
        }
    }

    pub fn prompt(&self) -> Option<&PromptBundle> {
        match self {
            Self::Continued { prompt, .. } => Some(prompt),
            Self::Stopped { prompt, .. } => prompt.as_ref(),
        }
    }

    pub fn snapshot(&self) -> Option<&GoalSnapshot> {
        match self {
            Self::Continued { snapshot, .. } => Some(snapshot),
            Self::Stopped { snapshot, .. } => snapshot.as_ref(),
        }
    }

    pub fn hint(&self) -> Option<ConvergenceHint> {
        match self {
            Self::Continued { hint, .. } | Self::Stopped { hint, .. } => *hint,
        }
    }

    pub const fn stop_reason(&self) -> Option<GoalDriverStop> {
        match self {
            Self::Stopped { reason, .. } => Some(*reason),
            Self::Continued { .. } => None,
        }
    }

    pub const fn rejected_complete(&self) -> bool {
        match self {
            Self::Continued {
                rejected_complete, ..
            } => *rejected_complete,
            Self::Stopped { .. } => false,
        }
    }

    pub fn effect(&self) -> Option<&GoalEffect> {
        match self {
            Self::Stopped { effect, .. } => effect.as_ref(),
            Self::Continued { .. } => None,
        }
    }
}

impl GoalDriverStop {
    pub const ALL: &'static [Self] = &[
        Self::NoGoal,
        Self::Paused,
        Self::Blocked,
        Self::Completed,
        Self::Cancelled,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoGoal => "no_goal",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::RepeatedMessage => "repeated_message",
        }
    }

    fn from_state(state: GoalState) -> Self {
        match state {
            GoalState::Active => Self::NoGoal,
            GoalState::Paused => Self::Paused,
            GoalState::Blocked => Self::Blocked,
        }
    }
}

impl GoalDriverError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "goal driver cancelled",
            Self::EvidenceMissing => "required goal evidence is missing",
            Self::State(_) => "goal lifecycle rejected the command",
            Self::Evidence(_) => "goal evidence validation failed",
            Self::Turn(_) => "goal driver turn failed",
            Self::Prompt(_) => "goal boundary prompt compile failed",
        }
    }

    pub const fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::EvidenceMissing => Some(ErrorCode::GoalEvidenceMissing),
            Self::State(err) => err.code(),
            Self::Evidence(err) => err.code(),
            Self::Turn(err) => err.code(),
            Self::Prompt(err) => err.code(),
        }
    }
}

impl From<GoalStateError> for GoalDriverError {
    fn from(err: GoalStateError) -> Self {
        match err {
            GoalStateError::Cancelled => Self::Cancelled,
            other => Self::State(other),
        }
    }
}

impl From<EvidenceError> for GoalDriverError {
    fn from(err: EvidenceError) -> Self {
        match err {
            EvidenceError::Cancelled => Self::Cancelled,
            EvidenceError::EvidenceMissing => Self::EvidenceMissing,
            other => Self::Evidence(other),
        }
    }
}

impl From<GoalBudgetError> for GoalDriverError {
    fn from(err: GoalBudgetError) -> Self {
        match err {
            GoalBudgetError::Cancelled => Self::Cancelled,
        }
    }
}

impl From<TurnError> for GoalDriverError {
    fn from(err: TurnError) -> Self {
        match err {
            TurnError::Cancelled => Self::Cancelled,
            other => Self::Turn(other),
        }
    }
}

impl From<PromptError> for GoalDriverError {
    fn from(err: PromptError) -> Self {
        match err {
            PromptError::Cancelled => Self::Cancelled,
            other => Self::Prompt(other),
        }
    }
}

impl<T: ToolDriver> ToolDriver for GoalLifecycleTools<'_, T> {
    fn validate(
        &mut self,
        call: &ProposedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ValidatedToolCall, ToolStepError> {
        if cancel.is_cancelled() || self.cancel.is_cancelled() {
            return Err(ToolStepError::Cancelled);
        }
        if goal_command_kind(call.tool()).is_some() {
            return Ok(ValidatedToolCall::from_proposed(call));
        }
        self.inner.validate(call, cancel)
    }

    fn execute(
        &mut self,
        call: &ValidatedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        if cancel.is_cancelled() || self.cancel.is_cancelled() {
            return Err(ToolStepError::Cancelled);
        }
        let Some(kind) = goal_command_kind(call.tool()) else {
            return self.inner.execute(call, cancel);
        };
        let Some(snapshot) = self.machine.snapshot() else {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: None,
            });
        };
        let command = match kind {
            LifecycleKind::Complete => GoalCommand::Complete {
                goal_id: snapshot.id(),
            },
            LifecycleKind::Pause => GoalCommand::Pause {
                goal_id: snapshot.id(),
                process_recovered: false,
            },
            LifecycleKind::Block => GoalCommand::Block {
                goal_id: snapshot.id(),
                budget_exhausted: false,
            },
            LifecycleKind::Cancel => GoalCommand::Cancel {
                goal_id: snapshot.id(),
            },
        };
        match apply_lifecycle(self.machine, self.evidence, command, &self.actor, cancel) {
            Ok(effect) => {
                *self.applied = Some(effect);
                Ok(ToolStepResult::Succeeded {
                    call_id: call.call_id().to_owned(),
                    summary: kind.as_str().to_owned(),
                })
            }
            Err(GoalDriverError::Cancelled) => Err(ToolStepError::Cancelled),
            Err(GoalDriverError::EvidenceMissing) => {
                *self.rejected_complete = true;
                Ok(ToolStepResult::Failed {
                    call_id: call.call_id().to_owned(),
                    handled: true,
                    detail: None,
                })
            }
            Err(_) => Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: None,
            }),
        }
    }
}

#[derive(Clone, Copy)]
enum LifecycleKind {
    Complete,
    Pause,
    Block,
    Cancel,
}

impl LifecycleKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => GOAL_COMPLETE_TOOL,
            Self::Pause => GOAL_PAUSE_TOOL,
            Self::Block => GOAL_BLOCK_TOOL,
            Self::Cancel => GOAL_CANCEL_TOOL,
        }
    }
}

fn goal_command_kind(tool: &str) -> Option<LifecycleKind> {
    match tool {
        GOAL_COMPLETE_TOOL => Some(LifecycleKind::Complete),
        GOAL_PAUSE_TOOL => Some(LifecycleKind::Pause),
        GOAL_BLOCK_TOOL => Some(LifecycleKind::Block),
        GOAL_CANCEL_TOOL => Some(LifecycleKind::Cancel),
        _ => None,
    }
}

fn apply_lifecycle(
    machine: &mut GoalStateMachine,
    evidence: &EvidenceService,
    command: GoalCommand,
    actor: &GoalActor,
    cancel: &CancellationToken,
) -> Result<GoalEffect, GoalDriverError> {
    check_cancel(cancel)?;
    if let GoalCommand::Complete { goal_id } = command {
        let Some(snapshot) = machine.snapshot() else {
            return Err(GoalDriverError::from(GoalStateError::NotActive));
        };
        if snapshot.id() != goal_id {
            return Err(GoalDriverError::from(GoalStateError::GoalMismatch {
                expected: snapshot.id(),
                found: goal_id,
            }));
        }
        if !evidence.can_complete(snapshot).allowed() {
            return Err(GoalDriverError::EvidenceMissing);
        }
    }
    machine
        .apply_with_cancel(command, actor, cancel)
        .map_err(GoalDriverError::from)
}

fn compile_boundary_prompt(
    snapshot: GoalSnapshot,
    hint: Option<ConvergenceHint>,
    tool_catalog_hash: ArtifactId,
    cancel: &CancellationToken,
) -> Result<PromptBundle, GoalDriverError> {
    let limits = PromptLimits::new().cancellation(cancel.clone());
    let mut inputs = PromptInputs::new(AgentRole::Main, tool_catalog_hash)
        .goal(snapshot)
        .limits(limits);
    if let Some(hint) = hint.filter(|hint| !hint.is_empty()) {
        inputs = inputs.converge(hint);
    }
    PromptCompiler::compile(&inputs).map_err(GoalDriverError::from)
}

fn stop_without_turn(snapshot: Option<&GoalSnapshot>) -> Option<GoalDriverStop> {
    match snapshot {
        None => Some(GoalDriverStop::NoGoal),
        Some(snapshot) => match snapshot.state() {
            GoalState::Active => None,
            GoalState::Paused => Some(GoalDriverStop::Paused),
            GoalState::Blocked => Some(GoalDriverStop::Blocked),
        },
    }
}

fn stop_after_turn(
    snapshot: Option<&GoalSnapshot>,
    effect: Option<&GoalEffect>,
) -> Option<GoalDriverStop> {
    if let Some(effect) = effect {
        return match effect.event() {
            GoalEventKind::Paused => Some(GoalDriverStop::Paused),
            GoalEventKind::Blocked => Some(GoalDriverStop::Blocked),
            GoalEventKind::Completed => Some(GoalDriverStop::Completed),
            GoalEventKind::Cancelled => Some(GoalDriverStop::Cancelled),
            GoalEventKind::Created | GoalEventKind::Replaced | GoalEventKind::Resumed => {
                stop_without_turn(snapshot)
            }
        };
    }
    stop_without_turn(snapshot)
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), GoalDriverError> {
    if cancel.is_cancelled() {
        Err(GoalDriverError::Cancelled)
    } else {
        Ok(())
    }
}

impl fmt::Display for GoalDriverStop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for GoalDriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for GoalDriverError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::str::FromStr;

    use protocol::{ArtifactId, EvidenceId, GoalId};

    use crate::evidence::{
        EvidenceKind, EvidenceProducer, EvidenceSource, EvidenceSpec, EvidenceStatus, TEST_PASSED,
    };
    use crate::goal::state::{
        Criterion, EvidenceRequirement, GoalBudget, GoalCommand, GoalEventKind, GoalSpec,
        GoalStopReason, GoalUsage,
    };
    use crate::prompt::PromptSection;
    use crate::turn::{
        ModelStepError, ModelStepInput, ModelStepOutput, ProposedToolCall, TurnBudget, TurnEvent,
        TurnEventKind, TurnStatus,
    };

    const GOAL_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const EVIDENCE_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ae";
    const AGENT_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";
    const SESSION_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ac";

    struct ScriptedModel {
        outputs: VecDeque<Result<ModelStepOutput, ModelStepError>>,
        seen: u32,
    }

    struct ScriptedTools {
        executed: u32,
    }

    impl ScriptedModel {
        fn new(outputs: Vec<Result<ModelStepOutput, ModelStepError>>) -> Self {
            Self {
                outputs: outputs.into(),
                seen: 0,
            }
        }
    }

    impl ScriptedTools {
        fn new() -> Self {
            Self { executed: 0 }
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
            self.executed = self.executed.saturating_add(1);
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
        ArtifactId::from_bytes(b"rapidlm-goal-driver-catalog")
    }

    fn turn_budget() -> TurnBudget {
        TurnBudget::unlimited_steps()
    }

    fn spec(budget: GoalBudget) -> GoalSpec {
        GoalSpec::new(
            goal_id(),
            "ship auth",
            vec![Criterion::new("c1", "tests pass").expect("criterion")],
            budget,
            vec![EvidenceRequirement::new("c1", vec!["test".to_owned()]).expect("req")],
        )
        .expect("spec")
    }

    fn driver_with(budget: GoalBudget) -> GoalDriver {
        let mut driver = GoalDriver::new(
            session_id(),
            agent_id(),
            GoalActor::MainAgent {
                agent_id: agent_id(),
            },
            catalog(),
            turn_budget(),
        );
        driver
            .apply(GoalCommand::Create(spec(budget)), &live())
            .expect("create");
        driver
    }

    fn passing_test() -> EvidenceSpec {
        EvidenceSpec::new(
            parse_id::<EvidenceId>(EVIDENCE_ID),
            goal_id(),
            EvidenceKind::Test,
            TEST_PASSED,
            EvidenceProducer::System,
            EvidenceSource::new(ArtifactId::from_bytes(b"rapidlm-evidence-fixture")),
            EvidenceStatus::Passed,
            "src/lib.rs",
        )
        .expect("spec")
        .with_criterion_id("c1")
        .expect("criterion")
        .with_command("cargo test")
        .expect("command")
    }

    fn terminal(text: &str, tokens: u64) -> Result<ModelStepOutput, ModelStepError> {
        Ok(ModelStepOutput::Terminal {
            text: text.to_owned(),
            tokens,
            cost_usd_micros: None,
        })
    }

    fn tools_out(
        calls: Vec<ProposedToolCall>,
        tokens: u64,
    ) -> Result<ModelStepOutput, ModelStepError> {
        Ok(ModelStepOutput::ToolCalls {
            calls,
            tokens,
            cost_usd_micros: None,
        })
    }

    fn call(id: &str, tool: &str) -> ProposedToolCall {
        ProposedToolCall::new(id, tool, "{}").expect("call")
    }

    fn turn_starts(events: &[TurnEvent]) -> usize {
        events
            .iter()
            .filter(|event| event.kind() == TurnEventKind::TurnStarted)
            .count()
    }

    fn section(bundle: &PromptBundle, wanted: PromptSection) -> &str {
        bundle
            .messages()
            .iter()
            .find(|message| message.section() == wanted)
            .map(|message| message.content())
            .expect("section")
    }

    fn next_step(
        driver: &mut GoalDriver,
        model: &mut ScriptedModel,
        tools: &mut ScriptedTools,
        events: &mut Vec<TurnEvent>,
        cancel: &CancellationToken,
    ) -> Result<GoalDriverOutcome, GoalDriverError> {
        let mut session = GoalSession::new(model, tools, events, cancel);
        driver.next(&mut session)
    }

    #[test]
    fn next_starts_exactly_one_ordinary_turn() {
        let mut driver = driver_with(GoalBudget::default());
        let mut model = ScriptedModel::new(vec![terminal("one slice", 3)]);
        let mut tools = ScriptedTools::new();
        let mut events = Vec::new();
        let outcome =
            next_step(&mut driver, &mut model, &mut tools, &mut events, &live()).expect("next");
        assert!(outcome.continues());
        assert_eq!(model.seen, 1);
        assert_eq!(turn_starts(&events), 1);
        let turn = outcome.turn().expect("turn");
        assert_eq!(turn.status(), TurnStatus::Completed);
        assert_eq!(turn.usage().model_steps(), 1);
    }

    #[test]
    fn active_goal_continues_when_model_leaves_it_active() {
        let mut driver = driver_with(GoalBudget::default());
        let mut model = ScriptedModel::new(vec![terminal("first", 1), terminal("second", 1)]);
        let mut tools = ScriptedTools::new();
        let mut events = Vec::new();
        let first =
            next_step(&mut driver, &mut model, &mut tools, &mut events, &live()).expect("first");
        assert!(first.continues());
        assert_eq!(first.snapshot().expect("snap").state(), GoalState::Active);
        let second =
            next_step(&mut driver, &mut model, &mut tools, &mut events, &live()).expect("second");
        assert!(second.continues());
        assert_eq!(turn_starts(&events), 2);
        assert_eq!(model.seen, 2);
        assert_eq!(
            driver.snapshot().expect("still active").state(),
            GoalState::Active
        );
    }

    #[test]
    fn repeated_terminal_message_stops_the_goal_loop() {
        let mut driver = driver_with(GoalBudget::default());
        let mut model = ScriptedModel::new(vec![
            terminal("still working", 1),
            terminal("still working", 1),
            terminal("still working", 1),
        ]);
        let mut tools = ScriptedTools::new();
        let mut events = Vec::new();
        // Two identical messages are tolerated (below the threshold).
        assert!(
            next_step(&mut driver, &mut model, &mut tools, &mut events, &live())
                .expect("1")
                .continues()
        );
        assert!(
            next_step(&mut driver, &mut model, &mut tools, &mut events, &live())
                .expect("2")
                .continues()
        );
        // The third identical message trips the repeated-message loop guard.
        let third =
            next_step(&mut driver, &mut model, &mut tools, &mut events, &live()).expect("3");
        assert!(!third.continues());
        assert_eq!(third.stop_reason(), Some(GoalDriverStop::RepeatedMessage));
        assert_eq!(driver.snapshot().expect("snap").state(), GoalState::Paused);
        assert_eq!(model.seen, 3);
    }

    #[test]
    fn alternating_messages_do_not_stop_the_goal() {
        let mut driver = driver_with(GoalBudget::default());
        let mut model = ScriptedModel::new(vec![
            terminal("checking A", 1),
            terminal("checking B", 1),
            terminal("checking A", 1),
            terminal("checking B", 1),
        ]);
        let mut tools = ScriptedTools::new();
        let mut events = Vec::new();
        for index in 0..4 {
            let outcome =
                next_step(&mut driver, &mut model, &mut tools, &mut events, &live()).expect("turn");
            assert!(
                outcome.continues(),
                "alternating messages at turn {index} must not loop"
            );
        }
        assert_eq!(driver.snapshot().expect("snap").state(), GoalState::Active);
    }

    #[test]
    fn no_continuation_when_paused_blocked_or_cleared() {
        let cancel = live();
        for prepare in [
            |d: &mut GoalDriver| {
                d.apply(
                    GoalCommand::Pause {
                        goal_id: goal_id(),
                        process_recovered: false,
                    },
                    &live(),
                )
                .expect("pause");
            },
            |d: &mut GoalDriver| {
                d.apply(
                    GoalCommand::Block {
                        goal_id: goal_id(),
                        budget_exhausted: false,
                    },
                    &live(),
                )
                .expect("block");
            },
            |d: &mut GoalDriver| {
                d.apply(GoalCommand::Cancel { goal_id: goal_id() }, &live())
                    .expect("cancel");
            },
        ] {
            let mut driver = driver_with(GoalBudget::default());
            prepare(&mut driver);
            let mut model = ScriptedModel::new(vec![terminal("should not run", 1)]);
            let mut tools = ScriptedTools::new();
            let mut events = Vec::new();
            let outcome =
                next_step(&mut driver, &mut model, &mut tools, &mut events, &cancel).expect("idle");
            assert!(!outcome.continues());
            assert!(outcome.turn().is_none());
            assert_eq!(turn_starts(&events), 0);
            assert_eq!(model.seen, 0);
        }
    }

    #[test]
    fn complete_command_rejected_when_required_evidence_missing() {
        let mut driver = driver_with(GoalBudget::default());
        let err = driver
            .apply(GoalCommand::Complete { goal_id: goal_id() }, &live())
            .expect_err("missing evidence");
        assert_eq!(err, GoalDriverError::EvidenceMissing);
        assert_eq!(err.code(), Some(ErrorCode::GoalEvidenceMissing));
        assert_eq!(
            driver.snapshot().expect("still active").state(),
            GoalState::Active
        );

        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("c1", GOAL_COMPLETE_TOOL)], 1),
            terminal("still working", 1),
        ]);
        let mut tools = ScriptedTools::new();
        let mut events = Vec::new();
        let outcome = next_step(&mut driver, &mut model, &mut tools, &mut events, &live())
            .expect("rejected complete");
        assert!(outcome.continues());
        assert!(outcome.rejected_complete());
        assert_eq!(
            driver.snapshot().expect("active").state(),
            GoalState::Active
        );
        assert_eq!(tools.executed, 0);
    }

    #[test]
    fn complete_with_evidence_clears_and_stops() {
        let mut driver = driver_with(GoalBudget::default());
        driver
            .evidence_mut()
            .record(passing_test())
            .expect("record");
        let effect = driver
            .apply(GoalCommand::Complete { goal_id: goal_id() }, &live())
            .expect("complete");
        assert_eq!(effect.event(), GoalEventKind::Completed);
        assert!(effect.invalidate_context());
        assert!(driver.snapshot().is_none());

        let mut model = ScriptedModel::new(vec![terminal("should not run", 1)]);
        let mut tools = ScriptedTools::new();
        let mut events = Vec::new();
        let outcome =
            next_step(&mut driver, &mut model, &mut tools, &mut events, &live()).expect("cleared");
        assert_eq!(outcome.stop_reason(), Some(GoalDriverStop::NoGoal));
        assert_eq!(turn_starts(&events), 0);
    }

    #[test]
    fn injects_goal_snapshot_and_budget_hint_at_turn_boundary() {
        let mut machine = GoalStateMachine::new();
        machine
            .apply(
                GoalCommand::Create(spec(GoalBudget::new(None, Some(100), None, None))),
                &GoalActor::Human,
            )
            .expect("create");
        let snap = machine
            .snapshot()
            .expect("snap")
            .clone()
            .with_usage(GoalUsage::new(0, 75, 0, 0));
        let mut driver = GoalDriver::from_machine(
            session_id(),
            agent_id(),
            GoalActor::MainAgent {
                agent_id: agent_id(),
            },
            catalog(),
            turn_budget(),
            GoalStateMachine::from_snapshot(snap),
            EvidenceService::new(),
        );
        let mut model = ScriptedModel::new(vec![terminal("converge", 1)]);
        let mut tools = ScriptedTools::new();
        let mut events = Vec::new();
        let outcome =
            next_step(&mut driver, &mut model, &mut tools, &mut events, &live()).expect("next");
        let prompt = outcome.prompt().expect("prompt");
        let goal = section(prompt, PromptSection::Goal);
        assert!(goal.contains(GOAL_ID));
        assert!(goal.contains("state: active"));
        assert!(goal.contains("ship auth"));
        let converge = section(prompt, PromptSection::Converge);
        assert!(converge.contains("tokens"));
        assert!(outcome.hint().expect("hint").tokens());
    }

    #[test]
    fn complete_tool_with_evidence_stops_continuation() {
        let mut driver = driver_with(GoalBudget::default());
        driver
            .evidence_mut()
            .record(passing_test())
            .expect("record");
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("g1", GOAL_COMPLETE_TOOL)], 1),
            terminal("done", 1),
        ]);
        let mut tools = ScriptedTools::new();
        let mut events = Vec::new();
        let outcome =
            next_step(&mut driver, &mut model, &mut tools, &mut events, &live()).expect("complete");
        assert_eq!(outcome.stop_reason(), Some(GoalDriverStop::Completed));
        assert!(driver.snapshot().is_none());
        assert_eq!(turn_starts(&events), 1);
        let mut again = Vec::new();
        let idle = next_step(&mut driver, &mut model, &mut tools, &mut again, &live())
            .expect("no continue");
        assert!(!idle.continues());
        assert_eq!(turn_starts(&again), 0);
    }

    #[test]
    fn stop_on_pause_block_and_cancel_tools() {
        for (tool, reason) in [
            (GOAL_PAUSE_TOOL, GoalDriverStop::Paused),
            (GOAL_BLOCK_TOOL, GoalDriverStop::Blocked),
            (GOAL_CANCEL_TOOL, GoalDriverStop::Cancelled),
        ] {
            let mut driver = driver_with(GoalBudget::default());
            let mut model = ScriptedModel::new(vec![
                tools_out(vec![call("g1", tool)], 1),
                terminal("after lifecycle", 1),
            ]);
            let mut tools = ScriptedTools::new();
            let mut events = Vec::new();
            let outcome = next_step(&mut driver, &mut model, &mut tools, &mut events, &live())
                .expect("lifecycle");
            assert!(!outcome.continues(), "{tool} must stop");
            assert_eq!(outcome.stop_reason(), Some(reason), "{tool}");
            assert_eq!(turn_starts(&events), 1);
            let mut again = Vec::new();
            let idle = next_step(&mut driver, &mut model, &mut tools, &mut again, &live())
                .expect("no continue");
            assert!(!idle.continues());
            assert!(idle.turn().is_none());
            assert_eq!(turn_starts(&again), 0);
        }
    }

    #[test]
    fn budget_exhaustion_blocks_without_continuing() {
        let mut machine = GoalStateMachine::new();
        machine
            .apply(
                GoalCommand::Create(spec(GoalBudget::new(Some(1), None, None, None))),
                &GoalActor::Human,
            )
            .expect("create");
        let snap = machine
            .snapshot()
            .expect("snap")
            .clone()
            .with_usage(GoalUsage::new(1, 0, 0, 0));
        let mut driver = GoalDriver::from_machine(
            session_id(),
            agent_id(),
            GoalActor::MainAgent {
                agent_id: agent_id(),
            },
            catalog(),
            turn_budget(),
            GoalStateMachine::from_snapshot(snap),
            EvidenceService::new(),
        );
        let mut model = ScriptedModel::new(vec![terminal("should not run", 1)]);
        let mut tools = ScriptedTools::new();
        let mut events = Vec::new();
        let outcome =
            next_step(&mut driver, &mut model, &mut tools, &mut events, &live()).expect("blocked");
        assert_eq!(outcome.stop_reason(), Some(GoalDriverStop::Blocked));
        assert!(outcome.turn().is_none());
        assert_eq!(turn_starts(&events), 0);
        assert_eq!(
            driver.snapshot().expect("blocked").stop_reason(),
            Some(GoalStopReason::BudgetExhausted)
        );
    }

    #[test]
    fn cancelled_next_does_not_start_a_turn() {
        let cancel = live();
        cancel.cancel();
        let mut driver = driver_with(GoalBudget::default());
        let mut model = ScriptedModel::new(vec![terminal("nope", 1)]);
        let mut tools = ScriptedTools::new();
        let mut events = Vec::new();
        let err = next_step(&mut driver, &mut model, &mut tools, &mut events, &cancel)
            .expect_err("cancelled");
        assert_eq!(err, GoalDriverError::Cancelled);
        assert_eq!(turn_starts(&events), 0);
        assert_eq!(model.seen, 0);
        assert_eq!(
            GoalDriverError::Cancelled.to_string(),
            "goal driver cancelled"
        );
    }

    #[test]
    fn subagent_cannot_mutate_or_complete() {
        let mut driver = GoalDriver::new(
            session_id(),
            agent_id(),
            GoalActor::Subagent {
                agent_id: agent_id(),
            },
            catalog(),
            turn_budget(),
        );
        let err = driver
            .apply(GoalCommand::Create(spec(GoalBudget::default())), &live())
            .expect_err("subagent");
        assert_eq!(err.code(), Some(ErrorCode::PolicyDenied));
        assert!(driver.snapshot().is_none());
    }
}
