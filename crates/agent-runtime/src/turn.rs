//! Cancellable turn loop: model step → validated tool → model continuation.
//!
//! Terminal assistant output, budget exhaustion, and cancellation are the
//! only stop conditions. Kernel event kinds are emitted at each boundary.
//! An unhandled tool failure can never be reported as `turn.completed`.
//! Model text cannot override these checks.

use std::error::Error;
use std::fmt;

use protocol::{AgentId, ArtifactId, ErrorCode, SessionId, TurnId};

use crate::agent::model::CancellationToken;
use crate::loop_guard::ToolCallLoopDetector;

/// Hard ceiling on model steps in one turn, even if the spec budget is higher.
pub const MAX_MODEL_STEPS: u32 = 32;

/// Bounded retries for an empty model response before the turn fails clearly.
pub const EMPTY_RESPONSE_RETRY_LIMIT: u32 = 2;

/// Hard ceiling on tool calls accepted from one model step.
pub const MAX_TOOL_CALLS_PER_STEP: usize = 16;

/// Maximum events one turn may emit.
pub const MAX_TURN_EVENTS: usize = 512;

/// Maximum UTF-8 bytes accepted in a model `request_id` or tool `call_id`.
pub const MAX_CALL_ID_BYTES: usize = 128;

/// Maximum UTF-8 bytes accepted in a tool name.
pub const MAX_TOOL_NAME_BYTES: usize = 128;

/// Maximum UTF-8 bytes accepted in one tool-argument payload.
pub const MAX_ARGUMENT_BYTES: usize = 16 * 1024;

/// Maximum UTF-8 bytes accepted in terminal assistant text.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

const CANCEL_STRIDE: usize = 8;

/// Kernel event kinds this loop emits. Wire names match `EventKind`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum TurnEventKind {
    TurnStarted,
    TurnInterrupted,
    TurnCompleted,
    TurnFailed,
    ModelRequested,
    ModelCompleted,
    ModelFailed,
    ToolRequested,
    ToolStarted,
    ToolCompleted,
    ToolFailed,
    ToolDenied,
    ToolApprovalRequired,
}

/// Why a turn stopped without `turn.completed`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum TurnStopReason {
    Cancelled,
    BudgetExhausted,
    ModelFailed,
    ToolFailed,
    ApprovalRequired,
    RepeatedToolCall,
    EmptyResponse,
    ContextBoundExceeded,
}

/// Terminal class recorded on [`TurnResult`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum TurnStatus {
    Completed,
    Failed,
    Interrupted,
}

/// Typed turn-loop failure. Display never echoes model or argument text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum TurnError {
    Cancelled,
    InvalidBudget,
    InvalidToolCall,
    BoundExceeded,
    EventSink,
}

/// Model-step failure. Distinct from a terminal assistant message.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum ModelStepError {
    Cancelled,
    Failed,
    /// Provider-classified failure. `cause` is operator-actionable and never
    /// echoes provider bodies or credential material.
    ProviderFailed { cause: FailureCause },
    BoundExceeded,
}

/// Operator-actionable class of a provider/model failure. Carried as data so
/// provider distinctions survive the step → executor → CLI boundary instead
/// of collapsing into one "failed" status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum FailureCause {
    /// Credentials were rejected; fix the configured credential.
    Auth,
    /// The endpoint could not be reached or the connection broke; fix the
    /// network or the configured base URL.
    Connection,
    /// The provider rejected the request; fix the model id or request shape.
    Rejected,
    /// Temporary provider-side condition; the step layer retries within bounds.
    Transient { retry_after_ms: Option<u64> },
    /// Cause not provider-classified (internal or unclassified stream failure).
    Unspecified,
}

impl FailureCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "authentication",
            Self::Connection => "connection",
            Self::Rejected => "provider rejection",
            Self::Transient { .. } => "transient provider condition",
            Self::Unspecified => "unspecified failure",
        }
    }

    /// One-clause operator remedy. Static text only — never payload echo.
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::Auth => "check the configured API credential",
            Self::Connection => "check network reachability and the configured base_url",
            Self::Rejected => "check the model id and request shape",
            Self::Transient { .. } => "bounded retries were exhausted; try again later",
            Self::Unspecified => "no provider-classified detail is available",
        }
    }
}

/// Tool validate/execute failure that is never a structured model-visible result.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum ToolStepError {
    Cancelled,
    Invalid,
    Failed,
}

/// Optional ceilings. Unset axes stay unset; zero is exhausted, not unlimited.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TurnBudget {
    max_model_steps: u32,
    max_tool_calls: Option<u32>,
    max_tokens: Option<u64>,
}

/// Observed consumption for one [`run_turn`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct TurnUsage {
    model_steps: u32,
    tool_calls: u32,
    tokens: u64,
}

/// Model-proposed tool call. Structural bounds are enforced before dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposedToolCall {
    call_id: String,
    tool: String,
    arguments: String,
}

/// Call that passed structural and driver validation. Execute only this.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedToolCall {
    call_id: String,
    tool: String,
    arguments: String,
}

/// Input for one model step. Tool results from the previous step, if any.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelStepInput<'a> {
    step: u32,
    prior_tools: &'a [ToolStepResult],
    tool_surface: &'a [ToolSurface],
}

impl<'a> ModelStepInput<'a> {
    /// Harness seam: step input with no prior tool results (eval drivers).
    pub fn without_tools(step: u32) -> Self {
        Self {
            step,
            prior_tools: &[],
            tool_surface: &[],
        }
    }

    /// The tool surface the driver advertises to the model for this turn.
    pub fn tool_surface(&self) -> &'a [ToolSurface] {
        self.tool_surface
    }
}

/// Machine-controlled model output. Text cannot mark the turn complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelStepOutput {
    Terminal {
        text: String,
        tokens: u64,
    },
    ToolCalls {
        calls: Vec<ProposedToolCall>,
        tokens: u64,
    },
}

/// Structured tool outcome. [`ToolStepResult::Failed`] with `handled: false`
/// is an unhandled failure and cannot complete the turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolStepResult {
    Succeeded { call_id: String, summary: String },
    Failed { call_id: String, handled: bool },
    Denied { call_id: String },
    ApprovalRequired { call_id: String },
}

/// One kernel-shaped lifecycle event emitted by the loop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnEvent {
    Started {
        turn_id: TurnId,
    },
    Interrupted {
        turn_id: TurnId,
    },
    Completed {
        turn_id: TurnId,
    },
    Failed {
        turn_id: TurnId,
        reason: TurnStopReason,
    },
    ModelRequested {
        turn_id: TurnId,
        request_id: String,
        step: u32,
    },
    ModelCompleted {
        turn_id: TurnId,
        request_id: String,
        tokens: u64,
    },
    ModelFailed {
        turn_id: TurnId,
        request_id: String,
    },
    ToolRequested {
        turn_id: TurnId,
        call_id: String,
        tool: String,
    },
    ToolStarted {
        turn_id: TurnId,
        call_id: String,
        tool: String,
    },
    ToolCompleted {
        turn_id: TurnId,
        call_id: String,
        tool: String,
    },
    ToolFailed {
        turn_id: TurnId,
        call_id: String,
        tool: String,
    },
    ToolDenied {
        turn_id: TurnId,
        call_id: String,
        tool: String,
    },
    ToolApprovalRequired {
        turn_id: TurnId,
        call_id: String,
        tool: String,
    },
}

/// Bounded, UTF-8-safe final assistant response. Only the assistant-visible
/// answer; never hidden reasoning / chain-of-thought. Truncation is explicit.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct BoundedAssistantOutput {
    text: String,
    truncated: bool,
}

