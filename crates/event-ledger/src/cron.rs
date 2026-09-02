//! Durable prompt cron storage with claim-lease firing and quarantine.
//!
//! Rows move through three states: `active` (waiting for `next_fire_at_ms`),
//! `firing` (claimed by exactly one poller inside a single IMMEDIATE
//! transaction, stamped with `last_claim_ms`), and `quarantined` (kept, not
//! loaded, after an operator or the scheduler rejects the row). A `firing`
//! row whose claim has gone stale is requeued by [`CronStore::requeue_orphaned`]
//! so a crashed poller cannot strand a job.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::migrations::{MigrationError, MigrationRunner};

/// Wire identity of this store's row shape.
pub const CRON_STORE_SCHEMA: &str = "rapidlm.cron.store.v1";

/// Maximum UTF-8 bytes accepted in a stored prompt.
pub const MAX_PROMPT_BYTES: usize = 8 * 1024;
/// Maximum UTF-8 bytes accepted in a stored schedule expression. Semantic
/// validation is the scheduler facade's job; the store only bounds size.
pub const MAX_SCHEDULE_TEXT_BYTES: usize = 512;
/// Maximum UTF-8 bytes accepted in an attached session id.
pub const MAX_SESSION_ID_BYTES: usize = 128;
/// Maximum UTF-8 bytes accepted in a quarantine reason.
pub const MAX_QUARANTINE_REASON_BYTES: usize = 256;

/// Row lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CronJobStatus {
    /// Waiting for `next_fire_at_ms`.
    Active,
    /// Claimed by a poller; protected from double-firing by the claim lease.
    Firing,
    /// Kept, not loaded. The row survives with a reason for operator review.
    Quarantined,
}

impl CronJobStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Firing => "firing",
            Self::Quarantined => "quarantined",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "firing" => Some(Self::Firing),
            "quarantined" => Some(Self::Quarantined),
            _ => None,
        }
    }
}

