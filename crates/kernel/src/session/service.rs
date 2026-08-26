//! Kernel session repository: durable create and projection read.
//!
//! `create_session` inserts the session row, appends `session.created`, then
//! returns the projection at the committed seq. Success is returned only after
//! the ledger transaction commits. `get_session` replays committed events;
//! a missing session is `session.not_found`.

use std::error::Error;
use std::fmt;
use std::path::Path;

use event_ledger::event::{ActorRef, EventKind};
use event_ledger::ledger::{AppendOptions, EventLedger, LedgerError};
use protocol::{
    ApiError, ErrorCode, ProjectId, RedactionClass, SessionId, TraceId, UNKNOWN_INTERNAL_MESSAGE,
};
use serde::Serialize;

use crate::CancellationToken;
use crate::session::projection::{
    MAX_REPLAY_EVENTS, ProjectionError, ProjectionInvariant, SessionSnapshot, apply,
};

const CANCEL_CHECK_EVERY: usize = 32;

/// Request to create a new session in a project.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateSession {
    project_id: ProjectId,
    actor: ActorRef,
    trace_id: TraceId,
}

/// Durable session repository over the event ledger.
#[derive(Clone, Debug)]
pub struct SessionService {
    ledger: EventLedger,
}

/// Typed session repository failure. Public mapping uses [`SessionError::code`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionError {
    Cancelled,
    NotFound { session_id: SessionId },
    Conflict { session_id: SessionId },
    TooManyEvents,
    StorageCorrupt,
    Internal,
}

#[derive(Serialize)]
struct SessionCreatedPayload {
    project_id: ProjectId,
}

impl CreateSession {
    pub fn new(project_id: ProjectId, actor: ActorRef, trace_id: TraceId) -> Self {
        Self {
            project_id,
            actor,
            trace_id,
        }
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub fn actor(&self) -> &ActorRef {
        &self.actor
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }
}

impl SessionService {
    /// Open (or create) a file-backed ledger and wrap it as the repository.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let ledger = EventLedger::open(path).map_err(map_open)?;
        Ok(Self { ledger })
    }

    pub fn new(ledger: EventLedger) -> Self {
        Self { ledger }
    }

    pub(in crate::session) fn ledger(&self) -> &EventLedger {
        &self.ledger
    }

    /// Persist a new session. Returns the projection only after `session.created`
    /// is durably committed at seq 1.
    pub fn create_session(
        &self,
        req: CreateSession,
        cancel: &CancellationToken,
    ) -> Result<SessionSnapshot, SessionError> {
        check_cancel(cancel)?;
        let session_id = SessionId::new();
        let ledger_cancel = event_ledger::ledger::CancellationToken::new();
        self.ledger
            .create_session(session_id, req.project_id, &ledger_cancel)
            .map_err(map_ledger)?;
        check_cancel(cancel)?;
        let options = AppendOptions {
            redaction: RedactionClass::Project,
            trace_id: req.trace_id,
            expected_seq: Some(0),
        };
        let envelope = self
            .ledger
            .append(
                session_id,
                req.actor,
                EventKind::SessionCreated,
                SessionCreatedPayload {
                    project_id: req.project_id,
                },
                &options,
                &ledger_cancel,
            )
            .map_err(map_ledger)?;
        check_cancel(cancel)?;
        let erased = envelope.erase().map_err(|_| SessionError::Internal)?;
        apply(None, &erased).map_err(|err| map_projection(err, session_id))
    }

    /// Rebuild the current projection from committed events.
    ///
    /// Unknown sessions and session rows with no `session.created` event map
    /// to [`SessionError::NotFound`] (`session.not_found`).
    pub fn get_session(
        &self,
        id: SessionId,
        cancel: &CancellationToken,
    ) -> Result<SessionSnapshot, SessionError> {
        check_cancel(cancel)?;
        let ledger_cancel = event_ledger::ledger::CancellationToken::new();
        let last = self
            .ledger
            .last_seq(id, &ledger_cancel)
            .map_err(map_ledger)?;
        if last == 0 {
            return Err(SessionError::NotFound { session_id: id });
        }
        if last > MAX_REPLAY_EVENTS as u64 {
            return Err(SessionError::TooManyEvents);
        }
        let mut snapshot = None;
        for seq in 1..=last {
            let index = (seq as usize).saturating_sub(1);
            if index.is_multiple_of(CANCEL_CHECK_EVERY) {
                check_cancel(cancel)?;
            }
            let event = self
                .ledger
                .get(id, seq, &ledger_cancel)
                .map_err(|err| map_load_event(err, id))?;
            snapshot = Some(apply(snapshot, &event).map_err(|err| map_projection(err, id))?);
        }
        snapshot.ok_or(SessionError::NotFound { session_id: id })
    }
}

