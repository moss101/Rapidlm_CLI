//! Cancellable turn loop: model step → validated tool → model continuation.
//!
//! Terminal assistant output, budget exhaustion, and cancellation are the
//! only stop conditions. Kernel event kinds are emitted at each boundary.
//! An unhandled tool failure can never be reported as `turn.completed`.
//! Model text cannot override these checks.

use std::error::Error;
use std::fmt;

use protocol::{AgentId, ArtifactId, ErrorCode, SessionId, TurnId};
use serde::{Deserialize, Serialize};

use crate::agent::model::CancellationToken;
use crate::loop_guard::ToolCallLoopDetector;

/// Hard ceiling on model steps in one turn, even if the spec budget is higher.
pub const MAX_MODEL_STEPS: u32 = 32;

/// Hard ceiling on tool calls accepted from one model step.
pub const MAX_TOOL_CALLS_PER_STEP: usize = 16;

/// Maximum events one turn may emit. Computed from the turn's own other
/// hard ceilings, not a fixed guess: `Started` + one terminal event, plus
/// every step using its full allowance (`ModelRequested`/`ModelCompleted`
/// per step; `ToolRequested`/`ToolStarted`/one terminal tool event per
/// call). A cap sized independently of `MAX_MODEL_STEPS`/
/// `MAX_TOOL_CALLS_PER_STEP` — as a flat `512` previously was — can be hit
/// well inside the turn's own advertised budget (crossed at roughly 150
/// total tool calls, against a nominal ceiling of `32 * 16 = 512`), and
/// hitting it mid-batch silently discards already-executed tool side
/// effects (their results are never recorded) and ends the turn with no
/// terminal event at all — an ordinary heavy-tool-use turn, not just an
/// adversarial one, could trigger this. Deriving the cap from the same
/// constants that bound steps/calls makes it provably impossible to hit
/// under any turn this crate's own budget already allows, rather than
/// hoping a fixed number stays ahead of them as those change. `+ 8` is
/// slack for defensive margin, not load-bearing.
pub const MAX_TURN_EVENTS: usize =
    2 + (MAX_MODEL_STEPS as usize) * (2 + MAX_TOOL_CALLS_PER_STEP * 3) + 8;

/// Maximum UTF-8 bytes accepted in a model `request_id` or tool `call_id`.
pub const MAX_CALL_ID_BYTES: usize = 128;

/// Maximum UTF-8 bytes accepted in a tool name.
pub const MAX_TOOL_NAME_BYTES: usize = 128;

/// Maximum UTF-8 bytes accepted in one tool-argument payload.
pub const MAX_ARGUMENT_BYTES: usize = 16 * 1024;

/// Maximum UTF-8 bytes accepted in terminal assistant text.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

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
    ModelContinued,
    ModelFailed,
    ToolRequested,
    ToolStarted,
    ToolCompleted,
    ToolFailed,
    ToolDenied,
    ToolApprovalRequired,
    ToolContextRequired,
}

/// Why a turn stopped without `turn.completed`.
///
/// `ContextRequired` is distinct from `ContextBoundExceeded`: the latter is
/// the model's own context *window* overflowing (real material exists but
/// doesn't fit — a host/context-owner concern, repaired by
/// `execute_with_context_recovery`'s compaction-and-retry loop, never by
/// asking the user anything). `ContextRequired` is the model itself
/// reporting, through a real structured call (`ToolStepResult::
/// ContextRequired`, today only reachable via `ask_user` with no live
/// answer source), that the *user* needs to supply something no amount of
/// retrying or compacting can produce — an ambiguous target, a missing
/// preference, a credential only the user has. The two must never be
/// conflated: promoting ordinary window overflow to "ask the user" would be
/// wrong exactly as often as compaction would be wrong for a genuine
/// information gap.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
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
    ContextRequired,
}

/// Why a turn suspended awaiting a human, rather than failing.
///
/// A [`TurnStopReason::ApprovalRequired`] or [`TurnStopReason::ContextRequired`]
/// stop is not a malfunction: the turn is *paused* until an operator resolves
/// the pending tool approval or answers the model's question. The host owns
/// that wait (durable, across restarts) and resumes the exact turn afterward,
/// so the stop must carry everything the resume needs: the completed tool
/// exchanges so far — including the batch holding the pending call's
/// placeholder result — and which call is pending. Side effects already
/// committed before the suspension are inside `history` and are never
/// re-executed; the pending call itself has not run and is executed exactly
/// once, on resume.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct TurnSuspension {
    reason: TurnStopReason,
    call_id: String,
    /// Completed exchanges of this turn in order, ending with the batch that
    /// holds the pending call's `ApprovalRequired`/`ContextRequired` result.
    /// Callers synthesize a `Failed` placeholder for any batch call that
    /// errored at the step layer, so providers always see one result per
    /// proposed `call_id`.
    history: Vec<ToolStepExchange>,
    /// Exchanges dropped from the front of `history` by the byte bound —
    /// model-visible as "earlier steps omitted", never silently lost.
    omitted_earlier_steps: u32,
}

/// Byte budget for a suspension's serialized history. Tool results are each
/// bounded far below this (`MAX_RESULT_DETAIL_BYTES`); the bound exists so a
/// pathological turn cannot balloon the durable payload a host stores per
/// suspension, not because real turns approach it.
pub const MAX_SUSPENSION_HISTORY_BYTES: usize = 256 * 1024;

impl TurnSuspension {
    /// Build a suspension from the loop's state at the stop. `batch_calls` /
    /// `batch_results` are this step's proposed calls and their per-call
    /// outcomes in proposal order; an `Err` outcome becomes a synthesized
    /// `Failed` placeholder so the recorded exchange stays one-result-per-call.
    /// Public because the approval flow's tests exercise the exact construction
    /// the loop performs.
    pub fn new(
        reason: TurnStopReason,
        call_id: String,
        prior_history: &[ToolStepExchange],
        batch_calls: &[ProposedToolCall],
        batch_results: &[Result<ToolStepResult, ToolStepError>],
    ) -> Self {
        let batch: Vec<ToolStepResult> = batch_results
            .iter()
            .map(|outcome| {
                outcome
                    .clone()
                    .unwrap_or_else(|err| ToolStepResult::Failed {
                        call_id: String::new(),
                        handled: false,
                        detail: Some(err.as_str().to_owned()),
                    })
            })
            .collect();
        let mut history = prior_history.to_vec();
        history.push(ToolStepExchange::new(batch_calls.to_vec(), batch));
        // Enforce the byte bound by dropping whole exchanges from the front —
        // never a partial exchange, which would desync calls from results.
        let mut omitted = 0u32;
        while Self::history_bytes(&history) > MAX_SUSPENSION_HISTORY_BYTES && history.len() > 1 {
            history.remove(0);
            omitted += 1;
        }
        Self {
            reason,
            call_id,
            history,
            omitted_earlier_steps: omitted,
        }
    }

    fn history_bytes(history: &[ToolStepExchange]) -> usize {
        history
            .iter()
            .map(|exchange| {
                let mut bytes = 0usize;
                for call in exchange.calls() {
                    bytes += call.call_id.len() + call.tool().len() + call.arguments().len();
                }
                for result in exchange.results() {
                    bytes += result.serialized_size();
                }
                bytes
            })
            .sum()
    }

    pub const fn reason(&self) -> TurnStopReason {
        self.reason
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn history(&self) -> &[ToolStepExchange] {
        &self.history
    }

    pub const fn omitted_earlier_steps(&self) -> u32 {
        self.omitted_earlier_steps
    }
}

/// Bounded detail attached to a stop that needs one: [`TurnStopReason::
/// ToolFailed`] (`tool` names the failing call; `error` is the bounded
/// one-line underlying outcome) and [`TurnStopReason::ContextRequired`]
/// (`tool` is always the tool that raised it, e.g. `"ask_user"`; `error`
/// carries the model's own question text, not an error message — reused
/// here rather than a second, parallel struct shaped identically). Safe for
/// operator/user display in both cases — never a substitute for the
/// structured model-visible result.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TurnFailureDetail {
    tool: String,
    error: String,
}

