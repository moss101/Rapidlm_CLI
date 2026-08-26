//! Migration compatibility: prior-schema fixtures open → migrate → replay.
//!
//! Covers upgrade from at least two frozen prior schema versions, refuses
//! unknown future schema, treats downgrade as restore-from-backup (not reverse
//! SQL), and asserts that a failed apply is transactional.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use event_ledger::artifact_store::ArtifactStore;
use event_ledger::checkpoint::CheckpointStore;
use event_ledger::event::{ActorKind, ActorRef, ErasedEventEnvelope, EventKind};
use event_ledger::ledger::{AppendOptions, CancellationToken, EventLedger, LedgerError};
use event_ledger::migrations::{
    CURRENT_SCHEMA_VERSION, INVALID_VERSION_RECOVERY, MigrationError, MigrationRunner,
    SchemaVersion, UNKNOWN_FUTURE_RECOVERY,
};
use protocol::{EventId, ProjectId, RedactionClass, SessionId, TraceId};
use rusqlite::Connection;
use serde_json::{Value, json};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Frozen v1 DDL from the schema that shipped before V2 projection tables.
const V1_CORE_SQL: &str = "
CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  closed_at TEXT
);

CREATE TABLE events (
  session_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  event_id TEXT NOT NULL UNIQUE,
  recorded_at TEXT NOT NULL,
  actor_json TEXT NOT NULL,
  trace_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  redaction TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  PRIMARY KEY(session_id, seq),
  FOREIGN KEY(session_id) REFERENCES sessions(id)
);

CREATE TABLE checkpoints (
  session_id TEXT NOT NULL,
  through_seq INTEGER NOT NULL,
  projection_schema INTEGER NOT NULL,
  artifact_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(session_id, through_seq)
);

CREATE TABLE artifacts (
  id TEXT PRIMARY KEY,
  media_type TEXT NOT NULL,
  bytes INTEGER NOT NULL,
  redaction TEXT NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT
);

CREATE TABLE capability_audit (
  lease_id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  agent_id TEXT,
  action_hash BLOB NOT NULL,
  capability TEXT NOT NULL,
  resource_scope_json TEXT NOT NULL,
  decision TEXT NOT NULL,
  policy_revision TEXT NOT NULL,
  issued_at TEXT NOT NULL,
  expires_at TEXT
);

CREATE TABLE memories (
  id TEXT PRIMARY KEY,
  scope TEXT NOT NULL,
  project_id TEXT,
  source_json TEXT NOT NULL,
  confidence REAL NOT NULL,
  content TEXT NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT
);

CREATE TABLE context_chunks (
  id TEXT PRIMARY KEY,
  repo_id TEXT NOT NULL,
  path TEXT NOT NULL,
  start_byte INTEGER NOT NULL,
  end_byte INTEGER NOT NULL,
  language TEXT,
  content_hash TEXT NOT NULL,
  text TEXT NOT NULL,
  symbol_json TEXT,
  indexed_at TEXT NOT NULL
);

CREATE VIRTUAL TABLE context_fts USING fts5(
  chunk_id UNINDEXED,
  path,
  symbols,
  text,
  tokenize='unicode61'
);
";

const FIXTURE_SESSION: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
const FIXTURE_PROJECT: &str = "018f3c8a-7e2b-7a10-8c4d-1123456789ab";
const FIXTURE_ACTOR: &str = "018f3c8a-7e2b-7a10-8c4d-2123456789ab";
const FIXTURE_TRACE: &str = "018f3c8a-7e2b-7a10-8c4d-3123456789ab";
const FIXTURE_EVENT_1: &str = "018f3c8a-7e2b-7a10-8c4d-4123456789ab";
const FIXTURE_EVENT_2: &str = "018f3c8a-7e2b-7a10-8c4d-5123456789ab";
const FIXTURE_RECORDED_AT: &str = "2026-01-15T12:00:00.000Z";
const PROJECTION_SCHEMA: i32 = 1;

const CURRENT_TABLES: &[&str] = &[
    "sessions",
    "events",
    "checkpoints",
    "artifacts",
    "capability_audit",
    "memories",
    "context_chunks",
    "context_fts",
    "agent_pool",
    "agent_mail",
    "session_execution_leases",
    "control_leases",
    "knowledge_items",
    "playbooks",
    "automation_state",
    "trajectories",
    "experiments",
    "session_insight_reports",
    "operation_journal",
    "egress_receipts",
    "artifact_refs",
    "approvals",
];

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn create(label: &str) -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rapidlm-migrations-compat-{}-{}-{seq}",
            std::process::id(),
            label
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn db_path(&self) -> PathBuf {
        self.path.join("ledger.sqlite")
    }

    fn artifacts_path(&self) -> PathBuf {
        self.path.join("artifacts")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(suffix);
    PathBuf::from(raw)
}

