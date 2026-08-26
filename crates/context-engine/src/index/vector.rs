//! Optional rebuildable vector index keyed by chunk ID.
//!
//! Embeddings are supplied by an injected [`EmbeddingProvider`]. Offline mode
//! is [`VectorIndex::Disabled`] and keeps the same search/write surface.
//! Missing or corrupt on-disk caches degrade health and return no vector hits;
//! they do not surface as a hard search failure.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use protocol::{DEFAULT_MAX_INDEX_BYTES, EmbeddingsMode, RedactionClass, RepoId, RepoPath};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::chunk::{ChunkId, ChunkRecord, DEFAULT_MAX_CHUNK_BYTES, DEFAULT_MAX_CHUNKS};
use crate::ingest::content::{ContentHash, SourceLanguage};
use crate::repo_manifest::CancellationToken;

/// Default wall-clock budget for one writer or search call.
pub const DEFAULT_VECTOR_TIMEOUT: Duration = Duration::from_secs(5);

/// Default caller-facing search page size.
pub const DEFAULT_SEARCH_LIMIT: u32 = 20;

/// Hard default cap applied even when a caller asks for more hits.
pub const DEFAULT_MAX_VECTOR_RESULTS: u32 = 256;

/// Hard default cap on one embedding vector's dimensions.
pub const DEFAULT_MAX_VECTOR_DIMS: u32 = 4_096;

/// UTF-8 byte cap for an embedding-version label.
pub const MAX_VERSION_BYTES: usize = 64;

/// Bounded SQLite lock wait. Matches the ledger busy-timeout recovery rule.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

const CANCEL_STRIDE: usize = 16;
const SCHEMA_KIND: &str = "rapidlm.vector.v1";
const SCHEMA_VERSION: &str = "1";
const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

const HEALTH_HEALTHY: u8 = 0;
const HEALTH_MISSING: u8 = 1;
const HEALTH_CORRUPT: u8 = 2;
const HEALTH_PROVIDER: u8 = 3;

const SCHEMA_META: &str = "
CREATE TABLE IF NOT EXISTS context_vector_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
";

const SCHEMA_VECTORS: &str = "
CREATE TABLE IF NOT EXISTS context_vectors (
  chunk_id TEXT PRIMARY KEY,
  repo_id TEXT NOT NULL,
  path TEXT NOT NULL,
  language TEXT,
  start_byte INTEGER NOT NULL,
  end_byte INTEGER NOT NULL,
  content_hash TEXT NOT NULL,
  embedding_version TEXT NOT NULL,
  dims INTEGER NOT NULL,
  vector BLOB NOT NULL
);
";

const SCHEMA_PATH_INDEX: &str = "
CREATE INDEX IF NOT EXISTS context_vectors_repo_path
  ON context_vectors(repo_id, path);
";

/// Per-index resource bounds. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct VectorLimits {
    timeout: Duration,
    max_results: u32,
    max_chunks: usize,
    max_chunk_bytes: usize,
    max_dims: u32,
    max_index_bytes: u64,
    cancel: CancellationToken,
}

/// Stable embedding model/version label stored with each vector.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct EmbeddingVersion(String);

/// Injected embedder. Context Engine never calls an LLM provider directly.
pub trait EmbeddingProvider {
    fn version(&self) -> &EmbeddingVersion;
    fn dimensions(&self) -> u32;
    fn embed(
        &self,
        texts: &[&str],
        cancel: &CancellationToken,
    ) -> Result<Vec<Vec<f32>>, VectorError>;
}

/// One source path being written. Empty `chunks` means “remove this path”.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorDocument {
    repo_id: RepoId,
    path: RepoPath,
    language: Option<SourceLanguage>,
    chunks: Vec<VectorChunk>,
}

/// One embeddable (or secret-excluded) chunk, keyed by [`ChunkId`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorChunk {
    id: ChunkId,
    start_byte: u32,
    end_byte: u32,
    content_hash: ContentHash,
    text: String,
    redaction: RedactionClass,
}

/// Nearest-neighbor query. The vector is precomputed by the caller/provider.
#[derive(Clone, Debug)]
pub struct VectorQuery {
    vector: Vec<f32>,
    repo_id: Option<RepoId>,
    path: Option<RepoPath>,
    language: Option<SourceLanguage>,
    version: Option<EmbeddingVersion>,
    limit: u32,
    cancel: CancellationToken,
}

/// One cosine hit. `chunk_id` is the stored chunk-ID wire form.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorHit {
    chunk_id: String,
    repo_id: RepoId,
    path: RepoPath,
    language: Option<SourceLanguage>,
    start_byte: u32,
    end_byte: u32,
    content_hash: String,
    embedding_version: EmbeddingVersion,
    score: f32,
}

/// Rows removed and inserted by one transactional write.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VectorWriteStats {
    removed: u64,
    inserted: u64,
    skipped_secret: u64,
}

/// Observable index health. Missing/corrupt is degraded, never a lexical outage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorHealth {
    Disabled,
    Healthy,
    Degraded(VectorDegradeReason),
}

/// Why an enabled index is not serving (or not accepting) vectors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorDegradeReason {
    Missing,
    Corrupt,
    ProviderUnavailable,
}

/// Rebuildable vector cache. `Disabled` is offline mode and a no-op API.
pub enum VectorIndex {
    Disabled,
    Enabled(EnabledVectorIndex),
}

/// File-backed or in-memory enabled cache.
pub struct EnabledVectorIndex {
    conn: Option<Connection>,
    persist: Option<PathBuf>,
    limits: VectorLimits,
    health: AtomicU8,
}

/// Typed vector failure. Display never echoes source, queries, or host paths.
#[derive(Debug)]
pub enum VectorError {
    Cancelled,
    Timeout,
    EmptyQuery,
    InvalidLimit,
    InvalidDocument,
    InvalidPolicy,
    InvalidEmbedding,
    InvalidVersion,
    TooManyChunks,
    ChunkTooLarge,
    IndexTooLarge,
    ProviderUnavailable,
    /// Write ran but the transaction did not commit; prior rows are intact.
    NotCommitted,
    Corrupt(&'static str),
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

struct PreparedVector {
    values: Vec<f32>,
    blob: Vec<u8>,
}

struct PlannedRow {
    chunk: VectorChunk,
    version: EmbeddingVersion,
    vector: PreparedVector,
}

impl VectorLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn max_results(mut self, value: u32) -> Self {
        self.max_results = value;
        self
    }

    pub fn max_chunks(mut self, value: usize) -> Self {
        self.max_chunks = value;
        self
    }

    pub fn max_chunk_bytes(mut self, value: usize) -> Self {
        self.max_chunk_bytes = value;
        self
    }

    pub fn max_dims(mut self, value: u32) -> Self {
        self.max_dims = value;
        self
    }

    pub fn max_index_bytes(mut self, value: u64) -> Self {
        self.max_index_bytes = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }

    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }

    pub fn max_results_value(&self) -> u32 {
        self.max_results
    }

    pub fn max_chunks_value(&self) -> usize {
        self.max_chunks
    }

    pub fn max_chunk_bytes_value(&self) -> usize {
        self.max_chunk_bytes
    }

    pub fn max_dims_value(&self) -> u32 {
        self.max_dims
    }

    pub fn max_index_bytes_value(&self) -> u64 {
        self.max_index_bytes
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }

