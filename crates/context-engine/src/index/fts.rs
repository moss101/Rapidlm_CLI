//! SQLite FTS5 lexical index keyed by chunk ID.
//!
//! Chunks and symbol terms are persisted in the normative `context_chunks` /
//! `context_fts` schema. Reindex of one source path replaces prior rows in a
//! single IMMEDIATE transaction. Search is BM25 with repo/path/language filters
//! and a hard result cap.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use protocol::{DEFAULT_MAX_INDEX_BYTES, RepoId, RepoPath};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::chunk::{ChunkId, ChunkRecord, DEFAULT_MAX_CHUNK_BYTES, DEFAULT_MAX_CHUNKS};
use crate::ingest::content::{ContentHash, SourceLanguage};
use crate::parse::symbols::SymbolRecord;
use crate::repo_manifest::CancellationToken;

/// Default wall-clock budget for one writer or search call.
pub const DEFAULT_FTS_TIMEOUT: Duration = Duration::from_secs(5);

/// Default UTF-8 byte cap for a raw search string.
pub const DEFAULT_MAX_QUERY_BYTES: usize = 4_096;

/// Default maximum tokens compiled into one FTS5 MATCH.
pub const DEFAULT_MAX_QUERY_TERMS: usize = 32;

/// Default caller-facing search page size.
pub const DEFAULT_SEARCH_LIMIT: u32 = 20;

/// Hard default cap applied even when a caller asks for more hits.
pub const DEFAULT_MAX_FTS_RESULTS: u32 = 256;

/// Bounded SQLite lock wait. Matches the ledger busy-timeout recovery rule.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

const SCHEMA_CHUNKS: &str = "
CREATE TABLE IF NOT EXISTS context_chunks (
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
";

const SCHEMA_FTS: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS context_fts USING fts5(
  chunk_id UNINDEXED,
  path,
  symbols,
  text,
  tokenize='unicode61'
);
";

const SCHEMA_PATH_INDEX: &str = "
CREATE INDEX IF NOT EXISTS context_chunks_repo_path
  ON context_chunks(repo_id, path);
";

const CANCEL_STRIDE: usize = 16;

/// Per-index resource bounds. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct FtsLimits {
    timeout: Duration,
    max_query_bytes: usize,
    max_query_terms: usize,
    max_results: u32,
    max_chunks: usize,
    max_chunk_bytes: usize,
    max_index_bytes: u64,
    cancel: CancellationToken,
}

/// One source path being written. Empty `chunks` means “remove this path”.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FtsDocument {
    repo_id: RepoId,
    path: RepoPath,
    language: Option<SourceLanguage>,
    chunks: Vec<FtsChunk>,
}

/// One FTS document, keyed by [`ChunkId`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FtsChunk {
    id: ChunkId,
    start_byte: u32,
    end_byte: u32,
    content_hash: ContentHash,
    text: String,
    symbols: Vec<String>,
}

/// Lexical query with optional structural filters.
#[derive(Clone, Debug)]
pub struct FtsQuery {
    text: String,
    repo_id: Option<RepoId>,
    path: Option<RepoPath>,
    language: Option<SourceLanguage>,
    limit: u32,
    cancel: CancellationToken,
}

/// One BM25 hit. `chunk_id` is the stored chunk-ID wire form.
#[derive(Clone, Debug, PartialEq)]
pub struct FtsHit {
    chunk_id: String,
    repo_id: RepoId,
    path: RepoPath,
    language: Option<SourceLanguage>,
    start_byte: u32,
    end_byte: u32,
    content_hash: String,
    score: f64,
}

/// Rows removed and inserted by one transactional write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FtsWriteStats {
    removed: u64,
    inserted: u64,
}

/// File-backed or in-memory FTS5 index.
pub struct FtsIndex {
    conn: Connection,
    limits: FtsLimits,
    fail_before_commit: AtomicBool,
}

/// Typed FTS failure. Display never echoes source, queries, or host paths.
#[derive(Debug)]
pub enum FtsError {
    Cancelled,
    Timeout,
    EmptyQuery,
    QueryTooLarge,
    InvalidLimit,
    InvalidDocument,
    InvalidPolicy,
    TooManyChunks,
    ChunkTooLarge,
    IndexTooLarge,
    /// Write ran but the transaction did not commit; prior rows are intact.
    NotCommitted,
    Corrupt(&'static str),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
}

struct PreparedQuery {
    match_sql: String,
    limit: u32,
}

impl FtsLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn max_query_bytes(mut self, value: usize) -> Self {
        self.max_query_bytes = value;
        self
    }