/// Byte bound for the operator-facing error text in [`TurnFailureDetail`].
/// Sized to hold a `ContextRequired` question in full: `ask_user` bounds a
/// question to 1024 bytes and its options to 8 entries of up to 256 bytes
/// each (see `apps/rapid/src/exec_tools.rs`'s `parse_ask_user_args`), so the
/// combined question-plus-options text this stop attaches never actually
/// hits this bound in practice — the bound exists to keep the type honestly
/// finite, not to routinely truncate it. `ToolFailed`'s own error text is
/// already pre-bounded far below this (see `MAX_RESULT_DETAIL_BYTES`) before
/// it ever reaches here, so raising this bound changes nothing for it.
const FAILURE_DETAIL_ERROR_BYTES: usize = 4096;

impl TurnFailureDetail {
    pub fn new(tool: impl Into<String>, error: &str) -> Self {
        let mut end = FAILURE_DETAIL_ERROR_BYTES.min(error.len());
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            tool: tool.into(),
            error: error[..end].to_owned(),
        }
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn error(&self) -> &str {
        &self.error
    }
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
    ProviderFailed {
        cause: FailureCause,
    },
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
    /// The account has no quota or credit left (HTTP 402); never retried.
    Quota,
    /// A proxy on the path refused its own credentials (HTTP 407); never
    /// retried — asking again sends the same ones.
    ProxyAuth,
    /// Temporary provider-side condition; the step layer retries within bounds.
    /// `rate_limited`: the provider said to slow down (HTTP 429) rather than
    /// failing on its side (5xx, a dropped stream) — a per-model retry
    /// policy may treat the two differently.
    Transient {
        retry_after_ms: Option<u64>,
        rate_limited: bool,
    },
    /// Cause not provider-classified (internal or unclassified stream failure).
    Unspecified,
}

impl FailureCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "authentication",
            Self::Connection => "connection",
            Self::Rejected => "provider rejection",
            Self::Quota => "exhausted provider quota",
            Self::ProxyAuth => "proxy authentication",
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
            Self::Quota => "check the provider account's quota or billing",
            Self::ProxyAuth => "check the user and password in the proxy variable",
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
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
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
/// `pending_calls` are the proposed calls that produced `prior_tools` (empty
/// One completed tool step: the calls the model proposed and their per-call
/// results, kept as an ordered pair. The full sequence of exchanges within a
/// turn is the model's working memory — each request carries every prior
/// exchange (bounded by the request layer), not just the latest one.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ToolStepExchange {
    calls: Vec<ProposedToolCall>,
    results: Vec<ToolStepResult>,
}

impl ToolStepExchange {
    pub fn new(calls: Vec<ProposedToolCall>, results: Vec<ToolStepResult>) -> Self {
        Self { calls, results }
    }

    pub fn calls(&self) -> &[ProposedToolCall] {
        &self.calls
    }

    pub fn results(&self) -> &[ToolStepResult] {
        &self.results
    }
}

/// Input for one model step. `history` holds every completed tool exchange
/// of the turn in order (empty before the first tool step), so the request
/// layer can replay the whole assistant/tool conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelStepInput<'a> {
    step: u32,
    history: &'a [ToolStepExchange],
    tool_surface: &'a [ToolSurface],
    /// Tokens the turn's budget still allows (`None`: no token budget): a
    /// driver that makes more than one request for a step (a continuation)
    /// stops asking once it is spent.
    token_allowance: Option<u64>,
}

impl<'a> ModelStepInput<'a> {
    /// Build a step input from a full exchange history (public so
    /// request-construction tests can drive the same shape the loop
    /// produces).
    pub fn with_history(
        step: u32,
        history: &'a [ToolStepExchange],
        tool_surface: &'a [ToolSurface],
    ) -> Self {
        Self {
            step,
            history,
            tool_surface,
            token_allowance: None,
        }
    }

    /// The same input with a token allowance (see [`Self::token_allowance`]).
    pub fn with_token_allowance(mut self, allowance: Option<u64>) -> Self {
        self.token_allowance = allowance;
        self
    }

    /// Tokens the turn's budget still allows, when it has a token budget.
    pub fn token_allowance(&self) -> Option<u64> {
        self.token_allowance
    }

    /// Harness seam: step input with no prior tool results (eval drivers).
    pub fn without_tools(step: u32) -> Self {
        Self {
            step,
            history: &[],
            tool_surface: &[],
            token_allowance: None,
        }
    }

    /// The tool surface the driver advertises to the model for this turn.
    pub fn tool_surface(&self) -> &'a [ToolSurface] {
        self.tool_surface
    }

    /// Every completed tool exchange of the turn, oldest first.
    pub fn history(&self) -> &'a [ToolStepExchange] {
        self.history
    }

    /// Results of the most recent tool step (empty before the first step).
    pub fn prior_tools(&self) -> &'a [ToolStepResult] {
        self.history
            .last()
            .map(|exchange| exchange.results.as_slice())
            .unwrap_or(&[])
    }

    /// Calls of the most recent tool step (empty before the first step).
    pub fn pending_calls(&self) -> &'a [ProposedToolCall] {
        self.history
            .last()
            .map(|exchange| exchange.calls.as_slice())
            .unwrap_or(&[])
    }
}

/// Machine-controlled model output. Text cannot mark the turn complete.
///
/// `cost_usd_micros` is the provider-reported dollar cost of this step, in
/// millionths of a US dollar (integer, avoids float rounding), when the
/// backing model-invoker can report one. `None` means "unknown," never
/// "free" — most `ModelDriver`/harness implementations (scripted test
/// doubles, non-live drivers) have no real cost to report and should pass
/// `None`, not `Some(0)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelStepOutput {
    Terminal {
        text: String,
        tokens: u64,
        cost_usd_micros: Option<u64>,
    },
    ToolCalls {
        calls: Vec<ProposedToolCall>,
        tokens: u64,
        cost_usd_micros: Option<u64>,
    },
}

/// Structured tool outcome. [`ToolStepResult::Failed`] with `handled: false`
/// is an unhandled failure and cannot complete the turn. `Denied` carries the
/// typed refusal reason text so a headless denial is model-visible, never a
/// silent pass; detail text is bounded by the producing driver.
///
/// `ContextRequired` is distinct from `Failed`/`Denied`: the *call itself*
/// executed correctly (a driver asked its own configured way of reaching a
/// human, e.g. `ask_user`), it simply had no live answer source to read from
/// — the model asked a well-formed question that only a human can answer,
/// not a malfunction. Stops the turn the same unconditional way
/// `ApprovalRequired` does (see `dispatch_prepared`'s own handling of both)
/// rather than letting the loop continue on the model's own judgment, since
/// silently nudging the model to guess is exactly the behavior this variant
/// exists to replace. `question` is the driver's own bounded question text
/// (e.g. `ask_user`'s own `question` argument, already capped by its parser)
/// — carried here, not reconstructed from prose downstream, so the specific
/// question a user sees is the model's own, not a generic substitute.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ToolStepResult {
    Succeeded {
        call_id: String,
        summary: String,
    },
    Failed {
        call_id: String,
        handled: bool,
        detail: Option<String>,
    },
    Denied {
        call_id: String,
        detail: Option<String>,
    },
    ApprovalRequired {
        call_id: String,
    },
    ContextRequired {
        call_id: String,
        question: String,
    },
}

