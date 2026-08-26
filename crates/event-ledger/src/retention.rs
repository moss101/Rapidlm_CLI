//! Artifact GC roots: pin reachable blobs, collect unreferenced ones.
//!
//! Checkpoints, explicit pins, and journal wait/operation refs are roots.
//! Unreferenced published blobs (and their sidecars) are deleted. Missing
//! roots fail closed: GC never deletes a blob that still has a catalog row
//! pointing at it from `checkpoints`.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use protocol::ArtifactId;
use rusqlite::{Connection, TransactionBehavior, params};

use crate::artifact_store::{ArtifactError, ArtifactStore};
use crate::ledger::EventLedger;

const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Cooperative cancellation for pin/GC.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Why an artifact is retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RootKind {
    Checkpoint,
    Pin,
    Operation,
    Wait,
}

/// File-backed retention over ledger catalog + CAS.
#[derive(Clone, Debug)]
pub struct RetentionService {
    ledger: EventLedger,
    artifacts: ArtifactStore,
}

/// Typed retention/GC failures.
#[derive(Debug)]
pub enum RetentionError {
    Cancelled,
    InvalidRoot,
    Artifact(ArtifactError),
    Sqlite(rusqlite::Error),
    Corrupt(&'static str),
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

    pub fn check(&self) -> Result<(), RetentionError> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(RetentionError::Cancelled)
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

impl RootKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Checkpoint => "checkpoint",
            Self::Pin => "pin",
            Self::Operation => "operation",
            Self::Wait => "wait",
        }
    }
}

impl RetentionService {
    pub fn new(ledger: EventLedger, artifacts: ArtifactStore) -> Self {
        Self { ledger, artifacts }
    }

    pub fn artifacts(&self) -> &ArtifactStore {
        &self.artifacts
    }

    pub fn ledger(&self) -> &EventLedger {
        &self.ledger
    }

    /// Record a GC root. Idempotent for the same (artifact, kind, key).
    pub fn pin(
        &self,
        artifact_id: ArtifactId,
        kind: RootKind,
        root_key: &str,
        cancel: &CancellationToken,
    ) -> Result<(), RetentionError> {
        cancel.check()?;
        validate_root_key(root_key)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        cancel.check()?;
        let created_at: String =
            tx.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
                row.get(0)
            })?;
        tx.execute(
            "INSERT OR IGNORE INTO artifact_refs (artifact_id, root_kind, root_key, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![artifact_id.to_string(), kind.as_str(), root_key, created_at,],
        )?;
        cancel.check()?;
        tx.commit()?;
        Ok(())
    }

    pub fn unpin(
        &self,
        artifact_id: ArtifactId,
        kind: RootKind,
        root_key: &str,
        cancel: &CancellationToken,
    ) -> Result<(), RetentionError> {
        cancel.check()?;
        let conn = self.connect()?;
        conn.execute(
            "DELETE FROM artifact_refs
             WHERE artifact_id = ?1 AND root_kind = ?2 AND root_key = ?3",
            params![artifact_id.to_string(), kind.as_str(), root_key],
        )?;
        Ok(())
    }

    /// Delete published blobs that have no checkpoint or artifact_refs root.
    pub fn collect(&self, cancel: &CancellationToken) -> Result<u64, RetentionError> {
        cancel.check()?;
        let roots = self.rooted_ids(cancel)?;
        let artifact_cancel = crate::artifact_store::CancellationToken::new();
        let published = self.artifacts.list_published(&artifact_cancel)?;
        let mut removed = 0u64;
        for id in published {
            cancel.check()?;
            if roots.contains(&id) {
                continue;
            }
            self.artifacts.unpublish(&id)?;
            removed += 1;
        }
        Ok(removed)
    }

    fn rooted_ids(
        &self,
        cancel: &CancellationToken,
    ) -> Result<BTreeSet<ArtifactId>, RetentionError> {
        cancel.check()?;
        let conn = self.connect()?;
        let mut ids = BTreeSet::new();
        let mut stmt = conn.prepare("SELECT artifact_id FROM artifact_refs")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            cancel.check()?;
            let wire = row?;
            let id = ArtifactId::from_str(&wire)
                .map_err(|_| RetentionError::Corrupt("invalid artifact_id in refs"))?;
            ids.insert(id);
        }
        let mut stmt = conn.prepare("SELECT artifact_id FROM checkpoints")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            cancel.check()?;
            let wire = row?;
            let id = ArtifactId::from_str(&wire)
                .map_err(|_| RetentionError::Corrupt("invalid checkpoint artifact_id"))?;
            ids.insert(id);
        }
        Ok(ids)
    }

    fn connect(&self) -> Result<Connection, RetentionError> {
        let conn = Connection::open(self.ledger.path())?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", 1)?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(conn)
    }
}

fn validate_root_key(key: &str) -> Result<(), RetentionError> {
    if key.is_empty() || key.len() > 256 {
        return Err(RetentionError::InvalidRoot);
    }
    Ok(())
}

