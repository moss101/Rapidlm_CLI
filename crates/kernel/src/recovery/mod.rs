//! Session crash recovery.
//!
//! [`RecoveryManager::recover_session`] verifies storage, loads the newest
//! compatible checkpoint when one exists, replays later committed events,
//! applies interrupt/pause actions, and returns a safe snapshot. Active
//! top-level goals are parked paused at the recovery hook. The pipeline is
//! idempotent across repeated startup.

pub mod classify;

use std::error::Error;
use std::fmt;
use std::path::Path;

use event_ledger::checkpoint::{CheckpointError, CheckpointStore, LoadedCheckpoint};
use event_ledger::event::{ActorKind, ActorRef, ErasedEventEnvelope, EventKind};
use event_ledger::ledger::{AppendOptions, EventLedger, LedgerError};
use protocol::{
    ApiError, ErrorCode, EventId, GoalId, RedactionClass, SessionId, TraceId,
    UNKNOWN_INTERNAL_MESSAGE,
};
use serde_json::{Value, json};

use crate::CancellationToken;
use crate::session::projection::{
    GoalStopReason, MAX_REPLAY_EVENTS, ProjectionError, ProjectionInvariant,
    SESSION_SNAPSHOT_SCHEMA, SessionSnapshot, SessionStatus, apply, replay,
};

pub use classify::{
    ClassifyError, InflightModel, InflightProjection, InflightTool, MAX_INFLIGHT_ID_BYTES,
    MAX_INFLIGHT_MODELS, MAX_INFLIGHT_TOOLS, RecoveryAction, RecoveryActions, RecoveryDisposition,
    classify_inflight, project_inflight,
};

const CANCEL_CHECK_EVERY: usize = 32;

/// Durable recovery pipeline over the event ledger and optional checkpoints.
#[derive(Clone, Debug)]
pub struct RecoveryManager {
    ledger: EventLedger,
    checkpoints: Option<CheckpointStore>,
}

/// Typed recovery failure. Public mapping uses [`RecoveryError::code`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryError {
    Cancelled,
    NotFound { session_id: SessionId },
    Conflict { session_id: SessionId },
    TooManyEvents,
    StorageCorrupt,
    Internal,
}

