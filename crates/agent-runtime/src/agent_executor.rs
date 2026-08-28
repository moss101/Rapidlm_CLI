//! Canonical host-owned AgentExecutor (agent-harness §2, P5-002).
//!
//! The single authoritative boundary between the graph/scheduler host and an
//! agent run. It coordinates a `ModelDriver` + `ToolDriver` through `run_turn`,
//! applies the execution context (role/clean task context/workspace/budget), and
//! assembles a canonical [`AgentResult`] where host-owned provenance fields
//! (evidence/artifact refs, blockers, context lineage, tool-repair stats) come
//! from host state and the terminal assistant output comes from the turn.
//!
//! It does NOT become the Policy Engine, Capability Broker, Context Fabric,
//! Workspace Manager or Model Provider — those remain separate authorities.

use std::error::Error;
use std::fmt;

use protocol::{ArtifactRef, EvidenceId, SessionId, TurnId};

use crate::agent::model::{
    AgentBudget, AgentResult, AgentSpec, AgentTerminalStatus, Blocker, CancellationToken, Claim,
    ContextRevision, ToolRepairStats,
};
use crate::context_recovery::{
    ContextController, ContextOverflow, ContextRecoveryDecision, ContextRecoveryError,
    ContextRetryPolicy, RetryOutcome, should_retry,
};
use crate::role_profile::RoleToolSurface;
use crate::turn::{FailureCause};
use crate::turn::{
    MAX_MODEL_STEPS, ModelDriver, ToolDriver, TurnBudget, TurnError, TurnEventSink, TurnResult,
    TurnSpec, TurnStatus, TurnStopReason, run_turn,
};

/// Maximum accepted callable tools on one execution (mirrors turn ceiling).
pub const MAX_EXECUTOR_TOOL_CALLS: u32 = 256;

/// Host-owned execution inputs for one agent run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentExecutionRequest {
    spec: AgentSpec,
    session_id: SessionId,
    selected_context: Option<String>,
    capability_surface: RoleToolSurface,
    evidence: Vec<EvidenceId>,
    artifacts: Vec<ArtifactRef>,
    claims: Vec<Claim>,
    open_questions: Vec<String>,
    blockers: Vec<Blocker>,
    context_lineage: Vec<ContextRevision>,
    tool_repair_stats: ToolRepairStats,
}

impl AgentExecutionRequest {
    pub fn new(spec: AgentSpec, session_id: SessionId) -> Self {
        Self {
            spec,
            session_id,
            selected_context: None,
            capability_surface: RoleToolSurface::none(),
            evidence: Vec::new(),
            artifacts: Vec::new(),
            claims: Vec::new(),
            open_questions: Vec::new(),
            blockers: Vec::new(),
            context_lineage: Vec::new(),
            tool_repair_stats: ToolRepairStats::default(),
        }
    }

    pub fn with_selected_context(mut self, context: impl Into<String>) -> Self {
        self.selected_context = Some(context.into());
        self
    }

    pub fn with_capability_surface(mut self, surface: RoleToolSurface) -> Self {
        self.capability_surface = surface;
        self
    }

    pub fn with_evidence(mut self, evidence: Vec<EvidenceId>) -> Self {
        self.evidence = evidence;
        self
    }

    pub fn with_artifacts(mut self, artifacts: Vec<ArtifactRef>) -> Self {
        self.artifacts = artifacts;
        self
    }

    pub fn with_claims(mut self, claims: Vec<Claim>) -> Self {
        self.claims = claims;
        self
    }

    pub fn with_open_questions(mut self, questions: Vec<String>) -> Self {
        self.open_questions = questions;
        self
    }

    pub fn with_blockers(mut self, blockers: Vec<Blocker>) -> Self {
        self.blockers = blockers;
        self
    }

    pub fn with_context_lineage(mut self, lineage: Vec<ContextRevision>) -> Self {
        self.context_lineage = lineage;
        self
    }

    pub fn with_tool_repair_stats(mut self, stats: ToolRepairStats) -> Self {
        self.tool_repair_stats = stats;
        self
    }

    pub fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn selected_context(&self) -> Option<&str> {
        self.selected_context.as_deref()
    }

    pub fn capability_surface(&self) -> RoleToolSurface {
        self.capability_surface
    }