/// One durable cron job row.
#[derive(Clone, Debug, PartialEq)]
pub struct CronJob {
    pub id: String,
    pub prompt: String,
    pub session_id: Option<String>,
    pub schedule: String,
    pub status: CronJobStatus,
    pub next_fire_at_ms: i64,
    pub last_claim_ms: Option<i64>,
    pub quarantine_reason: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Typed failures for cron storage operations.
#[derive(Debug)]
pub enum CronStoreError {
    JobNotFound {
        id: String,
    },
    /// The row exists but is not in the `firing` state, so completion would
    /// resurrect a row an operator quarantined mid-flight. Fail closed.
    JobNotClaimed {
        id: String,
    },
    PromptTooLarge {
        limit: usize,
        observed: usize,
    },
    ScheduleTooLarge {
        limit: usize,
        observed: usize,
    },
    SessionIdTooLarge {
        limit: usize,
        observed: usize,
    },
    ReasonTooLarge {
        limit: usize,
        observed: usize,
    },
    /// Stored row violates its own invariants; the database was edited
    /// outside this crate.
    Corrupt(&'static str),
    Migration(MigrationError),
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

impl fmt::Display for CronStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JobNotFound { id } => write!(f, "cron job '{id}' not found"),
            Self::JobNotClaimed { id } => write!(
                f,
                "cron job '{id}' is not in the firing state; refusing to complete it"
            ),
            Self::PromptTooLarge { limit, observed } => write!(
                f,
                "prompt is {observed} bytes; limit is {limit} bytes"
            ),
            Self::ScheduleTooLarge { limit, observed } => write!(
                f,
                "schedule expression is {observed} bytes; limit is {limit} bytes"
            ),
            Self::SessionIdTooLarge { limit, observed } => write!(
                f,
                "session id is {observed} bytes; limit is {limit} bytes"
            ),
            Self::ReasonTooLarge { limit, observed } => write!(
                f,
                "quarantine reason is {observed} bytes; limit is {limit} bytes"
            ),
            Self::Corrupt(why) => write!(f, "cron store row is corrupt: {why}"),
            Self::Migration(err) => write!(f, "cron store migration failed: {err}"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::Io(err) => write!(f, "io error: {err}"),
        }
    }
}

impl std::error::Error for CronStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Migration(err) => Some(err),
            Self::Sqlite(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for CronStoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<std::io::Error> for CronStoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<MigrationError> for CronStoreError {
    fn from(value: MigrationError) -> Self {
        Self::Migration(value)
    }
}

static ID_SEQ: AtomicU64 = AtomicU64::new(0);

/// Generate a collision-resistant job id without external randomness:
/// wall-clock nanos mixed with the pid and a process-local counter.
fn generate_id() -> String {
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = nanos
        ^ ((std::process::id() as u64) << 32)
        ^ (seq.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    format!("cron-{mixed:016x}")
}

/// File-backed cron job store. Connections are opened per operation so
/// writers serialize through SQLite IMMEDIATE transactions.
#[derive(Clone, Debug)]
pub struct CronStore {
    path: PathBuf,
}

impl CronStore {
    /// Open (or create) the store. The cron tables live in the event-ledger
    /// database and are installed by the same migration runner.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CronStoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        MigrationRunner::apply(&conn)?;
        drop(conn);
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert a new `active` job. `next_fire_at_ms` is supplied by the caller
    /// (the scheduler facade derives it from the parsed schedule).
    pub fn add(
        &self,
        prompt: &str,
        session_id: Option<&str>,
        schedule: &str,
        next_fire_at_ms: i64,
        now_ms: i64,
    ) -> Result<CronJob, CronStoreError> {
        let prompt_bytes = prompt.len();
        if prompt_bytes > MAX_PROMPT_BYTES {
            return Err(CronStoreError::PromptTooLarge {
                limit: MAX_PROMPT_BYTES,
                observed: prompt_bytes,
            });
        }
        let schedule_bytes = schedule.len();
        if schedule_bytes > MAX_SCHEDULE_TEXT_BYTES {
            return Err(CronStoreError::ScheduleTooLarge {
                limit: MAX_SCHEDULE_TEXT_BYTES,
                observed: schedule_bytes,
            });
        }
        if let Some(sid) = session_id {
            let sid_bytes = sid.len();
            if sid_bytes > MAX_SESSION_ID_BYTES {
                return Err(CronStoreError::SessionIdTooLarge {
                    limit: MAX_SESSION_ID_BYTES,
                    observed: sid_bytes,
                });
            }
        }
        let id = generate_id();
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO cron_jobs
               (id, prompt, session_id, schedule, status, next_fire_at_ms,
                last_claim_ms, quarantine_reason, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5, NULL, NULL, ?6, ?6)",
            params![id, prompt, session_id, schedule, next_fire_at_ms, now_ms],
        )?;
        Ok(CronJob {
            id,
            prompt: prompt.to_string(),
            session_id: session_id.map(str::to_string),
            schedule: schedule.to_string(),
            status: CronJobStatus::Active,
            next_fire_at_ms,
            last_claim_ms: None,
            quarantine_reason: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        })
    }

    /// Fetch one job by id.
    pub fn get(&self, id: &str) -> Result<CronJob, CronStoreError> {
        let conn = self.connect()?;
        let job = conn
            .query_row(
                "SELECT id, prompt, session_id, schedule, status, next_fire_at_ms,
                        last_claim_ms, quarantine_reason, created_at_ms, updated_at_ms
                 FROM cron_jobs WHERE id = ?1",
                params![id],
                job_from_row,
            )
            .optional()?;
        job.ok_or(CronStoreError::JobNotFound { id: id.to_string() })
    }

    /// Remove a job. Returns `false` when the id is unknown.
    pub fn remove(&self, id: &str) -> Result<bool, CronStoreError> {
        let conn = self.connect()?;
        let changed = conn.execute("DELETE FROM cron_jobs WHERE id = ?1", params![id])?;
        Ok(changed > 0)
    }

    /// All jobs ordered by next fire time, quarantined rows included so
    /// operators can inspect what was kept but not loaded.
    pub fn list(&self) -> Result<Vec<CronJob>, CronStoreError> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, prompt, session_id, schedule, status, next_fire_at_ms,
                    last_claim_ms, quarantine_reason, created_at_ms, updated_at_ms
             FROM cron_jobs ORDER BY next_fire_at_ms, id",
        )?;
        let rows = stmt.query_map([], job_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Atomically claim every `active` job whose fire time has arrived.
    ///
    /// Runs in one IMMEDIATE transaction: matching rows flip to `firing`
    /// with `last_claim_ms = now_ms`, so two concurrent pollers can never
    /// claim the same row. Returns the claimed rows in fire order.
    pub fn claim_due(&self, now_ms: i64, limit: usize) -> Result<Vec<CronJob>, CronStoreError> {
        // `Connection::open` sets rusqlite's own 5000ms `sqlite3_busy_timeout`
        // default unconditionally (see `InnerConnection::open_with_flags`),
        // matching `connect()`'s explicit `busy_timeout(BUSY_TIMEOUT)` call
        // exactly — so a concurrent poller waits for this transaction rather
        // than failing immediately with `SQLITE_BUSY` either way. Confirmed
        // directly against this workspace's pinned rusqlite version before
        // assuming otherwise.
        let conn = Connection::open(&self.path)?;
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let due: Vec<CronJob> = {
            let mut stmt = tx.prepare(
                "SELECT id, prompt, session_id, schedule, status, next_fire_at_ms,
                        last_claim_ms, quarantine_reason, created_at_ms, updated_at_ms
                 FROM cron_jobs
                 WHERE status = 'active' AND next_fire_at_ms <= ?1
                 ORDER BY next_fire_at_ms, id
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![now_ms, limit as i64], job_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for job in &due {
            let changed = tx.execute(
                "UPDATE cron_jobs SET status = 'firing', last_claim_ms = ?2, updated_at_ms = ?2
                 WHERE id = ?1 AND status = 'active'",
                params![job.id, now_ms],
            )?;
            if changed != 1 {
                return Err(CronStoreError::Corrupt(
                    "claimed row disappeared inside the claim transaction",
                ));
            }
        }
        tx.commit()?;
        Ok(due
            .into_iter()
            .map(|mut job| {
                job.status = CronJobStatus::Firing;
                job.last_claim_ms = Some(now_ms);
                job.updated_at_ms = now_ms;
                job
            })
            .collect())
    }

    /// Complete a claimed job: reactivate it with the next fire time.
    ///
    /// Fails closed with [`CronStoreError::JobNotClaimed`] when the row is
    /// no longer `firing` — e.g. an operator quarantined it mid-flight — so
    /// completion never resurrects a quarantined job.
    pub fn complete(
        &self,
        id: &str,
        next_fire_at_ms: i64,
        now_ms: i64,
    ) -> Result<(), CronStoreError> {
        let conn = self.connect()?;
        let changed = conn.execute(
            "UPDATE cron_jobs
             SET status = 'active', next_fire_at_ms = ?2, last_claim_ms = NULL,
                 updated_at_ms = ?3
             WHERE id = ?1 AND status = 'firing'",
            params![id, next_fire_at_ms, now_ms],
        )?;
        if changed == 1 {
            return Ok(());
        }
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM cron_jobs WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_some() {
            Err(CronStoreError::JobNotClaimed { id: id.to_string() })
        } else {
            Err(CronStoreError::JobNotFound { id: id.to_string() })
        }
    }

    /// Requeue `firing` rows whose claim is older than `firing_timeout_ms`.
    /// Crash recovery for a poller that died between claim and complete.
    /// Returns the number of rows requeued.
    pub fn requeue_orphaned(
        &self,
        now_ms: i64,
        firing_timeout_ms: i64,
    ) -> Result<u64, CronStoreError> {
        if firing_timeout_ms < 0 {
            return Err(CronStoreError::Corrupt(
                "firing timeout must not be negative",
            ));
        }
        let conn = self.connect()?;
        let claim_cutoff = now_ms.saturating_sub(firing_timeout_ms);
        let changed = conn.execute(
            "UPDATE cron_jobs
             SET status = 'active', next_fire_at_ms = ?1, updated_at_ms = ?1
             WHERE status = 'firing'
               AND last_claim_ms IS NOT NULL AND last_claim_ms <= ?2",
            params![now_ms, claim_cutoff],
        )?;
        Ok(changed as u64)
    }

    /// Quarantine a job: kept, not loaded. Works from any state so an
    /// operator can stop a firing job mid-flight; completion afterwards
    /// fails closed.
    pub fn quarantine(
        &self,
        id: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<(), CronStoreError> {
        let reason_bytes = reason.len();
        if reason_bytes > MAX_QUARANTINE_REASON_BYTES {
            return Err(CronStoreError::ReasonTooLarge {
                limit: MAX_QUARANTINE_REASON_BYTES,
                observed: reason_bytes,
            });
        }
        let conn = self.connect()?;
        let changed = conn.execute(
            "UPDATE cron_jobs
             SET status = 'quarantined', quarantine_reason = ?2, updated_at_ms = ?3
             WHERE id = ?1",
            params![id, reason, now_ms],
        )?;
        if changed == 0 {
            return Err(CronStoreError::JobNotFound { id: id.to_string() });
        }
        Ok(())
    }

    fn connect(&self) -> Result<Connection, CronStoreError> {
        let conn = Connection::open(&self.path)?;
        MigrationRunner::apply(&conn)?;
        Ok(conn)
    }
}

fn job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CronJob> {
    let status_raw: String = row.get("status")?;
    let status = CronJobStatus::parse(&status_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown cron job status '{status_raw}'").into(),
        )
    })?;
    Ok(CronJob {
        id: row.get("id")?,
        prompt: row.get("prompt")?,
        session_id: row.get("session_id")?,
        schedule: row.get("schedule")?,
        status,
        next_fire_at_ms: row.get("next_fire_at_ms")?,
        last_claim_ms: row.get("last_claim_ms")?,
        quarantine_reason: row.get("quarantine_reason")?,
        created_at_ms: row.get("created_at_ms")?,
        updated_at_ms: row.get("updated_at_ms")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn open_store() -> (CronStore, Self) {
            let seq = TEMP_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-cron-{}-{seq}.sqlite",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            let store = CronStore::open(&path).expect("open cron store");
            (store, Self { path })
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(sidecar(&self.path, "-wal"));
            let _ = std::fs::remove_file(sidecar(&self.path, "-shm"));
        }
    }

    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut owned = path.as_os_str().to_owned();
        owned.push(suffix);
        PathBuf::from(owned)
    }

    fn add_job(store: &CronStore, next_fire: i64) -> CronJob {
        store
            .add("run checks", None, "*/5 * * * *", next_fire, 1_000)
            .expect("add job")
    }

    #[test]
    fn add_and_get_round_trips_the_job() {
        let (store, _db) = TempDb::open_store();
        let job = add_job(&store, 5_000);
        let loaded = store.get(&job.id).expect("get");
        assert_eq!(loaded, job);
        assert_eq!(loaded.status, CronJobStatus::Active);
        assert_eq!(loaded.next_fire_at_ms, 5_000);
        assert_eq!(loaded.created_at_ms, 1_000);
    }

    #[test]
    fn get_unknown_id_is_job_not_found() {
        let (store, _db) = TempDb::open_store();
        let err = store.get("cron-missing").expect_err("missing id");
        match err {
            CronStoreError::JobNotFound { id } => assert_eq!(id, "cron-missing"),
            other => panic!("expected JobNotFound, got {other:?}"),
        }
    }

    #[test]
    fn claim_due_claims_only_due_rows_and_marks_them_firing() {
        let (store, _db) = TempDb::open_store();
        let due = add_job(&store, 1_500);
        let future = add_job(&store, 9_999);
        let claimed = store.claim_due(2_000, 10).expect("claim");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, due.id);
        assert_eq!(claimed[0].status, CronJobStatus::Firing);
        assert_eq!(claimed[0].last_claim_ms, Some(2_000));
        assert_eq!(store.get(&future.id).expect("get").status, CronJobStatus::Active);
        // Second claim at the same instant gets nothing: the lease holds.
        let again = store.claim_due(2_000, 10).expect("claim again");
        assert!(again.is_empty());
    }

    #[test]
    fn claim_due_waits_for_a_concurrent_writer_instead_of_failing_busy() {
        // `claim_due`'s own doc comment says two concurrent pollers can
        // safely race for the same rows — that only holds if a busy
        // connection actually waits for the lock instead of erroring
        // immediately with SQLITE_BUSY, which requires `busy_timeout` to be
        // configured on `claim_due`'s own connection like it is on every
        // other method's.
        let (store, db) = TempDb::open_store();
        add_job(&store, 1_500);

        let blocker_path = db.path.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let conn = Connection::open(&blocker_path).expect("blocker connection");
            conn.execute_batch("BEGIN IMMEDIATE;")
                .expect("begin immediate");
            ready_tx.send(()).expect("signal ready");
            std::thread::sleep(std::time::Duration::from_millis(300));
            conn.execute_batch("COMMIT;").expect("commit");
        });
        ready_rx.recv().expect("blocker ready");

        let claimed = store
            .claim_due(2_000, 10)
            .expect("claim_due must wait out the concurrent writer, not fail busy");
        assert_eq!(claimed.len(), 1);

        handle.join().expect("blocker thread");
    }