    pub fn max_query_terms(mut self, value: usize) -> Self {
        self.max_query_terms = value;
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

    pub fn max_query_bytes_value(&self) -> usize {
        self.max_query_bytes
    }

    pub fn max_query_terms_value(&self) -> usize {
        self.max_query_terms
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

    pub fn max_index_bytes_value(&self) -> u64 {
        self.max_index_bytes
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }

    fn validate(&self) -> Result<(), FtsError> {
        if self.max_query_bytes == 0
            || self.max_query_terms == 0
            || self.max_results == 0
            || self.max_chunks == 0
            || self.max_chunk_bytes == 0
        {
            return Err(FtsError::InvalidPolicy);
        }
        Ok(())
    }
}

impl Default for FtsLimits {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_FTS_TIMEOUT,
            max_query_bytes: DEFAULT_MAX_QUERY_BYTES,
            max_query_terms: DEFAULT_MAX_QUERY_TERMS,
            max_results: DEFAULT_MAX_FTS_RESULTS,
            max_chunks: DEFAULT_MAX_CHUNKS,
            max_chunk_bytes: DEFAULT_MAX_CHUNK_BYTES,
            max_index_bytes: DEFAULT_MAX_INDEX_BYTES,
            cancel: CancellationToken::new(),
        }
    }
}