impl BoundedAssistantOutput {
    /// Hard byte cap for one terminal assistant output.
    pub const MAX_BYTES: usize = 16 * 1024;

    /// Build a bounded output. Truncates on a character boundary and records it.
    pub fn new(text: impl Into<String>) -> Self {
        let raw = text.into();
        if raw.len() <= Self::MAX_BYTES {
            Self {
                text: raw,
                truncated: false,
            }
        } else {
            let mut cut = Self::MAX_BYTES;
            while !raw.is_char_boundary(cut) {
                cut -= 1;
            }
            Self {
                text: raw[..cut].to_owned(),
                truncated: true,
            }
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Outcome after a terminal kernel event has been emitted.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TurnResult {
    turn_id: TurnId,
    status: TurnStatus,
    reason: Option<TurnStopReason>,
    usage: TurnUsage,
    /// SHA-256 of the terminal assistant message, if the turn completed with one.
    /// Never carries the raw text so it can be observed across goal-continuation
    /// turns without persisting a payload.
    terminal_hash: Option<ArtifactId>,
    /// Bounded final assistant-visible response, for an `AgentExecutor` to
    /// assemble a canonical `AgentResult`. Independent of `terminal_hash`.
    terminal_output: Option<BoundedAssistantOutput>,
    /// Provider-classified cause when the turn failed on a model step.
    failure_cause: Option<FailureCause>,
}

/// Collaborators and identity for one [`run_turn`].
pub struct TurnSpec<'a, M, T, E> {
    turn_id: TurnId,
    session_id: SessionId,
    agent_id: AgentId,
    budget: TurnBudget,
    model: &'a mut M,
    tools: &'a mut T,
    events: &'a mut E,
}

/// One cancellable model invocation.
pub trait ModelDriver {
    fn step(
        &mut self,
        input: &ModelStepInput<'_>,
        cancel: &CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError>;
}

/// Catalog/schema gate plus executor. The loop never executes an unvalidated call.
pub trait ToolDriver {
    fn validate(
        &mut self,
        call: &ProposedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ValidatedToolCall, ToolStepError>;

    fn execute(
        &mut self,
        call: &ValidatedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError>;

    /// Tools the driver can execute, advertised to the model so providers
    /// receive them as structured tool schemas. An empty surface means the
    /// turn sends no tool definitions (refusal stays fail-closed at
    /// `validate`).
    fn tool_surface(&self) -> Vec<ToolSurface> {
        Vec::new()
    }
}

/// Model-visible description of one tool a driver can execute. Carried as
/// data so providers can advertise the driver's surface without the turn
/// layer knowing tool semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolSurface {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

impl ToolSurface {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn parameters(&self) -> &serde_json::Value {
        &self.parameters
    }
}

/// Bounded sink for kernel turn/model/tool events.
pub trait TurnEventSink {
    fn emit(&mut self, event: TurnEvent) -> Result<(), TurnError>;
}

struct LoopState {
    turn_id: TurnId,
    budget: TurnBudget,
    usage: TurnUsage,
    unhandled_tool_failure: bool,
    loop_detector: ToolCallLoopDetector,
    empty_responses: u32,
}

enum StepDecision {
    Continue(Vec<ToolStepResult>),
    Stop(TurnResult),
}

impl TurnEventKind {
    /// Kernel `EventKind` wire form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnStarted => "turn.started",
            Self::TurnInterrupted => "turn.interrupted",
            Self::TurnCompleted => "turn.completed",
            Self::TurnFailed => "turn.failed",
            Self::ModelRequested => "model.requested",
            Self::ModelCompleted => "model.completed",
            Self::ModelFailed => "model.failed",
            Self::ToolRequested => "tool.requested",
            Self::ToolStarted => "tool.started",
            Self::ToolCompleted => "tool.completed",
            Self::ToolFailed => "tool.failed",
            Self::ToolDenied => "tool.denied",
            Self::ToolApprovalRequired => "tool.approval_required",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::TurnCompleted | Self::TurnFailed | Self::TurnInterrupted
        )
    }
}

impl TurnStopReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::BudgetExhausted => "budget_exhausted",
            Self::ModelFailed => "model_failed",
            Self::ToolFailed => "tool_failed",
            Self::ApprovalRequired => "approval_required",
            Self::RepeatedToolCall => "repeated_tool_call",
            Self::EmptyResponse => "empty_response",
            Self::ContextBoundExceeded => "context_bound_exceeded",
        }
    }
}

impl TurnStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

impl TurnError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "turn cancelled",
            Self::InvalidBudget => "turn budget is invalid",
            Self::InvalidToolCall => "tool call failed structural validation",
            Self::BoundExceeded => "turn resource bound exceeded",
            Self::EventSink => "turn event sink rejected an event",
        }
    }

    /// Public error code when this failure has a wire mapping.
    ///
    /// [`TurnError::Cancelled`] has no public code.
    pub const fn code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::InvalidBudget | Self::InvalidToolCall | Self::BoundExceeded => {
                Some(ErrorCode::ConfigInvalid)
            }
            Self::EventSink => Some(ErrorCode::InternalUnexpected),
        }
    }
}

impl ModelStepError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "model step cancelled",
            Self::Failed => "model step failed",
            Self::ProviderFailed { .. } => "model step failed (provider cause)",
            Self::BoundExceeded => "model step bound exceeded",
        }
    }
}

impl ToolStepError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "tool step cancelled",
            Self::Invalid => "tool call is invalid",
            Self::Failed => "tool step failed",
        }
    }
}

impl TurnBudget {
    pub const fn new(
        max_model_steps: u32,
        max_tool_calls: Option<u32>,
        max_tokens: Option<u64>,
    ) -> Result<Self, TurnError> {
        if max_model_steps == 0 || max_model_steps > MAX_MODEL_STEPS {
            return Err(TurnError::InvalidBudget);
        }
        Ok(Self {
            max_model_steps,
            max_tool_calls,
            max_tokens,
        })
    }

    pub const fn unlimited_steps() -> Self {
        Self {
            max_model_steps: MAX_MODEL_STEPS,
            max_tool_calls: None,
            max_tokens: None,
        }
    }

    pub const fn max_model_steps(self) -> u32 {
        self.max_model_steps
    }

    pub const fn max_tool_calls(self) -> Option<u32> {
        self.max_tool_calls
    }

    pub const fn max_tokens(self) -> Option<u64> {
        self.max_tokens
    }
}

impl TurnUsage {
    pub const fn new(model_steps: u32, tool_calls: u32, tokens: u64) -> Self {
        Self {
            model_steps,
            tool_calls,
            tokens,
        }
    }

    pub const fn model_steps(self) -> u32 {
        self.model_steps
    }

    pub const fn tool_calls(self) -> u32 {
        self.tool_calls
    }

    pub const fn tokens(self) -> u64 {
        self.tokens
    }
}

