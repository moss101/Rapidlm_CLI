//! Context-owning live-recovery host (P5-024 production wiring).
//!
//! This is the composition-root host that owns a reusable live model context and
//! runs the recovery-capable executor. It couples:
//!
//!   - a live [`ContextPacket`] (the single Context-Fabric authority), rebuilt
//!     via `context_engine::compile` + policy-verified `compact_with_policy` on
//!     overflow (fail-closed: a compaction that doesn't shrink enough is a typed
//!     `StillOverHard`, never a silently still-oversized packet);
//!   - a [`LiveModelCall`] backing (the provider adapter) that reads that packet;
//!   - the existing [`ContextController`]/[`ContextRetryPolicy`] recovery seam.
//!
//! It does NOT become the Policy Engine, Capability Broker, Workspace Manager or
//! Model Provider; those remain separate authorities. It preserves the Goal +
//! completion criteria, AGENTS/rules, selected skills, workspace/evidence/artifact
//! refs and the output-token reserve across a rebuild, and never replays committed
//! tool effects (the executor's recovery loop fails closed when `tool_calls > 0`).

use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use agent_runtime::{
    AgentExecutionError, AgentExecutionRequest, AgentExecutor, AgentOutcome, AgentResult,
    CancellationToken, ContextController, ContextOverflow, ContextRecoveryDecision,
    ContextRetryPolicy, ContextRevision, FailureCause, ModelDriver, ModelStepError, ModelStepInput,
    ModelStepOutput, ProposedToolCall, ToolDriver, ToolStepError, ToolStepResult,
    TurnAgentExecutor, TurnEventSink, TurnFailureDetail, TurnStopReason, ValidatedToolCall,
};
use context_engine::CancellationToken as CeCancel;
use context_engine::compact_policy::{
    CompactPolicyError, CompactionPolicy, CompactionStrategy, compact_with_policy,
};
use context_engine::compile::{
    CompileContext, CompileError, CompileInput, ContextBlock, ContextPacket, compile,
};
use llm_router::fallback::{
    AttemptProgress, FallbackAction, FallbackController, FallbackPlan, FallbackTrigger,
};
use llm_router::provider::{
    CancellationToken as RouterCancellationToken, ModelRef, ProviderError,
};
use protocol::{ArtifactId, ArtifactRef, EvidenceId, WorkspaceViewId};

/// Hard byte cap for the goal statement.
pub const MAX_GOAL_BYTES: usize = 16 * 1024;
/// Maximum retained completion criteria.
pub const MAX_CRITERIA: usize = 64;
/// Byte cap for one criterion text.
pub const MAX_CRITERION_BYTES: usize = 4 * 1024;
/// Byte cap for composed AGENTS/rules text.
pub const MAX_RULES_BYTES: usize = 32 * 1024;
/// Byte cap for the rendered system-prompt block.
pub const MAX_SYSTEM_PROMPT_BLOCK_BYTES: usize = 32 * 1024;
/// Always-loaded memory index bounds (Claude MEMORY.md parity: 200 lines /
/// 25 KB).
pub const MAX_MEMORY_INDEX_LINES: usize = 200;
pub const MAX_MEMORY_INDEX_BYTES: usize = 25 * 1024;
/// Byte cap for composed selected-skills text.
pub const MAX_SKILLS_BYTES: usize = 16 * 1024;
/// Maximum retained evidence ids.
pub const MAX_EVIDENCE: usize = 64;
/// Maximum retained artifact refs.
pub const MAX_ARTIFACTS: usize = 64;
/// Byte cap for a compaction summary block.
pub const MAX_COMPACTION_SUMMARY: usize = 8 * 1024;
/// Byte cap for the rendered active-reminders block.
pub const MAX_REMINDERS_BLOCK_BYTES: usize = 8 * 1024;

/// Maximum retry attempts for a transient-class step failure; a turn makes at
/// most `MAX_TRANSIENT_RETRIES + 1` step invocations per model step. Five
/// bounded retries with the base below cover provider stream drops (b.ai
/// drops long sessions for seconds at a time).
pub const MAX_TRANSIENT_RETRIES: u32 = 5;
/// Maximum retry attempts for an empty terminal response specifically. Empty
/// responses are provider transients too, but each one is a full billed
/// request, so the ceiling stays tighter than [`MAX_TRANSIENT_RETRIES`].
pub const MAX_EMPTY_RESPONSE_RETRIES: u32 = 2;
/// Base backoff before the first retry; doubled per subsequent retry
/// (1s, 2s, 4s, 8s, 16s — bounded, cancellable, honoring retry-after).
/// `RAPIDLM_RETRY_BASE_MS` overrides the base for one run (clamped to
/// 0..=60000) so deployments and tests can shorten the waits.
pub const RETRY_BACKOFF_BASE_MS: u64 = 1000;
/// Backoff waits are polled in slices of this size so cancellation stays
/// responsive without busy-spinning.
const RETRY_SLEEP_SLICE_MS: u64 = 50;
/// Maximum retained diagnostics lines per run; further lines are dropped.
pub const MAX_DIAG_LINES: usize = 256;
/// Maximum bytes for the endpoint host label in diagnostics lines.
pub const MAX_DIAG_HOST_BYTES: usize = 128;

/// Typed host-construction failure. Display never echoes block text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostError {
    InvalidPreserved,
    Compile(CompileError),
}

/// State preserved across a live-context rebuild. Authority over the actual
/// context stays with the Context Fabric; this only carries what a rebuild must
/// not lose.
///
/// No `Eq`/`PartialEq`: `retrieved_context` holds `CompileInput`, which
/// doesn't implement them (nothing compared `PreservedLiveContext` with
/// `==` before this field existed either).
#[derive(Clone, Debug)]
pub struct PreservedLiveContext {
    goal_statement: String,
    completion_criteria: Vec<String>,
    agents_rules: String,
    skills: String,
    workspace_view: Option<WorkspaceViewId>,
    evidence: Vec<EvidenceId>,
    artifacts: Vec<ArtifactRef>,
    context_limit: u32,
    output_reserve: u32,
    reminders_block: Option<String>,
    system_prompt: Option<String>,
    memory_index: Option<String>,
    retrieved_context: Vec<CompileInput>,
}

impl PreservedLiveContext {
    pub fn new(
        goal_statement: impl Into<String>,
        completion_criteria: Vec<String>,
        agents_rules: impl Into<String>,
        skills: impl Into<String>,
        context_limit: u32,
        output_reserve: u32,
    ) -> Result<Self, HostError> {
        let goal_statement = goal_statement.into();
        let agents_rules = agents_rules.into();
        let skills = skills.into();
        if goal_statement.is_empty()
            || goal_statement.len() > MAX_GOAL_BYTES
            || completion_criteria.len() > MAX_CRITERIA
            || completion_criteria
                .iter()
                .any(|c| c.is_empty() || c.len() > MAX_CRITERION_BYTES)
            || agents_rules.len() > MAX_RULES_BYTES
            || skills.len() > MAX_SKILLS_BYTES
            || context_limit == 0
        {
            return Err(HostError::InvalidPreserved);
        }
        Ok(Self {
            goal_statement,
            completion_criteria,
            agents_rules,
            skills,
            workspace_view: None,
            evidence: Vec::new(),
            artifacts: Vec::new(),
            context_limit,
            output_reserve,
            reminders_block: None,
            system_prompt: None,
            memory_index: None,
            retrieved_context: Vec::new(),
        })
    }

    /// Attach proactively-retrieved repo content for this turn (Context
    /// Scout hits against the task prompt) — see `context_retrieval.rs`.
    /// Bounded by the retrieval pass itself (`MAX_REFERENCES`); no further
    /// limit here, `compile()`'s own retrieved-share budget does the rest.
    pub fn with_retrieved_context(mut self, blocks: Vec<CompileInput>) -> Self {
        self.retrieved_context = blocks;
        self
    }

    pub fn retrieved_context(&self) -> &[CompileInput] {
        &self.retrieved_context
    }

    /// Attach the always-loaded memory index (`.rapidlm/MEMORY.md`).
    pub fn with_memory_index(mut self, memory: Option<String>) -> Self {
        let within_bounds = memory.as_ref().is_none_or(|text| {
            !text.is_empty() && text.len() <= MAX_MEMORY_INDEX_BYTES
        });
        if within_bounds {
            self.memory_index = memory;
        }
        self
    }

    pub fn memory_index(&self) -> Option<&str> {
        self.memory_index.as_deref()
    }