impl RecoveryManager {
    /// Open (or create) a file-backed ledger without a checkpoint store.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RecoveryError> {
        let ledger = EventLedger::open(path).map_err(map_open)?;
        Ok(Self {
            ledger,
            checkpoints: None,
        })
    }

    pub fn new(ledger: EventLedger) -> Self {
        Self {
            ledger,
            checkpoints: None,
        }
    }

    pub fn with_checkpoints(mut self, checkpoints: CheckpointStore) -> Self {
        self.checkpoints = Some(checkpoints);
        self
    }

    pub fn ledger(&self) -> &EventLedger {
        &self.ledger
    }

    /// Verify storage, replay, apply recovery actions, and return a safe snapshot.
    ///
    /// Incomplete model/tool/turn work is interrupted. An active autonomous
    /// goal is parked paused by [`park_recovered_goal`]. Repeated startup
    /// against an already-recovered session does not append again.
    pub fn recover_session(
        &self,
        session_id: SessionId,
        cancel: &CancellationToken,
    ) -> Result<SessionSnapshot, RecoveryError> {
        check_cancel(cancel)?;
        let events = self.load_events(session_id, cancel)?;
        self.verify_checkpoint(session_id, &events, cancel)?;
        let inflight = project_inflight(&events, cancel).map_err(map_classify)?;
        let actions = classify_inflight(&inflight);
        if actions.contains_success() {
            return Err(RecoveryError::Internal);
        }
        let mut snapshot = inflight.snapshot().clone();
        if snapshot.status() == SessionStatus::Closed || actions.is_empty() {
            return Ok(snapshot);
        }

        let actor = system_actor()?;
        let trace_id = TraceId::new();
        let last_kind = events.last().map(ErasedEventEnvelope::kind);
        if last_kind != Some(EventKind::SessionRecovered) {
            snapshot = self.append_recovery_event(
                snapshot,
                &actor,
                trace_id,
                (EventKind::SessionRecovered, json!({})),
                cancel,
            )?;
        }
        for action in actions.iter() {
            check_cancel(cancel)?;
            snapshot = self.append_recovery_event(
                snapshot,
                &actor,
                trace_id,
                action_event(action),
                cancel,
            )?;
        }
        Ok(snapshot)
    }

    fn load_events(
        &self,
        session_id: SessionId,
        cancel: &CancellationToken,
    ) -> Result<Vec<ErasedEventEnvelope>, RecoveryError> {
        check_cancel(cancel)?;
        let ledger_cancel = event_ledger::ledger::CancellationToken::new();
        let last = self
            .ledger
            .last_seq(session_id, &ledger_cancel)
            .map_err(map_ledger)?;
        if last == 0 {
            return Err(RecoveryError::NotFound { session_id });
        }
        if last > MAX_REPLAY_EVENTS as u64 {
            return Err(RecoveryError::TooManyEvents);
        }
        let mut events = Vec::with_capacity(last as usize);
        for seq in 1..=last {
            let index = (seq as usize).saturating_sub(1);
            if index.is_multiple_of(CANCEL_CHECK_EVERY) {
                check_cancel(cancel)?;
            }
            let event = self
                .ledger
                .get(session_id, seq, &ledger_cancel)
                .map_err(|err| map_load_event(err, session_id))?;
            events.push(event);
        }
        Ok(events)
    }

    fn verify_checkpoint(
        &self,
        session_id: SessionId,
        events: &[ErasedEventEnvelope],
        cancel: &CancellationToken,
    ) -> Result<(), RecoveryError> {
        let Some(store) = self.checkpoints.as_ref() else {
            return Ok(());
        };
        check_cancel(cancel)?;
        let checkpoint_cancel = event_ledger::checkpoint::CancellationToken::new();
        let loaded = store
            .load_checkpoint(
                session_id,
                i32::from(SESSION_SNAPSHOT_SCHEMA),
                &checkpoint_cancel,
            )
            .map_err(map_checkpoint)?;
        let Some(loaded) = loaded else {
            return Ok(());
        };
        verify_loaded_checkpoint(session_id, events, &loaded, cancel)
    }

    fn append_recovery_event(
        &self,
        snapshot: SessionSnapshot,
        actor: &ActorRef,
        trace_id: TraceId,
        event: (EventKind, Value),
        cancel: &CancellationToken,
    ) -> Result<SessionSnapshot, RecoveryError> {
        check_cancel(cancel)?;
        let session_id = snapshot.id();
        let (kind, payload) = event;
        let options = AppendOptions {
            redaction: RedactionClass::Project,
            trace_id,
            expected_seq: Some(snapshot.seq()),
        };
        let ledger_cancel = event_ledger::ledger::CancellationToken::new();
        let envelope = self
            .ledger
            .append(
                session_id,
                actor.clone(),
                kind,
                payload,
                &options,
                &ledger_cancel,
            )
            .map_err(map_ledger)?;
        check_cancel(cancel)?;
        let erased = envelope.erase().map_err(|_| RecoveryError::Internal)?;
        apply(Some(snapshot), &erased).map_err(|err| map_projection(err, session_id))
    }
}

/// Load checkpoint/replay events, apply recovery actions, return a safe snapshot.
pub fn recover_session(
    manager: &RecoveryManager,
    session_id: SessionId,
    cancel: &CancellationToken,
) -> Result<SessionSnapshot, RecoveryError> {
    manager.recover_session(session_id, cancel)
}

/// Recovery hook that parks an active autonomous goal after process restart.
pub fn park_recovered_goal(goal_id: GoalId) -> RecoveryAction {
    RecoveryAction::PauseGoal {
        goal_id,
        reason: GoalStopReason::ProcessRecovered,
    }
}

fn verify_loaded_checkpoint(
    session_id: SessionId,
    events: &[ErasedEventEnvelope],
    loaded: &LoadedCheckpoint,
    cancel: &CancellationToken,
) -> Result<(), RecoveryError> {
    check_cancel(cancel)?;
    let through_seq = loaded.through_seq();
    if through_seq == 0 || through_seq > events.len() as u64 {
        return Err(RecoveryError::StorageCorrupt);
    }
    let decoded = match serde_json::from_slice::<SessionSnapshot>(loaded.projection()) {
        Ok(snapshot) => snapshot,
        Err(_) => return Ok(()),
    };
    if decoded.id() != session_id
        || decoded.schema() != SESSION_SNAPSHOT_SCHEMA
        || decoded.seq() != through_seq
    {
        return Err(RecoveryError::StorageCorrupt);
    }
    let prefix = replay(&events[..through_seq as usize], cancel).map_err(map_replay)?;
    if prefix != decoded {
        return Err(RecoveryError::StorageCorrupt);
    }
    Ok(())
}

