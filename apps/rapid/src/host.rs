//! Context-owning live-recovery host (P5-024 production wiring).
//!
//! This is the composition-root host that owns a reusable live model context and
//! runs the recovery-capable executor. It couples:
//!
//!   - a live [`ContextPacket`] (the single Context-Fabric authority), rebuilt
//!     via `context_engine::compile` on overflow with the conversation folded
//!     into a model-written summary (fail-closed: a rebuild that is not
//!     strictly smaller than the packet the provider refused is
//!     `NotRecoverable`, never a silently identical retry);
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
    ModelStepOutput, ProposedToolCall, ToolDriver, ToolStepError, ToolStepExchange, ToolStepResult,
    TurnAgentExecutor, TurnEventSink, TurnFailureDetail, TurnStopReason, TurnSuspension,
    ValidatedToolCall,
};
use context_engine::compile::{
    CompileContext, CompileError, CompileInput, ContextBlock, ContextPacket, compile,
};
use llm_router::fallback::{
    AttemptProgress, FallbackAction, FallbackController, FallbackPlan, FallbackTrigger,
};
use llm_router::provider::{CancellationToken as RouterCancellationToken, ModelRef, ProviderError};
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

/// Most prior turns a live turn carries into the model's context. Older
/// turns are dropped first, and the compile's memory partition trims
/// further under budget pressure (see `build_packet`). A session's full
/// history stays in the ledger; this is what one turn re-reads of it.
pub const MAX_CONVERSATION_TURNS: usize = 32;

/// One earlier turn of the session as the model sees it on a later turn:
/// what the user asked and how the turn ended. The transcript the user
/// sees is a display projection; this is read from the ledger's own
/// `turn.started`/`turn.*` terminal events, the record of what was said.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationTurn {
    user: String,
    outcome: ConversationOutcome,
}

/// How an earlier turn ended, as the model should understand it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationOutcome {
    /// The assistant's final text, when it produced one.
    Answered(Option<String>),
    Failed(String),
    Interrupted,
}

impl ConversationTurn {
    pub fn new(user: impl Into<String>, outcome: ConversationOutcome) -> Self {
        Self {
            user: user.into(),
            outcome,
        }
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn outcome(&self) -> &ConversationOutcome {
        &self.outcome
    }

    /// The block text: the user's words verbatim, then how the turn ended.
    fn render(&self) -> String {
        let mut text = String::with_capacity(self.user.len() + 64);
        text.push_str("[user]\n");
        text.push_str(&self.user);
        text.push_str("\n[assistant]\n");
        match &self.outcome {
            ConversationOutcome::Answered(Some(answer)) => text.push_str(answer),
            ConversationOutcome::Answered(None) => text.push_str("(finished without a reply)"),
            ConversationOutcome::Failed(reason) => {
                text.push_str("(the turn failed: ");
                text.push_str(reason);
                text.push(')');
            }
            ConversationOutcome::Interrupted => text.push_str("(the turn was interrupted)"),
        }
        text
    }
}

/// A session's earlier conversation as one turn re-reads it: the turns since
/// the last compaction, and the summary that compaction left of everything
/// before them. Both empty for a session that has said nothing yet.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConversationHistory {
    pub summary: Option<String>,
    pub turns: Vec<ConversationTurn>,
    /// The session seq this history was read through: what a compaction of
    /// it covers, and so what its `context.compacted` records.
    pub through_seq: u64,
}

/// Read cap for `.rapidlm/todos.json`, well above the legitimate maximum
/// (`MAX_TODOS` entries at `MAX_TODO_CONTENT_BYTES` each plus JSON
/// overhead) so any realistically-written file always parses; an oversized
/// file is treated as corrupt (`None`), matching `load_todos_index`'s own
/// documented fail-open contract for malformed content.
pub const MAX_TODOS_INDEX_BYTES: usize = 256 * 1024;
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
/// Byte cap for the model-visible stall-warning block (Modbit `AGT-017`):
/// generous for a formatted sentence, tight enough to bound an adversarial
/// tool name.
pub const MAX_STALL_WARNING_BYTES: usize = 1024;

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
    todos_index: Option<String>,
    stall_warning: Option<String>,
    retrieved_context: Vec<CompileInput>,
    conversation: Vec<ConversationTurn>,
    /// What an earlier compaction folded the turns before `conversation`
    /// into — see [`Self::with_compaction_summary`].
    compaction_summary: Option<String>,
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
            todos_index: None,
            stall_warning: None,
            retrieved_context: Vec::new(),
            conversation: Vec::new(),
            compaction_summary: None,
        })
    }

    /// Attach the session's earlier turns, oldest first. Only the newest
    /// [`MAX_CONVERSATION_TURNS`] are kept here; the compile trims further.
    pub fn with_conversation(mut self, turns: Vec<ConversationTurn>) -> Self {
        let mut turns = turns;
        if turns.len() > MAX_CONVERSATION_TURNS {
            turns.drain(..turns.len() - MAX_CONVERSATION_TURNS);
        }
        self.conversation = turns;
        self
    }

    pub fn conversation(&self) -> &[ConversationTurn] {
        &self.conversation
    }

    /// Attach the summary an earlier compaction left of the turns before
    /// [`Self::with_conversation`]'s — `context.compacted`'s own text. It
    /// enters the packet ahead of the turns it stands for, kept in
    /// preference to them under pressure, and switches the system prompt's
    /// post-compaction section on. Empty or oversized text attaches
    /// nothing: the summary is model output, and its bound is the
    /// writer's (`MAX_COMPACTION_SUMMARY`), so exceeding it here means the
    /// ledger carries something this build did not write.
    pub fn with_compaction_summary(mut self, summary: Option<String>) -> Self {
        let within_bounds = summary
            .as_ref()
            .is_none_or(|text| !text.is_empty() && text.len() <= MAX_COMPACTION_SUMMARY);
        if within_bounds {
            self.compaction_summary = summary;
        }
        self
    }

    pub fn compaction_summary(&self) -> Option<&str> {
        self.compaction_summary.as_deref()
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
        let within_bounds = memory
            .as_ref()
            .is_none_or(|text| !text.is_empty() && text.len() <= MAX_MEMORY_INDEX_BYTES);
        if within_bounds {
            self.memory_index = memory;
        }
        self
    }

    pub fn memory_index(&self) -> Option<&str> {
        self.memory_index.as_deref()
    }

    /// Attach the persisted plan/todo projection (`.rapidlm/todos.json`,
    /// written by the `todo_write` tool) so it survives compaction instead
    /// of only living in the transcript (Modbit `AGT-016`). Same bounds
    /// discipline as `with_memory_index`.
    pub fn with_todos_index(mut self, todos: Option<String>) -> Self {
        let within_bounds = todos
            .as_ref()
            .is_none_or(|text| !text.is_empty() && text.len() <= MAX_MEMORY_INDEX_BYTES);
        if within_bounds {
            self.todos_index = todos;
        }
        self
    }

    pub fn todos_index(&self) -> Option<&str> {
        self.todos_index.as_deref()
    }

    /// Attach the current stall-detection warning (Modbit `AGT-017`) so the
    /// model sees its own repeated-call pattern instead of only an operator
    /// seeing it in `--verbose` diagnostics. Set/cleared by
    /// `LiveContextModelDriver::step` on every step as the pattern
    /// appears/resolves — never by a caller directly.
    fn with_stall_warning(mut self, warning: Option<String>) -> Self {
        let within_bounds = warning
            .as_ref()
            .is_none_or(|text| !text.is_empty() && text.len() <= MAX_STALL_WARNING_BYTES);
        if within_bounds {
            self.stall_warning = warning;
        }
        self
    }

    pub fn stall_warning(&self) -> Option<&str> {
        self.stall_warning.as_deref()
    }

    /// Attach the rendered dynamic system prompt for this turn. Empty or
    /// oversized blocks are refused at the caller; None adds no block.
    pub fn with_system_prompt(mut self, system_prompt: Option<String>) -> Self {
        let within_bounds = system_prompt
            .as_ref()
            .is_none_or(|text| !text.is_empty() && text.len() <= MAX_SYSTEM_PROMPT_BLOCK_BYTES);
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
    /// The compaction summary currently baked into `packet`, if any — kept
    /// here (not solely on `LiveRecoveryController`) so any packet rebuild,
    /// not only a compaction rebuild, can preserve it.
    summary: Option<String>,
    /// A summary the model wrote during this turn's overflow recovery, and
    /// how many turns it folded — what the caller records as
    /// `context.compacted`, so the next turn reads the summary instead of
    /// overflowing on the same turns and paying for the same recovery.
    /// `None` when no recovery wrote one (a carried-in summary, or a
    /// fold that dropped the turns unsummarised, is not this).
    recovered: Option<RecoveredSummary>,
}

/// A summary an overflow recovery wrote — see [`LiveContext::recovered`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveredSummary {
    pub text: String,
    /// Turns the summary folded in.
    pub turns: usize,
}

impl LiveContext {
    pub fn packet(&self) -> &ContextPacket {
        &self.packet
    }

    pub fn recovered(&self) -> Option<&RecoveredSummary> {
        self.recovered.as_ref()
    }
    pub fn revision(&self) -> ArtifactId {
        self.revision
    }
    pub fn preserved(&self) -> &PreservedLiveContext {
        &self.preserved
    }
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
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

    /// Attach a live text-delta sink: every provider text delta forwards to
    /// it while the response arrives (delivery goal §2 progressive
    /// streaming). Default: no-op for models/drivers that do not support
    /// live deltas (scripted, unconfigured).
    fn set_delta_sink(&mut self, sink: Option<crate::model::DeltaSink>) {
        let _ = sink;
    }

    /// Attach a shared accumulator for provider-reported token splits
    /// (input / cached-input / output) across this binding's steps. Default:
    /// no-op — scripted and unconfigured backings report no usage detail.
    /// Never affects turn behavior; machine readers (the eval harness, CI
    /// wrappers) use it for cost estimation at published rates.
    fn set_usage_totals(&mut self, totals: Option<crate::model::UsageTotalsHandle>) {
        let _ = totals;
    }
}

/// A [`ModelDriver`] bound to the host-owned live context. Reads the (possibly
/// rebuilt) packet on every step, so overflow recovery changes what it sees.
///
/// The backing is shared with the [`LiveRecoveryController`], which needs
/// the same model to write the summary an overflow recovery folds the
/// conversation into. The two never run at once — the executor calls one
/// or the other — so the `RefCell` is never contended.
pub struct LiveContextModelDriver<B> {
    live: Rc<RefCell<LiveContext>>,
    backing: Rc<RefCell<B>>,
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
        // Model-visible stall surfacing (Modbit `AGT-017`): recompute on
        // every step and, when it changes (a new stall appears or an old one
        // resolves), rebuild the packet so the model sees the pattern
        // itself, not just an operator watching `--verbose`. Best-effort: a
        // rebuild failure here is silently skipped, never fails the turn —
        // this is diagnostic enrichment, not something a turn depends on.
        let warning = detect_stall(input.history());
        if warning.as_deref() != self.live.borrow().preserved().stall_warning() {
            let mut live = self.live.borrow_mut();
            let preserved = live.preserved.clone().with_stall_warning(warning);
            if let Ok(packet) = build_packet(&preserved, live.summary()) {
                live.packet = packet;
                live.preserved = preserved;
            }
        }
        let live = self.live.borrow();
        self.backing
            .borrow_mut()
            .step(live.packet().blocks(), input, cancel)
    }
}

/// Rebuilds the live context via the Context Fabric on a typed overflow.
///
/// The provider has refused the packet, so the compiler's own estimate —
/// which admitted it — is what was wrong, and the only recovery that means
/// anything is a packet that is genuinely smaller. A recovery folds what
/// the packet still carries, largest first: with turns present, they and
/// the earlier summary become one model-written summary and the retrieved
/// repo context goes with them (the model can re-read files with its
/// tools; the retry bound is short, so one fold should be enough); with
/// no turns, the retrieved context alone goes; with only a summary left, it
/// goes. Every candidate is checked against the packet it replaces, as
/// compiled — a model-written summary that leaves the packet no smaller is
/// set aside for the next smaller candidate (the summary the packet already
/// carried, then none), and a packet with nothing left to fold is
/// `NotRecoverable` rather than retried as it was. The executor's retry
/// bound caps the sequence.
pub struct LiveRecoveryController<B> {
    live: Rc<RefCell<LiveContext>>,
    backing: Rc<RefCell<B>>,
    /// The turn's own token, set by [`LiveContextHost::execute`]: the
    /// recovery's model call must stop when the turn does.
    cancel: CancellationToken,
}

/// What one overflow recovery does with the packet's optional content —
/// one value, decided from what the packet still carries.
enum RecoveryFold {
    /// The turns (and any earlier summary) become a model-written summary,
    /// and the retrieved context goes; if the model cannot write one, the
    /// earlier summary is kept and the turns are dropped unsummarised.
    Conversation,
    /// No turns to fold: the retrieved context goes, the summary stays.
    Retrieved,
    /// Only the summary is left: it goes too.
    Summary,
    /// Nothing optional is in the packet.
    Nothing,
}