impl FtsDocument {
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
        symbols: &[SymbolRecord],
    ) -> Result<Self, FtsError> {
        let Some(first) = records.first() else {
            return Err(FtsError::InvalidDocument);
        };
        let mut document = Self::new(first.repo_id(), first.path().clone(), first.language());
        document.push_records(records, symbols)?;
        Ok(document)
    }

    pub fn push_records(
        &mut self,
        records: &[ChunkRecord],
        symbols: &[SymbolRecord],
    ) -> Result<(), FtsError> {
        for record in records {
            if record.repo_id() != self.repo_id || record.path() != &self.path {
                return Err(FtsError::InvalidDocument);
            }
            self.chunks
                .push(FtsChunk::from_record(record).with_overlapping_symbols(record, symbols));
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

    pub fn chunks(&self) -> &[FtsChunk] {
        &self.chunks
    }
}

impl FtsChunk {
    pub fn from_record(record: &ChunkRecord) -> Self {
        let mut symbols = Vec::new();
        if let Some(locator) = record.symbol_id() {
            push_symbol_term(&mut symbols, locator);
        }
        Self {
            id: record.id(),
            start_byte: record.start_byte(),
            end_byte: record.end_byte(),
            content_hash: record.content_hash(),
            text: record.text().to_string(),
            symbols,
        }
    }

    fn with_overlapping_symbols(mut self, record: &ChunkRecord, symbols: &[SymbolRecord]) -> Self {
        for symbol in symbols {
            let range = symbol.range();
            if range.end_byte() <= record.start_byte() || range.start_byte() >= record.end_byte() {
                continue;
            }
            push_symbol_term(&mut self.symbols, symbol.name());
            push_symbol_term(&mut self.symbols, symbol.fq_name());
            push_symbol_term(&mut self.symbols, symbol.locator());
        }
        self
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

    pub fn symbols(&self) -> &[String] {
        &self.symbols
    }
}

impl FtsQuery {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            repo_id: None,
            path: None,
            language: None,
            limit: DEFAULT_SEARCH_LIMIT,
            cancel: CancellationToken::new(),
        }
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

    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    pub fn cancellation(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn text(&self) -> &str {
        &self.text
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

    pub fn limit_value(&self) -> u32 {
        self.limit
    }
}

impl FtsHit {
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

    pub fn score(&self) -> f64 {
        self.score
    }
}

impl FtsWriteStats {
    pub fn removed(self) -> u64 {
        self.removed
    }

    pub fn inserted(self) -> u64 {
        self.inserted
    }
}

impl FtsIndex {
    /// Open (or create) a file-backed index and ensure the FTS schema exists.
    pub fn open(path: impl AsRef<Path>, limits: FtsLimits) -> Result<Self, FtsError> {
        limits.validate()?;
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        configure_connection(&conn, true)?;
        ensure_schema(&conn)?;
        Ok(Self {
            conn,
            limits,
            fail_before_commit: AtomicBool::new(false),
        })
    }

    /// Open a process-private in-memory index. Rebuildable; not durable.
    pub fn open_in_memory(limits: FtsLimits) -> Result<Self, FtsError> {
        limits.validate()?;
        let conn = Connection::open_in_memory()?;
        configure_connection(&conn, false)?;
        ensure_schema(&conn)?;
        Ok(Self {
            conn,
            limits,
            fail_before_commit: AtomicBool::new(false),
        })
    }

    /// Replace every chunk for `document.path` in one transaction.
    pub fn upsert_document(&mut self, document: &FtsDocument) -> Result<FtsWriteStats, FtsError> {
        let started = Instant::now();
        let Self {
            conn,
            limits,
            fail_before_commit,
        } = self;
        check_bounds(&limits.cancel, started, limits.timeout)?;
        validate_document(document, limits)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_bounds(&limits.cancel, started, limits.timeout)?;
        let stats = replace_path(&tx, document, limits, started)?;
        ensure_index_budget(&tx, limits.max_index_bytes)?;
        check_bounds(&limits.cancel, started, limits.timeout)?;
        if fail_before_commit.swap(false, Ordering::SeqCst) {
            return Err(FtsError::NotCommitted);
        }
        tx.commit()?;
        Ok(stats)
    }

    /// Remove one FTS document by chunk-ID wire form.
    pub fn delete_document(&mut self, chunk_id: &str) -> Result<bool, FtsError> {
        let started = Instant::now();
        let Self {
            conn,
            limits,
            fail_before_commit,
        } = self;
        check_bounds(&limits.cancel, started, limits.timeout)?;
        if chunk_id.is_empty() || chunk_id.len() > 128 || chunk_id.contains('\0') {
            return Err(FtsError::InvalidDocument);
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_bounds(&limits.cancel, started, limits.timeout)?;
        let fts_n = tx.execute(
            "DELETE FROM context_fts WHERE chunk_id = ?1",
            params![chunk_id],
        )?;
        let chunk_n = tx.execute(
            "DELETE FROM context_chunks WHERE id = ?1",
            params![chunk_id],
        )?;
        check_bounds(&limits.cancel, started, limits.timeout)?;
        if fail_before_commit.swap(false, Ordering::SeqCst) {
            return Err(FtsError::NotCommitted);
        }
        tx.commit()?;
        Ok(fts_n > 0 || chunk_n > 0)
    }

    /// BM25 search. Limit is clamped to the index hard cap and is stable.
    pub fn search(&self, query: &FtsQuery) -> Result<Vec<FtsHit>, FtsError> {
        let started = Instant::now();
        self.check_query(query, started)?;
        let prepared = compile_query(query, &self.limits)?;
        let repo = query.repo_id.map(|id| id.to_string());
        let path = query.path.as_ref().map(|p| p.as_str().to_string());
        let path_like = query.path.as_ref().map(|p| like_prefix(p.as_str()));
        let language = query.language.map(|l| l.as_str());

        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.repo_id, c.path, c.language, c.start_byte, c.end_byte,
                    c.content_hash, bm25(context_fts) AS score
             FROM context_fts
             INNER JOIN context_chunks AS c ON c.id = context_fts.chunk_id
             WHERE context_fts MATCH ?1
               AND (?2 IS NULL OR c.repo_id = ?2)
               AND (?3 IS NULL OR c.path = ?3 OR c.path LIKE ?4 ESCAPE '\\')
               AND (?5 IS NULL OR c.language = ?5)
             ORDER BY score ASC, c.id ASC
             LIMIT ?6",
        )?;
        let mut rows = stmt.query(params![
            prepared.match_sql,
            repo,
            path,
            path_like,
            language,
            prepared.limit as i64,
        ])?;

        let mut hits = Vec::new();
        let mut steps = 0usize;
        while let Some(row) = rows.next()? {
            steps = steps.saturating_add(1);
            if steps.is_multiple_of(CANCEL_STRIDE) {
                self.check_query(query, started)?;
            }
            hits.push(hit_from_row(row)?);
            if hits.len() as u32 >= prepared.limit {
                break;
            }
        }
        self.check_query(query, started)?;
        Ok(hits)
    }

    fn check_query(&self, query: &FtsQuery, started: Instant) -> Result<(), FtsError> {
        check_bounds(&query.cancel, started, self.limits.timeout)?;
        check_bounds(&self.limits.cancel, started, self.limits.timeout)
    }

    #[cfg(test)]
    fn fail_next_commit(&self) {
        self.fail_before_commit.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn chunk_count(&self) -> Result<u64, FtsError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM context_chunks", [], |row| row.get(0))?;
        u64::try_from(n).map_err(|_| FtsError::Corrupt("negative chunk count"))
    }

    #[cfg(test)]
    fn fts_count(&self) -> Result<u64, FtsError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM context_fts", [], |row| row.get(0))?;
        u64::try_from(n).map_err(|_| FtsError::Corrupt("negative fts count"))
    }
}

impl FtsError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::EmptyQuery => "empty_query",
            Self::QueryTooLarge => "query_too_large",
            Self::InvalidLimit => "invalid_limit",
            Self::InvalidDocument => "invalid_document",
            Self::InvalidPolicy => "invalid_policy",
            Self::TooManyChunks => "too_many_chunks",
            Self::ChunkTooLarge => "chunk_too_large",
            Self::IndexTooLarge => "index_too_large",
            Self::NotCommitted => "not_committed",
            Self::Corrupt(_) => "corrupt",
            Self::Sqlite(_) => "sqlite",
            Self::Json(_) => "json",
            Self::Io(_) => "io",
        }
    }
}