    pub fn evidence(&self) -> &[EvidenceId] {
        &self.evidence
    }

    pub fn artifacts(&self) -> &[ArtifactRef] {
        &self.artifacts
    }

    pub fn claims(&self) -> &[Claim] {
        &self.claims
    }

    pub fn open_questions(&self) -> &[String] {
        &self.open_questions
    }

    pub fn blockers(&self) -> &[Blocker] {
        &self.blockers
    }

    pub fn context_lineage(&self) -> &[ContextRevision] {
        &self.context_lineage
    }

    pub fn tool_repair_stats(&self) -> &ToolRepairStats {
        &self.tool_repair_stats
    }
}

/// Canonical agent-execution abstraction. Implementations own the host recovery
/// policy; they never grant authority (policy/broker stay separate).
pub trait AgentExecutor {
    fn execute<M, T, E>(
        &mut self,
        request: &AgentExecutionRequest,
        model: &mut M,
        tools: &mut T,
        events: &mut E,
        cancel: &CancellationToken,
    ) -> Result<AgentResult, AgentExecutionError>
    where
        M: ModelDriver,
        T: ToolDriver,
        E: TurnEventSink;

    /// Execute with host-owned live-context recovery (P5-024).
    ///
    /// Runs the turn through the same [`ModelDriver`], and on a typed
    /// context/bound overflow hands the recovery decision to the host-owned
    /// [`ContextController`]. A recovered overflow retries within
    /// [`ContextRetryPolicy`], records a new [`ContextRevision`] in lineage, and
    /// never replays committed tool effects (it fails closed).
    ///
    /// The outcome carries the provider-classified failure cause so hosts can
    /// name auth/connection/rejection/transient classes at their boundary.
    ///
    /// Implementations without a recovery owner fall back to plain [`execute`]
    /// (no recovery) — the default keeps the trait single-authority and safe.
    #[allow(clippy::too_many_arguments)]
    fn execute_with_context_recovery<C, M, T, E>(
        &mut self,
        request: &AgentExecutionRequest,
        controller: &mut C,
        policy: ContextRetryPolicy,
        model: &mut M,
        tools: &mut T,
        events: &mut E,
        cancel: &CancellationToken,
    ) -> Result<AgentOutcome, AgentExecutionError>
    where
        C: ContextController,
        M: ModelDriver,
        T: ToolDriver,
        E: TurnEventSink,
    {
        let _ = (controller, policy);
        self.execute(request, model, tools, events, cancel)
            .map(|result| AgentOutcome {
                result,
                failure_cause: None,
            })
    }
}

/// Typed agent-execution failure. Display never echoes task/output text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentExecutionError {
    Cancelled,
    InvalidRequest,
    AgentResult,
    Turn(TurnError),
    /// The turn overflowed only after committing tool effects, so a blind retry
    /// would replay committed side effects. The executor fails closed rather
    /// than re-run.
    ContextOverflowAfterEffects,
    /// Repeated overflow exhausted the bounded recovery policy.
    ContextRetryExceeded,
    ContextRecovery(ContextRecoveryError),
}

impl AgentExecutionError {
    pub const fn error_code(&self) -> Option<protocol::ErrorCode> {
        match self {
            Self::Cancelled | Self::ContextRecovery(ContextRecoveryError::Cancelled) => None,
            Self::InvalidRequest | Self::AgentResult | Self::Turn(_) => {
                Some(protocol::ErrorCode::ToolInvalidArguments)
            }
            Self::ContextOverflowAfterEffects
            | Self::ContextRetryExceeded
            | Self::ContextRecovery(_) => Some(protocol::ErrorCode::ProviderContextTooLarge),
        }
    }
}

/// Canonical execution outcome: the assembled [`AgentResult`] plus the
/// provider-classified cause when the turn failed on a model step. The cause
/// rides this in-memory struct — `AgentResult` is a wire type and stays
/// unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentOutcome {
    pub result: AgentResult,
    pub failure_cause: Option<FailureCause>,
}

/// Default executor: runs a turn and assembles a canonical `AgentResult`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TurnAgentExecutor;

/// Host-supplied label for a lineage entry recorded after an overflow recovery
/// when the controller does not report a specific rebuilt context.
pub const DEFAULT_RECOVERY_SOURCE: &str = "context/overflow-recovery";

