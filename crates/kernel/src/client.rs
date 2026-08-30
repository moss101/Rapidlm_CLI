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
use event_ledger::ledger::{AppendOptions, EventLedger, LedgerError};
use event_ledger::subscription::{EventStream as LedgerEventStream, SubscriptionError};
use protocol::{
    ApiError, ErrorCode, RedactionClass, SessionId, TraceId, TurnId, UNKNOWN_INTERNAL_MESSAGE,
};
use serde::Serialize;

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
    sessions: SessionService,
    turns: TurnSubmissionGuard,
    runtime: Arc<Runtime>,
}

/// Submit a foreground turn at `expected_seq`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmitTurn {
    session_id: SessionId,
    expected_seq: u64,
    actor: ActorRef,
    trace_id: TraceId,
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
}

/// Resume an event subscription after `from_seq` (first event is `from_seq + 1`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscribeEvents {
    session_id: SessionId,
    from_seq: u64,
}

/// Resolve a pending approval. Appends `approval.resolved` at `expected_seq`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolveApproval {
    session_id: SessionId,
    expected_seq: u64,
    decision: ApprovalDecision,
    actor: ActorRef,
    trace_id: TraceId,
}

/// Decision recorded on `approval.resolved`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ApprovalDecision {
    Approved,
    Denied,
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
}

#[derive(Serialize)]
struct TurnInterruptedPayload {
    turn_id: TurnId,
    reason: InterruptReason,
}

#[derive(Serialize)]
struct ApprovalResolvedPayload {
    decision: ApprovalDecision,
}

impl SubmitTurn {
    pub fn new(
        session_id: SessionId,
        expected_seq: u64,
        actor: ActorRef,
        trace_id: TraceId,
    ) -> Self {
        Self {
            session_id,
            expected_seq,
            actor,
            trace_id,
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
        }
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
        Self {
            ledger,
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
            TurnStartedPayload { turn_id },
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

    fn interrupt_sync(&self, req: Interrupt) -> Result<(), ApiError> {
        let trace = req.trace_id;
        let snapshot = self
            .sessions
            .get_session(req.session_id, &live())
            .map_err(|err| session_api(err, trace))?;

        self.cancel_live_turn(req.session_id);

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
            req.actor,
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
                Ok(())
            }
            Err(LedgerError::SequenceConflict { .. }) => {
                let again = self
                    .sessions
                    .get_session(req.session_id, &live())
                    .map_err(|err| session_api(err, trace))?;
                if again.active_turn().is_none() {
                    self.take_live_turn(req.session_id);
                    Ok(())
                } else {
                    Err(api_error(
                        ErrorCode::SessionConflict,
                        "Session conflict",
                        trace,
                    ))
                }
            }
            Err(err) => Err(ledger_api(err, trace)),
        }
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
                payload_json: serde_json::to_string(env.payload())
                    .map_err(|_| api_error(ErrorCode::InternalUnexpected, "export encode", trace))?,
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
        self.ledger
            .append(
                req.session_id,
                req.actor,
                EventKind::ApprovalResolved,
                ApprovalResolvedPayload {
                    decision: req.decision,
                },
                &options,
                &ledger_live(),
            )
            .map_err(|err| ledger_api(err, trace))?;
        Ok(())
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
    fn submit_turn_stale_seq_is_session_conflict() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        let err = block_on(tmp.client.submit_turn(SubmitTurn::new(
            created.id(),
            0,
            actor(),
            TraceId::new(),
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
        block_on(tmp.client.approve(ResolveApproval::new(
            created.id(),
            created.seq(),
            ApprovalDecision::Approved,
            actor(),
            TraceId::new(),
        )))
        .expect("approve");
        let loaded = block_on(tmp.client.get_session(created.id())).expect("get");
        assert_eq!(loaded.seq(), 2);
        let mut stream = block_on(tmp.client.subscribe(SubscribeEvents::new(created.id(), 1)))
            .expect("subscribe");
        let event = stream.recv().expect("resolved");
        assert_eq!(event.kind(), EventKind::ApprovalResolved);
        assert_eq!(
            event.payload().get("decision").and_then(|v| v.as_str()),
            Some("approved")
        );
    }

    #[test]
    fn rewind_returns_prefix_projection_without_mutating_stream() {
        let tmp = TempClient::create();
        let created = block_on(tmp.client.create_session(create_req())).expect("create");
        block_on(tmp.client.approve(ResolveApproval::new(
            created.id(),
            created.seq(),
            ApprovalDecision::Denied,
            actor(),
            TraceId::new(),
        )))
        .expect("second event");
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
