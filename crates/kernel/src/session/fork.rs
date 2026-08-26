//! Session fork: new event stream from a source projection prefix.
//!
//! `fork_session` copies the conversation projection and workspace pointer
//! into a new session. It does not copy an active top-level goal, capability
//! leases, execution leases, or control leases. The child stream starts at
//! seq 1 with `session.forked`.

use event_ledger::event::{ActorRef, ErasedEventEnvelope, EventKind};
use event_ledger::ledger::AppendOptions;
use protocol::{ProjectId, RedactionClass, SessionId, TraceId, WorkspaceViewId};
use serde::Serialize;
use serde_json::Value;

use crate::CancellationToken;
use crate::session::projection::{MAX_REPLAY_EVENTS, SessionSnapshot, apply};
use crate::session::service::{
    SessionError, SessionService, check_cancel, map_ledger, map_load_event, map_projection,
};

const CANCEL_CHECK_EVERY: usize = 32;

/// Request to fork a source session at a committed sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkSession {
    source: SessionId,
    at_seq: u64,
    actor: ActorRef,
    trace_id: TraceId,
}

#[derive(Serialize)]
struct SessionForkedPayload {
    project_id: ProjectId,
    parent_session_id: SessionId,
    source_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_view_id: Option<WorkspaceViewId>,
}

impl ForkSession {
    pub fn new(source: SessionId, at_seq: u64, actor: ActorRef, trace_id: TraceId) -> Self {
        Self {
            source,
            at_seq,
            actor,
            trace_id,
        }
    }

    pub fn source(&self) -> SessionId {
        self.source
    }

    pub fn at_seq(&self) -> u64 {
        self.at_seq
    }

    pub fn actor(&self) -> &ActorRef {
        &self.actor
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }
}

impl SessionService {
    /// Persist a child session from `source` through `at_seq`.
    ///
    /// Success is returned only after `session.forked` is durably committed at
    /// child seq 1. The source stream is not mutated.
    pub fn fork_session(
        &self,
        req: ForkSession,
        cancel: &CancellationToken,
    ) -> Result<SessionSnapshot, SessionError> {
        check_cancel(cancel)?;
        let source = load_source_prefix(self, req.source, req.at_seq, cancel)?;
        check_cancel(cancel)?;

        let child_id = SessionId::new();
        let ledger_cancel = event_ledger::ledger::CancellationToken::new();
        self.ledger()
            .create_session(child_id, source.project_id, &ledger_cancel)
            .map_err(map_ledger)?;
        check_cancel(cancel)?;

        let options = AppendOptions {
            redaction: RedactionClass::Project,
            trace_id: req.trace_id,
            expected_seq: Some(0),
        };
        let envelope = self
            .ledger()
            .append(
                child_id,
                req.actor,
                EventKind::SessionForked,
                SessionForkedPayload {
                    project_id: source.project_id,
                    parent_session_id: req.source,
                    source_seq: req.at_seq,
                    workspace_view_id: source.workspace_view_id,
                },
                &options,
                &ledger_cancel,
            )
            .map_err(map_ledger)?;
        check_cancel(cancel)?;
        let erased = envelope.erase().map_err(|_| SessionError::Internal)?;
        apply(None, &erased).map_err(|err| map_projection(err, child_id))
    }
}

struct SourcePrefix {
    project_id: ProjectId,
    workspace_view_id: Option<WorkspaceViewId>,
}