fn remove_db_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar(path, "-wal"));
    let _ = std::fs::remove_file(sidecar(path, "-shm"));
    let _ = std::fs::remove_file(sidecar(path, "-journal"));
}

fn open_raw(path: &Path) -> Connection {
    let conn = Connection::open(path).expect("open fixture sqlite");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("wal");
    conn.pragma_update(None, "foreign_keys", 1).expect("fk");
    conn
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .expect("query sqlite_master")
}

fn user_version(conn: &Connection) -> i32 {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("user_version")
}

fn assert_current_schema(path: &Path) {
    let conn = Connection::open(path).expect("reopen migrated db");
    assert_eq!(user_version(&conn), CURRENT_SCHEMA_VERSION);
    for name in CURRENT_TABLES {
        assert!(table_exists(&conn, name), "missing table {name}");
    }
}

fn write_v0_fixture(path: &Path) {
    remove_db_files(path);
    let conn = open_raw(path);
    conn.pragma_update(None, "user_version", 0).expect("v0");
    assert_eq!(user_version(&conn), 0);
    assert!(!table_exists(&conn, "sessions"));
    drop(conn);
}

fn write_v1_fixture(path: &Path) {
    remove_db_files(path);
    let conn = open_raw(path);
    conn.execute_batch(V1_CORE_SQL).expect("v1 ddl");
    conn.pragma_update(None, "user_version", 1).expect("v1");
    let actor_json = format!(r#"{{"kind":"system","id":"{FIXTURE_ACTOR}"}}"#);
    conn.execute(
        "INSERT INTO sessions (id, project_id, created_at, closed_at)
         VALUES (?1, ?2, ?3, NULL)",
        [FIXTURE_SESSION, FIXTURE_PROJECT, FIXTURE_RECORDED_AT],
    )
    .expect("v1 session");
    conn.execute(
        "INSERT INTO events (
            session_id, seq, event_id, recorded_at, actor_json,
            trace_id, kind, redaction, payload_json
         ) VALUES (?1, 1, ?2, ?3, ?4, ?5, 'session.created', 'project', ?6)",
        [
            FIXTURE_SESSION,
            FIXTURE_EVENT_1,
            FIXTURE_RECORDED_AT,
            actor_json.as_str(),
            FIXTURE_TRACE,
            r#"{"project_id":"018f3c8a-7e2b-7a10-8c4d-1123456789ab"}"#,
        ],
    )
    .expect("v1 event 1");
    conn.execute(
        "INSERT INTO events (
            session_id, seq, event_id, recorded_at, actor_json,
            trace_id, kind, redaction, payload_json
         ) VALUES (?1, 2, ?2, ?3, ?4, ?5, 'turn.started', 'project', ?6)",
        [
            FIXTURE_SESSION,
            FIXTURE_EVENT_2,
            FIXTURE_RECORDED_AT,
            actor_json.as_str(),
            FIXTURE_TRACE,
            r#"{"i":0}"#,
        ],
    )
    .expect("v1 event 2");
    assert_eq!(user_version(&conn), 1);
    assert!(table_exists(&conn, "sessions"));
    assert!(!table_exists(&conn, "agent_pool"));
    drop(conn);
}

fn live() -> CancellationToken {
    CancellationToken::new()
}

fn fixture_session() -> SessionId {
    SessionId::from_str(FIXTURE_SESSION).expect("session id")
}

fn replay_events(ledger: &EventLedger, session: SessionId) -> Vec<ErasedEventEnvelope> {
    let last = ledger.last_seq(session, &live()).expect("last_seq");
    (1..=last)
        .map(|seq| ledger.get(session, seq, &live()).expect("replay event"))
        .collect()
}

fn projection_document(events: &[ErasedEventEnvelope]) -> Value {
    json!({
        "last_seq": events.last().map(ErasedEventEnvelope::seq).unwrap_or(0),
        "event_ids": events.iter().map(|e| e.event_id().to_string()).collect::<Vec<_>>(),
        "kinds": events.iter().map(|e| e.kind().as_str()).collect::<Vec<_>>(),
        "payloads": events.iter().map(|e| e.payload().clone()).collect::<Vec<_>>(),
    })
}