impl ProposedToolCall {
    pub fn new(
        call_id: impl Into<String>,
        tool: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Result<Self, TurnError> {
        let call = Self {
            call_id: call_id.into(),
            tool: tool.into(),
            arguments: arguments.into(),
        };
        validate_proposed(&call)?;
        Ok(call)
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn arguments(&self) -> &str {
        &self.arguments
    }
}

impl ValidatedToolCall {
    /// Mark a structurally accepted proposed call as executable.
    pub fn from_proposed(call: &ProposedToolCall) -> Self {
        Self {
            call_id: call.call_id.clone(),
            tool: call.tool.clone(),
            arguments: call.arguments.clone(),
        }
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn arguments(&self) -> &str {
        &self.arguments
    }
}

impl ModelStepInput<'_> {
    pub const fn step(&self) -> u32 {
        self.step
    }

    pub fn prior_tools(&self) -> &[ToolStepResult] {
        self.prior_tools
    }
}

impl ToolStepResult {
    pub fn call_id(&self) -> &str {
        match self {
            Self::Succeeded { call_id, .. }
            | Self::Failed { call_id, .. }
            | Self::Denied { call_id }
            | Self::ApprovalRequired { call_id } => call_id,
        }
    }

    pub const fn is_unhandled_failure(&self) -> bool {
        matches!(self, Self::Failed { handled: false, .. })
    }
}

impl TurnEvent {
    pub const fn kind(&self) -> TurnEventKind {
        match self {
            Self::Started { .. } => TurnEventKind::TurnStarted,
            Self::Interrupted { .. } => TurnEventKind::TurnInterrupted,
            Self::Completed { .. } => TurnEventKind::TurnCompleted,
            Self::Failed { .. } => TurnEventKind::TurnFailed,
            Self::ModelRequested { .. } => TurnEventKind::ModelRequested,
            Self::ModelCompleted { .. } => TurnEventKind::ModelCompleted,
            Self::ModelFailed { .. } => TurnEventKind::ModelFailed,
            Self::ToolRequested { .. } => TurnEventKind::ToolRequested,
            Self::ToolStarted { .. } => TurnEventKind::ToolStarted,
            Self::ToolCompleted { .. } => TurnEventKind::ToolCompleted,
            Self::ToolFailed { .. } => TurnEventKind::ToolFailed,
            Self::ToolDenied { .. } => TurnEventKind::ToolDenied,
            Self::ToolApprovalRequired { .. } => TurnEventKind::ToolApprovalRequired,
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        match self {
            Self::Started { turn_id }
            | Self::Interrupted { turn_id }
            | Self::Completed { turn_id }
            | Self::Failed { turn_id, .. }
            | Self::ModelRequested { turn_id, .. }
            | Self::ModelCompleted { turn_id, .. }
            | Self::ModelFailed { turn_id, .. }
            | Self::ToolRequested { turn_id, .. }
            | Self::ToolStarted { turn_id, .. }
            | Self::ToolCompleted { turn_id, .. }
            | Self::ToolFailed { turn_id, .. }
            | Self::ToolDenied { turn_id, .. }
            | Self::ToolApprovalRequired { turn_id, .. } => *turn_id,
        }
    }
}

impl TurnResult {
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn status(&self) -> TurnStatus {
        self.status
    }

    pub const fn reason(&self) -> Option<TurnStopReason> {
        self.reason
    }

    pub const fn usage(&self) -> TurnUsage {
        self.usage
    }

    /// Content hash of the completed terminal message, if any.
    pub const fn terminal_hash(&self) -> Option<ArtifactId> {
        self.terminal_hash
    }

    /// Bounded final assistant-visible output, if the turn completed with one.
    pub fn terminal_output(&self) -> Option<&BoundedAssistantOutput> {
        self.terminal_output.as_ref()
    }

    /// Provider-classified cause when the turn failed on a model step; `None`
    /// for completed, interrupted, budget/tool, and unclassified stops.
    pub const fn failure_cause(&self) -> Option<FailureCause> {
        self.failure_cause
    }
}

impl<'a, M, T, E> TurnSpec<'a, M, T, E> {
    pub fn new(
        turn_id: TurnId,
        session_id: SessionId,
        agent_id: AgentId,
        budget: TurnBudget,
        model: &'a mut M,
        tools: &'a mut T,
        events: &'a mut E,
    ) -> Self {
        Self {
            turn_id,
            session_id,
            agent_id,
            budget,
            model,
            tools,
            events,
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    pub const fn budget(&self) -> TurnBudget {
        self.budget
    }
}

impl TurnEventSink for Vec<TurnEvent> {
    fn emit(&mut self, event: TurnEvent) -> Result<(), TurnError> {
        if self.len() >= MAX_TURN_EVENTS {
            return Err(TurnError::BoundExceeded);
        }
        self.push(event);
        Ok(())
    }
}

/// Execute model step → validated tool → model continuation until a stop.
///
/// Emits `turn.started`, then model/tool events, then exactly one of
/// `turn.completed`, `turn.failed`, or `turn.interrupted`.
pub fn run_turn<M, T, E>(
    spec: TurnSpec<'_, M, T, E>,
    cancel: &CancellationToken,
) -> Result<TurnResult, TurnError>
where
    M: ModelDriver,
    T: ToolDriver,
    E: TurnEventSink,
{
    check_cancel(cancel)?;
    let mut state = LoopState {
        turn_id: spec.turn_id,
        budget: spec.budget,
        usage: TurnUsage::default(),
        unhandled_tool_failure: false,
        loop_detector: ToolCallLoopDetector::new(),
        empty_responses: 0,
    };
    let TurnSpec {
        model,
        tools,
        events,
        ..
    } = spec;

    emit(
        events,
        TurnEvent::Started {
            turn_id: state.turn_id,
        },
    )?;

    let mut prior_tools: Vec<ToolStepResult> = Vec::new();
    loop {
        match run_model_step(&mut state, model, tools, events, &prior_tools, cancel)? {
            StepDecision::Continue(next) => prior_tools = next,
            StepDecision::Stop(result) => return Ok(result),
        }
    }
}

fn run_model_step<M, T, E>(
    state: &mut LoopState,
    model: &mut M,
    tools: &mut T,
    events: &mut E,
    prior_tools: &[ToolStepResult],
    cancel: &CancellationToken,
) -> Result<StepDecision, TurnError>
where
    M: ModelDriver,
    T: ToolDriver,
    E: TurnEventSink,
{
    if let Some(stop) = stop_if_cancelled(state, events, cancel)? {
        return Ok(StepDecision::Stop(stop));
    }
    if state.unhandled_tool_failure {
        let stop = fail(state, events, TurnStopReason::ToolFailed)?;
        return Ok(StepDecision::Stop(stop));
    }
    if model_budget_exhausted(state) {
        let stop = fail(state, events, TurnStopReason::BudgetExhausted)?;
        return Ok(StepDecision::Stop(stop));
    }

    let step = state.usage.model_steps.saturating_add(1);
    let request_id = model_request_id(step);
    emit(
        events,
        TurnEvent::ModelRequested {
            turn_id: state.turn_id,
            request_id: request_id.clone(),
            step,
        },
    )?;

    if let Some(stop) = stop_if_cancelled(state, events, cancel)? {
        return Ok(StepDecision::Stop(stop));
    }

    let surface = tools.tool_surface();
    let input = ModelStepInput {
        step,
        prior_tools,
        tool_surface: &surface,
    };
    let output = match model.step(&input, cancel) {
        Ok(output) => output,
        Err(ModelStepError::Cancelled) => {
            emit(
                events,
                TurnEvent::ModelFailed {
                    turn_id: state.turn_id,
                    request_id,
                },
            )?;
            return Ok(StepDecision::Stop(interrupt(state, events)?));
        }
        Err(ModelStepError::Failed) => {
            emit(
                events,
                TurnEvent::ModelFailed {
                    turn_id: state.turn_id,
                    request_id,
                },
            )?;
            return Ok(StepDecision::Stop(fail_with_cause(
                state,
                events,
                TurnStopReason::ModelFailed,
                Some(FailureCause::Unspecified),
            )?));
        }
        Err(ModelStepError::ProviderFailed { cause }) => {
            emit(
                events,
                TurnEvent::ModelFailed {
                    turn_id: state.turn_id,
                    request_id,
                },
            )?;
            return Ok(StepDecision::Stop(fail_with_cause(
                state,
                events,
                TurnStopReason::ModelFailed,
                Some(cause),
            )?));
        }
        Err(ModelStepError::BoundExceeded) => {
            emit(
                events,
                TurnEvent::ModelFailed {
                    turn_id: state.turn_id,
                    request_id,
                },
            )?;
            // A context/bound overflow is a recovery candidate, NOT a provider
            // failure. It is surfaced as a distinct typed outcome so the
            // context-owning layer can decide whether to compact and retry.
            return Ok(StepDecision::Stop(fail(
                state,
                events,
                TurnStopReason::ContextBoundExceeded,
            )?));
        }
    };

    if let Some(stop) = stop_if_cancelled(state, events, cancel)? {
        return Ok(StepDecision::Stop(stop));
    }

    let tokens = match &output {
        ModelStepOutput::Terminal { text, tokens } => {
            if text.len() > MAX_TEXT_BYTES {
                emit(
                    events,
                    TurnEvent::ModelFailed {
                        turn_id: state.turn_id,
                        request_id,
                    },
                )?;
                return Ok(StepDecision::Stop(fail(
                    state,
                    events,
                    TurnStopReason::ModelFailed,
                )?));
            }
            *tokens
        }
        ModelStepOutput::ToolCalls { calls, tokens } => {
            if calls.len() > MAX_TOOL_CALLS_PER_STEP {
                emit(
                    events,
                    TurnEvent::ModelFailed {
                        turn_id: state.turn_id,
                        request_id,
                    },
                )?;
                return Ok(StepDecision::Stop(fail(
                    state,
                    events,
                    TurnStopReason::ModelFailed,
                )?));
            }
            *tokens
        }
    };

    state.usage.model_steps = step;
    state.usage.tokens = state.usage.tokens.saturating_add(tokens);
    emit(
        events,
        TurnEvent::ModelCompleted {
            turn_id: state.turn_id,
            request_id,
            tokens,
        },
    )?;

    let empty_response = match &output {
        ModelStepOutput::Terminal { text, .. } => text.is_empty(),
        ModelStepOutput::ToolCalls { calls, .. } => calls.is_empty(),
    };
    if empty_response {
        state.empty_responses = state.empty_responses.saturating_add(1);
        if state.empty_responses <= EMPTY_RESPONSE_RETRY_LIMIT {
            // Bounded retry: re-invoke the model for the same step with the same
            // prior tool results, consuming model budget. An empty response is
            // never fabricated into assistant content.
            return Ok(StepDecision::Continue(prior_tools.to_vec()));
        }
        return Ok(StepDecision::Stop(fail(
            state,
            events,
            TurnStopReason::EmptyResponse,
        )?));
    }

    match output {
        ModelStepOutput::Terminal { text, .. } => {
            if state.unhandled_tool_failure {
                Ok(StepDecision::Stop(fail(
                    state,
                    events,
                    TurnStopReason::ToolFailed,
                )?))
            } else {
                Ok(StepDecision::Stop(complete(state, events, Some(&text))?))
            }
        }
        ModelStepOutput::ToolCalls { calls, .. } => {
            if calls.is_empty() {
                return Ok(StepDecision::Stop(fail(
                    state,
                    events,
                    TurnStopReason::ModelFailed,
                )?));
            }
            run_tool_steps(state, tools, events, calls, cancel)
        }
    }
}

fn run_tool_steps<T, E>(
    state: &mut LoopState,
    tools: &mut T,
    events: &mut E,
    calls: Vec<ProposedToolCall>,
    cancel: &CancellationToken,
) -> Result<StepDecision, TurnError>
where
    T: ToolDriver,
    E: TurnEventSink,
{
    let mut results = Vec::new();
    for (index, call) in calls.into_iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE)
            && let Some(stop) = stop_if_cancelled(state, events, cancel)?
        {
            return Ok(StepDecision::Stop(stop));
        }
        if tool_budget_exhausted(state) {
            return Ok(StepDecision::Stop(fail(
                state,
                events,
                TurnStopReason::BudgetExhausted,
            )?));
        }
        state.loop_detector.observe(call.tool(), call.arguments());
        if state.loop_detector.is_looping() {
            return Ok(StepDecision::Stop(fail(
                state,
                events,
                TurnStopReason::RepeatedToolCall,
            )?));
        }
        match run_one_tool(state, tools, events, call, cancel)? {
            StepDecision::Continue(mut batch) => results.append(&mut batch),
            stop => return Ok(stop),
        }
    }
    if state.unhandled_tool_failure {
        return Ok(StepDecision::Stop(fail(
            state,
            events,
            TurnStopReason::ToolFailed,
        )?));
    }
    Ok(StepDecision::Continue(results))
}

fn run_one_tool<T, E>(
    state: &mut LoopState,
    tools: &mut T,
    events: &mut E,
    call: ProposedToolCall,
    cancel: &CancellationToken,
) -> Result<StepDecision, TurnError>
where
    T: ToolDriver,
    E: TurnEventSink,
{
    if let Some(stop) = stop_if_cancelled(state, events, cancel)? {
        return Ok(StepDecision::Stop(stop));
    }

    emit(
        events,
        TurnEvent::ToolRequested {
            turn_id: state.turn_id,
            call_id: call.call_id.clone(),
            tool: call.tool.clone(),
        },
    )?;

    if let Err(err) = validate_proposed(&call) {
        emit_tool_failed(state, events, &call)?;
        state.unhandled_tool_failure = true;
        return match err {
            TurnError::Cancelled => Ok(StepDecision::Stop(interrupt(state, events)?)),
            _ => Ok(StepDecision::Stop(fail(
                state,
                events,
                TurnStopReason::ToolFailed,
            )?)),
        };
    }

    if let Some(stop) = stop_if_cancelled(state, events, cancel)? {
        return Ok(StepDecision::Stop(stop));
    }

    let validated = match tools.validate(&call, cancel) {
        Ok(validated) => validated,
        Err(ToolStepError::Cancelled) => {
            emit_tool_failed(state, events, &call)?;
            return Ok(StepDecision::Stop(interrupt(state, events)?));
        }
        Err(ToolStepError::Invalid | ToolStepError::Failed) => {
            emit_tool_failed(state, events, &call)?;
            state.unhandled_tool_failure = true;
            return Ok(StepDecision::Stop(fail(
                state,
                events,
                TurnStopReason::ToolFailed,
            )?));
        }
    };

    if validated.call_id != call.call_id || validated.tool != call.tool {
        emit_tool_failed(state, events, &call)?;
        state.unhandled_tool_failure = true;
        return Ok(StepDecision::Stop(fail(
            state,
            events,
            TurnStopReason::ToolFailed,
        )?));
    }

    emit(
        events,
        TurnEvent::ToolStarted {
            turn_id: state.turn_id,
            call_id: call.call_id.clone(),
            tool: call.tool.clone(),
        },
    )?;

    if let Some(stop) = stop_if_cancelled(state, events, cancel)? {
        return Ok(StepDecision::Stop(stop));
    }

    let result = match tools.execute(&validated, cancel) {
        Ok(result) => result,
        Err(ToolStepError::Cancelled) => {
            emit_tool_failed(state, events, &call)?;
            return Ok(StepDecision::Stop(interrupt(state, events)?));
        }
        Err(ToolStepError::Invalid | ToolStepError::Failed) => {
            emit_tool_failed(state, events, &call)?;
            state.unhandled_tool_failure = true;
            return Ok(StepDecision::Stop(fail(
                state,
                events,
                TurnStopReason::ToolFailed,
            )?));
        }
    };

    if let Some(stop) = stop_if_cancelled(state, events, cancel)? {
        return Ok(StepDecision::Stop(stop));
    }

    state.usage.tool_calls = state.usage.tool_calls.saturating_add(1);
    emit_tool_result(state, events, &call, &result)?;

    if matches!(result, ToolStepResult::ApprovalRequired { .. }) {
        return Ok(StepDecision::Stop(fail(
            state,
            events,
            TurnStopReason::ApprovalRequired,
        )?));
    }
    if result.is_unhandled_failure() {
        state.unhandled_tool_failure = true;
        return Ok(StepDecision::Stop(fail(
            state,
            events,
            TurnStopReason::ToolFailed,
        )?));
    }
    Ok(StepDecision::Continue(vec![result]))
}

fn emit_tool_result<E: TurnEventSink>(
    state: &LoopState,
    events: &mut E,
    call: &ProposedToolCall,
    result: &ToolStepResult,
) -> Result<(), TurnError> {
    let event = match result {
        ToolStepResult::Succeeded { .. } => TurnEvent::ToolCompleted {
            turn_id: state.turn_id,
            call_id: call.call_id.clone(),
            tool: call.tool.clone(),
        },
        ToolStepResult::Failed { .. } => TurnEvent::ToolFailed {
            turn_id: state.turn_id,
            call_id: call.call_id.clone(),
            tool: call.tool.clone(),
        },
        ToolStepResult::Denied { .. } => TurnEvent::ToolDenied {
            turn_id: state.turn_id,
            call_id: call.call_id.clone(),
            tool: call.tool.clone(),
        },
        ToolStepResult::ApprovalRequired { .. } => TurnEvent::ToolApprovalRequired {
            turn_id: state.turn_id,
            call_id: call.call_id.clone(),
            tool: call.tool.clone(),
        },
    };
    emit(events, event)
}

fn emit_tool_failed<E: TurnEventSink>(
    state: &LoopState,
    events: &mut E,
    call: &ProposedToolCall,
) -> Result<(), TurnError> {
    emit(
        events,
        TurnEvent::ToolFailed {
            turn_id: state.turn_id,
            call_id: call.call_id.clone(),
            tool: call.tool.clone(),
        },
    )
}

fn complete<E: TurnEventSink>(
    state: &LoopState,
    events: &mut E,
    terminal_text: Option<&str>,
) -> Result<TurnResult, TurnError> {
    if state.unhandled_tool_failure {
        return fail(state, events, TurnStopReason::ToolFailed);
    }
    emit(
        events,
        TurnEvent::Completed {
            turn_id: state.turn_id,
        },
    )?;
    Ok(TurnResult {
        turn_id: state.turn_id,
        status: TurnStatus::Completed,
        reason: None,
        usage: state.usage,
        terminal_hash: terminal_text.map(|text| ArtifactId::from_bytes(text.as_bytes())),
        terminal_output: terminal_text.map(BoundedAssistantOutput::new),
        failure_cause: None,
    })
}

fn fail<E: TurnEventSink>(
    state: &LoopState,
    events: &mut E,
    reason: TurnStopReason,
) -> Result<TurnResult, TurnError> {
    fail_with_cause(state, events, reason, None)
}

/// Like [`fail`], but records the provider-classified cause that produced the
/// stop so the executor and CLI can name it. Only provider-caused model stops
/// carry a cause; budget/tool/empty-response stops do not.
fn fail_with_cause<E: TurnEventSink>(
    state: &LoopState,
    events: &mut E,
    reason: TurnStopReason,
    cause: Option<FailureCause>,
) -> Result<TurnResult, TurnError> {
    emit(
        events,
        TurnEvent::Failed {
            turn_id: state.turn_id,
            reason,
        },
    )?;
    Ok(TurnResult {
        turn_id: state.turn_id,
        status: TurnStatus::Failed,
        reason: Some(reason),
        usage: state.usage,
        terminal_hash: None,
        terminal_output: None,
        failure_cause: cause,
    })
}

fn interrupt<E: TurnEventSink>(state: &LoopState, events: &mut E) -> Result<TurnResult, TurnError> {
    emit(
        events,
        TurnEvent::Interrupted {
            turn_id: state.turn_id,
        },
    )?;
    Ok(TurnResult {
        turn_id: state.turn_id,
        status: TurnStatus::Interrupted,
        reason: Some(TurnStopReason::Cancelled),
        usage: state.usage,
        terminal_hash: None,
        terminal_output: None,
        failure_cause: None,
    })
}

fn stop_if_cancelled<E: TurnEventSink>(
    state: &LoopState,
    events: &mut E,
    cancel: &CancellationToken,
) -> Result<Option<TurnResult>, TurnError> {
    if cancel.is_cancelled() {
        Ok(Some(interrupt(state, events)?))
    } else {
        Ok(None)
    }
}

fn model_budget_exhausted(state: &LoopState) -> bool {
    if state.usage.model_steps >= state.budget.max_model_steps {
        return true;
    }
    if let Some(max_tokens) = state.budget.max_tokens
        && state.usage.tokens >= max_tokens
    {
        return true;
    }
    false
}

fn tool_budget_exhausted(state: &LoopState) -> bool {
    match state.budget.max_tool_calls {
        Some(limit) => state.usage.tool_calls >= limit,
        None => false,
    }
}

fn emit<E: TurnEventSink>(events: &mut E, event: TurnEvent) -> Result<(), TurnError> {
    events.emit(event).map_err(|_| TurnError::EventSink)
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), TurnError> {
    if cancel.is_cancelled() {
        Err(TurnError::Cancelled)
    } else {
        Ok(())
    }
}

fn validate_proposed(call: &ProposedToolCall) -> Result<(), TurnError> {
    if !valid_ident(&call.call_id, MAX_CALL_ID_BYTES) {
        return Err(TurnError::InvalidToolCall);
    }
    if !valid_ident(&call.tool, MAX_TOOL_NAME_BYTES) {
        return Err(TurnError::InvalidToolCall);
    }
    if call.arguments.len() > MAX_ARGUMENT_BYTES {
        return Err(TurnError::InvalidToolCall);
    }
    if call.arguments.chars().any(|c| c.is_control()) {
        return Err(TurnError::InvalidToolCall);
    }
    Ok(())
}

fn valid_ident(value: &str, max_bytes: usize) -> bool {
    if value.is_empty() || value.len() > max_bytes {
        return false;
    }
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-' || b == b':')
}

fn model_request_id(step: u32) -> String {
    let mut id = String::from("m");
    let mut n = step;
    let mut digits = [0u8; 10];
    let mut len = 0;
    if n == 0 {
        id.push('0');
        return id;
    }
    while n > 0 && len < digits.len() {
        digits[len] = b'0' + (n % 10) as u8;
        n /= 10;
        len += 1;
    }
    for i in (0..len).rev() {
        id.push(digits[i] as char);
    }
    id
}

impl fmt::Display for TurnEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for TurnStopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for TurnStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for TurnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ModelStepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ToolStepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for TurnError {}
impl Error for ModelStepError {}
impl Error for ToolStepError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct ScriptedModel {
        outputs: VecDeque<Result<ModelStepOutput, ModelStepError>>,
        seen: u32,
        cancel_at: Option<u32>,
        token: Option<CancellationToken>,
    }

    struct ScriptedTools {
        outcomes: VecDeque<Result<ToolStepResult, ToolStepError>>,
        validate_err: Option<ToolStepError>,
        executed: u32,
        cancel_on_execute: bool,
        token: Option<CancellationToken>,
    }

    impl ScriptedModel {
        fn new(outputs: Vec<Result<ModelStepOutput, ModelStepError>>) -> Self {
            Self {
                outputs: outputs.into(),
                seen: 0,
                cancel_at: None,
                token: None,
            }
        }

        fn cancel_at(mut self, step: u32, token: CancellationToken) -> Self {
            self.cancel_at = Some(step);
            self.token = Some(token);
            self
        }
    }

    impl ScriptedTools {
        fn new(outcomes: Vec<Result<ToolStepResult, ToolStepError>>) -> Self {
            Self {
                outcomes: outcomes.into(),
                validate_err: None,
                executed: 0,
                cancel_on_execute: false,
                token: None,
            }
        }

        fn reject(err: ToolStepError) -> Self {
            Self {
                outcomes: VecDeque::new(),
                validate_err: Some(err),
                executed: 0,
                cancel_on_execute: false,
                token: None,
            }
        }

        fn cancel_on_execute(mut self, token: CancellationToken) -> Self {
            self.cancel_on_execute = true;
            self.token = Some(token);
            self
        }
    }

    impl ModelDriver for ScriptedModel {
        fn step(
            &mut self,
            input: &ModelStepInput<'_>,
            cancel: &CancellationToken,
        ) -> Result<ModelStepOutput, ModelStepError> {
            if cancel.is_cancelled() {
                return Err(ModelStepError::Cancelled);
            }
            self.seen = self.seen.saturating_add(1);
            if self.cancel_at == Some(input.step)
                && let Some(token) = &self.token
            {
                token.cancel();
                return Err(ModelStepError::Cancelled);
            }
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
            if let Some(err) = self.validate_err {
                return Err(err);
            }
            Ok(ValidatedToolCall {
                call_id: call.call_id.clone(),
                tool: call.tool.clone(),
                arguments: call.arguments.clone(),
            })
        }

        fn execute(
            &mut self,
            call: &ValidatedToolCall,
            cancel: &CancellationToken,
        ) -> Result<ToolStepResult, ToolStepError> {
            if cancel.is_cancelled() {
                return Err(ToolStepError::Cancelled);
            }
            if self.cancel_on_execute {
                if let Some(token) = &self.token {
                    token.cancel();
                }
                return Err(ToolStepError::Cancelled);
            }
            self.executed = self.executed.saturating_add(1);
            match self.outcomes.pop_front() {
                Some(Ok(result)) => Ok(result),
                Some(Err(err)) => Err(err),
                None => Ok(ToolStepResult::Succeeded {
                    call_id: call.call_id.clone(),
                    summary: "ok".to_owned(),
                }),
            }
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn terminal(text: &str, tokens: u64) -> Result<ModelStepOutput, ModelStepError> {
        Ok(ModelStepOutput::Terminal {
            text: text.to_owned(),
            tokens,
        })
    }

    fn tools_out(
        calls: Vec<ProposedToolCall>,
        tokens: u64,
    ) -> Result<ModelStepOutput, ModelStepError> {
        Ok(ModelStepOutput::ToolCalls { calls, tokens })
    }

    fn call(id: &str, tool: &str) -> ProposedToolCall {
        ProposedToolCall::new(id, tool, "{}").expect("call")
    }

    fn kinds(events: &[TurnEvent]) -> Vec<&'static str> {
        events.iter().map(|event| event.kind().as_str()).collect()
    }

    fn run(
        budget: TurnBudget,
        model: &mut ScriptedModel,
        tools: &mut ScriptedTools,
        events: &mut Vec<TurnEvent>,
        cancel: &CancellationToken,
    ) -> Result<TurnResult, TurnError> {
        run_turn(
            TurnSpec::new(
                TurnId::new(),
                SessionId::new(),
                AgentId::new(),
                budget,
                model,
                tools,
                events,
            ),
            cancel,
        )
    }

    #[test]
    fn tool_surface_flows_from_the_driver_into_model_input() {
        struct SurfaceModel {
            seen: Vec<Vec<String>>,
        }
        impl ModelDriver for SurfaceModel {
            fn step(
                &mut self,
                input: &ModelStepInput<'_>,
                _cancel: &CancellationToken,
            ) -> Result<ModelStepOutput, ModelStepError> {
                self.seen.push(
                    input
                        .tool_surface()
                        .iter()
                        .map(|entry| entry.name().to_owned())
                        .collect(),
                );
                Ok(ModelStepOutput::Terminal {
                    text: "done".to_owned(),
                    tokens: 1,
                })
            }
        }
        struct SurfacedTools(ScriptedTools);
        impl ToolDriver for SurfacedTools {
            fn validate(
                &mut self,
                call: &ProposedToolCall,
                cancel: &CancellationToken,
            ) -> Result<ValidatedToolCall, ToolStepError> {
                self.0.validate(call, cancel)
            }

            fn execute(
                &mut self,
                call: &ValidatedToolCall,
                cancel: &CancellationToken,
            ) -> Result<ToolStepResult, ToolStepError> {
                self.0.execute(call, cancel)
            }

            fn tool_surface(&self) -> Vec<ToolSurface> {
                vec![ToolSurface::new(
                    "t.one",
                    "does one thing",
                    serde_json::json!({"type": "object"}),
                )]
            }
        }
        let mut model = SurfaceModel { seen: Vec::new() };
        let mut tools = SurfacedTools(ScriptedTools::new(Vec::new()));
        let mut events = Vec::new();
        let result = run_turn(
            TurnSpec::new(
                TurnId::new(),
                SessionId::new(),
                AgentId::new(),
                TurnBudget::unlimited_steps(),
                &mut model,
                &mut tools,
                &mut events,
            ),
            &CancellationToken::new(),
        )
        .expect("turn");
        assert_eq!(result.status(), TurnStatus::Completed);
        assert_eq!(model.seen, vec![vec!["t.one".to_owned()]]);
    }

    #[test]
    fn terminal_output_is_bounded_utf8_safe_and_truncates_deterministically() {
        let short = BoundedAssistantOutput::new("hello");
        assert_eq!(short.text(), "hello");
        assert!(!short.truncated());
        assert_eq!(
            BoundedAssistantOutput::new("hello"),
            BoundedAssistantOutput::new("hello")
        );
        // Multi-byte char-boundary truncation never splits a char, is bounded, and
        // is deterministic for the same input.
        let big = "é".repeat(BoundedAssistantOutput::MAX_BYTES);
        let out = BoundedAssistantOutput::new(big.clone());
        assert!(out.truncated());
        assert!(std::str::from_utf8(out.text().as_bytes()).is_ok());
        assert!(out.text().len() <= BoundedAssistantOutput::MAX_BYTES);
        assert_eq!(out, BoundedAssistantOutput::new(big));
    }

    #[test]
    fn terminal_visible_output_is_exposed_on_a_completed_turn() {
        let mut model = ScriptedModel::new(vec![terminal("final answer", 4)]);
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Completed);
        let out = result.terminal_output().expect("output");
        assert_eq!(out.text(), "final answer");
        assert!(!out.truncated());
        assert!(!out.text().contains("chain-of-thought"));
        // A failed turn carries no terminal output.
        let mut fail_model = ScriptedModel::new(vec![Err(ModelStepError::Failed)]);
        let failed = run(
            TurnBudget::unlimited_steps(),
            &mut fail_model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("fail");
        assert_eq!(failed.reason(), Some(TurnStopReason::ModelFailed));
        assert!(failed.terminal_output().is_none());
    }

    #[test]
    fn terminal_model_emits_started_model_completed() {
        let mut model = ScriptedModel::new(vec![terminal("done", 4)]);
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Completed);
        assert_eq!(result.reason(), None);
        assert_eq!(result.usage().model_steps(), 1);
        assert_eq!(result.usage().tokens(), 4);
        assert_eq!(
            kinds(&events),
            [
                "turn.started",
                "model.requested",
                "model.completed",
                "turn.completed",
            ]
        );
        assert!(
            events
                .iter()
                .all(|event| event.turn_id() == result.turn_id())
        );
    }

    #[test]
    fn model_then_validated_tool_then_model_continuation() {
        let call = call("c1", "repo.read");
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call.clone()], 2),
            terminal("after tool", 3),
        ]);
        let mut tools = ScriptedTools::new(vec![Ok(ToolStepResult::Succeeded {
            call_id: "c1".to_owned(),
            summary: "read".to_owned(),
        })]);
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Completed);
        assert_eq!(result.usage().tool_calls(), 1);
        assert_eq!(result.usage().model_steps(), 2);
        assert_eq!(tools.executed, 1);
        assert_eq!(
            kinds(&events),
            [
                "turn.started",
                "model.requested",
                "model.completed",
                "tool.requested",
                "tool.started",
                "tool.completed",
                "model.requested",
                "model.completed",
                "turn.completed",
            ]
        );
    }

    #[test]
    fn unhandled_tool_failure_never_emits_completed() {
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("c1", "shell.exec")], 1),
            terminal("should not complete", 1),
        ]);
        let mut tools = ScriptedTools::new(vec![Ok(ToolStepResult::Failed {
            call_id: "c1".to_owned(),
            handled: false,
        })]);
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(result.reason(), Some(TurnStopReason::ToolFailed));
        assert!(
            events
                .iter()
                .any(|event| event.kind() == TurnEventKind::ToolFailed)
        );
        assert!(
            !events
                .iter()
                .any(|event| event.kind() == TurnEventKind::TurnCompleted)
        );
        assert_eq!(
            events.last().map(TurnEvent::kind),
            Some(TurnEventKind::TurnFailed)
        );
        assert_eq!(model.seen, 1);
    }

    #[test]
    fn tool_execute_error_is_unhandled_and_not_complete() {
        let mut model = ScriptedModel::new(vec![tools_out(vec![call("c1", "repo.search")], 1)]);
        let mut tools = ScriptedTools::new(vec![Err(ToolStepError::Failed)]);
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(result.reason(), Some(TurnStopReason::ToolFailed));
        assert!(!kinds(&events).contains(&"turn.completed"));
        assert!(kinds(&events).contains(&"tool.started"));
        assert!(kinds(&events).contains(&"tool.failed"));
        assert_eq!(tools.executed, 1);
    }

    #[test]
    fn repeated_exact_tool_call_is_a_loop_and_never_completes() {
        // The model issues the exact same tool call 3 times; the third identical
        // call trips the loop detector before it is executed.
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("c1", "repo.search")], 1),
            tools_out(vec![call("c1", "repo.search")], 1),
            tools_out(vec![call("c1", "repo.search")], 1),
        ]);
        let mut tools = ScriptedTools::new(vec![
            Ok(ToolStepResult::Succeeded {
                call_id: "c1".to_owned(),
                summary: "hit".to_owned(),
            }),
            Ok(ToolStepResult::Succeeded {
                call_id: "c1".to_owned(),
                summary: "hit".to_owned(),
            }),
        ]);
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(result.reason(), Some(TurnStopReason::RepeatedToolCall));
        assert_eq!(tools.executed, 2, "the 3rd identical call must not execute");
        assert!(!kinds(&events).contains(&"turn.completed"));
        assert!(kinds(&events).contains(&"turn.failed"));
    }

    #[test]
    fn empty_response_retries_then_succeeds_within_budget() {
        let mut model = ScriptedModel::new(vec![terminal("", 1), terminal("ok", 2)]);
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Completed);
        assert_eq!(result.reason(), None);
        assert_eq!(model.seen, 2, "one bounded retry after the empty response");
    }

    #[test]
    fn repeated_empty_response_is_a_bounded_stop_and_never_completes() {
        let mut model = ScriptedModel::new(vec![terminal("", 1), terminal("", 1), terminal("", 1)]);
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(result.reason(), Some(TurnStopReason::EmptyResponse));
        assert_eq!(model.seen, 3);
        assert!(!kinds(&events).contains(&"turn.completed"));
        assert!(kinds(&events).contains(&"turn.failed"));
    }

    #[test]
    fn empty_response_retry_respects_cancellation() {
        let cancel = live();
        let mut model = ScriptedModel::new(vec![terminal("", 1)]).cancel_at(2, cancel.clone());
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &cancel,
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Interrupted);
        assert_eq!(result.reason(), Some(TurnStopReason::Cancelled));
        assert!(kinds(&events).contains(&"turn.interrupted"));
    }

    #[test]
    fn empty_response_retry_does_not_replay_already_executed_tool_effects() {
        // step1 executes callA (one effect). step2 is an empty response that is
        // retried. The retry must NOT re-execute callA: it re-invokes the model
        // with the prior tool result as an observation, and the next model step
        // simply finishes. Only one tool execution occurs.
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("c1", "repo.search")], 1),
            terminal("", 1),
            terminal("done", 1),
        ]);
        let mut tools = ScriptedTools::new(vec![Ok(ToolStepResult::Succeeded {
            call_id: "c1".to_owned(),
            summary: "hit".to_owned(),
        })]);
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Completed);
        assert_eq!(
            tools.executed, 1,
            "the retry must not replay the executed tool call"
        );
        assert_eq!(model.seen, 3);
    }

    #[test]
    fn provider_error_is_not_treated_as_empty_response() {
        let mut model = ScriptedModel::new(vec![Err(ModelStepError::Failed)]);
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(result.reason(), Some(TurnStopReason::ModelFailed));
        assert_eq!(
            model.seen, 1,
            "a provider error is never retried as an empty response"
        );
    }

    #[test]
    fn context_bound_overflow_is_distinct_from_provider_failure_and_cancellation() {
        let mut tools = ScriptedTools::new(Vec::new());

        let mut overflow_model = ScriptedModel::new(vec![Err(ModelStepError::BoundExceeded)]);
        let mut overflow_events = Vec::new();
        let overflow = run(
            TurnBudget::unlimited_steps(),
            &mut overflow_model,
            &mut tools,
            &mut overflow_events,
            &live(),
        )
        .expect("overflow");
        assert_eq!(
            overflow.reason(),
            Some(TurnStopReason::ContextBoundExceeded)
        );
        assert_eq!(overflow.status(), TurnStatus::Failed);

        let mut fail_model = ScriptedModel::new(vec![Err(ModelStepError::Failed)]);
        let mut fail_events = Vec::new();
        let failed = run(
            TurnBudget::unlimited_steps(),
            &mut fail_model,
            &mut tools,
            &mut fail_events,
            &live(),
        )
        .expect("provider failure");
        assert_eq!(failed.reason(), Some(TurnStopReason::ModelFailed));

        let cancel = live();
        let mut cancel_model = ScriptedModel::new(vec![Err(ModelStepError::Cancelled)]);
        let mut cancel_events = Vec::new();
        let cancelled = run(
            TurnBudget::unlimited_steps(),
            &mut cancel_model,
            &mut tools,
            &mut cancel_events,
            &cancel,
        )
        .expect("cancellation");
        assert_eq!(cancelled.reason(), Some(TurnStopReason::Cancelled));
        assert_eq!(cancelled.status(), TurnStatus::Interrupted);

        // The distinction is typed; no string matching is required.
        assert_ne!(overflow.reason(), failed.reason());
        assert_ne!(overflow.reason(), cancelled.reason());
    }

    #[test]
    fn invalid_tool_is_not_executed() {
        let mut model = ScriptedModel::new(vec![tools_out(vec![call("c1", "repo.read")], 1)]);
        let mut tools = ScriptedTools::reject(ToolStepError::Invalid);
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(tools.executed, 0);
        assert!(kinds(&events).contains(&"tool.requested"));
        assert!(!kinds(&events).contains(&"tool.started"));
        assert!(kinds(&events).contains(&"tool.failed"));
        assert!(!kinds(&events).contains(&"turn.completed"));
    }

    #[test]
    fn structural_invalid_tool_is_not_executed() {
        let bad = ProposedToolCall {
            call_id: String::new(),
            tool: "repo.read".to_owned(),
            arguments: "{}".to_owned(),
        };
        let mut model = ScriptedModel::new(vec![tools_out(vec![bad], 1)]);
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(tools.executed, 0);
        assert!(!kinds(&events).contains(&"tool.started"));
        assert!(!kinds(&events).contains(&"turn.completed"));
    }

    #[test]
    fn handled_tool_failure_continues_to_model() {
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("c1", "repo.read")], 1),
            terminal("recovered", 1),
        ]);
        let mut tools = ScriptedTools::new(vec![Ok(ToolStepResult::Failed {
            call_id: "c1".to_owned(),
            handled: true,
        })]);
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Completed);
        assert!(kinds(&events).contains(&"tool.failed"));
        assert!(kinds(&events).contains(&"turn.completed"));
        assert_eq!(model.seen, 2);
    }

    #[test]
    fn every_model_and_tool_step_is_cancellable() {
        let cancel = live();
        cancel.cancel();
        let mut model = ScriptedModel::new(vec![terminal("nope", 1)]);
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let err = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &cancel,
        )
        .expect_err("pre-start cancel");
        assert_eq!(err, TurnError::Cancelled);
        assert!(events.is_empty());
        assert_eq!(TurnError::Cancelled.code(), None);

        let cancel = live();
        let mut model = ScriptedModel::new(vec![terminal("nope", 1)]).cancel_at(1, cancel.clone());
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &cancel,
        )
        .expect("model cancel");
        assert_eq!(result.status(), TurnStatus::Interrupted);
        assert_eq!(result.reason(), Some(TurnStopReason::Cancelled));
        assert!(kinds(&events).contains(&"turn.started"));
        assert!(kinds(&events).contains(&"model.requested"));
        assert!(kinds(&events).contains(&"turn.interrupted"));
        assert!(!kinds(&events).contains(&"turn.completed"));

        let cancel = live();
        let mut model = ScriptedModel::new(vec![tools_out(vec![call("c1", "repo.read")], 1)]);
        let mut tools = ScriptedTools::new(Vec::new()).cancel_on_execute(cancel.clone());
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &cancel,
        )
        .expect("tool cancel");
        assert_eq!(result.status(), TurnStatus::Interrupted);
        assert!(kinds(&events).contains(&"tool.started"));
        assert!(kinds(&events).contains(&"turn.interrupted"));
        assert!(!kinds(&events).contains(&"turn.completed"));
    }

    #[test]
    fn budget_exhaustion_fails_and_does_not_complete() {
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("c1", "repo.read")], 1),
            terminal("second", 1),
        ]);
        let mut tools = ScriptedTools::new(vec![Ok(ToolStepResult::Succeeded {
            call_id: "c1".to_owned(),
            summary: "ok".to_owned(),
        })]);
        let mut events = Vec::new();
        let budget = TurnBudget::new(1, None, None).expect("budget");
        let result = run(budget, &mut model, &mut tools, &mut events, &live()).expect("run");
        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(result.reason(), Some(TurnStopReason::BudgetExhausted));
        assert_eq!(result.usage().model_steps(), 1);
        assert_eq!(model.seen, 1);
        assert!(!kinds(&events).contains(&"turn.completed"));
        assert_eq!(
            events.last().map(TurnEvent::kind),
            Some(TurnEventKind::TurnFailed)
        );
    }

    #[test]
    fn token_budget_blocks_continuation() {
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("c1", "repo.read")], 10),
            terminal("nope", 1),
        ]);
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let budget = TurnBudget::new(8, None, Some(10)).expect("budget");
        let result = run(budget, &mut model, &mut tools, &mut events, &live()).expect("run");
        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(result.reason(), Some(TurnStopReason::BudgetExhausted));
        assert_eq!(model.seen, 1);
        assert!(!kinds(&events).contains(&"turn.completed"));
    }

    #[test]
    fn zero_or_oversized_budget_is_rejected() {
        assert_eq!(
            TurnBudget::new(0, None, None),
            Err(TurnError::InvalidBudget)
        );
        assert_eq!(
            TurnBudget::new(MAX_MODEL_STEPS + 1, None, None),
            Err(TurnError::InvalidBudget)
        );
    }

    #[test]
    fn model_failure_emits_failed_not_completed() {
        let mut model = ScriptedModel::new(vec![Err(ModelStepError::Failed)]);
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(result.reason(), Some(TurnStopReason::ModelFailed));
        assert_eq!(
            kinds(&events),
            [
                "turn.started",
                "model.requested",
                "model.failed",
                "turn.failed",
            ]
        );
    }

    #[test]
    fn approval_required_stops_without_complete() {
        let mut model = ScriptedModel::new(vec![tools_out(vec![call("c1", "workspace.patch")], 1)]);
        let mut tools = ScriptedTools::new(vec![Ok(ToolStepResult::ApprovalRequired {
            call_id: "c1".to_owned(),
        })]);
        let mut events = Vec::new();
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(result.reason(), Some(TurnStopReason::ApprovalRequired));
        assert!(kinds(&events).contains(&"tool.approval_required"));
        assert!(!kinds(&events).contains(&"turn.completed"));
    }

    #[test]
    fn kernel_event_kind_wire_forms_are_stable() {
        assert_eq!(TurnEventKind::TurnStarted.as_str(), "turn.started");
        assert_eq!(TurnEventKind::TurnInterrupted.as_str(), "turn.interrupted");
        assert_eq!(TurnEventKind::TurnCompleted.as_str(), "turn.completed");
        assert_eq!(TurnEventKind::TurnFailed.as_str(), "turn.failed");
        assert_eq!(TurnEventKind::ModelRequested.as_str(), "model.requested");
        assert_eq!(TurnEventKind::ModelCompleted.as_str(), "model.completed");
        assert_eq!(TurnEventKind::ModelFailed.as_str(), "model.failed");
        assert_eq!(TurnEventKind::ToolRequested.as_str(), "tool.requested");
        assert_eq!(TurnEventKind::ToolStarted.as_str(), "tool.started");
        assert_eq!(TurnEventKind::ToolCompleted.as_str(), "tool.completed");
        assert_eq!(TurnEventKind::ToolFailed.as_str(), "tool.failed");
        assert_eq!(TurnEventKind::ToolDenied.as_str(), "tool.denied");
        assert_eq!(
            TurnEventKind::ToolApprovalRequired.as_str(),
            "tool.approval_required"
        );
        assert!(TurnEventKind::TurnCompleted.is_terminal());
        assert!(!TurnEventKind::ModelCompleted.is_terminal());
        assert_eq!(model_request_id(1), "m1");
        assert_eq!(model_request_id(12), "m12");
    }
}