impl ToolStepResult {
    /// Bounded size estimate for suspension budgeting: the fields a recorded
    /// exchange persists, not a full JSON serialization.
    pub fn serialized_size(&self) -> usize {
        match self {
            Self::Succeeded { call_id, summary } => call_id.len() + summary.len(),
            Self::Failed {
                call_id,
                detail: Some(detail),
                ..
            } => call_id.len() + detail.len(),
            Self::Failed { call_id, .. } => call_id.len(),
            Self::Denied {
                call_id,
                detail: Some(detail),
            } => call_id.len() + detail.len(),
            Self::Denied { call_id, .. } => call_id.len(),
            Self::ApprovalRequired { call_id } => call_id.len(),
            Self::ContextRequired { call_id, question } => call_id.len() + question.len(),
        }
    }
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
    /// One request of a step that continued a length-truncated answer:
    /// `index` 0 is the step's first request, 1.. the continuations, all of
    /// `continuation_of` (the step's request id); `tokens` is that request's
    /// own share of the step's total (S3: a stitched answer is never silent).
    ModelContinued {
        turn_id: TurnId,
        continuation_of: String,
        index: u32,
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
        /// Why the call was refused, when the driver supplied a reason.
        ///
        /// Carried because the same text is the *user's* only guide to what
        /// to do next — for a permission denial it names the remedy — and it
        /// previously reached only the model, as the tool result. Optional:
        /// a `ToolDriver` is not obliged to explain itself.
        reason: Option<String>,
    },
    ToolApprovalRequired {
        turn_id: TurnId,
        call_id: String,
        tool: String,
    },
    /// A tool call that stopped the turn because the model needs
    /// information only the user can supply. Mirrors
    /// [`TurnEvent::ToolApprovalRequired`]'s own dedicated-shape precedent
    /// rather than reusing `ToolFailed`'s: the model didn't fail anything,
    /// so surfacing it as a failure marker in a UI would misrepresent the
    /// outcome. The question itself lives on the turn's terminal
    /// `TurnResult.failure_detail`, not here — this is only the per-call
    /// progress marker.
    ToolContextRequired {
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
    /// Failing-tool detail when the turn stopped on [`TurnStopReason::ToolFailed`].
    failure_detail: Option<TurnFailureDetail>,
    /// Pending human input when the turn stopped on
    /// [`TurnStopReason::ApprovalRequired`] / [`TurnStopReason::ContextRequired`]:
    /// everything a host needs to resume the exact turn without repeating
    /// committed side effects. `None` for every other stop.
    suspension: Option<TurnSuspension>,
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

    /// The requests the last successful step made, when it continued a
    /// length-truncated answer: each request's tokens, the first request
    /// first. Empty when the step made one request (the usual case). Taking
    /// them clears them.
    fn take_continuations(&mut self) -> Vec<u64> {
        Vec::new()
    }

    /// Tokens the driver was billed for that belong to no answer and no
    /// continuation record (a fallen-back model's discarded work), taken
    /// when a step fails so they still count. Default: none.
    fn take_uncounted_tokens(&mut self) -> u64 {
        0
    }
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

    /// Dispatch class of one advertised tool. Defaults to write: unknown or
    /// unclassified tools never run concurrently.
    fn tool_kind(&self, tool: &str) -> ToolKind {
        let _ = tool;
        ToolKind::Write
    }

    /// Completed background-job notifications pending replay to the model.
    /// Empty by default; drivers with background state return one synthetic
    /// exchange per newly finished job so the model learns the outcome
    /// without polling.
    fn drain_notifications(&mut self) -> Vec<ToolStepExchange> {
        Vec::new()
    }

    /// Execute an already-validated batch, returning one outcome per call in
    /// proposal order. The default runs the calls sequentially; drivers whose
    /// calls are independent override this to dispatch concurrently, with
    /// same-path (or otherwise conflicting) writes serialized inside the
    /// override. A per-call failure is a `ToolStepResult`, never a batch
    /// abort.
    fn execute_batch(
        &mut self,
        calls: &[ValidatedToolCall],
        cancel: &CancellationToken,
    ) -> Vec<Result<ToolStepResult, ToolStepError>> {
        calls
            .iter()
            .map(|call| self.execute(call, cancel))
            .collect()
    }
}

/// Read/write classification the parallel dispatcher uses to decide what may
/// run concurrently.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ToolKind {
    /// Pure read: may run concurrently with reads and writes.
    Read,
    /// Mutates workspace state (file write/patch, process execution).
    Write,
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
    /// Most recent failing tool, kept so every `ToolFailed` stop — including
    /// leftover-state stops with no call at hand — can name it.
    failure_detail: Option<TurnFailureDetail>,
    loop_detector: ToolCallLoopDetector,
    /// Set once by the batch dispatcher when the turn suspends on an approval
    /// or a clarification; `fail`/`interrupt` copy it into the `TurnResult`.
    suspension: Option<TurnSuspension>,
}

enum StepDecision {
    /// A completed tool step: append its exchange to the turn history.
    Continue(ToolStepExchange),
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
            Self::ModelContinued => "model.continued",
            Self::ModelFailed => "model.failed",
            Self::ToolRequested => "tool.requested",
            Self::ToolStarted => "tool.started",
            Self::ToolCompleted => "tool.completed",
            Self::ToolFailed => "tool.failed",
            Self::ToolDenied => "tool.denied",
            Self::ToolApprovalRequired => "tool.approval_required",
            Self::ToolContextRequired => "tool.context_required",
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
            Self::ContextRequired => "context_required",
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