    fn validate(&self) -> Result<(), VectorError> {
        if self.max_results == 0
            || self.max_chunks == 0
            || self.max_chunk_bytes == 0
            || self.max_dims == 0
        {
            return Err(VectorError::InvalidPolicy);
        }
        Ok(())
    }
}

impl Default for VectorLimits {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_VECTOR_TIMEOUT,
            max_results: DEFAULT_MAX_VECTOR_RESULTS,
            max_chunks: DEFAULT_MAX_CHUNKS,
            max_chunk_bytes: DEFAULT_MAX_CHUNK_BYTES,
            max_dims: DEFAULT_MAX_VECTOR_DIMS,
            max_index_bytes: DEFAULT_MAX_INDEX_BYTES,
            cancel: CancellationToken::new(),
        }
    }
}

impl EmbeddingVersion {
    pub fn new(value: impl Into<String>) -> Result<Self, VectorError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_VERSION_BYTES
            || value.contains('\0')
            || value.chars().any(char::is_control)
        {
            return Err(VectorError::InvalidVersion);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EmbeddingVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl VectorDocument {
    pub fn new(repo_id: RepoId, path: RepoPath, language: Option<SourceLanguage>) -> Self {
        Self {
            repo_id,
            path,
            language,
            chunks: Vec::new(),
        }
    }

    /// Build a document from chunk records. Requires at least one record.
    pub fn from_records(
        records: &[ChunkRecord],
        redaction: RedactionClass,
    ) -> Result<Self, VectorError> {
        let Some(first) = records.first() else {
            return Err(VectorError::InvalidDocument);
        };
        let mut document = Self::new(first.repo_id(), first.path().clone(), first.language());
        document.push_records(records, redaction)?;
        Ok(document)
    }

    pub fn push_records(
        &mut self,
        records: &[ChunkRecord],
        redaction: RedactionClass,
    ) -> Result<(), VectorError> {
        for record in records {
            if record.repo_id() != self.repo_id || record.path() != &self.path {
                return Err(VectorError::InvalidDocument);
            }
            self.chunks
                .push(VectorChunk::from_record(record, redaction));
        }
        Ok(())
    }

    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn language(&self) -> Option<SourceLanguage> {
        self.language
    }

    pub fn chunks(&self) -> &[VectorChunk] {
        &self.chunks
    }
}

impl VectorChunk {
    pub fn from_record(record: &ChunkRecord, redaction: RedactionClass) -> Self {
        Self {
            id: record.id(),
            start_byte: record.start_byte(),
            end_byte: record.end_byte(),
            content_hash: record.content_hash(),
            text: record.text().to_string(),
            redaction,
        }
    }

    pub fn id(&self) -> ChunkId {
        self.id
    }

    pub fn start_byte(&self) -> u32 {
        self.start_byte
    }

    pub fn end_byte(&self) -> u32 {
        self.end_byte
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn redaction(&self) -> RedactionClass {
        self.redaction
    }
}

/// Text sent to an embedder. Secret-classified sources are never included.
pub fn embedding_text(chunk: &VectorChunk) -> Option<&str> {
    if chunk.redaction == RedactionClass::Secret || chunk.text.is_empty() {
        None
    } else {
        Some(chunk.text.as_str())
    }
}

impl VectorQuery {
    pub fn new(vector: Vec<f32>) -> Self {
        Self {
            vector,
            repo_id: None,
            path: None,
            language: None,
            version: None,
            limit: DEFAULT_SEARCH_LIMIT,
            cancel: CancellationToken::new(),
        }
    }

    /// Embed `text` through `provider`. The resulting query keeps the same API.
    pub fn from_text(
        text: &str,
        provider: &dyn EmbeddingProvider,
        cancel: &CancellationToken,
    ) -> Result<Self, VectorError> {
        if text.is_empty() {
            return Err(VectorError::EmptyQuery);
        }
        let mut vectors = provider.embed(&[text], cancel)?;
        if vectors.len() != 1 {
            return Err(VectorError::InvalidEmbedding);
        }
        let vector = vectors.remove(0);
        Ok(Self::new(vector))
    }

    pub fn repo(mut self, repo_id: RepoId) -> Self {
        self.repo_id = Some(repo_id);
        self
    }

    pub fn path(mut self, path: RepoPath) -> Self {
        self.path = Some(path);
        self
    }

    pub fn language(mut self, language: SourceLanguage) -> Self {
        self.language = Some(language);
        self
    }

    pub fn version(mut self, version: EmbeddingVersion) -> Self {
        self.version = Some(version);
        self
    }

    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    pub fn cancellation(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn vector(&self) -> &[f32] {
        &self.vector
    }

    pub fn repo_id(&self) -> Option<RepoId> {
        self.repo_id
    }

    pub fn path_filter(&self) -> Option<&RepoPath> {
        self.path.as_ref()
    }

    pub fn language_filter(&self) -> Option<SourceLanguage> {
        self.language
    }

    pub fn version_filter(&self) -> Option<&EmbeddingVersion> {
        self.version.as_ref()
    }

    pub fn limit_value(&self) -> u32 {
        self.limit
    }
}

impl VectorHit {
    pub fn chunk_id(&self) -> &str {
        &self.chunk_id
    }

    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn language(&self) -> Option<SourceLanguage> {
        self.language
    }

    pub fn start_byte(&self) -> u32 {
        self.start_byte
    }

    pub fn end_byte(&self) -> u32 {
        self.end_byte
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub fn embedding_version(&self) -> &EmbeddingVersion {
        &self.embedding_version
    }

    pub fn score(&self) -> f32 {
        self.score
    }
}

impl VectorWriteStats {
    pub fn removed(self) -> u64 {
        self.removed
    }

    pub fn inserted(self) -> u64 {
        self.inserted
    }

    pub fn skipped_secret(self) -> u64 {
        self.skipped_secret
    }
}

impl VectorHealth {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Healthy => "healthy",
            Self::Degraded(VectorDegradeReason::Missing) => "degraded_missing",
            Self::Degraded(VectorDegradeReason::Corrupt) => "degraded_corrupt",
            Self::Degraded(VectorDegradeReason::ProviderUnavailable) => {
                "degraded_provider_unavailable"
            }
        }
    }

    pub const fn is_degraded(self) -> bool {
        matches!(self, Self::Degraded(_))
    }
}

impl fmt::Display for VectorHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl VectorDegradeReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Corrupt => "corrupt",
            Self::ProviderUnavailable => "provider_unavailable",
        }
    }
}

impl VectorIndex {
    /// Offline index. Writes and searches are no-ops; the engine API is unchanged.
    pub fn disabled() -> Self {
        Self::Disabled
    }

    /// `EmbeddingsMode::Off` disables the cache. `Auto` opens the rebuildable file.
    pub fn from_mode(
        mode: EmbeddingsMode,
        path: impl AsRef<Path>,
        limits: VectorLimits,
    ) -> Result<Self, VectorError> {
        if mode == EmbeddingsMode::Off {
            Ok(Self::Disabled)
        } else {
            Self::open(path, limits)
        }
    }

