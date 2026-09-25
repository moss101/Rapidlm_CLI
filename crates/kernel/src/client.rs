//! In-process [`KernelClient`] facade over session, turn, and event primitives.
//!
//! Frontends call this API. They do not open the ledger database or mutate
//! session state through TUI-only paths. Durable writes append through the
//! event ledger before success is returned.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use event_ledger::event::{ActorRef, ErasedEventEnvelope, EventKind};
use event_ledger::journal::{JournalError, OperationJournal, WaitState};
use event_ledger::ledger::{AppendOptions, EventLedger, LedgerError};
use event_ledger::subscription::{EventStream as LedgerEventStream, SubscriptionError};
use protocol::{
    ApiError, ErrorCode, RedactionClass, SessionId, TraceId, TurnId, UNKNOWN_INTERNAL_MESSAGE,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::CancellationToken;
use crate::cancel::{CancelOwner, CancelToken, CancellationTree};
use crate::session::fork::ForkSession;
use crate::session::projection::{
    MAX_REPLAY_EVENTS, ProjectionError, ProjectionInvariant, SessionSnapshot, apply,
};
use crate::session::service::{CreateSession, SessionError, SessionService};
use crate::turn::guard::{TurnGuardError, TurnLease, TurnSubmissionGuard};

/// Shared request/response surface used by in-process and later IPC transports.
pub trait KernelClient: Send + Sync {
    fn create_session(
        &self,
        req: CreateSession,
    ) -> impl Future<Output = Result<SessionSnapshot, ApiError>> + Send;
    fn get_session(
        &self,
        id: SessionId,
    ) -> impl Future<Output = Result<SessionSnapshot, ApiError>> + Send;
    fn submit_turn(
        &self,
        req: SubmitTurn,
    ) -> impl Future<Output = Result<TurnHandle, ApiError>> + Send;
    fn interrupt(&self, req: Interrupt) -> impl Future<Output = Result<(), ApiError>> + Send;
    fn subscribe(
        &self,
        req: SubscribeEvents,
    ) -> impl Future<Output = Result<EventStream, ApiError>> + Send;
    fn approve(&self, req: ResolveApproval) -> impl Future<Output = Result<(), ApiError>> + Send;
    fn record_approval(
        &self,
        req: RecordApproval,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;
    fn pending_approvals(
        &self,
        session_id: SessionId,
    ) -> impl Future<Output = Result<Vec<PendingApproval>, ApiError>> + Send;
    /// The suspended-turn detail recorded for a pending approval — the
    /// opaque payload the requesting surface wrote alongside its
    /// `approval.requested`, which its resume path replays.
    fn approval_detail(
        &self,
        session_id: SessionId,
        token: &str,
    ) -> impl Future<Output = Result<Option<String>, ApiError>> + Send;
    fn fork_session(
        &self,
        req: ForkSession,
    ) -> impl Future<Output = Result<SessionSnapshot, ApiError>> + Send;
    fn rewind(
        &self,
        req: RewindSession,
    ) -> impl Future<Output = Result<RewindResult, ApiError>> + Send;
}

/// In-memory transport that delegates to kernel session/turn/ledger services.
#[derive(Clone, Debug)]
pub struct InProcessKernelClient {
    ledger: EventLedger,
    journal: OperationJournal,
    sessions: SessionService,
    turns: TurnSubmissionGuard,
    runtime: Arc<Runtime>,
}

/// Submit a foreground turn at `expected_seq`. `text` is the user's own
/// composed message that starts the turn; bounded to [`MAX_TURN_TEXT_BYTES`]
/// (truncated at a UTF-8 boundary, never rejected — this is display text,
/// not a security boundary).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmitTurn {
    session_id: SessionId,
    expected_seq: u64,
    actor: ActorRef,
    trace_id: TraceId,
    text: String,
}

/// Cap on `SubmitTurn::text`, matching `crates/tui`'s own composer bound
/// (`MAX_COMPOSER_BYTES`) — kernel does not depend on tui, so this is a
/// parallel constant rather than a shared one.
pub const MAX_TURN_TEXT_BYTES: usize = 32 * 1024;

/// Bound on the read-then-append retry loops that record a turn's terminal
/// event — `interrupt_sync`'s and `finish_turn`'s — against a concurrent
/// writer to the same session (see their doc comments). Each attempt is a
/// local, fast (sub-millisecond to low-millisecond) SQLite read+append, not
/// a network call, so a generous bound costs little in the rare case it's
/// actually needed.
const MAX_TERMINAL_APPEND_ATTEMPTS: u32 = 20;

/// Truncate `text` to `MAX_TURN_TEXT_BYTES`, backing off to the nearest
/// UTF-8 char boundary so a multibyte character is never split.
pub fn bounded_turn_text(text: &str) -> String {
    if text.len() <= MAX_TURN_TEXT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_TURN_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Accepted foreground turn after `turn.started` is durably committed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnHandle {
    session_id: SessionId,
    turn_id: TurnId,
    seq: u64,
}

/// Interrupt the active foreground turn, if any. Idempotent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Interrupt {
    session_id: SessionId,
    reason: InterruptReason,
    actor: ActorRef,
    trace_id: TraceId,
}

/// Why a client asked the kernel to stop the current turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum InterruptReason {
    ClientRequested,
    /// The turn paused itself: a tool call needs human approval before it may
    /// run. Not an error — the turn's effects so far are durable, its lease is
    /// released while it waits, and resolution (`ResolveApproval`) resumes it.
    ApprovalPending,
}

/// Resume an event subscription after `from_seq` (first event is `from_seq + 1`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscribeEvents {
    session_id: SessionId,
    from_seq: u64,
}

/// Resolve a pending approval. Appends `approval.resolved` at `expected_seq`
/// and marks the durable wait record its token names terminal in the same
/// transaction, so a duplicate, unknown-token or tokenless resolution fails
/// closed before anything is appended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolveApproval {
    session_id: SessionId,
    expected_seq: u64,
    decision: ApprovalDecision,
    actor: ActorRef,
    trace_id: TraceId,
    /// The wait token the pending `approval.requested` carried. Required:
    /// an empty token names no wait and is refused.
    wait_token: String,
    /// Approve-and-remember: the approver wants the grant persisted beyond
    /// this one call. The kernel records the intent on `approval.resolved`;
    /// translating it into a scoped persisted grant is the surface's job
    /// (the grant store is project-level and user-owned).
    remember: bool,
}

/// Decision recorded on `approval.resolved`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ApprovalDecision {
    Approved,
    Denied,
}

/// The manual `Serialize` above writes the decision as its `as_str` wire form
/// ("approved"/"denied"); this is the exact inverse. The derived forms would
/// disagree with it and every read-back of an `approval.resolved` would fail.
impl<'de> Deserialize<'de> for ApprovalDecision {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        match text.as_str() {
            "approved" => Ok(Self::Approved),
            "denied" => Ok(Self::Denied),
            other => Err(serde::de::Error::custom(format!(
                "unknown approval decision: {other}"
            ))),
        }
    }
}

/// Read the committed projection through `to_seq` without mutating the stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RewindSession {
    session_id: SessionId,
    to_seq: u64,
}

/// Historical (or current) projection at the requested sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindResult {
    snapshot: SessionSnapshot,
    through_seq: u64,
    current_seq: u64,
}

/// Bounded live/replay stream of committed session events.
pub struct EventStream {
    inner: LedgerEventStream,
}

impl fmt::Debug for EventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream")
            .field("cursor", &self.inner.cursor())
            .finish()
    }
}

/// Terminal failures after [`KernelClient::subscribe`] succeeds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventStreamError {
    Cancelled { resume_cursor: u64 },
    Lagged { resume_cursor: u64 },
    NotFound { session_id: SessionId },
    StorageCorrupt,
    Internal,
}

struct Runtime {
    sessions: Mutex<HashMap<SessionId, SessionRuntime>>,
}

struct SessionRuntime {
    cancel: CancelToken,
    live_turn: Option<LiveTurn>,
}

struct LiveTurn {
    /// Occupancy guard; released when this value is dropped.
    #[allow(dead_code)]
    lease: TurnLease,
    cancel: CancelToken,
}

#[derive(Serialize)]
struct TurnStartedPayload {
    turn_id: TurnId,
    text: String,
}

#[derive(Serialize)]
struct TurnInterruptedPayload {
    turn_id: TurnId,
    reason: InterruptReason,
}

#[derive(Serialize)]
struct TurnCompletedPayload {
    turn_id: TurnId,
    /// The assistant's final text, when the turn produced one. Absent for a
    /// turn that ended some other way `TurnCompleted` still legitimately
    /// covers (kernel does not itself interpret this — it is display text
    /// for the frontend's transcript).
    text: Option<String>,
}