impl fmt::Display for FtsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt(_) => f.write_str("corrupt fts index"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::Json(_) => f.write_str("symbol json error"),
            Self::Io(_) => f.write_str("io error"),
            other => f.write_str(other.as_str()),
        }
    }
}

impl Error for FtsError {}

impl From<rusqlite::Error> for FtsError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<serde_json::Error> for FtsError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<std::io::Error> for FtsError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl fmt::Debug for FtsIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FtsIndex").finish_non_exhaustive()
    }
}

fn configure_connection(conn: &Connection, require_wal: bool) -> Result<(), FtsError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if require_wal {
        let journal_mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(FtsError::Corrupt("journal_mode is not wal"));
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
    }
    conn.pragma_update(None, "foreign_keys", 1)?;
    Ok(())
}

fn ensure_schema(conn: &Connection) -> Result<(), FtsError> {
    conn.execute_batch(SCHEMA_CHUNKS)?;
    conn.execute_batch(SCHEMA_FTS)?;
    conn.execute_batch(SCHEMA_PATH_INDEX)?;
    Ok(())
}

fn validate_document(document: &FtsDocument, limits: &FtsLimits) -> Result<(), FtsError> {
    if document.chunks.len() > limits.max_chunks {
        return Err(FtsError::TooManyChunks);
    }
    let mut seen = BTreeSet::new();
    for chunk in &document.chunks {
        if chunk.end_byte <= chunk.start_byte || chunk.text.is_empty() {
            return Err(FtsError::InvalidDocument);
        }
        if chunk.text.len() > limits.max_chunk_bytes {
            return Err(FtsError::ChunkTooLarge);
        }
        if chunk.text.contains('\0') {
            return Err(FtsError::InvalidDocument);
        }
        let id = chunk.id.to_string();
        if !seen.insert(id) {
            return Err(FtsError::InvalidDocument);
        }
    }
    Ok(())
}