    /// Reconstruct a call that already passed [`Self::new`] once — the resume
    /// path replaying a recorded suspension's history. Deliberately
    /// non-validating: the bounds were enforced when the call was first
    /// accepted, and re-validating a replay could reject what a prior turn
    /// lawfully ran (bounds may tighten between processes). Never feed this
    /// fresh model input; that path must go through `new`.
    pub fn replay(
        call_id: impl Into<String>,
        tool: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool: tool.into(),
            arguments: arguments.into(),
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
}

impl ToolStepResult {
    pub fn call_id(&self) -> &str {
        match self {
            Self::Succeeded { call_id, .. }
            | Self::Failed { call_id, .. }
            | Self::Denied { call_id, .. }
            | Self::ApprovalRequired { call_id }
            | Self::ContextRequired { call_id, .. } => call_id,
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
            Self::ModelContinued { .. } => TurnEventKind::ModelContinued,
            Self::ModelFailed { .. } => TurnEventKind::ModelFailed,
            Self::ToolRequested { .. } => TurnEventKind::ToolRequested,
            Self::ToolStarted { .. } => TurnEventKind::ToolStarted,
            Self::ToolCompleted { .. } => TurnEventKind::ToolCompleted,
            Self::ToolFailed { .. } => TurnEventKind::ToolFailed,
            Self::ToolDenied { .. } => TurnEventKind::ToolDenied,
            Self::ToolApprovalRequired { .. } => TurnEventKind::ToolApprovalRequired,
            Self::ToolContextRequired { .. } => TurnEventKind::ToolContextRequired,
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
            | Self::ModelContinued { turn_id, .. }
            | Self::ModelFailed { turn_id, .. }
            | Self::ToolRequested { turn_id, .. }
            | Self::ToolStarted { turn_id, .. }
            | Self::ToolCompleted { turn_id, .. }
            | Self::ToolFailed { turn_id, .. }
            | Self::ToolDenied { turn_id, .. }
            | Self::ToolApprovalRequired { turn_id, .. }
            | Self::ToolContextRequired { turn_id, .. } => *turn_id,
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

    /// Failing-tool detail when the turn stopped on
    /// [`TurnStopReason::ToolFailed`]; `None` for every other stop.
    pub fn failure_detail(&self) -> Option<&TurnFailureDetail> {
        self.failure_detail.as_ref()
    }

    /// Pending human input when the turn suspended on an approval or a
    /// clarification; `None` for every other stop.
    pub fn suspension(&self) -> Option<&TurnSuspension> {
        self.suspension.as_ref()
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
    run_turn_seeded(spec, Vec::new(), cancel)
}

/// [`run_turn`] continued from prior completed exchanges: the resume path for
/// a turn that suspended on an approval or a clarification. `seed_history` is
/// the suspension's own recorded history — including the exchange holding the
/// pending call's placeholder result, which the host replaces with the real
/// post-resolution outcome before calling this — so the model's working
/// memory is exactly what it was when the turn paused.
pub fn run_turn_seeded<M, T, E>(
    spec: TurnSpec<'_, M, T, E>,
    seed_history: Vec<ToolStepExchange>,
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
        failure_detail: None,
        loop_detector: ToolCallLoopDetector::new(),
        suspension: None,
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

    let mut history = seed_history;
    loop {
        // Absorb completed background-job notifications so the model learns
        // their outcome without polling.
        for exchange in tools.drain_notifications() {
            history.push(exchange);
        }
        match run_model_step(&mut state, model, tools, events, &history, cancel)? {
            StepDecision::Continue(exchange) => history.push(exchange),
            StepDecision::Stop(result) => return Ok(result),
        }
    }
}

fn run_model_step<M, T, E>(
    state: &mut LoopState,
    model: &mut M,
    tools: &mut T,
    events: &mut E,
    history: &[ToolStepExchange],
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
        history,
        tool_surface: &surface,
        token_allowance: state
            .budget
            .max_tokens
            .map(|max| max.saturating_sub(state.usage.tokens)),
    };
    let stepped = model.step(&input, cancel);
    // A step that failed after continuing still made those requests: they
    // are on record before its failure.
    if stepped.is_err() {
        // Billed, and the step's failure carries no tokens: they count here.
        let spent = emit_continuations(model, state, events, &request_id)?
            .saturating_add(model.take_uncounted_tokens());
        state.usage.tokens = state.usage.tokens.saturating_add(spent);
    }
    let output = match stepped {
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
                None,
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
                None,
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
        ModelStepOutput::Terminal { text, tokens, .. } => {
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
        ModelStepOutput::ToolCalls { calls, tokens, .. } => {
            // A `call_id` repeated across two calls in one step breaks the
            // one-to-one id-to-call assumption every downstream consumer
            // makes (`apps/rapid/src/exec_tools.rs::batch_dispatch` restores
            // results by index rather than id, so it survives this, but a
            // `ModelDriver` that folds a provider stream keyed on `call_id` —
            // the real one does, in `apps/rapid/src/model.rs::fold_stream` —
            // can silently misattribute one call's argument deltas onto
            // another's entry, dispatching a real tool call built from the
            // wrong arguments while both entries still report the same id).
            // Rejected here, at the trait-contract boundary, so no
            // `ModelDriver` implementation can smuggle this past the turn
            // loop regardless of how it assembles a step's proposed calls.
            let has_duplicate_call_id = {
                let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
                !calls.iter().all(|call| seen.insert(call.call_id()))
            };
            if calls.len() > MAX_TOOL_CALLS_PER_STEP || has_duplicate_call_id {
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
    // A continued answer: one record per request, before the step's
    // completion, each counted in the step's tokens above.
    let _ = emit_continuations(model, state, events, &request_id)?;
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
        // Retrying an empty response is the host's job now: `SupervisedModel`
        // (apps/rapid/src/host.rs) already retries an empty terminal response
        // up to its own bounded limit with exponential backoff before this
        // layer ever sees the result. A second independent retry loop here
        // would compound with that one (each retry re-enters `SupervisedModel`
        // fresh, resetting its attempt counter), multiplying real provider
        // calls and backoff sleeps. So an empty response reaching this point
        // is treated as terminal and never fabricated into assistant content.
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
            let proposed = calls.clone();
            match run_tool_steps(state, tools, events, calls, history, cancel)? {
                ToolBatchOutcome::Stopped(stop) => Ok(StepDecision::Stop(stop)),
                ToolBatchOutcome::Completed(results) => Ok(StepDecision::Continue(
                    ToolStepExchange::new(proposed, results),
                )),
            }
        }
    }
}

/// Outcome of one dispatched tool batch: either every accepted call produced
/// a result, or the turn stopped.
// Private and returned once per tool batch, never stored: the size
// difference (a `TurnResult` against a `Vec`) costs nothing worth a box.
#[allow(clippy::large_enum_variant)]
enum ToolBatchOutcome {
    Completed(Vec<ToolStepResult>),
    Stopped(TurnResult),
}

/// Why a proposed call was refused before dispatch, applied after the calls
/// accepted ahead of it have run (per-call budget/loop semantics are kept).
struct StepRefusal {
    /// The refused call, when its `tool.requested`/`tool.failed` events must
    /// still be emitted (validation refusals).
    call: Option<ProposedToolCall>,
    /// The turn stop the refusal produces.
    action: RefusalAction,
}

enum RefusalAction {
    Interrupt,
    Fail(TurnStopReason),
}

fn run_tool_steps<T, E>(
    state: &mut LoopState,
    tools: &mut T,
    events: &mut E,
    calls: Vec<ProposedToolCall>,
    history: &[ToolStepExchange],
    cancel: &CancellationToken,
) -> Result<ToolBatchOutcome, TurnError>
where
    T: ToolDriver,
    E: TurnEventSink,
{
    // Phase 1: per-call gates and validation, in proposal order. The first
    // refusal is recorded; calls accepted ahead of it still dispatch.
    let mut prepared: Vec<(ProposedToolCall, ValidatedToolCall)> = Vec::new();
    let mut refusal: Option<StepRefusal> = None;
    for call in calls {
        if cancel.is_cancelled() {
            refusal = Some(StepRefusal {
                call: None,
                action: RefusalAction::Interrupt,
            });
            break;
        }
        // The budget gate counts calls already accepted in this batch, so a
        // proposal beyond the remaining budget is refused before executing.
        if tool_budget_exhausted(state.usage.tool_calls, prepared.len(), state.budget) {
            refusal = Some(StepRefusal {
                call: None,
                action: RefusalAction::Fail(TurnStopReason::BudgetExhausted),
            });
            break;
        }
        state.loop_detector.observe(call.tool(), call.arguments());
        if state.loop_detector.is_looping() {
            refusal = Some(StepRefusal {
                call: None,
                action: RefusalAction::Fail(TurnStopReason::RepeatedToolCall),
            });
            break;
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
            let interrupting = matches!(err, TurnError::Cancelled);
            if !interrupting {
                state.unhandled_tool_failure = true;
            }
            refusal = Some(if interrupting {
                StepRefusal {
                    call: Some(call),
                    action: RefusalAction::Interrupt,
                }
            } else {
                StepRefusal {
                    call: Some(call),
                    action: RefusalAction::Fail(TurnStopReason::ToolFailed),
                }
            });
            break;
        }

        let validated = match tools.validate(&call, cancel) {
            Ok(validated) => validated,
            Err(ToolStepError::Cancelled) => {
                refusal = Some(StepRefusal {
                    call: Some(call),
                    action: RefusalAction::Interrupt,
                });
                break;
            }
            Err(err @ (ToolStepError::Invalid | ToolStepError::Failed)) => {
                state.unhandled_tool_failure = true;
                state.failure_detail =
                    Some(TurnFailureDetail::new(call.tool.clone(), err.as_str()));
                refusal = Some(StepRefusal {
                    call: Some(call),
                    action: RefusalAction::Fail(TurnStopReason::ToolFailed),
                });
                break;
            }
        };

        if validated.call_id != call.call_id || validated.tool != call.tool {
            state.unhandled_tool_failure = true;
            state.failure_detail = Some(TurnFailureDetail::new(
                call.tool.clone(),
                "tool call identity changed during validation",
            ));
            refusal = Some(StepRefusal {
                call: Some(call),
                action: RefusalAction::Fail(TurnStopReason::ToolFailed),
            });
            break;
        }
        prepared.push((call, validated));
    }

    // Phase 2 + 3: dispatch the accepted calls, then apply the refusal (if
    // any) so an accepted prefix still runs, as the per-call loop did.
    let mut results = Vec::new();
    if !prepared.is_empty() {
        match dispatch_prepared(state, tools, events, prepared, history, cancel)? {
            ToolBatchOutcome::Stopped(stop) => return Ok(ToolBatchOutcome::Stopped(stop)),
            ToolBatchOutcome::Completed(completed) => results = completed,
        }
    }
    if let Some(refusal) = refusal {
        if let Some(call) = &refusal.call {
            emit_tool_failed(state, events, call)?;
        }
        let stop = match refusal.action {
            RefusalAction::Interrupt => interrupt(state, events)?,
            RefusalAction::Fail(reason) => fail(state, events, reason)?,
        };
        return Ok(ToolBatchOutcome::Stopped(stop));
    }
    if state.unhandled_tool_failure {
        return Ok(ToolBatchOutcome::Stopped(fail(
            state,
            events,
            TurnStopReason::ToolFailed,
        )?));
    }
    Ok(ToolBatchOutcome::Completed(results))
}

/// Phase 2: mark the batch started and dispatch it to the driver at once
/// (independent calls may run concurrently inside the driver). Phase 3
/// accounts per-call outcomes in proposal order. `history` — every completed
/// exchange before this batch — is recorded into a [`TurnSuspension`] when the
/// batch suspends the turn on an approval or a clarification, so the host can
/// resume the exact turn later.
fn dispatch_prepared<T, E>(
    state: &mut LoopState,
    tools: &mut T,
    events: &mut E,
    prepared: Vec<(ProposedToolCall, ValidatedToolCall)>,
    history: &[ToolStepExchange],
    cancel: &CancellationToken,
) -> Result<ToolBatchOutcome, TurnError>
where
    T: ToolDriver,
    E: TurnEventSink,
{
    for (call, _) in &prepared {
        emit(
            events,
            TurnEvent::ToolStarted {
                turn_id: state.turn_id,
                call_id: call.call_id.clone(),
                tool: call.tool.clone(),
            },
        )?;
    }
    if let Some(stop) = stop_if_cancelled(state, events, cancel)? {
        return Ok(ToolBatchOutcome::Stopped(stop));
    }
    let validated: Vec<ValidatedToolCall> = prepared
        .iter()
        .map(|(_, validated)| validated.clone())
        .collect();
    let outcomes = tools.execute_batch(&validated, cancel);

    // `execute_batch` already ran every call concurrently and joined all of
    // them before returning `outcomes` — every outcome here corresponds to a
    // real, already-committed side effect. So this pass must record ALL of
    // them (push to `results`, emit their event, account usage) before
    // deciding whether to stop; returning early on the first bad outcome
    // would silently discard the already-executed results of every call
    // after it. The stop reason reported is still the FIRST bad outcome
    // encountered, by original index, matching prior behavior.
    let mut results = Vec::new();
    // One entry per proposal, in proposal order: the recorded `results` skip
    // step-level errors, so the suspension builder keeps its own full-length
    // copy (errors included) to synthesize placeholders from.
    let mut batch: Vec<Result<ToolStepResult, ToolStepError>> = Vec::new();
    let mut stop_outcome: Option<ToolBatchOutcome> = None;
    for ((call, _), outcome) in prepared.iter().zip(outcomes) {
        batch.push(outcome.clone());
        let result = match outcome {
            Ok(result) => result,
            Err(ToolStepError::Cancelled) => {
                emit_tool_failed(state, events, call)?;
                if stop_outcome.is_none() {
                    stop_outcome = Some(ToolBatchOutcome::Stopped(interrupt(state, events)?));
                }
                continue;
            }
            Err(err @ (ToolStepError::Invalid | ToolStepError::Failed)) => {
                emit_tool_failed(state, events, call)?;
                state.unhandled_tool_failure = true;
                state.failure_detail =
                    Some(TurnFailureDetail::new(call.tool.clone(), err.as_str()));
                if stop_outcome.is_none() {
                    stop_outcome = Some(ToolBatchOutcome::Stopped(fail(
                        state,
                        events,
                        TurnStopReason::ToolFailed,
                    )?));
                }
                continue;
            }
        };

        state.usage.tool_calls = state.usage.tool_calls.saturating_add(1);
        emit_tool_result(state, events, call, &result)?;
        results.push(result.clone());

        if matches!(result, ToolStepResult::ApprovalRequired { .. }) {
            if stop_outcome.is_none() {
                // Record the suspension BEFORE failing: the stop must carry
                // every already-committed result of this batch (the pending
                // call's placeholder included) so a resume never repeats a
                // committed side effect nor re-runs the pending call blindly.
                if state.suspension.is_none() {
                    state.suspension = Some(TurnSuspension::new(
                        TurnStopReason::ApprovalRequired,
                        call.call_id.clone(),
                        history,
                        &prepared_calls(&prepared),
                        &batch,
                    ));
                }
                stop_outcome = Some(ToolBatchOutcome::Stopped(fail(
                    state,
                    events,
                    TurnStopReason::ApprovalRequired,
                )?));
            }
        } else if let ToolStepResult::ContextRequired { question, .. } = &result {
            // Unconditional, the same way ApprovalRequired is: the model
            // asked a real question and there was no one to answer it, so
            // the loop must not continue on its own judgment — that is
            // exactly the silent-guessing behavior this variant exists to
            // replace. `state.failure_detail` carries the question itself
            // (not `unhandled_tool_failure`/`ToolFailed` bookkeeping — this
            // is not a malfunction).
            state.failure_detail = Some(TurnFailureDetail::new(call.tool.clone(), question));
            if stop_outcome.is_none() {
                // Same suspension contract as the approval arm: the question
                // is pending human input, and the answer arrives on a resume
                // of this exact exchange.
                if state.suspension.is_none() {
                    state.suspension = Some(TurnSuspension::new(
                        TurnStopReason::ContextRequired,
                        call.call_id.clone(),
                        history,
                        &prepared_calls(&prepared),
                        &batch,
                    ));
                }
                stop_outcome = Some(ToolBatchOutcome::Stopped(fail(
                    state,
                    events,
                    TurnStopReason::ContextRequired,
                )?));
            }
        } else if result.is_unhandled_failure() {
            state.unhandled_tool_failure = true;
            state.failure_detail = Some(TurnFailureDetail::new(
                call.tool.clone(),
                &unhandled_failure_text(&result),
            ));
            if stop_outcome.is_none() {
                stop_outcome = Some(ToolBatchOutcome::Stopped(fail(
                    state,
                    events,
                    TurnStopReason::ToolFailed,
                )?));
            }
        }
    }
    if let Some(stop_outcome) = stop_outcome {
        return Ok(stop_outcome);
    }
    Ok(ToolBatchOutcome::Completed(results))
}

/// The batch's proposed calls, in proposal order — the calls side of the
/// suspension's final recorded exchange.
fn prepared_calls(prepared: &[(ProposedToolCall, ValidatedToolCall)]) -> Vec<ProposedToolCall> {
    prepared.iter().map(|(call, _)| call.clone()).collect()
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
        ToolStepResult::Denied { detail, .. } => TurnEvent::ToolDenied {
            turn_id: state.turn_id,
            call_id: call.call_id.clone(),
            tool: call.tool.clone(),
            reason: detail.clone(),
        },
        ToolStepResult::ApprovalRequired { .. } => TurnEvent::ToolApprovalRequired {
            turn_id: state.turn_id,
            call_id: call.call_id.clone(),
            tool: call.tool.clone(),
        },
        ToolStepResult::ContextRequired { .. } => TurnEvent::ToolContextRequired {
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

/// Operator-facing one-liner for an unhandled per-call tool failure: the
/// driver's own detail when present, else a stable fallback.
fn unhandled_failure_text(result: &ToolStepResult) -> String {
    match result {
        ToolStepResult::Failed {
            detail: Some(detail),
            ..
        } => detail.clone(),
        _ => "tool reported an unhandled failure".to_owned(),
    }
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
        failure_detail: None,
        suspension: None,
    })
}

fn fail<E: TurnEventSink>(
    state: &LoopState,
    events: &mut E,
    reason: TurnStopReason,
) -> Result<TurnResult, TurnError> {
    // A ToolFailed stop carries the most recent failing tool so the CLI can
    // name it; a ContextRequired stop carries the model's own question the
    // same way (`dispatch_prepared` sets `state.failure_detail` for both
    // before calling this); every other stop has no tool detail by
    // definition.
    let detail = if matches!(
        reason,
        TurnStopReason::ToolFailed | TurnStopReason::ContextRequired
    ) {
        state.failure_detail.clone()
    } else {
        None
    };
    fail_with_cause(state, events, reason, None, detail)
}

/// Like [`fail`], but records the provider-classified cause that produced the
/// stop so the executor and CLI can name it. Only provider-caused model stops
/// carry a cause; budget/tool/empty-response stops do not.
fn fail_with_cause<E: TurnEventSink>(
    state: &LoopState,
    events: &mut E,
    reason: TurnStopReason,
    cause: Option<FailureCause>,
    detail: Option<TurnFailureDetail>,
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
        failure_detail: detail,
        suspension: state.suspension.clone(),
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
        failure_detail: None,
        suspension: None,
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

/// One `model.continued` record per request the step made, when it
/// continued an answer (none otherwise); their tokens' sum.
fn emit_continuations<M: ModelDriver, E: TurnEventSink>(
    model: &mut M,
    state: &LoopState,
    events: &mut E,
    request_id: &str,
) -> Result<u64, TurnError> {
    let mut spent: u64 = 0;
    for (index, request_tokens) in model.take_continuations().into_iter().enumerate() {
        spent = spent.saturating_add(request_tokens);
        emit(
            events,
            TurnEvent::ModelContinued {
                turn_id: state.turn_id,
                continuation_of: request_id.to_owned(),
                index: u32::try_from(index).unwrap_or(u32::MAX),
                tokens: request_tokens,
            },
        )?;
    }
    Ok(spent)
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

fn tool_budget_exhausted(used: u32, pending_in_batch: usize, budget: TurnBudget) -> bool {
    match budget.max_tool_calls {
        Some(limit) => (used as usize).saturating_add(pending_in_batch) >= limit as usize,
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

    #[test]
    fn tool_failure_stop_names_the_failing_tool_and_error() {
        // The proposed call's execution fails at the driver: the turn stops
        // ToolFailed and the result carries the failing tool's name plus the
        // underlying error text, so a CLI can diagnose without replaying.
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("c1", "repo.search")], 1),
            terminal("unreached", 1),
        ]);
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
        assert!(result.failure_cause().is_none());
        let detail = result.failure_detail().expect("tool detail");
        assert_eq!(detail.tool(), "repo.search");
        assert_eq!(detail.error(), "tool step failed");
    }

    #[test]
    fn unhandled_tool_result_detail_surfaces_in_failure_detail() {
        // An unhandled Failed result (driver-produced detail): the detail text
        // rides the stop instead of collapsing into the bare reason.
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("c1", "workspace.write")], 1),
            terminal("unreached", 1),
        ]);
        let mut tools = ScriptedTools::new(vec![Ok(ToolStepResult::Failed {
            call_id: "c1".to_owned(),
            handled: false,
            detail: Some("write refused: outside the workspace root".to_owned()),
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
        let detail = result.failure_detail().expect("tool detail");
        assert_eq!(detail.tool(), "workspace.write");
        assert_eq!(detail.error(), "write refused: outside the workspace root");
    }

    #[test]
    fn a_denial_carries_its_reason_onto_the_event_not_just_into_the_tool_result() {
        // `TurnEvent::ToolDenied` carried only `turn_id`/`call_id`/`tool`,
        // so a refusal's reason reached the *model* as the tool result and
        // nothing else. Every frontend therefore showed "this tool was
        // denied" with no cause and no remedy — which matters most in the
        // permission mode the product ships in by default, where the reason
        // is the only thing naming the way forward.
        const REASON: &str = "workspace.write denied: requires approval; pre-approve it with `rapid permissions allow <tool>`";
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("c1", "workspace.write")], 1),
            terminal("done", 1),
        ]);
        let mut tools = ScriptedTools::new(vec![Ok(ToolStepResult::Denied {
            call_id: "c1".to_owned(),
            detail: Some(REASON.to_owned()),
        })]);
        let mut events = Vec::new();
        run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");