#[derive(Serialize)]
struct TurnFailedPayload {
    turn_id: TurnId,
    /// Human-readable failure reason. Kernel does not interpret this; the
    /// caller (the actual turn executor) supplies whatever text it has.
    reason: String,
}

/// How a turn actually executed by the caller (e.g. via
/// `agent_runtime::run_turn`, which `crates/kernel` does not depend on)
/// finished, so [`InProcessKernelClient::finish_turn`] can append the right
/// terminal ledger event and release the turn's lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnOutcome {
    Completed {
        text: Option<String>,
    },
    Failed {
        reason: String,
    },
    Interrupted,
    /// The turn paused on pending human input (a tool approval or a
    /// clarification). Releases the turn's lease exactly like every other
    /// terminal outcome — a waiting turn must never hold the session's
    /// execution slot — and records `turn.interrupted` with the
    /// [`InterruptReason::ApprovalPending`] reason so transcripts and
    /// projections can show the pause as a wait, not a failure.
    Waiting,
}

/// Report how a turn finished. A no-op if this turn's lease was already
/// released by something else (e.g. `interrupt` racing ahead of the caller
/// noticing its own cancellation token) — whichever side observes the live
/// turn first does the real work; the other sees nothing left to finish.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinishTurn {
    session_id: SessionId,
    turn_id: TurnId,
    actor: ActorRef,
    trace_id: TraceId,
    outcome: TurnOutcome,
}

impl FinishTurn {
    pub fn new(
        session_id: SessionId,
        turn_id: TurnId,
        actor: ActorRef,
        trace_id: TraceId,
        outcome: TurnOutcome,
    ) -> Self {
        Self {
            session_id,
            turn_id,
            actor,
            trace_id,
            outcome,
        }
    }
}

impl SubmitTurn {
    pub fn new(
        session_id: SessionId,
        expected_seq: u64,
        actor: ActorRef,
        trace_id: TraceId,
        text: impl Into<String>,
    ) -> Self {
        Self {
            session_id,
            expected_seq,
            actor,
            trace_id,
            text: bounded_turn_text(&text.into()),
        }
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn expected_seq(&self) -> u64 {
        self.expected_seq
    }

    pub fn actor(&self) -> &ActorRef {
        &self.actor
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl TurnHandle {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }
}

impl Interrupt {
    pub fn new(
        session_id: SessionId,
        reason: InterruptReason,
        actor: ActorRef,
        trace_id: TraceId,
    ) -> Self {
        Self {
            session_id,
            reason,
            actor,
            trace_id,
        }
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn reason(&self) -> InterruptReason {
        self.reason
    }

    pub fn actor(&self) -> &ActorRef {
        &self.actor
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }
}

impl InterruptReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClientRequested => "client_requested",
            Self::ApprovalPending => "approval_pending",
        }
    }
}

impl SubscribeEvents {
    pub fn new(session_id: SessionId, from_seq: u64) -> Self {
        Self {
            session_id,
            from_seq,
        }
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn from_seq(&self) -> u64 {
        self.from_seq
    }
}

impl ResolveApproval {
    pub fn new(
        session_id: SessionId,
        expected_seq: u64,
        decision: ApprovalDecision,
        actor: ActorRef,
        trace_id: TraceId,
    ) -> Self {
        Self {
            session_id,
            expected_seq,
            decision,
            actor,
            trace_id,
            wait_token: String::new(),
            remember: false,
        }
    }

    /// Name the wait token of the pending `approval.requested` this resolves,
    /// so the durable wait record is marked terminal (duplicate resolutions
    /// of the same token then fail closed). Required: a resolution without
    /// one is refused, since an `approval.resolved` that names nothing
    /// closes no request and is a malformed event to every projection.
    pub fn with_wait_token(mut self, wait_token: impl Into<String>) -> Self {
        self.wait_token = bounded_payload_str(&wait_token.into(), MAX_APPROVAL_TOKEN_BYTES);
        self
    }

    /// Approve-and-remember: record that the approver wants a persisted,
    /// scoped grant for this tool/scope beyond the one call.
    pub const fn remembering(mut self) -> Self {
        self.remember = true;
        self
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn expected_seq(&self) -> u64 {
        self.expected_seq
    }

    pub fn decision(&self) -> ApprovalDecision {
        self.decision
    }

    pub fn actor(&self) -> &ActorRef {
        &self.actor
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub fn wait_token(&self) -> &str {
        &self.wait_token
    }

    pub const fn remember(&self) -> bool {
        self.remember
    }
}

/// Byte cap on a wait token. Tokens are minted by the requesting surface
/// (`approval-<ulid>`-shaped); the cap rejects runaway inputs at the boundary
/// without being a security boundary itself.
pub const MAX_APPROVAL_TOKEN_BYTES: usize = 128;

/// Byte caps for the call/tool identity fields of an `approval.requested` —
/// kernel-local bounds (kernel does not depend on agent-runtime, whose own
/// constants bound the same fields at their source).
const MAX_APPROVAL_CALL_ID_BYTES: usize = 128;
const MAX_APPROVAL_TOOL_BYTES: usize = 128;

/// Byte caps for the human-facing fields of an `approval.requested`. The diff
/// preview and the suspension detail dominate; the detail cap matches
/// `agent_runtime::MAX_SUSPENSION_HISTORY_BYTES` plus JSON overhead.
pub const MAX_APPROVAL_SUMMARY_BYTES: usize = 512;
pub const MAX_APPROVAL_SCOPE_BYTES: usize = 512;
pub const MAX_APPROVAL_SCOPE_ENTRIES: usize = 16;
pub const MAX_APPROVAL_DIFF_BYTES: usize = 16 * 1024;
pub const MAX_APPROVAL_DETAIL_BYTES: usize = 384 * 1024;

fn bounded_payload_str(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Record a pending approval: append `approval.requested` with everything a
/// surface needs to present the call (action summary, scope, bounded diff)
/// plus the suspended-turn detail the resume path replays, and create the
/// durable wait row. The turn that requested it is already finishing —
/// recording a pending approval never holds the execution lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordApproval {
    session_id: SessionId,
    expected_seq: u64,
    actor: ActorRef,
    trace_id: TraceId,
    token: String,
    call_id: String,
    tool: String,
    summary: String,
    scope: Vec<String>,
    diff: String,
    detail: String,
    source: Option<String>,
    arguments_digest: Option<String>,
    remember_as: Option<String>,
}

impl RecordApproval {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: SessionId,
        expected_seq: u64,
        actor: ActorRef,
        trace_id: TraceId,
        token: impl Into<String>,
        call_id: impl Into<String>,
        tool: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            session_id,
            expected_seq,
            actor,
            trace_id,
            token: bounded_payload_str(&token.into(), MAX_APPROVAL_TOKEN_BYTES),
            call_id: bounded_payload_str(&call_id.into(), MAX_APPROVAL_CALL_ID_BYTES),
            tool: bounded_payload_str(&tool.into(), MAX_APPROVAL_TOOL_BYTES),
            summary: bounded_payload_str(&summary.into(), MAX_APPROVAL_SUMMARY_BYTES),
            scope: Vec::new(),
            diff: String::new(),
            detail: String::new(),
            source: None,
            arguments_digest: None,
            remember_as: None,
        }
    }

    pub fn with_scope(mut self, scope: Vec<String>) -> Self {
        self.scope = scope
            .into_iter()
            .take(MAX_APPROVAL_SCOPE_ENTRIES)
            .map(|entry| bounded_payload_str(&entry, MAX_APPROVAL_SCOPE_BYTES))
            .collect();
        self
    }

    pub fn with_diff(mut self, diff: impl Into<String>) -> Self {
        self.diff = bounded_payload_str(&diff.into(), MAX_APPROVAL_DIFF_BYTES);
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = bounded_payload_str(&detail.into(), MAX_APPROVAL_DETAIL_BYTES);
        self
    }