fn replay_and_checkpoint(tmp: &TempDir, ledger: &EventLedger, session: SessionId) -> Value {
    let events = replay_events(ledger, session);
    for (idx, event) in events.iter().enumerate() {
        assert_eq!(event.seq(), (idx as u64) + 1, "replay seq must be gap-free");
        assert_eq!(event.session_id(), session);
    }
    let document = projection_document(&events);
    let bytes = serde_json::to_vec(&document).expect("encode projection");
    let artifacts = ArtifactStore::create(tmp.artifacts_path()).expect("artifact store");
    let checkpoints = CheckpointStore::new(ledger.clone(), artifacts);
    let written = checkpoints
        .write_checkpoint(
            session,
            events.last().map(ErasedEventEnvelope::seq).unwrap_or(0),
            PROJECTION_SCHEMA,
            &bytes,
            &event_ledger::checkpoint::CancellationToken::new(),
        )
        .expect("write checkpoint");
    let loaded = checkpoints
        .load_checkpoint(
            session,
            PROJECTION_SCHEMA,
            &event_ledger::checkpoint::CancellationToken::new(),
        )
        .expect("load checkpoint")
        .expect("checkpoint present");
    assert_eq!(loaded.through_seq(), written.through_seq());
    assert_eq!(loaded.projection_schema(), PROJECTION_SCHEMA);
    assert_eq!(loaded.projection(), bytes.as_slice());
    document
}

fn assert_emits_recovery(err: &MigrationError, expected: &str) {
    assert_eq!(err.recovery_instruction(), Some(expected));
    let display = err.to_string();
    assert!(
        display.contains("restore the pre-migration backup"),
        "irrecoverable error must emit backup/recovery instruction: {display}"
    );
    assert!(
        display.contains(expected),
        "display must include typed recovery instruction: {display}"
    );
}

#[test]
fn upgrade_v0_and_v1_fixtures_then_replay_projections() {
    let v0 = TempDir::create("v0");
    write_v0_fixture(&v0.db_path());
    let v0_applied = {
        let conn = Connection::open(v0.db_path()).expect("open v0");
        MigrationRunner::apply(&conn).expect("migrate v0")
    };
    assert_eq!(v0_applied.from, SchemaVersion(0));
    assert_eq!(v0_applied.to, SchemaVersion(CURRENT_SCHEMA_VERSION));
    let v0_ledger = EventLedger::open(v0.db_path()).expect("open migrated v0");
    assert_current_schema(&v0.db_path());

    let v0_session = SessionId::new();
    v0_ledger
        .create_session(v0_session, ProjectId::new(), &live())
        .expect("post-upgrade create session");
    v0_ledger
        .append(
            v0_session,
            ActorRef::new(ActorKind::System, FIXTURE_ACTOR).expect("actor"),
            EventKind::SessionCreated,
            json!({"origin":"v0-upgrade"}),
            &AppendOptions {
                redaction: RedactionClass::Project,
                trace_id: TraceId::from_str(FIXTURE_TRACE).expect("trace"),
                expected_seq: Some(0),
            },
            &live(),
        )
        .expect("post-upgrade append");
    let v0_projection = replay_and_checkpoint(&v0, &v0_ledger, v0_session);
    assert_eq!(v0_projection["last_seq"], 1);
    assert_eq!(v0_projection["kinds"], json!(["session.created"]));

    let v1 = TempDir::create("v1");
    write_v1_fixture(&v1.db_path());
    let v1_applied = {
        let conn = Connection::open(v1.db_path()).expect("open v1");
        MigrationRunner::apply(&conn).expect("migrate v1")
    };
    assert_eq!(v1_applied.from, SchemaVersion(1));
    assert_eq!(v1_applied.to, SchemaVersion(CURRENT_SCHEMA_VERSION));
    let v1_ledger = EventLedger::open(v1.db_path()).expect("open migrated v1");
    assert_current_schema(&v1.db_path());

    let v1_session = fixture_session();
    let v1_projection = replay_and_checkpoint(&v1, &v1_ledger, v1_session);
    assert_eq!(v1_projection["last_seq"], 2);
    assert_eq!(
        v1_projection["event_ids"],
        json!([FIXTURE_EVENT_1, FIXTURE_EVENT_2])
    );
    assert_eq!(
        v1_projection["kinds"],
        json!(["session.created", "turn.started"])
    );
    assert_eq!(v1_projection["payloads"][0]["project_id"], FIXTURE_PROJECT);

    assert_eq!(
        v0_projection["kinds"][0], v1_projection["kinds"][0],
        "upgraded fixtures must share the session.created invariant"
    );
}

