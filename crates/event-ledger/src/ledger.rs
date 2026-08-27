//! Transactional event append with monotonic per-session sequence.
//!
//! `append` allocates `seq` inside one IMMEDIATE SQLite transaction and
//! returns an envelope only after that transaction commits. `synchronous=FULL`
//! is set on every connection so a successful return is durable.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use protocol::{EventId, ProjectId, RedactionClass, SessionId, TraceId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::Value;

use crate::event::{
    ActorRef, ErasedEventEnvelope, EventEnvelope, EventKind, EventKindParseError, RecordedAt,
};
use crate::migrations::{MigrationError, MigrationRunner};

/// Maximum UTF-8 bytes accepted in a serialized event payload.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

/// Bounded SQLite lock wait. Matches the ledger busy-timeout recovery rule.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// File-backed event ledger. Connections are opened per operation so writers
/// serialize through SQLite IMMEDIATE transactions rather than a process lock.
#[derive(Clone, Debug)]
pub struct EventLedger {
    path: PathBuf,
    fail_before_commit: Arc<AtomicBool>,
}

/// Attribution and concurrency metadata for [`EventLedger::append`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendOptions {
    pub redaction: RedactionClass,
    pub trace_id: TraceId,
    pub expected_seq: Option<u64>,
}

/// Cooperative cancellation for ledger operations.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Typed failures for ledger append and session-row operations.
#[derive(Debug)]
pub enum LedgerError {
    Cancelled,
    SessionNotFound {
        session_id: SessionId,
    },
    SessionExists {
        session_id: SessionId,
    },
    SequenceConflict {
        session_id: SessionId,
        expected: u64,
        actual: u64,
    },
    SequenceExhausted,
    EventNotFound {
        session_id: SessionId,
        seq: u64,
    },
    PayloadBound {
        limit: usize,
        observed: usize,
    },
    /// Insert ran but the transaction did not commit; the event is not durable.
    NotCommitted,
    Corrupt(&'static str),
    ForeignKeysDisabled,
    InvalidTimestamp,
    Migration(MigrationError),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<(), LedgerError> {
        if self.is_cancelled() {
            Err(LedgerError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

/// One row of the cross-session listing used by inspectors and the CLI.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    pub session_id: String,
    pub last_seq: u64,
    pub first_seen: String,
}

impl EventLedger {
    /// Open (or create) a file-backed ledger and apply migrations.
    ///
    /// In-memory databases are rejected because they cannot enable WAL.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        MigrationRunner::apply(&conn)?;
        drop(conn);
        Ok(Self {
            path: path.to_path_buf(),
            fail_before_commit: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert the `sessions` row required by the events foreign key.
    ///
    /// Durable before success. Does not append `session.created`.
    pub fn create_session(
        &self,
        session_id: SessionId,
        project_id: ProjectId,
        cancel: &CancellationToken,
    ) -> Result<(), LedgerError> {
        cancel.check()?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        cancel.check()?;
        let created_at = read_recorded_at(&tx)?;
        match tx.execute(
            "INSERT INTO sessions (id, project_id, created_at, closed_at)
             VALUES (?1, ?2, ?3, NULL)",
            params![
                session_id.to_string(),
                project_id.to_string(),
                created_at.as_str()
            ],
        ) {
            Ok(1) => {}
            Ok(_) => {
                return Err(LedgerError::Corrupt(
                    "session insert did not affect one row",
                ));
            }
            Err(err) if is_constraint(&err) => {
                return Err(LedgerError::SessionExists { session_id });
            }
            Err(err) => return Err(err.into()),
        }
        cancel.check()?;
        if self.fail_before_commit.swap(false, Ordering::SeqCst) {
            return Err(LedgerError::NotCommitted);
        }
        tx.commit()?;
        Ok(())
    }

    /// Append one event. `seq` is allocated inside the same transaction that
    /// inserts the row; success is returned only after commit.
    pub fn append<P: Serialize>(
        &self,
        session: SessionId,
        actor: ActorRef,
        kind: EventKind,
        payload: P,
        options: &AppendOptions,
        cancel: &CancellationToken,
    ) -> Result<EventEnvelope<P>, LedgerError> {
        cancel.check()?;
        let payload_json = serialize_payload(&payload)?;
        let actor_json = serde_json::to_string(&actor)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        cancel.check()?;
        ensure_session(&tx, session)?;
        let last = last_seq_tx(&tx, session)?;
        match options.expected_seq {
            Some(expected) if last != expected => {
                return Err(LedgerError::SequenceConflict {
                    session_id: session,
                    expected,
                    actual: last,
                });
            }
            _ => {}
        }
        let seq = next_seq(last)?;
        let event_id = EventId::new();
        let recorded_at = read_recorded_at(&tx)?;
        match tx.execute(
            "INSERT INTO events (
                session_id, seq, event_id, recorded_at, actor_json,
                trace_id, kind, redaction, payload_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session.to_string(),
                seq as i64,
                event_id.to_string(),
                recorded_at.as_str(),
                actor_json,
                options.trace_id.to_string(),
                kind.as_str(),
                options.redaction.as_str(),
                payload_json,
            ],
        ) {
            Ok(1) => {}
            Ok(_) => return Err(LedgerError::Corrupt("event insert did not affect one row")),
            Err(err) => return Err(err.into()),
        }
        cancel.check()?;
        if self.fail_before_commit.swap(false, Ordering::SeqCst) {
            return Err(LedgerError::NotCommitted);
        }
        tx.commit()?;
        Ok(EventEnvelope::new(
            event_id,
            session,
            seq,
            recorded_at,
            actor,
            options.trace_id,
            kind,
            options.redaction,
            payload,
        ))
    }

    /// Highest committed `seq` for `session`, or `0` when the session has none.
    pub fn last_seq(
        &self,
        session: SessionId,
        cancel: &CancellationToken,
    ) -> Result<u64, LedgerError> {
        cancel.check()?;
        let conn = self.connect()?;
        ensure_session(&conn, session)?;
        last_seq_tx(&conn, session)
    }

    /// Load one committed event. Missing `(session, seq)` is [`LedgerError::EventNotFound`].
    /// Cross-session index for the sessions inspector: one row per session
    /// ever written, ordered by first activity.
    pub fn list_sessions(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<SessionSummary>, LedgerError> {
        
        cancel.check()?;
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, COALESCE(MAX(seq),0), MIN(recorded_at)
             FROM events GROUP BY session_id ORDER BY MIN(recorded_at)",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (session_id, last_seq, first_seen) = row?;
            out.push(SessionSummary {
                session_id,
                last_seq: last_seq.max(0) as u64,
                first_seen,
            });
        }
        drop(stmt);
        drop(conn);
        Ok(out)
    }

    pub fn get(
        &self,
        session: SessionId,
        seq: u64,
        cancel: &CancellationToken,
    ) -> Result<ErasedEventEnvelope, LedgerError> {
        cancel.check()?;
        if seq == 0 || seq > i64::MAX as u64 {
            return Err(LedgerError::EventNotFound {
                session_id: session,
                seq,
            });
        }
        let conn = self.connect()?;
        ensure_session(&conn, session)?;
        let row = conn
            .query_row(
                "SELECT event_id, recorded_at, actor_json, trace_id, kind, redaction, payload_json
                 FROM events WHERE session_id = ?1 AND seq = ?2",
                params![session.to_string(), seq as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((event_id, recorded_at, actor_json, trace_id, kind, redaction, payload_json)) =
            row
        else {
            return Err(LedgerError::EventNotFound {
                session_id: session,
                seq,
            });
        };
        envelope_from_row(
            session,
            seq,
            StoredEventRow {
                event_id,
                recorded_at,
                actor_json,
                trace_id,
                kind,
                redaction,
                payload_json,
            },
        )
    }

    /// Arm a one-shot rollback after a successful INSERT and before COMMIT.
    ///
    /// Used to prove that a pre-commit failure cannot acknowledge an event.
    #[cfg(test)]
    pub fn inject_fail_before_commit(&self) {
        self.fail_before_commit.store(true, Ordering::SeqCst);
    }

    fn connect(&self) -> Result<Connection, LedgerError> {
        let conn = Connection::open(&self.path)?;
        configure_connection(&conn)?;
        Ok(conn)
    }
}

fn configure_connection(conn: &Connection) -> Result<(), LedgerError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", 1)?;
    let foreign_keys: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(LedgerError::ForeignKeysDisabled);
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn ensure_session(conn: &Connection, session_id: SessionId) -> Result<(), LedgerError> {
    let found = conn
        .query_row(
            "SELECT 1 FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |_| Ok(()),
        )
        .optional()?;
    if found.is_some() {
        Ok(())
    } else {
        Err(LedgerError::SessionNotFound { session_id })
    }
}

fn last_seq_tx(conn: &Connection, session_id: SessionId) -> Result<u64, LedgerError> {
    let last: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
        [session_id.to_string()],
        |row| row.get(0),
    )?;
    if last < 0 {
        return Err(LedgerError::Corrupt("negative event seq"));
    }
    Ok(last as u64)
}

fn next_seq(last: u64) -> Result<u64, LedgerError> {
    let next = last.checked_add(1).ok_or(LedgerError::SequenceExhausted)?;
    if next > i64::MAX as u64 {
        return Err(LedgerError::SequenceExhausted);
    }
    Ok(next)
}

fn read_recorded_at(conn: &Connection) -> Result<RecordedAt, LedgerError> {
    let raw: String =
        conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
            row.get(0)
        })?;
    raw.parse().map_err(|_| LedgerError::InvalidTimestamp)
}

fn serialize_payload<P: Serialize>(payload: &P) -> Result<String, LedgerError> {
    let payload_json = serde_json::to_string(payload)?;
    let observed = payload_json.len();
    if observed > MAX_PAYLOAD_BYTES {
        return Err(LedgerError::PayloadBound {
            limit: MAX_PAYLOAD_BYTES,
            observed,
        });
    }
    Ok(payload_json)
}

struct StoredEventRow {
    event_id: String,
    recorded_at: String,
    actor_json: String,
    trace_id: String,
    kind: String,
    redaction: String,
    payload_json: String,
}

fn envelope_from_row(
    session_id: SessionId,
    seq: u64,
    row: StoredEventRow,
) -> Result<ErasedEventEnvelope, LedgerError> {
    let event_id: EventId = row
        .event_id
        .parse()
        .map_err(|_| LedgerError::Corrupt("malformed stored event_id"))?;
    let recorded_at: RecordedAt = row
        .recorded_at
        .parse()
        .map_err(|_| LedgerError::Corrupt("malformed stored recorded_at"))?;
    let actor: ActorRef = serde_json::from_str(&row.actor_json)
        .map_err(|_| LedgerError::Corrupt("malformed stored actor_json"))?;
    let trace_id: TraceId = row
        .trace_id
        .parse()
        .map_err(|_| LedgerError::Corrupt("malformed stored trace_id"))?;
    let kind: EventKind = row
        .kind
        .parse()
        .map_err(|_: EventKindParseError| LedgerError::Corrupt("malformed stored kind"))?;
    let redaction: RedactionClass = row
        .redaction
        .parse()
        .map_err(|_| LedgerError::Corrupt("malformed stored redaction"))?;
    let payload: Value = serde_json::from_str(&row.payload_json)
        .map_err(|_| LedgerError::Corrupt("malformed stored payload_json"))?;
    Ok(EventEnvelope::new(
        event_id,
        session_id,
        seq,
        recorded_at,
        actor,
        trace_id,
        kind,
        redaction,
        payload,
    ))
}

fn is_constraint(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ConstraintViolation)
    )
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("ledger operation cancelled"),
            Self::SessionNotFound { session_id } => {
                write!(f, "session {session_id} not found")
            }
            Self::SessionExists { session_id } => {
                write!(f, "session {session_id} already exists")
            }
            Self::SequenceConflict {
                session_id,
                expected,
                actual,
            } => write!(
                f,
                "session {session_id} sequence conflict: expected {expected}, actual {actual}"
            ),
            Self::SequenceExhausted => f.write_str("session event sequence is exhausted"),
            Self::EventNotFound { session_id, seq } => {
                write!(f, "event {session_id}#{seq} not found")
            }
            Self::PayloadBound { limit, observed } => {
                write!(
                    f,
                    "event payload exceeds bound {limit}, observed {observed}"
                )
            }
            Self::NotCommitted => {
                f.write_str("event insert was not committed and must not be acknowledged")
            }
            Self::Corrupt(reason) => write!(f, "ledger data is corrupt: {reason}"),
            Self::ForeignKeysDisabled => f.write_str("sqlite foreign_keys pragma is disabled"),
            Self::InvalidTimestamp => f.write_str("sqlite produced a non-canonical recorded_at"),
            Self::Migration(err) => write!(f, "ledger migration error: {err}"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::Json(err) => write!(f, "event json error: {err}"),
            Self::Io(err) => write!(f, "ledger io error: {err}"),
        }
    }
}

impl std::error::Error for LedgerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Migration(err) => Some(err),
            Self::Sqlite(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::Cancelled
            | Self::SessionNotFound { .. }
            | Self::SessionExists { .. }
            | Self::SequenceConflict { .. }
            | Self::SequenceExhausted
            | Self::EventNotFound { .. }
            | Self::PayloadBound { .. }
            | Self::NotCommitted
            | Self::Corrupt(_)
            | Self::ForeignKeysDisabled
            | Self::InvalidTimestamp => None,
        }
    }
}