    /// Who raised the ask when it was not the permission lattice — a hook
    /// (`hook:pre_tool_use[0]#<12-hex command digest>`), a plan proposal, an
    /// elicitation. Absent for
    /// a lattice `Ask`, which is what every request was before the field
    /// existed; bounded like the summary.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(bounded_payload_str(
            &source.into(),
            MAX_APPROVAL_SUMMARY_BYTES,
        ));
        self
    }

    /// SHA-256 (lowercase hex) of the exact arguments the human is shown, so
    /// the resume can prove the call it runs is the call that was approved
    /// and not a later rewrite of it.
    pub fn with_arguments_digest(mut self, digest: impl Into<String>) -> Self {
        self.arguments_digest = Some(bounded_payload_str(&digest.into(), 64));
        self
    }

    /// The persisted grant that would answer the same call from then on,
    /// set by the surface that raised the ask only when it has one. Never
    /// cut to fit: a shortened pattern could name another tool, so one over
    /// the bound is dropped, as if there were none.
    pub fn with_remember_as(mut self, pattern: impl Into<String>) -> Self {
        let pattern = pattern.into();
        self.remember_as =
            (!pattern.is_empty() && pattern.len() <= MAX_APPROVAL_SUMMARY_BYTES).then_some(pattern);
        self
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn expected_seq(&self) -> u64 {
        self.expected_seq
    }

    pub fn actor(&self) -> &ActorRef {
        &self.actor
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn scope(&self) -> &[String] {
        &self.scope
    }

    pub fn diff(&self) -> &str {
        &self.diff
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// Wire payload of `approval.requested`. `Serialize` for the append,
/// `Deserialize` for the pending-approval reader and a restarted process
/// reconstructing what it must still ask a human about.
/// `id` is the wait token; the TUI's approval projection keys off an
/// `id`/`approval_id` payload field, so the token travels under that name.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequestedPayload {
    pub id: String,
    pub call_id: String,
    pub tool: String,
    pub summary: String,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub diff: String,
    /// Opaque-to-the-kernel suspended-turn detail: the requesting surface's
    /// own serialized resume state.
    #[serde(default)]
    pub detail: String,
    /// Who raised the ask when it was not the permission lattice (a hook,
    /// a plan, an elicitation). Absent on every request recorded before the
    /// field existed and on a lattice `Ask` today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// SHA-256 of the arguments the human was shown (`approval.requested`
    /// carries the summary and diff, not the raw arguments); the resume
    /// refuses to treat a call with different arguments as approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_digest: Option<String>,
    /// The persisted grant pattern (`Tool` or `Tool(subject)`) that would
    /// answer this same call from then on, as the asking surface found it.
    /// Absent when it found none, and on every request recorded before the
    /// field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remember_as: Option<String>,
}

/// Wire payload of `approval.resolved`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApprovalResolvedPayload {
    pub decision: ApprovalDecision,
    /// The wait token this resolves, also carried as `id` — the field the
    /// TUI's approval projection keys on, so a resolution closes the modal
    /// the request opened.
    #[serde(default)]
    pub wait_token: String,
    #[serde(default)]
    pub remember: bool,
    #[serde(default)]
    pub id: String,
}

/// One unresolved `approval.requested`, read back from the ledger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingApproval {
    seq: u64,
    payload: ApprovalRequestedPayload,
}

impl PendingApproval {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn payload(&self) -> &ApprovalRequestedPayload {
        &self.payload
    }

    pub fn token(&self) -> &str {
        &self.payload.id
    }
}

impl ApprovalDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
        }
    }
}

impl RewindSession {
    pub fn new(session_id: SessionId, to_seq: u64) -> Self {
        Self { session_id, to_seq }
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn to_seq(&self) -> u64 {
        self.to_seq
    }
}

impl RewindResult {
    pub fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }

    pub fn through_seq(&self) -> u64 {
        self.through_seq
    }

    pub fn current_seq(&self) -> u64 {
        self.current_seq
    }
}

/// One exported ledger event (P10-027). Bounded fields only.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportedEvent {
    pub seq: u64,
    pub kind: String,
    pub recorded_at: String,
    pub payload_json: String,
}