fn action_event(action: &RecoveryAction) -> (EventKind, Value) {
    match action {
        RecoveryAction::InterruptModel { request_id, .. } => (
            EventKind::ModelFailed,
            interrupt_payload("request_id", request_id),
        ),
        RecoveryAction::InterruptTool { call_id, .. } => {
            (EventKind::ToolFailed, interrupt_payload("call_id", call_id))
        }
        RecoveryAction::InterruptTurn { turn_id } => {
            (EventKind::TurnInterrupted, id_payload("turn_id", *turn_id))
        }
        RecoveryAction::PauseGoal { goal_id, reason } => {
            (EventKind::GoalPaused, goal_pause_payload(*goal_id, *reason))
        }
    }
}

fn interrupt_payload(field: &'static str, id: &Option<String>) -> Value {
    let mut payload = serde_json::Map::new();
    if let Some(id) = id {
        payload.insert(field.to_owned(), Value::String(id.clone()));
    }
    payload.insert(
        "reason".to_owned(),
        Value::String(GoalStopReason::ProcessRecovered.as_str().to_owned()),
    );
    Value::Object(payload)
}

fn id_payload(field: &'static str, id: impl serde::Serialize) -> Value {
    json!({ field: id })
}

fn goal_pause_payload(goal_id: GoalId, reason: GoalStopReason) -> Value {
    json!({
        "goal_id": goal_id,
        "process_recovered": reason == GoalStopReason::ProcessRecovered,
    })
}