    /// Open a file-backed cache. Missing/corrupt files degrade instead of failing.
    pub fn open(path: impl AsRef<Path>, limits: VectorLimits) -> Result<Self, VectorError> {
        limits.validate()?;
        let persist = path.as_ref().to_path_buf();
        if !persist.exists() {
            return Ok(Self::Enabled(EnabledVectorIndex::degraded(
                persist,
                limits,
                VectorDegradeReason::Missing,
            )));
        }
        if inspect_existing(&persist).is_err() {
            return Ok(Self::Enabled(EnabledVectorIndex::degraded(
                persist,
                limits,
                VectorDegradeReason::Corrupt,
            )));
        }
        match EnabledVectorIndex::open_ready(&persist, limits.clone()) {
            Ok(enabled) => Ok(Self::Enabled(enabled)),
            Err(VectorError::InvalidPolicy) => Err(VectorError::InvalidPolicy),
            Err(_) => Ok(Self::Enabled(EnabledVectorIndex::degraded(
                persist,
                limits,
                VectorDegradeReason::Corrupt,
            ))),
        }
    }

    /// Open a process-private in-memory index. Rebuildable; not durable.
    pub fn open_in_memory(limits: VectorLimits) -> Result<Self, VectorError> {
        limits.validate()?;
        let conn = Connection::open_in_memory()?;
        configure_connection(&conn, false)?;
        ensure_schema(&conn)?;
        Ok(Self::Enabled(EnabledVectorIndex {
            conn: Some(conn),
            persist: None,
            limits,
            health: AtomicU8::new(HEALTH_HEALTHY),
        }))
    }

    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }

    pub fn health(&self) -> VectorHealth {
        match self {
            Self::Disabled => VectorHealth::Disabled,
            Self::Enabled(inner) => inner.health(),
        }
    }

    /// Replace every vector for `document.path` in one transaction.
    pub fn upsert_document(
        &mut self,
        document: &VectorDocument,
        provider: &dyn EmbeddingProvider,
    ) -> Result<VectorWriteStats, VectorError> {
        match self {
            Self::Disabled => Ok(count_secret_skips(document)),
            Self::Enabled(inner) => inner.upsert_document(document, provider),
        }
    }

    /// Wipe the cache and rewrite `documents`. Secret chunks stay unembedded.
    pub fn rebuild(
        &mut self,
        documents: &[VectorDocument],
        provider: &dyn EmbeddingProvider,
    ) -> Result<VectorWriteStats, VectorError> {
        match self {
            Self::Disabled => Ok(VectorWriteStats::default()),
            Self::Enabled(inner) => inner.rebuild(documents, provider),
        }
    }

    /// Remove one vector by chunk-ID wire form.
    pub fn delete_document(&mut self, chunk_id: &str) -> Result<bool, VectorError> {
        match self {
            Self::Disabled => Ok(false),
            Self::Enabled(inner) => inner.delete_document(chunk_id),
        }
    }

    /// Cosine search. Disabled/missing/corrupt return an empty page, not an error.
    pub fn search(&self, query: &VectorQuery) -> Result<Vec<VectorHit>, VectorError> {
        match self {
            Self::Disabled => Ok(Vec::new()),
            Self::Enabled(inner) => inner.search(query),
        }
    }
}

impl EnabledVectorIndex {
    fn degraded(persist: PathBuf, limits: VectorLimits, reason: VectorDegradeReason) -> Self {
        Self {
            conn: None,
            persist: Some(persist),
            limits,
            health: AtomicU8::new(health_code(reason)),
        }
    }

    fn open_ready(path: &Path, limits: VectorLimits) -> Result<Self, VectorError> {
        let conn = Connection::open(path)?;
        configure_connection(&conn, true)?;
        ensure_schema(&conn)?;
        Ok(Self {
            conn: Some(conn),
            persist: Some(path.to_path_buf()),
            limits,
            health: AtomicU8::new(HEALTH_HEALTHY),
        })
    }

    pub fn health(&self) -> VectorHealth {
        decode_health(self.health.load(AtomicOrdering::SeqCst))
    }

    fn set_health(&self, health: VectorHealth) {
        self.health
            .store(encode_health(health), AtomicOrdering::SeqCst);
    }

    fn upsert_document(
        &mut self,
        document: &VectorDocument,
        provider: &dyn EmbeddingProvider,
    ) -> Result<VectorWriteStats, VectorError> {
        let started = Instant::now();
        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        validate_document(document, &self.limits)?;
        if matches!(
            self.health(),
            VectorHealth::Degraded(VectorDegradeReason::Corrupt)
        ) {
            return Ok(count_secret_skips(document));
        }
        let planned = match plan_embeddings(document, provider, &self.limits, started) {
            Ok(planned) => planned,
            Err(VectorError::ProviderUnavailable) => {
                self.set_health(VectorHealth::Degraded(
                    VectorDegradeReason::ProviderUnavailable,
                ));
                return Ok(count_secret_skips(document));
            }
            Err(err) => return Err(err),
        };
        if !self.ensure_store(started, false)? {
            return Ok(count_secret_skips(document));
        }
        self.write_path(document, &planned, started)
    }