impl TurnAgentExecutor {
    fn budget_from(spec_budget: AgentBudget) -> TurnBudget {
        let max_tool_calls = spec_budget
            .max_tool_calls()
            .map(|n| n.min(u32::MAX as u64) as u32)
            .unwrap_or(MAX_EXECUTOR_TOOL_CALLS);
        TurnBudget::new(
            MAX_MODEL_STEPS,
            Some(max_tool_calls),
            spec_budget.max_tokens(),
        )
        .unwrap_or(TurnBudget::unlimited_steps())
    }

    fn summarize(turn: &TurnResult) -> String {
        if let Some(output) = turn.terminal_output() {
            output.text().to_owned()
        } else {
            format!("agent turn {}", turn.status().as_str())
        }
    }

    /// Run exactly one model/tool turn and assemble a canonical `AgentResult`
    /// using the request's host-owned provenance.
    fn run_once<M, T, E>(
        request: &AgentExecutionRequest,
        model: &mut M,
        tools: &mut T,
        events: &mut E,
        cancel: &CancellationToken,
    ) -> Result<(TurnResult, AgentResult), AgentExecutionError>
    where
        M: ModelDriver,
        T: ToolDriver,
        E: TurnEventSink,
    {
        if cancel.is_cancelled() {
            return Err(AgentExecutionError::Cancelled);
        }
        let spec = request.spec();
        let turn = run_turn(
            TurnSpec::new(
                TurnId::new(),
                request.session_id(),
                spec.id(),
                Self::budget_from(spec.budget()),
                model,
                tools,
                events,
            ),
            cancel,
        )
        .map_err(|err| match err {
            TurnError::Cancelled => AgentExecutionError::Cancelled,
            other => AgentExecutionError::Turn(other),
        })?;
        let result = Self::assemble(request, &turn, request.context_lineage())?;
        Ok((turn, result))
    }

    /// Assemble a canonical `AgentResult` from host state + a turn. Host-owned
    /// provenance (claims/blockers/lineage/repair stats) always comes from the
    /// request, never from model text.
    fn assemble(
        request: &AgentExecutionRequest,
        turn: &TurnResult,
        lineage: &[ContextRevision],
    ) -> Result<AgentResult, AgentExecutionError> {
        let status = match turn.status() {
            TurnStatus::Completed => AgentTerminalStatus::Succeeded,
            TurnStatus::Failed => AgentTerminalStatus::Failed,
            TurnStatus::Interrupted => AgentTerminalStatus::Cancelled,
        };
        let summary = Self::summarize(turn);
        if summary.is_empty() {
            return Err(AgentExecutionError::AgentResult);
        }
        let spec = request.spec();
        let result = AgentResult::new(
            spec.id(),
            status,
            summary,
            request.evidence().to_vec(),
            Some(spec.workspace_view_id()),
            None,
            request.artifacts().to_vec(),
        )
        .map_err(|_| AgentExecutionError::AgentResult)?;
        result
            .with_claims(request.claims().to_vec())
            .and_then(|r| r.with_open_questions(request.open_questions().to_vec()))
            .and_then(|r| r.with_blockers(request.blockers().to_vec()))
            .and_then(|r| r.with_context_lineage(lineage.to_vec()))
            .and_then(|r| r.with_tool_repair_stats(request.tool_repair_stats().clone()))
            .map_err(|_| AgentExecutionError::AgentResult)
    }

    fn recovery_error(decision: ContextRecoveryDecision) -> AgentExecutionError {
        match decision {
            ContextRecoveryDecision::Cancelled => AgentExecutionError::Cancelled,
            ContextRecoveryDecision::BudgetExceeded => {
                AgentExecutionError::ContextRecovery(ContextRecoveryError::BudgetExceeded)
            }
            // `should_retry` returns Stop here only when the retry bound is hit.
            ContextRecoveryDecision::Recovered => AgentExecutionError::ContextRetryExceeded,
            ContextRecoveryDecision::NotRecoverable => {
                AgentExecutionError::ContextRecovery(ContextRecoveryError::NotRecoverable)
            }
        }
    }
}