#[test]
fn refuses_unknown_future_schema_and_emits_recovery_instruction() {
    let tmp = TempDir::create("future");
    let path = tmp.db_path();
    EventLedger::open(&path).expect("install current schema");
    {
        let conn = Connection::open(&path).expect("stamp future");
        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 3)
            .expect("future version");
    }

    let conn = Connection::open(&path).expect("reopen future");
    let err = MigrationRunner::apply(&conn).expect_err("future schema");
    match &err {
        MigrationError::UnknownFutureVersion { found, supported } => {
            assert_eq!(*found, CURRENT_SCHEMA_VERSION + 3);
            assert_eq!(*supported, CURRENT_SCHEMA_VERSION);
        }
        other => panic!("expected UnknownFutureVersion, got {other:?}"),
    }
    assert_emits_recovery(&err, UNKNOWN_FUTURE_RECOVERY);
    assert_eq!(user_version(&conn), CURRENT_SCHEMA_VERSION + 3);
    assert!(table_exists(&conn, "sessions"));

    match EventLedger::open(&path) {
        Err(LedgerError::Migration(MigrationError::UnknownFutureVersion { found, supported })) => {
            assert_eq!(found, CURRENT_SCHEMA_VERSION + 3);
            assert_eq!(supported, CURRENT_SCHEMA_VERSION);
        }
        other => panic!("EventLedger::open must refuse future schema: {other:?}"),
    }
}

#[test]
fn downgrade_is_restore_from_backup_not_reverse_migration() {
    let current = TempDir::create("current");
    let ledger = EventLedger::open(current.db_path()).expect("current schema");
    let session = SessionId::new();
    ledger
        .create_session(session, ProjectId::new(), &live())
        .expect("session");
    ledger
        .append(
            session,
            ActorRef::new(ActorKind::System, &EventId::new().to_string()).expect("actor"),
            EventKind::SessionCreated,
            json!({}),
            &AppendOptions {
                redaction: RedactionClass::Project,
                trace_id: TraceId::new(),
                expected_seq: Some(0),
            },
            &live(),
        )
        .expect("append");

    let applied = {
        let conn = Connection::open(current.db_path()).expect("reopen");
        MigrationRunner::apply(&conn).expect("idempotent apply")
    };
    assert_eq!(applied.from, SchemaVersion(CURRENT_SCHEMA_VERSION));
    assert_eq!(applied.to, SchemaVersion(CURRENT_SCHEMA_VERSION));
    assert_current_schema(&current.db_path());
    assert_eq!(ledger.last_seq(session, &live()).expect("events kept"), 1);

    let older_binary = TempDir::create("older-binary-sees-v2");
    write_v1_fixture(&older_binary.db_path());
    {
        let conn = Connection::open(older_binary.db_path()).expect("open v1");
        MigrationRunner::apply(&conn).expect("upgrade v1 to current");
        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)
            .expect("stay at current");
    }
    {
        let conn = Connection::open(older_binary.db_path()).expect("stamp as seen-by-older");
        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
            .expect("newer than this binary");
        let err = MigrationRunner::apply(&conn).expect_err("refuse downgrade");
        match err {
            MigrationError::UnknownFutureVersion { found, supported } => {
                assert_eq!(found, CURRENT_SCHEMA_VERSION + 1);
                assert_eq!(supported, CURRENT_SCHEMA_VERSION);
            }
            other => panic!("expected UnknownFutureVersion, got {other:?}"),
        }
        assert_emits_recovery(
            &MigrationRunner::apply(&conn).expect_err("still refused"),
            UNKNOWN_FUTURE_RECOVERY,
        );
        assert!(
            table_exists(&conn, "agent_pool"),
            "refusing downgrade must not reverse-migrate v2 tables away"
        );
        assert_eq!(user_version(&conn), CURRENT_SCHEMA_VERSION + 1);
    }
}

#[test]
fn migration_is_transactional() {
    let tmp = TempDir::create("tx");
    let path = tmp.db_path();
    write_v0_fixture(&path);
    {
        let conn = open_raw(&path);
        conn.execute("CREATE TABLE events (x INTEGER)", [])
            .expect("conflicting events table");
        let err = MigrationRunner::apply(&conn).expect_err("apply must fail");
        assert!(matches!(err, MigrationError::Sqlite(_)));
        assert_eq!(err.recovery_instruction(), None);
        assert_eq!(user_version(&conn), 0);
        assert!(!table_exists(&conn, "sessions"));
        assert!(!table_exists(&conn, "agent_pool"));
        assert!(table_exists(&conn, "events"));
    }
}

#[test]
fn irrecoverable_invalid_version_emits_recovery_instruction() {
    let tmp = TempDir::create("invalid");
    let path = tmp.db_path();
    write_v0_fixture(&path);
    let conn = open_raw(&path);
    conn.pragma_update(None, "user_version", -2)
        .expect("invalid version");
    let err = MigrationRunner::apply(&conn).expect_err("invalid version");
    assert!(matches!(err, MigrationError::InvalidVersion(-2)));
    assert_emits_recovery(&err, INVALID_VERSION_RECOVERY);
    assert_eq!(user_version(&conn), -2);
    assert!(!table_exists(&conn, "sessions"));
}