fn replace_path(
    tx: &Transaction<'_>,
    document: &FtsDocument,
    limits: &FtsLimits,
    started: Instant,
) -> Result<FtsWriteStats, FtsError> {
    let repo = document.repo_id.to_string();
    let path = document.path.as_str();
    let mut ids = BTreeSet::new();
    {
        let mut stmt =
            tx.prepare("SELECT id FROM context_chunks WHERE repo_id = ?1 AND path = ?2")?;
        let rows = stmt.query_map(params![repo, path], |row| row.get::<_, String>(0))?;
        for id in rows {
            ids.insert(id?);
        }
    }
    for chunk in &document.chunks {
        ids.insert(chunk.id.to_string());
    }

    let mut removed = 0u64;
    for (step, id) in ids.iter().enumerate() {
        if step.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(&limits.cancel, started, limits.timeout)?;
        }
        let fts_n = tx.execute("DELETE FROM context_fts WHERE chunk_id = ?1", params![id])?;
        let chunk_n = tx.execute("DELETE FROM context_chunks WHERE id = ?1", params![id])?;
        if fts_n > 0 || chunk_n > 0 {
            removed = removed.saturating_add(1);
        }
    }

    let indexed_at: String =
        tx.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
            row.get(0)
        })?;
    let language = document.language.map(|l| l.as_str());
    let mut inserted = 0u64;
    for (step, chunk) in document.chunks.iter().enumerate() {
        if step.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(&limits.cancel, started, limits.timeout)?;
        }
        let id = chunk.id.to_string();
        let content_hash = chunk.content_hash.to_string();
        let symbol_json = serde_json::to_string(&chunk.symbols)?;
        let fts_symbols = chunk.symbols.join(" ");
        let start = i64::from(chunk.start_byte);
        let end = i64::from(chunk.end_byte);
        tx.execute(
            "INSERT INTO context_chunks (
                id, repo_id, path, start_byte, end_byte, language,
                content_hash, text, symbol_json, indexed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                repo,
                path,
                start,
                end,
                language,
                content_hash,
                chunk.text,
                symbol_json,
                indexed_at,
            ],
        )?;
        tx.execute(
            "INSERT INTO context_fts (chunk_id, path, symbols, text) VALUES (?1, ?2, ?3, ?4)",
            params![id, path, fts_symbols, chunk.text],
        )?;
        inserted = inserted.saturating_add(1);
    }
    Ok(FtsWriteStats { removed, inserted })
}

fn ensure_index_budget(conn: &Transaction<'_>, max_bytes: u64) -> Result<(), FtsError> {
    if max_bytes == 0 {
        return Err(FtsError::IndexTooLarge);
    }
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let pages = u64::try_from(page_count).unwrap_or(u64::MAX);
    let size = u64::try_from(page_size).unwrap_or(u64::MAX);
    let bytes = pages.saturating_mul(size);
    if bytes > max_bytes {
        return Err(FtsError::IndexTooLarge);
    }
    Ok(())
}

fn compile_query(query: &FtsQuery, limits: &FtsLimits) -> Result<PreparedQuery, FtsError> {
    if query.limit == 0 {
        return Err(FtsError::InvalidLimit);
    }
    if query.text.len() > limits.max_query_bytes {
        return Err(FtsError::QueryTooLarge);
    }
    if query.text.contains('\0') {
        return Err(FtsError::EmptyQuery);
    }
    let terms = tokenize_query(&query.text);
    if terms.is_empty() {
        return Err(FtsError::EmptyQuery);
    }
    if terms.len() > limits.max_query_terms {
        return Err(FtsError::QueryTooLarge);
    }
    let mut compiled = String::from("{symbols text}:");
    for term in &terms {
        compiled.push(' ');
        compiled.push('"');
        compiled.push_str(&term.replace('"', "\"\""));
        compiled.push('"');
    }
    Ok(PreparedQuery {
        match_sql: compiled,
        limit: query.limit.min(limits.max_results),
    })
}