fn load_source_prefix(
    sessions: &SessionService,
    source: SessionId,
    at_seq: u64,
    cancel: &CancellationToken,
) -> Result<SourcePrefix, SessionError> {
    let ledger_cancel = event_ledger::ledger::CancellationToken::new();
    let last = sessions
        .ledger()
        .last_seq(source, &ledger_cancel)
        .map_err(map_ledger)?;
    if last == 0 || at_seq == 0 || at_seq > last {
        return Err(SessionError::NotFound { session_id: source });
    }
    if at_seq > MAX_REPLAY_EVENTS as u64 {
        return Err(SessionError::TooManyEvents);
    }

    let mut snapshot = None;
    let mut workspace_view_id = None;
    for seq in 1..=at_seq {
        let index = (seq as usize).saturating_sub(1);
        if index.is_multiple_of(CANCEL_CHECK_EVERY) {
            check_cancel(cancel)?;
        }
        let event = sessions
            .ledger()
            .get(source, seq, &ledger_cancel)
            .map_err(|err| map_load_event(err, source))?;
        if let Some(view_id) = workspace_pointer(&event)? {
            workspace_view_id = Some(view_id);
        }
        snapshot = Some(apply(snapshot, &event).map_err(|err| map_projection(err, source))?);
    }

    let snapshot = snapshot.ok_or(SessionError::NotFound { session_id: source })?;
    Ok(SourcePrefix {
        project_id: snapshot.project_id(),
        workspace_view_id,
    })
}

fn workspace_pointer(event: &ErasedEventEnvelope) -> Result<Option<WorkspaceViewId>, SessionError> {
    if !carries_workspace_pointer(event.kind()) {
        return Ok(None);
    }
    let payload = event.payload();
    let raw = payload
        .get("workspace_view_id")
        .or_else(|| payload.get("view_id"));
    match raw {
        None => Ok(None),
        Some(Value::String(value)) => value
            .parse()
            .map(Some)
            .map_err(|_| SessionError::StorageCorrupt),
        Some(_) => Err(SessionError::StorageCorrupt),
    }
}

