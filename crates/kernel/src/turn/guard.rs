//! Exclusive foreground turn occupancy with optimistic `expected_seq` checks.
//!
//! `begin_turn` admits at most one in-process lease per session. The durable
//! last seq must match `expected_seq`; a mismatch, live occupancy, or an
//! already-active projected turn is `session.conflict`. Completing, failing,
//! cancelling, or dropping the lease releases occupancy.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use protocol::{ApiError, ErrorCode, SessionId, TraceId, TurnId, UNKNOWN_INTERNAL_MESSAGE};

use crate::CancellationToken;
use crate::session::projection::SessionStatus;
use crate::session::service::{SessionError, SessionService};

/// In-process table of exclusive foreground turn leases.
#[derive(Clone, Debug)]
pub struct TurnSubmissionGuard {
    shared: Arc<Shared>,
}

/// Exclusive lease that mutates one session until released.
#[derive(Debug)]
pub struct TurnLease {
    shared: Arc<Shared>,
    session_id: SessionId,
    turn_id: TurnId,
    expected_seq: u64,
    generation: u64,
    released: bool,
}

/// Typed turn-submission failure. Public mapping uses [`TurnGuardError::code`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnGuardError {
    Cancelled,
    NotFound { session_id: SessionId },
    Conflict { session_id: SessionId },
    TooManyEvents,
    StorageCorrupt,
    Internal,
}

#[derive(Debug)]
struct Shared {
    sessions: SessionService,
    next_generation: AtomicU64,
    occupied: Mutex<HashMap<SessionId, u64>>,
}

impl TurnSubmissionGuard {
    pub fn new(sessions: SessionService) -> Self {
        Self {
            shared: Arc::new(Shared {
                sessions,
                next_generation: AtomicU64::new(1),
                occupied: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Acquire the single foreground turn for `session` at `expected_seq`.
    ///
    /// Occupancy is claimed before the durable seq is read so two concurrent
    /// submits cannot both observe a matching seq. A later seq mismatch,
    /// missing session, cancellation, or projected active turn releases the
    /// claim before returning.
    pub fn begin_turn(
        &self,
        session: SessionId,
        expected_seq: u64,
        cancel: &CancellationToken,
    ) -> Result<TurnLease, TurnGuardError> {
        check_cancel(cancel)?;
        let generation = self.try_occupy(session)?;
        if let Err(err) = check_cancel(cancel) {
            self.release(session, generation);
            return Err(err);
        }
        match self.validate_seq(session, expected_seq, cancel) {
            Ok(()) => Ok(TurnLease {
                shared: Arc::clone(&self.shared),
                session_id: session,
                turn_id: TurnId::new(),
                expected_seq,
                generation,
                released: false,
            }),
            Err(err) => {
                self.release(session, generation);
                Err(err)
            }
        }
    }

    fn try_occupy(&self, session: SessionId) -> Result<u64, TurnGuardError> {
        let mut occupied = lock_occupied(&self.shared.occupied);
        if occupied.contains_key(&session) {
            return Err(TurnGuardError::Conflict {
                session_id: session,
            });
        }
        let generation = self.shared.next_generation.fetch_add(1, Ordering::Relaxed);
        occupied.insert(session, generation);
        Ok(generation)
    }

    fn release(&self, session: SessionId, generation: u64) {
        release_occupancy(&self.shared.occupied, session, generation);
    }

    fn validate_seq(
        &self,
        session: SessionId,
        expected_seq: u64,
        cancel: &CancellationToken,
    ) -> Result<(), TurnGuardError> {
        let snapshot = self
            .shared
            .sessions
            .get_session(session, cancel)
            .map_err(map_session)?;
        if snapshot.seq() != expected_seq
            || snapshot.active_turn().is_some()
            || snapshot.status() == SessionStatus::Closed
        {
            return Err(TurnGuardError::Conflict {
                session_id: session,
            });
        }
        Ok(())
    }
}

impl TurnLease {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub fn expected_seq(&self) -> u64 {
        self.expected_seq
    }

    /// Release occupancy after a completed turn.
    pub fn complete(mut self) {
        self.release_occupancy();
    }

    /// Release occupancy after a failed turn.
    pub fn fail(mut self) {
        self.release_occupancy();
    }

    /// Release occupancy after a cancelled turn.
    pub fn cancel(mut self) {
        self.release_occupancy();
    }

    fn release_occupancy(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        release_occupancy(&self.shared.occupied, self.session_id, self.generation);
    }
}

impl Drop for TurnLease {
    fn drop(&mut self) {
        self.release_occupancy();
    }
}

impl TurnGuardError {
    /// Public error code when this failure has a wire mapping.
    ///
    /// [`TurnGuardError::Cancelled`] has no public code.
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

impl fmt::Display for TurnGuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("turn submission cancelled"),
            Self::NotFound { session_id } => write!(f, "session {session_id} not found"),
            Self::Conflict { session_id } => write!(f, "session {session_id} conflict"),
            Self::TooManyEvents => f.write_str("session event stream exceeds the replay bound"),
            Self::StorageCorrupt => f.write_str("session store is corrupt"),
            Self::Internal => f.write_str("turn submission failed internally"),
        }
    }
}

impl Error for TurnGuardError {}

fn check_cancel(cancel: &CancellationToken) -> Result<(), TurnGuardError> {
    if cancel.is_cancelled() {
        Err(TurnGuardError::Cancelled)
    } else {
        Ok(())
    }
}

fn lock_occupied(
    mutex: &Mutex<HashMap<SessionId, u64>>,
) -> std::sync::MutexGuard<'_, HashMap<SessionId, u64>> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn release_occupancy(mutex: &Mutex<HashMap<SessionId, u64>>, session: SessionId, generation: u64) {
    let mut occupied = lock_occupied(mutex);
    if occupied.get(&session) == Some(&generation) {
        occupied.remove(&session);
    }
}

fn map_session(err: SessionError) -> TurnGuardError {
    match err {
        SessionError::Cancelled => TurnGuardError::Cancelled,
        SessionError::NotFound { session_id } => TurnGuardError::NotFound { session_id },
        SessionError::Conflict { session_id } => TurnGuardError::Conflict { session_id },
        SessionError::TooManyEvents => TurnGuardError::TooManyEvents,
        SessionError::StorageCorrupt => TurnGuardError::StorageCorrupt,
        SessionError::Internal => TurnGuardError::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_ledger::event::{ActorKind, ActorRef, EventKind};
    use event_ledger::ledger::{AppendOptions, EventLedger};
    use protocol::{EventId, ProjectId, RedactionClass};
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempGuard {
        path: PathBuf,
        sessions: SessionService,
        guard: TurnSubmissionGuard,
    }

    impl TempGuard {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-turn-guard-{}-{seq}.sqlite",
                std::process::id()
            ));
            remove_db_files(&path);
            let sessions = SessionService::open(&path).expect("open session service");
            let guard = TurnSubmissionGuard::new(sessions.clone());
            Self {
                path,
                sessions,
                guard,
            }
        }