impl InProcessKernelClient {
    /// Open (or create) a file-backed ledger and wrap it as the in-process client.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ApiError> {
        let trace = TraceId::new();
        let ledger = EventLedger::open(path).map_err(|err| ledger_api(err, trace))?;
        Ok(Self::from_ledger(ledger))
    }

    pub fn from_ledger(ledger: EventLedger) -> Self {
        let sessions = SessionService::new(ledger.clone());
        let turns = TurnSubmissionGuard::new(sessions.clone());
        let journal = OperationJournal::new(ledger.clone());
        Self {
            ledger,
            journal,
            sessions,
            turns,
            runtime: Arc::new(Runtime {
                sessions: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn create_session_sync(&self, req: CreateSession) -> Result<SessionSnapshot, ApiError> {
        let trace = req.trace_id();
        let snapshot = self
            .sessions
            .create_session(req, &live())
            .map_err(|err| session_api(err, trace))?;
        self.register_session(snapshot.id(), trace)?;
        Ok(snapshot)
    }

    fn get_session_sync(&self, id: SessionId) -> Result<SessionSnapshot, ApiError> {
        let trace = TraceId::new();
        self.sessions
            .get_session(id, &live())
            .map_err(|err| session_api(err, trace))
    }

    fn submit_turn_sync(&self, req: SubmitTurn) -> Result<TurnHandle, ApiError> {
        let trace = req.trace_id;
        let session_cancel = self.register_session(req.session_id, trace)?;
        let lease = self
            .turns
            .begin_turn(req.session_id, req.expected_seq, &live())
            .map_err(|err| turn_api(err, trace))?;
        let turn_id = lease.turn_id();
        let turn_cancel = match CancellationTree::child(&session_cancel, CancelOwner::Turn(turn_id))
        {
            Ok(token) => token,
            Err(_) => {
                lease.cancel();
                return Err(api_error(
                    ErrorCode::InternalUnexpected,
                    UNKNOWN_INTERNAL_MESSAGE,
                    trace,
                ));
            }
        };

        let options = AppendOptions {
            redaction: RedactionClass::Project,
            trace_id: req.trace_id,
            expected_seq: Some(req.expected_seq),
        };
        let envelope = match self.ledger.append(
            req.session_id,
            req.actor,
            EventKind::TurnStarted,
            TurnStartedPayload {
                turn_id,
                text: req.text,
            },
            &options,
            &ledger_live(),
        ) {
            Ok(envelope) => envelope,
            Err(err) => {
                lease.cancel();
                return Err(ledger_api(err, trace));
            }
        };

        self.store_live_turn(
            req.session_id,
            LiveTurn {
                lease,
                cancel: turn_cancel,
            },
        );
        Ok(TurnHandle {
            session_id: req.session_id,
            turn_id,
            seq: envelope.seq(),
        })
    }

    /// The session's highest committed `seq` — one ledger query, where
    /// `get_session` replays every event to rebuild the projection. For a
    /// caller that only needs to know whether it has seen everything.
    pub fn session_tip(&self, session_id: SessionId) -> Result<u64, ApiError> {
        let trace = TraceId::new();
        self.ledger
            .last_seq(session_id, &ledger_live())
            .map_err(|err| ledger_api(err, trace))
    }

    /// The cancellation token for the turn currently live on `session_id`,
    /// if any. `crates/kernel` has no dependency on any execution engine
    /// (e.g. `agent-runtime`), so a caller that actually runs the turn on
    /// another thread reads this to bridge kernel's own cancellation into
    /// whatever token type that engine expects — mirroring how `Interrupt`
    /// (Ctrl-C) already cancels it today.
    pub fn turn_cancel_token(&self, session_id: SessionId) -> Option<CancelToken> {
        let sessions = lock_sessions(&self.runtime.sessions);
        sessions
            .get(&session_id)
            .and_then(|runtime| runtime.live_turn.as_ref())
            .map(|turn| turn.cancel.clone())
    }

    /// Append one non-terminal progress event for a turn's own execution
    /// (e.g. a model step or tool call starting/finishing). Does not touch
    /// occupancy/lease state — only [`finish_turn`](Self::finish_turn) does.
    /// Appended at the session's current tip (`expected_seq: None`): this can
    /// run concurrently with other writers to the same session (an
    /// `Interrupt` racing in, another progress event from the same turn),
    /// and each append is its own atomic, serialized ledger transaction, so
    /// there is no lost update to guard against with an optimistic check
    /// here the way session-authority writes (`submit_turn`/`interrupt`)
    /// need one.
    pub fn append_turn_progress<P: Serialize>(
        &self,
        session_id: SessionId,
        actor: &ActorRef,
        trace_id: TraceId,
        kind: EventKind,
        payload: P,
    ) -> Result<(), ApiError> {
        let options = AppendOptions {
            redaction: RedactionClass::Project,
            trace_id,
            expected_seq: None,
        };
        self.ledger
            .append(
                session_id,
                actor.clone(),
                kind,
                payload,
                &options,
                &ledger_live(),
            )
            .map(|_| ())
            .map_err(|err| ledger_api(err, trace_id))
    }

    /// Record how a turn finished and release its lease. A no-op — `Ok(())`,
    /// nothing appended — if the lease was already released by something
    /// else (the `Interrupt`/Ctrl-C path already appends `turn.interrupted`
    /// and releases the lease itself, and may well win this race, since it
    /// runs on the frontend's own input-handling thread rather than waiting
    /// on the turn's execution to actually notice cancellation).
    ///
    /// Taking the lease and appending the terminal event are two steps, and
    /// `interrupt_sync` appends *before* it takes: this call could take the
    /// lease, lose the race to that append, and then record a second
    /// terminal event — `turn.interrupted` followed by `turn.completed` for
    /// one turn. The projection refuses the second (`TurnNotActive`), and
    /// since every read of the session replays its events, that made the
    /// session permanently unreadable ("Session store is corrupt") after a
    /// Ctrl-C that landed as a fast turn was finishing. So the append here
    /// is made only while the projection still shows this turn active, at
    /// its `expected_seq`; a `SequenceConflict` re-reads, and a turn no
    /// longer active means its terminal event is already recorded.
    pub fn finish_turn(&self, req: FinishTurn) -> Result<(), ApiError> {
        let Some(live) = self.take_live_turn(req.session_id) else {
            return Ok(());
        };
        if live.lease.turn_id() != req.turn_id {
            // Should not be reachable in this single-live-turn-per-session
            // design (a new turn cannot start while this one's lease is
            // still held), but never release a lease this call did not
            // actually finish.
            self.store_live_turn(req.session_id, live);
            return Ok(());
        }
        let append_result = self.append_terminal(&req);
        // The lease is released regardless of whether the ledger append
        // above succeeded: a stuck lease (the original bug this exists to
        // fix) is worse than a turn whose terminal ledger event is missing
        // because of a real storage error — occupancy must not survive a
        // finished turn.
        live.lease.complete();
        append_result
    }

    /// `finish_turn`'s append: the terminal event for `req.outcome`, only
    /// while the projection still shows `req.turn_id` active, at the seq it
    /// was read at. `Ok(())` without appending once the turn is no longer
    /// active — another path recorded its end first.
    fn append_terminal(&self, req: &FinishTurn) -> Result<(), ApiError> {
        let trace = req.trace_id;
        for _ in 0..MAX_TERMINAL_APPEND_ATTEMPTS {
            let snapshot = self
                .sessions
                .get_session(req.session_id, &live())
                .map_err(|err| session_api(err, trace))?;
            if snapshot.active_turn() != Some(req.turn_id) {
                return Ok(());
            }
            let options = AppendOptions {
                redaction: RedactionClass::Project,
                trace_id: trace,
                expected_seq: Some(snapshot.seq()),
            };
            let appended: Result<(), LedgerError> = match &req.outcome {
                TurnOutcome::Completed { text } => self
                    .ledger
                    .append(
                        req.session_id,
                        req.actor.clone(),
                        EventKind::TurnCompleted,
                        TurnCompletedPayload {
                            turn_id: req.turn_id,
                            text: text.clone(),
                        },
                        &options,
                        &ledger_live(),
                    )
                    .map(|_| ()),
                TurnOutcome::Failed { reason } => self
                    .ledger
                    .append(
                        req.session_id,
                        req.actor.clone(),
                        EventKind::TurnFailed,
                        TurnFailedPayload {
                            turn_id: req.turn_id,
                            reason: reason.clone(),
                        },
                        &options,
                        &ledger_live(),
                    )
                    .map(|_| ()),
                TurnOutcome::Interrupted | TurnOutcome::Waiting => self
                    .ledger
                    .append(
                        req.session_id,
                        req.actor.clone(),
                        EventKind::TurnInterrupted,
                        TurnInterruptedPayload {
                            turn_id: req.turn_id,
                            reason: match &req.outcome {
                                TurnOutcome::Waiting => InterruptReason::ApprovalPending,
                                _ => InterruptReason::ClientRequested,
                            },
                        },
                        &options,
                        &ledger_live(),
                    )
                    .map(|_| ()),
            };
            match appended {
                Ok(_) => return Ok(()),
                Err(LedgerError::SequenceConflict { .. }) => continue,
                Err(err) => return Err(ledger_api(err, trace)),
            }
        }
        Err(api_error(
            ErrorCode::SessionConflict,
            "Session conflict",
            trace,
        ))
    }

    fn interrupt_sync(&self, req: Interrupt) -> Result<(), ApiError> {
        let trace = req.trace_id;
        self.cancel_live_turn(req.session_id);

        // Read-then-append against `expected_seq` is optimistic concurrency:
        // a concurrent writer to the same session can win the race between
        // the read and this append, producing `SequenceConflict`. That
        // writer is routinely a live turn's own `append_turn_progress`
        // calls (one per model step / tool-call transition) — a tool-heavy
        // turn now appends continuously, so a single-attempt retry (the
        // original shape here) leaves a real, not-just-theoretical window
        // where this call gives up while the turn is still genuinely being
        // interrupted, stranding its lease with no terminal event ever
        // recorded — an adversarial review of the interactive turn-
        // execution feature traced this precisely. Retried in a bounded
        // loop, re-reading the snapshot fresh each attempt, instead.
        for attempt in 0..MAX_TERMINAL_APPEND_ATTEMPTS {
            let snapshot = self
                .sessions
                .get_session(req.session_id, &live())
                .map_err(|err| session_api(err, trace))?;
            let Some(turn_id) = snapshot.active_turn() else {
                self.take_live_turn(req.session_id);
                return Ok(());
            };
            let options = AppendOptions {
                redaction: RedactionClass::Project,
                trace_id: req.trace_id,
                expected_seq: Some(snapshot.seq()),
            };
            match self.ledger.append(
                req.session_id,
                req.actor.clone(),
                EventKind::TurnInterrupted,
                TurnInterruptedPayload {
                    turn_id,
                    reason: req.reason,
                },
                &options,
                &ledger_live(),
            ) {
                Ok(_) => {
                    self.take_live_turn(req.session_id);
                    return Ok(());
                }
                Err(LedgerError::SequenceConflict { .. }) => {
                    let _ = attempt;
                    continue;
                }
                Err(err) => return Err(ledger_api(err, trace)),
            }
        }
        Err(api_error(
            ErrorCode::SessionConflict,
            "Session conflict",
            trace,
        ))
    }

    /// P10-022: cross-session listing for the sessions inspector/CLI.
    pub fn list_sessions(
        &self,
        _cancel: &CancellationToken,
    ) -> Result<Vec<event_ledger::ledger::SessionSummary>, ApiError> {
        let trace = TraceId::new();
        let live = ledger_live();
        self.ledger
            .list_sessions(&live)
            .map_err(|err| ledger_api(err, trace))
    }

    /// P10-027: bounded event export for one session, oldest first. Payload
    /// stays the stored JSON text; nothing here is a capability grant.
    pub fn export_events(
        &self,
        session: SessionId,
        cancel: &CancellationToken,
    ) -> Result<Vec<ExportedEvent>, ApiError> {
        const MAX_EXPORT_EVENTS: usize = 10_000;
        let trace = TraceId::new();
        if cancel.is_cancelled() {
            return Err(api_error(ErrorCode::InternalUnexpected, "cancelled", trace));
        }
        let live = ledger_live();
        let last = self
            .ledger
            .last_seq(session, &live)
            .map_err(|err| ledger_api(err, trace))?;
        if last as usize > MAX_EXPORT_EVENTS {
            return Err(api_error(
                ErrorCode::SessionConflict,
                "Export bound exceeded",
                trace,
            ));
        }
        let mut out = Vec::new();
        for seq in 1..=last {
            let env = self
                .ledger
                .get(session, seq, &live)
                .map_err(|err| ledger_api(err, trace))?;
            out.push(ExportedEvent {
                seq: env.seq(),
                kind: env.kind().as_str().to_owned(),
                recorded_at: env.recorded_at().as_str().to_owned(),
                payload_json: serde_json::to_string(env.payload()).map_err(|_| {
                    api_error(ErrorCode::InternalUnexpected, "export encode", trace)
                })?,
            });
        }
        Ok(out)
    }

    fn subscribe_sync(&self, req: SubscribeEvents) -> Result<EventStream, ApiError> {
        let trace = TraceId::new();
        let last = self
            .ledger
            .last_seq(req.session_id, &ledger_live())
            .map_err(|err| ledger_api(err, trace))?;
        if req.from_seq > last {
            return Err(api_error(
                ErrorCode::SessionConflict,
                "Session conflict",
                trace,
            ));
        }
        let inner = self
            .ledger
            .subscribe(req.session_id, req.from_seq, &ledger_live())
            .map_err(|err| ledger_api(err, trace))?;
        Ok(EventStream { inner })
    }

    fn approve_sync(&self, req: ResolveApproval) -> Result<(), ApiError> {
        let trace = req.trace_id;
        self.sessions
            .get_session(req.session_id, &live())
            .map_err(|err| session_api(err, trace))?;
        let options = AppendOptions {
            redaction: RedactionClass::Project,
            trace_id: req.trace_id,
            expected_seq: Some(req.expected_seq),
        };
        let payload = ApprovalResolvedPayload {
            decision: req.decision,
            wait_token: req.wait_token.clone(),
            remember: req.remember,
            id: req.wait_token.clone(),
        };
        // The wait turns terminal in the transaction that appends its
        // `approval.resolved`: an unknown token or an already-resolved wait
        // fails before anything is appended, so one wait is never answered
        // twice in the ledger. `expected_seq` alone cannot catch the
        // duplicate — a second resolver that read the tip after the first
        // one's event carries a current `expected_seq`. An empty token names
        // no wait and is refused the same way: a tokenless resolution closes
        // no request, and projections keyed on the id reject it as malformed.
        let state = match req.decision {
            ApprovalDecision::Approved => WaitState::Approved,
            ApprovalDecision::Denied => WaitState::Denied,
        };
        self.journal
            .resolve_wait_with_event(
                req.session_id,
                &req.wait_token,
                state,
                req.actor,
                EventKind::ApprovalResolved,
                payload,
                &options,
                &journal_live(),
            )
            .map_err(|err| match err {
                JournalError::WaitNotFound { .. } => api_error(
                    ErrorCode::SessionNotFound,
                    "no pending approval waits under that token",
                    trace,
                ),
                JournalError::Conflict => api_error(
                    ErrorCode::SessionConflict,
                    "approval already resolved",
                    trace,
                ),
                JournalError::Ledger(err) => ledger_api(err, trace),
                other => journal_api(other, trace),
            })?;
        Ok(())
    }

    fn record_approval_sync(&self, req: RecordApproval) -> Result<(), ApiError> {
        let trace = req.trace_id;
        self.sessions
            .get_session(req.session_id, &live())
            .map_err(|err| session_api(err, trace))?;
        // The wait row exists before the event: a duplicate token fails
        // closed here rather than after an event was already published.
        self.journal
            .request_wait(req.session_id, req.token.clone(), &journal_live())
            .map_err(|err| journal_api(err, trace))?;
        let options = AppendOptions {
            redaction: RedactionClass::Project,
            trace_id: req.trace_id,
            expected_seq: Some(req.expected_seq),
        };
        let payload = ApprovalRequestedPayload {
            id: req.token.clone(),
            call_id: req.call_id.clone(),
            tool: req.tool.clone(),
            summary: req.summary.clone(),
            scope: req.scope.clone(),
            diff: req.diff.clone(),
            detail: req.detail.clone(),
            source: req.source.clone(),
            arguments_digest: req.arguments_digest.clone(),
            remember_as: req.remember_as.clone(),
        };
        if let Err(err) = self.ledger.append(
            req.session_id,
            req.actor,
            EventKind::ApprovalRequested,
            payload,
            &options,
            &ledger_live(),
        ) {
            // The event did not land; release the wait row so a retry of the
            // same token is not blocked by a half-recorded request.
            let _ = self.journal.resolve_wait(
                req.session_id,
                &req.token,
                WaitState::Expired,
                &journal_live(),
            );
            return Err(ledger_api(err, trace));
        }
        Ok(())
    }

    /// The newest suspension detail recorded for `token`'s pending approval.
    /// Progress events for the same call can be appended more than once (a
    /// resumed turn that suspends again on the same call); newest wins.
    fn approval_detail_sync(
        &self,
        session_id: SessionId,
        token: &str,
    ) -> Result<Option<String>, ApiError> {
        let trace = TraceId::new();
        let last = self
            .ledger
            .last_seq(session_id, &ledger_live())
            .map_err(|err| ledger_api(err, trace))?;
        let mut detail = None;
        for seq in 1..=last {
            let event = self
                .ledger
                .get(session_id, seq, &ledger_live())
                .map_err(|err| ledger_api(err, trace))?;
            if event.kind() == EventKind::ToolApprovalRequired {
                let payload = event.payload();
                let event_token = payload.get("approval_token").and_then(Value::as_str);
                if event_token == Some(token) {
                    detail = payload
                        .get("suspension")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
        }
        Ok(detail)
    }

    /// One committed event by sequence. The queue-restore and suspension
    /// readers replay sessions event-by-event; this is their read path.
    pub fn read_event(
        &self,
        session_id: SessionId,
        seq: u64,
    ) -> Result<ErasedEventEnvelope, ApiError> {
        self.ledger
            .get(session_id, seq, &ledger_live())
            .map_err(|err| ledger_api(err, TraceId::new()))
    }

    /// Every `approval.requested` still awaiting resolution, in ledger order.
    /// Resolution is paired by wait token, so an approval resolved under a
    /// different token leaves its request listed — the same rule the wait
    /// table enforces.
    fn pending_approvals_sync(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<PendingApproval>, ApiError> {
        let trace = TraceId::new();
        let last = self
            .ledger
            .last_seq(session_id, &ledger_live())
            .map_err(|err| ledger_api(err, trace))?;
        let mut pending: Vec<PendingApproval> = Vec::new();
        for seq in 1..=last {
            let event = self
                .ledger
                .get(session_id, seq, &ledger_live())
                .map_err(|err| ledger_api(err, trace))?;
            if event.kind() == EventKind::ApprovalRequested {
                if let Ok(payload) =
                    serde_json::from_value::<ApprovalRequestedPayload>(event.payload().clone())
                {
                    pending.push(PendingApproval { seq, payload });
                    let _ = &pending;
                }
            } else if event.kind() == EventKind::ApprovalResolved
                && let Ok(resolved) =
                    serde_json::from_value::<ApprovalResolvedPayload>(event.payload().clone())
            {
                pending.retain(|request| request.payload.id != resolved.wait_token);
            }
        }
        Ok(pending)
    }

    fn fork_session_sync(&self, req: ForkSession) -> Result<SessionSnapshot, ApiError> {
        let trace = req.trace_id();
        let snapshot = self
            .sessions
            .fork_session(req, &live())
            .map_err(|err| session_api(err, trace))?;
        self.register_session(snapshot.id(), trace)?;
        Ok(snapshot)
    }

    fn rewind_sync(&self, req: RewindSession) -> Result<RewindResult, ApiError> {
        let trace = TraceId::new();
        if req.to_seq == 0 {
            return Err(api_error(
                ErrorCode::SessionNotFound,
                "Session not found",
                trace,
            ));
        }
        if req.to_seq > MAX_REPLAY_EVENTS as u64 {
            return Err(api_error(
                ErrorCode::InternalUnexpected,
                UNKNOWN_INTERNAL_MESSAGE,
                trace,
            ));
        }
        let last = self
            .ledger
            .last_seq(req.session_id, &ledger_live())
            .map_err(|err| ledger_api(err, trace))?;
        if last == 0 || req.to_seq > last {
            return Err(api_error(
                ErrorCode::SessionNotFound,
                "Session not found",
                trace,
            ));
        }

        let mut snapshot = None;
        for seq in 1..=req.to_seq {
            let event = self
                .ledger
                .get(req.session_id, seq, &ledger_live())
                .map_err(|err| ledger_api(err, trace))?;
            snapshot = Some(apply(snapshot, &event).map_err(|err| projection_api(err, trace))?);
        }
        let snapshot = snapshot
            .ok_or_else(|| api_error(ErrorCode::SessionNotFound, "Session not found", trace))?;
        Ok(RewindResult {
            snapshot,
            through_seq: req.to_seq,
            current_seq: last,
        })
    }

    fn register_session(
        &self,
        session_id: SessionId,
        trace: TraceId,
    ) -> Result<CancelToken, ApiError> {
        let mut sessions = lock_sessions(&self.runtime.sessions);
        if let Some(existing) = sessions.get(&session_id) {
            return Ok(existing.cancel.clone());
        }
        let cancel = CancellationTree::root(CancelOwner::Session(session_id)).map_err(|_| {
            api_error(
                ErrorCode::InternalUnexpected,
                UNKNOWN_INTERNAL_MESSAGE,
                trace,
            )
        })?;
        sessions.insert(
            session_id,
            SessionRuntime {
                cancel: cancel.clone(),
                live_turn: None,
            },
        );
        Ok(cancel)
    }

    fn store_live_turn(&self, session_id: SessionId, live_turn: LiveTurn) {
        let mut sessions = lock_sessions(&self.runtime.sessions);
        if let Some(runtime) = sessions.get_mut(&session_id) {
            runtime.live_turn = Some(live_turn);
        }
    }

    fn cancel_live_turn(&self, session_id: SessionId) {
        let sessions = lock_sessions(&self.runtime.sessions);
        if let Some(runtime) = sessions.get(&session_id)
            && let Some(turn) = runtime.live_turn.as_ref()
        {
            turn.cancel.cancel();
        }
    }

    fn take_live_turn(&self, session_id: SessionId) -> Option<LiveTurn> {
        let mut sessions = lock_sessions(&self.runtime.sessions);
        sessions
            .get_mut(&session_id)
            .and_then(|runtime| runtime.live_turn.take())
    }
}

impl KernelClient for InProcessKernelClient {
    async fn create_session(&self, req: CreateSession) -> Result<SessionSnapshot, ApiError> {
        self.create_session_sync(req)
    }

    async fn get_session(&self, id: SessionId) -> Result<SessionSnapshot, ApiError> {
        self.get_session_sync(id)
    }

    async fn submit_turn(&self, req: SubmitTurn) -> Result<TurnHandle, ApiError> {
        self.submit_turn_sync(req)
    }

    async fn interrupt(&self, req: Interrupt) -> Result<(), ApiError> {
        self.interrupt_sync(req)
    }

    async fn subscribe(&self, req: SubscribeEvents) -> Result<EventStream, ApiError> {
        self.subscribe_sync(req)
    }

    async fn approve(&self, req: ResolveApproval) -> Result<(), ApiError> {
        self.approve_sync(req)
    }

    async fn record_approval(&self, req: RecordApproval) -> Result<(), ApiError> {
        self.record_approval_sync(req)
    }

    async fn pending_approvals(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<PendingApproval>, ApiError> {
        self.pending_approvals_sync(session_id)
    }

    async fn approval_detail(
        &self,
        session_id: SessionId,
        token: &str,
    ) -> Result<Option<String>, ApiError> {
        self.approval_detail_sync(session_id, token)
    }

    async fn fork_session(&self, req: ForkSession) -> Result<SessionSnapshot, ApiError> {
        self.fork_session_sync(req)
    }

    async fn rewind(&self, req: RewindSession) -> Result<RewindResult, ApiError> {
        self.rewind_sync(req)
    }
}

impl EventStream {
    /// Last seq delivered to this consumer (`from_seq` if nothing received).
    pub fn cursor(&self) -> u64 {
        self.inner.cursor()
    }

    /// Wait for the next committed event after [`Self::cursor`].
    pub fn recv(&mut self) -> Result<ErasedEventEnvelope, EventStreamError> {
        self.inner.recv().map_err(map_stream)
    }

    /// Non-blocking poll. `Ok(None)` means no new committed event is queued.
    pub fn try_recv(&mut self) -> Result<Option<ErasedEventEnvelope>, EventStreamError> {
        self.inner.try_recv().map_err(map_stream)
    }

    /// Stop the worker. Further `recv` returns [`EventStreamError::Cancelled`].
    pub fn close(&mut self) {
        self.inner.close();
    }
}

impl InterruptReason {
    fn serialize_str(self) -> &'static str {
        self.as_str()
    }
}

impl Serialize for InterruptReason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.serialize_str())
    }
}

impl Serialize for ApprovalDecision {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl fmt::Display for InterruptReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ApprovalDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for EventStreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled { resume_cursor } => {
                write!(f, "event subscription cancelled at seq {resume_cursor}")
            }
            Self::Lagged { resume_cursor } => {
                write!(
                    f,
                    "event subscription lagged; reconnect from seq {resume_cursor}"
                )
            }
            Self::NotFound { session_id } => write!(f, "session {session_id} not found"),
            Self::StorageCorrupt => f.write_str("event subscription store is corrupt"),
            Self::Internal => f.write_str("event subscription failed internally"),
        }
    }
}

impl Error for EventStreamError {}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sessions = lock_sessions(&self.sessions);
        f.debug_struct("Runtime")
            .field("sessions", &sessions.len())
            .finish()
    }
}