impl<B: LiveModelCall> ContextController for LiveRecoveryController<B> {
    fn recover_from_overflow(&mut self, _request: ContextOverflow) -> ContextRecoveryDecision {
        if self.cancel.is_cancelled() {
            return ContextRecoveryDecision::Cancelled;
        }
        let (fold, before, prior, turns, context_limit, output_reserve) = {
            let live = self.live.borrow();
            let preserved = live.preserved();
            let prior = live
                .summary()
                .or(preserved.compaction_summary())
                .map(str::to_owned);
            let fold = if !preserved.conversation().is_empty() {
                RecoveryFold::Conversation
            } else if !preserved.retrieved_context().is_empty() {
                RecoveryFold::Retrieved
            } else if prior.is_some() {
                RecoveryFold::Summary
            } else {
                RecoveryFold::Nothing
            };
            (
                fold,
                live.packet().partitions().included_tokens(),
                prior,
                preserved.conversation().to_vec(),
                preserved.context_limit,
                preserved.output_reserve,
            )
        };
        // The summaries to try, in order; the first whose packet is
        // smaller wins. A model-written summary first, then the one the
        // packet already carried, then none.
        let mut written: Option<RecoveredSummary> = None;
        let candidates: Vec<Option<String>> = match fold {
            RecoveryFold::Nothing => return ContextRecoveryDecision::NotRecoverable,
            RecoveryFold::Conversation => {
                let summarised = summarize_conversation(
                    &mut *self.backing.borrow_mut(),
                    prior.as_deref(),
                    &turns,
                    context_limit,
                    output_reserve,
                    &self.cancel,
                );
                match summarised {
                    Ok(summary) => {
                        written = Some(RecoveredSummary {
                            text: summary.text.clone(),
                            turns: summary.turns,
                        });
                        vec![Some(summary.text), prior, None]
                    }
                    Err(CompactionError::Model(ModelStepError::Cancelled)) => {
                        return ContextRecoveryDecision::Cancelled;
                    }
                    // The model could not write the summary: the turns
                    // are dropped unsummarised and whatever summary the
                    // packet already carried stays — a smaller packet with
                    // less history, the same trade the memory partition
                    // makes under pressure, rather than a failed turn.
                    Err(_) => vec![prior, None],
                }
            }
            RecoveryFold::Retrieved => vec![prior, None],
            RecoveryFold::Summary => vec![None],
        };
        // Everything optional goes from the preserved state; the summary
        // chosen below is the one thing carried forward, through
        // `live.summary` alone.
        let mut folded = self.live.borrow().preserved().clone();
        folded.conversation.clear();
        folded.retrieved_context.clear();
        folded.compaction_summary = None;
        // Fail closed on the rebuilt packet itself, not an estimate of it:
        // a retry of something no smaller than what the provider refused
        // is the silent no-op this exists to replace.
        let mut tried: Vec<Option<String>> = Vec::new();
        for candidate in candidates {
            if tried.contains(&candidate) {
                continue;
            }
            let packet = match build_packet(&folded, candidate.as_deref()) {
                Ok(packet) => packet,
                Err(_) => return ContextRecoveryDecision::NotRecoverable,
            };
            if packet.partitions().included_tokens() < before {
                let mut live = self.live.borrow_mut();
                // A written summary is recorded only while it is what the
                // packet carries — this fold's, or an earlier fold's that
                // this one kept as `prior`.
                let previous = live.recovered.take();
                live.recovered = written
                    .or(previous)
                    .filter(|recovered| Some(&recovered.text) == candidate.as_ref());
                live.packet = packet;
                live.revision = ArtifactId::from_bytes(b"context/live-recovery");
                live.preserved = folded;
                live.summary = candidate;
                return ContextRecoveryDecision::Recovered;
            }
            tried.push(candidate);
        }
        ContextRecoveryDecision::NotRecoverable
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
    controller: LiveRecoveryController<B>,
    policy: ContextRetryPolicy,
}

impl<B: LiveModelCall> LiveContextHost<B> {
    /// Compiled-context usage of the packet the host currently holds:
    /// `(included_tokens, context_limit)`. See [`ExecOutcome::context_tokens`]
    /// on why callers read this *after* execution.
    pub fn context_usage(&self) -> Option<(u32, u32)> {
        let live = self.live.try_borrow().ok()?;
        let partitions = live.packet().partitions();
        Some((partitions.included_tokens(), partitions.context_limit()))
    }

