//! Versioned, transactional SQLite migrations for the event ledger.
//!
//! `MigrationRunner::apply` is the startup entry point: it enables and asserts
//! WAL + foreign keys, records `PRAGMA user_version`, and refuses a database
//! whose version is newer than this binary.

use std::fmt;
use std::time::Duration;

use rusqlite::{Connection, Transaction, TransactionBehavior};

/// Schema version written after the latest bundled migration succeeds.
pub const CURRENT_SCHEMA_VERSION: i32 = 4;

/// Bounded SQLite lock wait. Matches the ledger busy-timeout recovery rule.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Operator instruction when a newer schema cannot be opened in-place.
pub const UNKNOWN_FUTURE_RECOVERY: &str = "in-place downgrade is not supported; restore the pre-migration backup or run a binary that supports this schema version";

/// Operator instruction when `PRAGMA user_version` is not a usable schema.
pub const INVALID_VERSION_RECOVERY: &str =
    "schema version is invalid; restore the pre-migration backup";

/// Integer schema version persisted in `PRAGMA user_version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaVersion(pub i32);

/// Outcome of [`MigrationRunner::apply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedMigration {
    pub from: SchemaVersion,
    pub to: SchemaVersion,
}

/// Forward-only SQLite schema migrations.
pub struct MigrationRunner;

#[derive(Debug)]
pub enum MigrationError {
    Sqlite(rusqlite::Error),
    JournalMode {
        expected: &'static str,
        actual: String,
    },
    ForeignKeysDisabled,
    UnknownFutureVersion {
        found: i32,
        supported: i32,
    },
    InvalidVersion(i32),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::JournalMode { expected, actual } => {
                write!(f, "sqlite journal_mode must be {expected}, found {actual}")
            }
            Self::ForeignKeysDisabled => {
                write!(f, "sqlite foreign_keys pragma is disabled")
            }
            Self::UnknownFutureVersion { found, supported } => {
                write!(
                    f,
                    "sqlite schema version {found} is newer than supported {supported}; {}",
                    UNKNOWN_FUTURE_RECOVERY
                )
            }
            Self::InvalidVersion(found) => {
                write!(
                    f,
                    "sqlite user_version {found} is invalid; {INVALID_VERSION_RECOVERY}"
                )
            }
        }
    }
}

impl std::error::Error for MigrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(err) => Some(err),
            Self::JournalMode { .. }
            | Self::ForeignKeysDisabled
            | Self::UnknownFutureVersion { .. }
            | Self::InvalidVersion(_) => None,
        }
    }
}

impl From<rusqlite::Error> for MigrationError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl MigrationError {
    /// Backup/restore instruction for failures that cannot be migrated in-place.
    ///
    /// Transactional apply failures are not irrecoverable: the IMMEDIATE
    /// transaction rolls back and `user_version` stays at the prior schema.
    pub fn recovery_instruction(&self) -> Option<&'static str> {
        match self {
            Self::UnknownFutureVersion { .. } => Some(UNKNOWN_FUTURE_RECOVERY),
            Self::InvalidVersion(_) => Some(INVALID_VERSION_RECOVERY),
            Self::Sqlite(_) | Self::JournalMode { .. } | Self::ForeignKeysDisabled => None,
        }
    }
}

struct Migration {
    version: i32,
    sql: &'static str,
}

/// v1: core ledger/session/context tables from `data-models/sqlite-schema.sql.md`.
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

/// v2: additive V2 projection/store tables from the same schema document.
const V2_EXTENSIONS_SQL: &str = "
CREATE TABLE agent_pool (
  session_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  parent_agent_id TEXT,
  class TEXT NOT NULL,
  role TEXT NOT NULL,
  state TEXT NOT NULL,
  workspace_view_id TEXT,
  budget_json TEXT NOT NULL,
  last_mail_cursor INTEGER NOT NULL DEFAULT 0,
  private_state_artifact_id TEXT,
  updated_at TEXT NOT NULL,
  PRIMARY KEY(session_id, agent_id)
);