impl AgentExecutor for TurnAgentExecutor {
    fn execute<M, T, E>(
        &mut self,
        request: &AgentExecutionRequest,
        model: &mut M,
        tools: &mut T,
        events: &mut E,
        cancel: &CancellationToken,
    ) -> Result<AgentResult, AgentExecutionError>
    where
        M: ModelDriver,
        T: ToolDriver,
        E: TurnEventSink,
    {
        Self::run_once(request, model, tools, events, cancel).map(|(_, result)| result)
    }

    fn execute_with_context_recovery<C, M, T, E>(
        &mut self,
        request: &AgentExecutionRequest,
        controller: &mut C,
        policy: ContextRetryPolicy,
        model: &mut M,
        tools: &mut T,
        events: &mut E,
        cancel: &CancellationToken,
    ) -> Result<AgentOutcome, AgentExecutionError>
    where
        C: ContextController,
        M: ModelDriver,
        T: ToolDriver,
        E: TurnEventSink,
    {
        let mut lineage = request.context_lineage().to_vec();
        let mut attempt: u32 = 0;
        loop {
            let (turn, result) = Self::run_once(request, model, tools, events, cancel)?;
            let cause = turn.failure_cause();
            let overflow = turn.status() == TurnStatus::Failed
                && turn.reason() == Some(TurnStopReason::ContextBoundExceeded);
            if !overflow {
                // Success (or a non-overflow failure): bind the (possibly grown)
                // lineage so recovered revisions are reflected in the result.
                return result
                    .with_context_lineage(lineage)
                    .map(|bound| AgentOutcome {
                        result: bound,
                        failure_cause: cause,
                    })
                    .map_err(|_| AgentExecutionError::AgentResult);
            }

            // Failed closed: never blindly re-run an attempt that already
            // dispatched tool calls, which may have committed side effects.
            if turn.usage().tool_calls() > 0 {
                return Err(AgentExecutionError::ContextOverflowAfterEffects);
            }

            // Bounded: once the retry bound is reached, stop without consulting
            // the controller (host state is not mutated for a retry that cannot
            // happen).
            if attempt >= policy.max_attempts() {
                return Err(AgentExecutionError::ContextRetryExceeded);
            }

            let decision =
                controller.recover_from_overflow(ContextOverflow::new(turn.turn_id(), attempt));
            match should_retry(attempt, decision, &policy) {
                RetryOutcome::Retry => {
                    // Host rebuilt the context; record genuine provenance (never
                    // raw context contents) and retry within the bound.
                    let rebuild = controller
                        .rebuilt_context()
                        .unwrap_or_else(|| ContextRevision::new(DEFAULT_RECOVERY_SOURCE, None));
                    lineage.push(rebuild);
                    attempt += 1;
                    continue;
                }
                RetryOutcome::Stop => return Err(Self::recovery_error(decision)),
            }
        }
    }
}

impl fmt::Display for AgentExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("agent execution cancelled"),
            Self::InvalidRequest => f.write_str("agent execution request is invalid"),
            Self::AgentResult => f.write_str("agent execution could not assemble a result"),
            Self::Turn(err) => write!(f, "{err}"),
            Self::ContextOverflowAfterEffects => {
                f.write_str("context overflow after committed tool effects")
            }
            Self::ContextRetryExceeded => f.write_str("context recovery retries exhausted"),
            Self::ContextRecovery(err) => write!(f, "{err}"),
        }
    }
}

impl Error for AgentExecutionError {}