    /// Attach the rendered dynamic system prompt for this turn. Empty or
    /// oversized blocks are refused at the caller; None adds no block.
    pub fn with_system_prompt(mut self, system_prompt: Option<String>) -> Self {
        let within_bounds = system_prompt.as_ref().is_none_or(|text| {
            !text.is_empty() && text.len() <= MAX_SYSTEM_PROMPT_BLOCK_BYTES
        });
        if within_bounds {
            self.system_prompt = system_prompt;
        }
        self
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn with_workspace_view(mut self, view: WorkspaceViewId) -> Self {
        self.workspace_view = Some(view);
        self
    }

    /// Attach the rendered active-reminders block for this turn. Empty
    /// blocks are ignored (no block, no packet entry).
    pub fn with_reminders_block(mut self, block: Option<String>) -> Self {
        let within_bounds = block
            .as_ref()
            .is_none_or(|text| !text.is_empty() && text.len() <= MAX_REMINDERS_BLOCK_BYTES);
        if within_bounds {
            self.reminders_block = block;
        }
        self
    }

    pub fn reminders_block(&self) -> Option<&str> {
        self.reminders_block.as_deref()
    }

    pub fn with_evidence(mut self, evidence: Vec<EvidenceId>) -> Self {
        self.evidence = evidence;
        self
    }

    pub fn with_artifacts(mut self, artifacts: Vec<ArtifactRef>) -> Result<Self, HostError> {
        if artifacts.len() > MAX_ARTIFACTS {
            return Err(HostError::InvalidPreserved);
        }
        self.artifacts = artifacts;
        Ok(self)
    }
}

/// The live in-flight model context the host owns and can rebuild.
#[derive(Clone, Debug)]
pub struct LiveContext {
    packet: ContextPacket,
    revision: ArtifactId,
    preserved: PreservedLiveContext,
}

impl LiveContext {
    pub fn packet(&self) -> &ContextPacket {
        &self.packet
    }
    pub fn revision(&self) -> ArtifactId {
        self.revision
    }
    pub fn preserved(&self) -> &PreservedLiveContext {
        &self.preserved
    }
}

/// Provider-adapter contract: produce one model step from the live context.
/// `tool_surface` is what the turn's tool driver advertises for structured
/// tool schemas (empty = no tools advertised).
pub trait LiveModelCall {
    fn step(
        &mut self,
        blocks: &[ContextBlock],
        input: &ModelStepInput<'_>,
        cancel: &CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError>;
}

/// A [`ModelDriver`] bound to the host-owned live context. Reads the (possibly
/// rebuilt) packet on every step, so overflow recovery changes what it sees.
pub struct LiveContextModelDriver<B> {
    live: Rc<RefCell<LiveContext>>,
    backing: B,
}

impl<B: LiveModelCall> ModelDriver for LiveContextModelDriver<B> {
    fn step(
        &mut self,
        input: &ModelStepInput<'_>,
        cancel: &CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError> {
        if cancel.is_cancelled() {
            return Err(ModelStepError::Cancelled);
        }
        let live = self.live.borrow();
        self.backing.step(live.packet().blocks(), input, cancel)
    }
}

/// Rebuilds the live context via the Context Fabric on a typed overflow.
pub struct LiveRecoveryController {
    live: Rc<RefCell<LiveContext>>,
    summary: Option<String>,
}

impl LiveRecoveryController {
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }
}

impl ContextController for LiveRecoveryController {
    fn recover_from_overflow(&mut self, _request: ContextOverflow) -> ContextRecoveryDecision {
        // Compact via compact_with_policy, not the raw compactor: this is the
        // fail-closed post-compaction verification path — the replacement's
        // re-estimated size is checked against the hard threshold before it
        // is ever accepted, typed StillOverHard rather than a silent
        // still-oversized summary. (Checking only the summary's own byte
        // length, as before, doesn't verify shrinkage against the budget at
        // all; that machinery existed in context-engine but had no caller.)
        {
            let live = self.live.borrow();
            let hard_tokens = live
                .preserved()
                .context_limit
                .saturating_sub(live.preserved().output_reserve)
                .max(1);
            let soft_tokens = hard_tokens.saturating_mul(4) / 5;
            let soft_tokens = soft_tokens.clamp(1, hard_tokens.saturating_sub(1).max(1));
            let policy = match CompactionPolicy::new(soft_tokens, hard_tokens, CompactionStrategy::ModelPreferred) {
                Ok(policy) => policy,
                Err(_) => return ContextRecoveryDecision::NotRecoverable,
            };
            let outcome = match compact_with_policy(live.packet(), &policy, None, &CeCancel::new()) {
                Ok(outcome) => outcome,
                Err(CompactPolicyError::StillOverHard { .. } | CompactPolicyError::InvalidPacket | CompactPolicyError::Cancelled) => {
                    return ContextRecoveryDecision::NotRecoverable;
                }
            };
            let summary = outcome
                .compacted
                .as_ref()
                .map(|compacted| compacted.summary().to_owned())
                .unwrap_or_default();
            if summary.is_empty() || summary.len() > MAX_COMPACTION_SUMMARY {
                self.summary = None;
            } else {
                self.summary = Some(summary);
            }
        }
        let new_packet = {
            let live = self.live.borrow();
            match build_packet(live.preserved(), self.summary.as_deref()) {
                Ok(packet) => packet,
                Err(_) => return ContextRecoveryDecision::NotRecoverable,
            }
        };
        {
            let mut live = self.live.borrow_mut();
            live.packet = new_packet;
            live.revision = ArtifactId::from_bytes(b"context/live-recovery");
        }
        ContextRecoveryDecision::Recovered
    }

    fn rebuilt_context(&self) -> Option<ContextRevision> {
        Some(ContextRevision::new(
            "context/live-recovery",
            Some(self.live.borrow().revision()),
        ))
    }
}

/// The context-owning host. Runs the recovery-capable executor over a live
/// context it can rebuild; each rebuilt context is a new [`ContextRevision`].
pub struct LiveContextHost<B> {
    live: Rc<RefCell<LiveContext>>,
    model: LiveContextModelDriver<B>,
    controller: LiveRecoveryController,
    policy: ContextRetryPolicy,
}

impl<B: LiveModelCall> LiveContextHost<B> {
    pub fn from_preserved(
        preserved: PreservedLiveContext,
        backing: B,
        policy: ContextRetryPolicy,
    ) -> Result<Self, HostError> {
        Self::build(preserved, backing, policy)
    }

    pub fn build(
        preserved: PreservedLiveContext,
        backing: B,
        policy: ContextRetryPolicy,
    ) -> Result<Self, HostError> {
        let packet = build_packet(&preserved, None).map_err(HostError::Compile)?;
        let revision = ArtifactId::from_bytes(b"context/live");
        let live = Rc::new(RefCell::new(LiveContext {
            packet,
            revision,
            preserved,
        }));
        let model = LiveContextModelDriver {
            live: Rc::clone(&live),
            backing,
        };
        let controller = LiveRecoveryController {
            live: Rc::clone(&live),
            summary: None,
        };
        Ok(Self {
            live,
            model,
            controller,
            policy,
        })
    }

    /// Run one agent turn through the recovery-capable executor. The outcome
    /// carries the provider-classified failure cause, when the turn failed on
    /// a model step.
    pub fn execute<T, E>(
        &mut self,
        request: &AgentExecutionRequest,
        tools: &mut T,
        events: &mut E,
        cancel: &CancellationToken,
    ) -> Result<AgentOutcome, AgentExecutionError>
    where
        T: ToolDriver,
        E: TurnEventSink,
    {
        TurnAgentExecutor.execute_with_context_recovery(
            request,
            &mut self.controller,
            self.policy,
            &mut self.model,
            tools,
            events,
            cancel,
        )
    }

    pub fn live_context(&self) -> &Rc<RefCell<LiveContext>> {
        &self.live
    }

    pub fn policy(&self) -> ContextRetryPolicy {
        self.policy
    }
}

impl From<CompileError> for HostError {
    fn from(err: CompileError) -> Self {
        Self::Compile(err)
    }
}

/// A [`ToolDriver`] with no tool gateway configured. Structural tool calls are
/// never executed; used by the bare CLI `exec` entry that has no tool wiring.
pub struct NoopTools;

impl ToolDriver for NoopTools {
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
    ) -> Result<ToolStepResult, ToolStepError> {
        Err(ToolStepError::Invalid)
    }
}

/// A [`LiveModelCall`] that has no provider configured. A real llm-router
/// adapter replaces this in production; without it a model step is a typed
/// provider failure (never a panic, never synthetic completion).
pub struct UnconfiguredModel;

impl LiveModelCall for UnconfiguredModel {
    fn step(
        &mut self,
        _blocks: &[ContextBlock],
        _input: &ModelStepInput<'_>,
        cancel: &CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError> {
        if cancel.is_cancelled() {
            Err(ModelStepError::Cancelled)
        } else {
            Err(ModelStepError::Failed)
        }
    }
}

/// Where bounded diagnostics lines go. `Quiet` is the default (zero output).
#[derive(Clone)]
pub enum DiagSink {
    Quiet,
    Stderr,
    Buffer(Rc<RefCell<Vec<String>>>),
}

/// Per-run diagnostics configuration: an endpoint host label plus a sink.
/// Lines are bounded and never carry prompt text, provider bodies, or
/// credential material.
#[derive(Clone)]
pub struct StepDiag {
    host: String,
    sink: DiagSink,
}

impl StepDiag {
    /// A disabled sink: every line is dropped, output stays byte-identical
    /// to a run without diagnostics.
    pub fn disabled() -> Self {
        Self {
            host: String::new(),
            sink: DiagSink::Quiet,
        }
    }