CREATE TABLE agent_mail (
  session_id TEXT NOT NULL,
  cursor INTEGER NOT NULL,
  message_id TEXT NOT NULL UNIQUE,
  from_agent_id TEXT NOT NULL,
  to_agent_id TEXT NOT NULL,
  topic TEXT NOT NULL,
  body_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(session_id, cursor)
);

CREATE TABLE session_execution_leases (
  session_id TEXT PRIMARY KEY,
  generation INTEGER NOT NULL,
  owner_runtime_id TEXT NOT NULL,
  state TEXT NOT NULL,
  lease_digest TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE control_leases (
  session_id TEXT NOT NULL,
  domain TEXT NOT NULL,
  generation INTEGER NOT NULL,
  holder_json TEXT NOT NULL,
  acquired_at TEXT NOT NULL,
  expires_at TEXT,
  state TEXT NOT NULL,
  PRIMARY KEY(session_id, domain)
);

CREATE TABLE knowledge_items (
  id TEXT PRIMARY KEY,
  revision INTEGER NOT NULL,
  scope_json TEXT NOT NULL,
  trigger_json TEXT NOT NULL,
  content TEXT NOT NULL,
  provenance_json TEXT NOT NULL,
  owner_json TEXT NOT NULL,
  confidence REAL,
  freshness_json TEXT,
  status TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE playbooks (
  id TEXT NOT NULL,
  version INTEGER NOT NULL,
  spec_json TEXT NOT NULL,
  digest TEXT NOT NULL,
  status TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(id, version)
);

CREATE TABLE automation_state (
  automation_id TEXT PRIMARY KEY,
  playbook_id TEXT NOT NULL,
  playbook_version INTEGER NOT NULL,
  trigger_json TEXT NOT NULL,
  cursor_json TEXT,
  last_idempotency_key TEXT,
  state TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE trajectories (
  id TEXT PRIMARY KEY,
  source_json TEXT NOT NULL,
  manifest_artifact_id TEXT NOT NULL,
  events_artifact_id TEXT NOT NULL,
  outcome_json TEXT,
  reward_json TEXT,
  data_policy TEXT NOT NULL,
  digest TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE experiments (
  id TEXT PRIMARY KEY,
  spec_json TEXT NOT NULL,
  baseline_version TEXT NOT NULL,
  candidate_version TEXT NOT NULL,
  suite_version TEXT NOT NULL,
  state TEXT NOT NULL,
  result_artifact_id TEXT,
  created_at TEXT NOT NULL,
  completed_at TEXT
);

CREATE TABLE session_insight_reports (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  through_seq INTEGER NOT NULL,
  report_artifact_id TEXT NOT NULL,
  data_policy TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE INDEX idx_agent_mail_recipient ON agent_mail(session_id, to_agent_id, cursor);
CREATE INDEX idx_knowledge_status ON knowledge_items(status, updated_at);
CREATE INDEX idx_trajectory_policy ON trajectories(data_policy, created_at);
";

/// v3: operation journal, hash-linked egress receipts, GC roots, durable waits.
const V3_JOURNAL_SQL: &str = "
CREATE TABLE operation_journal (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  state TEXT NOT NULL,
  idempotency TEXT NOT NULL,
  reconcile_ref TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY(session_id) REFERENCES sessions(id)
);

CREATE TABLE egress_receipts (
  session_id TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  attempt INTEGER NOT NULL,
  prev_hash TEXT NOT NULL,
  receipt_hash TEXT NOT NULL,
  outcome TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(session_id, operation_id, attempt),
  FOREIGN KEY(session_id) REFERENCES sessions(id),
  FOREIGN KEY(operation_id) REFERENCES operation_journal(id)
);

CREATE TABLE artifact_refs (
  artifact_id TEXT NOT NULL,
  root_kind TEXT NOT NULL,
  root_key TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(artifact_id, root_kind, root_key)
);

CREATE TABLE approvals (
  session_id TEXT NOT NULL,
  wait_token TEXT NOT NULL,
  state TEXT NOT NULL,
  created_at TEXT NOT NULL,
  resolved_at TEXT,
  PRIMARY KEY(session_id, wait_token),
  FOREIGN KEY(session_id) REFERENCES sessions(id)
);

CREATE INDEX idx_operation_journal_session ON operation_journal(session_id, state);
CREATE INDEX idx_approvals_pending ON approvals(session_id, state);
";

/// v4: prompt cron jobs with claim-lease firing state and quarantine.
const V4_CRON_SQL: &str = "
CREATE TABLE cron_jobs (
  id TEXT PRIMARY KEY,
  prompt TEXT NOT NULL,
  session_id TEXT,
  schedule TEXT NOT NULL,
  status TEXT NOT NULL,
  next_fire_at_ms INTEGER NOT NULL,
  last_claim_ms INTEGER,
  quarantine_reason TEXT,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);

CREATE INDEX idx_cron_jobs_due ON cron_jobs(status, next_fire_at_ms);
";

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: V1_CORE_SQL,
    },
    Migration {
        version: 2,
        sql: V2_EXTENSIONS_SQL,
    },
    Migration {
        version: 3,
        sql: V3_JOURNAL_SQL,
    },
    Migration {
        version: 4,
        sql: V4_CRON_SQL,
    },
];

impl MigrationRunner {
    /// Apply pending migrations and return the resulting schema version.
    ///
    /// Always re-applies WAL, foreign keys, busy timeout, and synchronous
    /// settings. `foreign_keys` is per-connection and is not persisted.
    pub fn apply(conn: &Connection) -> Result<AppliedMigration, MigrationError> {
        configure_and_assert_pragmas(conn)?;
        let from = read_user_version(conn)?;
        if from < 0 {
            return Err(MigrationError::InvalidVersion(from));
        }
        if from > CURRENT_SCHEMA_VERSION {
            return Err(MigrationError::UnknownFutureVersion {
                found: from,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        if from < CURRENT_SCHEMA_VERSION {
            apply_pending(conn, from)?;
        }
        configure_and_assert_pragmas(conn)?;
        let to = read_user_version(conn)?;
        Ok(AppliedMigration {
            from: SchemaVersion(from),
            to: SchemaVersion(to),
        })
    }

    /// Read the persisted schema version without mutating the database.
    pub fn schema_version(conn: &Connection) -> Result<SchemaVersion, MigrationError> {
        Ok(SchemaVersion(read_user_version(conn)?))
    }
}

fn configure_and_assert_pragmas(conn: &Connection) -> Result<(), MigrationError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;

    let journal_mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(MigrationError::JournalMode {
            expected: "wal",
            actual: journal_mode,
        });
    }

    conn.pragma_update(None, "foreign_keys", 1)?;
    let foreign_keys: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(MigrationError::ForeignKeysDisabled);
    }

    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn read_user_version(conn: &Connection) -> Result<i32, MigrationError> {
    let version = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    Ok(version)
}

fn apply_pending(conn: &Connection, from: i32) -> Result<(), MigrationError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    for migration in MIGRATIONS {
        if migration.version <= from {
            continue;
        }
        tx.execute_batch(migration.sql)?;
        tx.pragma_update(None, "user_version", migration.version)?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDb {
        path: PathBuf,
        conn: Option<Connection>,
    }

    impl TempDb {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-event-ledger-mig-{}-{seq}.sqlite",
                std::process::id()
            ));
            remove_db_files(&path);
            let conn = Connection::open(&path).expect("open temp sqlite file");
            Self {
                path,
                conn: Some(conn),
            }
        }

        fn conn(&self) -> &Connection {
            self.conn.as_ref().expect("temp db connection is open")
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            drop(self.conn.take());
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

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .expect("query sqlite_master")
    }

    fn journal_mode(conn: &Connection) -> String {
        conn.pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("query journal_mode")
    }

    fn foreign_keys(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("query foreign_keys")
    }

    fn assert_core_and_v2_tables(conn: &Connection) {
        for name in [
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
        ] {
            assert!(table_exists(conn, name), "missing table {name}");
        }
    }

    #[test]
    fn fresh_db_applies_current_schema() {
        let db = TempDb::create();
        let applied = MigrationRunner::apply(db.conn()).expect("apply fresh db");
        assert_eq!(applied.from, SchemaVersion(0));
        assert_eq!(applied.to, SchemaVersion(CURRENT_SCHEMA_VERSION));
        assert_eq!(
            MigrationRunner::schema_version(db.conn()).expect("read version"),
            SchemaVersion(CURRENT_SCHEMA_VERSION)
        );
        assert_core_and_v2_tables(db.conn());
        assert_eq!(journal_mode(db.conn()).to_ascii_lowercase(), "wal");
        assert_eq!(foreign_keys(db.conn()), 1);
    }

    #[test]
    fn upgrade_from_v0_applies_pending_migrations() {
        let db = TempDb::create();
        db.conn()
            .pragma_update(None, "user_version", 0)
            .expect("set v0");
        assert_eq!(
            MigrationRunner::schema_version(db.conn()).expect("read v0"),
            SchemaVersion(0)
        );
        assert!(!table_exists(db.conn(), "sessions"));

        let applied = MigrationRunner::apply(db.conn()).expect("upgrade from v0");
        assert_eq!(applied.from, SchemaVersion(0));
        assert_eq!(applied.to, SchemaVersion(CURRENT_SCHEMA_VERSION));
        assert_core_and_v2_tables(db.conn());
    }

    #[test]
    fn upgrade_from_v1_applies_v2_only() {
        let db = TempDb::create();
        db.conn()
            .execute_batch(V1_CORE_SQL)
            .expect("install v1 fixture");
        db.conn()
            .pragma_update(None, "user_version", 1)
            .expect("record v1");

        let applied = MigrationRunner::apply(db.conn()).expect("upgrade from v1");
        assert_eq!(applied.from, SchemaVersion(1));
        assert_eq!(applied.to, SchemaVersion(CURRENT_SCHEMA_VERSION));
        assert!(table_exists(db.conn(), "sessions"));
        assert!(table_exists(db.conn(), "agent_pool"));
        assert!(table_exists(db.conn(), "session_execution_leases"));
        assert!(table_exists(db.conn(), "operation_journal"));
        assert!(table_exists(db.conn(), "egress_receipts"));
        assert!(table_exists(db.conn(), "artifact_refs"));
        assert!(table_exists(db.conn(), "approvals"));
    }

    #[test]
    fn upgrade_from_v2_applies_v3_and_v4() {
        let db = TempDb::create();
        db.conn()
            .execute_batch(V1_CORE_SQL)
            .expect("install v1 fixture");
        db.conn()
            .execute_batch(V2_EXTENSIONS_SQL)
            .expect("install v2 fixture");
        db.conn()
            .pragma_update(None, "user_version", 2)
            .expect("record v2");

        let applied = MigrationRunner::apply(db.conn()).expect("upgrade from v2");
        assert_eq!(applied.from, SchemaVersion(2));
        assert_eq!(applied.to, SchemaVersion(CURRENT_SCHEMA_VERSION));
        assert!(table_exists(db.conn(), "agent_pool"));
        assert!(table_exists(db.conn(), "operation_journal"));
        assert!(table_exists(db.conn(), "approvals"));
        assert!(table_exists(db.conn(), "cron_jobs"));
    }

    #[test]
    fn apply_is_idempotent() {
        let db = TempDb::create();
        let first = MigrationRunner::apply(db.conn()).expect("first apply");
        let second = MigrationRunner::apply(db.conn()).expect("second apply");
        assert_eq!(first.to, SchemaVersion(CURRENT_SCHEMA_VERSION));
        assert_eq!(second.from, SchemaVersion(CURRENT_SCHEMA_VERSION));
        assert_eq!(second.to, SchemaVersion(CURRENT_SCHEMA_VERSION));
    }

    #[test]
    fn refuses_unknown_future_schema_version() {
        let db = TempDb::create();
        MigrationRunner::apply(db.conn()).expect("apply current");
        db.conn()
            .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 7)
            .expect("stamp future version");

        let err = MigrationRunner::apply(db.conn()).expect_err("future version");
        match err {
            MigrationError::UnknownFutureVersion { found, supported } => {
                assert_eq!(found, CURRENT_SCHEMA_VERSION + 7);
                assert_eq!(supported, CURRENT_SCHEMA_VERSION);
            }
            other => panic!("expected UnknownFutureVersion, got {other:?}"),
        }
        assert_eq!(
            MigrationRunner::schema_version(db.conn()).expect("version unchanged"),
            SchemaVersion(CURRENT_SCHEMA_VERSION + 7)
        );
    }

    #[test]
    fn asserts_wal_and_foreign_keys_at_runtime() {
        let db = TempDb::create();
        MigrationRunner::apply(db.conn()).expect("apply");
        assert_eq!(journal_mode(db.conn()).to_ascii_lowercase(), "wal");
        assert_eq!(foreign_keys(db.conn()), 1);

        db.conn()
            .pragma_update(None, "foreign_keys", 0)
            .expect("disable fk");
        assert_eq!(foreign_keys(db.conn()), 0);
        MigrationRunner::apply(db.conn()).expect("re-apply asserts fk");
        assert_eq!(foreign_keys(db.conn()), 1);
        assert_eq!(journal_mode(db.conn()).to_ascii_lowercase(), "wal");
    }

    #[test]
    fn migration_transaction_rolls_back_on_failure() {
        let db = TempDb::create();
        db.conn()
            .execute("CREATE TABLE events (x INTEGER)", [])
            .expect("conflicting events table");

        let err = MigrationRunner::apply(db.conn()).expect_err("migration must fail");
        assert!(matches!(err, MigrationError::Sqlite(_)));
        assert_eq!(
            MigrationRunner::schema_version(db.conn()).expect("still v0"),
            SchemaVersion(0)
        );
        assert!(!table_exists(db.conn(), "sessions"));
        assert!(table_exists(db.conn(), "events"));
    }

    #[test]
    fn foreign_keys_reject_orphan_event() {
        let db = TempDb::create();
        MigrationRunner::apply(db.conn()).expect("apply");
        let err = db
            .conn()
            .execute(
                "INSERT INTO events (
                    session_id, seq, event_id, recorded_at, actor_json,
                    trace_id, kind, redaction, payload_json
                 ) VALUES (
                    'missing', 1, 'evt-1', '2026-08-14T00:00:00Z', '{}',
                    'trace', 'session.created', 'project', '{}'
                 )",
                [],
            )
            .expect_err("orphan event");
        assert_eq!(
            err.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
    }

    #[test]
    fn negative_user_version_is_rejected() {
        let db = TempDb::create();
        db.conn()
            .pragma_update(None, "user_version", -1)
            .expect("set negative version");
        let err = MigrationRunner::apply(db.conn()).expect_err("negative version");
        assert!(matches!(err, MigrationError::InvalidVersion(-1)));
    }

    #[test]
    fn in_memory_database_cannot_enable_wal() {
        let conn = Connection::open_in_memory().expect("open memory db");
        let err = MigrationRunner::apply(&conn).expect_err("memory is not WAL");
        match err {
            MigrationError::JournalMode { expected, actual } => {
                assert_eq!(expected, "wal");
                assert_eq!(actual.to_ascii_lowercase(), "memory");
            }
            other => panic!("expected JournalMode, got {other:?}"),
        }
    }
}
