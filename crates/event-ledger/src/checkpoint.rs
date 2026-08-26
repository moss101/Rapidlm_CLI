//! Hashed compressed projection checkpoints keyed by through-seq/schema.
//!
//! `write_checkpoint` publishes the compressed artifact first, then inserts
//! the catalog and checkpoint rows in one SQLite transaction. `load_checkpoint`
//! walks newest-first, verifies the artifact hash, and skips corrupt or
//! schema-incompatible rows so recovery can fall back to a prior checkpoint
//! or full replay. Checkpoints never write the `events` table.

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use protocol::{ArtifactId, RedactionClass, SessionId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::artifact_store::{
    ArtifactError, ArtifactMetadata, ArtifactStore, CancellationToken as ArtifactCancel,
};
use crate::event::RecordedAt;
use crate::ledger::EventLedger;

/// Maximum uncompressed projection bytes accepted by write/load.
pub const MAX_PROJECTION_BYTES: usize = 16 * 1024 * 1024;

/// Maximum published checkpoint blob size (header + deflate payload).
pub const MAX_CHECKPOINT_BLOB_BYTES: usize = MAX_PROJECTION_BYTES + 64 * 1024;

/// Media type stored with the compressed projection artifact.
pub const CHECKPOINT_MEDIA_TYPE: &str = "application/vnd.rapidlm.projection-checkpoint";

const BLOB_MAGIC: &[u8] = b"rapidlm.projection.checkpoint.v1\n";
const HEADER_LEN: usize = BLOB_MAGIC.len() + 4 + 8 + 8 + 32;
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Cooperative cancellation for checkpoint operations.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// File-backed checkpoint store over a ledger DB and artifact blobs.
#[derive(Clone, Debug)]
pub struct CheckpointStore {
    ledger: EventLedger,
    artifacts: ArtifactStore,
    fail_before_commit: Arc<AtomicBool>,
}

/// Durable checkpoint pointer. Payload bytes live in the artifact store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointRecord {
    session_id: SessionId,
    through_seq: u64,
    projection_schema: i32,
    artifact_id: ArtifactId,
    created_at: String,
}

/// Verified checkpoint plus the decompressed projection document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedCheckpoint {
    record: CheckpointRecord,
    projection: Vec<u8>,
}

/// Typed failures for checkpoint persist and load.
#[derive(Debug)]
pub enum CheckpointError {
    Cancelled,
    SessionNotFound {
        session_id: SessionId,
    },
    SequenceOutOfRange {
        session_id: SessionId,
        through_seq: u64,
        last_seq: u64,
    },
    Conflict {
        session_id: SessionId,
        through_seq: u64,
    },
    InvalidSchema,
    ProjectionBound {
        limit: usize,
        observed: usize,
    },
    BlobBound {
        limit: usize,
        observed: usize,
    },
    /// Artifact and/or row were not committed; the checkpoint is not durable.
    NotCommitted,
    Corrupt(&'static str),
    ForeignKeysDisabled,
    InvalidTimestamp,
    Artifact(ArtifactError),
    Sqlite(rusqlite::Error),
    Io(io::Error),
}

struct StoredCheckpointRow {
    session_id: SessionId,
    through_seq: u64,
    projection_schema: i32,
    artifact_id: ArtifactId,
    created_at: String,
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