fn live() -> CancellationToken {
    CancellationToken::new()
}

fn ledger_live() -> event_ledger::ledger::CancellationToken {
    event_ledger::ledger::CancellationToken::new()
}

fn journal_live() -> event_ledger::journal::CancellationToken {
    event_ledger::journal::CancellationToken::new()
}

/// Map a journal (operation/wait table) failure onto the client API error set.
fn journal_api(err: JournalError, trace: TraceId) -> ApiError {
    match err {
        JournalError::Cancelled => api_error(
            ErrorCode::InternalUnexpected,
            UNKNOWN_INTERNAL_MESSAGE,
            trace,
        ),
        JournalError::NotFound { .. } => {
            api_error(ErrorCode::SessionNotFound, "Session not found", trace)
        }
        _ => api_error(
            ErrorCode::InternalUnexpected,
            UNKNOWN_INTERNAL_MESSAGE,
            trace,
        ),
    }
}

fn lock_sessions(
    mutex: &Mutex<HashMap<SessionId, SessionRuntime>>,
) -> MutexGuard<'_, HashMap<SessionId, SessionRuntime>> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn map_stream(err: SubscriptionError) -> EventStreamError {
    match err {
        SubscriptionError::Cancelled { resume_cursor } => {
            EventStreamError::Cancelled { resume_cursor }
        }
        SubscriptionError::Lagged { resume_cursor } => EventStreamError::Lagged { resume_cursor },
        SubscriptionError::Ledger(LedgerError::SessionNotFound { session_id }) => {
            EventStreamError::NotFound { session_id }
        }
        SubscriptionError::Ledger(LedgerError::Corrupt(_))
        | SubscriptionError::Ledger(LedgerError::ForeignKeysDisabled)
        | SubscriptionError::Ledger(LedgerError::InvalidTimestamp) => {
            EventStreamError::StorageCorrupt
        }
        SubscriptionError::Ledger(_) => EventStreamError::Internal,
    }
}