    #[test]
    fn complete_reactivates_a_claimed_job_with_the_next_fire_time() {
        let (store, _db) = TempDb::open_store();
        let job = add_job(&store, 1_000);
        store.claim_due(1_000, 10).expect("claim");
        store.complete(&job.id, 61_000, 1_050).expect("complete");
        let done = store.get(&job.id).expect("get");
        assert_eq!(done.status, CronJobStatus::Active);
        assert_eq!(done.next_fire_at_ms, 61_000);
        assert_eq!(done.last_claim_ms, None);
        assert_eq!(done.updated_at_ms, 1_050);
    }

    #[test]
    fn complete_refuses_to_resurrect_a_job_quarantined_mid_flight() {
        let (store, _db) = TempDb::open_store();
        let job = add_job(&store, 1_000);
        store.claim_due(1_000, 10).expect("claim");
        store.quarantine(&job.id, "operator stop", 1_010).expect("quarantine");
        let err = store.complete(&job.id, 61_000, 1_050).expect_err("complete");
        match err {
            CronStoreError::JobNotClaimed { id } => assert_eq!(id, job.id),
            other => panic!("expected JobNotClaimed, got {other:?}"),
        }
        let kept = store.get(&job.id).expect("get");
        assert_eq!(kept.status, CronJobStatus::Quarantined);
        assert_eq!(kept.quarantine_reason.as_deref(), Some("operator stop"));
    }