    /// Diagnostics to stderr, labelled with the endpoint host parsed from the
    /// provider base URL.
    pub fn stderr(base_url: &str) -> Self {
        Self {
            host: diag_host_from_base_url(base_url),
            sink: DiagSink::Stderr,
        }
    }

    /// Diagnostics into a shared buffer (test seam), with the buffer handed
    /// back for inspection.
    pub fn buffer(base_url: &str) -> (Self, Rc<RefCell<Vec<String>>>) {
        let buffer = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                host: diag_host_from_base_url(base_url),
                sink: DiagSink::Buffer(Rc::clone(&buffer)),
            },
            buffer,
        )
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    fn line(&self, text: String) {
        match &self.sink {
            DiagSink::Quiet => {}
            DiagSink::Stderr => {
                // Resilient: a failed stderr write must never panic inside a
                // model-step or tool-batch worker thread.
                crate::exec_diag::stderr_line(&text);
            }
            DiagSink::Buffer(buffer) => {
                let mut buffer = buffer.borrow_mut();
                if buffer.len() < MAX_DIAG_LINES {
                    buffer.push(text);
                }
            }
        }
    }
}

/// Extract a bounded host label from a provider base URL
/// (`scheme://host[:port]/...`). Userinfo, path, query, and fragment are
/// dropped; the label is truncated to [`MAX_DIAG_HOST_BYTES`].
pub fn diag_host_from_base_url(base_url: &str) -> String {
    let rest = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = authority.rsplit('@').next().unwrap_or(authority);
    host.chars().take(MAX_DIAG_HOST_BYTES).collect()
}

/// Short outcome tag for a failure cause in diagnostics lines.
fn cause_tag(cause: FailureCause) -> &'static str {
    match cause {
        FailureCause::Auth => "auth",
        FailureCause::Connection => "connection",
        FailureCause::Rejected => "rejected",
        FailureCause::Transient { .. } => "transient",
        FailureCause::Unspecified => "unspecified",
        // Future causes stay typed failures and classify as unspecified.
        _ => "unspecified",
    }
}

/// Wait out the bounded backoff before retry `attempt` (0-based), honoring a
/// provider retry-after hint (never shorter than the doubling backoff) and
/// staying cancellable in small slices. Returns false when cancelled.
fn sleep_backoff(cancel: &CancellationToken, attempt: u32, retry_after_ms: Option<u64>) -> bool {
    let base = retry_base_ms();
    let doubling = base << attempt.min(16);
    let wait_ms = retry_after_ms.map_or(doubling, |after| after.max(doubling));
    let mut waited = 0u64;
    while waited < wait_ms {
        if cancel.is_cancelled() {
            return false;
        }
        let slice = RETRY_SLEEP_SLICE_MS.min(wait_ms - waited);
        std::thread::sleep(std::time::Duration::from_millis(slice));
        waited += slice;
    }
    !cancel.is_cancelled()
}