impl fmt::Display for RetentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("retention operation cancelled"),
            Self::InvalidRoot => f.write_str("invalid GC root key"),
            Self::Artifact(err) => write!(f, "{err}"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::Corrupt(reason) => write!(f, "retention catalog corrupt ({reason})"),
        }
    }
}

impl std::error::Error for RetentionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Artifact(err) => Some(err),
            Self::Sqlite(err) => Some(err),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for RetentionError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<ArtifactError> for RetentionError {
    fn from(value: ArtifactError) -> Self {
        Self::Artifact(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_store::{ArtifactMetadata, CancellationToken as ArtifactCancel};
    use protocol::{ProjectId, RedactionClass, SessionId};
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempRetention {
        db: PathBuf,
        blobs: PathBuf,
        service: RetentionService,
    }

    impl TempRetention {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let db = std::env::temp_dir().join(format!("rapidlm-retention-{pid}-{seq}.sqlite"));
            let blobs = std::env::temp_dir().join(format!("rapidlm-retention-blobs-{pid}-{seq}"));
            let _ = std::fs::remove_file(&db);
            let _ = std::fs::remove_dir_all(&blobs);
            let ledger = EventLedger::open(&db).expect("ledger");
            ledger
                .create_session(
                    SessionId::new(),
                    ProjectId::new(),
                    &crate::ledger::CancellationToken::new(),
                )
                .expect("session");
            let artifacts = ArtifactStore::create(&blobs).expect("store");
            Self {
                db,
                blobs,
                service: RetentionService::new(ledger, artifacts),
            }
        }
    }

    impl Drop for TempRetention {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.db);
            let mut wal = self.db.as_os_str().to_os_string();
            wal.push("-wal");
            let _ = std::fs::remove_file(&wal);
            let mut shm = self.db.as_os_str().to_os_string();
            shm.push("-shm");
            let _ = std::fs::remove_file(&shm);
            let _ = std::fs::remove_dir_all(&self.blobs);
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn put(service: &RetentionService, bytes: &[u8]) -> ArtifactId {
        service
            .artifacts
            .put(
                Cursor::new(bytes),
                ArtifactMetadata::new("text/plain", RedactionClass::Public),
                &ArtifactCancel::new(),
            )
            .expect("put")
            .id
    }

    #[test]
    fn gc_keeps_pinned_and_deletes_unreferenced() {
        let tmp = TempRetention::create();
        let keep = put(&tmp.service, b"keep-me");
        let drop = put(&tmp.service, b"drop-me");
        tmp.service
            .pin(keep, RootKind::Pin, "user-pin", &live())
            .expect("pin");
        assert!(tmp.service.artifacts.exists(&keep));
        assert!(tmp.service.artifacts.exists(&drop));

        let removed = tmp.service.collect(&live()).expect("gc");
        assert_eq!(removed, 1);
        assert!(tmp.service.artifacts.exists(&keep));
        assert!(!tmp.service.artifacts.exists(&drop));
    }

    #[test]
    fn checkpoint_artifact_is_a_gc_root() {
        use crate::checkpoint::{CancellationToken as CheckpointCancel, CheckpointStore};
        use crate::event::{ActorKind, ActorRef, EventKind};
        use crate::ledger::{AppendOptions, CancellationToken as LedgerCancel};
        use protocol::{EventId, TraceId};

        let tmp = TempRetention::create();
        let session = SessionId::new();
        tmp.service
            .ledger()
            .create_session(session, ProjectId::new(), &LedgerCancel::new())
            .expect("session");
        tmp.service
            .ledger()
            .append(
                session,
                ActorRef::new(ActorKind::System, &EventId::new().to_string()).expect("actor"),
                EventKind::SessionCreated,
                serde_json::json!({}),
                &AppendOptions {
                    redaction: RedactionClass::Project,
                    trace_id: TraceId::new(),
                    expected_seq: Some(0),
                },
                &LedgerCancel::new(),
            )
            .expect("append");
        let store = CheckpointStore::new(
            tmp.service.ledger().clone(),
            tmp.service.artifacts().clone(),
        );
        let written = store
            .write_checkpoint(session, 1, 1, b"{\"seq\":1}", &CheckpointCancel::new())
            .expect("checkpoint");
        let orphan = put(&tmp.service, b"orphan");
        let removed = tmp.service.collect(&live()).expect("gc");
        assert_eq!(removed, 1);
        assert!(tmp.service.artifacts.exists(&written.artifact_id()));
        assert!(!tmp.service.artifacts.exists(&orphan));
    }

    #[test]
    fn unpin_then_gc_removes_blob() {
        let tmp = TempRetention::create();
        let id = put(&tmp.service, b"temporary");
        tmp.service
            .pin(id, RootKind::Pin, "tmp", &live())
            .expect("pin");
        assert_eq!(tmp.service.collect(&live()).expect("noop"), 0);
        tmp.service
            .unpin(id, RootKind::Pin, "tmp", &live())
            .expect("unpin");
        assert_eq!(tmp.service.collect(&live()).expect("collect"), 1);
        assert!(!tmp.service.artifacts.exists(&id));
    }
}