fn system_actor() -> Result<ActorRef, RecoveryError> {
    ActorRef::new(ActorKind::System, &EventId::new().to_string())
        .map_err(|_| RecoveryError::Internal)
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), RecoveryError> {
    if cancel.is_cancelled() {
        Err(RecoveryError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_open(err: LedgerError) -> RecoveryError {
    match err {
        LedgerError::Cancelled => RecoveryError::Cancelled,
        LedgerError::Corrupt(_)
        | LedgerError::Migration(_)
        | LedgerError::ForeignKeysDisabled
        | LedgerError::InvalidTimestamp => RecoveryError::StorageCorrupt,
        _ => RecoveryError::Internal,
    }
}

fn map_ledger(err: LedgerError) -> RecoveryError {
    match err {
        LedgerError::Cancelled => RecoveryError::Cancelled,
        LedgerError::SessionNotFound { session_id } => RecoveryError::NotFound { session_id },
        LedgerError::SessionExists { session_id }
        | LedgerError::SequenceConflict { session_id, .. } => {
            RecoveryError::Conflict { session_id }
        }
        LedgerError::Corrupt(_)
        | LedgerError::ForeignKeysDisabled
        | LedgerError::InvalidTimestamp => RecoveryError::StorageCorrupt,
        LedgerError::EventNotFound { session_id, .. } => RecoveryError::NotFound { session_id },
        LedgerError::NotCommitted
        | LedgerError::SequenceExhausted
        | LedgerError::PayloadBound { .. }
        | LedgerError::Migration(_)
        | LedgerError::Sqlite(_)
        | LedgerError::Json(_)
        | LedgerError::Io(_) => RecoveryError::Internal,
    }
}

fn map_load_event(err: LedgerError, session_id: SessionId) -> RecoveryError {
    match err {
        LedgerError::EventNotFound { .. } => RecoveryError::StorageCorrupt,
        other => {
            let mapped = map_ledger(other);
            if matches!(mapped, RecoveryError::NotFound { .. }) {
                RecoveryError::NotFound { session_id }
            } else {
                mapped
            }
        }
    }
}

fn map_checkpoint(err: CheckpointError) -> RecoveryError {
    match err {
        CheckpointError::Cancelled => RecoveryError::Cancelled,
        CheckpointError::SessionNotFound { session_id } => RecoveryError::NotFound { session_id },
        CheckpointError::Corrupt(_)
        | CheckpointError::ForeignKeysDisabled
        | CheckpointError::InvalidTimestamp => RecoveryError::StorageCorrupt,
        CheckpointError::Conflict { session_id, .. } => RecoveryError::Conflict { session_id },
        _ => RecoveryError::Internal,
    }
}

fn map_classify(err: ClassifyError) -> RecoveryError {
    match err {
        ClassifyError::Cancelled => RecoveryError::Cancelled,
        ClassifyError::TooManyEvents | ClassifyError::TooManyInflight => {
            RecoveryError::TooManyEvents
        }
        ClassifyError::InvalidField { .. } | ClassifyError::FieldTooLong { .. } => {
            RecoveryError::StorageCorrupt
        }
        ClassifyError::Projection(inner) => map_replay(inner),
    }
}

fn map_replay(err: ProjectionError) -> RecoveryError {
    match err {
        ProjectionError::Cancelled => RecoveryError::Cancelled,
        ProjectionError::TooManyEvents => RecoveryError::TooManyEvents,
        ProjectionError::Invariant(ProjectionInvariant::SessionNotCreated) => {
            RecoveryError::StorageCorrupt
        }
        ProjectionError::Invariant(_) => RecoveryError::StorageCorrupt,
    }
}

fn map_projection(err: ProjectionError, session_id: SessionId) -> RecoveryError {
    match err {
        ProjectionError::Invariant(ProjectionInvariant::SessionNotCreated) => {
            RecoveryError::NotFound { session_id }
        }
        other => map_replay(other),
    }
}

impl RecoveryError {
    /// Public error code when this failure has a wire mapping.
    ///
    /// [`RecoveryError::Cancelled`] has no public code.
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

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("session recovery cancelled"),
            Self::NotFound { session_id } => write!(f, "session {session_id} not found"),
            Self::Conflict { session_id } => write!(f, "session {session_id} conflict"),
            Self::TooManyEvents => f.write_str("session event stream exceeds the replay bound"),
            Self::StorageCorrupt => f.write_str("session store is corrupt"),
            Self::Internal => f.write_str("session recovery failed internally"),
        }
    }
}

impl Error for RecoveryError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::projection::GoalState;
    use crate::session::service::{CreateSession, SessionService};
    use event_ledger::artifact_store::ArtifactStore;
    use event_ledger::event::ActorKind;
    use protocol::{EventId, ProjectId};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    const TURN_ID: &str = "019c0000-0000-7000-8000-000000000012";
    const GOAL_ID: &str = "019c0000-0000-7000-8000-000000000014";
    const MODEL_REQUEST: &str = "model-req-1";
    const TOOL_CALL: &str = "call_7";

    struct TempRecovery {
        db_path: PathBuf,
        artifact_root: Option<PathBuf>,
        manager: RecoveryManager,
        sessions: SessionService,
        ledger: EventLedger,
    }

    impl TempRecovery {
        fn create() -> Self {
            let paths = temp_paths();
            remove_db_files(&paths.db);
            let ledger = EventLedger::open(&paths.db).expect("open ledger");
            let sessions = SessionService::new(ledger.clone());
            let manager = RecoveryManager::new(ledger.clone());
            Self {
                db_path: paths.db,
                artifact_root: None,
                manager,
                sessions,
                ledger,
            }
        }

        fn create_with_checkpoints() -> Self {
            let paths = temp_paths();
            remove_db_files(&paths.db);
            let _ = std::fs::remove_dir_all(&paths.artifacts);
            let ledger = EventLedger::open(&paths.db).expect("open ledger");
            let artifacts = ArtifactStore::create(&paths.artifacts).expect("artifacts");
            let checkpoints = CheckpointStore::new(ledger.clone(), artifacts);
            let sessions = SessionService::new(ledger.clone());
            let manager = RecoveryManager::new(ledger.clone()).with_checkpoints(checkpoints);
            Self {
                db_path: paths.db,
                artifact_root: Some(paths.artifacts),
                manager,
                sessions,
                ledger,
            }
        }

        fn checkpoint_store(&self) -> CheckpointStore {
            let root = self.artifact_root.as_ref().expect("artifact root");
            let artifacts = ArtifactStore::create(root).expect("reopen artifacts");
            CheckpointStore::new(self.ledger.clone(), artifacts)
        }
    }

    impl Drop for TempRecovery {
        fn drop(&mut self) {
            remove_db_files(&self.db_path);
            if let Some(root) = &self.artifact_root {
                let _ = std::fs::remove_dir_all(root);
            }
        }
    }

    struct TempPaths {
        db: PathBuf,
        artifacts: PathBuf,
    }

    fn temp_paths() -> TempPaths {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        TempPaths {
            db: std::env::temp_dir().join(format!("rapidlm-recovery-{pid}-{seq}.sqlite")),
            artifacts: std::env::temp_dir().join(format!("rapidlm-recovery-art-{pid}-{seq}")),
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

    fn options() -> AppendOptions {
        AppendOptions {
            redaction: RedactionClass::Project,
            trace_id: TraceId::new(),
            expected_seq: None,
        }
    }

    fn ledger_live() -> event_ledger::ledger::CancellationToken {
        event_ledger::ledger::CancellationToken::new()
    }

    fn append(tmp: &TempRecovery, session: SessionId, kind: EventKind, payload: Value) {
        tmp.ledger
            .append(session, actor(), kind, payload, &options(), &ledger_live())
            .expect("append");
    }

    fn kinds_from(tmp: &TempRecovery, session: SessionId, from_seq: u64) -> Vec<EventKind> {
        let last = tmp
            .ledger
            .last_seq(session, &ledger_live())
            .expect("last_seq");
        let mut kinds = Vec::new();
        for seq in from_seq..=last {
            kinds.push(
                tmp.ledger
                    .get(session, seq, &ledger_live())
                    .expect("get")
                    .kind(),
            );
        }
        kinds
    }

    fn goal_id() -> GoalId {
        GOAL_ID.parse().expect("goal")
    }

    #[test]
    fn recover_session_parks_active_goal_via_hook() {
        let tmp = TempRecovery::create();
        let created = tmp
            .sessions
            .create_session(create_req(), &live())
            .expect("create");
        append(
            &tmp,
            created.id(),
            EventKind::GoalCreated,
            json!({"goal_id": GOAL_ID, "statement": "keep going"}),
        );
        append(
            &tmp,
            created.id(),
            EventKind::TurnStarted,
            json!({"turn_id": TURN_ID}),
        );

        let recovered = recover_session(&tmp.manager, created.id(), &live()).expect("recover");
        assert_eq!(recovered.status(), SessionStatus::Paused);
        assert!(recovered.active_turn().is_none());
        let goal = recovered.top_level_goal().expect("goal");
        assert_eq!(goal.id(), goal_id());
        assert_eq!(goal.state(), GoalState::Paused);
        assert_eq!(goal.stop_reason(), Some(GoalStopReason::ProcessRecovered));
        assert_eq!(
            park_recovered_goal(goal_id()),
            RecoveryAction::PauseGoal {
                goal_id: goal_id(),
                reason: GoalStopReason::ProcessRecovered,
            }
        );
        assert_eq!(
            kinds_from(&tmp, created.id(), 4),
            vec![
                EventKind::SessionRecovered,
                EventKind::TurnInterrupted,
                EventKind::GoalPaused,
            ]
        );
        let paused = tmp
            .ledger
            .get(created.id(), 6, &ledger_live())
            .expect("goal.paused");
        assert_eq!(
            paused
                .payload()
                .get("process_recovered")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(paused.actor().kind(), ActorKind::System);
    }

    #[test]
    fn recover_session_interrupts_inflight_model_without_success() {
        let tmp = TempRecovery::create();
        let created = tmp
            .sessions
            .create_session(create_req(), &live())
            .expect("create");
        append(
            &tmp,
            created.id(),
            EventKind::TurnStarted,
            json!({"turn_id": TURN_ID}),
        );
        append(
            &tmp,
            created.id(),
            EventKind::ModelRequested,
            json!({"request_id": MODEL_REQUEST}),
        );
        append(
            &tmp,
            created.id(),
            EventKind::ModelStreamDelta,
            json!({"request_id": MODEL_REQUEST, "chunk": "partial"}),
        );

        let recovered = tmp
            .manager
            .recover_session(created.id(), &live())
            .expect("recover");
        assert_eq!(recovered.status(), SessionStatus::Ready);
        assert!(recovered.active_turn().is_none());
        assert_eq!(
            kinds_from(&tmp, created.id(), 5),
            vec![
                EventKind::SessionRecovered,
                EventKind::ModelFailed,
                EventKind::TurnInterrupted,
            ]
        );
        assert!(!kinds_from(&tmp, created.id(), 1).contains(&EventKind::ModelCompleted));
        let failed = tmp
            .ledger
            .get(created.id(), 6, &ledger_live())
            .expect("model.failed");
        assert_eq!(
            failed.payload().get("request_id").and_then(Value::as_str),
            Some(MODEL_REQUEST)
        );
    }

    #[test]
    fn recover_session_interrupts_inflight_tool() {
        let tmp = TempRecovery::create();
        let created = tmp
            .sessions
            .create_session(create_req(), &live())
            .expect("create");
        append(
            &tmp,
            created.id(),
            EventKind::TurnStarted,
            json!({"turn_id": TURN_ID}),
        );
        append(
            &tmp,
            created.id(),
            EventKind::ToolStarted,
            json!({"call_id": TOOL_CALL}),
        );

        let recovered = tmp
            .manager
            .recover_session(created.id(), &live())
            .expect("recover");
        assert!(recovered.active_turn().is_none());
        assert_eq!(
            kinds_from(&tmp, created.id(), 4),
            vec![
                EventKind::SessionRecovered,
                EventKind::ToolFailed,
                EventKind::TurnInterrupted,
            ]
        );
        assert!(!kinds_from(&tmp, created.id(), 1).contains(&EventKind::ToolCompleted));
    }

    #[test]
    fn recover_session_is_idempotent_across_repeated_startup() {
        let tmp = TempRecovery::create();
        let created = tmp
            .sessions
            .create_session(create_req(), &live())
            .expect("create");
        append(
            &tmp,
            created.id(),
            EventKind::GoalCreated,
            json!({"goal_id": GOAL_ID, "statement": "autonomous"}),
        );
        append(
            &tmp,
            created.id(),
            EventKind::TurnStarted,
            json!({"turn_id": TURN_ID}),
        );

        let first = tmp
            .manager
            .recover_session(created.id(), &live())
            .expect("first recover");
        let first_seq = tmp
            .ledger
            .last_seq(created.id(), &ledger_live())
            .expect("seq after first");
        let second = tmp
            .manager
            .recover_session(created.id(), &live())
            .expect("second recover");
        let second_seq = tmp
            .ledger
            .last_seq(created.id(), &ledger_live())
            .expect("seq after second");

        assert_eq!(first, second);
        assert_eq!(first_seq, second_seq);
        assert_eq!(second.status(), SessionStatus::Paused);
        assert_eq!(
            second.top_level_goal().expect("goal").stop_reason(),
            Some(GoalStopReason::ProcessRecovered)
        );
        assert_eq!(
            kinds_from(&tmp, created.id(), 4)
                .iter()
                .filter(|kind| **kind == EventKind::GoalPaused)
                .count(),
            1
        );
    }

    #[test]
    fn recover_session_ready_session_does_not_append() {
        let tmp = TempRecovery::create();
        let created = tmp
            .sessions
            .create_session(create_req(), &live())
            .expect("create");
        let recovered = tmp
            .manager
            .recover_session(created.id(), &live())
            .expect("recover");
        assert_eq!(recovered, created);
        assert_eq!(
            tmp.ledger
                .last_seq(created.id(), &ledger_live())
                .expect("last"),
            1
        );
    }

    #[test]
    fn recover_session_closed_is_noop() {
        let tmp = TempRecovery::create();
        let created = tmp
            .sessions
            .create_session(create_req(), &live())
            .expect("create");
        append(&tmp, created.id(), EventKind::SessionClosed, json!({}));
        let recovered = tmp
            .manager
            .recover_session(created.id(), &live())
            .expect("recover");
        assert_eq!(recovered.status(), SessionStatus::Closed);
        assert_eq!(
            tmp.ledger
                .last_seq(created.id(), &ledger_live())
                .expect("last"),
            2
        );
    }

    #[test]
    fn recover_session_unknown_is_not_found() {
        let tmp = TempRecovery::create();
        let missing = SessionId::new();
        let err = tmp
            .manager
            .recover_session(missing, &live())
            .expect_err("unknown");
        assert_eq!(
            err,
            RecoveryError::NotFound {
                session_id: missing
            }
        );
        assert_eq!(err.code(), Some(ErrorCode::SessionNotFound));
        let api = err
            .into_api_error(TraceId::new())
            .expect("public session.not_found");
        assert_eq!(api.code().as_str(), "session.not_found");
    }

    #[test]
    fn recover_session_observes_cancellation() {
        let tmp = TempRecovery::create();
        let created = tmp
            .sessions
            .create_session(create_req(), &live())
            .expect("create");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tmp
            .manager
            .recover_session(created.id(), &cancel)
            .expect_err("cancelled");
        assert_eq!(err, RecoveryError::Cancelled);
        assert!(err.code().is_none());
        assert!(err.into_api_error(TraceId::new()).is_none());
        assert_eq!(
            tmp.ledger
                .last_seq(created.id(), &ledger_live())
                .expect("last"),
            1
        );
    }

    #[test]
    fn recover_session_loads_checkpoint_then_replays() {
        let tmp = TempRecovery::create_with_checkpoints();
        let created = tmp
            .sessions
            .create_session(create_req(), &live())
            .expect("create");
        let bytes = serde_json::to_vec(&created).expect("snapshot bytes");
        tmp.checkpoint_store()
            .write_checkpoint(
                created.id(),
                created.seq(),
                i32::from(SESSION_SNAPSHOT_SCHEMA),
                &bytes,
                &event_ledger::checkpoint::CancellationToken::new(),
            )
            .expect("write checkpoint");
        append(
            &tmp,
            created.id(),
            EventKind::GoalCreated,
            json!({"goal_id": GOAL_ID, "statement": "after checkpoint"}),
        );
        append(
            &tmp,
            created.id(),
            EventKind::TurnStarted,
            json!({"turn_id": TURN_ID}),
        );

        let recovered = tmp
            .manager
            .recover_session(created.id(), &live())
            .expect("recover");
        assert_eq!(recovered.status(), SessionStatus::Paused);
        assert_eq!(
            recovered.top_level_goal().expect("goal").state(),
            GoalState::Paused
        );
        assert_eq!(
            kinds_from(&tmp, created.id(), 4),
            vec![
                EventKind::SessionRecovered,
                EventKind::TurnInterrupted,
                EventKind::GoalPaused,
            ]
        );
    }

    #[test]
    fn recover_session_checkpoint_mismatch_is_corrupt() {
        let tmp = TempRecovery::create_with_checkpoints();
        let created = tmp
            .sessions
            .create_session(create_req(), &live())
            .expect("create");
        let forged = json!({
            "schema": SESSION_SNAPSHOT_SCHEMA,
            "id": created.id(),
            "project_id": ProjectId::new(),
            "status": "ready",
            "active_turn": null,
            "top_level_goal": null,
            "active_agents": [],
            "seq": 1,
            "created_at": created.created_at(),
            "updated_at": created.updated_at(),
        });
        tmp.checkpoint_store()
            .write_checkpoint(
                created.id(),
                1,
                i32::from(SESSION_SNAPSHOT_SCHEMA),
                &serde_json::to_vec(&forged).expect("forged bytes"),
                &event_ledger::checkpoint::CancellationToken::new(),
            )
            .expect("write forged checkpoint");

        let err = tmp
            .manager
            .recover_session(created.id(), &live())
            .expect_err("mismatch");
        assert_eq!(err, RecoveryError::StorageCorrupt);
        assert_eq!(err.code(), Some(ErrorCode::StorageCorrupt));
        assert_eq!(
            tmp.ledger
                .last_seq(created.id(), &ledger_live())
                .expect("last"),
            1
        );
    }

    #[test]
    fn recover_session_unusable_checkpoint_falls_back_to_replay() {
        let tmp = TempRecovery::create_with_checkpoints();
        let created = tmp
            .sessions
            .create_session(create_req(), &live())
            .expect("create");
        tmp.checkpoint_store()
            .write_checkpoint(
                created.id(),
                1,
                i32::from(SESSION_SNAPSHOT_SCHEMA),
                b"not-a-snapshot",
                &event_ledger::checkpoint::CancellationToken::new(),
            )
            .expect("write opaque checkpoint");
        append(
            &tmp,
            created.id(),
            EventKind::TurnStarted,
            json!({"turn_id": TURN_ID}),
        );

        let recovered = tmp
            .manager
            .recover_session(created.id(), &live())
            .expect("replay fallback");
        assert!(recovered.active_turn().is_none());
        assert_eq!(recovered.status(), SessionStatus::Ready);
        assert!(kinds_from(&tmp, created.id(), 1).contains(&EventKind::TurnInterrupted));
    }
}