fn session_api(err: SessionError, trace: TraceId) -> ApiError {
    let fallback = ApiError::from_unknown(trace, &err);
    err.into_api_error(trace).unwrap_or(fallback)
}

fn turn_api(err: TurnGuardError, trace: TraceId) -> ApiError {
    let fallback = ApiError::from_unknown(trace, &err);
    err.into_api_error(trace).unwrap_or(fallback)
}

fn projection_api(err: ProjectionError, trace: TraceId) -> ApiError {
    match err {
        ProjectionError::Cancelled => ApiError::from_unknown(trace, &err),
        ProjectionError::TooManyEvents => api_error(
            ErrorCode::InternalUnexpected,
            UNKNOWN_INTERNAL_MESSAGE,
            trace,
        ),
        ProjectionError::Invariant(ProjectionInvariant::SessionNotCreated) => {
            api_error(ErrorCode::SessionNotFound, "Session not found", trace)
        }
        ProjectionError::Invariant(_) => {
            api_error(ErrorCode::StorageCorrupt, "Session store is corrupt", trace)
        }
    }
}

fn ledger_api(err: LedgerError, trace: TraceId) -> ApiError {
    match err {
        LedgerError::SessionNotFound { .. } | LedgerError::EventNotFound { .. } => {
            api_error(ErrorCode::SessionNotFound, "Session not found", trace)
        }
        // Same `ErrorCode` (neither is more or less severe than the other),
        // but two genuinely different situations — creating a session whose
        // id already exists is a client-side collision, while a sequence
        // conflict is the real cross-process dual-writer race `AGT-018`'s
        // fencing exists to catch (`newtask.md` §2.6). Every other `"Session
        // conflict"` message elsewhere in this crate (`turn/guard.rs`,
        // `session/service.rs`, `subscribe_sync` below) already describes
        // this same single scenario consistently and is left alone.
        LedgerError::SessionExists { .. } => api_error(
            ErrorCode::SessionConflict,
            "A session with this id already exists",
            trace,
        ),
        LedgerError::SequenceConflict { .. } => api_error(
            ErrorCode::SessionConflict,
            "Another writer already advanced this session past the expected sequence",
            trace,
        ),
        LedgerError::Corrupt(_)
        | LedgerError::ForeignKeysDisabled
        | LedgerError::InvalidTimestamp
        | LedgerError::Migration(_) => {
            api_error(ErrorCode::StorageCorrupt, "Session store is corrupt", trace)
        }
        _ => ApiError::from_unknown(trace, &err),
    }
}