impl From<TurnError> for AgentExecutionError {
    fn from(err: TurnError) -> Self {
        match err {
            TurnError::Cancelled => Self::Cancelled,
            other => Self::Turn(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::model::{AgentRole, AgentSpec};
    use crate::role_profile::{RoleRegistry, RoleToolClass};
    use crate::turn::{
        FailureCause, ModelStepError, ModelStepInput, ModelStepOutput, ProposedToolCall,
        ToolStepError, ValidatedToolCall,
    };
    use protocol::{AgentId, WorkspaceViewId};
    use std::collections::VecDeque;

    fn agent_id() -> AgentId {
        AgentId::new()
    }

    fn view() -> WorkspaceViewId {
        WorkspaceViewId::new()
    }

    fn spec(role: AgentRole, task: &str, profile: &str) -> AgentSpec {
        AgentSpec::builder(agent_id(), role, task, view())
            .permissions_profile(profile)
            .build()
            .expect("spec")
    }

    struct Model {
        outputs: VecDeque<Result<ModelStepOutput, ModelStepError>>,
    }
    impl ModelDriver for Model {
        fn step(
            &mut self,
            _input: &ModelStepInput<'_>,
            cancel: &CancellationToken,
        ) -> Result<ModelStepOutput, ModelStepError> {
            if cancel.is_cancelled() {
                return Err(ModelStepError::Cancelled);
            }
            self.outputs
                .pop_front()
                .unwrap_or(Err(ModelStepError::Failed))
        }
    }

    struct Tools;
    impl ToolDriver for Tools {
        fn validate(
            &mut self,
            _call: &ProposedToolCall,
            _cancel: &CancellationToken,
        ) -> Result<ValidatedToolCall, ToolStepError> {
            Err(ToolStepError::Invalid)
        }
        fn execute(
            &mut self,
            _call: &ValidatedToolCall,
            _cancel: &CancellationToken,
        ) -> Result<crate::turn::ToolStepResult, ToolStepError> {
            Err(ToolStepError::Invalid)
        }
    }

    fn terminal(text: &str) -> Model {
        Model {
            outputs: vec![Ok(ModelStepOutput::Terminal {
                text: text.to_owned(),
                tokens: 1,
            })]
            .into(),
        }
    }

    #[test]
    fn executor_runs_a_turn_and_assembles_canonical_agent_result() {
        let spec = spec(AgentRole::Coder, "implement foo", "work");
        let request = AgentExecutionRequest::new(spec, SessionId::new())
            .with_capability_surface(RoleRegistry::profile(AgentRole::Coder).tool_surface());
        let mut model = terminal("implemented the function");
        let mut events = Vec::new();
        let result = TurnAgentExecutor
            .execute(
                &request,
                &mut model,
                &mut Tools,
                &mut events,
                &CancellationToken::new(),
            )
            .expect("execute");
        assert_eq!(result.status(), AgentTerminalStatus::Succeeded);
        assert_eq!(result.summary(), "implemented the function");
        assert!(!result.summary().contains("chain-of-thought"));
        assert!(result.workspace_view().is_some());
        // Host capability surface is attached to the request, never broadens.
        assert!(request.capability_surface().allows(RoleToolClass::Read));
        assert!(request.capability_surface().allows(RoleToolClass::Write));
    }

    #[test]
    fn executor_propagates_host_provenance_and_lineage() {
        use crate::agent::model::{BlockerKind, Claim, ClaimResult};
        let spec = spec(AgentRole::Reviewer, "review", "read_only");
        let claim = Claim::new(
            Some("c1".to_owned()),
            "tests pass".to_owned(),
            ClaimResult::Satisfied,
        );
        let blocker = Blocker::new(BlockerKind::Policy, "needs review");
        let lineage = ContextRevision::new("agent/main", None);
        let stats = ToolRepairStats::new(1, 1, vec!["enum_alias_coercion".to_owned()]);
        let request = AgentExecutionRequest::new(spec, SessionId::new())
            .with_claims(vec![claim])
            .with_blockers(vec![blocker])
            .with_context_lineage(vec![lineage])
            .with_tool_repair_stats(stats.clone());
        let mut model = terminal("review done");
        let mut events = Vec::new();
        let result = TurnAgentExecutor
            .execute(
                &request,
                &mut model,
                &mut Tools,
                &mut events,
                &CancellationToken::new(),
            )
            .expect("execute");
        assert_eq!(result.claims().len(), 1);
        assert_eq!(result.blockers().len(), 1);
        assert_eq!(result.context_lineage()[0].source(), "agent/main");
        assert_eq!(result.tool_repair_stats(), &stats);
        assert_eq!(result.status(), AgentTerminalStatus::Succeeded);
    }

    #[test]
    fn executor_cancellation_propagates() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let spec = spec(AgentRole::Coder, "x", "work");
        let request = AgentExecutionRequest::new(spec, SessionId::new());
        let result = TurnAgentExecutor.execute(
            &request,
            &mut terminal("y"),
            &mut Tools,
            &mut Vec::new(),
            &cancel,
        );
        assert_eq!(result, Err(AgentExecutionError::Cancelled));
    }

    struct RecoveryController {
        decision: ContextRecoveryDecision,
        rebuilt: Option<ContextRevision>,
        calls: u32,
    }

    impl RecoveryController {
        fn new(decision: ContextRecoveryDecision) -> Self {
            Self {
                decision,
                rebuilt: None,
                calls: 0,
            }
        }

        fn rebuilt(mut self, revision: ContextRevision) -> Self {
            self.rebuilt = Some(revision);
            self
        }
    }

    impl ContextController for RecoveryController {
        fn recover_from_overflow(&mut self, _request: ContextOverflow) -> ContextRecoveryDecision {
            self.calls = self.calls.saturating_add(1);
            self.decision
        }

        fn rebuilt_context(&self) -> Option<ContextRevision> {
            self.rebuilt.clone()
        }
    }

    struct RecordingAcceptingTools {
        executed: std::cell::Cell<u32>,
    }

    impl RecordingAcceptingTools {
        fn new() -> Self {
            Self {
                executed: std::cell::Cell::new(0),
            }
        }
    }

    impl ToolDriver for RecordingAcceptingTools {
        fn validate(
            &mut self,
            call: &ProposedToolCall,
            _cancel: &CancellationToken,
        ) -> Result<ValidatedToolCall, ToolStepError> {
            Ok(ValidatedToolCall::from_proposed(call))
        }

        fn execute(
            &mut self,
            call: &ValidatedToolCall,
            _cancel: &CancellationToken,
        ) -> Result<crate::turn::ToolStepResult, ToolStepError> {
            self.executed.set(self.executed.get().saturating_add(1));
            Ok(crate::turn::ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: "ok".to_owned(),
            })
        }
    }

    fn run_recovering(
        request: &AgentExecutionRequest,
        controller: &mut RecoveryController,
        policy: ContextRetryPolicy,
        model: &mut Model,
        tools: &mut impl ToolDriver,
        cancel: &CancellationToken,
    ) -> Result<AgentOutcome, AgentExecutionError> {
        let mut events = Vec::new();
        TurnAgentExecutor.execute_with_context_recovery(
            request,
            controller,
            policy,
            model,
            tools,
            &mut events,
            cancel,
        )
    }

    fn overflow_then(text: impl Into<String>) -> Model {
        Model {
            outputs: vec![
                Err(ModelStepError::BoundExceeded),
                Ok(ModelStepOutput::Terminal {
                    text: text.into(),
                    tokens: 1,
                }),
            ]
            .into(),
        }
    }

    #[test]
    fn recovery_normal_context_succeeds_without_retry() {
        let spec = spec(AgentRole::Coder, "implement", "work");
        let request = AgentExecutionRequest::new(spec, SessionId::new());
        let mut controller = RecoveryController::new(ContextRecoveryDecision::Recovered);
        let mut model = terminal("done");
        let outcome = run_recovering(
            &request,
            &mut controller,
            ContextRetryPolicy::new(2),
            &mut model,
            &mut Tools,
            &CancellationToken::new(),
        )
        .expect("execute");
        let result = outcome.result;
        assert_eq!(result.status(), AgentTerminalStatus::Succeeded);
        assert_eq!(result.summary(), "done");
        assert_eq!(controller.calls, 0, "no overflow → no recovery consult");
        assert!(result.context_lineage().is_empty(), "lineage unchanged");
        assert_eq!(outcome.failure_cause, None, "success carries no cause");
        // The bounded terminal output is exposed intact, never truncated.
        assert!(!result.summary().contains("chain-of-thought"));
    }

    #[test]
    fn recovery_overflow_rebuilds_and_retries_with_new_revision() {
        let spec = spec(AgentRole::Coder, "implement", "work");
        let request = AgentExecutionRequest::new(spec, SessionId::new());
        let mut controller = RecoveryController::new(ContextRecoveryDecision::Recovered)
            .rebuilt(ContextRevision::new("context/compacted", None));
        let mut model = overflow_then("after rebuild");
        let outcome = run_recovering(
            &request,
            &mut controller,
            ContextRetryPolicy::new(2),
            &mut model,
            &mut Tools,
            &CancellationToken::new(),
        )
        .expect("execute");
        let result = outcome.result;
        assert_eq!(result.status(), AgentTerminalStatus::Succeeded);
        assert_eq!(result.summary(), "after rebuild");
        assert_eq!(controller.calls, 1, "one overflow triggers one recovery");
        let lineage = result.context_lineage();
        assert_eq!(lineage.len(), 1);
        assert_eq!(lineage[0].source(), "context/compacted");
        // Lineage is a revision reference, never a raw context dump.
        assert!(lineage[0].source().starts_with("context"));
        assert!(!result.summary().contains("context/compacted"));
    }

    #[test]
    fn recovery_retries_are_bounded_and_repeated_overflow_is_typed() {
        let spec = spec(AgentRole::Coder, "implement", "work");
        let request = AgentExecutionRequest::new(spec, SessionId::new());
        let mut controller = RecoveryController::new(ContextRecoveryDecision::Recovered);
        let mut model = Model {
            outputs: vec![Err(ModelStepError::BoundExceeded); 3].into(),
        };
        let err = run_recovering(
            &request,
            &mut controller,
            ContextRetryPolicy::new(2),
            &mut model,
            &mut Tools,
            &CancellationToken::new(),
        )
        .expect_err("repeated overflow");
        assert_eq!(err, AgentExecutionError::ContextRetryExceeded);
        assert_eq!(controller.calls, 2, "two recoveries then bounded stop");
        assert_eq!(
            err.error_code(),
            Some(protocol::ErrorCode::ProviderContextTooLarge)
        );
    }

    #[test]
    fn recovery_fails_closed_after_committed_tool_effects() {
        let spec = spec(AgentRole::Coder, "implement", "work");
        let request = AgentExecutionRequest::new(spec, SessionId::new());
        let mut controller = RecoveryController::new(ContextRecoveryDecision::Recovered);
        let mut model = Model {
            outputs: vec![
                Ok(ModelStepOutput::ToolCalls {
                    calls: vec![ProposedToolCall::new("c1", "repo.read", "{}").expect("call")],
                    tokens: 1,
                }),
                Err(ModelStepError::BoundExceeded),
            ]
            .into(),
        };
        let mut tools = RecordingAcceptingTools::new();
        let err = run_recovering(
            &request,
            &mut controller,
            ContextRetryPolicy::new(2),
            &mut model,
            &mut tools,
            &CancellationToken::new(),
        )
        .expect_err("must fail closed");
        assert_eq!(err, AgentExecutionError::ContextOverflowAfterEffects);
        // The controller was never consulted and the effect was executed once,
        // never replayed.
        assert_eq!(controller.calls, 0);
        assert_eq!(tools.executed.get(), 1);
        assert_eq!(
            err.error_code(),
            Some(protocol::ErrorCode::ProviderContextTooLarge)
        );
    }

    #[test]
    fn recovery_surfaces_provider_failure_cause_on_failed_turns() {
        let spec = spec(AgentRole::Coder, "implement", "work");
        let request = AgentExecutionRequest::new(spec, SessionId::new());
        let mut controller = RecoveryController::new(ContextRecoveryDecision::Recovered);
        // Each cause class survives to the outcome; the controller is never
        // consulted for a non-overflow failure.
        for (cause, expected) in [
            (FailureCause::Auth, Some(FailureCause::Auth)),
            (FailureCause::Connection, Some(FailureCause::Connection)),
            (FailureCause::Rejected, Some(FailureCause::Rejected)),
            (
                FailureCause::Transient {
                    retry_after_ms: Some(1200),
                },
                Some(FailureCause::Transient {
                    retry_after_ms: Some(1200),
                }),
            ),
            (FailureCause::Unspecified, Some(FailureCause::Unspecified)),
        ] {
            let mut model = Model {
                outputs: vec![Err(ModelStepError::ProviderFailed { cause })].into(),
            };
            let outcome = run_recovering(
                &request,
                &mut controller,
                ContextRetryPolicy::new(2),
                &mut model,
                &mut Tools,
                &CancellationToken::new(),
            )
            .expect("typed failure is an Ok(Failed) outcome");
            assert_eq!(outcome.result.status(), AgentTerminalStatus::Failed);
            assert_eq!(outcome.failure_cause, expected);
            assert_eq!(controller.calls, 0, "no compaction for provider failure");
        }
    }

    #[test]
    fn recovery_cancellation_during_rebuild_stops_execution() {
        // Controller reports cancellation → the host stops, never retries.
        let spec = spec(AgentRole::Coder, "implement", "work");
        let request = AgentExecutionRequest::new(spec, SessionId::new());
        let mut controller = RecoveryController::new(ContextRecoveryDecision::Cancelled);
        let mut model = overflow_then("never");
        let err = run_recovering(
            &request,
            &mut controller,
            ContextRetryPolicy::new(2),
            &mut model,
            &mut Tools,
            &CancellationToken::new(),
        )
        .expect_err("cancelled");
        assert_eq!(err, AgentExecutionError::Cancelled);
        assert_eq!(err.error_code(), None);
        assert_eq!(controller.calls, 1);

        // A pre-cancelled token aborts before any run / recovery.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut model = overflow_then("never");
        let err = run_recovering(
            &request,
            &mut RecoveryController::new(ContextRecoveryDecision::Recovered),
            ContextRetryPolicy::new(2),
            &mut model,
            &mut Tools,
            &cancel,
        )
        .expect_err("pre-cancelled");
        assert_eq!(err, AgentExecutionError::Cancelled);
    }

    #[test]
    fn recovery_reuses_same_budget_across_attempts_and_keeps_output() {
        // Reserved output budget is preserved because the executor derives the
        // same TurnBudget from the spec for every retry; the final terminal
        // output is intact and not truncated.
        let spec = spec(AgentRole::Coder, "implement", "work");
        let request = AgentExecutionRequest::new(spec, SessionId::new());
        let mut controller = RecoveryController::new(ContextRecoveryDecision::Recovered)
            .rebuilt(ContextRevision::new("context/compacted-2", None));
        let mut model = overflow_then("full final answer");
        let outcome = run_recovering(
            &request,
            &mut controller,
            ContextRetryPolicy::new(2),
            &mut model,
            &mut Tools,
            &CancellationToken::new(),
        )
        .expect("execute");
        let result = outcome.result;
        assert_eq!(result.summary(), "full final answer");
        assert_eq!(result.context_lineage().len(), 1);
    }

    #[test]
    fn executor_propagates_spec_budget_to_the_turn() {
        use crate::agent::model::AgentBudget;
        let budget = AgentBudget::new(None, None, None, Some(1));
        let spec = AgentSpec::builder(agent_id(), AgentRole::Coder, "task", view())
            .permissions_profile("work")
            .budget(budget)
            .build()
            .expect("spec");
        let request = AgentExecutionRequest::new(spec, SessionId::new());
        let mut model = Model {
            outputs: vec![Ok(ModelStepOutput::ToolCalls {
                calls: vec![
                    ProposedToolCall::new("c1", "repo.read", "{}").expect("c1"),
                    ProposedToolCall::new("c2", "repo.search", "{}").expect("c2"),
                ],
                tokens: 1,
            })]
            .into(),
        };
        let mut tools = RecordingAcceptingTools::new();
        let result = TurnAgentExecutor
            .execute(
                &request,
                &mut model,
                &mut tools,
                &mut Vec::new(),
                &CancellationToken::new(),
            )
            .expect("execute");
        assert_eq!(result.status(), AgentTerminalStatus::Failed);
        assert_eq!(
            tools.executed.get(),
            1,
            "spec budget gates the 2nd tool call"
        );
        assert_eq!(
            result.summary(),
            "agent turn failed",
            "budget exhaustion is not a fabricated success"
        );
    }

    #[test]
    fn executor_propagates_host_evidence_and_artifacts() {
        use protocol::{ArtifactId, ArtifactRef, EvidenceId, RedactionClass};
        let spec = spec(AgentRole::Coder, "x", "work");
        let eid = EvidenceId::new();
        let artifact = ArtifactRef::new(
            ArtifactId::from_bytes(b"artifact-1"),
            "text/plain",
            0,
            RedactionClass::Public,
        );
        let request = AgentExecutionRequest::new(spec, SessionId::new())
            .with_evidence(vec![eid])
            .with_artifacts(vec![artifact.clone()]);
        let result = TurnAgentExecutor
            .execute(
                &request,
                &mut terminal("done"),
                &mut Tools,
                &mut Vec::new(),
                &CancellationToken::new(),
            )
            .expect("execute");
        assert_eq!(result.evidence().len(), 1);
        assert_eq!(result.evidence()[0], eid);
        assert_eq!(result.artifacts().len(), 1);
        assert_eq!(result.artifacts()[0], artifact);
    }
}