    fn rebuild(
        &mut self,
        documents: &[VectorDocument],
        provider: &dyn EmbeddingProvider,
    ) -> Result<VectorWriteStats, VectorError> {
        let started = Instant::now();
        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        let mut planned_docs = Vec::new();
        let mut skipped_secret = 0u64;
        for (step, document) in documents.iter().enumerate() {
            if step.is_multiple_of(CANCEL_STRIDE) {
                check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
            }
            validate_document(document, &self.limits)?;
            skipped_secret =
                skipped_secret.saturating_add(count_secret_skips(document).skipped_secret);
            match plan_embeddings(document, provider, &self.limits, started) {
                Ok(planned) => planned_docs.push((document, planned)),
                Err(VectorError::ProviderUnavailable) => {
                    self.set_health(VectorHealth::Degraded(
                        VectorDegradeReason::ProviderUnavailable,
                    ));
                    return Ok(VectorWriteStats {
                        skipped_secret,
                        ..VectorWriteStats::default()
                    });
                }
                Err(err) => return Err(err),
            }
        }
        if !self.ensure_store(started, true)? {
            return Ok(VectorWriteStats {
                skipped_secret,
                ..VectorWriteStats::default()
            });
        }
        let Some(conn) = self.conn.as_mut() else {
            return Ok(VectorWriteStats {
                skipped_secret,
                ..VectorWriteStats::default()
            });
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        tx.execute("DELETE FROM context_vectors", [])?;
        let mut inserted = 0u64;
        for (document, planned) in &planned_docs {
            inserted = inserted.saturating_add(insert_rows(
                &tx,
                document,
                planned,
                &self.limits,
                started,
            )?);
        }
        ensure_index_budget(&tx, self.limits.max_index_bytes)?;
        tx.commit()?;
        self.set_health(VectorHealth::Healthy);
        Ok(VectorWriteStats {
            removed: 0,
            inserted,
            skipped_secret,
        })
    }

    fn delete_document(&mut self, chunk_id: &str) -> Result<bool, VectorError> {
        let started = Instant::now();
        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        if chunk_id.is_empty() || chunk_id.len() > 128 || chunk_id.contains('\0') {
            return Err(VectorError::InvalidDocument);
        }
        if !self.is_searchable() {
            return Ok(false);
        }
        let Some(conn) = self.conn.as_mut() else {
            return Ok(false);
        };
        let n = conn.execute(
            "DELETE FROM context_vectors WHERE chunk_id = ?1",
            params![chunk_id],
        )?;
        Ok(n > 0)
    }

    fn search(&self, query: &VectorQuery) -> Result<Vec<VectorHit>, VectorError> {
        let started = Instant::now();
        check_bounds(&query.cancel, started, self.limits.timeout)?;
        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        if !self.is_searchable() {
            return Ok(Vec::new());
        }
        let Some(conn) = self.conn.as_ref() else {
            return Ok(Vec::new());
        };
        let prepared = prepare_query(query, &self.limits)?;
        match scan_hits(conn, query, &prepared, &self.limits, started) {
            Ok(hits) => Ok(hits),
            Err(VectorError::Cancelled) => Err(VectorError::Cancelled),
            Err(VectorError::Timeout) => Err(VectorError::Timeout),
            Err(
                VectorError::InvalidEmbedding | VectorError::InvalidLimit | VectorError::EmptyQuery,
            ) => Err(VectorError::InvalidEmbedding),
            Err(_) => {
                self.set_health(VectorHealth::Degraded(VectorDegradeReason::Corrupt));
                Ok(Vec::new())
            }
        }
    }

    fn is_searchable(&self) -> bool {
        match self.health() {
            VectorHealth::Healthy
            | VectorHealth::Degraded(VectorDegradeReason::ProviderUnavailable) => {
                self.conn.is_some()
            }
            VectorHealth::Disabled
            | VectorHealth::Degraded(VectorDegradeReason::Missing)
            | VectorHealth::Degraded(VectorDegradeReason::Corrupt) => false,
        }
    }

    fn ensure_store(
        &mut self,
        started: Instant,
        replace_corrupt: bool,
    ) -> Result<bool, VectorError> {
        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        if replace_corrupt
            && matches!(
                self.health(),
                VectorHealth::Degraded(VectorDegradeReason::Corrupt)
            )
        {
            self.conn = None;
            if let Some(path) = self.persist.clone() {
                remove_cache_files(&path);
            }
        } else if matches!(
            self.health(),
            VectorHealth::Degraded(VectorDegradeReason::Corrupt)
        ) {
            return Ok(false);
        }
        if self.conn.is_some() {
            return Ok(true);
        }
        match self.persist.clone() {
            Some(path) => {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent)?;
                }
                let conn = Connection::open(&path)?;
                configure_connection(&conn, true)?;
                ensure_schema(&conn)?;
                self.conn = Some(conn);
            }
            None => {
                let conn = Connection::open_in_memory()?;
                configure_connection(&conn, false)?;
                ensure_schema(&conn)?;
                self.conn = Some(conn);
            }
        }
        if !matches!(
            self.health(),
            VectorHealth::Degraded(VectorDegradeReason::ProviderUnavailable)
        ) {
            self.set_health(VectorHealth::Healthy);
        }
        Ok(true)
    }

    fn write_path(
        &mut self,
        document: &VectorDocument,
        planned: &[PlannedRow],
        started: Instant,
    ) -> Result<VectorWriteStats, VectorError> {
        let skipped_secret = count_secret_skips(document).skipped_secret;
        let Some(conn) = self.conn.as_mut() else {
            return Ok(VectorWriteStats {
                skipped_secret,
                ..VectorWriteStats::default()
            });
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        let removed = delete_path(&tx, document, &self.limits, started)?;
        let inserted = insert_rows(&tx, document, planned, &self.limits, started)?;
        ensure_index_budget(&tx, self.limits.max_index_bytes)?;
        tx.commit()?;
        if self.health() != VectorHealth::Degraded(VectorDegradeReason::ProviderUnavailable) {
            self.set_health(VectorHealth::Healthy);
        }
        Ok(VectorWriteStats {
            removed,
            inserted,
            skipped_secret,
        })
    }

    #[cfg(test)]
    fn vector_count(&self) -> Result<u64, VectorError> {
        let Some(conn) = self.conn.as_ref() else {
            return Ok(0);
        };
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM context_vectors", [], |row| row.get(0))?;
        u64::try_from(n).map_err(|_| VectorError::Corrupt("negative vector count"))
    }
}

impl VectorError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::EmptyQuery => "empty_query",
            Self::InvalidLimit => "invalid_limit",
            Self::InvalidDocument => "invalid_document",
            Self::InvalidPolicy => "invalid_policy",
            Self::InvalidEmbedding => "invalid_embedding",
            Self::InvalidVersion => "invalid_version",
            Self::TooManyChunks => "too_many_chunks",
            Self::ChunkTooLarge => "chunk_too_large",
            Self::IndexTooLarge => "index_too_large",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::NotCommitted => "not_committed",
            Self::Corrupt(_) => "corrupt",
            Self::Sqlite(_) => "sqlite",
            Self::Io(_) => "io",
        }
    }
}

impl fmt::Display for VectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt(_) => f.write_str("corrupt vector index"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::Io(_) => f.write_str("io error"),
            other => f.write_str(other.as_str()),
        }
    }
}

impl Error for VectorError {}

impl From<rusqlite::Error> for VectorError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<std::io::Error> for VectorError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl fmt::Debug for VectorIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VectorIndex")
            .field("health", &self.health())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for EnabledVectorIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnabledVectorIndex")
            .field("health", &self.health())
            .finish_non_exhaustive()
    }
}

fn health_code(reason: VectorDegradeReason) -> u8 {
    match reason {
        VectorDegradeReason::Missing => HEALTH_MISSING,
        VectorDegradeReason::Corrupt => HEALTH_CORRUPT,
        VectorDegradeReason::ProviderUnavailable => HEALTH_PROVIDER,
    }
}

fn encode_health(health: VectorHealth) -> u8 {
    match health {
        VectorHealth::Healthy | VectorHealth::Disabled => HEALTH_HEALTHY,
        VectorHealth::Degraded(reason) => health_code(reason),
    }
}

fn decode_health(code: u8) -> VectorHealth {
    match code {
        HEALTH_MISSING => VectorHealth::Degraded(VectorDegradeReason::Missing),
        HEALTH_CORRUPT => VectorHealth::Degraded(VectorDegradeReason::Corrupt),
        HEALTH_PROVIDER => VectorHealth::Degraded(VectorDegradeReason::ProviderUnavailable),
        _ => VectorHealth::Healthy,
    }
}

fn inspect_existing(path: &Path) -> Result<(), VectorError> {
    let header = std::fs::read(path)?;
    if header.len() < SQLITE_MAGIC.len() || !header.starts_with(SQLITE_MAGIC) {
        return Err(VectorError::Corrupt("not a sqlite vector cache"));
    }
    Ok(())
}

fn remove_cache_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    let _ = std::fs::remove_file(&wal);
    let mut shm = path.as_os_str().to_os_string();
    shm.push("-shm");
    let _ = std::fs::remove_file(&shm);
}

fn configure_connection(conn: &Connection, require_wal: bool) -> Result<(), VectorError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if require_wal {
        let journal_mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(VectorError::Corrupt("journal_mode is not wal"));
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
    }
    conn.pragma_update(None, "foreign_keys", 1)?;
    Ok(())
}