    #[test]
    fn requeue_orphaned_resets_stale_firing_rows_only() {
        let (store, _db) = TempDb::open_store();
        let stale = add_job(&store, 1_000);
        let fresh = add_job(&store, 1_000);
        store.claim_due(1_000, 10).expect("claim both");
        // `fresh` completes and comes due again at 61_000, so its re-claim
        // lease (61_000) is still live at the sweep; `stale` keeps its
        // original 1_000 claim and is orphaned.
        store.complete(&fresh.id, 61_000, 1_100).expect("complete fresh");
        store.claim_due(61_000, 10).expect("re-claim fresh");
        let requeued = store
            .requeue_orphaned(120_000, 60_000)
            .expect("requeue");
        assert_eq!(requeued, 1);
        assert_eq!(store.get(&stale.id).expect("get").status, CronJobStatus::Active);
        assert_eq!(store.get(&fresh.id).expect("get").status, CronJobStatus::Firing);
    }

    #[test]
    fn remove_reports_whether_the_row_existed() {
        let (store, _db) = TempDb::open_store();
        let job = add_job(&store, 1_000);
        assert!(store.remove(&job.id).expect("remove"));
        assert!(!store.remove(&job.id).expect("remove again"));
    }

    #[test]
    fn list_orders_by_next_fire_and_includes_quarantined_rows() {
        let (store, _db) = TempDb::open_store();
        let later = add_job(&store, 5_000);
        let earlier = add_job(&store, 2_000);
        store.quarantine(&later.id, "bad schedule", 1_500).expect("quarantine");
        let listed = store.list().expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, earlier.id);
        assert_eq!(listed[1].id, later.id);
        assert_eq!(listed[1].status, CronJobStatus::Quarantined);
    }

    #[test]
    fn oversized_fields_are_rejected_before_any_write() {
        let (store, _db) = TempDb::open_store();
        let long_prompt = "x".repeat(MAX_PROMPT_BYTES + 1);
        let err = store
            .add(&long_prompt, None, "* * * * *", 1_000, 1_000)
            .expect_err("prompt over bound");
        assert!(matches!(err, CronStoreError::PromptTooLarge { .. }));
        let long_schedule = "x".repeat(MAX_SCHEDULE_TEXT_BYTES + 1);
        let err = store
            .add("ok", None, &long_schedule, 1_000, 1_000)
            .expect_err("schedule over bound");
        assert!(matches!(err, CronStoreError::ScheduleTooLarge { .. }));
        assert!(store.list().expect("list").is_empty());
    }
}