fn carries_workspace_pointer(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::SessionForked
            | EventKind::WorkspaceViewCreated
            | EventKind::WorkspaceMutationDetected
            | EventKind::WorkspacePatchStaged
            | EventKind::WorkspaceTransactionCommitted
            | EventKind::WorkspaceTransactionRolledBack
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_ledger::event::ActorKind;
    use event_ledger::ledger::EventLedger;
    use protocol::{ErrorCode, EventId, GoalId, LeaseId, ProjectId};
    use serde_json::{Value, json};
    use std::path::Path;
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
                "rapidlm-session-fork-{}-{seq}.sqlite",
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

    fn create_source(tmp: &TempSessions) -> SessionSnapshot {
        tmp.service
            .create_session(
                crate::CreateSession::new(ProjectId::new(), actor(), TraceId::new()),
                &live(),
            )
            .expect("create source")
    }

    fn fork_req(source: SessionId, at_seq: u64) -> ForkSession {
        ForkSession::new(source, at_seq, actor(), TraceId::new())
    }

    fn append(
        tmp: &TempSessions,
        session: SessionId,
        expected_seq: u64,
        kind: EventKind,
        payload: Value,
    ) {
        let ledger = EventLedger::open(&tmp.path).expect("reopen ledger");
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
                payload,
                &options,
                &event_ledger::ledger::CancellationToken::new(),
            )
            .expect("append");
    }

    fn load_event(tmp: &TempSessions, session: SessionId, seq: u64) -> ErasedEventEnvelope {
        EventLedger::open(&tmp.path)
            .expect("reopen ledger")
            .get(
                session,
                seq,
                &event_ledger::ledger::CancellationToken::new(),
            )
            .expect("load event")
    }

    #[test]
    fn fork_emits_session_forked_with_source_reference_at_seq_one() {
        let tmp = TempSessions::create();
        let source = create_source(&tmp);
        let child = tmp
            .service
            .fork_session(fork_req(source.id(), source.seq()), &live())
            .expect("fork");

        assert_ne!(child.id(), source.id());
        assert_eq!(child.project_id(), source.project_id());
        assert_eq!(child.seq(), 1);
        assert_eq!(child.status(), crate::SessionStatus::Ready);
        assert!(child.active_turn().is_none());
        assert!(child.top_level_goal().is_none());
        assert!(child.active_agents().is_empty());

        let event = load_event(&tmp, child.id(), 1);
        assert_eq!(event.kind(), EventKind::SessionForked);
        assert_eq!(event.seq(), 1);
        assert_eq!(event.session_id(), child.id());
        assert_eq!(
            event
                .payload()
                .get("parent_session_id")
                .and_then(Value::as_str),
            Some(source.id().to_string()).as_deref()
        );
        assert_eq!(
            event.payload().get("source_seq").and_then(Value::as_u64),
            Some(source.seq())
        );
        assert_eq!(
            event.payload().get("project_id").and_then(Value::as_str),
            Some(source.project_id().to_string()).as_deref()
        );
        assert!(event.payload().get("workspace_view_id").is_none());
        assert!(event.payload().get("goal_id").is_none());
        assert!(event.payload().get("lease_id").is_none());
    }

    #[test]
    fn fork_copies_workspace_pointer_without_goal_or_leases() {
        let tmp = TempSessions::create();
        let source = create_source(&tmp);
        let view_id = WorkspaceViewId::new();
        let goal_id = GoalId::new();
        let lease_id = LeaseId::new();
        append(
            &tmp,
            source.id(),
            1,
            EventKind::WorkspaceViewCreated,
            json!({"workspace_view_id": view_id.to_string()}),
        );
        append(
            &tmp,
            source.id(),
            2,
            EventKind::GoalCreated,
            json!({"goal_id": goal_id.to_string(), "statement": "ship it"}),
        );
        append(
            &tmp,
            source.id(),
            3,
            EventKind::ToolAuthorized,
            json!({
                "lease_id": lease_id.to_string(),
                "capability": "fs.write",
                "resource": "/tmp/secret"
            }),
        );

        let source_after = tmp
            .service
            .get_session(source.id(), &live())
            .expect("source after privileged events");
        assert!(source_after.top_level_goal().is_some());

        let child = tmp
            .service
            .fork_session(fork_req(source.id(), 4), &live())
            .expect("fork");
        assert_eq!(child.seq(), 1);
        assert!(child.top_level_goal().is_none());
        assert!(child.active_agents().is_empty());

        let event = load_event(&tmp, child.id(), 1);
        assert_eq!(event.kind(), EventKind::SessionForked);
        assert_eq!(
            event
                .payload()
                .get("workspace_view_id")
                .and_then(Value::as_str),
            Some(view_id.to_string()).as_deref()
        );
        let payload = event.payload();
        assert!(payload.get("goal_id").is_none());
        assert!(payload.get("top_level_goal").is_none());
        assert!(payload.get("lease_id").is_none());
        assert!(payload.get("capability").is_none());
        assert!(payload.get("resource").is_none());
        assert!(payload.get("control_lease_id").is_none());
        assert!(payload.get("execution_lease").is_none());

        let last = EventLedger::open(&tmp.path)
            .expect("ledger")
            .last_seq(child.id(), &event_ledger::ledger::CancellationToken::new())
            .expect("child last seq");
        assert_eq!(last, 1);

        let source_unchanged = tmp
            .service
            .get_session(source.id(), &live())
            .expect("source unchanged");
        assert_eq!(source_unchanged, source_after);
        assert_eq!(
            source_unchanged.top_level_goal().map(|goal| goal.id()),
            Some(goal_id)
        );
    }

    #[test]
    fn fork_at_historical_seq_ignores_later_source_events() {
        let tmp = TempSessions::create();
        let source = create_source(&tmp);
        let first_view = WorkspaceViewId::new();
        let later_view = WorkspaceViewId::new();
        append(
            &tmp,
            source.id(),
            1,
            EventKind::WorkspaceViewCreated,
            json!({"workspace_view_id": first_view.to_string()}),
        );
        append(
            &tmp,
            source.id(),
            2,
            EventKind::WorkspaceViewCreated,
            json!({"workspace_view_id": later_view.to_string()}),
        );

        let child = tmp
            .service
            .fork_session(fork_req(source.id(), 2), &live())
            .expect("historical fork");
        let event = load_event(&tmp, child.id(), 1);
        assert_eq!(
            event.payload().get("source_seq").and_then(Value::as_u64),
            Some(2)
        );
        assert_eq!(
            event
                .payload()
                .get("workspace_view_id")
                .and_then(Value::as_str),
            Some(first_view.to_string()).as_deref()
        );
    }

    #[test]
    fn fork_is_durable_and_independent_of_source_seq() {
        let tmp = TempSessions::create();
        let source = create_source(&tmp);
        let child = tmp
            .service
            .fork_session(fork_req(source.id(), 1), &live())
            .expect("fork");

        append(
            &tmp,
            source.id(),
            1,
            EventKind::TurnStarted,
            json!({"turn_id": protocol::TurnId::new().to_string()}),
        );

        let reopened = SessionService::open(&tmp.path).expect("reopen");
        let loaded = reopened
            .get_session(child.id(), &live())
            .expect("get forked after reopen");
        assert_eq!(loaded, child);
        assert_eq!(loaded.seq(), 1);
        assert!(loaded.active_turn().is_none());

        let source_later = reopened
            .get_session(source.id(), &live())
            .expect("source advanced");
        assert_eq!(source_later.seq(), 2);
        assert!(source_later.active_turn().is_some());
    }

    #[test]
    fn unknown_source_or_seq_is_not_found() {
        let tmp = TempSessions::create();
        let missing = SessionId::new();
        let err = tmp
            .service
            .fork_session(fork_req(missing, 1), &live())
            .expect_err("unknown source");
        assert_eq!(
            err,
            SessionError::NotFound {
                session_id: missing
            }
        );
        assert_eq!(err.code(), Some(ErrorCode::SessionNotFound));

        let source = create_source(&tmp);
        let err = tmp
            .service
            .fork_session(fork_req(source.id(), 0), &live())
            .expect_err("seq 0");
        assert_eq!(
            err,
            SessionError::NotFound {
                session_id: source.id()
            }
        );

        let err = tmp
            .service
            .fork_session(fork_req(source.id(), 9), &live())
            .expect_err("future seq");
        assert_eq!(
            err,
            SessionError::NotFound {
                session_id: source.id()
            }
        );
    }

    #[test]
    fn cancelled_fork_does_not_succeed() {
        let tmp = TempSessions::create();
        let source = create_source(&tmp);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tmp
            .service
            .fork_session(fork_req(source.id(), 1), &cancel)
            .expect_err("cancelled");
        assert_eq!(err, SessionError::Cancelled);
        assert_eq!(err.code(), None);
        assert!(err.into_api_error(TraceId::new()).is_none());
    }

    #[test]
    fn fork_does_not_copy_handoff_or_control_leases() {
        let tmp = TempSessions::create();
        let source = create_source(&tmp);
        append(
            &tmp,
            source.id(),
            1,
            EventKind::HandoffExecutionLeaseCommitted,
            json!({
                "generation": 3,
                "owner_runtime_id": protocol::RuntimeId::new().to_string(),
                "lease_digest": "sha256:not-a-real-lease"
            }),
        );
        append(
            &tmp,
            source.id(),
            2,
            EventKind::ControlTransferredToAgent,
            json!({
                "control_lease_id": protocol::ControlLeaseId::new().to_string(),
                "generation": 1
            }),
        );

        let child = tmp
            .service
            .fork_session(fork_req(source.id(), 3), &live())
            .expect("fork");
        let event = load_event(&tmp, child.id(), 1);
        let payload = event.payload();
        assert_eq!(event.kind(), EventKind::SessionForked);
        assert!(payload.get("generation").is_none());
        assert!(payload.get("owner_runtime_id").is_none());
        assert!(payload.get("lease_digest").is_none());
        assert!(payload.get("control_lease_id").is_none());
        assert_eq!(
            EventLedger::open(&tmp.path)
                .expect("ledger")
                .last_seq(child.id(), &event_ledger::ledger::CancellationToken::new())
                .expect("last"),
            1
        );
    }
}