impl SessionError {
    /// Public error code when this failure has a wire mapping.
    ///
    /// [`SessionError::Cancelled`] has no public code.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::NotFound { .. } => Some(ErrorCode::SessionNotFound),
            Self::Conflict { .. } => Some(ErrorCode::SessionConflict),
            Self::TooManyEvents | Self::Internal => Some(ErrorCode::InternalUnexpected),
            Self::StorageCorrupt => Some(ErrorCode::StorageCorrupt),
        }
    }

    /// Convert to the public envelope. Cancellation is not an API error.
    pub fn into_api_error(self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = match &self {
            Self::Cancelled => return None,
            Self::NotFound { .. } => "Session not found",
            Self::Conflict { .. } => "Session conflict",
            Self::TooManyEvents | Self::Internal => UNKNOWN_INTERNAL_MESSAGE,
            Self::StorageCorrupt => "Session store is corrupt",
        };
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }
}

pub(in crate::session) fn check_cancel(cancel: &CancellationToken) -> Result<(), SessionError> {
    if cancel.is_cancelled() {
        Err(SessionError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_open(err: LedgerError) -> SessionError {
    match err {
        LedgerError::Cancelled => SessionError::Cancelled,
        LedgerError::Corrupt(_)
        | LedgerError::Migration(_)
        | LedgerError::ForeignKeysDisabled
        | LedgerError::InvalidTimestamp => SessionError::StorageCorrupt,
        _ => SessionError::Internal,
    }
}

pub(in crate::session) fn map_ledger(err: LedgerError) -> SessionError {
    match err {
        LedgerError::Cancelled => SessionError::Cancelled,
        LedgerError::SessionNotFound { session_id } => SessionError::NotFound { session_id },
        LedgerError::SessionExists { session_id }
        | LedgerError::SequenceConflict { session_id, .. } => SessionError::Conflict { session_id },
        LedgerError::Corrupt(_)
        | LedgerError::ForeignKeysDisabled
        | LedgerError::InvalidTimestamp => SessionError::StorageCorrupt,
        LedgerError::EventNotFound { session_id, .. } => SessionError::NotFound { session_id },
        LedgerError::NotCommitted
        | LedgerError::SequenceExhausted
        | LedgerError::PayloadBound { .. }
        | LedgerError::Migration(_)
        | LedgerError::Sqlite(_)
        | LedgerError::Json(_)
        | LedgerError::Io(_) => SessionError::Internal,
    }
}

pub(in crate::session) fn map_load_event(err: LedgerError, session_id: SessionId) -> SessionError {
    match err {
        LedgerError::EventNotFound { .. } => SessionError::StorageCorrupt,
        other => {
            let mapped = map_ledger(other);
            if matches!(mapped, SessionError::NotFound { .. }) {
                SessionError::NotFound { session_id }
            } else {
                mapped
            }
        }
    }
}

pub(in crate::session) fn map_projection(
    err: ProjectionError,
    session_id: SessionId,
) -> SessionError {
    match err {
        ProjectionError::Cancelled => SessionError::Cancelled,
        ProjectionError::TooManyEvents => SessionError::TooManyEvents,
        ProjectionError::Invariant(ProjectionInvariant::SessionNotCreated) => {
            SessionError::NotFound { session_id }
        }
        ProjectionError::Invariant(_) => SessionError::StorageCorrupt,
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("session operation cancelled"),
            Self::NotFound { session_id } => write!(f, "session {session_id} not found"),
            Self::Conflict { session_id } => write!(f, "session {session_id} conflict"),
            Self::TooManyEvents => f.write_str("session event stream exceeds the replay bound"),
            Self::StorageCorrupt => f.write_str("session store is corrupt"),
            Self::Internal => f.write_str("session operation failed internally"),
        }
    }
}

impl Error for SessionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use event_ledger::event::ActorKind;
    use protocol::EventId;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempSessions {
        path: PathBuf,
        service: SessionService,
    }

    impl TempSessions {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-session-service-{}-{seq}.sqlite",
                std::process::id()
            ));
            remove_db_files(&path);
            let service = SessionService::open(&path).expect("open session service");
            Self { path, service }
        }
    }

    impl Drop for TempSessions {
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

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn actor() -> ActorRef {
        ActorRef::new(ActorKind::Human, &EventId::new().to_string()).expect("actor")
    }

    fn create_req() -> CreateSession {
        CreateSession::new(ProjectId::new(), actor(), TraceId::new())
    }

    #[test]
    fn create_emits_session_created_and_returns_projection_at_seq_one() {
        let tmp = TempSessions::create();
        let snapshot = tmp
            .service
            .create_session(create_req(), &live())
            .expect("create");

        assert_eq!(snapshot.seq(), 1);
        assert_eq!(snapshot.status(), crate::SessionStatus::Ready);
        assert!(snapshot.active_turn().is_none());
        assert!(snapshot.top_level_goal().is_none());
        assert!(snapshot.active_agents().is_empty());
        assert_eq!(snapshot.created_at(), snapshot.updated_at());

        let ledger_cancel = event_ledger::ledger::CancellationToken::new();
        let ledger = EventLedger::open(&tmp.path).expect("reopen ledger");
        let event = ledger
            .get(snapshot.id(), 1, &ledger_cancel)
            .expect("committed created event");
        assert_eq!(event.kind(), EventKind::SessionCreated);
        assert_eq!(event.seq(), snapshot.seq());
        assert_eq!(event.session_id(), snapshot.id());
        let project_id = snapshot.project_id().to_string();
        assert_eq!(
            event.payload().get("project_id").and_then(|v| v.as_str()),
            Some(project_id.as_str())
        );
    }

    #[test]
    fn create_is_durable_before_success() {
        let tmp = TempSessions::create();
        let req = create_req();
        let project_id = req.project_id();
        let created = tmp.service.create_session(req, &live()).expect("create");
        assert_eq!(created.project_id(), project_id);
        assert_eq!(created.seq(), 1);

        let reopened = SessionService::open(&tmp.path).expect("reopen service");
        let loaded = reopened
            .get_session(created.id(), &live())
            .expect("get after reopen");
        assert_eq!(loaded, created);
        assert_eq!(loaded.seq(), 1);
        assert_eq!(loaded.project_id(), project_id);
    }

    #[test]
    fn get_unknown_session_is_not_found() {
        let tmp = TempSessions::create();
        let missing = SessionId::new();
        let err = tmp
            .service
            .get_session(missing, &live())
            .expect_err("unknown session");
        assert_eq!(
            err,
            SessionError::NotFound {
                session_id: missing
            }
        );
        assert_eq!(err.code(), Some(ErrorCode::SessionNotFound));

        let api = err
            .into_api_error(TraceId::new())
            .expect("public session.not_found");
        assert_eq!(api.code(), ErrorCode::SessionNotFound);
        assert_eq!(api.code().as_str(), "session.not_found");
        assert!(!api.retryable());
    }

    #[test]
    fn get_session_row_without_created_event_is_not_found() {
        let tmp = TempSessions::create();
        let session = SessionId::new();
        let ledger_cancel = event_ledger::ledger::CancellationToken::new();
        EventLedger::open(&tmp.path)
            .expect("ledger")
            .create_session(session, ProjectId::new(), &ledger_cancel)
            .expect("session row");

        let err = tmp
            .service
            .get_session(session, &live())
            .expect_err("no created event");
        assert_eq!(
            err,
            SessionError::NotFound {
                session_id: session
            }
        );
        assert_eq!(err.code().map(ErrorCode::as_str), Some("session.not_found"));
    }

    #[test]
    fn create_then_get_returns_same_projection() {
        let tmp = TempSessions::create();
        let created = tmp
            .service
            .create_session(create_req(), &live())
            .expect("create");
        let loaded = tmp.service.get_session(created.id(), &live()).expect("get");
        assert_eq!(loaded, created);
    }

    #[test]
    fn cancelled_create_does_not_succeed() {
        let tmp = TempSessions::create();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tmp
            .service
            .create_session(create_req(), &cancel)
            .expect_err("cancelled");
        assert_eq!(err, SessionError::Cancelled);
        assert_eq!(err.code(), None);
        assert!(err.into_api_error(TraceId::new()).is_none());
    }

    #[test]
    fn cancelled_get_does_not_lookup() {
        let tmp = TempSessions::create();
        let created = tmp
            .service
            .create_session(create_req(), &live())
            .expect("create");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tmp
            .service
            .get_session(created.id(), &cancel)
            .expect_err("cancelled");
        assert_eq!(err, SessionError::Cancelled);
    }
}