    /// Per-class usage of the packet the host currently holds. Same timing
    /// rule as [`Self::context_usage`].
    pub fn context_partitions(&self) -> Vec<(&'static str, u32, u32)> {
        let Ok(live) = self.live.try_borrow() else {
            return Vec::new();
        };
        let p = live.packet().partitions();
        [
            ("system", p.system()),
            ("user", p.user()),
            ("goal", p.goal()),
            ("diff", p.diff()),
            ("retrieved", p.retrieved()),
            ("memory", p.memory()),
            ("read_set", p.read_set()),
        ]
        .into_iter()
        .map(|(name, budget)| (name, budget.used(), budget.cap()))
        .collect()
    }

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
            summary: None,
            recovered: None,
        }));
        let backing = Rc::new(RefCell::new(backing));
        let model = LiveContextModelDriver {
            live: Rc::clone(&live),
            backing: Rc::clone(&backing),
        };
        let controller = LiveRecoveryController {
            live: Rc::clone(&live),
            backing,
            cancel: CancellationToken::new(),
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
        self.controller.cancel = cancel.clone();
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

    /// [`Self::execute`] continued from prior completed exchanges — the
    /// resume path for a turn that suspended on an approval or a
    /// clarification. `seed_history` is the suspension's recorded history
    /// with the pending call's placeholder result already replaced by its
    /// post-resolution outcome.
    pub fn execute_seeded<T, E>(
        &mut self,
        request: &AgentExecutionRequest,
        tools: &mut T,
        events: &mut E,
        cancel: &CancellationToken,
        seed_history: Vec<ToolStepExchange>,
    ) -> Result<AgentOutcome, AgentExecutionError>
    where
        T: ToolDriver,
        E: TurnEventSink,
    {
        self.controller.cancel = cancel.clone();
        TurnAgentExecutor.recovering_with_seed(
            request,
            &mut self.controller,
            self.policy,
            &mut self.model,
            tools,
            events,
            cancel,
            seed_history,
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
        FailureCause::Quota => "quota",
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
    cost: CostAccumulator,
    diag: Option<StepDiag>,
}

/// Sums every step's `cost_usd_micros`, distinguishing "no step ever
/// reported a real cost" (`total()` returns `None`, same as
/// `ModelStepOutput`'s own field) from "the reported total happens to be
/// zero." A plain `Arc<AtomicU64>` alone can't make that distinction, and
/// `None` here must never silently read back as `Some(0)`.
#[derive(Clone)]
struct CostAccumulator {
    sum_usd_micros: std::sync::Arc<std::sync::atomic::AtomicU64>,
    any_reported: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl CostAccumulator {
    fn new() -> Self {
        Self {
            sum_usd_micros: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            any_reported: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn add(&self, cost_usd_micros: Option<u64>) {
        if let Some(cost) = cost_usd_micros {
            self.sum_usd_micros
                .fetch_add(cost, std::sync::atomic::Ordering::Relaxed);
            self.any_reported
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn total(&self) -> Option<u64> {
        if self.any_reported.load(std::sync::atomic::Ordering::Relaxed) {
            Some(
                self.sum_usd_micros
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
        } else {
            None
        }
    }
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
    let mut counts: std::collections::BTreeMap<(&str, &str), usize> =
        std::collections::BTreeMap::new();
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
                Ok(ModelStepOutput::ToolCalls {
                    tokens,
                    cost_usd_micros,
                    ..
                }) => {
                    let tokens = *tokens;
                    self.counter
                        .fetch_add(tokens, std::sync::atomic::Ordering::Relaxed);
                    self.cost.add(*cost_usd_micros);
                    self.diag_attempt(attempt, "ok", tokens);
                    return result;
                }
                Ok(ModelStepOutput::Terminal {
                    text,
                    tokens,
                    cost_usd_micros,
                }) => {
                    let tokens = *tokens;
                    self.counter
                        .fetch_add(tokens, std::sync::atomic::Ordering::Relaxed);
                    self.cost.add(*cost_usd_micros);
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

/// One routing decision made mid-turn by a [`FallbackChainModel`] (Modbit
/// `MOD-005`: "routing must be auditable" — a real, typed, queryable record
/// rather than only an ephemeral `--verbose` diagnostic line). The common
/// case — a turn that never needed to retry or fall back — produces zero
/// records, not a record saying so; absence *is* the "used the requested
/// model with no incident" signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouterDecisionRecord {
    pub requested_model: String,
    pub resolved_model: String,
    pub reason: RouterDecisionReason,
    /// Cumulative cost already incurred on `requested_model` this turn, as
    /// of this decision — `None` when no attempt on it ever reported a real
    /// cost, matching `CostAccumulator::total()`'s own "unknown is not
    /// confirmed zero" discipline. This is the achievable half of Modbit
    /// `MOD-005`'s "estimated vs. actual cost" ask: the *actual* cost spent
    /// on the model being retried/abandoned, not an *estimate* of the next
    /// attempt's cost — that half needs a real pricing/catalog lookup this
    /// record doesn't have access to, and is deliberately not attempted
    /// here (see `newtask.md` §2.8).
    pub spent_usd_micros: Option<u64>,
    /// `managed_config::ManagedPolicy::policy_version` of whatever managed
    /// policy was active when this decision was made, or `None` when no
    /// managed policy is configured at all — the remaining half of Modbit
    /// `MOD-005`'s "policy version" ask, closed 2026-09-05. A content
    /// identity, not a semantic version; see that field's own doc comment.
    /// Best-effort, not transactional: `interactive.rs`'s real caller reads
    /// the policy file once for this turn and reuses that single read for
    /// both this field and its own disk/network-ceiling narrowing, but that
    /// read is still independent of the earlier, security-relevant read
    /// `exec_permission_lattice` uses to actually gate permission mode — if
    /// the file changes between those two reads, this field can describe a
    /// policy that wasn't the one actually enforced for the turn. Narrowing
    /// that further would mean threading a value across a function boundary
    /// that already deliberately re-loads rather than threads (see that
    /// site's own doc comment); not attempted here.
    pub policy_version: Option<String>,
}

/// Why a step's resolved model differs from (or repeats) the one requested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouterDecisionReason {
    /// Same model, retried after a transient failure.
    RetrySame,
    /// Switched to a different model in the configured chain.
    FallbackTo,
    /// The chain gave up; `resolved_model` is unset (`stop_reason` names why).
    Stop(String),
}

/// Shared, append-only log of routing decisions for one turn. `Arc<Mutex<_>>`
/// mirrors `CostAccumulator`'s shape: a handle is cloned out to the caller
/// before the model is moved into `SelectedModel`/`run_live_exec`, then read
/// back after the turn resolves.
#[derive(Clone, Default)]
pub struct RouterDecisionLog(std::sync::Arc<std::sync::Mutex<Vec<RouterDecisionRecord>>>);

impl RouterDecisionLog {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&self, record: RouterDecisionRecord) {
        if let Ok(mut log) = self.0.lock() {
            log.push(record);
        }
    }

    /// Every decision recorded so far, in order.
    pub fn snapshot(&self) -> Vec<RouterDecisionRecord> {
        self.0.lock().map(|log| log.clone()).unwrap_or_default()
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
    decisions: RouterDecisionLog,
    /// Cumulative real cost reported so far this turn, per backend
    /// (`model_label`-keyed) — populated from every successful step's own
    /// `cost_usd_micros`, regardless of which model produced it. Read back
    /// when a decision is recorded so `RouterDecisionRecord::spent_usd_micros`
    /// reflects what was actually spent on the model being retried/
    /// abandoned, not a running turn-wide total.
    spent_usd_micros: std::collections::BTreeMap<String, u64>,
    /// Set once via `set_policy_version` (not a `new()` parameter, to avoid
    /// touching every existing test call site for a field most callers
    /// leave `None`) and copied onto every `RouterDecisionRecord` pushed
    /// from then on.
    policy_version: Option<String>,
}

impl<B: LiveModelCall> FallbackChainModel<B> {
    /// `backends` must include an entry for `controller.current()` and every
    /// model `controller.chain()` can ever name — the composition root
    /// builds both from the same resolved `[models] fallback` list, so this
    /// invariant holds by construction.
    pub fn new(
        backends: Vec<(ModelRef, B)>,
        controller: FallbackController,
        diag: Option<StepDiag>,
    ) -> Self {
        Self {
            backends,
            controller,
            diag,
            decisions: RouterDecisionLog::new(),
            spent_usd_micros: std::collections::BTreeMap::new(),
            policy_version: None,
        }
    }

    /// Attach the active managed policy's content version, if any, to every
    /// `RouterDecisionRecord` this chain records from this point on. Not a
    /// `new()` parameter — see the field's own doc comment for why.
    pub fn set_policy_version(&mut self, version: Option<String>) {
        self.policy_version = version;
    }

    /// Cumulative cost reported so far this turn for `model`, or `None` if
    /// no attempt on it has reported a real cost yet.
    fn spent_on(&self, model: &ModelRef) -> Option<u64> {
        self.spent_usd_micros.get(&model_label(model)).copied()
    }

    /// Shared handle to this chain's routing-decision log, readable after
    /// the turn ends regardless of how many times this model is subsequently
    /// borrowed — clone it out before moving `self` into `SelectedModel`.
    pub fn decisions(&self) -> RouterDecisionLog {
        self.decisions.clone()
    }

    /// Every backend this chain could dispatch to, in configured order —
    /// read-only access for a caller that needs to reason about the whole
    /// candidate set (e.g. deriving a context budget safe no matter which
    /// one ends up serving the request) without exposing mutation or the
    /// routing controller itself.
    pub fn backends(&self) -> impl Iterator<Item = &B> {
        self.backends.iter().map(|(_, backend)| backend)
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
                    let cost = match &output {
                        ModelStepOutput::Terminal {
                            cost_usd_micros, ..
                        }
                        | ModelStepOutput::ToolCalls {
                            cost_usd_micros, ..
                        } => *cost_usd_micros,
                    };
                    if let Some(cost) = cost {
                        *self
                            .spent_usd_micros
                            .entry(model_label(&current))
                            .or_insert(0) += cost;
                    }
                    self.diag_line(format!(
                        "fallback model={} outcome=ok",
                        model_label(&current)
                    ));
                    return Ok(output);
                }
                Err(ModelStepError::Cancelled) => return Err(ModelStepError::Cancelled),
                Err(err) => err,
            };
            let trigger = to_fallback_trigger(&err);
            let router_cancel = RouterCancellationToken::new();
            let plan =
                match self
                    .controller
                    .plan(&trigger, AttemptProgress::PreResponse, &router_cancel)
                {
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
                FallbackPlan::PreResponse {
                    action: FallbackAction::RetrySame { backoff_ms, .. },
                    ..
                } => {
                    self.diag_line(format!(
                        "fallback model={} outcome=retry backoff_ms={backoff_ms}",
                        model_label(&current)
                    ));
                    self.decisions.push(RouterDecisionRecord {
                        requested_model: model_label(&current),
                        resolved_model: model_label(&current),
                        reason: RouterDecisionReason::RetrySame,
                        spent_usd_micros: self.spent_on(&current),
                        policy_version: self.policy_version.clone(),
                    });
                    if !sleep_millis_cancellable(cancel, *backoff_ms) {
                        return Err(ModelStepError::Cancelled);
                    }
                }
                FallbackPlan::PreResponse {
                    action: FallbackAction::FallbackTo { to, backoff_ms, .. },
                    ..
                } => {
                    self.diag_line(format!(
                        "fallback model={} -> {} backoff_ms={backoff_ms}",
                        model_label(&current),
                        model_label(to)
                    ));
                    self.decisions.push(RouterDecisionRecord {
                        requested_model: model_label(&current),
                        resolved_model: model_label(to),
                        reason: RouterDecisionReason::FallbackTo,
                        spent_usd_micros: self.spent_on(&current),
                        policy_version: self.policy_version.clone(),
                    });
                    if !sleep_millis_cancellable(cancel, *backoff_ms) {
                        return Err(ModelStepError::Cancelled);
                    }
                }
                FallbackPlan::PreResponse {
                    action: FallbackAction::Stop { reason, .. },
                    ..
                } => {
                    self.diag_line(format!(
                        "fallback model={} outcome=stop reason={}",
                        model_label(&current),
                        reason.as_str()
                    ));
                    self.decisions.push(RouterDecisionRecord {
                        requested_model: model_label(&current),
                        resolved_model: String::new(),
                        reason: RouterDecisionReason::Stop(reason.as_str().to_owned()),
                        spent_usd_micros: self.spent_on(&current),
                        policy_version: self.policy_version.clone(),
                    });
                    return Err(err);
                }
                FallbackPlan::PartiallyStreamed { reason, .. }
                | FallbackPlan::ToolSideEffect { reason, .. } => {
                    self.diag_line(format!(
                        "fallback model={} outcome=stop reason={}",
                        model_label(&current),
                        reason.as_str()
                    ));
                    self.decisions.push(RouterDecisionRecord {
                        requested_model: model_label(&current),
                        resolved_model: String::new(),
                        reason: RouterDecisionReason::Stop(reason.as_str().to_owned()),
                        spent_usd_micros: self.spent_on(&current),
                        policy_version: self.policy_version.clone(),
                    });
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
            FailureCause::Quota => ProviderError::QuotaExceeded,
            FailureCause::Connection => ProviderError::Connection,
            FailureCause::Rejected => ProviderError::InvalidRequest,
            FailureCause::Transient {
                retry_after_ms: Some(after),
            } => ProviderError::RateLimited {
                retry_after_ms: Some(*after),
            },
            FailureCause::Transient {
                retry_after_ms: None,
            } => ProviderError::Transient,
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
/// typed stop reason and tool-call count (for exit-code decisions), the
/// provider-reported token total, and the provider-reported dollar cost
/// (summed in micro-USD across every model step; `None` when no step ever
/// reported one — see `CostAccumulator`, never conflated with a real zero).
#[derive(Clone, Debug)]
pub struct ExecOutcome {
    pub result: AgentResult,
    pub failure_cause: Option<FailureCause>,
    pub failure_detail: Option<TurnFailureDetail>,
    pub stop_reason: Option<TurnStopReason>,
    pub tool_calls: u32,
    pub tokens: u64,
    pub cost_usd_micros: Option<u64>,
    /// Per-class compiled-context usage as this turn *ended*: `(class,
    /// used, cap)` for each hard partition, in the order the `/context`
    /// panel shows them. Empty when nothing reported one.
    ///
    /// Carried beside `context_tokens` rather than derived from it because
    /// the totals cannot answer the question a reader actually has — which
    /// class is consuming the window — and the compiler already computed
    /// every class's own used/cap.
    pub context_partitions: Vec<(&'static str, u32, u32)>,
    /// A summary the model wrote during this turn's overflow recovery, when
    /// one did and the retried packet carried it — see
    /// [`LiveContext::recovered`]. The caller records it (`context.compacted`)
    /// so the next turn reads the summary rather than overflowing on the
    /// same turns again.
    pub recovered: Option<RecoveredSummary>,
    /// Compiled-context usage as this turn *ended*: tokens included in the
    /// packet the model was last given, against the hard context limit.
    ///
    /// Read after execution rather than at build time on purpose — the
    /// packet is rebuilt by compaction and overflow recovery
    /// (`LiveRecoveryController`), so the figure taken before the turn ran
    /// would describe a context the model may no longer have been using. The
    /// status line's `ctx:` item and any context panel want the one that
    /// actually applied.
    pub context_tokens: Option<(u32, u32)>,
    /// Pending human input when the turn suspended on an approval or a
    /// clarification. The host records it durably and finishes the turn as
    /// `TurnOutcome::Waiting`; the resume path replays it. `None` for every
    /// other stop.
    pub suspension: Option<TurnSuspension>,
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
    let cost = CostAccumulator::new();
    let turn_diag = diag.clone();
    let supervised = SupervisedModel {
        inner: backing,
        counter: std::sync::Arc::clone(&counter),
        cost: cost.clone(),
        diag,
    };
    let mut host = LiveContextHost::build(preserved, supervised, policy)
        .map_err(|_| AgentExecutionError::InvalidRequest)?;
    let outcome = host.execute(request, tools, events, cancel)?;
    let context_tokens = host.context_usage();
    let context_partitions = host.context_partitions();
    let recovered = host
        .live_context()
        .try_borrow()
        .ok()
        .and_then(|live| live.recovered().cloned());
    let tokens = counter.load(std::sync::atomic::Ordering::Relaxed);
    let cost_usd_micros = cost.total();
    if let Some(diag) = turn_diag {
        let mut line = format!(
            "turn outcome={} tokens={tokens}",
            outcome.result.status().as_str()
        );
        if let Some(cost_usd_micros) = cost_usd_micros {
            line.push_str(&format!(" cost_usd_micros={cost_usd_micros}"));
        }
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
        cost_usd_micros,
        recovered,
        context_tokens,
        context_partitions,
        suspension: outcome.suspension,
    })
}

/// [`run_live_exec`] continued from prior completed exchanges — the resume
/// path for a turn that suspended on an approval or a clarification. The
/// packet, model and tools are rebuilt exactly as a fresh turn; only the
/// tool-exchange history is the suspension's own record.
#[allow(clippy::too_many_arguments)]
pub fn run_live_exec_seeded<B, T, E>(
    preserved: PreservedLiveContext,
    backing: B,
    request: &AgentExecutionRequest,
    tools: &mut T,
    events: &mut E,
    cancel: &CancellationToken,
    policy: ContextRetryPolicy,
    diag: Option<StepDiag>,
    seed_history: Vec<ToolStepExchange>,
) -> Result<ExecOutcome, AgentExecutionError>
where
    B: LiveModelCall,
    T: ToolDriver,
    E: TurnEventSink,
{
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let cost = CostAccumulator::new();
    let _turn_diag = diag.clone();
    let supervised = SupervisedModel {
        inner: backing,
        counter: std::sync::Arc::clone(&counter),
        cost: cost.clone(),
        diag,
    };
    let mut host = LiveContextHost::build(preserved, supervised, policy)
        .map_err(|_| AgentExecutionError::InvalidRequest)?;
    let outcome = host.execute_seeded(request, tools, events, cancel, seed_history)?;
    let context_tokens = host.context_usage();
    let context_partitions = host.context_partitions();
    let recovered = host
        .live_context()
        .try_borrow()
        .ok()
        .and_then(|live| live.recovered().cloned());
    let tokens = counter.load(std::sync::atomic::Ordering::Relaxed);
    Ok(ExecOutcome {
        result: outcome.result,
        failure_cause: outcome.failure_cause,
        failure_detail: outcome.failure_detail,
        stop_reason: outcome.stop_reason,
        tool_calls: outcome.tool_calls,
        tokens,
        cost_usd_micros: cost.total(),
        recovered,
        context_tokens,
        context_partitions,
        suspension: outcome.suspension,
    })
}

/// Reads `.rapidlm/MEMORY.md` through a `MAX_MEMORY_INDEX_BYTES` cap on the
/// read itself, rather than trusting the file's size on disk — the same
/// stat-then-read gap `read_file_bounded` closes elsewhere in this binary.
/// `.rapidlm/MEMORY.md` is git-committed and team-shared, so it arrives via
/// `git clone`, not a bounded write this binary controls. Unlike
/// `read_file_bounded`, an oversized file must still yield the truncated
/// content `load_memory_index`'s own doc comment promises, not `None` — so
/// this caps the read directly rather than treating "too large" as a
/// rejection. `from_utf8_lossy` tolerates a multi-byte character split by
/// the cap; this is advisory display content, not something that must
/// round-trip exactly. Split out from `load_memory_index` so the raw,
/// pre-line-truncation byte cap is directly unit-testable.
fn read_memory_index_bounded(root: &Path) -> Option<String> {
    use std::io::Read as _;
    let path = root.join(".rapidlm").join("MEMORY.md");
    let mut buf = Vec::new();
    fs::File::open(&path)
        .ok()?
        .take(MAX_MEMORY_INDEX_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Load the project memory index (`.rapidlm/MEMORY.md`): a bounded,
/// always-loaded pointer file the model can rely on (Claude MEMORY.md
/// parity). Missing file → None; oversized content is truncated to the line
/// and byte bounds rather than dropped entirely.
pub fn load_memory_index(root: &Path) -> Option<String> {
    let text = read_memory_index_bounded(root)?;
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

/// Load the persisted plan/todo state (`.rapidlm/todos.json`, written by the
/// `todo_write` tool) as a readable projection for `with_todos_index`
/// (Modbit `AGT-016`: plan state as durable state outside the transcript,
/// not prompt-only — this is the model-visible half; `scheduler`'s
/// `NodeState` graph is the separate structured-state half, not wired to
/// this pass — see `newtask.md` §2.5). Missing/corrupt file, or a file with
/// no entries → None, matching `load_memory_index`'s fail-open shape:
/// persisted plan state is advisory context, not something a turn should
/// fail to start over. Malformed individual entries are skipped, not fatal
/// to the whole projection.
pub fn load_todos_index(root: &Path) -> Option<String> {
    // Bound the read rather than trusting the file's size on disk (same
    // rationale as `load_memory_index` above) — an oversized file is
    // treated the same as any other unparseable content by the `?` chain
    // below, matching this function's own "corrupt file → None" contract.
    let bytes = crate::exec_tools::read_file_bounded(
        &root.join(crate::exec_tools::TODOS_PATH),
        MAX_TODOS_INDEX_BYTES,
    )
    .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let entries = value.get("todos")?.as_array()?;
    // `owner`/`depends_on` are rendered when present (Modbit `AGT-016`/
    // `AGT-017`: durable plan-node state, and the known blocker, surfaced to
    // the model rather than left implicit) — a dependency that isn't yet
    // "completed" is labeled "blocked by" instead of "depends on", giving
    // the model the same "known state plus blocker" signal `detect_stall`
    // already surfaces for a different failure shape. `evidence_ids` is
    // deliberately not rendered here: it's an audit trail the model already
    // knows the content of (it cited it when writing the todo), not new
    // information worth spending context budget on every turn.
    let lines: Vec<String> = entries
        .iter()
        .filter_map(|entry| {
            let content = entry.get("content")?.as_str()?;
            let status = entry.get("status")?.as_str()?;
            let mut line = format!("- [{status}] {content}");
            if let Some(owner) = entry.get("owner").and_then(serde_json::Value::as_str) {
                line.push_str(&format!(" (owner: {owner})"));
            }
            let deps: Vec<&str> = entry
                .get("depends_on")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .collect();
            if !deps.is_empty() {
                let blocked = deps.iter().any(|dep| {
                    entries.iter().any(|other| {
                        other.get("id").and_then(serde_json::Value::as_str) == Some(*dep)
                            && other.get("status").and_then(serde_json::Value::as_str)
                                != Some("completed")
                    })
                });
                let label = if blocked { "blocked by" } else { "depends on" };
                line.push_str(&format!(" ({label}: {})", deps.join(", ")));
            }
            Some(line)
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n"))
}

/// Locator prefix of the conversation blocks `build_packet` emits, one per
/// earlier turn: `conversation/turn-<n>`, `n` counting from the oldest kept.
pub const CONVERSATION_LOCATOR_PREFIX: &str = "conversation/turn-";

/// Locator of the compaction-summary block: the one memory block that
/// stands for every turn older than the `conversation/` blocks.
pub const COMPACTION_LOCATOR: &str = "context/compaction";

/// Locator of the short system block that tells the model its history is a
/// summary — present exactly when [`COMPACTION_LOCATOR`]'s block is.
pub const POST_COMPACTION_LOCATOR: &str = "system/post-compaction";

/// The summary block's score: above the compiler's default for a memory
/// block (300, what a conversation turn gets), so under pressure the
/// summary outlives any one turn and, within the partition, reads before
/// them. A score of 0 would mean "use the default", not "lowest".
const COMPACTION_SUMMARY_SCORE: u32 = 400;

/// Compile a live [`ContextPacket`] from preserved state + optional compaction
/// summary. This is the single Context-Fabric rebuild path.
///
/// `summary` is the summary an in-turn overflow recovery produced, and it
/// supersedes the one the preserved context carried in from the ledger
/// (the recovery summarised that one along with the turns): `None` here
/// means "whatever was carried", not "none".
///
/// The session's earlier turns go in as one memory block each, in order.
/// Memory is an optional partition, so under pressure the compiler drops
/// blocks — by its own ranking, which for equal-scored blocks is not
/// "oldest first". A conversation with a hole in the middle is worse than a
/// shorter one, so when any conversation block was dropped the oldest turn
/// is removed and the packet compiled again, until every turn that is
/// carried fits. Bounded by the turn count; each compile is local and
/// fast.
pub fn build_packet(
    preserved: &PreservedLiveContext,
    summary: Option<&str>,
) -> Result<ContextPacket, CompileError> {
    let mut skip_oldest = 0;
    loop {
        let packet = build_packet_with(preserved, summary, skip_oldest)?;
        let carried = preserved.conversation.len().saturating_sub(skip_oldest);
        let dropped_turn = packet
            .dropped()
            .iter()
            .any(|block| block.locator().starts_with(CONVERSATION_LOCATOR_PREFIX));
        if !dropped_turn || carried == 0 {
            return Ok(packet);
        }
        skip_oldest += 1;
    }
}

fn build_packet_with(
    preserved: &PreservedLiveContext,
    summary: Option<&str>,
    skip_oldest: usize,
) -> Result<ContextPacket, CompileError> {
    let mut ctx = CompileContext::new(preserved.context_limit, preserved.output_reserve)
        .task("live agent turn")
        .goal(preserved.goal_statement.clone());
    if let Some(system_prompt) = preserved.system_prompt() {
        ctx = ctx.system(CompileInput::new("system/prompt", system_prompt.to_owned()));
    }
    if let Some(memory) = preserved.memory_index() {
        ctx = ctx.system(CompileInput::new("memory/index", memory.to_owned()));
    }
    if let Some(todos) = preserved.todos_index() {
        ctx = ctx.system(CompileInput::new("plan/todos", todos.to_owned()));
    }
    if let Some(warning) = preserved.stall_warning() {
        ctx = ctx.system(CompileInput::new(
            "diagnostics/stall",
            format!("Stall detected: {warning}"),
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
        .or(preserved.compaction_summary())
        .filter(|text| !text.is_empty())
    {
        // Ahead of the turns it stands for (the compiler orders equal-
        // source blocks by score, then input order) and kept in preference
        // to them under pressure: one summary of the older history is
        // worth more than any one of the newer turns.
        ctx = ctx.memory(
            CompileInput::new(COMPACTION_LOCATOR, summary.to_owned())
                .score(COMPACTION_SUMMARY_SCORE),
        );
        ctx = ctx.system(CompileInput::new(
            POST_COMPACTION_LOCATOR,
            format!(
                "## After compaction\n{}",
                agent_runtime::POST_COMPACTION_SYSTEM_PROMPT
            ),
        ));
    }
    if let Some(reminders) = preserved.reminders_block() {
        ctx = ctx.system(CompileInput::new("reminders/active", reminders.to_owned()));
    }
    for (index, turn) in preserved.conversation.iter().enumerate().skip(skip_oldest) {
        ctx = ctx.memory(CompileInput::new(
            format!("{CONVERSATION_LOCATOR_PREFIX}{index}"),
            turn.render(),
        ));
    }
    for block in preserved.retrieved_context() {
        ctx = ctx.retrieved(block.clone());
    }
    compile(&ctx)
}

/// What the compaction model call is told. The history follows as the
/// request's remaining messages; the reply is the summary and nothing else.
const COMPACTION_INSTRUCTIONS: &str = "\
You are compacting the history of a coding-agent session so the agent can \
continue with a summary in place of the transcript. After this instruction \
come the messages that make up that history: an earlier summary, if one \
exists, then the turns since it, each as what the user asked and how the \
turn ended.
Write one summary that preserves, in this order: what the user wants overall \
and every constraint or preference they stated; what was done, naming the \
files, commands and results that matter; decisions and their reasons; what is \
unresolved, including questions the user has not answered; and what the next \
step was going to be. Keep identifiers (paths, names, error messages, \
numbers) exact. Do not invent, do not editorialise, do not address the user. \
Reply with the summary only, as plain text, in under 600 words.";

/// The user-role request the history is attached to.
const COMPACTION_TASK: &str =
    "Summarise the session history that follows, as instructed. Reply with the summary only.";

/// What one compaction produced and cost.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionSummary {
    pub text: String,
    /// Turns the summary folded in (not counting the earlier summary).
    pub turns: usize,
    pub tokens: u64,
    pub cost_usd_micros: Option<u64>,
}

/// Why a compaction produced no summary. Display never echoes history text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionError {
    /// No turns and no earlier summary: there is nothing to fold.
    NothingToCompact,
    /// Not even the newest turn fits the model's window beside the
    /// instructions.
    Compile(CompileError),
    /// The model step failed; the step's own typed error.
    Model(ModelStepError),
    /// The model answered with no text, or with tool calls where none were
    /// offered.
    NoSummary,
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NothingToCompact => f.write_str("nothing to compact"),
            Self::Compile(err) => write!(f, "history does not fit the model's window ({err})"),
            Self::Model(ModelStepError::Cancelled) => f.write_str("cancelled"),
            Self::Model(ModelStepError::ProviderFailed { cause }) => {
                write!(f, "the model call failed ({})", cause_tag(*cause))
            }
            Self::Model(err) => write!(f, "the model call failed ({err})"),
            Self::NoSummary => f.write_str("the model returned no summary"),
        }
    }
}

impl std::error::Error for CompactionError {}

/// Most turns one compaction request carries. Well under the compiler's
/// block capacity (256) with the instructions, the task and an earlier
/// summary beside them; a session longer than this folds its newest turns
/// and the summary it already carries, which is all a turn ever saw of it.
pub const MAX_COMPACTION_TURNS: usize = 192;

/// How many times the request is re-sent with the oldest half of its turns
/// left out when the provider refuses it as too large — the provider's own
/// count disagreeing with the compiler's estimate, the same way a turn's
/// packet can be refused.
const MAX_COMPACTION_SHRINKS: usize = 3;

/// Fold `turns` (and the earlier summary they follow, if any) into one
/// summary with a single model call — the one summarizer, run by `/compact`
/// and by the in-turn overflow recovery alike.
///
/// The request is compiled through the same compiler a turn's packet is,
/// with the whole leftover budget given to the history: when it still does
/// not fit, the oldest turns are left out first, the same suffix rule
/// `build_packet` applies; and when the provider refuses the request anyway,
/// the oldest half of what is left goes, a bounded number of times.
/// Whatever is left out is lost to the summary — it did not fit the
/// window, so no summary could have covered it — and `turns` in the result
/// says how many were folded. A history of which not even the newest turn
/// fits is an error, never a summary of the earlier summary alone under a
/// count of zero.
///
/// The reply is trimmed and capped at [`MAX_COMPACTION_SUMMARY`] bytes on a
/// character boundary. `tokens`/`cost_usd_micros` are the step's own; the
/// caller accounts for them exactly once.
pub fn summarize_conversation<B: LiveModelCall>(
    backing: &mut B,
    prior_summary: Option<&str>,
    turns: &[ConversationTurn],
    context_limit: u32,
    output_reserve: u32,
    cancel: &CancellationToken,
) -> Result<CompactionSummary, CompactionError> {
    let prior_summary = prior_summary.filter(|text| !text.is_empty());
    if turns.is_empty() && prior_summary.is_none() {
        return Err(CompactionError::NothingToCompact);
    }
    if cancel.is_cancelled() {
        return Err(CompactionError::Model(ModelStepError::Cancelled));
    }
    let mut skip_oldest = turns.len().saturating_sub(MAX_COMPACTION_TURNS);
    let mut shrinks = 0;
    let output = loop {
        let packet = loop {
            let packet = compile_compaction_request(
                prior_summary,
                turns,
                skip_oldest,
                context_limit,
                output_reserve,
            )
            .map_err(CompactionError::Compile)?;
            let dropped_turn = packet
                .dropped()
                .iter()
                .any(|block| block.locator().starts_with(CONVERSATION_LOCATOR_PREFIX));
            if !dropped_turn || skip_oldest >= turns.len() {
                break packet;
            }
            skip_oldest += 1;
        };
        // Not one turn fits beside the instructions (or, with no turns, not
        // the earlier summary): the window is too small for this history,
        // which is what the compiler's own variant for an over-budget
        // mandatory set names.
        let carries_history = packet
            .blocks()
            .iter()
            .any(|block| block.source() == context_engine::compile::ContextSource::Memory);
        if (!turns.is_empty() && skip_oldest >= turns.len()) || !carries_history {
            return Err(CompactionError::Compile(
                CompileError::MandatoryExceedsBudget,
            ));
        }
        match backing.step(packet.blocks(), &ModelStepInput::without_tools(0), cancel) {
            Err(ModelStepError::BoundExceeded)
                if shrinks < MAX_COMPACTION_SHRINKS && skip_oldest < turns.len() =>
            {
                let remaining = turns.len() - skip_oldest;
                skip_oldest += remaining.div_ceil(2);
                shrinks += 1;
            }
            Err(err) => return Err(CompactionError::Model(err)),
            Ok(output) => break output,
        }
    };
    let (text, tokens, cost_usd_micros) = match output {
        ModelStepOutput::Terminal {
            text,
            tokens,
            cost_usd_micros,
        } => (text, tokens, cost_usd_micros),
        // Tool calls, where none were offered: not a summary.
        ModelStepOutput::ToolCalls { .. } => return Err(CompactionError::NoSummary),
    };
    let text = bounded_summary(text.trim());
    if text.is_empty() {
        return Err(CompactionError::NoSummary);
    }
    Ok(CompactionSummary {
        text,
        turns: turns.len().saturating_sub(skip_oldest),
        tokens,
        cost_usd_micros,
    })
}

/// The compaction request: instructions, the earlier summary, then the
/// turns from `skip_oldest` on — the memory partition given the whole
/// leftover budget, since the history is the request.
fn compile_compaction_request(
    prior_summary: Option<&str>,
    turns: &[ConversationTurn],
    skip_oldest: usize,
    context_limit: u32,
    output_reserve: u32,
) -> Result<ContextPacket, CompileError> {
    let limits = context_engine::compile::CompileLimits::new()
        .retrieved_share_bps(0)
        .memory_share_bps(10_000)
        .read_set_share_bps(0);
    let mut ctx = CompileContext::new(context_limit, output_reserve)
        .limits(limits)
        .system(CompileInput::new(
            "system/compaction",
            COMPACTION_INSTRUCTIONS.to_owned(),
        ))
        .task(COMPACTION_TASK);
    if let Some(prior) = prior_summary {
        ctx = ctx.memory(
            CompileInput::new(COMPACTION_LOCATOR, prior.to_owned()).score(COMPACTION_SUMMARY_SCORE),
        );
    }
    for (index, turn) in turns.iter().enumerate().skip(skip_oldest) {
        ctx = ctx.memory(CompileInput::new(
            format!("{CONVERSATION_LOCATOR_PREFIX}{index}"),
            turn.render(),
        ));
    }
    compile(&ctx)
}

/// Cap a summary at [`MAX_COMPACTION_SUMMARY`] bytes without splitting a
/// character. The instructions ask for far less; this is the ceiling.
fn bounded_summary(text: &str) -> String {
    if text.len() <= MAX_COMPACTION_SUMMARY {
        return text.to_owned();
    }
    let mut end = MAX_COMPACTION_SUMMARY;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Production entry for `/compact`: the summarizer behind the same
/// supervised step layer a turn's model calls go through (bounded transient
/// retries, `--verbose` diagnostics), so a rate-limited provider is retried
/// here exactly as it is mid-turn, and the tokens the call cost are reported
/// once, in the result.
pub fn run_live_compaction<B: LiveModelCall>(
    backing: B,
    history: &ConversationHistory,
    context_limit: u32,
    output_reserve: u32,
    cancel: &CancellationToken,
    diag: Option<StepDiag>,
) -> Result<CompactionSummary, CompactionError> {
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let cost = CostAccumulator::new();
    let mut supervised = SupervisedModel {
        inner: backing,
        counter: std::sync::Arc::clone(&counter),
        cost: cost.clone(),
        diag,
    };
    let summary = summarize_conversation(
        &mut supervised,
        history.summary.as_deref(),
        &history.turns,
        context_limit,
        output_reserve,
        cancel,
    )?;
    // The supervised layer's own tally, not the step's: a retried step
    // costs every attempt.
    Ok(CompactionSummary {
        tokens: counter.load(std::sync::atomic::Ordering::Relaxed),
        cost_usd_micros: cost.total(),
        ..summary
    })
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

    fn conversation_of(n: usize, words_per_turn: usize) -> Vec<ConversationTurn> {
        (0..n)
            .map(|i| {
                let user = format!("turn {i} question ") + &"lorem ".repeat(words_per_turn);
                let answer = format!("turn {i} answer ") + &"ipsum ".repeat(words_per_turn);
                ConversationTurn::new(user, ConversationOutcome::Answered(Some(answer)))
            })
            .collect()
    }

    fn carried_turns(packet: &ContextPacket) -> Vec<usize> {
        packet
            .blocks()
            .iter()
            .filter_map(|block| {
                block
                    .locator()
                    .strip_prefix(CONVERSATION_LOCATOR_PREFIX)
                    .and_then(|n| n.parse().ok())
            })
            .collect()
    }

    #[test]
    fn conversation_blocks_are_carried_in_order_and_the_oldest_go_first_under_pressure() {
        let turns = conversation_of(4, 120);
        let mut previous = 0usize;
        for context_limit in [900u32, 1_400, 2_000, 3_000, 6_000, 40_000] {
            let preserved =
                PreservedLiveContext::new("goal", Vec::new(), "", "", context_limit, 128)
                    .expect("preserved")
                    .with_conversation(turns.clone());
            let packet = build_packet(&preserved, None).expect("compile");
            let carried = carried_turns(&packet);
            // Chronological, and a suffix of the history: never a hole.
            let expected: Vec<usize> = (turns.len() - carried.len()..turns.len()).collect();
            assert_eq!(carried, expected, "limit {context_limit}");
            assert!(
                carried.len() >= previous,
                "more room never carries fewer turns"
            );
            previous = carried.len();
            let rendered: Vec<&str> = packet
                .blocks()
                .iter()
                .filter(|b| b.locator().starts_with(CONVERSATION_LOCATOR_PREFIX))
                .map(|b| b.text())
                .collect();
            for (text, i) in rendered.iter().zip(&carried) {
                assert!(
                    text.contains(&format!("turn {i} question")),
                    "block order follows turn order"
                );
            }
        }
        assert_eq!(previous, turns.len(), "at 40k tokens all four turns fit");
    }

    #[test]
    fn a_conversation_turn_renders_the_user_verbatim_and_says_how_it_ended() {
        let answered = ConversationTurn::new(
            "rename it",
            ConversationOutcome::Answered(Some("Renamed.".to_owned())),
        )
        .render();
        assert_eq!(answered, "[user]\nrename it\n[assistant]\nRenamed.");
        let failed =
            ConversationTurn::new("try", ConversationOutcome::Failed("model down".to_owned()))
                .render();
        assert!(
            failed.ends_with("(the turn failed: model down)"),
            "{failed}"
        );
        let interrupted = ConversationTurn::new("go", ConversationOutcome::Interrupted).render();
        assert!(
            interrupted.ends_with("(the turn was interrupted)"),
            "{interrupted}"
        );
        let silent = ConversationTurn::new("hm", ConversationOutcome::Answered(None)).render();
        assert!(silent.ends_with("(finished without a reply)"), "{silent}");
    }

    #[test]
    fn only_the_newest_turns_are_kept_past_the_cap() {
        let preserved = PreservedLiveContext::new("goal", Vec::new(), "", "", 8192, 256)
            .expect("preserved")
            .with_conversation(conversation_of(MAX_CONVERSATION_TURNS + 5, 1));
        assert_eq!(preserved.conversation().len(), MAX_CONVERSATION_TURNS);
        assert!(preserved.conversation()[0].user().starts_with("turn 5 "));
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
            history.push(exchange(
                "repo_read",
                &format!(r#"{{"path":"distinct-{i}.rs"}}"#),
            ));
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
        /// Every step's blocks as `(locator, text)`, one entry per step —
        /// what the model was actually handed, so a test can say what a
        /// recovery or a compaction put in front of it.
        seen: SeenBlocks,
    }

    /// One `(locator, text)` list per step.
    type SeenBlocks = Rc<RefCell<Vec<Vec<(String, String)>>>>;
    impl ScriptedBacking {
        fn new(outputs: Vec<Result<ModelStepOutput, ModelStepError>>) -> Self {
            Self {
                outputs: Rc::new(RefCell::new(outputs.into())),
                saw_blocks: Rc::new(RefCell::new(Vec::new())),
                seen: Rc::new(RefCell::new(Vec::new())),
            }
        }

        /// The `(locator, text)` blocks of step `index` (0-based).
        fn step_blocks(&self, index: usize) -> Vec<(String, String)> {
            self.seen.borrow().get(index).cloned().unwrap_or_default()
        }
    }
    impl Clone for ScriptedBacking {
        fn clone(&self) -> Self {
            Self {
                outputs: Rc::clone(&self.outputs),
                saw_blocks: Rc::clone(&self.saw_blocks),
                seen: Rc::clone(&self.seen),
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
            self.seen.borrow_mut().push(
                blocks
                    .iter()
                    .map(|block| (block.locator().to_owned(), block.text().to_owned()))
                    .collect(),
            );
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
        Ok(ModelStepOutput::Terminal {
            text: text.to_owned(),
            tokens: 1,
            cost_usd_micros: None,
        })
    }

    fn auth_failure() -> Result<ModelStepOutput, ModelStepError> {
        Err(ModelStepError::ProviderFailed {
            cause: FailureCause::Auth,
        })
    }

    fn connection_failure() -> Result<ModelStepOutput, ModelStepError> {
        Err(ModelStepError::ProviderFailed {
            cause: FailureCause::Connection,
        })
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
        assert_eq!(
            err,
            ModelStepError::ProviderFailed {
                cause: FailureCause::Auth
            }
        );
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
    fn spent_on_accumulates_across_multiple_successful_steps_for_the_same_model() {
        let primary_ref = model_ref("b-ai", "deepseek");
        let controller = chain_controller(primary_ref.clone(), Vec::new());
        let primary = ScriptedBacking::new(vec![
            Ok(ModelStepOutput::Terminal {
                text: "one".to_owned(),
                tokens: 1,
                cost_usd_micros: Some(1_000),
            }),
            Ok(ModelStepOutput::Terminal {
                text: "two".to_owned(),
                tokens: 1,
                cost_usd_micros: Some(500),
            }),
        ]);
        let mut chain =
            FallbackChainModel::new(vec![(primary_ref.clone(), primary)], controller, None);
        assert_eq!(
            chain.spent_on(&primary_ref),
            None,
            "nothing spent before any step has run"
        );
        let _ = chain
            .step(&[], &step_input(), &CancellationToken::new())
            .expect("first step");
        assert_eq!(chain.spent_on(&primary_ref), Some(1_000));
        let _ = chain
            .step(&[], &step_input(), &CancellationToken::new())
            .expect("second step");
        assert_eq!(
            chain.spent_on(&primary_ref),
            Some(1_500),
            "cost accumulates across steps within one turn, it is not reset per step"
        );
    }

    #[test]
    fn a_fallback_decision_reports_cost_already_spent_on_the_abandoned_model() {
        let primary_ref = model_ref("b-ai", "deepseek");
        let alt_ref = model_ref("openrouter", "ling-3");
        let controller = chain_controller(primary_ref.clone(), vec![alt_ref.clone()]);
        let primary = ScriptedBacking::new(vec![
            Ok(ModelStepOutput::Terminal {
                text: "first step ok".to_owned(),
                tokens: 1,
                cost_usd_micros: Some(2_500),
            }),
            auth_failure(),
        ]);
        let alt = ScriptedBacking::new(vec![ok_terminal("from alternate")]);
        let mut chain = FallbackChainModel::new(
            vec![(primary_ref, primary), (alt_ref, alt)],
            controller,
            None,
        );
        let _ = chain
            .step(&[], &step_input(), &CancellationToken::new())
            .expect("first step succeeds and reports a real cost");
        let _ = chain
            .step(&[], &step_input(), &CancellationToken::new())
            .expect("second step falls back on an auth failure");

        let decisions = chain.decisions().snapshot();
        let fallback = decisions
            .iter()
            .find(|decision| decision.reason == RouterDecisionReason::FallbackTo)
            .expect("a fallback decision was recorded");
        assert_eq!(
            fallback.spent_usd_micros,
            Some(2_500),
            "the decision must report what was already spent on the model being abandoned, \
             not None just because this specific step's own attempt never succeeded"
        );
    }

    #[test]
    fn decisions_carry_no_policy_version_until_set_and_a_real_one_after() {
        // No alternate configured, so an auth failure (never retried on the
        // same backend) goes straight to `Stop` — one `RouterDecisionRecord`
        // pushed per `step()` call, giving two independent decisions to
        // compare before/after `set_policy_version` on the same chain.
        let primary_ref = model_ref("b-ai", "deepseek");
        let controller = chain_controller(primary_ref.clone(), Vec::new());
        let primary = ScriptedBacking::new(vec![auth_failure(), auth_failure()]);
        let mut chain = FallbackChainModel::new(vec![(primary_ref, primary)], controller, None);

        let _ = chain.step(&[], &step_input(), &CancellationToken::new());
        chain.set_policy_version(Some("deadbeefcafef00d".to_owned()));
        let _ = chain.step(&[], &step_input(), &CancellationToken::new());

        let decisions = chain.decisions().snapshot();
        assert_eq!(decisions.len(), 2, "one Stop decision per step call");
        assert_eq!(
            decisions[0].policy_version, None,
            "no managed policy was set yet when the first decision was recorded"
        );
        assert_eq!(
            decisions[1].policy_version.as_deref(),
            Some("deadbeefcafef00d"),
            "once set, every subsequent decision must carry the attached policy version"
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
        // RouterDecisionRecord (Modbit MOD-005): every retry/fallback is
        // captured, not just diagnosed to stderr.
        let decisions = chain.decisions().snapshot();
        assert_eq!(decisions.len(), 3, "{decisions:?}");
        assert!(matches!(
            decisions[0].reason,
            RouterDecisionReason::RetrySame
        ));
        assert!(matches!(
            decisions[1].reason,
            RouterDecisionReason::RetrySame
        ));
        assert!(matches!(
            decisions[2].reason,
            RouterDecisionReason::FallbackTo
        ));
        assert!(
            decisions[2].resolved_model.contains("ling-3"),
            "{decisions:?}"
        );
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
        assert_eq!(
            err,
            ModelStepError::ProviderFailed {
                cause: FailureCause::Auth
            }
        );
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
                cost_usd_micros: None,
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
        assert_eq!(
            reminders.source(),
            context_engine::compile::ContextSource::System
        );
        // Out-of-bounds blocks are refused: nothing enters the packet.
        let oversized = "x".repeat(MAX_REMINDERS_BLOCK_BYTES + 1);
        let with_oversized = preserved().with_reminders_block(Some(oversized));
        let packet = build_packet(&with_oversized, None).expect("packet");
        assert!(
            packet
                .blocks()
                .iter()
                .all(|b| !b.text().contains("reminders schema"))
        );
        // None adds no block.
        let packet = build_packet(&preserved(), None).expect("packet");
        assert!(
            packet
                .blocks()
                .iter()
                .all(|b| !b.text().contains("reminders schema"))
        );
    }

    #[test]
    fn todos_index_is_compiled_into_the_packet_as_a_system_block() {
        let text = "- [in_progress] wire the thing\n- [pending] test it";
        let with_todos = preserved().with_todos_index(Some(text.to_owned()));
        let packet = build_packet(&with_todos, None).expect("packet");
        let block = packet
            .blocks()
            .iter()
            .find(|b| b.text().contains("wire the thing"))
            .expect("todos block in packet");
        assert_eq!(
            block.source(),
            context_engine::compile::ContextSource::System
        );
        // None adds no block.
        let packet = build_packet(&preserved(), None).expect("packet");
        assert!(
            packet
                .blocks()
                .iter()
                .all(|b| !b.text().contains("wire the thing"))
        );
    }

    #[test]
    fn stall_warning_is_compiled_into_the_packet_as_a_system_block() {
        let with_warning = preserved().with_stall_warning(Some("repo_read repeated 3x".to_owned()));
        let packet = build_packet(&with_warning, None).expect("packet");
        let block = packet
            .blocks()
            .iter()
            .find(|b| b.text().contains("repo_read repeated 3x"))
            .expect("stall block in packet");
        assert_eq!(
            block.source(),
            context_engine::compile::ContextSource::System
        );
        assert!(block.text().starts_with("Stall detected:"));
        // Out-of-bounds is refused: nothing enters the packet.
        let oversized = "x".repeat(MAX_STALL_WARNING_BYTES + 1);
        let with_oversized = preserved().with_stall_warning(Some(oversized));
        let packet = build_packet(&with_oversized, None).expect("packet");
        assert!(
            packet
                .blocks()
                .iter()
                .all(|b| !b.text().contains("Stall detected"))
        );
        // None adds no block.
        let packet = build_packet(&preserved(), None).expect("packet");
        assert!(
            packet
                .blocks()
                .iter()
                .all(|b| !b.text().contains("Stall detected"))
        );
    }

    #[test]
    fn live_context_model_driver_injects_and_clears_the_stall_block_across_steps() {
        // End-to-end wiring test for Modbit `AGT-017`'s "model sees its own
        // stall" half: `LiveContextModelDriver::step` is the layer that
        // rebuilds the packet, not `build_packet` in isolation.
        let base = preserved();
        let live = Rc::new(RefCell::new(LiveContext {
            packet: build_packet(&base, None).expect("packet"),
            revision: ArtifactId::from_bytes(b"test/stall-wiring"),
            preserved: base,
            summary: None,
            recovered: None,
        }));
        let backing = ScriptedBacking::new(vec![
            ok_terminal("s1"),
            ok_terminal("s2"),
            ok_terminal("s3"),
        ]);
        let mut driver = LiveContextModelDriver {
            live: Rc::clone(&live),
            backing: Rc::new(RefCell::new(backing)),
        };

        // No history yet: no stall block.
        driver
            .step(&ModelStepInput::without_tools(1), &CancellationToken::new())
            .expect("step 1");
        assert!(live.borrow().preserved().stall_warning().is_none());
        assert!(
            live.borrow()
                .packet()
                .blocks()
                .iter()
                .all(|b| !b.text().contains("Stall detected"))
        );

        // A real stall: the same call repeated with no distinct progress.
        let stalled = vec![
            exchange("repo_read", r#"{"path":"a.rs"}"#),
            exchange("repo_read", r#"{"path":"a.rs"}"#),
            exchange("repo_read", r#"{"path":"a.rs"}"#),
        ];
        driver
            .step(
                &ModelStepInput::with_history(2, &stalled, &[]),
                &CancellationToken::new(),
            )
            .expect("step 2");
        assert!(live.borrow().preserved().stall_warning().is_some());
        assert!(
            live.borrow()
                .packet()
                .blocks()
                .iter()
                .any(|b| b.text().contains("Stall detected") && b.text().contains("repo_read"))
        );

        // Distinct progress: the warning clears from both preserved state and
        // the packet, not just left stale from the prior step.
        let varied = vec![
            exchange("repo_read", r#"{"path":"a.rs"}"#),
            exchange("repo_read", r#"{"path":"b.rs"}"#),
            exchange("repo_read", r#"{"path":"c.rs"}"#),
        ];
        driver
            .step(
                &ModelStepInput::with_history(3, &varied, &[]),
                &CancellationToken::new(),
            )
            .expect("step 3");
        assert!(live.borrow().preserved().stall_warning().is_none());
        assert!(
            live.borrow()
                .packet()
                .blocks()
                .iter()
                .all(|b| !b.text().contains("Stall detected"))
        );
    }

    #[test]
    fn read_memory_index_bounded_never_buffers_past_the_cap() {
        // `.rapidlm/MEMORY.md` is git-committed and team-shared, so it
        // arrives via `git clone`, not a bounded write this binary
        // controls. Directly asserting on the raw, pre-line-truncation
        // read size (rather than `load_memory_index`'s final output,
        // which a post-hoc truncate-after-full-read would also satisfy)
        // is what actually distinguishes "the read itself is capped" from
        // "the whole file is read, then the output is trimmed" — the two
        // are behaviorally identical from the caller's side for any file
        // under a few MB, which is exactly why this needs its own test on
        // the extracted helper instead of only on `load_memory_index`.
        let root = std::env::temp_dir().join(format!(
            "rapidlm-host-memory-bound-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".rapidlm")).expect("dir");
        let oversized = vec![b'x'; MAX_MEMORY_INDEX_BYTES * 4];
        std::fs::write(root.join(".rapidlm").join("MEMORY.md"), &oversized).expect("write");

        let text = read_memory_index_bounded(&root).expect("file exists");
        assert_eq!(
            text.len(),
            MAX_MEMORY_INDEX_BYTES,
            "the read must stop at the cap regardless of the file's real size on disk"
        );
        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn load_memory_index_truncates_an_oversized_multiline_file_instead_of_dropping_it() {
        // This function's own doc comment promises "oversized content is
        // truncated to the line and byte bounds rather than dropped
        // entirely". Many short lines whose cumulative size, not any
        // single line, is what exceeds the cap — a single line bigger
        // than the whole budget is dropped outright by the existing
        // line-level trimming (unrelated to and unchanged by the read-
        // bounding fix above), so that shape wouldn't exercise this
        // specific promise the way a realistic oversized team memory doc
        // does.
        let root = std::env::temp_dir().join(format!(
            "rapidlm-host-memory-truncate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".rapidlm")).expect("dir");
        let oversized: String = "0123456789012345678901234567890123456789\n".repeat(30_000);
        std::fs::write(root.join(".rapidlm").join("MEMORY.md"), &oversized).expect("write");

        let text = load_memory_index(&root)
            .expect("oversized multi-line content must be truncated, not dropped");
        assert!(
            text.len() <= MAX_MEMORY_INDEX_BYTES,
            "output must still respect the byte bound: {} bytes",
            text.len()
        );
        assert!(text.lines().count() <= MAX_MEMORY_INDEX_LINES);
        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn load_todos_index_renders_persisted_entries_and_fails_open() {
        let root = std::env::temp_dir().join(format!(
            "rapidlm-host-todos-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".rapidlm")).expect("dir");

        // Missing file: None, not an error.
        assert!(load_todos_index(&root).is_none());

        // Corrupt JSON: None.
        std::fs::write(root.join(crate::exec_tools::TODOS_PATH), b"not json").expect("write");
        assert!(load_todos_index(&root).is_none());

        // Empty todos array: None (nothing worth injecting).
        std::fs::write(
            root.join(crate::exec_tools::TODOS_PATH),
            br#"{"schema":1,"todos":[]}"#,
        )
        .expect("write");
        assert!(load_todos_index(&root).is_none());

        // Real entries, including one malformed entry that must be skipped
        // without failing the whole projection.
        std::fs::write(
            root.join(crate::exec_tools::TODOS_PATH),
            br#"{"schema":1,"todos":[
                {"id":"1","content":"wire the thing","status":"in_progress"},
                {"id":"2","content":"test it","status":"pending"},
                {"id":"3","status":"pending"}
            ]}"#,
        )
        .expect("write");
        let rendered = load_todos_index(&root).expect("rendered");
        assert_eq!(
            rendered,
            "- [in_progress] wire the thing\n- [pending] test it"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_todos_index_renders_owner_and_distinguishes_blocked_from_satisfied_dependencies() {
        let root = std::env::temp_dir().join(format!(
            "rapidlm-host-todos-metadata-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".rapidlm")).expect("dir");
        std::fs::write(
            root.join(crate::exec_tools::TODOS_PATH),
            br#"{"schema":1,"todos":[
                {"id":"1","content":"prereq","status":"completed"},
                {"id":"2","content":"still open","status":"pending"},
                {"id":"3","content":"ready to start","status":"pending",
                 "owner":"alice","depends_on":["1"]},
                {"id":"4","content":"waiting on something","status":"pending",
                 "depends_on":["2"]}
            ]}"#,
        )
        .expect("write");
        let rendered = load_todos_index(&root).expect("rendered");
        assert_eq!(
            rendered,
            "- [completed] prereq\n\
             - [pending] still open\n\
             - [pending] ready to start (owner: alice) (depends on: 1)\n\
             - [pending] waiting on something (blocked by: 2)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_todos_index_treats_a_file_past_the_bound_as_corrupt_even_if_it_is_valid_json() {
        // A file merely oversized-and-garbage would return `None` either
        // way (garbage never parses, bounded or not), which wouldn't
        // actually distinguish "the read is capped" from "the whole file
        // is read, then rejected". Using genuinely valid, complete JSON
        // that only exceeds the cap because of a large trailing field
        // does distinguish them: an unbounded read gets the whole
        // document and parses it successfully, while a read capped short
        // of the file's real size truncates mid-value, making the bytes
        // actually read invalid JSON — the observable proof that the read
        // itself, not just the outcome, is now bounded.
        let root = std::env::temp_dir().join(format!(
            "rapidlm-host-todos-bound-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".rapidlm")).expect("dir");
        let padding = "a".repeat(MAX_TODOS_INDEX_BYTES * 2);
        let content = format!(
            r#"{{"schema":1,"todos":[{{"id":"1","content":"keep this","status":"pending"}}],"padding":"{padding}"}}"#
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&content).is_ok(),
            "the fixture itself must be valid JSON when read in full"
        );
        std::fs::write(root.join(crate::exec_tools::TODOS_PATH), &content).expect("write");

        assert!(
            load_todos_index(&root).is_none(),
            "a file whose real size exceeds the bound must be treated as \
             corrupt (truncated mid-value), not parsed in full"
        );

        drop(std::fs::remove_dir_all(&root));
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
        assert_eq!(
            retrieved.source(),
            context_engine::compile::ContextSource::Retrieved
        );
        assert_eq!(retrieved.trust(), TrustClass::Untrusted);
        assert_eq!(retrieved.freshness(), Freshness::Fresh);
        // No retrieved blocks configured: none enter the packet either.
        let packet = build_packet(&preserved(), None).expect("packet");
        assert!(
            packet
                .blocks()
                .iter()
                .all(|b| !b.text().contains("LRUCache"))
        );
    }

    fn ok(text: &str) -> Result<ModelStepOutput, ModelStepError> {
        Ok(ModelStepOutput::Terminal {
            text: text.to_owned(),
            tokens: 1,
            cost_usd_micros: None,
        })
    }

    fn locators(blocks: &[(String, String)]) -> Vec<&str> {
        blocks.iter().map(|(locator, _)| locator.as_str()).collect()
    }

    fn with_history(turns: usize) -> PreservedLiveContext {
        preserved().with_conversation(conversation_of(turns, 40))
    }

    #[test]
    fn a_carried_summary_enters_the_packet_ahead_of_the_turns_with_the_post_compaction_prompt() {
        let preserved =
            with_history(2).with_compaction_summary(Some("what came before".to_owned()));
        let packet = build_packet(&preserved, None).expect("packet");
        let locators: Vec<&str> = packet.blocks().iter().map(|b| b.locator()).collect();
        let summary_at = locators
            .iter()
            .position(|l| *l == COMPACTION_LOCATOR)
            .expect("summary block");
        let first_turn_at = locators
            .iter()
            .position(|l| l.starts_with(CONVERSATION_LOCATOR_PREFIX))
            .expect("turn block");
        assert!(
            summary_at < first_turn_at,
            "the summary stands for the older history, so it reads first: {locators:?}"
        );
        let post = packet
            .blocks()
            .iter()
            .find(|b| b.locator() == POST_COMPACTION_LOCATOR)
            .expect("post-compaction system block");
        assert_eq!(
            post.source(),
            context_engine::compile::ContextSource::System
        );
        assert!(
            post.text()
                .contains(agent_runtime::POST_COMPACTION_SYSTEM_PROMPT)
        );

        // A recovery's own summary supersedes the carried one.
        let packet = build_packet(&preserved, Some("newer")).expect("packet");
        let texts: Vec<&str> = packet
            .blocks()
            .iter()
            .filter(|b| b.locator() == COMPACTION_LOCATOR)
            .map(|b| b.text())
            .collect();
        assert_eq!(texts, vec!["newer"]);

        // No summary: neither block.
        let packet = build_packet(&with_history(2), None).expect("packet");
        assert!(
            packet.blocks().iter().all(
                |b| b.locator() != COMPACTION_LOCATOR && b.locator() != POST_COMPACTION_LOCATOR
            )
        );

        // Bounds: empty and oversized summaries attach nothing.
        assert_eq!(
            preserved
                .clone()
                .with_compaction_summary(Some(String::new()))
                .compaction_summary(),
            Some("what came before")
        );
        assert_eq!(
            preserved
                .clone()
                .with_compaction_summary(Some("x".repeat(MAX_COMPACTION_SUMMARY + 1)))
                .compaction_summary(),
            Some("what came before")
        );
        assert_eq!(
            preserved.with_compaction_summary(None).compaction_summary(),
            None
        );
    }

    #[test]
    fn summarize_conversation_sends_the_history_and_returns_the_reply() {
        let backing = ScriptedBacking::new(vec![Ok(ModelStepOutput::Terminal {
            text: "  Nightjar; tests still red.  ".to_owned(),
            tokens: 77,
            cost_usd_micros: Some(5),
        })]);
        let turns = conversation_of(2, 3);
        let summary = summarize_conversation(
            &mut backing.clone(),
            Some("earlier"),
            &turns,
            8192,
            256,
            &CancellationToken::new(),
        )
        .expect("summary");
        assert_eq!(summary.text, "Nightjar; tests still red.");
        assert_eq!(summary.turns, 2);
        assert_eq!((summary.tokens, summary.cost_usd_micros), (77, Some(5)));

        let request = backing.step_blocks(0);
        let locators = locators(&request);
        let earlier_at = locators
            .iter()
            .position(|l| *l == COMPACTION_LOCATOR)
            .expect("earlier summary in the request");
        let turn0_at = locators
            .iter()
            .position(|l| *l == "conversation/turn-0")
            .expect("turn 0");
        let turn1_at = locators
            .iter()
            .position(|l| *l == "conversation/turn-1")
            .expect("turn 1");
        assert!(earlier_at < turn0_at && turn0_at < turn1_at, "{locators:?}");
        assert!(
            request
                .iter()
                .any(|(l, t)| l == "system/compaction" && t.contains("Reply with the summary only")),
            "{locators:?}"
        );
        assert!(
            request.iter().any(|(_, t)| t.contains("turn 1 question")),
            "the turns go in as the model would see them on a turn"
        );
    }

    #[test]
    fn summarize_conversation_refuses_nothing_and_a_reply_that_is_not_a_summary() {
        let mut nothing = ScriptedBacking::new(vec![ok("unused")]);
        assert_eq!(
            summarize_conversation(
                &mut nothing,
                None,
                &[],
                8192,
                256,
                &CancellationToken::new()
            ),
            Err(CompactionError::NothingToCompact)
        );
        assert!(
            nothing.seen.borrow().is_empty(),
            "no model call for nothing"
        );

        let call = ProposedToolCall::new("c1", "repo_read", "{}").expect("call");
        let mut tools = ScriptedBacking::new(vec![Ok(ModelStepOutput::ToolCalls {
            calls: vec![call],
            tokens: 1,
            cost_usd_micros: None,
        })]);
        assert_eq!(
            summarize_conversation(
                &mut tools,
                None,
                &conversation_of(1, 2),
                8192,
                256,
                &CancellationToken::new()
            ),
            Err(CompactionError::NoSummary)
        );

        let mut blank = ScriptedBacking::new(vec![ok("   \n ")]);
        assert_eq!(
            summarize_conversation(
                &mut blank,
                None,
                &conversation_of(1, 2),
                8192,
                256,
                &CancellationToken::new()
            ),
            Err(CompactionError::NoSummary)
        );

        let mut failing = ScriptedBacking::new(vec![Err(ModelStepError::Failed)]);
        assert_eq!(
            summarize_conversation(
                &mut failing,
                None,
                &conversation_of(1, 2),
                8192,
                256,
                &CancellationToken::new()
            ),
            Err(CompactionError::Model(ModelStepError::Failed))
        );
    }

    #[test]
    fn summarize_conversation_leaves_the_oldest_turns_out_when_they_do_not_fit() {
        // A tiny window: the newest turns still get summarised, the oldest
        // are left out — a suffix, the same rule the turn packet applies —
        // and the count reports what was actually folded.
        let backing = ScriptedBacking::new(vec![ok("summary")]);
        let turns = conversation_of(6, 40);
        let summary = summarize_conversation(
            &mut backing.clone(),
            None,
            &turns,
            1_400,
            64,
            &CancellationToken::new(),
        )
        .expect("summary");
        let carried: Vec<usize> = backing
            .step_blocks(0)
            .iter()
            .filter_map(|(l, _)| l.strip_prefix(CONVERSATION_LOCATOR_PREFIX)?.parse().ok())
            .collect();
        assert!(!carried.is_empty() && carried.len() < 6, "{carried:?}");
        let expected: Vec<usize> = (6 - carried.len()..6).collect();
        assert_eq!(carried, expected, "a suffix of the history, newest kept");
        assert_eq!(summary.turns, carried.len());
    }

    #[test]
    fn a_compaction_request_the_provider_refuses_is_resent_with_fewer_turns() {
        // The provider's own count disagrees with the compiler's estimate,
        // as it can for a turn's packet: the request is re-sent without
        // the oldest half of its turns, a bounded number of times.
        let backing = ScriptedBacking::new(vec![
            Err(ModelStepError::BoundExceeded),
            Err(ModelStepError::BoundExceeded),
            ok("summary of the newest"),
        ]);
        let turns = conversation_of(8, 4);
        let summary = summarize_conversation(
            &mut backing.clone(),
            None,
            &turns,
            8192,
            256,
            &CancellationToken::new(),
        )
        .expect("summary");
        assert_eq!(summary.text, "summary of the newest");
        let carried = |step: usize| -> Vec<usize> {
            backing
                .step_blocks(step)
                .iter()
                .filter_map(|(l, _)| l.strip_prefix(CONVERSATION_LOCATOR_PREFIX)?.parse().ok())
                .collect()
        };
        assert_eq!(carried(0), (0..8).collect::<Vec<_>>());
        assert_eq!(
            carried(1),
            (4..8).collect::<Vec<_>>(),
            "the oldest half went"
        );
        assert_eq!(carried(2), (6..8).collect::<Vec<_>>(), "and half again");
        assert_eq!(summary.turns, 2, "what was actually folded");

        // Refused every time: the bound holds and the refusal is the error.
        let mut always = ScriptedBacking::new(vec![
            Err(ModelStepError::BoundExceeded),
            Err(ModelStepError::BoundExceeded),
            Err(ModelStepError::BoundExceeded),
            Err(ModelStepError::BoundExceeded),
            ok("never"),
        ]);
        assert_eq!(
            summarize_conversation(
                &mut always,
                None,
                &turns,
                8192,
                256,
                &CancellationToken::new()
            ),
            Err(CompactionError::Model(ModelStepError::BoundExceeded))
        );
        assert_eq!(
            always.seen.borrow().len(),
            4,
            "three shrinks, then the refusal stands"
        );
    }

    #[test]
    fn a_history_of_which_no_turn_fits_is_refused_not_summarised_as_zero_turns() {
        // With an earlier summary present, a window too small for even the
        // newest turn used to re-summarise the summary alone under a count
        // of zero — and the record would then have covered the turns that
        // never fit.
        let mut backing = ScriptedBacking::new(vec![ok("never")]);
        let turns = conversation_of(1, 400);
        assert!(matches!(
            summarize_conversation(
                &mut backing,
                Some("earlier"),
                &turns,
                700,
                64,
                &CancellationToken::new()
            ),
            Err(CompactionError::Compile(_))
        ));
        assert!(backing.seen.borrow().is_empty(), "no model call");
    }

    #[test]
    fn a_very_long_history_folds_its_newest_turns() {
        // More turns than the compiler admits blocks: the newest
        // `MAX_COMPACTION_TURNS` are folded rather than the request failing
        // with a capacity error.
        let backing = ScriptedBacking::new(vec![ok("summary")]);
        let turns = conversation_of(MAX_COMPACTION_TURNS + 40, 1);
        let summary = summarize_conversation(
            &mut backing.clone(),
            None,
            &turns,
            200_000,
            256,
            &CancellationToken::new(),
        )
        .expect("summary");
        assert_eq!(summary.turns, MAX_COMPACTION_TURNS);
        let carried: Vec<usize> = backing
            .step_blocks(0)
            .iter()
            .filter_map(|(l, _)| l.strip_prefix(CONVERSATION_LOCATOR_PREFIX)?.parse().ok())
            .collect();
        assert_eq!(carried.first().copied(), Some(40));
        assert_eq!(carried.last().copied(), Some(MAX_COMPACTION_TURNS + 39));
    }

    #[test]
    fn a_summary_is_capped_on_a_character_boundary() {
        let long = "é".repeat(MAX_COMPACTION_SUMMARY);
        let capped = bounded_summary(&long);
        assert!(capped.len() <= MAX_COMPACTION_SUMMARY);
        assert!(capped.chars().all(|c| c == 'é'));
        assert_eq!(bounded_summary("short"), "short");
    }

    #[test]
    fn run_live_compaction_counts_the_tokens_of_every_attempt() {
        // Through the supervised layer, an empty reply is a billed
        // transient that is retried, and the reported tokens are the tally
        // of every attempt — not the final step's own figure.
        let backing = ScriptedBacking::new(vec![
            Ok(ModelStepOutput::Terminal {
                text: String::new(),
                tokens: 10,
                cost_usd_micros: Some(1),
            }),
            Ok(ModelStepOutput::Terminal {
                text: "summary".to_owned(),
                tokens: 40,
                cost_usd_micros: Some(3),
            }),
        ]);
        let history = ConversationHistory {
            summary: None,
            turns: conversation_of(1, 2),
            through_seq: 0,
        };
        let summary = run_live_compaction(
            backing.clone(),
            &history,
            8192,
            256,
            &CancellationToken::new(),
            None,
        )
        .expect("summary");
        assert_eq!(summary.text, "summary");
        assert_eq!((summary.tokens, summary.cost_usd_micros), (50, Some(4)));
        assert_eq!(backing.seen.borrow().len(), 2, "retried once");
    }

    #[test]
    fn overflow_folds_the_conversation_into_a_summary_the_model_wrote() {
        // The provider refused the packet. The recovery asks the same model
        // to summarise the session's earlier turns, and the retried step
        // is handed that summary where the turns were — a packet that is
        // genuinely smaller, not the same one plus a stats line.
        let backing = ScriptedBacking::new(vec![
            Err(ModelStepError::BoundExceeded),
            ok("The user named the project Nightjar and asked for tests."),
            ok("after rebuild"),
        ]);
        let host =
            LiveContextHost::build(with_history(3), backing.clone(), ContextRetryPolicy::new(2))
                .expect("host");
        let result = run_session(host).expect("execute").result;
        assert_eq!(result.summary(), "after rebuild");
        assert_eq!(result.context_lineage().len(), 1);
        assert_eq!(
            result.context_lineage()[0].source(),
            "context/live-recovery"
        );

        let refused = backing.step_blocks(0);
        assert_eq!(
            locators(&refused)
                .iter()
                .filter(|l| l.starts_with(CONVERSATION_LOCATOR_PREFIX))
                .count(),
            3,
            "the refused packet carried the turns: {:?}",
            locators(&refused)
        );

        let summarising = backing.step_blocks(1);
        assert!(
            summarising.iter().any(|(l, t)| l == "system/compaction" && t.contains("Reply with the summary only")),
            "the second call is the compaction request: {:?}",
            locators(&summarising)
        );
        assert_eq!(
            locators(&summarising)
                .iter()
                .filter(|l| l.starts_with(CONVERSATION_LOCATOR_PREFIX))
                .count(),
            3,
            "and it carries every turn to summarise"
        );
        assert!(
            summarising
                .iter()
                .all(|(l, _)| l != "rules/agents" && l != "criterion"),
            "the compaction request is the history and the instructions, not the turn's packet: {:?}",
            locators(&summarising)
        );

        let retried = backing.step_blocks(2);
        let summary = retried
            .iter()
            .find(|(l, _)| l == COMPACTION_LOCATOR)
            .map(|(_, t)| t.as_str());
        assert_eq!(
            summary,
            Some("The user named the project Nightjar and asked for tests."),
            "the retried step sees the summary: {:?}",
            locators(&retried)
        );
        assert!(
            retried
                .iter()
                .all(|(l, _)| !l.starts_with(CONVERSATION_LOCATOR_PREFIX)),
            "and none of the turns it stands for: {:?}",
            locators(&retried)
        );
        assert!(
            retried
                .iter()
                .any(|(l, t)| l == POST_COMPACTION_LOCATOR && t.contains("summary")),
            "and is told its history is a summary: {:?}",
            locators(&retried)
        );
        assert!(
            retried.iter().any(|(l, _)| l == "rules/agents")
                && retried.iter().any(|(l, _)| l == "task"),
            "everything mandatory survives the rebuild: {:?}",
            locators(&retried)
        );
    }

    #[test]
    fn an_overflow_with_nothing_optional_to_fold_is_not_recoverable() {
        // No turns, no retrieved context, no summary: a rebuild would be
        // the packet the provider just refused. That was retried as-is
        // until the bound ran out; now it is refused outright, as what it
        // is.
        let host = LiveContextHost::build(
            preserved(),
            overflow_then_terminal("never"),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let err = run_session(host).expect_err("not recoverable");
        assert_eq!(
            err,
            AgentExecutionError::ContextRecovery(
                agent_runtime::ContextRecoveryError::NotRecoverable
            )
        );
    }

    #[test]
    fn a_summary_that_does_not_shrink_the_packet_is_set_aside_and_the_turns_dropped() {
        // One tiny turn, and a model that answers the compaction request
        // with a summary longer than the turn it replaces: sending the
        // rebuilt packet would be the silent no-op retry this recovery
        // exists to end. The summary is set aside, the turns go anyway,
        // and the retry runs on the strictly smaller packet — recording
        // nothing, since the packet carries no summary.
        let backing = ScriptedBacking::new(vec![
            Err(ModelStepError::BoundExceeded),
            ok(&"long summary ".repeat(300)),
            ok("after fallback"),
        ]);
        let tiny = preserved().with_conversation(vec![ConversationTurn::new(
            "hi",
            ConversationOutcome::Answered(Some("yo".to_owned())),
        )]);
        let mut host = LiveContextHost::build(tiny, backing.clone(), ContextRetryPolicy::new(2))
            .expect("host");
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let outcome = host
            .execute(
                &request,
                &mut CountingTools { executed: 0 },
                &mut events,
                &CancellationToken::new(),
            )
            .expect("recovered on the smaller packet");
        assert_eq!(outcome.result.summary(), "after fallback");
        let retried = backing.step_blocks(2);
        assert!(
            retried.iter().all(
                |(l, _)| l != COMPACTION_LOCATOR && !l.starts_with(CONVERSATION_LOCATOR_PREFIX)
            ),
            "neither the oversized summary nor the turn: {:?}",
            locators(&retried)
        );
        assert_eq!(
            host.live_context().borrow().recovered(),
            None,
            "a summary the packet does not carry is not recorded"
        );
    }

    #[test]
    fn a_first_fold_takes_the_retrieved_context_with_the_turns_and_records_the_summary() {
        // Turns and Context Scout hits together: the first fold writes the
        // summary and drops the retrieved blocks with the turns (the model
        // can re-read files; the retry bound is short), and the summary is
        // what the turn reports for the caller to record.
        let backing = ScriptedBacking::new(vec![
            Err(ModelStepError::BoundExceeded),
            ok("what the turns said"),
            ok("done"),
        ]);
        let retrieved = CompileInput::new("retrieved:lru.py", "class LRUCache:\n    pass\n")
            .reason(context_engine::compile::CompileReason::Retrieved)
            .trust(TrustClass::Untrusted)
            .freshness(Freshness::Fresh);
        let preserved = with_history(2).with_retrieved_context(vec![retrieved]);
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let outcome = run_live_exec(
            preserved,
            backing.clone(),
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.result.summary(), "done");
        let refused = backing.step_blocks(0);
        assert!(
            refused.iter().any(|(l, _)| l == "retrieved:lru.py"),
            "{:?}",
            locators(&refused)
        );
        let retried = backing.step_blocks(2);
        assert!(
            retried.iter().all(
                |(l, _)| l != "retrieved:lru.py" && !l.starts_with(CONVERSATION_LOCATOR_PREFIX)
            ),
            "{:?}",
            locators(&retried)
        );
        assert!(
            retried
                .iter()
                .any(|(l, t)| l == COMPACTION_LOCATOR && t == "what the turns said")
        );
        assert_eq!(
            outcome.recovered,
            Some(RecoveredSummary {
                text: "what the turns said".to_owned(),
                turns: 2,
            })
        );
    }

    #[test]
    fn a_summarizer_failure_drops_the_turns_and_keeps_the_summary_already_carried() {
        // The model could not write the summary (a provider failure on the
        // compaction call). The turns still go — a smaller packet with
        // less history beats a failed turn, the trade the memory partition
        // already makes — and the summary the session carried in from an
        // earlier compaction stays, since it is the only account of the
        // older history left.
        let backing = ScriptedBacking::new(vec![
            Err(ModelStepError::BoundExceeded),
            Err(ModelStepError::Failed),
            ok("after rebuild"),
        ]);
        let preserved = with_history(2).with_compaction_summary(Some("older summary".to_owned()));
        let mut host =
            LiveContextHost::build(preserved, backing.clone(), ContextRetryPolicy::new(2))
                .expect("host");
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let result = host
            .execute(
                &request,
                &mut CountingTools { executed: 0 },
                &mut events,
                &CancellationToken::new(),
            )
            .expect("execute")
            .result;
        assert_eq!(result.summary(), "after rebuild");
        assert_eq!(
            host.live_context().borrow().recovered(),
            None,
            "the carried summary is not a summary this turn wrote"
        );
        let retried = backing.step_blocks(2);
        assert_eq!(
            retried
                .iter()
                .find(|(l, _)| l == COMPACTION_LOCATOR)
                .map(|(_, t)| t.as_str()),
            Some("older summary")
        );
        assert!(
            retried
                .iter()
                .all(|(l, _)| !l.starts_with(CONVERSATION_LOCATOR_PREFIX)),
            "{:?}",
            locators(&retried)
        );
    }

    #[test]
    fn a_cancelled_summarizer_call_ends_the_turn_as_cancelled() {
        // Ctrl-C while the recovery's compaction call is out: the turn ends
        // as cancelled, not as a context-recovery failure with a reason
        // nobody asked for.
        let cancel = CancellationToken::new();
        let cancel_on_second_step = {
            let cancel = cancel.clone();
            move |step: usize| {
                if step == 1 {
                    cancel.cancel();
                }
            }
        };
        struct CancelOnStep<F: FnMut(usize)> {
            inner: ScriptedBacking,
            steps: usize,
            hook: F,
        }
        impl<F: FnMut(usize)> LiveModelCall for CancelOnStep<F> {
            fn step(
                &mut self,
                blocks: &[ContextBlock],
                input: &ModelStepInput<'_>,
                cancel: &CancellationToken,
            ) -> Result<ModelStepOutput, ModelStepError> {
                (self.hook)(self.steps);
                self.steps += 1;
                self.inner.step(blocks, input, cancel)
            }
        }
        let backing = CancelOnStep {
            inner: ScriptedBacking::new(vec![
                Err(ModelStepError::BoundExceeded),
                ok("never"),
                ok("never"),
            ]),
            steps: 0,
            hook: cancel_on_second_step,
        };
        let mut host = LiveContextHost::build(with_history(2), backing, ContextRetryPolicy::new(2))
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
    fn normal_context_succeeds_without_recovery() {
        let host = LiveContextHost::build(
            preserved(),
            ScriptedBacking::new(vec![Ok(ModelStepOutput::Terminal {
                text: "done".to_owned(),
                tokens: 1,
                cost_usd_micros: None,
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
        // Each recovery makes the packet strictly smaller — the turns
        // become a summary, then the summary goes — and the executor's
        // bound still caps the sequence.
        let backing = ScriptedBacking::new(vec![
            Err(ModelStepError::BoundExceeded),
            ok("summary"),
            Err(ModelStepError::BoundExceeded),
            Err(ModelStepError::BoundExceeded),
        ]);
        let host =
            LiveContextHost::build(with_history(2), backing.clone(), ContextRetryPolicy::new(2))
                .expect("host");
        let err = run_session(host).expect_err("bounded stop");
        assert_eq!(err, AgentExecutionError::ContextRetryExceeded);
        let last = backing.step_blocks(3);
        assert!(
            last.iter().all(
                |(l, _)| l != COMPACTION_LOCATOR && !l.starts_with(CONVERSATION_LOCATOR_PREFIX)
            ),
            "the second recovery dropped the summary too: {:?}",
            locators(&last)
        );
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
                    cost_usd_micros: None,
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
            with_history(1),
            ScriptedBacking::new(vec![
                Err(ModelStepError::BoundExceeded),
                ok("summary"),
                ok("wired recovery"),
            ]),
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.result.summary(), "wired recovery");
        assert_eq!(
            outcome.tokens, 2,
            "the recovery's summary call and the terminal step are both counted, once each"
        );
        assert_eq!(
            outcome
                .recovered
                .as_ref()
                .map(|r| (r.text.as_str(), r.turns)),
            Some(("summary", 1)),
            "the summary the recovery wrote is reported for the caller to record"
        );
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
        assert_eq!(
            outcome.result.status(),
            agent_runtime::AgentTerminalStatus::Failed
        );
        assert_eq!(outcome.tokens, 0, "failed turns report no provider tokens");
        assert_eq!(
            outcome.failure_cause,
            Some(FailureCause::Unspecified),
            "unconfigured backing is an unspecified provider failure"
        );
        assert!(
            outcome.result.context_lineage().is_empty(),
            "no fake recovery"
        );
    }

    #[test]
    fn transient_step_failure_retries_then_succeeds() {
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        // Sequence: transient blip → context overflow (recovery: the
        // conversation is summarised) → terminal ok.
        let mut outputs = vec![Err(ModelStepError::ProviderFailed {
            cause: FailureCause::Transient {
                retry_after_ms: None,
            },
        })];
        outputs.push(Err(ModelStepError::BoundExceeded));
        outputs.push(ok("summary"));
        outputs.push(Ok(ModelStepOutput::Terminal {
            text: "after transient blip".to_owned(),
            tokens: 2,
            cost_usd_micros: None,
        }));
        let backing = ScriptedBacking::new(outputs);
        let witness = backing.clone();
        let outcome = run_live_exec(
            with_history(1),
            backing,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("transient failure recovers without operator action");
        assert_eq!(
            outcome.result.status(),
            agent_runtime::AgentTerminalStatus::Succeeded
        );
        assert_eq!(outcome.result.summary(), "after transient blip");
        assert_eq!(outcome.failure_cause, None);
        assert_eq!(
            witness.saw_blocks.borrow().len(),
            4,
            "transient step retried, plus the recovery's summary call"
        );
        assert_eq!(outcome.tokens, 3, "the summary call's token counted once");
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
        assert_eq!(
            outcome.result.status(),
            agent_runtime::AgentTerminalStatus::Failed
        );
        assert_eq!(
            outcome.failure_cause,
            Some(FailureCause::Transient {
                retry_after_ms: None
            }),
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
                cost_usd_micros: None,
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
        assert_eq!(
            outcome.result.status(),
            agent_runtime::AgentTerminalStatus::Failed
        );
        assert_eq!(outcome.failure_cause, Some(FailureCause::Auth));
        assert_eq!(
            witness.saw_blocks.borrow().len(),
            1,
            "auth failures are actionable, never auto-retried"
        );
    }

    #[test]
    fn an_exhausted_quota_is_not_retried() {
        // A 402 is not a capacity blip: retrying burns the backoff budget
        // against the same wall. It fails once, with its own cause.
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let backing = ScriptedBacking::new(vec![
            Err(ModelStepError::ProviderFailed {
                cause: FailureCause::Quota,
            }),
            Ok(ModelStepOutput::Terminal {
                text: "never reached".to_owned(),
                tokens: 1,
                cost_usd_micros: None,
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
        assert_eq!(outcome.failure_cause, Some(FailureCause::Quota));
        assert_eq!(witness.saw_blocks.borrow().len(), 1);
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
                cost_usd_micros: None,
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
        assert_eq!(
            witness.saw_blocks.borrow().len(),
            2,
            "one retry, then success"
        );
    }

    #[test]
    fn empty_terminal_response_is_retried_then_succeeds() {
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let backing = ScriptedBacking::new(vec![
            Ok(ModelStepOutput::Terminal {
                text: String::new(),
                tokens: 1,
                cost_usd_micros: None,
            }),
            Ok(ModelStepOutput::Terminal {
                text: String::new(),
                tokens: 1,
                cost_usd_micros: None,
            }),
            Ok(ModelStepOutput::Terminal {
                text: "finally non-empty".to_owned(),
                tokens: 1,
                cost_usd_micros: None,
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
                cost_usd_micros: None,
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
        assert_eq!(
            outcome.result.status(),
            agent_runtime::AgentTerminalStatus::Failed
        );
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
                cost_usd_micros: None,
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
        assert_eq!(
            witness.saw_blocks.borrow().len(),
            2,
            "one retry, then success"
        );

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
                cost_usd_micros: None,
            }),
            Err(ModelStepError::ProviderFailed {
                cause: FailureCause::Transient {
                    retry_after_ms: None,
                },
            }),
            Ok(ModelStepOutput::Terminal {
                text: "recovered after tools".to_owned(),
                tokens: 2,
                cost_usd_micros: None,
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
        assert_eq!(
            outcome.result.status(),
            agent_runtime::AgentTerminalStatus::Succeeded
        );
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
                    cause: FailureCause::Transient {
                        retry_after_ms: None,
                    },
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
            cause: FailureCause::Transient {
                retry_after_ms: None,
            },
        })];
        outputs.push(Ok(ModelStepOutput::Terminal {
            text: "done".to_owned(),
            tokens: 7,
            cost_usd_micros: None,
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
    fn exec_outcome_sums_reported_cost_across_steps_and_diagnoses_it() {
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let outputs = vec![
            Ok(ModelStepOutput::ToolCalls {
                calls: vec![ProposedToolCall::new("c1", "repo_read", "{}").expect("call")],
                tokens: 3,
                cost_usd_micros: Some(1_200),
            }),
            Ok(ModelStepOutput::Terminal {
                text: "done".to_owned(),
                tokens: 7,
                cost_usd_micros: Some(800),
            }),
        ];
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
        assert_eq!(
            outcome.cost_usd_micros,
            Some(2_000),
            "cost sums across every step that reported one"
        );
        let lines = lines.borrow().clone();
        assert!(
            lines
                .last()
                .expect("turn line")
                .contains("cost_usd_micros=2000"),
            "turn summary line should carry the total: {lines:?}"
        );
    }

    #[test]
    fn exec_outcome_cost_stays_none_when_no_step_ever_reports_one() {
        // None must never read back as "$0" — a provider that never reports
        // cost is "unknown," not "confirmed free."
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let outputs = vec![Ok(ModelStepOutput::Terminal {
            text: "done".to_owned(),
            tokens: 7,
            cost_usd_micros: None,
        })];
        let backing = ScriptedBacking::new(outputs);
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
        assert_eq!(outcome.cost_usd_micros, None);
    }

    #[test]
    fn diagnostics_lines_are_bounded_and_host_labels_never_carry_paths() {
        let (diag, lines) = StepDiag::buffer("https://user:secret@api.example.com/v1/path?q=1");
        for index in 0..(MAX_DIAG_LINES + 10) {
            diag.line(format!("line {index}"));
        }
        let lines = lines.borrow().clone();
        assert_eq!(
            lines.len(),
            MAX_DIAG_LINES,
            "extra lines are dropped, bounded"
        );
        assert_eq!(
            diag.host(),
            "api.example.com",
            "userinfo and path are stripped"
        );
    }
}