impl From<MigrationError> for LedgerError {
    fn from(value: MigrationError) -> Self {
        Self::Migration(value)
    }
}

impl From<rusqlite::Error> for LedgerError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<serde_json::Error> for LedgerError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<std::io::Error> for LedgerError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ActorKind;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicU64;
    use std::thread;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempLedger {
        path: PathBuf,
        ledger: EventLedger,
    }

    impl TempLedger {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-event-ledger-{}-{seq}.sqlite",
                std::process::id()
            ));
            remove_db_files(&path);
            let ledger = EventLedger::open(&path).expect("open ledger");
            Self { path, ledger }
        }
    }

    impl Drop for TempLedger {
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

    fn options() -> AppendOptions {
        AppendOptions {
            redaction: RedactionClass::Project,
            trace_id: TraceId::new(),
            expected_seq: None,
        }
    }

    fn actor() -> ActorRef {
        ActorRef::new(ActorKind::System, &EventId::new().to_string()).expect("actor")
    }

    fn seed_session(ledger: &EventLedger) -> SessionId {
        let session = SessionId::new();
        ledger
            .create_session(session, ProjectId::new(), &live())
            .expect("create session");
        session
    }

    fn append_n(ledger: &EventLedger, session: SessionId, n: usize) -> Vec<u64> {
        let mut seqs = Vec::with_capacity(n);
        for i in 0..n {
            let envelope = ledger
                .append(
                    session,
                    actor(),
                    EventKind::TurnStarted,
                    serde_json::json!({ "i": i }),
                    &options(),
                    &live(),
                )
                .expect("append");
            seqs.push(envelope.seq());
        }
        seqs
    }

    fn committed_seqs(ledger: &EventLedger, session: SessionId) -> Vec<u64> {
        let last = ledger.last_seq(session, &live()).expect("last_seq");
        (1..=last)
            .map(|seq| {
                let event = ledger.get(session, seq, &live()).expect("get committed");
                event.seq()
            })
            .collect()
    }

    #[test]
    fn append_allocates_monotonic_seq_inside_transaction() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        assert_eq!(tmp.ledger.last_seq(session, &live()).expect("empty"), 0);

        let first = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::SessionCreated,
                serde_json::json!({}),
                &AppendOptions {
                    expected_seq: Some(0),
                    ..options()
                },
                &live(),
            )
            .expect("first");
        assert_eq!(first.seq(), 1);
        assert_eq!(first.session_id(), session);
        assert_eq!(first.kind(), EventKind::SessionCreated);

        let second = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnStarted,
                serde_json::json!({"k":"v"}),
                &options(),
                &live(),
            )
            .expect("second");
        assert_eq!(second.seq(), 2);
        assert_eq!(committed_seqs(&tmp.ledger, session), vec![1, 2]);
    }

    #[test]
    fn concurrent_append_produces_gap_free_unique_seqs() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        let n_threads = 16;
        let per_thread = 4;
        let barrier = Arc::new(Barrier::new(n_threads));
        let mut handles = Vec::with_capacity(n_threads);
        for t in 0..n_threads {
            let ledger = tmp.ledger.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                let mut seqs = Vec::with_capacity(per_thread);
                for i in 0..per_thread {
                    let envelope = ledger
                        .append(
                            session,
                            actor(),
                            EventKind::TurnStarted,
                            serde_json::json!({ "t": t, "i": i }),
                            &options(),
                            &live(),
                        )
                        .expect("concurrent append");
                    seqs.push(envelope.seq());
                }
                seqs
            }));
        }

        let mut seen = BTreeSet::new();
        for handle in handles {
            for seq in handle.join().expect("thread") {
                assert!(seen.insert(seq), "duplicate seq {seq}");
            }
        }
        let expected: BTreeSet<u64> = (1..=(n_threads * per_thread) as u64).collect();
        assert_eq!(seen, expected);
        assert_eq!(
            committed_seqs(&tmp.ledger, session),
            expected.into_iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn fail_before_commit_does_not_acknowledge_uncommitted_event() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        tmp.ledger
            .append(
                session,
                actor(),
                EventKind::TurnStarted,
                serde_json::json!({"ok": true}),
                &options(),
                &live(),
            )
            .expect("committed predecessor");

        tmp.ledger.inject_fail_before_commit();
        let err = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnCompleted,
                serde_json::json!({"ack": "must-not-happen"}),
                &options(),
                &live(),
            )
            .expect_err("injected pre-commit failure");
        assert!(
            matches!(err, LedgerError::NotCommitted),
            "acknowledged uncommitted event: {err}"
        );
        assert_eq!(tmp.ledger.last_seq(session, &live()).expect("last"), 1);
        let err = tmp
            .ledger
            .get(session, 2, &live())
            .expect_err("seq 2 must be absent");
        assert!(matches!(err, LedgerError::EventNotFound { seq: 2, .. }));

        let reopened = EventLedger::open(&tmp.path).expect("reopen after injected failure");
        assert_eq!(reopened.last_seq(session, &live()).expect("reopen last"), 1);
        let surviving = reopened.get(session, 1, &live()).expect("seq 1 durable");
        assert_eq!(surviving.kind(), EventKind::TurnStarted);
        let err = reopened
            .get(session, 2, &live())
            .expect_err("reopen must not see uncommitted");
        assert!(matches!(err, LedgerError::EventNotFound { seq: 2, .. }));
    }

    #[test]
    fn successful_append_is_durable_after_reopen() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        let written = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::ToolCompleted,
                serde_json::json!({"status":"ok"}),
                &options(),
                &live(),
            )
            .expect("append");
        assert_eq!(written.seq(), 1);

        let reopened = EventLedger::open(&tmp.path).expect("reopen");
        let loaded = reopened.get(session, 1, &live()).expect("reload");
        assert_eq!(loaded.event_id(), written.event_id());
        assert_eq!(loaded.seq(), 1);
        assert_eq!(loaded.kind(), EventKind::ToolCompleted);
        assert_eq!(loaded.redaction(), RedactionClass::Project);
        assert_eq!(loaded.payload()["status"], "ok");
        assert_eq!(reopened.last_seq(session, &live()).expect("last"), 1);
    }

    #[test]
    fn expected_seq_conflict_does_not_append() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        append_n(&tmp.ledger, session, 1);
        let err = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnStarted,
                serde_json::json!({}),
                &AppendOptions {
                    expected_seq: Some(0),
                    ..options()
                },
                &live(),
            )
            .expect_err("stale expected_seq");
        match err {
            LedgerError::SequenceConflict {
                session_id,
                expected,
                actual,
            } => {
                assert_eq!(session_id, session);
                assert_eq!(expected, 0);
                assert_eq!(actual, 1);
            }
            other => panic!("expected SequenceConflict, got {other}"),
        }
        assert_eq!(tmp.ledger.last_seq(session, &live()).expect("unchanged"), 1);

        let next = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnCompleted,
                serde_json::json!({}),
                &AppendOptions {
                    expected_seq: Some(1),
                    ..options()
                },
                &live(),
            )
            .expect("matching expected_seq");
        assert_eq!(next.seq(), 2);
    }

    #[test]
    fn missing_session_is_not_appended() {
        let tmp = TempLedger::create();
        let session = SessionId::new();
        let err = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnStarted,
                serde_json::json!({}),
                &options(),
                &live(),
            )
            .expect_err("missing session");
        assert!(matches!(
            err,
            LedgerError::SessionNotFound { session_id } if session_id == session
        ));
    }

    #[test]
    fn cancelled_append_is_not_acknowledged() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnStarted,
                serde_json::json!({}),
                &options(),
                &cancel,
            )
            .expect_err("cancelled");
        assert!(matches!(err, LedgerError::Cancelled));
        assert_eq!(tmp.ledger.last_seq(session, &live()).expect("empty"), 0);
    }

    #[test]
    fn payload_bound_is_enforced() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        let blob = "x".repeat(MAX_PAYLOAD_BYTES);
        let err = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::JobOutput,
                serde_json::json!({ "blob": blob }),
                &options(),
                &live(),
            )
            .expect_err("bound");
        assert!(
            matches!(err, LedgerError::PayloadBound { limit, .. } if limit == MAX_PAYLOAD_BYTES),
            "got {err}"
        );
        assert_eq!(tmp.ledger.last_seq(session, &live()).expect("empty"), 0);
    }

    #[test]
    fn sessions_have_independent_sequences() {
        let tmp = TempLedger::create();
        let a = seed_session(&tmp.ledger);
        let b = seed_session(&tmp.ledger);
        assert_eq!(append_n(&tmp.ledger, a, 2), vec![1, 2]);
        assert_eq!(append_n(&tmp.ledger, b, 1), vec![1]);
        assert_eq!(tmp.ledger.last_seq(a, &live()).expect("a"), 2);
        assert_eq!(tmp.ledger.last_seq(b, &live()).expect("b"), 1);
    }

    #[test]
    fn duplicate_session_row_is_rejected() {
        let tmp = TempLedger::create();
        let session = SessionId::new();
        let project = ProjectId::new();
        tmp.ledger
            .create_session(session, project, &live())
            .expect("first");
        let err = tmp
            .ledger
            .create_session(session, project, &live())
            .expect_err("duplicate");
        assert!(matches!(
            err,
            LedgerError::SessionExists { session_id } if session_id == session
        ));
    }
}