        let reason = events
            .iter()
            .find_map(|event| match event {
                TurnEvent::ToolDenied { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .expect("a denial emits its event");
        assert_eq!(
            reason.as_deref(),
            Some(REASON),
            "the denial reason must ride the event, not only the tool result"
        );
    }

    #[test]
    fn a_denial_with_no_reason_emits_none_rather_than_an_empty_string() {
        // A `ToolDriver` is not obliged to explain itself; "no reason" must
        // stay distinguishable from "an empty reason" so a frontend renders
        // the bare form rather than a stray separator.
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("c1", "workspace.write")], 1),
            terminal("done", 1),
        ]);
        let mut tools = ScriptedTools::new(vec![Ok(ToolStepResult::Denied {
            call_id: "c1".to_owned(),
            detail: None,
        })]);
        let mut events = Vec::new();
        run(
            TurnBudget::unlimited_steps(),
            &mut model,
            &mut tools,
            &mut events,
            &live(),
        )
        .expect("run");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TurnEvent::ToolDenied { reason: None, .. }))
        );
    }

    #[test]
    fn turn_failure_detail_error_text_is_bounded() {
        let long = "x".repeat(10_000);
        let detail = TurnFailureDetail::new("shell_exec", &long);
        assert_eq!(detail.tool(), "shell_exec");
        assert_eq!(detail.error().len(), FAILURE_DETAIL_ERROR_BYTES);
        // Bounds are char-safe: a multibyte tail never splits a code point.
        let multibyte = "héllo ".repeat(2_000);
        let detail = TurnFailureDetail::new("t", &multibyte);
        assert!(detail.error().is_char_boundary(detail.error().len()));
    }

    fn kinds(events: &[TurnEvent]) -> Vec<&'static str> {
        events.iter().map(|event| event.kind().as_str()).collect()
    }

    fn run<M: ModelDriver, T: ToolDriver>(
        budget: TurnBudget,
        model: &mut M,
        tools: &mut T,
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

    /// The driver only implements `execute_batch`; the sequential `execute`
    /// path signals failure, so a completed turn proves the loop used the
    /// batch seam and kept per-call ids and outcomes.
    struct BatchOnlyTools;
    impl ToolDriver for BatchOnlyTools {
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
        ) -> Result<ToolStepResult, ToolStepError> {
            let _ = call;
            Err(ToolStepError::Failed)
        }
        fn execute_batch(
            &mut self,
            calls: &[ValidatedToolCall],
            _cancel: &CancellationToken,
        ) -> Vec<Result<ToolStepResult, ToolStepError>> {
            calls
                .iter()
                .map(|call| {
                    Ok(ToolStepResult::Succeeded {
                        call_id: call.call_id().to_owned(),
                        summary: format!("batch:{}", call.call_id()),
                    })
                })
                .collect()
        }
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
                    cost_usd_micros: None,
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
    fn history_accumulates_across_steps_so_early_results_stay_visible() {
        // Step 1 reads a.txt; step 2 reads b.txt; step 3 must still see BOTH
        // exchanges — the model's request at step 3 carries the full turn
        // history, not just the last exchange.
        struct HistoryModel {
            seen_histories: Vec<Vec<Vec<String>>>,
        }
        impl ModelDriver for HistoryModel {
            fn step(
                &mut self,
                input: &ModelStepInput<'_>,
                _cancel: &CancellationToken,
            ) -> Result<ModelStepOutput, ModelStepError> {
                self.seen_histories.push(
                    input
                        .history()
                        .iter()
                        .map(|exchange| {
                            exchange
                                .results()
                                .iter()
                                .map(|result| result.call_id().to_owned())
                                .collect()
                        })
                        .collect(),
                );
                match self.seen_histories.len() {
                    1 => tools_out(vec![call("ra", "repo.read")], 1),
                    2 => tools_out(vec![call("rb", "repo.read")], 1),
                    _ => terminal("have both", 1),
                }
            }
        }
        let mut model = HistoryModel {
            seen_histories: Vec::new(),
        };
        let mut tools = ScriptedTools::new(vec![
            Ok(ToolStepResult::Succeeded {
                call_id: "ra".to_owned(),
                summary: "17".to_owned(),
            }),
            Ok(ToolStepResult::Succeeded {
                call_id: "rb".to_owned(),
                summary: "25".to_owned(),
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
        assert_eq!(result.status(), TurnStatus::Completed);
        assert_eq!(model.seen_histories.len(), 3);
        assert_eq!(model.seen_histories[0].len(), 0, "first step: no history");
        assert_eq!(model.seen_histories[1], vec![vec!["ra".to_owned()]]);
        assert_eq!(
            model.seen_histories[2],
            vec![vec!["ra".to_owned()], vec!["rb".to_owned()]],
            "step 3 must still see step 1's exchange"
        );
    }

    #[test]
    fn one_step_calls_dispatch_as_a_batch_with_per_call_results() {
        let mut model = ScriptedModel::new(vec![
            tools_out(
                vec![
                    call("c1", "repo.read"),
                    call("c2", "repo.search"),
                    call("c3", "repo.read"),
                ],
                2,
            ),
            terminal("done", 1),
        ]);
        let mut tools = BatchOnlyTools;
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
        assert_eq!(result.usage().tool_calls(), 3);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind() == TurnEventKind::ToolCompleted)
                .count(),
            3
        );
    }

    /// `execute_batch` runs every call in the batch before returning (real
    /// side effects already happened for all of them), but the middle call
    /// is scripted to come back as an unhandled failure.
    struct MixedOutcomeBatchTools;
    impl ToolDriver for MixedOutcomeBatchTools {
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
        ) -> Result<ToolStepResult, ToolStepError> {
            let _ = call;
            Err(ToolStepError::Failed)
        }
        fn execute_batch(
            &mut self,
            calls: &[ValidatedToolCall],
            _cancel: &CancellationToken,
        ) -> Vec<Result<ToolStepResult, ToolStepError>> {
            calls
                .iter()
                .map(|call| {
                    if call.call_id() == "c2" {
                        Ok(ToolStepResult::Failed {
                            call_id: call.call_id().to_owned(),
                            detail: Some("boom".to_owned()),
                            handled: false,
                        })
                    } else {
                        Ok(ToolStepResult::Succeeded {
                            call_id: call.call_id().to_owned(),
                            summary: format!("batch:{}", call.call_id()),
                        })
                    }
                })
                .collect()
        }
    }

    #[test]
    fn a_batch_failure_does_not_drop_the_already_executed_calls_after_it() {
        // `execute_batch` already ran c1, c2, and c3 concurrently before
        // returning: all three have real, committed side effects. c2 comes
        // back as an unhandled failure, which must stop the turn — but c3's
        // outcome (which ran right alongside c2) must still be recorded:
        // pushed to results, its completion event emitted, and usage
        // incremented. Silently dropping it would hide an already-executed
        // effect from the turn's history and usage accounting.
        let mut model = ScriptedModel::new(vec![tools_out(
            vec![
                call("c1", "repo.read"),
                call("c2", "repo.search"),
                call("c3", "repo.read"),
            ],
            2,
        )]);
        let mut tools = MixedOutcomeBatchTools;
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
        // c1 and c3 both succeeded and must both be accounted, even though
        // c2 (between them) is the one that stopped the turn.
        assert_eq!(
            result.usage().tool_calls(),
            2,
            "both successful calls must be counted, not just the one before the failure"
        );
        let completed = events
            .iter()
            .filter_map(|event| match event {
                TurnEvent::ToolCompleted { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            completed,
            vec!["c1", "c3"],
            "c3's completion, after the failing c2, must still be emitted"
        );
        assert!(
            events.iter().any(
                |event| matches!(event, TurnEvent::ToolFailed { call_id, .. } if call_id == "c2")
            ),
            "c2's failure must still be reported"
        );
    }

    #[test]
    fn continuation_step_sees_the_pending_calls_next_to_their_results() {
        struct PairRecordingModel {
            seen: Vec<(Vec<String>, Vec<String>)>,
        }
        impl ModelDriver for PairRecordingModel {
            fn step(
                &mut self,
                input: &ModelStepInput<'_>,
                _cancel: &CancellationToken,
            ) -> Result<ModelStepOutput, ModelStepError> {
                self.seen.push((
                    input
                        .pending_calls()
                        .iter()
                        .map(|call| call.call_id().to_owned())
                        .collect(),
                    input
                        .prior_tools()
                        .iter()
                        .map(|result| result.call_id().to_owned())
                        .collect(),
                ));
                if input.prior_tools().is_empty() {
                    Ok(
                        tools_out(vec![call("c1", "repo.read"), call("c2", "repo.read")], 1)
                            .expect("calls"),
                    )
                } else {
                    terminal("paired", 1)
                }
            }
        }
        let mut model = PairRecordingModel { seen: Vec::new() };
        let mut tools = ScriptedTools::new(vec![
            Ok(ToolStepResult::Succeeded {
                call_id: "c1".to_owned(),
                summary: "one".to_owned(),
            }),
            Ok(ToolStepResult::Succeeded {
                call_id: "c2".to_owned(),
                summary: "two".to_owned(),
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
        assert_eq!(result.status(), TurnStatus::Completed);
        assert_eq!(model.seen.len(), 2);
        assert_eq!(model.seen[0], (vec![], vec![]));
        assert_eq!(
            model.seen[1],
            (
                vec!["c1".to_owned(), "c2".to_owned()],
                vec!["c1".to_owned(), "c2".to_owned()]
            )
        );
    }

    #[test]
    fn background_notifications_land_in_history_before_the_next_step() {
        // The driver reports a completed background job on drain; the next
        // model step must see it as a synthetic exchange in the history
        // without any polling tool call from the model.
        struct NotifyDriver {
            drained: Vec<ToolStepExchange>,
        }
        impl ToolDriver for NotifyDriver {
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
            ) -> Result<ToolStepResult, ToolStepError> {
                Ok(ToolStepResult::Succeeded {
                    call_id: call.call_id().to_owned(),
                    summary: "started".to_owned(),
                })
            }
            fn drain_notifications(&mut self) -> Vec<ToolStepExchange> {
                std::mem::take(&mut self.drained)
            }
        }
        let notification = ToolStepExchange::new(
            vec![ProposedToolCall::new("notify-job-1", "background_jobs", "{}").expect("call")],
            vec![ToolStepResult::Succeeded {
                call_id: "notify-job-1".to_owned(),
                summary: "job-1: completed exit 0 — output: done".to_owned(),
            }],
        );
        struct NotifyModel {
            notification_seen: bool,
            steps: u32,
        }
        impl ModelDriver for NotifyModel {
            fn step(
                &mut self,
                input: &ModelStepInput<'_>,
                _cancel: &CancellationToken,
            ) -> Result<ModelStepOutput, ModelStepError> {
                self.steps += 1;
                let seen_notification = input.history().iter().any(|exchange| {
                    exchange.results().iter().any(|result| match result {
                        ToolStepResult::Succeeded { summary, .. } => {
                            summary.contains("completed exit 0")
                        }
                        _ => false,
                    })
                });
                if seen_notification {
                    self.notification_seen = true;
                    return Ok(ModelStepOutput::Terminal {
                        text: "noticed the job".to_owned(),
                        tokens: 1,
                        cost_usd_micros: None,
                    });
                }
                Ok(ModelStepOutput::ToolCalls {
                    calls: vec![
                        ProposedToolCall::new("bg", "background_jobs", "{}").expect("call"),
                    ],
                    tokens: 1,
                    cost_usd_micros: None,
                })
            }
        }
        let mut model = NotifyModel {
            notification_seen: false,
            steps: 0,
        };
        let mut tools = NotifyDriver {
            drained: vec![notification],
        };
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
        assert!(
            model.notification_seen,
            "step 2 must see the drained notification in history"
        );
    }

    #[test]
    fn proposal_above_the_per_step_cap_is_refused_before_any_execution() {
        // 17 proposed calls in one step exceed MAX_TOOL_CALLS_PER_STEP: the
        // step fails typed and none of the calls reach the driver.
        let calls: Vec<ProposedToolCall> = (0..=MAX_TOOL_CALLS_PER_STEP)
            .map(|index| call(&format!("c{index}"), "repo.read"))
            .collect();
        let mut model = ScriptedModel::new(vec![tools_out(calls, 1)]);
        let mut tools = BatchOnlyTools;
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
        assert_eq!(result.usage().tool_calls(), 0, "no call may execute");
        assert!(
            !events
                .iter()
                .any(|event| event.kind() == TurnEventKind::ToolStarted),
            "refused calls never reach dispatch"
        );
    }

    #[test]
    fn a_step_proposing_two_calls_sharing_one_call_id_is_refused_before_any_execution() {
        // Two structurally valid calls that happen to share a `call_id` (the
        // shape a provider stream reusing an id across two `ToolCallStart`
        // events would produce, per `apps/rapid/src/model.rs::fold_stream`)
        // must never reach the driver — neither can be told apart from the
        // other by id alone downstream.
        let calls = vec![
            ProposedToolCall::new("dup", "repo.read", "{}").expect("call"),
            ProposedToolCall::new("dup", "workspace.write", "{}").expect("call"),
        ];
        let mut model = ScriptedModel::new(vec![tools_out(calls, 1)]);
        let mut tools = BatchOnlyTools;
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
        assert_eq!(result.usage().tool_calls(), 0, "no call may execute");
        assert!(
            !events
                .iter()
                .any(|event| event.kind() == TurnEventKind::ToolStarted),
            "refused calls never reach dispatch"
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
            detail: None,
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
    fn empty_response_is_terminal_and_never_completes() {
        // Retrying an empty model response is now solely `SupervisedModel`'s
        // job (apps/rapid/src/host.rs), with its own bounded backoff. A
        // second retry loop at this layer would compound with that one
        // (each retry here re-enters `SupervisedModel` fresh), so an empty
        // response reaching `run_model_step` must fail the turn immediately
        // rather than re-invoking the model.
        let mut model = ScriptedModel::new(vec![terminal("", 1), terminal("unreached", 1)]);
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
        assert_eq!(model.seen, 1, "an empty response is never retried here");
        assert!(!kinds(&events).contains(&"turn.completed"));
        assert!(kinds(&events).contains(&"turn.failed"));
    }

    #[test]
    fn empty_response_after_a_tool_step_does_not_replay_its_effects() {
        // step1 executes callA (one effect). step2 is an empty response,
        // which now fails the turn immediately (no retry layer here). The
        // already-executed tool effect from step1 must not be replayed.
        let mut model = ScriptedModel::new(vec![
            tools_out(vec![call("c1", "repo.search")], 1),
            terminal("", 1),
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
        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(result.reason(), Some(TurnStopReason::EmptyResponse));
        assert_eq!(
            tools.executed, 1,
            "the executed tool call is never replayed"
        );
        assert_eq!(model.seen, 2);
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
            detail: None,
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
    fn a_continued_answer_is_one_record_per_request_before_its_completion() {
        // SEAM-02 AC-06 in the loop: three requests (the first and two
        // continuations) — three `model.continued` records of the step's
        // request, then its completion; the budget counts all three.
        struct Continued {
            taken: bool,
            allowance: Option<Option<u64>>,
        }
        impl ModelDriver for Continued {
            fn step(
                &mut self,
                input: &ModelStepInput<'_>,
                _cancel: &CancellationToken,
            ) -> Result<ModelStepOutput, ModelStepError> {
                self.allowance = Some(input.token_allowance());
                Ok(ModelStepOutput::Terminal {
                    text: "one stitched message".to_owned(),
                    tokens: 18,
                    cost_usd_micros: None,
                })
            }

            fn take_continuations(&mut self) -> Vec<u64> {
                if std::mem::replace(&mut self.taken, true) {
                    Vec::new()
                } else {
                    vec![5, 6, 7]
                }
            }
        }
        let mut model = Continued {
            taken: false,
            allowance: None,
        };
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let budget = TurnBudget::new(8, None, Some(100)).expect("budget");
        let result = run(budget, &mut model, &mut tools, &mut events, &live()).expect("run");
        assert_eq!(result.status(), TurnStatus::Completed);
        assert_eq!(
            model.allowance,
            Some(Some(100)),
            "the budget's allowance reached the driver"
        );
        assert_eq!(result.usage().tokens, 18, "5 + 6 + 7");
        let continued: Vec<(u32, u64, String)> = events
            .iter()
            .filter_map(|event| match event {
                TurnEvent::ModelContinued {
                    continuation_of,
                    index,
                    tokens,
                    ..
                } => Some((*index, *tokens, continuation_of.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(continued.len(), 3);
        assert_eq!(
            continued
                .iter()
                .map(|(index, tokens, _)| (*index, *tokens))
                .collect::<Vec<_>>(),
            vec![(0, 5), (1, 6), (2, 7)]
        );
        let requested = events.iter().find_map(|event| match event {
            TurnEvent::ModelRequested { request_id, .. } => Some(request_id.clone()),
            _ => None,
        });
        assert!(
            continued
                .iter()
                .all(|(_, _, of)| Some(of) == requested.as_ref())
        );
        let order = kinds(&events);
        let last_continued = order.iter().rposition(|kind| *kind == "model.continued");
        let completed = order.iter().position(|kind| *kind == "model.completed");
        assert!(last_continued < completed, "{order:?}");
        // A step that made one request: no record (today's events exactly).
        let mut plain = ScriptedModel::new(vec![terminal("done", 3)]);
        let mut events = Vec::new();
        run(
            TurnBudget::unlimited_steps(),
            &mut plain,
            &mut ScriptedTools::new(Vec::new()),
            &mut events,
            &live(),
        )
        .expect("run");
        assert!(!kinds(&events).contains(&"model.continued"));
    }

    #[test]
    fn a_step_that_fails_after_continuing_leaves_its_requests_on_record() {
        struct FailsLate;
        impl ModelDriver for FailsLate {
            fn step(
                &mut self,
                _input: &ModelStepInput<'_>,
                _cancel: &CancellationToken,
            ) -> Result<ModelStepOutput, ModelStepError> {
                Err(ModelStepError::Failed)
            }

            fn take_continuations(&mut self) -> Vec<u64> {
                vec![9]
            }

            fn take_uncounted_tokens(&mut self) -> u64 {
                4
            }
        }
        let mut events = Vec::new();
        let _ = run(
            TurnBudget::unlimited_steps(),
            &mut FailsLate,
            &mut ScriptedTools::new(Vec::new()),
            &mut events,
            &live(),
        );
        let order = kinds(&events);
        let continued = order.iter().position(|kind| *kind == "model.continued");
        let failed = order.iter().position(|kind| *kind == "model.failed");
        assert!(continued.is_some() && continued < failed, "{order:?}");
        // Billed: the records' tokens count although the step failed.
        let result = run(
            TurnBudget::unlimited_steps(),
            &mut FailsLate,
            &mut ScriptedTools::new(Vec::new()),
            &mut Vec::new(),
            &live(),
        )
        .expect("run");
        assert_eq!(result.usage().tokens, 9 + 4, "records and uncounted work");
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
    fn context_required_stops_without_complete_and_carries_the_question() {
        let mut model = ScriptedModel::new(vec![tools_out(vec![call("c1", "ask_user")], 1)]);
        let mut tools = ScriptedTools::new(vec![Ok(ToolStepResult::ContextRequired {
            call_id: "c1".to_owned(),
            question: "Which environment should I deploy to?".to_owned(),
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
        assert_eq!(result.reason(), Some(TurnStopReason::ContextRequired));
        let detail = result.failure_detail().expect("failure detail");
        assert_eq!(detail.tool(), "ask_user");
        assert_eq!(detail.error(), "Which environment should I deploy to?");
        // Never reuses `tool.failed`'s event shape: a consumer that treats
        // any `tool.failed` as an operator-visible failure marker must not
        // see one for a call that didn't actually fail.
        assert!(kinds(&events).contains(&"tool.context_required"));
        assert!(!kinds(&events).contains(&"tool.failed"));
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
        assert_eq!(
            TurnEventKind::ToolContextRequired.as_str(),
            "tool.context_required"
        );
        assert!(TurnEventKind::TurnCompleted.is_terminal());
        assert!(!TurnEventKind::ModelCompleted.is_terminal());
        assert_eq!(model_request_id(1), "m1");
        assert_eq!(model_request_id(12), "m12");
    }

    /// A turn legal under `MAX_MODEL_STEPS`/`MAX_TOOL_CALLS_PER_STEP` alone
    /// (no `max_tool_calls` cap) can drive every one of the 32 steps to its
    /// full 16-call allowance — 512 tool calls total. That is well inside
    /// what those two constants advertise as fine, yet the event count it
    /// produces (`Started` + 2/step + 3/call + one terminal = 1602) used to
    /// exceed the old, independently-chosen `MAX_TURN_EVENTS = 512`. Once
    /// crossed mid-batch, `dispatch_prepared` unwound via `emit`'s `?` with
    /// already-executed results never recorded and no terminal event ever
    /// emitted — this test proves that no longer happens and pins the exact
    /// event count the fully-loaded envelope produces.
    #[test]
    fn a_fully_loaded_turn_within_the_advertised_envelope_never_overruns_the_event_cap() {
        let steps: Vec<Result<ModelStepOutput, ModelStepError>> = (0..MAX_MODEL_STEPS)
            .map(|step| {
                let calls = (0..MAX_TOOL_CALLS_PER_STEP)
                    .map(|i| {
                        ProposedToolCall::new(
                            format!("c{step}_{i}"),
                            "test.tool",
                            format!("{{\"step\":{step},\"i\":{i}}}"),
                        )
                        .expect("call")
                    })
                    .collect();
                tools_out(calls, 1)
            })
            .collect();
        let mut model = ScriptedModel::new(steps);
        let mut tools = ScriptedTools::new(Vec::new());
        let mut events = Vec::new();
        let budget = TurnBudget::new(MAX_MODEL_STEPS, None, None).expect("budget");
        let result = run(budget, &mut model, &mut tools, &mut events, &live())
            .expect("a fully-loaded but budget-legal turn must not overrun the event cap");

        assert_eq!(result.status(), TurnStatus::Failed);
        assert_eq!(result.reason(), Some(TurnStopReason::BudgetExhausted));
        assert_eq!(kinds(&events).last(), Some(&"turn.failed"));
        let expected_events = 1 // turn.started
            + MAX_MODEL_STEPS as usize * 2 // model.requested + model.completed
            + MAX_MODEL_STEPS as usize * MAX_TOOL_CALLS_PER_STEP * 3 // tool.requested + tool.started + tool.completed
            + 1; // the one terminal event
        assert_eq!(events.len(), expected_events);
        assert!(expected_events <= MAX_TURN_EVENTS);
    }
}