        fn create_session(&self) -> crate::SessionSnapshot {
            self.sessions
                .create_session(create_req(), &live())
                .expect("create session")
        }
    }

    impl Drop for TempGuard {
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

    fn create_req() -> crate::CreateSession {
        crate::CreateSession::new(ProjectId::new(), actor(), TraceId::new())
    }

    fn append_kind(path: &Path, session: SessionId, expected_seq: u64, kind: EventKind) {
        let ledger = EventLedger::open(path).expect("reopen ledger");
        let options = AppendOptions {
            redaction: RedactionClass::Project,
            trace_id: TraceId::new(),
            expected_seq: Some(expected_seq),
        };
        ledger
            .append(
                session,
                actor(),
                kind,
                serde_json::json!({}),
                &options,
                &event_ledger::ledger::CancellationToken::new(),
            )
            .expect("append");
    }

    #[test]
    fn begin_turn_returns_lease_at_expected_seq() {
        let tmp = TempGuard::create();
        let snapshot = tmp.create_session();
        let lease = tmp
            .guard
            .begin_turn(snapshot.id(), snapshot.seq(), &live())
            .expect("begin");
        assert_eq!(lease.session_id(), snapshot.id());
        assert_eq!(lease.expected_seq(), snapshot.seq());
        assert_ne!(lease.turn_id(), TurnId::new());
    }

    #[test]
    fn concurrent_submit_permits_exactly_one() {
        const THREADS: usize = 16;
        let tmp = TempGuard::create();
        let snapshot = tmp.create_session();
        let start = Arc::new(Barrier::new(THREADS));
        let held = Arc::new(Barrier::new(THREADS));
        let successes = Arc::new(AtomicUsize::new(0));
        let conflicts = Arc::new(AtomicUsize::new(0));
        let unexpected = Arc::new(AtomicUsize::new(0));

        thread::scope(|scope| {
            for _ in 0..THREADS {
                let guard = tmp.guard.clone();
                let start = Arc::clone(&start);
                let held = Arc::clone(&held);
                let successes = Arc::clone(&successes);
                let conflicts = Arc::clone(&conflicts);
                let unexpected = Arc::clone(&unexpected);
                let session = snapshot.id();
                let expected = snapshot.seq();
                scope.spawn(move || {
                    start.wait();
                    match guard.begin_turn(session, expected, &live()) {
                        Ok(lease) => {
                            successes.fetch_add(1, AtomicOrdering::SeqCst);
                            held.wait();
                            lease.complete();
                        }
                        Err(TurnGuardError::Conflict { session_id }) if session_id == session => {
                            conflicts.fetch_add(1, AtomicOrdering::SeqCst);
                            held.wait();
                        }
                        Err(_) => {
                            unexpected.fetch_add(1, AtomicOrdering::SeqCst);
                            held.wait();
                        }
                    }
                });
            }
        });

        assert_eq!(successes.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(conflicts.load(AtomicOrdering::SeqCst), THREADS - 1);
        assert_eq!(unexpected.load(AtomicOrdering::SeqCst), 0);

        tmp.guard
            .begin_turn(snapshot.id(), snapshot.seq(), &live())
            .expect("occupancy released after winner completed");
    }

    #[test]
    fn release_on_completion_failure_and_cancellation() {
        let tmp = TempGuard::create();
        let snapshot = tmp.create_session();
        let session = snapshot.id();
        let expected = snapshot.seq();

        tmp.guard
            .begin_turn(session, expected, &live())
            .expect("complete path")
            .complete();
        tmp.guard
            .begin_turn(session, expected, &live())
            .expect("after complete")
            .fail();
        tmp.guard
            .begin_turn(session, expected, &live())
            .expect("after fail")
            .cancel();
        drop(
            tmp.guard
                .begin_turn(session, expected, &live())
                .expect("after cancel"),
        );
        tmp.guard
            .begin_turn(session, expected, &live())
            .expect("after drop");
    }

    #[test]
    fn stale_expected_seq_is_session_conflict() {
        let tmp = TempGuard::create();
        let snapshot = tmp.create_session();
        append_kind(
            &tmp.path,
            snapshot.id(),
            snapshot.seq(),
            EventKind::ArtifactCreated,
        );

        let err = tmp
            .guard
            .begin_turn(snapshot.id(), snapshot.seq(), &live())
            .expect_err("stale seq");
        assert_eq!(
            err,
            TurnGuardError::Conflict {
                session_id: snapshot.id()
            }
        );
        assert_eq!(err.code(), Some(ErrorCode::SessionConflict));
        let api = err
            .into_api_error(TraceId::new())
            .expect("public session.conflict");
        assert_eq!(api.code(), ErrorCode::SessionConflict);
        assert_eq!(api.code().as_str(), "session.conflict");
        assert!(!api.retryable());

        let lease = tmp
            .guard
            .begin_turn(snapshot.id(), snapshot.seq() + 1, &live())
            .expect("matching seq after advance");
        assert_eq!(lease.expected_seq(), snapshot.seq() + 1);
    }

    #[test]
    fn projected_active_turn_is_conflict() {
        let tmp = TempGuard::create();
        let snapshot = tmp.create_session();
        let ledger = EventLedger::open(&tmp.path).expect("ledger");
        let options = AppendOptions {
            redaction: RedactionClass::Project,
            trace_id: TraceId::new(),
            expected_seq: Some(snapshot.seq()),
        };
        ledger
            .append(
                snapshot.id(),
                actor(),
                EventKind::TurnStarted,
                serde_json::json!({"turn_id": TurnId::new().to_string()}),
                &options,
                &event_ledger::ledger::CancellationToken::new(),
            )
            .expect("turn.started");

        let err = tmp
            .guard
            .begin_turn(snapshot.id(), snapshot.seq() + 1, &live())
            .expect_err("active turn");
        assert_eq!(
            err,
            TurnGuardError::Conflict {
                session_id: snapshot.id()
            }
        );
    }

    #[test]
    fn cancelled_begin_does_not_occupy() {
        let tmp = TempGuard::create();
        let snapshot = tmp.create_session();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tmp
            .guard
            .begin_turn(snapshot.id(), snapshot.seq(), &cancel)
            .expect_err("cancelled");
        assert_eq!(err, TurnGuardError::Cancelled);
        assert_eq!(err.code(), None);
        assert!(err.into_api_error(TraceId::new()).is_none());

        tmp.guard
            .begin_turn(snapshot.id(), snapshot.seq(), &live())
            .expect("occupancy not leaked by cancelled begin");
    }

    #[test]
    fn unknown_session_is_not_found() {
        let tmp = TempGuard::create();
        let missing = SessionId::new();
        let err = tmp
            .guard
            .begin_turn(missing, 1, &live())
            .expect_err("unknown");
        assert_eq!(
            err,
            TurnGuardError::NotFound {
                session_id: missing
            }
        );
        assert_eq!(err.code(), Some(ErrorCode::SessionNotFound));
        tmp.guard
            .begin_turn(missing, 1, &live())
            .expect_err("occupancy not leaked by not-found");
    }

    #[test]
    fn distinct_sessions_may_hold_leases_together() {
        let tmp = TempGuard::create();
        let first = tmp.create_session();
        let second = tmp.create_session();
        let a = tmp
            .guard
            .begin_turn(first.id(), first.seq(), &live())
            .expect("first");
        let b = tmp
            .guard
            .begin_turn(second.id(), second.seq(), &live())
            .expect("second");
        assert_ne!(a.session_id(), b.session_id());
        a.complete();
        b.fail();
    }
}