fn api_error(code: ErrorCode, message: &'static str, trace: TraceId) -> ApiError {
    match ApiError::new(code, message, trace) {
        Ok(err) => err,
        Err(build) => ApiError::from_unknown(trace, &build),
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let sessions = lock_sessions(&self.sessions);
        for runtime in sessions.values() {
            runtime.cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_ledger::event::ActorKind;
    use protocol::{ErrorCode, EventId, ProjectId};
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempClient {
        path: PathBuf,
        client: InProcessKernelClient,
    }

    impl TempClient {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-kernel-client-{}-{seq}.sqlite",
                std::process::id()
            ));
            remove_db_files(&path);
            let client = InProcessKernelClient::open(&path).expect("open client");
            Self { path, client }
        }
    }

    impl Drop for TempClient {
        fn drop(&mut self) {
            remove_db_files(&self.path);
        }
    }

    fn remove_db_files(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(sidecar(path, "-wal"));
        let _ = std::fs::remove_file(sidecar(path, "-shm"));
        let _ = std::fs::remove_file(sidecar(path, "-journal"));
    }

    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut raw = path.as_os_str().to_os_string();
        raw.push(suffix);
        PathBuf::from(raw)
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("in-process kernel future stayed pending"),
        }
    }

    fn actor() -> ActorRef {
        ActorRef::new(ActorKind::Human, &EventId::new().to_string()).expect("actor")
    }

    fn create_req() -> CreateSession {
        CreateSession::new(ProjectId::new(), actor(), TraceId::new())
    }

    #[test]
    fn contract_create_get_subscribe_interrupt_fork() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        assert_eq!(created.seq(), 1);
        assert!(created.active_turn().is_none());

        let loaded = block_on(tmp.client.get_session(created.id())).expect("get");
        assert_eq!(loaded, created);

        let mut stream = block_on(tmp.client.subscribe(SubscribeEvents::new(created.id(), 0)))
            .expect("subscribe");
        let first = stream.recv().expect("created event");
        assert_eq!(first.kind(), EventKind::SessionCreated);
        assert_eq!(first.session_id(), created.id());
        assert_eq!(first.seq(), 1);
        assert_eq!(stream.cursor(), 1);

        block_on(tmp.client.interrupt(Interrupt::new(
            created.id(),
            InterruptReason::ClientRequested,
            actor(),
            TraceId::new(),
        )))
        .expect("interrupt without turn is idempotent");

        let child = block_on(tmp.client.fork_session(ForkSession::new(
            created.id(),
            created.seq(),
            actor(),
            TraceId::new(),
        )))
        .expect("fork");
        assert_ne!(child.id(), created.id());
        assert_eq!(child.project_id(), created.project_id());
        assert_eq!(child.seq(), 1);

        let child_loaded = block_on(tmp.client.get_session(child.id())).expect("get child");
        assert_eq!(child_loaded, child);

        let parent = block_on(tmp.client.get_session(created.id())).expect("parent unchanged");
        assert_eq!(parent, created);
        assert!(tmp.path.exists());
    }

    #[test]
    fn subscribe_replays_then_tails_turn_events() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        let mut stream = block_on(tmp.client.subscribe(SubscribeEvents::new(created.id(), 0)))
            .expect("subscribe");
        assert_eq!(
            stream.recv().expect("created").kind(),
            EventKind::SessionCreated
        );

        let handle = block_on(tmp.client.submit_turn(SubmitTurn::new(
            created.id(),
            created.seq(),
            actor(),
            TraceId::new(),
            "hello",
        )))
        .expect("submit");
        let started = stream.recv().expect("turn.started");
        assert_eq!(started.kind(), EventKind::TurnStarted);
        assert_eq!(started.seq(), handle.seq());
        assert_eq!(
            started.payload().get("turn_id").and_then(|v| v.as_str()),
            Some(handle.turn_id().to_string()).as_deref()
        );

        block_on(tmp.client.interrupt(Interrupt::new(
            created.id(),
            InterruptReason::ClientRequested,
            actor(),
            TraceId::new(),
        )))
        .expect("interrupt");
        let interrupted = stream.recv().expect("turn.interrupted");
        assert_eq!(interrupted.kind(), EventKind::TurnInterrupted);
        assert_eq!(
            interrupted.payload().get("reason").and_then(|v| v.as_str()),
            Some("client_requested")
        );
    }

    #[test]
    fn interrupt_is_idempotent_and_clears_active_turn() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        let handle = block_on(tmp.client.submit_turn(SubmitTurn::new(
            created.id(),
            created.seq(),
            actor(),
            TraceId::new(),
            "hello",
        )))
        .expect("submit");
        let busy = block_on(tmp.client.get_session(created.id())).expect("busy");
        assert_eq!(busy.active_turn(), Some(handle.turn_id()));
        assert_eq!(busy.status(), crate::SessionStatus::Busy);

        let req = Interrupt::new(
            created.id(),
            InterruptReason::ClientRequested,
            actor(),
            TraceId::new(),
        );
        block_on(tmp.client.interrupt(req.clone())).expect("first interrupt");
        block_on(tmp.client.interrupt(req)).expect("second interrupt");

        let ready = block_on(tmp.client.get_session(created.id())).expect("ready");
        assert!(ready.active_turn().is_none());
        assert_eq!(ready.status(), crate::SessionStatus::Ready);
        assert_eq!(ready.seq(), 3);
    }

    #[test]
    fn interrupt_survives_a_concurrent_writer_racing_the_same_session() {
        // `interrupt_sync`'s read-then-append is optimistic concurrency: a
        // concurrent writer to the same session (in production, a busy
        // turn's own `append_turn_progress` calls — one per model step /
        // tool-call transition) can win the race between the read and this
        // append. An adversarial review of the interactive turn-execution
        // feature found the original single-attempt shape gave up on the
        // very first conflict, stranding the turn's lease with no terminal
        // event ever recorded. This drives a real concurrent writer hard
        // enough that `interrupt` almost certainly collides with it at
        // least once, and asserts it still succeeds via the bounded retry.
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        let handle = block_on(tmp.client.submit_turn(SubmitTurn::new(
            created.id(),
            created.seq(),
            actor(),
            TraceId::new(),
            "hello",
        )))
        .expect("submit");

        let writer_client = tmp.client.clone();
        let session_id = created.id();
        let turn_id = handle.turn_id();
        let writer = std::thread::spawn(move || {
            for i in 0..15 {
                let _ = writer_client.append_turn_progress(
                    session_id,
                    &actor(),
                    TraceId::new(),
                    EventKind::ModelRequested,
                    serde_json::json!({"turn_id": turn_id, "i": i}),
                );
            }
        });

        let result = block_on(tmp.client.interrupt(Interrupt::new(
            session_id,
            InterruptReason::ClientRequested,
            actor(),
            TraceId::new(),
        )));
        writer.join().expect("writer thread");

        assert!(
            result.is_ok(),
            "interrupt must survive a concurrent writer to the same session: {result:?}"
        );
        let after = block_on(tmp.client.get_session(session_id)).expect("session");
        assert!(after.active_turn().is_none(), "turn must be interrupted");
    }

    #[test]
    fn an_interrupt_that_lands_while_finish_turn_holds_the_lease_does_not_corrupt_the_session() {
        // The race: `finish_turn` takes the live turn, then `interrupt_sync`
        // — which appends *before* it takes — records `turn.interrupted`,
        // then `finish_turn` records `turn.completed`. Two terminal events
        // for one turn; the projection refuses the second, and every later
        // read of the session replays into that refusal: "Session store is
        // corrupt", forever, from one Ctrl-C. CI's macOS runner hit it in
        // `first_ctrl_c_interrupts_then_second_exits`.
        //
        // The interleaving's end state is built directly: the interrupt's
        // event is appended the way `interrupt_sync` appends it, while the
        // lease is still held so `finish_turn` still believes it owns the
        // turn's end.
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        let handle = block_on(tmp.client.submit_turn(SubmitTurn::new(
            created.id(),
            created.seq(),
            actor(),
            TraceId::new(),
            "hello",
        )))
        .expect("submit");
        let session_id = created.id();
        let turn_id = handle.turn_id();

        tmp.client
            .ledger
            .append(
                session_id,
                actor(),
                EventKind::TurnInterrupted,
                TurnInterruptedPayload {
                    turn_id,
                    reason: InterruptReason::ClientRequested,
                },
                &AppendOptions {
                    redaction: RedactionClass::Project,
                    trace_id: TraceId::new(),
                    expected_seq: Some(handle.seq()),
                },
                &ledger_live(),
            )
            .expect("the interrupt's terminal event");

        tmp.client
            .finish_turn(FinishTurn::new(
                session_id,
                turn_id,
                actor(),
                TraceId::new(),
                TurnOutcome::Completed {
                    text: Some("done".to_owned()),
                },
            ))
            .expect("finish after interrupt is not an error");

        let after = block_on(tmp.client.get_session(session_id))
            .expect("the session is still readable after the race");
        assert!(after.active_turn().is_none());
        let mut stream =
            block_on(tmp.client.subscribe(SubscribeEvents::new(session_id, 0))).expect("subscribe");
        let mut terminals = Vec::new();
        for _ in 0..after.seq() {
            let event = stream.recv().expect("event");
            if matches!(
                event.kind(),
                EventKind::TurnCompleted | EventKind::TurnInterrupted | EventKind::TurnFailed
            ) {
                terminals.push(event.kind());
            }
        }
        assert_eq!(
            terminals,
            vec![EventKind::TurnInterrupted],
            "exactly one terminal event, the one that landed first"
        );
        // And the lease is released: the next turn can start.
        block_on(tmp.client.submit_turn(SubmitTurn::new(
            session_id,
            after.seq(),
            actor(),
            TraceId::new(),
            "again",
        )))
        .expect("a new turn after the race");
    }

    #[test]
    fn submit_turn_stale_seq_is_session_conflict() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        let err = block_on(tmp.client.submit_turn(SubmitTurn::new(
            created.id(),
            0,
            actor(),
            TraceId::new(),
            "hello",
        )))
        .expect_err("stale");
        assert_eq!(err.code(), ErrorCode::SessionConflict);
        assert_eq!(err.code().as_str(), "session.conflict");
        assert!(!err.retryable());
    }

    #[test]
    fn ledger_api_gives_session_exists_and_sequence_conflict_distinct_messages() {
        // Both share `ErrorCode::SessionConflict` (neither is more severe
        // than the other), but they're different situations — a client-side
        // id collision versus the real cross-process dual-writer race
        // `AGT-018`'s fencing exists to catch — so a caller reading the
        // message shouldn't see the same generic text for both.
        let session_id = SessionId::new();
        let trace = TraceId::new();
        let exists = ledger_api(LedgerError::SessionExists { session_id }, trace);
        let conflict = ledger_api(
            LedgerError::SequenceConflict {
                session_id,
                expected: 1,
                actual: 2,
            },
            trace,
        );
        assert_eq!(exists.code(), ErrorCode::SessionConflict);
        assert_eq!(conflict.code(), ErrorCode::SessionConflict);
        assert_ne!(exists.message(), conflict.message());
    }

    #[test]
    fn unknown_session_is_not_found() {
        let tmp = TempClient::create();
        let missing = SessionId::new();
        let err = block_on(tmp.client.get_session(missing)).expect_err("get");
        assert_eq!(err.code(), ErrorCode::SessionNotFound);
        assert_eq!(err.code().as_str(), "session.not_found");

        let err = block_on(tmp.client.subscribe(SubscribeEvents::new(missing, 0)))
            .expect_err("subscribe");
        assert_eq!(err.code(), ErrorCode::SessionNotFound);

        let err = block_on(tmp.client.fork_session(ForkSession::new(
            missing,
            1,
            actor(),
            TraceId::new(),
        )))
        .expect_err("fork");
        assert_eq!(err.code(), ErrorCode::SessionNotFound);
    }

    #[test]
    fn approve_appends_durable_approval_resolved() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        record_pending(&tmp, created.id(), "wait-1");
        block_on(
            tmp.client.approve(
                ResolveApproval::new(
                    created.id(),
                    tip(&tmp, created.id()),
                    ApprovalDecision::Approved,
                    actor(),
                    TraceId::new(),
                )
                .with_wait_token("wait-1"),
            ),
        )
        .expect("approve");
        let loaded = block_on(tmp.client.get_session(created.id())).expect("get");
        assert_eq!(loaded.seq(), 3);
        let mut stream = block_on(tmp.client.subscribe(SubscribeEvents::new(created.id(), 2)))
            .expect("subscribe");
        let event = stream.recv().expect("resolved");
        assert_eq!(event.kind(), EventKind::ApprovalResolved);
        assert_eq!(
            event.payload().get("decision").and_then(|v| v.as_str()),
            Some("approved")
        );
        assert_eq!(
            event.payload().get("id").and_then(|v| v.as_str()),
            Some("wait-1")
        );
    }

    #[test]
    fn tokenless_resolution_is_refused_before_any_append() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        let session = created.id();
        record_pending(&tmp, session, "wait-1");

        let seq = tip(&tmp, session);
        let err = block_on(tmp.client.approve(ResolveApproval::new(
            session,
            seq,
            ApprovalDecision::Approved,
            actor(),
            TraceId::new(),
        )))
        .expect_err("a resolution must name its wait");
        assert_eq!(err.code(), ErrorCode::SessionNotFound);
        assert!(resolutions(&tmp, session).is_empty());
        assert_eq!(tip(&tmp, session), seq);
        let pending = block_on(tmp.client.pending_approvals(session)).expect("pending");
        assert_eq!(pending.len(), 1);
    }

    fn tip(tmp: &TempClient, session: SessionId) -> u64 {
        block_on(tmp.client.get_session(session))
            .expect("session")
            .seq()
    }

    /// Every `approval.resolved` in the session, oldest first.
    fn resolutions(tmp: &TempClient, session: SessionId) -> Vec<ApprovalResolvedPayload> {
        (1..=tip(tmp, session))
            .map(|seq| tmp.client.read_event(session, seq).expect("event"))
            .filter(|event| event.kind() == EventKind::ApprovalResolved)
            .map(|event| serde_json::from_value(event.payload().clone()).expect("payload"))
            .collect()
    }

    fn record_pending(tmp: &TempClient, session: SessionId, token: &str) {
        block_on(tmp.client.record_approval(RecordApproval::new(
            session,
            tip(tmp, session),
            actor(),
            TraceId::new(),
            token,
            "call-1",
            "workspace_write",
            "write notes.txt",
        )))
        .expect("record approval");
    }

    #[test]
    fn duplicate_resolution_is_refused_before_any_append() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        let session = created.id();
        record_pending(&tmp, session, "wait-1");

        let seq = tip(&tmp, session);
        block_on(
            tmp.client.approve(
                ResolveApproval::new(
                    session,
                    seq,
                    ApprovalDecision::Approved,
                    actor(),
                    TraceId::new(),
                )
                .with_wait_token("wait-1"),
            ),
        )
        .expect("first resolution");

        // The second resolver read the tip after the first one's event, so
        // its `expected_seq` is current: only the spent wait can refuse it.
        let seq = tip(&tmp, session);
        let err = block_on(
            tmp.client.approve(
                ResolveApproval::new(
                    session,
                    seq,
                    ApprovalDecision::Denied,
                    actor(),
                    TraceId::new(),
                )
                .with_wait_token("wait-1")
                .remembering(),
            ),
        )
        .expect_err("duplicate resolution");
        assert_eq!(err.code(), ErrorCode::SessionConflict);
        assert_eq!(err.message(), "approval already resolved");

        let resolved = resolutions(&tmp, session);
        assert_eq!(resolved.len(), 1, "exactly one answer: {resolved:?}");
        assert_eq!(resolved[0].decision, ApprovalDecision::Approved);
        assert_eq!(resolved[0].wait_token, "wait-1");
        assert!(!resolved[0].remember);
        assert_eq!(
            tip(&tmp, session),
            seq,
            "the refused resolution appended nothing"
        );
    }

    #[test]
    fn unknown_token_resolution_is_refused_before_any_append() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        let session = created.id();
        record_pending(&tmp, session, "wait-1");

        let seq = tip(&tmp, session);
        let err = block_on(
            tmp.client.approve(
                ResolveApproval::new(
                    session,
                    seq,
                    ApprovalDecision::Approved,
                    actor(),
                    TraceId::new(),
                )
                .with_wait_token("never-requested"),
            ),
        )
        .expect_err("unknown token");
        assert_eq!(err.code(), ErrorCode::SessionNotFound);
        assert!(resolutions(&tmp, session).is_empty());
        assert_eq!(tip(&tmp, session), seq);
        // The real wait is untouched and still resolvable.
        let pending = block_on(tmp.client.pending_approvals(session)).expect("pending");
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn stale_seq_resolution_leaves_the_wait_pending() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        let session = created.id();
        record_pending(&tmp, session, "wait-1");

        let seq = tip(&tmp, session);
        let resolve = |expected_seq| {
            block_on(
                tmp.client.approve(
                    ResolveApproval::new(
                        session,
                        expected_seq,
                        ApprovalDecision::Denied,
                        actor(),
                        TraceId::new(),
                    )
                    .with_wait_token("wait-1"),
                ),
            )
        };
        let err = resolve(seq - 1).expect_err("stale expected_seq");
        assert_eq!(err.code(), ErrorCode::SessionConflict);
        assert_eq!(
            err.message(),
            "Another writer already advanced this session past the expected sequence"
        );
        assert!(resolutions(&tmp, session).is_empty());
        assert_eq!(tip(&tmp, session), seq);

        // The refused append did not spend the wait: a retry at the tip lands.
        resolve(seq).expect("retry at the tip");
        let resolved = resolutions(&tmp, session);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].decision, ApprovalDecision::Denied);
    }

    #[test]
    fn rewind_returns_prefix_projection_without_mutating_stream() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        record_pending(&tmp, created.id(), "wait-1");
        let current = block_on(tmp.client.get_session(created.id())).expect("current");
        assert_eq!(current.seq(), 2);

        let rewound =
            block_on(tmp.client.rewind(RewindSession::new(created.id(), 1))).expect("rewind");
        assert_eq!(rewound.through_seq(), 1);
        assert_eq!(rewound.current_seq(), 2);
        assert_eq!(rewound.snapshot().seq(), 1);
        assert_eq!(rewound.snapshot().id(), created.id());

        let still = block_on(tmp.client.get_session(created.id())).expect("unmutated");
        assert_eq!(still.seq(), 2);
    }

    #[test]
    fn client_open_does_not_require_frontend_db_handle() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        let clone = tmp.client.clone();
        let loaded = block_on(clone.get_session(created.id())).expect("clone get");
        assert_eq!(loaded.id(), created.id());
        let _ = &tmp.client;
    }
}