fn ensure_schema(conn: &Connection) -> Result<(), VectorError> {
    conn.execute_batch(SCHEMA_META)?;
    conn.execute_batch(SCHEMA_VECTORS)?;
    conn.execute_batch(SCHEMA_PATH_INDEX)?;
    conn.execute(
        "INSERT OR REPLACE INTO context_vector_meta(key, value) VALUES ('kind', ?1)",
        params![SCHEMA_KIND],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO context_vector_meta(key, value) VALUES ('schema', ?1)",
        params![SCHEMA_VERSION],
    )?;
    let kind: String = conn.query_row(
        "SELECT value FROM context_vector_meta WHERE key = 'kind'",
        [],
        |row| row.get(0),
    )?;
    if kind != SCHEMA_KIND {
        return Err(VectorError::Corrupt("unexpected vector schema"));
    }
    Ok(())
}

fn validate_document(document: &VectorDocument, limits: &VectorLimits) -> Result<(), VectorError> {
    if document.chunks.len() > limits.max_chunks {
        return Err(VectorError::TooManyChunks);
    }
    let mut seen = BTreeSet::new();
    for chunk in &document.chunks {
        if chunk.end_byte <= chunk.start_byte {
            return Err(VectorError::InvalidDocument);
        }
        if chunk.text.len() > limits.max_chunk_bytes {
            return Err(VectorError::ChunkTooLarge);
        }
        if chunk.text.contains('\0') {
            return Err(VectorError::InvalidDocument);
        }
        if !seen.insert(chunk.id.to_string()) {
            return Err(VectorError::InvalidDocument);
        }
    }
    Ok(())
}

fn count_secret_skips(document: &VectorDocument) -> VectorWriteStats {
    VectorWriteStats {
        skipped_secret: document
            .chunks
            .iter()
            .filter(|chunk| embedding_text(chunk).is_none())
            .count() as u64,
        ..VectorWriteStats::default()
    }
}

fn plan_embeddings(
    document: &VectorDocument,
    provider: &dyn EmbeddingProvider,
    limits: &VectorLimits,
    started: Instant,
) -> Result<Vec<PlannedRow>, VectorError> {
    check_bounds(&limits.cancel, started, limits.timeout)?;
    if provider.dimensions() == 0 || provider.dimensions() > limits.max_dims {
        return Err(VectorError::InvalidEmbedding);
    }
    let version = provider.version().clone();
    let mut texts = Vec::new();
    let mut owners = Vec::new();
    for chunk in &document.chunks {
        if let Some(text) = embedding_text(chunk) {
            texts.push(text);
            owners.push(chunk.clone());
        }
    }
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let vectors = match provider.embed(&texts, &limits.cancel) {
        Ok(vectors) => vectors,
        Err(VectorError::Cancelled) => return Err(VectorError::Cancelled),
        Err(VectorError::Timeout) => return Err(VectorError::Timeout),
        Err(VectorError::InvalidEmbedding) => return Err(VectorError::InvalidEmbedding),
        Err(_) => return Err(VectorError::ProviderUnavailable),
    };
    if vectors.len() != owners.len() {
        return Err(VectorError::InvalidEmbedding);
    }
    let mut planned = Vec::with_capacity(owners.len());
    for (chunk, raw) in owners.into_iter().zip(vectors) {
        if raw.len() != provider.dimensions() as usize {
            return Err(VectorError::InvalidEmbedding);
        }
        planned.push(PlannedRow {
            chunk,
            version: version.clone(),
            vector: prepare_stored_vector(raw, limits)?,
        });
    }
    Ok(planned)
}

fn prepare_stored_vector(
    raw: Vec<f32>,
    limits: &VectorLimits,
) -> Result<PreparedVector, VectorError> {
    if raw.is_empty() || raw.len() > limits.max_dims as usize {
        return Err(VectorError::InvalidEmbedding);
    }
    let values = l2_normalize(raw)?;
    let blob = encode_vector(&values);
    Ok(PreparedVector { values, blob })
}

fn prepare_query(query: &VectorQuery, limits: &VectorLimits) -> Result<Vec<f32>, VectorError> {
    if query.limit == 0 {
        return Err(VectorError::InvalidLimit);
    }
    if query.vector.is_empty() {
        return Err(VectorError::EmptyQuery);
    }
    if query.vector.len() > limits.max_dims as usize {
        return Err(VectorError::InvalidEmbedding);
    }
    l2_normalize(query.vector.clone())
}

fn l2_normalize(raw: Vec<f32>) -> Result<Vec<f32>, VectorError> {
    if raw.iter().any(|value| !value.is_finite()) {
        return Err(VectorError::InvalidEmbedding);
    }
    let mut sum = 0.0f64;
    for value in &raw {
        sum += f64::from(*value) * f64::from(*value);
    }
    let norm = sum.sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return Err(VectorError::InvalidEmbedding);
    }
    Ok(raw
        .into_iter()
        .map(|value| (f64::from(value) / norm) as f32)
        .collect())
}

fn encode_vector(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len().saturating_mul(4));
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn decode_vector(blob: &[u8], dims: usize) -> Result<Vec<f32>, VectorError> {
    if dims == 0 || blob.len() != dims.saturating_mul(4) {
        return Err(VectorError::Corrupt("vector blob size"));
    }
    let mut values = Vec::with_capacity(dims);
    for chunk in blob.chunks_exact(4) {
        let bytes = [chunk[0], chunk[1], chunk[2], chunk[3]];
        let value = f32::from_le_bytes(bytes);
        if !value.is_finite() {
            return Err(VectorError::Corrupt("non-finite vector"));
        }
        values.push(value);
    }
    Ok(values)
}

fn cosine(query: &[f32], candidate: &[f32]) -> f32 {
    if query.len() != candidate.len() {
        return 0.0;
    }
    let mut sum = 0.0f64;
    for (left, right) in query.iter().zip(candidate) {
        sum += f64::from(*left) * f64::from(*right);
    }
    sum as f32
}

fn delete_path(
    tx: &Transaction<'_>,
    document: &VectorDocument,
    limits: &VectorLimits,
    started: Instant,
) -> Result<u64, VectorError> {
    check_bounds(&limits.cancel, started, limits.timeout)?;
    let n = tx.execute(
        "DELETE FROM context_vectors WHERE repo_id = ?1 AND path = ?2",
        params![document.repo_id.to_string(), document.path.as_str()],
    )?;
    Ok(n as u64)
}

fn insert_rows(
    tx: &Transaction<'_>,
    document: &VectorDocument,
    planned: &[PlannedRow],
    limits: &VectorLimits,
    started: Instant,
) -> Result<u64, VectorError> {
    let mut inserted = 0u64;
    let language = document.language.map(SourceLanguage::as_str);
    for (step, row) in planned.iter().enumerate() {
        if step.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(&limits.cancel, started, limits.timeout)?;
        }
        tx.execute(
            "INSERT INTO context_vectors(
                chunk_id, repo_id, path, language, start_byte, end_byte,
                content_hash, embedding_version, dims, vector
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                row.chunk.id.to_string(),
                document.repo_id.to_string(),
                document.path.as_str(),
                language,
                i64::from(row.chunk.start_byte),
                i64::from(row.chunk.end_byte),
                row.chunk.content_hash.to_string(),
                row.version.as_str(),
                row.vector.values.len() as i64,
                row.vector.blob.as_slice(),
            ],
        )?;
        inserted = inserted.saturating_add(1);
    }
    Ok(inserted)
}