    pub fn check(&self) -> Result<(), CheckpointError> {
        if self.is_cancelled() {
            Err(CheckpointError::Cancelled)
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

impl CheckpointStore {
    /// Bind a migrated ledger file to an artifact store.
    pub fn new(ledger: EventLedger, artifacts: ArtifactStore) -> Self {
        Self {
            ledger,
            artifacts,
            fail_before_commit: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn ledger(&self) -> &EventLedger {
        &self.ledger
    }

    pub fn artifacts(&self) -> &ArtifactStore {
        &self.artifacts
    }

    /// Compress `projection`, publish the artifact, then insert catalog+row.
    ///
    /// The artifact is durable before the SQLite transaction starts. Success
    /// is returned only after that transaction commits. Event history is not
    /// read for mutation and is never written.
    pub fn write_checkpoint(
        &self,
        session_id: SessionId,
        through_seq: u64,
        projection_schema: i32,
        projection: &[u8],
        cancel: &CancellationToken,
    ) -> Result<CheckpointRecord, CheckpointError> {
        cancel.check()?;
        validate_schema(projection_schema)?;
        validate_projection_bound(projection.len())?;
        if through_seq == 0 || through_seq > i64::MAX as u64 {
            return Err(CheckpointError::SequenceOutOfRange {
                session_id,
                through_seq,
                last_seq: 0,
            });
        }

        let blob = encode_blob(projection_schema, through_seq, projection)?;
        cancel.check()?;
        let refer = self.artifacts.put(
            io::Cursor::new(blob),
            ArtifactMetadata::new(CHECKPOINT_MEDIA_TYPE, RedactionClass::Project),
            &to_artifact_cancel(cancel),
        )?;

        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        cancel.check()?;
        ensure_session(&tx, session_id)?;
        let last_seq = last_seq_tx(&tx, session_id)?;
        if through_seq > last_seq {
            return Err(CheckpointError::SequenceOutOfRange {
                session_id,
                through_seq,
                last_seq,
            });
        }

        let created_at = read_created_at(&tx)?;
        match tx.execute(
            "INSERT OR IGNORE INTO artifacts
                (id, media_type, bytes, redaction, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
            params![
                refer.id.to_string(),
                CHECKPOINT_MEDIA_TYPE,
                refer.bytes as i64,
                RedactionClass::Project.as_str(),
                created_at.as_str(),
            ],
        ) {
            Ok(0 | 1) => {}
            Ok(_) => {
                return Err(CheckpointError::Corrupt(
                    "artifact catalog insert affected an unexpected row count",
                ));
            }
            Err(err) => return Err(err.into()),
        }

        match tx.execute(
            "INSERT INTO checkpoints
                (session_id, through_seq, projection_schema, artifact_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id.to_string(),
                through_seq as i64,
                projection_schema,
                refer.id.to_string(),
                created_at.as_str(),
            ],
        ) {
            Ok(1) => {}
            Ok(_) => {
                return Err(CheckpointError::Corrupt(
                    "checkpoint insert did not affect one row",
                ));
            }
            Err(err) if is_constraint(&err) => {
                return existing_or_conflict(
                    &tx,
                    session_id,
                    through_seq,
                    projection_schema,
                    refer.id,
                );
            }
            Err(err) => return Err(err.into()),
        }

        let root_key = format!("{session_id}:{through_seq}");
        tx.execute(
            "INSERT OR IGNORE INTO artifact_refs (artifact_id, root_kind, root_key, created_at)
             VALUES (?1, 'checkpoint', ?2, ?3)",
            params![refer.id.to_string(), root_key, created_at.as_str()],
        )?;

        cancel.check()?;
        if self.fail_before_commit.swap(false, Ordering::SeqCst) {
            return Err(CheckpointError::NotCommitted);
        }
        tx.commit()?;
        Ok(CheckpointRecord {
            session_id,
            through_seq,
            projection_schema,
            artifact_id: refer.id,
            created_at: created_at.as_str().to_owned(),
        })
    }

    /// Load the newest compatible checkpoint whose artifact hash verifies.
    ///
    /// A corrupt or schema-incompatible latest row is skipped. `None` means
    /// replay from the start of the event stream.
    pub fn load_checkpoint(
        &self,
        session_id: SessionId,
        projection_schema: i32,
        cancel: &CancellationToken,
    ) -> Result<Option<LoadedCheckpoint>, CheckpointError> {
        cancel.check()?;
        validate_schema(projection_schema)?;
        let conn = self.connect()?;
        ensure_session(&conn, session_id)?;
        let candidates = list_candidates(&conn, session_id)?;
        drop(conn);

        for row in candidates {
            cancel.check()?;
            if row.projection_schema != projection_schema {
                continue;
            }
            match self.load_verified(&row, cancel) {
                Ok(loaded) => return Ok(Some(loaded)),
                Err(CheckpointError::Corrupt(_)) => continue,
                Err(err) => return Err(err),
            }
        }
        Ok(None)
    }

    /// Arm a one-shot rollback after a successful INSERT and before COMMIT.
    #[cfg(test)]
    pub fn inject_fail_before_commit(&self) {
        self.fail_before_commit.store(true, Ordering::SeqCst);
    }

    fn load_verified(
        &self,
        row: &StoredCheckpointRow,
        cancel: &CancellationToken,
    ) -> Result<LoadedCheckpoint, CheckpointError> {
        let blob = match self
            .artifacts
            .get(&row.artifact_id, &to_artifact_cancel(cancel))
        {
            Ok(blob) => blob,
            Err(ArtifactError::Integrity { .. })
            | Err(ArtifactError::NotFound { .. })
            | Err(ArtifactError::Metadata)
            | Err(ArtifactError::BoundExceeded { .. }) => {
                return Err(CheckpointError::Corrupt("checkpoint artifact unusable"));
            }
            Err(ArtifactError::Cancelled) => return Err(CheckpointError::Cancelled),
            Err(err) => return Err(CheckpointError::Artifact(err)),
        };
        let (schema, through_seq, projection) = decode_blob(&blob)?;
        if schema != row.projection_schema || through_seq != row.through_seq {
            return Err(CheckpointError::Corrupt(
                "checkpoint blob does not match row key",
            ));
        }
        Ok(LoadedCheckpoint {
            record: CheckpointRecord {
                session_id: row.session_id,
                through_seq: row.through_seq,
                projection_schema: row.projection_schema,
                artifact_id: row.artifact_id,
                created_at: row.created_at.clone(),
            },
            projection,
        })
    }

    fn connect(&self) -> Result<Connection, CheckpointError> {
        let conn = Connection::open(self.ledger.path())?;
        configure_connection(&conn)?;
        Ok(conn)
    }
}

impl CheckpointRecord {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn through_seq(&self) -> u64 {
        self.through_seq
    }

    pub fn projection_schema(&self) -> i32 {
        self.projection_schema
    }

    pub fn artifact_id(&self) -> ArtifactId {
        self.artifact_id
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }
}

impl LoadedCheckpoint {
    pub fn record(&self) -> &CheckpointRecord {
        &self.record
    }

    pub fn projection(&self) -> &[u8] {
        &self.projection
    }

    pub fn through_seq(&self) -> u64 {
        self.record.through_seq
    }

    pub fn projection_schema(&self) -> i32 {
        self.record.projection_schema
    }

    pub fn artifact_id(&self) -> ArtifactId {
        self.record.artifact_id
    }
}

fn configure_connection(conn: &Connection) -> Result<(), CheckpointError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", 1)?;
    let foreign_keys: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(CheckpointError::ForeignKeysDisabled);
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn ensure_session(conn: &Connection, session_id: SessionId) -> Result<(), CheckpointError> {
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
        Err(CheckpointError::SessionNotFound { session_id })
    }
}

fn last_seq_tx(conn: &Connection, session_id: SessionId) -> Result<u64, CheckpointError> {
    let last: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
        [session_id.to_string()],
        |row| row.get(0),
    )?;
    if last < 0 {
        return Err(CheckpointError::Corrupt("negative event seq"));
    }
    Ok(last as u64)
}

fn read_created_at(conn: &Connection) -> Result<RecordedAt, CheckpointError> {
    let raw: String =
        conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
            row.get(0)
        })?;
    raw.parse().map_err(|_| CheckpointError::InvalidTimestamp)
}

fn existing_or_conflict(
    conn: &Connection,
    session_id: SessionId,
    through_seq: u64,
    projection_schema: i32,
    artifact_id: ArtifactId,
) -> Result<CheckpointRecord, CheckpointError> {
    let row = conn
        .query_row(
            "SELECT projection_schema, artifact_id, created_at
             FROM checkpoints
             WHERE session_id = ?1 AND through_seq = ?2",
            params![session_id.to_string(), through_seq as i64],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((schema, stored_artifact, created_at)) = row else {
        return Err(CheckpointError::Corrupt(
            "checkpoint unique constraint without a matching row",
        ));
    };
    let schema = i32::try_from(schema).map_err(|_| CheckpointError::Corrupt("schema overflow"))?;
    let stored_id: ArtifactId = stored_artifact
        .parse()
        .map_err(|_| CheckpointError::Corrupt("malformed stored artifact_id"))?;
    if schema != projection_schema || stored_id != artifact_id {
        return Err(CheckpointError::Conflict {
            session_id,
            through_seq,
        });
    }
    Ok(CheckpointRecord {
        session_id,
        through_seq,
        projection_schema,
        artifact_id,
        created_at,
    })
}

fn list_candidates(
    conn: &Connection,
    session_id: SessionId,
) -> Result<Vec<StoredCheckpointRow>, CheckpointError> {
    let mut stmt = conn.prepare(
        "SELECT through_seq, projection_schema, artifact_id, created_at
         FROM checkpoints
         WHERE session_id = ?1
         ORDER BY through_seq DESC",
    )?;
    let rows = stmt.query_map([session_id.to_string()], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, schema, artifact_id, created_at) = row?;
        if seq < 0 {
            return Err(CheckpointError::Corrupt("negative checkpoint through_seq"));
        }
        let projection_schema =
            i32::try_from(schema).map_err(|_| CheckpointError::Corrupt("schema overflow"))?;
        let artifact_id: ArtifactId = artifact_id
            .parse()
            .map_err(|_| CheckpointError::Corrupt("malformed stored artifact_id"))?;
        out.push(StoredCheckpointRow {
            session_id,
            through_seq: seq as u64,
            projection_schema,
            artifact_id,
            created_at,
        });
    }
    Ok(out)
}

fn validate_schema(projection_schema: i32) -> Result<(), CheckpointError> {
    if projection_schema < 0 {
        Err(CheckpointError::InvalidSchema)
    } else {
        Ok(())
    }
}

fn validate_projection_bound(observed: usize) -> Result<(), CheckpointError> {
    if observed > MAX_PROJECTION_BYTES {
        Err(CheckpointError::ProjectionBound {
            limit: MAX_PROJECTION_BYTES,
            observed,
        })
    } else {
        Ok(())
    }
}

fn encode_blob(
    projection_schema: i32,
    through_seq: u64,
    projection: &[u8],
) -> Result<Vec<u8>, CheckpointError> {
    let digest = sha256(projection);
    let compressed = deflate(projection)?;
    let mut blob = Vec::with_capacity(HEADER_LEN + compressed.len());
    blob.extend_from_slice(BLOB_MAGIC);
    blob.extend_from_slice(&projection_schema.to_le_bytes());
    blob.extend_from_slice(&through_seq.to_le_bytes());
    blob.extend_from_slice(&(projection.len() as u64).to_le_bytes());
    blob.extend_from_slice(&digest);
    blob.extend_from_slice(&compressed);
    if blob.len() > MAX_CHECKPOINT_BLOB_BYTES {
        return Err(CheckpointError::BlobBound {
            limit: MAX_CHECKPOINT_BLOB_BYTES,
            observed: blob.len(),
        });
    }
    Ok(blob)
}

fn decode_blob(blob: &[u8]) -> Result<(i32, u64, Vec<u8>), CheckpointError> {
    if blob.len() < HEADER_LEN {
        return Err(CheckpointError::Corrupt("checkpoint blob truncated"));
    }
    if !blob.starts_with(BLOB_MAGIC) {
        return Err(CheckpointError::Corrupt("checkpoint blob magic mismatch"));
    }
    if blob.len() > MAX_CHECKPOINT_BLOB_BYTES {
        return Err(CheckpointError::BlobBound {
            limit: MAX_CHECKPOINT_BLOB_BYTES,
            observed: blob.len(),
        });
    }
    let mut rest = &blob[BLOB_MAGIC.len()..];
    let schema = read_i32(&mut rest)?;
    let through_seq = read_u64(&mut rest)?;
    let uncompressed_len = read_u64(&mut rest)?;
    let expected_digest = read_digest(&mut rest)?;
    if uncompressed_len > MAX_PROJECTION_BYTES as u64 {
        return Err(CheckpointError::ProjectionBound {
            limit: MAX_PROJECTION_BYTES,
            observed: uncompressed_len as usize,
        });
    }
    let projection = inflate(rest, uncompressed_len)?;
    if sha256(&projection) != expected_digest {
        return Err(CheckpointError::Corrupt(
            "uncompressed projection hash mismatch",
        ));
    }
    Ok((schema, through_seq, projection))
}

fn deflate(plain: &[u8]) -> Result<Vec<u8>, CheckpointError> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(plain)?;
    let compressed = encoder.finish()?;
    Ok(compressed)
}

fn inflate(compressed: &[u8], expected_len: u64) -> Result<Vec<u8>, CheckpointError> {
    let mut decoder = DeflateDecoder::new(compressed);
    let mut limited = (&mut decoder).take(expected_len.saturating_add(1));
    let mut out = Vec::new();
    limited
        .read_to_end(&mut out)
        .map_err(|_| CheckpointError::Corrupt("checkpoint deflate payload is invalid"))?;
    if out.len() as u64 != expected_len {
        return Err(CheckpointError::Corrupt(
            "uncompressed projection length mismatch",
        ));
    }
    let mut tail = [0u8; 1];
    match decoder.read(&mut tail) {
        Ok(0) => Ok(out),
        Ok(_) => Err(CheckpointError::Corrupt(
            "checkpoint deflate payload has trailing bytes",
        )),
        Err(_) => Err(CheckpointError::Corrupt(
            "checkpoint deflate payload is invalid",
        )),
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn read_i32(buf: &mut &[u8]) -> Result<i32, CheckpointError> {
    let (head, rest) = buf
        .split_at_checked(4)
        .ok_or(CheckpointError::Corrupt("truncated header"))?;
    let mut raw = [0u8; 4];
    raw.copy_from_slice(head);
    *buf = rest;
    Ok(i32::from_le_bytes(raw))
}

fn read_u64(buf: &mut &[u8]) -> Result<u64, CheckpointError> {
    let (head, rest) = buf
        .split_at_checked(8)
        .ok_or(CheckpointError::Corrupt("truncated header"))?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(head);
    *buf = rest;
    Ok(u64::from_le_bytes(raw))
}

fn read_digest(buf: &mut &[u8]) -> Result<[u8; 32], CheckpointError> {
    let (head, rest) = buf
        .split_at_checked(32)
        .ok_or(CheckpointError::Corrupt("truncated header"))?;
    let mut raw = [0u8; 32];
    raw.copy_from_slice(head);
    *buf = rest;
    Ok(raw)
}

fn to_artifact_cancel(cancel: &CancellationToken) -> ArtifactCancel {
    let token = ArtifactCancel::new();
    if cancel.is_cancelled() {
        token.cancel();
    }
    token
}

fn is_constraint(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ConstraintViolation)
    )
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("checkpoint operation cancelled"),
            Self::SessionNotFound { session_id } => {
                write!(f, "session {session_id} not found")
            }
            Self::SequenceOutOfRange {
                session_id,
                through_seq,
                last_seq,
            } => write!(
                f,
                "session {session_id} checkpoint through_seq {through_seq} exceeds last_seq {last_seq}"
            ),
            Self::Conflict {
                session_id,
                through_seq,
            } => write!(
                f,
                "session {session_id} already has a different checkpoint at seq {through_seq}"
            ),
            Self::InvalidSchema => f.write_str("projection schema must be non-negative"),
            Self::ProjectionBound { limit, observed } => {
                write!(f, "projection exceeds bound {limit}, observed {observed}")
            }
            Self::BlobBound { limit, observed } => {
                write!(
                    f,
                    "checkpoint blob exceeds bound {limit}, observed {observed}"
                )
            }
            Self::NotCommitted => {
                f.write_str("checkpoint insert was not committed and must not be acknowledged")
            }
            Self::Corrupt(reason) => write!(f, "checkpoint data is corrupt: {reason}"),
            Self::ForeignKeysDisabled => f.write_str("sqlite foreign_keys pragma is disabled"),
            Self::InvalidTimestamp => f.write_str("sqlite produced a non-canonical created_at"),
            Self::Artifact(err) => write!(f, "checkpoint artifact error: {err}"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::Io(err) => write!(f, "checkpoint io error: {err}"),
        }
    }
}

impl std::error::Error for CheckpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Artifact(err) => Some(err),
            Self::Sqlite(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::Cancelled
            | Self::SessionNotFound { .. }
            | Self::SequenceOutOfRange { .. }
            | Self::Conflict { .. }
            | Self::InvalidSchema
            | Self::ProjectionBound { .. }
            | Self::BlobBound { .. }
            | Self::NotCommitted
            | Self::Corrupt(_)
            | Self::ForeignKeysDisabled
            | Self::InvalidTimestamp => None,
        }
    }
}