/// Retry-after base in milliseconds: `RAPIDLM_RETRY_BASE_MS` when set and in
/// range, else [`RETRY_BACKOFF_BASE_MS`].
fn retry_base_ms() -> u64 {
    std::env::var("RAPIDLM_RETRY_BASE_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(|ms| ms.min(60_000))
        .unwrap_or(RETRY_BACKOFF_BASE_MS)
}

/// Step-layer supervision over the provider backing: accumulates
/// provider-reported tokens, retries transient-class failures, provider
/// rejections, and empty terminal responses with bounded backoff (honoring
/// provider retry-after hints and cancellation), and emits one diagnostics
/// line per attempt when enabled.
///
/// Only the model-step invocation itself is retried, and a failed step
/// proposed no tool calls — so already-committed tool effects are never
/// replayed by a retry.
struct SupervisedModel<B> {
    inner: B,
    counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    diag: Option<StepDiag>,
}

impl<B> SupervisedModel<B> {
    fn diag_attempt(&self, attempt: u32, outcome: &str, tokens: u64) {
        if let Some(diag) = &self.diag {
            diag.line(format!(
                "model step host={} attempt={attempt} outcome={outcome} tokens={tokens}",
                diag.host()
            ));
        }
    }
}

/// Bounded window of the most recent tool exchanges examined for a stall
/// pattern (Modbit `AGT-017`: detect repeated read/edit cycles without
/// progress, surface it rather than let the turn loop silently). Smaller
/// than `MAX_TOOL_HISTORY_EXCHANGES` on purpose — a stall is a recent-
/// behavior signal, not a whole-turn one.
const STALL_WINDOW: usize = 6;
/// An identical `(tool, arguments)` call repeated at least this many times
/// inside the window is reported as a stall.
const STALL_REPEAT_THRESHOLD: usize = 3;

/// Detect a model stuck repeating the exact same tool call with no distinct
/// progress in between. Returns a description of the repeated call when
/// found. Diagnostic-only: never fails or alters the turn, only surfaces a
/// `--verbose` warning line an operator or CI log can see. Injecting this
/// into the model's own context (so it can see and react to its own stall)
/// is further follow-up, not attempted here — see `newtask.md` §2.5.
fn detect_stall(history: &[agent_runtime::ToolStepExchange]) -> Option<String> {
    let window_start = history.len().saturating_sub(STALL_WINDOW);
    let mut counts: std::collections::BTreeMap<(&str, &str), usize> = std::collections::BTreeMap::new();
    for exchange in &history[window_start..] {
        for call in exchange.calls() {
            *counts.entry((call.tool(), call.arguments())).or_insert(0) += 1;
        }
    }
    let ((tool, _), count) = counts
        .into_iter()
        .find(|(_, count)| *count >= STALL_REPEAT_THRESHOLD)?;
    Some(format!(
        "{tool} repeated {count}x in the last {} exchange(s) with no distinct progress",
        history.len().min(STALL_WINDOW)
    ))
}

impl<B: LiveModelCall> LiveModelCall for SupervisedModel<B> {
    fn step(
        &mut self,
        blocks: &[context_engine::compile::ContextBlock],
        input: &ModelStepInput<'_>,
        cancel: &CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError> {
        if let Some(warning) = detect_stall(input.history())
            && let Some(diag) = &self.diag
        {
            diag.line(format!("stall warning: {warning}"));
        }
        // Connection and transient failures are retryable for a model step: a
        // step that failed committed no tool effects, so re-invoking is safe
        // (bounded retry ceiling).
        let mut attempt: u32 = 0;
        loop {
            let result = self.inner.step(blocks, input, cancel);
            match &result {
                Err(ModelStepError::ProviderFailed {
                    cause: FailureCause::Transient { retry_after_ms },
                }) if attempt < MAX_TRANSIENT_RETRIES => {
                    self.diag_attempt(attempt, "failed:transient", 0);
                    if !sleep_backoff(cancel, attempt, *retry_after_ms) {
                        return Err(ModelStepError::Cancelled);
                    }
                    attempt += 1;
                    continue;
                }
                Err(ModelStepError::ProviderFailed {
                    cause: FailureCause::Connection,
                }) if attempt < MAX_TRANSIENT_RETRIES => {
                    self.diag_attempt(attempt, "failed:connection", 0);
                    if !sleep_backoff(cancel, attempt, None) {
                        return Err(ModelStepError::Cancelled);
                    }
                    attempt += 1;
                    continue;
                }
                // A provider rejection of an already-shaped request (free-tier
                // rate limiting, transient capacity errors) is retryable: a
                // failed step committed no tool effects, so re-invoking is
                // safe within the bounded ceiling.
                Err(ModelStepError::ProviderFailed {
                    cause: FailureCause::Rejected,
                }) if attempt < MAX_TRANSIENT_RETRIES => {
                    self.diag_attempt(attempt, "failed:rejected", 0);
                    if !sleep_backoff(cancel, attempt, None) {
                        return Err(ModelStepError::Cancelled);
                    }
                    attempt += 1;
                    continue;
                }
                Err(ModelStepError::ProviderFailed { cause }) => {
                    let tag = cause_tag(*cause);
                    self.diag_attempt(attempt, &format!("failed:{tag}"), 0);
                    return result;
                }
                Err(ModelStepError::Failed) => {
                    self.diag_attempt(attempt, "failed:unspecified", 0);
                    return result;
                }
                Err(ModelStepError::BoundExceeded) => {
                    self.diag_attempt(attempt, "bound_exceeded", 0);
                    return result;
                }
                Err(ModelStepError::Cancelled) => {
                    self.diag_attempt(attempt, "cancelled", 0);
                    return result;
                }
                // Future ModelStepError variants stay typed failures (never
                // panics) and are classified as unspecified.
                Err(_) => {
                    self.diag_attempt(attempt, "failed:unspecified", 0);
                    return result;
                }
                Ok(ModelStepOutput::ToolCalls { tokens, .. }) => {
                    let tokens = *tokens;
                    self.counter
                        .fetch_add(tokens, std::sync::atomic::Ordering::Relaxed);
                    self.diag_attempt(attempt, "ok", tokens);
                    return result;
                }
                Ok(ModelStepOutput::Terminal { text, tokens }) => {
                    let tokens = *tokens;
                    self.counter
                        .fetch_add(tokens, std::sync::atomic::Ordering::Relaxed);
                    // An empty terminal response is a provider-side transient
                    // (the provider billed the request but returned nothing):
                    // retry under the same bounded backoff instead of failing
                    // the turn on first occurrence.
                    if text.is_empty() && attempt < MAX_EMPTY_RESPONSE_RETRIES {
                        self.diag_attempt(attempt, "empty_response", tokens);
                        if !sleep_backoff(cancel, attempt, None) {
                            return Err(ModelStepError::Cancelled);
                        }
                        attempt += 1;
                        continue;
                    }
                    self.diag_attempt(
                        attempt,
                        if text.is_empty() {
                            "empty_response"
                        } else {
                            "ok"
                        },
                        tokens,
                    );
                    return result;
                }
            }
        }
    }
}

/// A model backing bound to a user-configured ordered fallback chain
/// (`[models] fallback = [...]`). Fully self-contained — it does its own
/// retry-with-backoff and cross-model fallback internally and always
/// returns a final, resolved outcome, so the caller never needs a separate
/// retry loop around it (unlike [`SupervisedModel`], which only retries the
/// single backend it wraps).
///
/// Auth/config/safety failures fall back only onto a model the user
/// actually named in `fallback`; transient/rate-limited failures retry the
/// current backend up to the policy bound first, then fall back in the
/// user's configured order. When the chain is exhausted, the *original*
/// typed error from the last attempt is returned — never a fabricated
/// success and never a switch to a model the user did not approve.
pub struct FallbackChainModel<B> {
    backends: Vec<(ModelRef, B)>,
    controller: FallbackController,
    diag: Option<StepDiag>,
}

impl<B: LiveModelCall> FallbackChainModel<B> {
    /// `backends` must include an entry for `controller.current()` and every
    /// model `controller.chain()` can ever name — the composition root
    /// builds both from the same resolved `[models] fallback` list, so this
    /// invariant holds by construction.
    pub fn new(backends: Vec<(ModelRef, B)>, controller: FallbackController, diag: Option<StepDiag>) -> Self {
        Self { backends, controller, diag }
    }

    fn backend_mut(&mut self, target: &ModelRef) -> &mut B {
        &mut self
            .backends
            .iter_mut()
            .find(|(model_ref, _)| model_ref == target)
            .expect("FallbackController only names models present in `backends`")
            .1
    }

    fn diag_line(&self, line: String) {
        if let Some(diag) = &self.diag {
            diag.line(line);
        }
    }
}

impl<B: LiveModelCall> LiveModelCall for FallbackChainModel<B> {
    fn step(
        &mut self,
        blocks: &[ContextBlock],
        input: &ModelStepInput<'_>,
        cancel: &CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError> {
        loop {
            if cancel.is_cancelled() {
                return Err(ModelStepError::Cancelled);
            }
            let current = self.controller.current().clone();
            let result = self.backend_mut(&current).step(blocks, input, cancel);
            let err = match result {
                Ok(output) => {
                    self.diag_line(format!("fallback model={} outcome=ok", model_label(&current)));
                    return Ok(output);
                }
                Err(ModelStepError::Cancelled) => return Err(ModelStepError::Cancelled),
                Err(err) => err,
            };
            let trigger = to_fallback_trigger(&err);
            let router_cancel = RouterCancellationToken::new();
            let plan = match self.controller.plan(&trigger, AttemptProgress::PreResponse, &router_cancel) {
                Ok(plan) => plan,
                // The controller itself failed closed (e.g. an internal
                // bound) — surface the original error rather than a
                // second-order fallback failure the caller can't act on.
                Err(_) => return Err(err),
            };
            // apply() only fails on a stale/already-terminal plan, neither
            // of which is reachable here (plan was just computed fresh from
            // the controller's own current state) — best-effort, never
            // panics either way.
            let _ = self.controller.apply(&plan);
            match &plan {
                FallbackPlan::PreResponse { action: FallbackAction::RetrySame { backoff_ms, .. }, .. } => {
                    self.diag_line(format!(
                        "fallback model={} outcome=retry backoff_ms={backoff_ms}",
                        model_label(&current)
                    ));
                    if !sleep_millis_cancellable(cancel, *backoff_ms) {
                        return Err(ModelStepError::Cancelled);
                    }
                }
                FallbackPlan::PreResponse { action: FallbackAction::FallbackTo { to, backoff_ms, .. }, .. } => {
                    self.diag_line(format!(
                        "fallback model={} -> {} backoff_ms={backoff_ms}",
                        model_label(&current),
                        model_label(to)
                    ));
                    if !sleep_millis_cancellable(cancel, *backoff_ms) {
                        return Err(ModelStepError::Cancelled);
                    }
                }
                FallbackPlan::PreResponse { action: FallbackAction::Stop { reason, .. }, .. } => {
                    self.diag_line(format!(
                        "fallback model={} outcome=stop reason={}",
                        model_label(&current),
                        reason.as_str()
                    ));
                    return Err(err);
                }
                FallbackPlan::PartiallyStreamed { reason, .. } | FallbackPlan::ToolSideEffect { reason, .. } => {
                    self.diag_line(format!(
                        "fallback model={} outcome=stop reason={}",
                        model_label(&current),
                        reason.as_str()
                    ));
                    return Err(err);
                }
            }
        }
    }
}

fn model_label(model: &ModelRef) -> String {
    format!("{}/{}", model.provider().as_str(), model.model().as_str())
}

/// Reconstruct the `llm_router` failure classification from the typed
/// `ModelStepError` the step layer already produced. `ModelStepError`
/// deliberately doesn't carry the original `ProviderError` (agent-runtime
/// doesn't depend on llm-router), so this is a best-effort but faithful
/// reverse mapping of `model.rs::map_provider_error`'s forward one — the
/// one ambiguous case is `FailureCause::Rejected`, which collapses
/// `InvalidRequest`/`Permanent`/`UnknownVariant`; mapped to `InvalidRequest`
/// (`FailureClass::Config`) since a request malformed for one provider's
/// dialect may still be valid for another, and the user's configured
/// fallback chain is exactly the mechanism to let that recover.
fn to_fallback_trigger(err: &ModelStepError) -> FallbackTrigger {
    let provider_error = match err {
        ModelStepError::Cancelled => ProviderError::Cancelled,
        ModelStepError::BoundExceeded => ProviderError::ContextTooLarge,
        ModelStepError::Failed => ProviderError::Permanent,
        ModelStepError::ProviderFailed { cause } => match cause {
            FailureCause::Auth => ProviderError::AuthFailed,
            FailureCause::Connection => ProviderError::Connection,
            FailureCause::Rejected => ProviderError::InvalidRequest,
            FailureCause::Transient { retry_after_ms: Some(after) } => {
                ProviderError::RateLimited { retry_after_ms: Some(*after) }
            }
            FailureCause::Transient { retry_after_ms: None } => ProviderError::Transient,
            FailureCause::Unspecified => ProviderError::Permanent,
            // #[non_exhaustive]: an unrecognized future cause fails closed
            // rather than being guessed into a retryable class.
            _ => ProviderError::Permanent,
        },
        // #[non_exhaustive]: same fail-closed default for a future
        // ModelStepError variant this match doesn't know about yet.
        _ => ProviderError::Permanent,
    };
    FallbackTrigger::Provider(provider_error)
}

/// Sleep for exactly `wait_ms`, checking cancellation on each slice. Unlike
/// `sleep_backoff`, the caller (here, `FallbackController::plan`) already
/// computed the full backoff value — this does not re-derive it from an
/// attempt counter.
fn sleep_millis_cancellable(cancel: &CancellationToken, wait_ms: u64) -> bool {
    let mut waited = 0u64;
    while waited < wait_ms {
        if cancel.is_cancelled() {
            return false;
        }
        let slice = RETRY_SLEEP_SLICE_MS.min(wait_ms - waited);
        std::thread::sleep(std::time::Duration::from_millis(slice));
        waited += slice;
    }
    !cancel.is_cancelled()
}

/// What one `exec`/`goal` turn produced: the canonical result, the
/// provider-classified failure cause (when the turn failed on a model step),
/// the failing-tool detail (when the turn stopped on a tool failure), the
/// typed stop reason and tool-call count (for exit-code decisions), and the
/// provider-reported token total.
#[derive(Clone, Debug)]
pub struct ExecOutcome {
    pub result: AgentResult,
    pub failure_cause: Option<FailureCause>,
    pub failure_detail: Option<TurnFailureDetail>,
    pub stop_reason: Option<TurnStopReason>,
    pub tool_calls: u32,
    pub tokens: u64,
}

/// Production entry used by the CLI `exec`/`goal` command: build the
/// context-owning host and run one agent turn through the recovery-capable
/// executor. A real provider adapter is injected as the `backing`. Transient
/// provider failures are retried at the step layer with bounded backoff;
/// `diag` opts into bounded per-attempt diagnostics (off by default).
#[allow(clippy::too_many_arguments)]
pub fn run_live_exec<B, T, E>(
    preserved: PreservedLiveContext,
    backing: B,
    request: &AgentExecutionRequest,
    tools: &mut T,
    events: &mut E,
    cancel: &CancellationToken,
    policy: ContextRetryPolicy,
    diag: Option<StepDiag>,
) -> Result<ExecOutcome, AgentExecutionError>
where
    B: LiveModelCall,
    T: ToolDriver,
    E: TurnEventSink,
{
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let turn_diag = diag.clone();
    let supervised = SupervisedModel {
        inner: backing,
        counter: std::sync::Arc::clone(&counter),
        diag,
    };
    let mut host = LiveContextHost::build(preserved, supervised, policy)
        .map_err(|_| AgentExecutionError::InvalidRequest)?;
    let outcome = host.execute(request, tools, events, cancel)?;
    let tokens = counter.load(std::sync::atomic::Ordering::Relaxed);
    if let Some(diag) = turn_diag {
        let mut line = format!(
            "turn outcome={} tokens={tokens}",
            outcome.result.status().as_str()
        );
        if let Some(cause) = outcome.failure_cause {
            line.push_str(&format!(" cause={}", cause_tag(cause)));
        } else if let Some(reason) = outcome.stop_reason {
            line.push_str(&format!(" reason={}", reason.as_str()));
        }
        diag.line(line);
    }
    Ok(ExecOutcome {
        result: outcome.result,
        failure_cause: outcome.failure_cause,
        failure_detail: outcome.failure_detail,
        stop_reason: outcome.stop_reason,
        tool_calls: outcome.tool_calls,
        tokens,
    })
}

/// Load the project memory index (`.rapidlm/MEMORY.md`): a bounded,
/// always-loaded pointer file the model can rely on (Claude MEMORY.md
/// parity). Missing file → None; oversized content is truncated to the line
/// and byte bounds rather than dropped entirely.
pub fn load_memory_index(root: &Path) -> Option<String> {
    let path = root.join(".rapidlm").join("MEMORY.md");
    let text = fs::read_to_string(path).ok()?;
    let mut bounded: Vec<&str> = text.lines().take(MAX_MEMORY_INDEX_LINES).collect();
    let mut size = bounded.iter().map(|line| line.len() + 1).sum::<usize>();
    while size > MAX_MEMORY_INDEX_BYTES && !bounded.is_empty() {
        size -= bounded.last().map(|line| line.len() + 1).unwrap_or(0);
        bounded.pop();
    }
    if bounded.is_empty() {
        return None;
    }
    Some(bounded.join("\n"))
}

/// Compile a live [`ContextPacket`] from preserved state + optional compaction
/// summary. This is the single Context-Fabric rebuild path.
pub fn build_packet(
    preserved: &PreservedLiveContext,
    summary: Option<&str>,
) -> Result<ContextPacket, CompileError> {
    let mut ctx = CompileContext::new(preserved.context_limit, preserved.output_reserve)
        .task("live agent turn")
        .goal(preserved.goal_statement.clone());
    if let Some(system_prompt) = preserved.system_prompt() {
        ctx = ctx.system(CompileInput::new(
            "system/prompt",
            system_prompt.to_owned(),
        ));
    }
    if let Some(memory) = preserved.memory_index() {
        ctx = ctx.system(CompileInput::new(
            "memory/index",
            memory.to_owned(),
        ));
    }
    if !preserved.agents_rules.is_empty() {
        ctx = ctx.system(CompileInput::new(
            "rules/agents",
            preserved.agents_rules.clone(),
        ));
    }
    if !preserved.skills.is_empty() {
        ctx = ctx.system(CompileInput::new(
            "skills/selected",
            preserved.skills.clone(),
        ));
    }
    for criterion in &preserved.completion_criteria {
        ctx = ctx.goal_block(CompileInput::new("criterion", criterion.clone()));
    }
    if let Some(summary) = summary
        && !summary.is_empty() {
            ctx = ctx.memory(CompileInput::new("context/compaction", summary.to_owned()));
        }
    if let Some(reminders) = preserved.reminders_block() {
        ctx = ctx.system(CompileInput::new("reminders/active", reminders.to_owned()));
    }
    for block in preserved.retrieved_context() {
        ctx = ctx.retrieved(block.clone());
    }
    compile(&ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_runtime::{
        AgentRole, AgentSpec, ModelStepOutput, ProposedToolCall, ToolStepError, ToolStepResult,
        ValidatedToolCall,
    };
    use context_engine::retrieval::candidates::{Freshness, TrustClass};
    use protocol::{AgentId, SessionId};
    use std::collections::VecDeque;

    fn exchange(tool: &str, arguments: &str) -> agent_runtime::ToolStepExchange {
        let call = ProposedToolCall::new("c", tool, arguments).expect("call");
        agent_runtime::ToolStepExchange::new(
            vec![call],
            vec![ToolStepResult::Succeeded {
                call_id: "c".to_owned(),
                summary: "ok".to_owned(),
            }],
        )
    }

    #[test]
    fn stall_detection_flags_the_same_call_repeated_and_ignores_varied_history() {
        // Three identical repo_read calls on the same path within the
        // window: a real stall, no distinct progress in between.
        let stalled = vec![
            exchange("repo_read", r#"{"path":"a.rs"}"#),
            exchange("repo_read", r#"{"path":"a.rs"}"#),
            exchange("repo_read", r#"{"path":"a.rs"}"#),
        ];
        let warning = detect_stall(&stalled).expect("stall detected");
        assert!(warning.contains("repo_read"), "{warning}");
        assert!(warning.contains('3'), "{warning}");

        // Same tool, different arguments each time: real progress, not a
        // stall, even though the tool name repeats.
        let varied = vec![
            exchange("repo_read", r#"{"path":"a.rs"}"#),
            exchange("repo_read", r#"{"path":"b.rs"}"#),
            exchange("repo_read", r#"{"path":"c.rs"}"#),
        ];
        assert!(detect_stall(&varied).is_none());

        // Below the repeat threshold: two repeats is not (yet) a stall.
        let below_threshold = vec![
            exchange("repo_read", r#"{"path":"a.rs"}"#),
            exchange("repo_read", r#"{"path":"a.rs"}"#),
        ];
        assert!(detect_stall(&below_threshold).is_none());

        assert!(detect_stall(&[]).is_none());
    }

    #[test]
    fn stall_detection_only_looks_at_the_recent_window() {
        // A repeated call far in the past, outside STALL_WINDOW, must not
        // count against a turn that has since moved on to distinct calls.
        let mut history = vec![
            exchange("repo_read", r#"{"path":"a.rs"}"#),
            exchange("repo_read", r#"{"path":"a.rs"}"#),
            exchange("repo_read", r#"{"path":"a.rs"}"#),
        ];
        for i in 0..STALL_WINDOW {
            history.push(exchange("repo_read", &format!(r#"{{"path":"distinct-{i}.rs"}}"#)));
        }
        assert!(
            detect_stall(&history).is_none(),
            "old repetition outside the window must not still be flagged"
        );
    }

    /// Scripted backing whose call log is shared across clones, so a test can
    /// hand the backing to `run_live_exec` by value and still observe how many
    /// times each step was invoked.
    struct ScriptedBacking {
        outputs: Rc<RefCell<VecDeque<Result<ModelStepOutput, ModelStepError>>>>,
        saw_blocks: Rc<RefCell<Vec<usize>>>,
    }
    impl ScriptedBacking {
        fn new(outputs: Vec<Result<ModelStepOutput, ModelStepError>>) -> Self {
            Self {
                outputs: Rc::new(RefCell::new(outputs.into())),
                saw_blocks: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }
    impl Clone for ScriptedBacking {
        fn clone(&self) -> Self {
            Self {
                outputs: Rc::clone(&self.outputs),
                saw_blocks: Rc::clone(&self.saw_blocks),
            }
        }
    }
    impl LiveModelCall for ScriptedBacking {
        fn step(
            &mut self,
            blocks: &[ContextBlock],
            _input: &ModelStepInput<'_>,
            cancel: &CancellationToken,
        ) -> Result<ModelStepOutput, ModelStepError> {
            if cancel.is_cancelled() {
                return Err(ModelStepError::Cancelled);
            }
            self.saw_blocks.borrow_mut().push(blocks.len());
            self.outputs
                .borrow_mut()
                .pop_front()
                .unwrap_or(Err(ModelStepError::Failed))
        }
    }

    // --- FallbackChainModel -------------------------------------------------

    fn model_ref(provider: &str, model: &str) -> ModelRef {
        ModelRef::new(
            llm_router::provider::ProviderId::parse(provider).expect("provider"),
            llm_router::provider::ModelId::parse(model).expect("model"),
        )
    }

    fn chain_controller(primary: ModelRef, alternates: Vec<ModelRef>) -> FallbackController {
        FallbackController::from_explicit_chain(
            primary,
            alternates,
            llm_router::fallback::FallbackPolicy::standard(),
            &RouterCancellationToken::new(),
        )
        .expect("controller")
    }

    fn ok_terminal(text: &str) -> Result<ModelStepOutput, ModelStepError> {
        Ok(ModelStepOutput::Terminal { text: text.to_owned(), tokens: 1 })
    }

    fn auth_failure() -> Result<ModelStepOutput, ModelStepError> {
        Err(ModelStepError::ProviderFailed { cause: FailureCause::Auth })
    }

    fn connection_failure() -> Result<ModelStepOutput, ModelStepError> {
        Err(ModelStepError::ProviderFailed { cause: FailureCause::Connection })
    }

    fn step_input() -> ModelStepInput<'static> {
        ModelStepInput::without_tools(1)
    }

    #[test]
    fn fallback_switches_to_the_configured_alternate_on_auth_failure() {
        let primary_ref = model_ref("b-ai", "deepseek");
        let alt_ref = model_ref("openrouter", "ling-3");
        let controller = chain_controller(primary_ref.clone(), vec![alt_ref.clone()]);
        let primary = ScriptedBacking::new(vec![auth_failure()]);
        let alt = ScriptedBacking::new(vec![ok_terminal("from alternate")]);
        let mut chain = FallbackChainModel::new(
            vec![(primary_ref, primary), (alt_ref, alt)],
            controller,
            None,
        );
        let output = chain
            .step(&[], &step_input(), &CancellationToken::new())
            .expect("recovers onto the alternate");
        match output {
            ModelStepOutput::Terminal { text, .. } => assert_eq!(text, "from alternate"),
            other => panic!("expected terminal, got {other:?}"),
        }
    }

    #[test]
    fn fallback_never_switches_when_nothing_is_configured() {
        // Regression guard for the design's central safety property: no
        // configured alternates means an auth failure surfaces exactly as
        // it always has, never a silent substitution.
        let primary_ref = model_ref("b-ai", "deepseek");
        let controller = chain_controller(primary_ref.clone(), Vec::new());
        let primary = ScriptedBacking::new(vec![auth_failure()]);
        let mut chain = FallbackChainModel::new(vec![(primary_ref, primary)], controller, None);
        let err = chain
            .step(&[], &step_input(), &CancellationToken::new())
            .expect_err("no alternate configured, must stay a typed failure");
        assert_eq!(err, ModelStepError::ProviderFailed { cause: FailureCause::Auth });
    }

    #[test]
    fn fallback_retries_the_same_backend_for_transient_failures_before_falling_back() {
        let primary_ref = model_ref("b-ai", "deepseek");
        let alt_ref = model_ref("openrouter", "ling-3");
        let controller = chain_controller(primary_ref.clone(), vec![alt_ref.clone()]);
        // Standard policy allows 2 same-model retries before falling back;
        // recovering on the 2nd attempt must never touch the alternate.
        let primary = ScriptedBacking::new(vec![
            connection_failure(),
            connection_failure(),
            ok_terminal("recovered on the same backend"),
        ]);
        let alt = ScriptedBacking::new(vec![ok_terminal("must not be reached")]);
        let mut chain = FallbackChainModel::new(
            vec![(primary_ref, primary), (alt_ref, alt.clone())],
            controller,
            None,
        );
        let output = chain
            .step(&[], &step_input(), &CancellationToken::new())
            .expect("recovers without falling back");
        match output {
            ModelStepOutput::Terminal { text, .. } => {
                assert_eq!(text, "recovered on the same backend");
            }
            other => panic!("expected terminal, got {other:?}"),
        }
        assert_eq!(
            alt.outputs.borrow().len(),
            1,
            "the alternate backend must never have been called"
        );
    }

    #[test]
    fn fallback_falls_back_after_exhausting_same_model_retries_on_transient_failure() {
        let primary_ref = model_ref("b-ai", "deepseek");
        let alt_ref = model_ref("openrouter", "ling-3");
        let controller = chain_controller(primary_ref.clone(), vec![alt_ref.clone()]);
        // Standard policy allows 2 same-model retries; a 3rd consecutive
        // transient failure must exhaust the budget and fall back.
        let primary = ScriptedBacking::new(vec![
            connection_failure(),
            connection_failure(),
            connection_failure(),
        ]);
        let alt = ScriptedBacking::new(vec![ok_terminal("from alternate after exhaustion")]);
        let mut chain = FallbackChainModel::new(
            vec![(primary_ref, primary), (alt_ref, alt)],
            controller,
            None,
        );
        let output = chain
            .step(&[], &step_input(), &CancellationToken::new())
            .expect("falls back once same-model retries are exhausted");
        match output {
            ModelStepOutput::Terminal { text, .. } => {
                assert_eq!(text, "from alternate after exhaustion");
            }
            other => panic!("expected terminal, got {other:?}"),
        }
    }

    #[test]
    fn fallback_returns_the_original_error_once_the_whole_chain_is_exhausted() {
        let primary_ref = model_ref("b-ai", "deepseek");
        let alt_ref = model_ref("openrouter", "ling-3");
        let controller = chain_controller(primary_ref.clone(), vec![alt_ref.clone()]);
        let primary = ScriptedBacking::new(vec![auth_failure()]);
        let alt = ScriptedBacking::new(vec![auth_failure()]);
        let mut chain = FallbackChainModel::new(
            vec![(primary_ref, primary), (alt_ref, alt)],
            controller,
            None,
        );
        let err = chain
            .step(&[], &step_input(), &CancellationToken::new())
            .expect_err("both backends fail, chain exhausted");
        assert_eq!(err, ModelStepError::ProviderFailed { cause: FailureCause::Auth });
    }

    #[test]
    fn fallback_never_falls_back_on_context_too_large() {
        // A different model's context window is a routing/config decision,
        // not something this chain is allowed to guess its way around —
        // classify_failure() already stops on ContextTooLarge unconditionally
        // upstream in llm-router; this asserts the wiring respects that.
        let primary_ref = model_ref("b-ai", "deepseek");
        let alt_ref = model_ref("openrouter", "ling-3");
        let controller = chain_controller(primary_ref.clone(), vec![alt_ref.clone()]);
        let primary = ScriptedBacking::new(vec![Err(ModelStepError::BoundExceeded)]);
        let alt = ScriptedBacking::new(vec![ok_terminal("must not be reached")]);
        let mut chain = FallbackChainModel::new(
            vec![(primary_ref, primary), (alt_ref, alt)],
            controller,
            None,
        );
        let err = chain
            .step(&[], &step_input(), &CancellationToken::new())
            .expect_err("ContextTooLarge must stop, not fall back");
        assert_eq!(err, ModelStepError::BoundExceeded);
    }

    struct CountingTools {
        executed: u32,
    }
    impl ToolDriver for CountingTools {
        fn validate(
            &mut self,
            call: &agent_runtime::ProposedToolCall,
            _cancel: &CancellationToken,
        ) -> Result<ValidatedToolCall, ToolStepError> {
            Ok(ValidatedToolCall::from_proposed(call))
        }
        fn execute(
            &mut self,
            call: &ValidatedToolCall,
            _cancel: &CancellationToken,
        ) -> Result<ToolStepResult, ToolStepError> {
            self.executed += 1;
            Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: "ok".to_owned(),
            })
        }
    }

    fn preserved() -> PreservedLiveContext {
        PreservedLiveContext::new(
            "implement feature X",
            vec!["tests pass".to_owned()],
            "trusted AGENTS rules".to_owned(),
            "selected skill".to_owned(),
            8192,
            256,
        )
        .expect("preserved")
    }

    fn spec() -> AgentSpec {
        AgentSpec::builder(
            AgentId::new(),
            AgentRole::Coder,
            "implement",
            WorkspaceViewId::new(),
        )
        .permissions_profile("work")
        .build()
        .expect("spec")
    }

    fn overflow_then_terminal(text: &str) -> ScriptedBacking {
        ScriptedBacking::new(vec![
            Err(ModelStepError::BoundExceeded),
            Ok(ModelStepOutput::Terminal {
                text: text.to_owned(),
                tokens: 1,
            }),
        ])
    }

    fn run_session(
        mut host: LiveContextHost<ScriptedBacking>,
    ) -> Result<AgentOutcome, AgentExecutionError> {
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        host.execute(
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
        )
    }

    #[test]
    fn reminders_block_is_compiled_into_the_packet_as_a_system_block() {
        let block = "reminders schema=rapidlm.reminders.v1 feeds=ops\n\
                     [ops/budget-guard floor=medium] stay under the time budget\n";
        let with_reminders = preserved().with_reminders_block(Some(block.to_owned()));
        let packet = build_packet(&with_reminders, None).expect("packet");
        let reminders = packet
            .blocks()
            .iter()
            .find(|b| b.text().contains("budget-guard"))
            .expect("reminders block in packet");
        assert_eq!(reminders.source(), context_engine::compile::ContextSource::System);
        // Out-of-bounds blocks are refused: nothing enters the packet.
        let oversized = "x".repeat(MAX_REMINDERS_BLOCK_BYTES + 1);
        let with_oversized = preserved().with_reminders_block(Some(oversized));
        let packet = build_packet(&with_oversized, None).expect("packet");
        assert!(packet.blocks().iter().all(|b| !b.text().contains("reminders schema")));
        // None adds no block.
        let packet = build_packet(&preserved(), None).expect("packet");
        assert!(packet.blocks().iter().all(|b| !b.text().contains("reminders schema")));
    }

    #[test]
    fn retrieved_context_is_compiled_into_the_packet_as_untrusted_fresh_blocks() {
        let block = CompileInput::new("retrieved:lru.py", "class LRUCache:\n    pass\n")
            .reason(context_engine::compile::CompileReason::Retrieved)
            .trust(TrustClass::Untrusted)
            .freshness(Freshness::Fresh);
        let with_retrieved = preserved().with_retrieved_context(vec![block]);
        let packet = build_packet(&with_retrieved, None).expect("packet");
        let retrieved = packet
            .blocks()
            .iter()
            .find(|b| b.text().contains("LRUCache"))
            .expect("retrieved block in packet");
        assert_eq!(retrieved.source(), context_engine::compile::ContextSource::Retrieved);
        assert_eq!(retrieved.trust(), TrustClass::Untrusted);
        assert_eq!(retrieved.freshness(), Freshness::Fresh);
        // No retrieved blocks configured: none enter the packet either.
        let packet = build_packet(&preserved(), None).expect("packet");
        assert!(packet.blocks().iter().all(|b| !b.text().contains("LRUCache")));
    }

    #[test]
    fn overflow_rebuilds_live_context_and_recovers() {
        let host = LiveContextHost::build(
            preserved(),
            overflow_then_terminal("after rebuild"),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let result = run_session(host).expect("execute").result;
        assert_eq!(
            result.status(),
            agent_runtime::AgentTerminalStatus::Succeeded
        );
        assert_eq!(result.summary(), "after rebuild");
        // Lineage records the recovery transition without raw context contents.
        assert_eq!(result.context_lineage().len(), 1);
        assert_eq!(
            result.context_lineage()[0].source(),
            "context/live-recovery"
        );
    }

    #[test]
    fn normal_context_succeeds_without_recovery() {
        let host = LiveContextHost::build(
            preserved(),
            ScriptedBacking::new(vec![Ok(ModelStepOutput::Terminal {
                text: "done".to_owned(),
                tokens: 1,
            })]),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let result = run_session(host).expect("execute").result;
        assert_eq!(result.summary(), "done");
        assert!(result.context_lineage().is_empty());
    }

    #[test]
    fn repeated_overflow_fails_closed_with_typed_limit() {
        let host = LiveContextHost::build(
            preserved(),
            ScriptedBacking::new(vec![
                Err(ModelStepError::BoundExceeded),
                Err(ModelStepError::BoundExceeded),
                Err(ModelStepError::BoundExceeded),
            ]),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let err = run_session(host).expect_err("bounded stop");
        assert_eq!(err, AgentExecutionError::ContextRetryExceeded);
    }

    #[test]
    fn provider_failure_does_not_trigger_compaction() {
        let host = LiveContextHost::build(
            preserved(),
            ScriptedBacking::new(vec![Err(ModelStepError::Failed)]),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        // Provider failure is typed `ModelFailed`, never `ContextBoundExceeded`,
        // so the recovery controller is not consulted (no compaction).
        let result = run_session(host).expect("execute").result;
        assert_eq!(result.status(), agent_runtime::AgentTerminalStatus::Failed);
        assert!(result.context_lineage().is_empty());
    }

    #[test]
    fn committed_tool_effects_are_not_replayed_on_overflow() {
        let call = ProposedToolCall::new("c1", "repo_read", "{}").expect("call");
        let mut host = LiveContextHost::build(
            preserved(),
            ScriptedBacking::new(vec![
                Ok(ModelStepOutput::ToolCalls {
                    calls: vec![call],
                    tokens: 1,
                }),
                Err(ModelStepError::BoundExceeded),
            ]),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut tools = CountingTools { executed: 0 };
        let mut events = Vec::new();
        let err = host
            .execute(&request, &mut tools, &mut events, &CancellationToken::new())
            .expect_err("fail closed");
        assert_eq!(err, AgentExecutionError::ContextOverflowAfterEffects);
        assert_eq!(tools.executed, 1, "effect executed once, never replayed");
    }

    #[test]
    fn cancellation_during_recovery_stops_execution() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut host = LiveContextHost::build(
            preserved(),
            overflow_then_terminal("never"),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let err = host
            .execute(
                &request,
                &mut CountingTools { executed: 0 },
                &mut events,
                &cancel,
            )
            .expect_err("cancelled");
        assert_eq!(err, AgentExecutionError::Cancelled);
    }

    #[test]
    fn preserved_goal_rules_skills_survive_rebuild() {
        let preserved = preserved();
        let host = LiveContextHost::build(
            preserved.clone(),
            overflow_then_terminal("ok"),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let live = host.live_context().borrow();
        let p = live.preserved();
        assert_eq!(p.goal_statement, "implement feature X");
        assert_eq!(p.completion_criteria, vec!["tests pass".to_owned()]);
        assert_eq!(p.agents_rules, "trusted AGENTS rules");
        assert_eq!(p.skills, "selected skill");
    }

    #[test]
    fn cli_host_entry_reaches_recovery_capable_executor() {
        // Drives the exact entry a CLI `exec`/`goal` command uses (run_live_exec):
        // a goal/turn reaches the recovery-capable executor end-to-end.
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let outcome = run_live_exec(
            preserved(),
            overflow_then_terminal("wired recovery"),
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.result.summary(), "wired recovery");
        assert_eq!(outcome.tokens, 1, "terminal step's provider tokens are reported");
        assert_eq!(outcome.failure_cause, None, "success carries no cause");
        assert_eq!(outcome.result.context_lineage().len(), 1);
        assert_eq!(
            outcome.result.context_lineage()[0].source(),
            "context/live-recovery"
        );
    }

    #[test]
    fn unconfigured_provider_is_a_typed_failure_not_synthetic_completion() {
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let outcome = run_live_exec(
            preserved(),
            UnconfiguredModel,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.result.status(), agent_runtime::AgentTerminalStatus::Failed);
        assert_eq!(outcome.tokens, 0, "failed turns report no provider tokens");
        assert_eq!(
            outcome.failure_cause,
            Some(FailureCause::Unspecified),
            "unconfigured backing is an unspecified provider failure"
        );
        assert!(outcome.result.context_lineage().is_empty(), "no fake recovery");
    }

    #[test]
    fn transient_step_failure_retries_then_succeeds() {
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        // Sequence: transient blip → context overflow (recovery) → terminal ok.
        let mut outputs = vec![Err(ModelStepError::ProviderFailed {
            cause: FailureCause::Transient { retry_after_ms: None },
        })];
        outputs.push(Err(ModelStepError::BoundExceeded));
        outputs.push(Ok(ModelStepOutput::Terminal {
            text: "after transient blip".to_owned(),
            tokens: 2,
        }));
        let backing = ScriptedBacking::new(outputs);
        let witness = backing.clone();
        let outcome = run_live_exec(
            preserved(),
            backing,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("transient failure recovers without operator action");
        assert_eq!(outcome.result.status(), agent_runtime::AgentTerminalStatus::Succeeded);
        assert_eq!(outcome.result.summary(), "after transient blip");
        assert_eq!(outcome.failure_cause, None);
        assert_eq!(
            witness.saw_blocks.borrow().len(),
            3,
            "transient step retried (plus one context recovery)"
        );
        assert_eq!(outcome.tokens, 2);
    }

    #[test]
    fn exhausted_transient_retries_fail_with_transient_cause() {
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let transient = || {
            Err(ModelStepError::ProviderFailed {
                cause: FailureCause::Transient {
                    retry_after_ms: None,
                },
            })
        };
        let failures: Vec<_> = (0..=MAX_TRANSIENT_RETRIES as usize)
            .map(|_| transient())
            .collect();
        let backing = ScriptedBacking::new(failures);
        let witness = backing.clone();
        let outcome = run_live_exec(
            preserved(),
            backing,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("bounded retry exhaustion is a typed Failed result");
        assert_eq!(outcome.result.status(), agent_runtime::AgentTerminalStatus::Failed);
        assert_eq!(
            outcome.failure_cause,
            Some(FailureCause::Transient { retry_after_ms: None }),
            "the cause class survives to the CLI boundary"
        );
        assert_eq!(
            witness.saw_blocks.borrow().len(),
            MAX_TRANSIENT_RETRIES as usize + 1,
            "initial attempt plus the bounded retries"
        );
        assert_eq!(outcome.tokens, 0);
    }

    #[test]
    fn non_transient_provider_failure_is_not_retried() {
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let backing = ScriptedBacking::new(vec![
            Err(ModelStepError::ProviderFailed {
                cause: FailureCause::Auth,
            }),
            Ok(ModelStepOutput::Terminal {
                text: "never reached".to_owned(),
                tokens: 1,
            }),
        ]);
        let witness = backing.clone();
        let outcome = run_live_exec(
            preserved(),
            backing,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.result.status(), agent_runtime::AgentTerminalStatus::Failed);
        assert_eq!(outcome.failure_cause, Some(FailureCause::Auth));
        assert_eq!(
            witness.saw_blocks.borrow().len(),
            1,
            "auth failures are actionable, never auto-retried"
        );
    }

    #[test]
    fn provider_rejection_is_retried_then_succeeds() {
        // Free-tier endpoints reject already-shaped requests (capacity,
        // per-minute limits surfaced as 4xx): a failed step committed no tool
        // effects, so the rejection is retried under the bounded backoff.
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let backing = ScriptedBacking::new(vec![
            Err(ModelStepError::ProviderFailed {
                cause: FailureCause::Rejected,
            }),
            Ok(ModelStepOutput::Terminal {
                text: "recovered after rejection".to_owned(),
                tokens: 1,
            }),
        ]);
        let witness = backing.clone();
        let outcome = run_live_exec(
            preserved(),
            backing,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.result.summary(), "recovered after rejection");
        assert_eq!(outcome.failure_cause, None);
        assert_eq!(witness.saw_blocks.borrow().len(), 2, "one retry, then success");
    }

    #[test]
    fn empty_terminal_response_is_retried_then_succeeds() {
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let backing = ScriptedBacking::new(vec![
            Ok(ModelStepOutput::Terminal {
                text: String::new(),
                tokens: 1,
            }),
            Ok(ModelStepOutput::Terminal {
                text: String::new(),
                tokens: 1,
            }),
            Ok(ModelStepOutput::Terminal {
                text: "finally non-empty".to_owned(),
                tokens: 1,
            }),
        ]);
        let witness = backing.clone();
        let outcome = run_live_exec(
            preserved(),
            backing,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.result.summary(), "finally non-empty");
        assert_eq!(
            witness.saw_blocks.borrow().len(),
            3,
            "two bounded empty-response retries, then success"
        );
    }

    #[test]
    fn exhausted_empty_responses_stop_typed_and_report_tool_count() {
        // Every empty retry exhausted: the turn fails with the typed
        // empty_response stop and the outcome still reports that no tool
        // calls were executed — the exact inputs of the CLI exit-code
        // decision (work-committed empty finals exit 0; this run is not one).
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let empty = || {
            Ok(ModelStepOutput::Terminal {
                text: String::new(),
                tokens: 1,
            })
        };
        let mut outputs = Vec::new();
        for _ in 0..30 {
            outputs.push(empty());
        }
        let backing = ScriptedBacking::new(outputs);
        let witness = backing.clone();
        let outcome = run_live_exec(
            preserved(),
            backing,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        // `SupervisedModel` owns the only empty-response retry now (1 initial
        // + 2 backoff retries); `agent_runtime::turn::run_model_step` treats
        // the empty response it gets back as terminal and does not retry
        // again, so the backing sees exactly 3 calls, not a compounded 3x3.
        assert_eq!(witness.saw_blocks.borrow().len(), 3);
        assert_eq!(outcome.result.status(), agent_runtime::AgentTerminalStatus::Failed);
        assert_eq!(outcome.stop_reason, Some(TurnStopReason::EmptyResponse));
        assert_eq!(outcome.failure_cause, None);
        assert_eq!(outcome.tool_calls, 0);
    }

    #[test]
    fn connection_failure_is_retried_then_keeps_its_cause() {
        // A failed model step committed no tool effects, so a connection drop
        // is retried with bounded backoff; when retries exhaust, the typed
        // cause is preserved and nothing is fabricated.
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let backing = ScriptedBacking::new(vec![
            Err(ModelStepError::ProviderFailed {
                cause: FailureCause::Connection,
            }),
            Ok(ModelStepOutput::Terminal {
                text: "recovered after reconnect".to_owned(),
                tokens: 1,
            }),
        ]);
        let witness = backing.clone();
        let outcome = run_live_exec(
            preserved(),
            backing,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.result.summary(), "recovered after reconnect");
        assert_eq!(outcome.failure_cause, None);
        assert_eq!(witness.saw_blocks.borrow().len(), 2, "one retry, then success");

        // Exhausted connection retries still surface the typed cause.
        let mut events = Vec::new();
        let mut failures = Vec::new();
        for _ in 0..=(MAX_TRANSIENT_RETRIES as usize) {
            failures.push(Err(ModelStepError::ProviderFailed {
                cause: FailureCause::Connection,
            }));
        }
        let backing = ScriptedBacking::new(failures);
        let witness = backing.clone();
        let outcome = run_live_exec(
            preserved(),
            backing,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.failure_cause, Some(FailureCause::Connection));
        assert_eq!(
            witness.saw_blocks.borrow().len(),
            MAX_TRANSIENT_RETRIES as usize + 1,
            "initial attempt plus the bounded retries"
        );
    }

    #[test]
    fn transient_retry_never_replays_committed_tool_effects() {
        let call = ProposedToolCall::new("c1", "repo_read", "{}").expect("call");
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let mut tools = CountingTools { executed: 0 };
        let backing = ScriptedBacking::new(vec![
            Ok(ModelStepOutput::ToolCalls {
                calls: vec![call],
                tokens: 1,
            }),
            Err(ModelStepError::ProviderFailed {
                cause: FailureCause::Transient { retry_after_ms: None },
            }),
            Ok(ModelStepOutput::Terminal {
                text: "recovered after tools".to_owned(),
                tokens: 2,
            }),
        ]);
        let outcome = run_live_exec(
            preserved(),
            backing,
            &request,
            &mut tools,
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.result.status(), agent_runtime::AgentTerminalStatus::Succeeded);
        assert_eq!(
            tools.executed, 1,
            "the committed tool effect ran exactly once; the retry re-asked the model only"
        );
    }

    #[test]
    fn cancellation_during_backoff_stops_the_turn() {
        // The backing cancels the shared token while failing transiently, so
        // the retry wait must observe cancellation instead of sleeping on.
        struct CancelThenTransient;
        impl LiveModelCall for CancelThenTransient {
            fn step(
                &mut self,
                _blocks: &[ContextBlock],
                _input: &ModelStepInput<'_>,
                cancel: &CancellationToken,
            ) -> Result<ModelStepOutput, ModelStepError> {
                cancel.cancel();
                Err(ModelStepError::ProviderFailed {
                    cause: FailureCause::Transient { retry_after_ms: None },
                })
            }
        }
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let outcome = run_live_exec(
            preserved(),
            CancelThenTransient,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("cancel during backoff is a typed cancelled turn");
        // The backoff observed the cancel and stopped retrying: the turn ends
        // with a typed Cancelled status, no tokens, no synthetic completion.
        assert_eq!(
            outcome.result.status(),
            agent_runtime::AgentTerminalStatus::Cancelled,
            "cancellation must end the turn, not error"
        );
        assert_eq!(outcome.tokens, 0);
    }

    #[test]
    fn diagnostics_capture_host_attempts_outcomes_and_tokens() {
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let mut outputs = vec![Err(ModelStepError::ProviderFailed {
            cause: FailureCause::Transient { retry_after_ms: None },
        })];
        outputs.push(Ok(ModelStepOutput::Terminal {
            text: "done".to_owned(),
            tokens: 7,
        }));
        let backing = ScriptedBacking::new(outputs);
        let (diag, lines) = StepDiag::buffer("https://api.example.com/v1");
        let outcome = run_live_exec(
            preserved(),
            backing,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            Some(diag),
        )
        .expect("execute");
        assert_eq!(outcome.result.summary(), "done");
        let lines = lines.borrow().clone();
        assert_eq!(
            lines.len(),
            3,
            "one line per attempt plus the turn summary: {lines:?}"
        );
        assert!(
            lines[0].contains("host=api.example.com")
                && lines[0].contains("attempt=0")
                && lines[0].contains("outcome=failed:transient")
                && lines[0].contains("tokens=0"),
            "first attempt line: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("attempt=1") && lines[1].contains("outcome=ok tokens=7"),
            "second attempt line: {}",
            lines[1]
        );
        assert!(
            lines[2].contains("turn outcome=succeeded tokens=7"),
            "turn line: {}",
            lines[2]
        );
    }

    #[test]
    fn diagnostics_lines_are_bounded_and_host_labels_never_carry_paths() {
        let (diag, lines) = StepDiag::buffer("https://user:secret@api.example.com/v1/path?q=1");
        for index in 0..(MAX_DIAG_LINES + 10) {
            diag.line(format!("line {index}"));
        }
        let lines = lines.borrow().clone();
        assert_eq!(lines.len(), MAX_DIAG_LINES, "extra lines are dropped, bounded");
        assert_eq!(diag.host(), "api.example.com", "userinfo and path are stripped");
    }
}