fn scan_hits(
    conn: &Connection,
    query: &VectorQuery,
    prepared: &[f32],
    limits: &VectorLimits,
    started: Instant,
) -> Result<Vec<VectorHit>, VectorError> {
    let repo = query.repo_id.map(|id| id.to_string());
    let path = query.path.as_ref().map(|p| p.as_str().to_string());
    let path_like = query.path.as_ref().map(|p| like_prefix(p.as_str()));
    let language = query.language.map(SourceLanguage::as_str);
    let version = query.version.as_ref().map(EmbeddingVersion::as_str);
    let limit = query.limit.min(limits.max_results);

    let mut stmt = conn.prepare(
        "SELECT chunk_id, repo_id, path, language, start_byte, end_byte,
                content_hash, embedding_version, dims, vector
         FROM context_vectors
         WHERE (?1 IS NULL OR repo_id = ?1)
           AND (?2 IS NULL OR path = ?2 OR path LIKE ?3 ESCAPE '\\')
           AND (?4 IS NULL OR language = ?4)
           AND (?5 IS NULL OR embedding_version = ?5)",
    )?;
    let mut rows = stmt.query(params![repo, path, path_like, language, version])?;
    let mut hits = Vec::new();
    let mut steps = 0usize;
    while let Some(row) = rows.next()? {
        steps = steps.saturating_add(1);
        if steps.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(&query.cancel, started, limits.timeout)?;
            check_bounds(&limits.cancel, started, limits.timeout)?;
        }
        let dims = row.get::<_, i64>(8)?;
        let dims = usize::try_from(dims).map_err(|_| VectorError::Corrupt("dims out of range"))?;
        if dims != prepared.len() {
            continue;
        }
        let blob: Vec<u8> = row.get(9)?;
        let candidate = decode_vector(&blob, dims)?;
        let score = cosine(prepared, &candidate);
        hits.push(hit_from_row(row, score)?);
    }
    hits.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.chunk_id.cmp(&right.chunk_id))
    });
    hits.truncate(limit as usize);
    check_bounds(&query.cancel, started, limits.timeout)?;
    check_bounds(&limits.cancel, started, limits.timeout)?;
    Ok(hits)
}

fn hit_from_row(row: &rusqlite::Row<'_>, score: f32) -> Result<VectorHit, VectorError> {
    let chunk_id: String = row.get(0)?;
    let repo_raw: String = row.get(1)?;
    let path_raw: String = row.get(2)?;
    let language_raw: Option<String> = row.get(3)?;
    let start_byte: i64 = row.get(4)?;
    let end_byte: i64 = row.get(5)?;
    let content_hash: String = row.get(6)?;
    let version_raw: String = row.get(7)?;
    let repo_id =
        RepoId::from_str(&repo_raw).map_err(|_| VectorError::Corrupt("malformed repo_id"))?;
    let path = RepoPath::parse(&path_raw).map_err(|_| VectorError::Corrupt("malformed path"))?;
    let language = parse_language(language_raw.as_deref())?;
    let start_byte =
        u32::try_from(start_byte).map_err(|_| VectorError::Corrupt("start_byte out of range"))?;
    let end_byte =
        u32::try_from(end_byte).map_err(|_| VectorError::Corrupt("end_byte out of range"))?;
    if chunk_id.is_empty() || content_hash.is_empty() {
        return Err(VectorError::Corrupt("empty chunk identity"));
    }
    Ok(VectorHit {
        chunk_id,
        repo_id,
        path,
        language,
        start_byte,
        end_byte,
        content_hash,
        embedding_version: EmbeddingVersion::new(version_raw)
            .map_err(|_| VectorError::Corrupt("malformed embedding version"))?,
        score,
    })
}

fn parse_language(raw: Option<&str>) -> Result<Option<SourceLanguage>, VectorError> {
    match raw {
        None => Ok(None),
        Some(name) => SourceLanguage::ALL
            .iter()
            .copied()
            .find(|lang| lang.as_str() == name)
            .map(Some)
            .ok_or(VectorError::Corrupt("unknown language")),
    }
}