impl From<ArtifactError> for CheckpointError {
    fn from(value: ArtifactError) -> Self {
        match value {
            ArtifactError::Cancelled => Self::Cancelled,
            other => Self::Artifact(other),
        }
    }
}

impl From<rusqlite::Error> for CheckpointError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<io::Error> for CheckpointError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ActorKind, ActorRef, EventKind};
    use crate::ledger::{AppendOptions, CancellationToken as LedgerCancel};
    use protocol::{EventId, ProjectId, TraceId};
    use rusqlite::Connection;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempCheckpoints {
        store: CheckpointStore,
        db_path: PathBuf,
        artifact_root: PathBuf,
    }

    impl TempCheckpoints {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let db_path =
                std::env::temp_dir().join(format!("rapidlm-checkpoint-{pid}-{seq}.sqlite"));
            let artifact_root =
                std::env::temp_dir().join(format!("rapidlm-checkpoint-art-{pid}-{seq}"));
            remove_db_files(&db_path);
            let _ = fs::remove_dir_all(&artifact_root);
            let ledger = EventLedger::open(&db_path).expect("open ledger");
            let artifacts = ArtifactStore::create(&artifact_root).expect("open artifacts");
            Self {
                store: CheckpointStore::new(ledger, artifacts),
                db_path,
                artifact_root,
            }
        }
    }

    impl Drop for TempCheckpoints {
        fn drop(&mut self) {
            remove_db_files(&self.db_path);
            let _ = fs::remove_dir_all(&self.artifact_root);
        }
    }

    fn remove_db_files(path: &std::path::Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(sidecar(path, "-wal"));
        let _ = fs::remove_file(sidecar(path, "-shm"));
        let _ = fs::remove_file(sidecar(path, "-journal"));
    }

    fn sidecar(path: &std::path::Path, suffix: &str) -> PathBuf {
        let mut raw = path.as_os_str().to_os_string();
        raw.push(suffix);
        PathBuf::from(raw)
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn ledger_live() -> LedgerCancel {
        LedgerCancel::new()
    }

    fn actor() -> ActorRef {
        ActorRef::new(ActorKind::System, &EventId::new().to_string()).expect("actor")
    }

    fn options() -> AppendOptions {
        AppendOptions {
            redaction: RedactionClass::Project,
            trace_id: TraceId::new(),
            expected_seq: None,
        }
    }

    fn seed_session(store: &CheckpointStore, events: usize) -> SessionId {
        let session = SessionId::new();
        store
            .ledger
            .create_session(session, ProjectId::new(), &ledger_live())
            .expect("create session");
        for i in 0..events {
            store
                .ledger
                .append(
                    session,
                    actor(),
                    EventKind::TurnStarted,
                    serde_json::json!({ "i": i }),
                    &options(),
                    &ledger_live(),
                )
                .expect("append");
        }
        session
    }

    fn event_count(path: &std::path::Path, session: SessionId) -> i64 {
        let conn = Connection::open(path).expect("open db");
        conn.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1",
            [session.to_string()],
            |row| row.get(0),
        )
        .expect("count events")
    }

    fn checkpoint_count(path: &std::path::Path, session: SessionId) -> i64 {
        let conn = Connection::open(path).expect("open db");
        conn.query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE session_id = ?1",
            [session.to_string()],
            |row| row.get(0),
        )
        .expect("count checkpoints")
    }

    fn blob_path(store: &ArtifactStore, id: ArtifactId) -> PathBuf {
        let wire = id.to_string();
        let hex = wire
            .strip_prefix(protocol::ARTIFACT_ID_PREFIX)
            .expect("canonical artifact id");
        store.root().join("blobs").join(&hex[..2]).join(hex)
    }

    #[test]
    fn write_then_load_round_trips_and_verifies_artifact_hash() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 3);
        let projection = b"{\"schema\":1,\"status\":\"ready\"}".repeat(64);
        let written = tmp
            .store
            .write_checkpoint(session, 3, 1, &projection, &live())
            .expect("write");
        assert_eq!(written.session_id(), session);
        assert_eq!(written.through_seq(), 3);
        assert_eq!(written.projection_schema(), 1);

        let loaded = tmp
            .store
            .load_checkpoint(session, 1, &live())
            .expect("load")
            .expect("present");
        assert_eq!(loaded.projection(), projection);
        assert_eq!(loaded.through_seq(), 3);
        assert_eq!(loaded.artifact_id(), written.artifact_id());
        assert_eq!(loaded.record().session_id(), session);

        let blob = tmp
            .store
            .artifacts
            .get(&written.artifact_id(), &ArtifactCancel::new())
            .expect("get artifact");
        assert_eq!(ArtifactId::from_bytes(&blob), written.artifact_id());
        assert!(blob.starts_with(BLOB_MAGIC), "compressed wrapper missing");
        assert!(
            blob.len() < HEADER_LEN + projection.len(),
            "expected deflate to shrink {} bytes, got {}",
            projection.len(),
            blob.len()
        );
    }

    #[test]
    fn successful_write_is_durable_after_reopen() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 2);
        let projection = b"durable-projection";
        let written = tmp
            .store
            .write_checkpoint(session, 2, 1, projection, &live())
            .expect("write");

        let reopened = CheckpointStore::new(
            EventLedger::open(&tmp.db_path).expect("reopen ledger"),
            ArtifactStore::create(&tmp.artifact_root).expect("reopen artifacts"),
        );
        let loaded = reopened
            .load_checkpoint(session, 1, &live())
            .expect("load after reopen")
            .expect("present");
        assert_eq!(loaded.projection(), projection);
        assert_eq!(loaded.artifact_id(), written.artifact_id());
        assert_eq!(loaded.through_seq(), 2);
        assert_eq!(
            reopened
                .ledger
                .last_seq(session, &ledger_live())
                .expect("events survive"),
            2
        );
    }

    #[test]
    fn write_does_not_alter_event_history() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 2);
        let before = event_count(&tmp.db_path, session);
        let last = tmp
            .store
            .ledger
            .last_seq(session, &ledger_live())
            .expect("last");
        let first = tmp
            .store
            .ledger
            .get(session, 1, &ledger_live())
            .expect("get 1");
        tmp.store
            .write_checkpoint(session, 2, 1, b"proj-a", &live())
            .expect("write");
        assert_eq!(event_count(&tmp.db_path, session), before);
        assert_eq!(
            tmp.store
                .ledger
                .last_seq(session, &ledger_live())
                .expect("last after"),
            last
        );
        let again = tmp
            .store
            .ledger
            .get(session, 1, &ledger_live())
            .expect("get 1 after");
        assert_eq!(again.event_id(), first.event_id());
        assert_eq!(again.payload(), first.payload());
        assert_eq!(checkpoint_count(&tmp.db_path, session), 1);
    }

    #[test]
    fn corrupt_latest_falls_back_to_prior() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 3);
        tmp.store
            .write_checkpoint(session, 1, 1, b"first", &live())
            .expect("write 1");
        let latest = tmp
            .store
            .write_checkpoint(session, 3, 1, b"third", &live())
            .expect("write 3");
        fs::write(
            blob_path(&tmp.store.artifacts, latest.artifact_id()),
            b"xxx",
        )
        .expect("corrupt latest");

        let loaded = tmp
            .store
            .load_checkpoint(session, 1, &live())
            .expect("load")
            .expect("prior");
        assert_eq!(loaded.through_seq(), 1);
        assert_eq!(loaded.projection(), b"first");
    }

    #[test]
    fn latest_incompatible_schema_falls_back_to_prior() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 3);
        tmp.store
            .write_checkpoint(session, 1, 1, b"schema-1", &live())
            .expect("write schema 1");
        tmp.store
            .write_checkpoint(session, 3, 2, b"schema-2", &live())
            .expect("write schema 2");

        let loaded = tmp
            .store
            .load_checkpoint(session, 1, &live())
            .expect("load")
            .expect("compatible prior");
        assert_eq!(loaded.through_seq(), 1);
        assert_eq!(loaded.projection_schema(), 1);
        assert_eq!(loaded.projection(), b"schema-1");
    }

    #[test]
    fn all_unusable_checkpoints_yield_replay() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 2);
        let only = tmp
            .store
            .write_checkpoint(session, 2, 1, b"only", &live())
            .expect("write");
        fs::remove_file(blob_path(&tmp.store.artifacts, only.artifact_id())).expect("delete blob");
        let loaded = tmp
            .store
            .load_checkpoint(session, 1, &live())
            .expect("load");
        assert!(loaded.is_none(), "expected full replay, got {loaded:?}");
    }

    #[test]
    fn fail_before_commit_does_not_acknowledge_checkpoint() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 1);
        tmp.store.inject_fail_before_commit();
        let err = tmp
            .store
            .write_checkpoint(session, 1, 1, b"lost", &live())
            .expect_err("injected pre-commit failure");
        assert!(
            matches!(err, CheckpointError::NotCommitted),
            "acknowledged uncommitted checkpoint: {err}"
        );
        assert_eq!(checkpoint_count(&tmp.db_path, session), 0);
        assert!(
            tmp.store
                .load_checkpoint(session, 1, &live())
                .expect("load")
                .is_none()
        );
        assert_eq!(event_count(&tmp.db_path, session), 1);
    }

    #[test]
    fn write_is_idempotent_for_same_key_and_bytes() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 1);
        let first = tmp
            .store
            .write_checkpoint(session, 1, 1, b"same", &live())
            .expect("first");
        let second = tmp
            .store
            .write_checkpoint(session, 1, 1, b"same", &live())
            .expect("second");
        assert_eq!(first.artifact_id(), second.artifact_id());
        assert_eq!(checkpoint_count(&tmp.db_path, session), 1);
    }

    #[test]
    fn conflicting_payload_at_same_seq_is_rejected() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 1);
        tmp.store
            .write_checkpoint(session, 1, 1, b"a", &live())
            .expect("first");
        let err = tmp
            .store
            .write_checkpoint(session, 1, 1, b"b", &live())
            .expect_err("conflict");
        assert!(matches!(
            err,
            CheckpointError::Conflict {
                session_id,
                through_seq: 1
            } if session_id == session
        ));
    }

    #[test]
    fn through_seq_past_last_event_is_rejected() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 1);
        let err = tmp
            .store
            .write_checkpoint(session, 9, 1, b"future", &live())
            .expect_err("future seq");
        match err {
            CheckpointError::SequenceOutOfRange {
                session_id,
                through_seq,
                last_seq,
            } => {
                assert_eq!(session_id, session);
                assert_eq!(through_seq, 9);
                assert_eq!(last_seq, 1);
            }
            other => panic!("expected SequenceOutOfRange, got {other}"),
        }
        assert_eq!(checkpoint_count(&tmp.db_path, session), 0);
    }

    #[test]
    fn missing_session_is_not_checkpointed() {
        let tmp = TempCheckpoints::create();
        let session = SessionId::new();
        let err = tmp
            .store
            .write_checkpoint(session, 1, 1, b"x", &live())
            .expect_err("missing session");
        assert!(matches!(
            err,
            CheckpointError::SessionNotFound { session_id } if session_id == session
        ));
    }

    #[test]
    fn cancelled_write_is_not_acknowledged() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 1);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tmp
            .store
            .write_checkpoint(session, 1, 1, b"x", &cancel)
            .expect_err("cancelled");
        assert!(matches!(err, CheckpointError::Cancelled));
        assert_eq!(checkpoint_count(&tmp.db_path, session), 0);
    }

    #[test]
    fn projection_bound_is_enforced() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 1);
        let too_big = vec![b'x'; MAX_PROJECTION_BYTES + 1];
        let err = tmp
            .store
            .write_checkpoint(session, 1, 1, &too_big, &live())
            .expect_err("bound");
        assert!(
            matches!(
                err,
                CheckpointError::ProjectionBound {
                    limit: MAX_PROJECTION_BYTES,
                    observed
                } if observed == MAX_PROJECTION_BYTES + 1
            ),
            "got {err}"
        );
        assert_eq!(checkpoint_count(&tmp.db_path, session), 0);
    }

    #[test]
    fn load_empty_session_is_replay() {
        let tmp = TempCheckpoints::create();
        let session = seed_session(&tmp.store, 1);
        assert!(
            tmp.store
                .load_checkpoint(session, 1, &live())
                .expect("load")
                .is_none()
        );
    }
}