fn tokenize_query(raw: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    for ch in raw.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            terms.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    terms
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

fn check_bounds(
    cancel: &CancellationToken,
    started: Instant,
    timeout: Duration,
) -> Result<(), FtsError> {
    if cancel.is_cancelled() {
        return Err(FtsError::Cancelled);
    }
    if timeout.is_zero() || started.elapsed() > timeout {
        return Err(FtsError::Timeout);
    }
    Ok(())
}

fn hit_from_row(row: &rusqlite::Row<'_>) -> Result<FtsHit, FtsError> {
    let chunk_id: String = row.get(0)?;
    let repo_raw: String = row.get(1)?;
    let path_raw: String = row.get(2)?;
    let language_raw: Option<String> = row.get(3)?;
    let start_byte: i64 = row.get(4)?;
    let end_byte: i64 = row.get(5)?;
    let content_hash: String = row.get(6)?;
    let score: f64 = row.get(7)?;
    let repo_id =
        RepoId::from_str(&repo_raw).map_err(|_| FtsError::Corrupt("malformed repo_id"))?;
    let path = RepoPath::parse(&path_raw).map_err(|_| FtsError::Corrupt("malformed path"))?;
    let language = match language_raw.as_deref() {
        None => None,
        Some(name) => Some(
            SourceLanguage::ALL
                .iter()
                .copied()
                .find(|lang| lang.as_str() == name)
                .ok_or(FtsError::Corrupt("unknown language"))?,
        ),
    };
    let start_byte =
        u32::try_from(start_byte).map_err(|_| FtsError::Corrupt("start_byte out of range"))?;
    let end_byte =
        u32::try_from(end_byte).map_err(|_| FtsError::Corrupt("end_byte out of range"))?;
    if chunk_id.is_empty() || content_hash.is_empty() {
        return Err(FtsError::Corrupt("empty chunk identity"));
    }
    Ok(FtsHit {
        chunk_id,
        repo_id,
        path,
        language,
        start_byte,
        end_byte,
        content_hash,
        score,
    })
}

fn push_symbol_term(terms: &mut Vec<String>, value: &str) {
    if value.is_empty() || terms.iter().any(|existing| existing == value) {
        return;
    }
    terms.push(value.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    use crate::chunk::{ChunkPolicy, Document, chunk};

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

    fn document_from(src: &str, rel: &str, language: SourceLanguage) -> FtsDocument {
        let (doc, chunks) = records(rel, language, src);
        let fts = FtsDocument::from_records(&chunks, &[]).expect("fts document");
        assert_eq!(fts.repo_id(), doc.repo_id());
        fts
    }

    fn open_mem() -> FtsIndex {
        FtsIndex::open_in_memory(FtsLimits::new()).expect("open memory fts")
    }

    fn temp_path() -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("rapidlm-fts-{}-{seq}.sqlite", std::process::id()))
    }

    fn ids(hits: &[FtsHit]) -> Vec<String> {
        hits.iter().map(|h| h.chunk_id().to_string()).collect()
    }

    #[test]
    fn search_finds_indexed_text_and_symbol_terms() {
        let mut index = open_mem();
        let (doc, chunks) = records(
            "src/lib.rs",
            SourceLanguage::Rust,
            "fn unique_alpha() { 1 }\n",
        );
        let mut fts = FtsDocument::from_records(&chunks, &[]).expect("doc");
        fts.chunks[0].symbols.push("symbol_only_token".to_string());
        index.upsert_document(&fts).expect("upsert");

        let text_hits = index
            .search(&FtsQuery::new("unique_alpha").repo(doc.repo_id()))
            .expect("search text");
        assert_eq!(text_hits.len(), 1);
        assert_eq!(text_hits[0].chunk_id(), chunks[0].id().to_string());
        assert_eq!(text_hits[0].path().as_str(), "src/lib.rs");
        assert_eq!(text_hits[0].language(), Some(SourceLanguage::Rust));

        let symbol_hits = index
            .search(&FtsQuery::new("symbol_only_token"))
            .expect("search symbol");
        assert_eq!(symbol_hits.len(), 1);
        assert_eq!(symbol_hits[0].chunk_id(), chunks[0].id().to_string());
    }

    #[test]
    fn bm25_orders_more_frequent_terms_first() {
        let mut index = open_mem();
        let frequent = document_from(
            "alpha alpha alpha other words\n",
            "src/a.rs",
            SourceLanguage::Rust,
        );
        let rare = document_from("alpha once\n", "src/b.rs", SourceLanguage::Rust);
        index.upsert_document(&frequent).expect("upsert a");
        index.upsert_document(&rare).expect("upsert b");

        let hits = index.search(&FtsQuery::new("alpha")).expect("search");
        assert!(hits.len() >= 2);
        assert!(hits[0].score() <= hits[1].score());
        assert_eq!(hits[0].path().as_str(), "src/a.rs");
    }

    #[test]
    fn reindex_replaces_old_chunks_transactionally() {
        let mut index = open_mem();
        let first = document_from("unique_before token\n", "src/lib.rs", SourceLanguage::Rust);
        let first_id = first.chunks[0].id().to_string();
        index.upsert_document(&first).expect("upsert first");
        assert_eq!(index.chunk_count().expect("count"), 1);
        assert_eq!(index.fts_count().expect("fts"), 1);

        let mut second = document_from("unique_after token\n", "src/lib.rs", SourceLanguage::Rust);
        second.repo_id = first.repo_id();
        let stats = index.upsert_document(&second).expect("reindex");
        assert_eq!(stats.removed(), 1);
        assert_eq!(stats.inserted(), 1);
        assert_eq!(index.chunk_count().expect("count"), 1);
        assert_eq!(index.fts_count().expect("fts"), 1);

        let before = index.search(&FtsQuery::new("unique_before")).expect("old");
        let after = index.search(&FtsQuery::new("unique_after")).expect("new");
        assert!(before.is_empty());
        assert_eq!(after.len(), 1);
        assert_ne!(after[0].chunk_id(), first_id);
    }

    #[test]
    fn failed_reindex_leaves_prior_generation() {
        let mut index = open_mem();
        let first = document_from("keep_me token\n", "src/lib.rs", SourceLanguage::Rust);
        index.upsert_document(&first).expect("upsert first");
        index.fail_next_commit();
        let mut second = document_from("must_not_land token\n", "src/lib.rs", SourceLanguage::Rust);
        second.repo_id = first.repo_id();
        let err = index.upsert_document(&second).expect_err("not committed");
        assert!(matches!(err, FtsError::NotCommitted));
        assert_eq!(index.chunk_count().expect("count"), 1);
        assert!(
            !index
                .search(&FtsQuery::new("keep_me"))
                .expect("old")
                .is_empty()
        );
        assert!(
            index
                .search(&FtsQuery::new("must_not_land"))
                .expect("new")
                .is_empty()
        );
    }

    #[test]
    fn filters_repo_path_and_language() {
        let mut index = open_mem();
        let rust = document_from("shared_token rust_body\n", "src/a.rs", SourceLanguage::Rust);
        let rust_repo = rust.repo_id();
        index.upsert_document(&rust).expect("rust");

        let mut py = document_from(
            "shared_token python_body\n",
            "src/b.py",
            SourceLanguage::Python,
        );
        let py_repo = RepoId::new();
        py.repo_id = py_repo;
        index.upsert_document(&py).expect("python");

        let repo_hits = index
            .search(&FtsQuery::new("shared_token").repo(rust_repo))
            .expect("repo");
        assert_eq!(repo_hits.len(), 1);
        assert_eq!(repo_hits[0].repo_id(), rust_repo);

        let lang_hits = index
            .search(&FtsQuery::new("shared_token").language(SourceLanguage::Python))
            .expect("lang");
        assert_eq!(lang_hits.len(), 1);
        assert_eq!(lang_hits[0].language(), Some(SourceLanguage::Python));

        let file_hits = index
            .search(&FtsQuery::new("shared_token").path(path("src/a.rs")))
            .expect("file");
        assert_eq!(file_hits.len(), 1);
        assert_eq!(file_hits[0].path().as_str(), "src/a.rs");

        let prefix_hits = index
            .search(&FtsQuery::new("shared_token").path(path("src")))
            .expect("prefix");
        assert_eq!(prefix_hits.len(), 2);

        let wildcard = index
            .search(&FtsQuery::new("shared_token").path(path("src%")))
            .expect("escaped like");
        assert!(
            wildcard.is_empty(),
            "LIKE metacharacters in a path filter must not match src/a.rs"
        );
    }

    #[test]
    fn search_limit_is_stable_and_capped() {
        let mut index = FtsIndex::open_in_memory(FtsLimits::new().max_results(3)).expect("open");
        for i in 0..5 {
            let src = format!("stable_token body_{i}\n");
            let doc = document_from(&src, &format!("src/f{i}.rs"), SourceLanguage::Rust);
            index.upsert_document(&doc).expect("upsert");
        }
        let q = FtsQuery::new("stable_token").limit(10_000);
        let first = index.search(&q).expect("search 1");
        let second = index.search(&q).expect("search 2");
        assert_eq!(first.len(), 3);
        assert_eq!(ids(&first), ids(&second));
        assert!(
            first
                .windows(2)
                .all(|w| w[0].chunk_id() <= w[1].chunk_id() || w[0].score() < w[1].score())
        );
    }

    #[test]
    fn delete_document_removes_chunk_by_id() {
        let mut index = open_mem();
        let first = document_from("alpha_only\n", "src/a.rs", SourceLanguage::Rust);
        let second = document_from("beta_only\n", "src/b.rs", SourceLanguage::Rust);
        let first_id = first.chunks[0].id().to_string();
        index.upsert_document(&first).expect("a");
        index.upsert_document(&second).expect("b");
        assert!(index.delete_document(&first_id).expect("delete"));
        assert!(!index.delete_document(&first_id).expect("idempotent"));
        assert!(
            index
                .search(&FtsQuery::new("alpha_only"))
                .expect("a")
                .is_empty()
        );
        assert_eq!(
            index.search(&FtsQuery::new("beta_only")).expect("b").len(),
            1
        );
        assert_eq!(index.chunk_count().expect("count"), 1);
        assert_eq!(index.fts_count().expect("fts"), 1);
    }

    #[test]
    fn empty_document_clears_path() {
        let mut index = open_mem();
        let first = document_from("clear_me\n", "src/lib.rs", SourceLanguage::Rust);
        let repo = first.repo_id();
        index.upsert_document(&first).expect("upsert");
        let empty = FtsDocument::new(repo, path("src/lib.rs"), Some(SourceLanguage::Rust));
        let stats = index.upsert_document(&empty).expect("clear");
        assert_eq!(stats.removed(), 1);
        assert_eq!(stats.inserted(), 0);
        assert!(
            index
                .search(&FtsQuery::new("clear_me"))
                .expect("gone")
                .is_empty()
        );
    }

    #[test]
    fn raw_fts_operators_are_not_query_syntax() {
        let mut index = open_mem();
        let doc = document_from("alpha token\n", "src/lib.rs", SourceLanguage::Rust);
        index.upsert_document(&doc).expect("upsert");
        let hits = index
            .search(&FtsQuery::new("alpha OR missingterm"))
            .expect("search");
        assert!(hits.is_empty());
        assert!(matches!(
            index.search(&FtsQuery::new("***")).expect_err("empty"),
            FtsError::EmptyQuery
        ));
    }

    #[test]
    fn cancellation_and_timeout_are_typed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut index =
            FtsIndex::open_in_memory(FtsLimits::new().cancellation(cancel)).expect("open");
        let doc = document_from("cancel_me\n", "src/lib.rs", SourceLanguage::Rust);
        assert!(matches!(
            index.upsert_document(&doc).expect_err("cancel"),
            FtsError::Cancelled
        ));

        let mut timed =
            FtsIndex::open_in_memory(FtsLimits::new().timeout(Duration::ZERO)).expect("open timed");
        assert!(matches!(
            timed.upsert_document(&doc).expect_err("timeout"),
            FtsError::Timeout
        ));
        assert!(matches!(
            timed
                .search(&FtsQuery::new("cancel_me"))
                .expect_err("search timeout"),
            FtsError::Timeout
        ));
    }

    #[test]
    fn file_backed_index_survives_reopen() {
        let path = temp_path();
        let doc = document_from("persist_token\n", "src/lib.rs", SourceLanguage::Rust);
        let chunk_id = doc.chunks[0].id().to_string();
        {
            let mut index = FtsIndex::open(&path, FtsLimits::new()).expect("open");
            index.upsert_document(&doc).expect("upsert");
        }
        let index = FtsIndex::open(&path, FtsLimits::new()).expect("reopen");
        let hits = index
            .search(&FtsQuery::new("persist_token"))
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id(), chunk_id);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn query_and_policy_bounds() {
        let mut index =
            FtsIndex::open_in_memory(FtsLimits::new().max_query_bytes(4)).expect("open");
        let doc = document_from("bound_token\n", "src/lib.rs", SourceLanguage::Rust);
        index.upsert_document(&doc).expect("upsert");
        assert!(matches!(
            index
                .search(&FtsQuery::new("bound_token"))
                .expect_err("too large"),
            FtsError::QueryTooLarge
        ));
        assert!(matches!(
            index
                .search(&FtsQuery::new("ok").limit(0))
                .expect_err("limit"),
            FtsError::InvalidLimit
        ));
        assert!(matches!(
            FtsIndex::open_in_memory(FtsLimits::new().max_results(0)),
            Err(FtsError::InvalidPolicy)
        ));
    }
}