fn like_prefix(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 2);
    for ch in path.chars() {
        match ch {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.push('/');
    out.push('%');
    out
}

fn ensure_index_budget(tx: &Transaction<'_>, max_index_bytes: u64) -> Result<(), VectorError> {
    let page_count: i64 = tx.pragma_query_value(None, "page_count", |row| row.get(0))?;
    let page_size: i64 = tx.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let pages = u64::try_from(page_count).unwrap_or(u64::MAX);
    let size = u64::try_from(page_size).unwrap_or(u64::MAX);
    let used = pages.saturating_mul(size);
    if used > max_index_bytes {
        return Err(VectorError::IndexTooLarge);
    }
    Ok(())
}

fn check_bounds(
    cancel: &CancellationToken,
    started: Instant,
    timeout: Duration,
) -> Result<(), VectorError> {
    if cancel.is_cancelled() {
        return Err(VectorError::Cancelled);
    }
    if timeout.is_zero() || started.elapsed() > timeout {
        return Err(VectorError::Timeout);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    use crate::chunk::{ChunkPolicy, Document, chunk};

    struct HashEmbeddingProvider {
        version: EmbeddingVersion,
        dims: u32,
        fail: bool,
        seen: Mutex<Vec<String>>,
    }

    impl HashEmbeddingProvider {
        fn new(dims: u32) -> Self {
            Self {
                version: EmbeddingVersion::new("hash-v1").expect("version"),
                dims,
                fail: false,
                seen: Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            Self {
                fail: true,
                ..Self::new(8)
            }
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().expect("seen lock").clone()
        }
    }

    impl EmbeddingProvider for HashEmbeddingProvider {
        fn version(&self) -> &EmbeddingVersion {
            &self.version
        }

        fn dimensions(&self) -> u32 {
            self.dims
        }

        fn embed(
            &self,
            texts: &[&str],
            cancel: &CancellationToken,
        ) -> Result<Vec<Vec<f32>>, VectorError> {
            if cancel.is_cancelled() {
                return Err(VectorError::Cancelled);
            }
            if self.fail {
                return Err(VectorError::ProviderUnavailable);
            }
            let mut out = Vec::with_capacity(texts.len());
            let mut seen = self.seen.lock().expect("seen lock");
            for text in texts {
                seen.push((*text).to_string());
                out.push(hash_vector(text, self.dims as usize));
            }
            Ok(out)
        }
    }

    fn hash_vector(text: &str, dims: usize) -> Vec<f32> {
        let digest = ContentHash::from_bytes(text.as_bytes());
        let bytes = digest.as_digest();
        let mut values = vec![0.0f32; dims];
        for (i, slot) in values.iter_mut().enumerate() {
            *slot = f32::from(bytes[i % bytes.len()]) - 127.5;
        }
        values
    }

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn records(rel: &str, language: SourceLanguage, src: &str) -> (Document, Vec<ChunkRecord>) {
        let document = Document::new(RepoId::new(), path(rel), Some(language), src);
        let policy = ChunkPolicy::new().overlap_bytes(0).overlap_tokens(0);
        let chunks = chunk(&document, &[], &policy).expect("chunk");
        assert!(!chunks.is_empty());
        (document, chunks)
    }

    fn document_from(
        src: &str,
        rel: &str,
        language: SourceLanguage,
        redaction: RedactionClass,
    ) -> VectorDocument {
        let (_doc, chunks) = records(rel, language, src);
        VectorDocument::from_records(&chunks, redaction).expect("vector document")
    }

    fn open_mem() -> VectorIndex {
        VectorIndex::open_in_memory(VectorLimits::new()).expect("open memory vector")
    }

    fn temp_path() -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        std::env::temp_dir().join(format!(
            "rapidlm-vector-{}-{seq}.sqlite",
            std::process::id()
        ))
    }

    fn query_for(text: &str, provider: &HashEmbeddingProvider) -> VectorQuery {
        VectorQuery::from_text(text, provider, &CancellationToken::new()).expect("query")
    }

    fn vector_count(index: &VectorIndex) -> u64 {
        match index {
            VectorIndex::Disabled => 0,
            VectorIndex::Enabled(inner) => inner.vector_count().expect("count"),
        }
    }

    #[test]
    fn disabled_keeps_search_api_and_skips_provider() {
        let provider = HashEmbeddingProvider::new(8);
        let mut index = VectorIndex::disabled();
        assert!(index.is_disabled());
        assert_eq!(index.health(), VectorHealth::Disabled);
        let doc = document_from(
            "fn unused() {}\n",
            "src/lib.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        let stats = index.upsert_document(&doc, &provider).expect("upsert");
        assert_eq!(stats.inserted(), 0);
        let hits = index
            .search(&VectorQuery::new(vec![1.0, 0.0, 0.0, 0.0]))
            .expect("search");
        assert!(hits.is_empty());
        assert!(provider.seen().is_empty());
        assert!(matches!(index, VectorIndex::Disabled));
    }

    #[test]
    fn embeddings_mode_off_is_disabled() {
        let index = VectorIndex::from_mode(EmbeddingsMode::Off, temp_path(), VectorLimits::new())
            .expect("from mode");
        assert!(index.is_disabled());
        assert_eq!(index.health(), VectorHealth::Disabled);
    }

    #[test]
    fn embeddings_mode_auto_opens_rebuildable_cache() {
        let path = temp_path();
        let provider = HashEmbeddingProvider::new(8);
        let doc = document_from(
            "fn auto_mode_token() {}\n",
            "src/lib.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        {
            let mut index =
                VectorIndex::from_mode(EmbeddingsMode::Auto, &path, VectorLimits::new())
                    .expect("from mode auto");
            assert!(!index.is_disabled());
            assert_eq!(
                index.health(),
                VectorHealth::Degraded(VectorDegradeReason::Missing)
            );
            index.upsert_document(&doc, &provider).expect("upsert");
            assert_eq!(index.health(), VectorHealth::Healthy);
        }
        let index = VectorIndex::from_mode(EmbeddingsMode::Auto, &path, VectorLimits::new())
            .expect("reopen auto");
        assert!(!index.is_disabled());
        assert_eq!(index.health(), VectorHealth::Healthy);
        let hits = index
            .search(&query_for(doc.chunks()[0].text(), &provider))
            .expect("search");
        assert_eq!(hits.len(), 1);
        remove_cache_files(&path);
    }

    #[test]
    fn dim_mismatch_skips_hits_without_failing_search() {
        let provider = HashEmbeddingProvider::new(8);
        let mut index = open_mem();
        let doc = document_from(
            "fn dim_mismatch_token() {}\n",
            "src/lib.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        index.upsert_document(&doc, &provider).expect("upsert");
        let hits = index
            .search(&VectorQuery::new(vec![1.0, 0.0, 0.0, 0.0]))
            .expect("search");
        assert!(hits.is_empty());
        assert_eq!(index.health(), VectorHealth::Healthy);
        let matched = index
            .search(&query_for(doc.chunks()[0].text(), &provider))
            .expect("matched dims");
        assert_eq!(matched.len(), 1);
    }

    #[test]
    fn version_filter_excludes_other_embedding_versions() {
        let provider = HashEmbeddingProvider::new(8);
        let mut index = open_mem();
        let doc = document_from(
            "fn version_filter_token() {}\n",
            "src/lib.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        index.upsert_document(&doc, &provider).expect("upsert");
        let other = EmbeddingVersion::new("hash-v2").expect("version");
        let misses = index
            .search(&query_for(doc.chunks()[0].text(), &provider).version(other))
            .expect("other version");
        assert!(misses.is_empty());
        let hits = index
            .search(
                &query_for(doc.chunks()[0].text(), &provider).version(provider.version().clone()),
            )
            .expect("same version");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn provider_dimension_mismatch_is_invalid_embedding() {
        struct MismatchProvider {
            version: EmbeddingVersion,
        }
        impl EmbeddingProvider for MismatchProvider {
            fn version(&self) -> &EmbeddingVersion {
                &self.version
            }
            fn dimensions(&self) -> u32 {
                8
            }
            fn embed(
                &self,
                texts: &[&str],
                cancel: &CancellationToken,
            ) -> Result<Vec<Vec<f32>>, VectorError> {
                if cancel.is_cancelled() {
                    return Err(VectorError::Cancelled);
                }
                Ok(texts.iter().map(|_| vec![1.0f32, 0.0]).collect())
            }
        }

        let mut index = open_mem();
        let doc = document_from(
            "fn bad_dims() {}\n",
            "src/lib.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        let err = index
            .upsert_document(
                &doc,
                &MismatchProvider {
                    version: EmbeddingVersion::new("hash-v1").expect("version"),
                },
            )
            .expect_err("dim mismatch");
        assert!(matches!(err, VectorError::InvalidEmbedding));
        assert_eq!(vector_count(&index), 0);
        assert_eq!(index.health(), VectorHealth::Healthy);
    }

    #[test]
    fn secret_sources_are_excluded_from_embedding_text() {
        let provider = HashEmbeddingProvider::new(8);
        let mut index = open_mem();
        let secret = document_from(
            "const API_TOKEN: &str = \"not-a-real-secret\";\n",
            "src/secrets.rs",
            SourceLanguage::Rust,
            RedactionClass::Secret,
        );
        assert!(embedding_text(&secret.chunks()[0]).is_none());
        let stats = index.upsert_document(&secret, &provider).expect("upsert");
        assert_eq!(stats.inserted(), 0);
        assert_eq!(stats.skipped_secret(), 1);
        assert!(provider.seen().is_empty());
        assert_eq!(vector_count(&index), 0);

        let shown = format!("{stats:?}");
        assert!(!shown.contains("not-a-real-secret"));
        let hits = index.search(&query_for("API_TOKEN", &provider)).expect("q");
        assert!(hits.is_empty());
    }

    #[test]
    fn public_chunk_is_indexed_and_keyed_by_chunk_id() {
        let provider = HashEmbeddingProvider::new(8);
        let mut index = open_mem();
        let doc = document_from(
            "fn unique_alpha() { 1 }\n",
            "src/lib.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        let chunk_id = doc.chunks()[0].id().to_string();
        let stats = index.upsert_document(&doc, &provider).expect("upsert");
        assert_eq!(stats.inserted(), 1);
        assert_eq!(index.health(), VectorHealth::Healthy);
        let hits = index
            .search(&query_for(doc.chunks()[0].text(), &provider).repo(doc.repo_id()))
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id(), chunk_id);
        assert_eq!(hits[0].path().as_str(), "src/lib.rs");
    }

    #[test]
    fn reindex_replaces_vectors_for_the_same_path() {
        let provider = HashEmbeddingProvider::new(8);
        let mut index = open_mem();
        let first = document_from(
            "fn before_token() {}\n",
            "src/lib.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        let first_id = first.chunks()[0].id().to_string();
        index.upsert_document(&first, &provider).expect("first");

        let mut second = document_from(
            "fn after_token() {}\n",
            "src/lib.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        second.repo_id = first.repo_id();
        let stats = index.upsert_document(&second, &provider).expect("second");
        assert_eq!(stats.removed(), 1);
        assert_eq!(stats.inserted(), 1);
        assert_eq!(vector_count(&index), 1);

        let after = index
            .search(&query_for(second.chunks()[0].text(), &provider))
            .expect("new");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].chunk_id(), second.chunks()[0].id().to_string());
        assert_ne!(after[0].chunk_id(), first_id);
        assert!(after[0].score() > 0.99);
    }

    #[test]
    fn nearest_neighbor_orders_identical_text_first() {
        let provider = HashEmbeddingProvider::new(8);
        let mut index = open_mem();
        let match_doc = document_from(
            "alpha_unique_body\n",
            "src/a.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        let other = document_from(
            "beta_other_body\n",
            "src/b.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        index.upsert_document(&match_doc, &provider).expect("a");
        index.upsert_document(&other, &provider).expect("b");
        let hits = index
            .search(&query_for("alpha_unique_body\n", &provider).limit(2))
            .expect("search");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path().as_str(), "src/a.rs");
        assert!(hits[0].score() >= hits[1].score());
    }

    #[test]
    fn missing_index_is_degraded_and_search_is_empty() {
        let path = temp_path();
        let index = VectorIndex::open(&path, VectorLimits::new()).expect("open missing");
        assert_eq!(
            index.health(),
            VectorHealth::Degraded(VectorDegradeReason::Missing)
        );
        let provider = HashEmbeddingProvider::new(8);
        let hits = index
            .search(&query_for("anything", &provider))
            .expect("search");
        assert!(hits.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn corrupt_index_is_degraded_not_a_search_error() {
        let path = temp_path();
        std::fs::write(&path, b"not-a-vector-index").expect("write garbage");
        let index = VectorIndex::open(&path, VectorLimits::new()).expect("open corrupt");
        assert_eq!(
            index.health(),
            VectorHealth::Degraded(VectorDegradeReason::Corrupt)
        );
        let provider = HashEmbeddingProvider::new(8);
        let hits = index
            .search(&query_for("anything", &provider))
            .expect("search");
        assert!(hits.is_empty());
        let shown = format!("{index:?}");
        assert!(!shown.contains("not-a-vector-index"));
        assert!(!shown.contains(path.to_string_lossy().as_ref()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rebuild_recovers_a_corrupt_index() {
        let path = temp_path();
        std::fs::write(&path, b"broken").expect("write garbage");
        let mut index = VectorIndex::open(&path, VectorLimits::new()).expect("open");
        let provider = HashEmbeddingProvider::new(8);
        let doc = document_from(
            "fn recovered() {}\n",
            "src/lib.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        let stats = index
            .rebuild(std::slice::from_ref(&doc), &provider)
            .expect("rebuild");
        assert_eq!(stats.inserted(), 1);
        assert_eq!(index.health(), VectorHealth::Healthy);
        let hits = index
            .search(&query_for(doc.chunks()[0].text(), &provider))
            .expect("search");
        assert_eq!(hits.len(), 1);
        let _ = std::fs::remove_file(&path);
        remove_cache_files(&path);
    }

    #[test]
    fn provider_unavailable_degrades_without_failing_search() {
        let mut index = open_mem();
        let good = HashEmbeddingProvider::new(8);
        let doc = document_from(
            "fn keep_me() {}\n",
            "src/lib.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        index.upsert_document(&doc, &good).expect("seed");
        let failing = HashEmbeddingProvider::failing();
        let next = document_from(
            "fn skip_me() {}\n",
            "src/other.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        let stats = index.upsert_document(&next, &failing).expect("degrade");
        assert_eq!(stats.inserted(), 0);
        assert_eq!(
            index.health(),
            VectorHealth::Degraded(VectorDegradeReason::ProviderUnavailable)
        );
        let hits = index
            .search(&query_for(doc.chunks()[0].text(), &good))
            .expect("existing still searchable");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn cancellation_and_timeout_are_typed() {
        let provider = HashEmbeddingProvider::new(8);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut index =
            VectorIndex::open_in_memory(VectorLimits::new().cancellation(cancel)).expect("open");
        let doc = document_from(
            "fn cancel_me() {}\n",
            "src/lib.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        assert!(matches!(
            index.upsert_document(&doc, &provider).expect_err("cancel"),
            VectorError::Cancelled
        ));

        let mut timed = VectorIndex::open_in_memory(VectorLimits::new().timeout(Duration::ZERO))
            .expect("open timed");
        assert!(matches!(
            timed.upsert_document(&doc, &provider).expect_err("timeout"),
            VectorError::Timeout
        ));
        assert!(matches!(
            timed
                .search(&query_for("fn cancel_me() {}\n", &provider))
                .expect_err("search timeout"),
            VectorError::Timeout
        ));
    }

    #[test]
    fn file_backed_index_survives_reopen() {
        let path = temp_path();
        let provider = HashEmbeddingProvider::new(8);
        let doc = document_from(
            "fn persist_token() {}\n",
            "src/lib.rs",
            SourceLanguage::Rust,
            RedactionClass::Public,
        );
        let chunk_id = doc.chunks()[0].id().to_string();
        {
            let mut index = VectorIndex::open(&path, VectorLimits::new()).expect("open");
            assert_eq!(
                index.health(),
                VectorHealth::Degraded(VectorDegradeReason::Missing)
            );
            index.upsert_document(&doc, &provider).expect("upsert");
            assert_eq!(index.health(), VectorHealth::Healthy);
        }
        let index = VectorIndex::open(&path, VectorLimits::new()).expect("reopen");
        assert_eq!(index.health(), VectorHealth::Healthy);
        let hits = index
            .search(&query_for(doc.chunks()[0].text(), &provider))
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id(), chunk_id);
        remove_cache_files(&path);
    }

    #[test]
    fn search_limit_is_capped() {
        let provider = HashEmbeddingProvider::new(8);
        let mut index =
            VectorIndex::open_in_memory(VectorLimits::new().max_results(2)).expect("open");
        for i in 0..4 {
            let src = format!("stable_token body_{i}\n");
            let doc = document_from(
                &src,
                &format!("src/f{i}.rs"),
                SourceLanguage::Rust,
                RedactionClass::Public,
            );
            index.upsert_document(&doc, &provider).expect("upsert");
        }
        let hits = index
            .search(&query_for("stable_token body_0\n", &provider).limit(10_000))
            .expect("search");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn error_display_is_safe() {
        let err = VectorError::Corrupt("vector blob size");
        assert_eq!(err.to_string(), "corrupt vector index");
        assert_eq!(err.as_str(), "corrupt");
        assert!(!err.to_string().contains("secret"));
        assert!(!VectorError::ProviderUnavailable.to_string().contains('/'));
    }
}
