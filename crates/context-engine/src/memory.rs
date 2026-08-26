//! Durable user/project/session memory with provenance, confidence, and TTL.
//!
//! Writes require an explicit source and scope. Retrieval never returns
//! expired records or project/session rows that do not match the caller.
//! Memory text is stored as data and cannot grant capability.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use protocol::{ContextItemId, ProjectId, SessionId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::ingest::content::ContentHash;
use crate::repo_manifest::CancellationToken;

/// Default wall-clock budget for one write or retrieve call.
pub const DEFAULT_MEMORY_TIMEOUT: Duration = Duration::from_secs(1);

/// Default unique-row cap for one store.
pub const DEFAULT_MAX_MEMORY_RECORDS: usize = 8_192;

/// Default UTF-8 byte cap for one memory body.
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 16 * 1024;

/// Default retrieve page size.
pub const DEFAULT_MEMORY_LIMIT: u32 = 64;

/// Hard retrieve cap applied even when a caller asks for more rows.
pub const DEFAULT_MAX_MEMORY_RESULTS: u32 = 256;

/// Maximum UTF-8 bytes accepted in a source identity.
pub const MAX_SOURCE_ID_BYTES: usize = 128;

/// Payload schema written into `source_json` and `context.memory_written`.
pub const MEMORY_SOURCE_SCHEMA: u16 = 1;

/// Ledger event kind for a successful durable write.
pub const MEMORY_WRITTEN_KIND: &str = "context.memory_written";

const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);
const CANCEL_STRIDE: usize = 16;
const ROLE_DATA: &str = "data";

const SCHEMA_MEMORIES: &str = "
CREATE TABLE IF NOT EXISTS memories (
  id TEXT PRIMARY KEY,
  scope TEXT NOT NULL,
  project_id TEXT,
  source_json TEXT NOT NULL,
  confidence REAL NOT NULL,
  content TEXT NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT
);
";

const SCHEMA_SCOPE_INDEX: &str = "
CREATE INDEX IF NOT EXISTS memories_scope_project
  ON memories(scope, project_id);
";

/// Resource bounds for one [`MemoryStore`]. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct MemoryLimits {
    max_records: usize,
    max_content_bytes: usize,
    max_results: u32,
    timeout: Duration,
    cancel: CancellationToken,
}

/// User / project / session partition. Project and session variants carry IDs
/// so a lower-scope write cannot be stored without its isolation key.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MemoryScope {
    User,
    Project(ProjectId),
    Session {
        session_id: SessionId,
        project_id: ProjectId,
    },
}

/// Scope discriminant persisted in the `memories.scope` column.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MemoryScopeKind {
    User,
    Project,
    Session,
}

/// Who asserted the memory. Required on every write; never inferred.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct MemorySource {
    kind: MemorySourceKind,
    id: String,
    session_id: Option<SessionId>,
}

/// Provenance kind stored in `source_json`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MemorySourceKind {
    User,
    Agent,
    Tool,
    System,
}

/// Memory is always model-visible data. This type has no instruction variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MemoryRole {
    Data,
}

/// RFC3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SS[.frac]Z`).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct MemoryTimestamp {
    rfc3339: String,
}

/// Parse failure for a non-canonical memory timestamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryTimestampParseError;

/// Write request. Source and scope are constructor arguments, not defaults.
#[derive(Clone, Debug)]
pub struct MemoryWrite {
    scope: MemoryScope,
    source: MemorySource,
    confidence: f64,
    content: String,
    expires_at: Option<MemoryTimestamp>,
}

/// Durable memory row. `content` is opaque data, never a capability grant.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryRecord {
    id: MemoryId,
    scope: MemoryScope,
    source: MemorySource,
    confidence: f64,
    content: String,
    content_hash: ContentHash,
    created_at: MemoryTimestamp,
    expires_at: Option<MemoryTimestamp>,
    role: MemoryRole,
}

/// Stable identifier. Wire form is a lowercase UUIDv7 string.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct MemoryId(String);

/// Retrieve filter. Missing project/session IDs make those scopes inapplicable.
#[derive(Clone, Debug)]
pub struct MemoryQuery {
    project_id: Option<ProjectId>,
    session_id: Option<SessionId>,
    scopes: Option<Vec<MemoryScopeKind>>,
    now: Option<MemoryTimestamp>,
    limit: u32,
    cancel: CancellationToken,
}

/// Typed payload for `context.memory_written`. Content is hashed, not copied.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryWritten {
    schema: u16,
    memory_id: String,
    scope: String,
    project_id: Option<String>,
    session_id: Option<String>,
    source_kind: String,
    source_id: String,
    confidence: f64,
    content_hash: String,
    created_at: String,
    expires_at: Option<String>,
    role: String,
}

/// File-backed or in-memory memory projection.
pub struct MemoryStore {
    conn: Connection,
    limits: MemoryLimits,
    fail_before_commit: AtomicBool,
}

/// Typed memory failure. Display never echoes content, IDs, or source text.
#[derive(Debug)]
pub enum MemoryError {
    Cancelled,
    Timeout,
    MissingSource,
    MissingScope,
    InvalidWrite,
    InvalidQuery,
    InvalidTimestamp,
    InvalidConfidence,
    ContentTooLarge,
    CapacityExceeded,
    NotCommitted,
    Corrupt(&'static str),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredSource {
    schema: u16,
    kind: String,
    id: String,
    session_id: Option<String>,
    role: String,
}

impl MemoryLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_records(mut self, value: usize) -> Self {
        self.max_records = value;
        self
    }

    pub fn max_content_bytes(mut self, value: usize) -> Self {
        self.max_content_bytes = value;
        self
    }

    pub fn max_results(mut self, value: u32) -> Self {
        self.max_results = value;
        self
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }

    pub fn max_records_value(&self) -> usize {
        self.max_records
    }

    pub fn max_content_bytes_value(&self) -> usize {
        self.max_content_bytes
    }

    pub fn max_results_value(&self) -> u32 {
        self.max_results
    }

    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }

    fn validate(&self) -> Result<(), MemoryError> {
        if self.max_records == 0 || self.max_content_bytes == 0 || self.max_results == 0 {
            return Err(MemoryError::InvalidQuery);
        }
        Ok(())
    }
}

impl Default for MemoryLimits {
    fn default() -> Self {
        Self {
            max_records: DEFAULT_MAX_MEMORY_RECORDS,
            max_content_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_results: DEFAULT_MAX_MEMORY_RESULTS,
            timeout: DEFAULT_MEMORY_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }
}

impl MemoryScope {
    pub const fn kind(self) -> MemoryScopeKind {
        match self {
            Self::User => MemoryScopeKind::User,
            Self::Project(_) => MemoryScopeKind::Project,
            Self::Session { .. } => MemoryScopeKind::Session,
        }
    }

    pub fn project_id(self) -> Option<ProjectId> {
        match self {
            Self::User => None,
            Self::Project(id) => Some(id),
            Self::Session { project_id, .. } => Some(project_id),
        }
    }

    pub fn session_id(self) -> Option<SessionId> {
        match self {
            Self::Session { session_id, .. } => Some(session_id),
            Self::User | Self::Project(_) => None,
        }
    }
}

impl MemoryScopeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Session => "session",
        }
    }

    fn parse(value: &str) -> Result<Self, MemoryError> {
        match value {
            "user" => Ok(Self::User),
            "project" => Ok(Self::Project),
            "session" => Ok(Self::Session),
            _ => Err(MemoryError::Corrupt("unknown memory scope")),
        }
    }
}

impl MemorySource {
    pub fn new(kind: MemorySourceKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
            session_id: None,
        }
    }

    pub fn session(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn kind(&self) -> MemorySourceKind {
        self.kind
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    fn validate(&self) -> Result<(), MemoryError> {
        if self.id.is_empty() {
            return Err(MemoryError::MissingSource);
        }
        if self.id.len() > MAX_SOURCE_ID_BYTES || self.id.contains('\0') {
            return Err(MemoryError::InvalidWrite);
        }
        Ok(())
    }
}

impl MemorySourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Tool => "tool",
            Self::System => "system",
        }
    }

    fn parse(value: &str) -> Result<Self, MemoryError> {
        match value {
            "user" => Ok(Self::User),
            "agent" => Ok(Self::Agent),
            "tool" => Ok(Self::Tool),
            "system" => Ok(Self::System),
            _ => Err(MemoryError::Corrupt("unknown memory source kind")),
        }
    }
}

impl MemoryRole {
    pub const fn as_str(self) -> &'static str {
        ROLE_DATA
    }

    pub const fn grants_capability(self) -> bool {
        false
    }
}

impl MemoryTimestamp {
    pub fn as_str(&self) -> &str {
        &self.rfc3339
    }

    fn cmp_key(&self) -> String {
        timestamp_cmp_key(&self.rfc3339)
    }

    fn precedes_or_eq(&self, other: &Self) -> bool {
        self.cmp_key() <= other.cmp_key()
    }
}

impl fmt::Display for MemoryTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.rfc3339)
    }
}

impl FromStr for MemoryTimestamp {
    type Err = MemoryTimestampParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_timestamp(s)
    }
}

impl MemoryWrite {
    pub fn new(
        scope: MemoryScope,
        source: MemorySource,
        confidence: f64,
        content: impl Into<String>,
    ) -> Self {
        Self {
            scope,
            source,
            confidence,
            content: content.into(),
            expires_at: None,
        }
    }

    pub fn expires_at(mut self, value: MemoryTimestamp) -> Self {
        self.expires_at = Some(value);
        self
    }

    pub fn scope(&self) -> MemoryScope {
        self.scope
    }

    pub fn source(&self) -> &MemorySource {
        &self.source
    }

    pub fn confidence(&self) -> f64 {
        self.confidence
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn expires_at_value(&self) -> Option<&MemoryTimestamp> {
        self.expires_at.as_ref()
    }
}

impl MemoryId {
    fn allocate() -> Self {
        Self(ContextItemId::new().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MemoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for MemoryId {
    type Err = MemoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let _: ContextItemId = s
            .parse()
            .map_err(|_| MemoryError::Corrupt("malformed memory id"))?;
        Ok(Self(s.to_owned()))
    }
}

impl MemoryRecord {
    pub fn id(&self) -> &MemoryId {
        &self.id
    }

    pub fn scope(&self) -> MemoryScope {
        self.scope
    }

    pub fn source(&self) -> &MemorySource {
        &self.source
    }

    pub fn confidence(&self) -> f64 {
        self.confidence
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn created_at(&self) -> &MemoryTimestamp {
        &self.created_at
    }

    pub fn expires_at(&self) -> Option<&MemoryTimestamp> {
        self.expires_at.as_ref()
    }

    pub fn role(&self) -> MemoryRole {
        self.role
    }

    /// Memory is never a capability or policy instruction.
    pub fn is_capability_bearing(&self) -> bool {
        self.role.grants_capability()
    }

    /// Typed `context.memory_written` payload. Body text is not included.
    pub fn ledger_payload(&self) -> MemoryWritten {
        MemoryWritten {
            schema: MEMORY_SOURCE_SCHEMA,
            memory_id: self.id.0.clone(),
            scope: self.scope.kind().as_str().to_owned(),
            project_id: self.scope.project_id().map(|id| id.to_string()),
            session_id: self
                .scope
                .session_id()
                .or(self.source.session_id)
                .map(|id| id.to_string()),
            source_kind: self.source.kind.as_str().to_owned(),
            source_id: self.source.id.clone(),
            confidence: self.confidence,
            content_hash: self.content_hash.to_string(),
            created_at: self.created_at.rfc3339.clone(),
            expires_at: self.expires_at.as_ref().map(|ts| ts.rfc3339.clone()),
            role: MemoryRole::Data.as_str().to_owned(),
        }
    }

    pub fn is_expired_at(&self, now: &MemoryTimestamp) -> bool {
        self.expires_at
            .as_ref()
            .is_some_and(|expires| expires.precedes_or_eq(now))
    }
}

impl MemoryWritten {
    pub fn schema(&self) -> u16 {
        self.schema
    }

    pub fn memory_id(&self) -> &str {
        &self.memory_id
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn project_id(&self) -> Option<&str> {
        self.project_id.as_deref()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn source_kind(&self) -> &str {
        &self.source_kind
    }

    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    pub fn confidence(&self) -> f64 {
        self.confidence
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn expires_at(&self) -> Option<&str> {
        self.expires_at.as_deref()
    }

    pub fn role(&self) -> &str {
        &self.role
    }

    pub fn grants_capability(&self) -> bool {
        false
    }
}

impl MemoryQuery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn project(mut self, project_id: ProjectId) -> Self {
        self.project_id = Some(project_id);
        self
    }

    pub fn session(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn scopes(mut self, scopes: impl Into<Vec<MemoryScopeKind>>) -> Self {
        self.scopes = Some(scopes.into());
        self
    }

    pub fn at(mut self, now: MemoryTimestamp) -> Self {
        self.now = Some(now);
        self
    }

    pub fn limit(mut self, value: u32) -> Self {
        self.limit = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }

    pub fn project_id(&self) -> Option<ProjectId> {
        self.project_id
    }

    pub fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    pub fn now(&self) -> Option<&MemoryTimestamp> {
        self.now.as_ref()
    }

    pub fn limit_value(&self) -> u32 {
        self.limit
    }
}

impl Default for MemoryQuery {
    fn default() -> Self {
        Self {
            project_id: None,
            session_id: None,
            scopes: None,
            now: None,
            limit: DEFAULT_MEMORY_LIMIT,
            cancel: CancellationToken::new(),
        }
    }
}

impl MemoryStore {
    /// Open (or create) a file-backed store and ensure the memories schema exists.
    pub fn open(path: impl AsRef<Path>, limits: MemoryLimits) -> Result<Self, MemoryError> {
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

    /// Open a process-private in-memory store. Not durable across processes.
    pub fn open_in_memory(limits: MemoryLimits) -> Result<Self, MemoryError> {
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

    /// Persist one memory. Source and scope must be explicit and valid.
    pub fn write_memory(&mut self, req: MemoryWrite) -> Result<MemoryRecord, MemoryError> {
        let started = Instant::now();
        self.check_ready(started)?;
        let prepared = prepare_write(&req, &self.limits)?;
        let created_at = read_now(&self.conn)?;
        if let Some(expires) = prepared.expires_at.as_ref() {
            if expires.precedes_or_eq(&created_at) {
                return Err(MemoryError::InvalidWrite);
            }
        }

        let Self {
            conn,
            limits,
            fail_before_commit,
        } = self;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_bounds(&limits.cancel, started, limits.timeout)?;
        let count = table_count(&tx)?;
        if count >= limits.max_records as u64 {
            return Err(MemoryError::CapacityExceeded);
        }

        let id = MemoryId::allocate();
        let source_json = serde_json::to_string(&prepared.stored_source)?;
        let project_id = prepared.scope.project_id().map(|id| id.to_string());
        tx.execute(
            "INSERT INTO memories (
                id, scope, project_id, source_json, confidence, content, created_at, expires_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id.as_str(),
                prepared.scope.kind().as_str(),
                project_id,
                source_json,
                prepared.confidence,
                prepared.content,
                created_at.as_str(),
                prepared.expires_at.as_ref().map(MemoryTimestamp::as_str),
            ],
        )?;
        check_bounds(&limits.cancel, started, limits.timeout)?;
        if fail_before_commit.swap(false, Ordering::SeqCst) {
            return Err(MemoryError::NotCommitted);
        }
        tx.commit()?;

        Ok(MemoryRecord {
            id,
            scope: prepared.scope,
            source: prepared.source,
            confidence: prepared.confidence,
            content_hash: ContentHash::from_bytes(prepared.content.as_bytes()),
            content: prepared.content,
            created_at,
            expires_at: prepared.expires_at,
            role: MemoryRole::Data,
        })
    }

    /// Return applicable, unexpired records. Inapplicable scopes are omitted.
    pub fn retrieve(&self, query: &MemoryQuery) -> Result<Vec<MemoryRecord>, MemoryError> {
        let started = Instant::now();
        self.check_query(query, started)?;
        if query.limit == 0 {
            return Err(MemoryError::InvalidQuery);
        }
        let limit = query.limit.min(self.limits.max_results);
        let now = match query.now.as_ref() {
            Some(ts) => ts.clone(),
            None => read_now(&self.conn)?,
        };

        let mut stmt = self.conn.prepare(
            "SELECT id, scope, project_id, source_json, confidence, content, created_at, expires_at
             FROM memories
             ORDER BY created_at DESC, id DESC",
        )?;
        let mut rows = stmt.query([])?;
        let mut records = Vec::new();
        let mut steps = 0usize;
        while let Some(row) = rows.next()? {
            steps = steps.saturating_add(1);
            if steps.is_multiple_of(CANCEL_STRIDE) {
                self.check_query(query, started)?;
            }
            let record = record_from_row(row)?;
            if !record_applies(&record, query, &now) {
                continue;
            }
            records.push(record);
            if records.len() as u32 >= limit {
                break;
            }
        }
        self.check_query(query, started)?;
        Ok(records)
    }

    pub fn get(&self, id: &MemoryId) -> Result<Option<MemoryRecord>, MemoryError> {
        let started = Instant::now();
        self.check_ready(started)?;
        let row = self
            .conn
            .query_row(
                "SELECT id, scope, project_id, source_json, confidence, content, created_at, expires_at
                 FROM memories WHERE id = ?1",
                params![id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, f64>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?;
        match row {
            Some(values) => record_from_values(values).map(Some),
            None => Ok(None),
        }
    }

    fn check_ready(&self, started: Instant) -> Result<(), MemoryError> {
        check_bounds(&self.limits.cancel, started, self.limits.timeout)
    }

    fn check_query(&self, query: &MemoryQuery, started: Instant) -> Result<(), MemoryError> {
        check_bounds(&query.cancel, started, self.limits.timeout)?;
        check_bounds(&self.limits.cancel, started, self.limits.timeout)
    }

    #[cfg(test)]
    fn fail_next_commit(&self) {
        self.fail_before_commit.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn row_count(&self) -> Result<u64, MemoryError> {
        table_count(&self.conn)
    }
}

impl MemoryError {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::MissingSource => "missing_source",
            Self::MissingScope => "missing_scope",
            Self::InvalidWrite => "invalid_write",
            Self::InvalidQuery => "invalid_query",
            Self::InvalidTimestamp => "invalid_timestamp",
            Self::InvalidConfidence => "invalid_confidence",
            Self::ContentTooLarge => "content_too_large",
            Self::CapacityExceeded => "capacity_exceeded",
            Self::NotCommitted => "not_committed",
            Self::Corrupt(_) => "corrupt",
            Self::Sqlite(_) => "sqlite",
            Self::Json(_) => "json",
            Self::Io(_) => "io",
        }
    }
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt(_) => f.write_str("corrupt memory store"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::Json(_) => f.write_str("memory source json error"),
            Self::Io(_) => f.write_str("io error"),
            other => f.write_str(other.as_str()),
        }
    }
}

impl Error for MemoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for MemoryError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<serde_json::Error> for MemoryError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<std::io::Error> for MemoryError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryStore").finish_non_exhaustive()
    }
}

impl fmt::Display for MemoryTimestampParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed memory timestamp")
    }
}

impl Error for MemoryTimestampParseError {}

struct PreparedWrite {
    scope: MemoryScope,
    source: MemorySource,
    stored_source: StoredSource,
    confidence: f64,
    content: String,
    expires_at: Option<MemoryTimestamp>,
}

fn prepare_write(req: &MemoryWrite, limits: &MemoryLimits) -> Result<PreparedWrite, MemoryError> {
    req.source.validate()?;
    if !req.confidence.is_finite() || !(0.0..=1.0).contains(&req.confidence) {
        return Err(MemoryError::InvalidConfidence);
    }
    if req.content.is_empty() || req.content.contains('\0') {
        return Err(MemoryError::InvalidWrite);
    }
    if req.content.len() > limits.max_content_bytes {
        return Err(MemoryError::ContentTooLarge);
    }

    let mut source = req.source.clone();
    match req.scope {
        MemoryScope::User | MemoryScope::Project(_) => {}
        MemoryScope::Session { session_id, .. } => match source.session_id {
            Some(existing) if existing != session_id => {
                return Err(MemoryError::InvalidWrite);
            }
            Some(_) => {}
            None => source.session_id = Some(session_id),
        },
    }

    Ok(PreparedWrite {
        stored_source: StoredSource {
            schema: MEMORY_SOURCE_SCHEMA,
            kind: source.kind.as_str().to_owned(),
            id: source.id.clone(),
            session_id: source.session_id.map(|id| id.to_string()),
            role: MemoryRole::Data.as_str().to_owned(),
        },
        scope: req.scope,
        source,
        confidence: req.confidence,
        content: req.content.clone(),
        expires_at: req.expires_at.clone(),
    })
}

fn record_applies(record: &MemoryRecord, query: &MemoryQuery, now: &MemoryTimestamp) -> bool {
    if record.is_expired_at(now) {
        return false;
    }
    if let Some(scopes) = query.scopes.as_ref() {
        if !scopes.contains(&record.scope.kind()) {
            return false;
        }
    }
    match record.scope {
        MemoryScope::User => true,
        MemoryScope::Project(project_id) => query.project_id == Some(project_id),
        MemoryScope::Session {
            session_id,
            project_id,
        } => query.session_id == Some(session_id) && query.project_id == Some(project_id),
    }
}

fn record_from_row(row: &rusqlite::Row<'_>) -> Result<MemoryRecord, MemoryError> {
    record_from_values((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn record_from_values(
    values: (
        String,
        String,
        Option<String>,
        String,
        f64,
        String,
        String,
        Option<String>,
    ),
) -> Result<MemoryRecord, MemoryError> {
    let (id, scope, project_id, source_json, confidence, content, created_at, expires_at) = values;
    let stored: StoredSource = serde_json::from_str(&source_json)?;
    if stored.schema != MEMORY_SOURCE_SCHEMA {
        return Err(MemoryError::Corrupt("unsupported memory source schema"));
    }
    if stored.role != ROLE_DATA {
        return Err(MemoryError::Corrupt("memory promoted above data role"));
    }
    let source_kind = MemorySourceKind::parse(&stored.kind)?;
    if stored.id.is_empty() {
        return Err(MemoryError::Corrupt("memory source missing id"));
    }
    let source_session = match stored.session_id.as_deref() {
        Some(raw) => Some(
            raw.parse::<SessionId>()
                .map_err(|_| MemoryError::Corrupt("malformed source session id"))?,
        ),
        None => None,
    };
    let scope_kind = MemoryScopeKind::parse(&scope)?;
    let scope = match scope_kind {
        MemoryScopeKind::User => {
            if project_id.is_some() {
                return Err(MemoryError::Corrupt("user memory has project id"));
            }
            MemoryScope::User
        }
        MemoryScopeKind::Project => {
            let project_id = project_id
                .as_deref()
                .ok_or(MemoryError::Corrupt("project memory missing project id"))?;
            let project_id = project_id
                .parse::<ProjectId>()
                .map_err(|_| MemoryError::Corrupt("malformed project id"))?;
            MemoryScope::Project(project_id)
        }
        MemoryScopeKind::Session => {
            let project_id = project_id
                .as_deref()
                .ok_or(MemoryError::Corrupt("session memory missing project id"))?;
            let project_id = project_id
                .parse::<ProjectId>()
                .map_err(|_| MemoryError::Corrupt("malformed project id"))?;
            let session_id =
                source_session.ok_or(MemoryError::Corrupt("session memory missing session id"))?;
            MemoryScope::Session {
                session_id,
                project_id,
            }
        }
    };
    let created_at = created_at
        .parse()
        .map_err(|_| MemoryError::Corrupt("malformed created_at"))?;
    let expires_at = match expires_at {
        Some(raw) => Some(
            raw.parse()
                .map_err(|_| MemoryError::Corrupt("malformed expires_at"))?,
        ),
        None => None,
    };
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        return Err(MemoryError::Corrupt("invalid stored confidence"));
    }
    Ok(MemoryRecord {
        id: id.parse()?,
        scope,
        source: MemorySource {
            kind: source_kind,
            id: stored.id,
            session_id: source_session,
        },
        confidence,
        content_hash: ContentHash::from_bytes(content.as_bytes()),
        content,
        created_at,
        expires_at,
        role: MemoryRole::Data,
    })
}

fn configure_connection(conn: &Connection, require_wal: bool) -> Result<(), MemoryError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if require_wal {
        let journal_mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(MemoryError::Corrupt("journal_mode is not wal"));
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
    }
    conn.pragma_update(None, "foreign_keys", 1)?;
    Ok(())
}

fn ensure_schema(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(SCHEMA_MEMORIES)?;
    conn.execute_batch(SCHEMA_SCOPE_INDEX)?;
    Ok(())
}

fn read_now(conn: &Connection) -> Result<MemoryTimestamp, MemoryError> {
    let raw: String =
        conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
            row.get(0)
        })?;
    raw.parse().map_err(|_| MemoryError::InvalidTimestamp)
}

fn table_count(conn: &Connection) -> Result<u64, MemoryError> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
    u64::try_from(n).map_err(|_| MemoryError::Corrupt("negative memory count"))
}

fn check_bounds(
    cancel: &CancellationToken,
    started: Instant,
    timeout: Duration,
) -> Result<(), MemoryError> {
    if cancel.is_cancelled() {
        return Err(MemoryError::Cancelled);
    }
    if timeout.is_zero() || started.elapsed() > timeout {
        return Err(MemoryError::Timeout);
    }
    Ok(())
}

fn parse_timestamp(s: &str) -> Result<MemoryTimestamp, MemoryTimestampParseError> {
    if s.len() < 20 || s.len() > 40 {
        return Err(MemoryTimestampParseError);
    }
    let prefix = s.get(..19).ok_or(MemoryTimestampParseError)?;
    if !is_valid_datetime_prefix(prefix) {
        return Err(MemoryTimestampParseError);
    }
    let rest = &s[19..];
    if rest == "Z" {
        return Ok(MemoryTimestamp {
            rfc3339: s.to_owned(),
        });
    }
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.strip_suffix('Z').ok_or(MemoryTimestampParseError)?;
        if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(MemoryTimestampParseError);
        }
        return Ok(MemoryTimestamp {
            rfc3339: s.to_owned(),
        });
    }
    Err(MemoryTimestampParseError)
}

fn is_valid_datetime_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[5].is_ascii_digit()
        && b[6].is_ascii_digit()
        && b[8].is_ascii_digit()
        && b[9].is_ascii_digit()
        && b[11].is_ascii_digit()
        && b[12].is_ascii_digit()
        && b[14].is_ascii_digit()
        && b[15].is_ascii_digit()
        && b[17].is_ascii_digit()
        && b[18].is_ascii_digit()
}

fn timestamp_cmp_key(s: &str) -> String {
    let prefix = s.get(..19).unwrap_or(s);
    let rest = s.get(19..).unwrap_or("");
    let mut frac = String::from("000000000");
    if let Some(digits) = rest.strip_prefix('.').and_then(|r| r.strip_suffix('Z')) {
        let take = digits.len().min(9);
        frac.replace_range(..take, &digits[..take]);
    }
    let mut key = String::with_capacity(28);
    key.push_str(prefix);
    key.push('.');
    key.push_str(&frac);
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn open_mem() -> MemoryStore {
        MemoryStore::open_in_memory(MemoryLimits::new()).expect("open memory store")
    }

    fn temp_path() -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "rapidlm-memory-{}-{seq}.sqlite",
            std::process::id()
        ))
    }

    fn source(id: &str) -> MemorySource {
        MemorySource::new(MemorySourceKind::User, id)
    }

    fn ts(raw: &str) -> MemoryTimestamp {
        raw.parse().expect("timestamp")
    }

    #[test]
    fn write_requires_explicit_source_and_scope() {
        let mut store = open_mem();
        let err = store
            .write_memory(MemoryWrite::new(
                MemoryScope::User,
                MemorySource::new(MemorySourceKind::Agent, ""),
                0.5,
                "note",
            ))
            .expect_err("empty source");
        assert!(matches!(err, MemoryError::MissingSource));
        assert_eq!(err.as_str(), "missing_source");

        let written = store
            .write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("user-1"),
                0.8,
                "remember this",
            ))
            .expect("write");
        assert_eq!(written.scope(), MemoryScope::User);
        assert_eq!(written.source().id(), "user-1");
        assert_eq!(written.source().kind(), MemorySourceKind::User);
        assert!(!written.is_capability_bearing());
        assert_eq!(written.role(), MemoryRole::Data);
    }

    #[test]
    fn project_memory_does_not_bleed_across_project_ids() {
        let mut store = open_mem();
        let project_a = ProjectId::new();
        let project_b = ProjectId::new();
        store
            .write_memory(MemoryWrite::new(
                MemoryScope::Project(project_a),
                source("author"),
                0.9,
                "secret-to-a",
            ))
            .expect("write a");
        store
            .write_memory(MemoryWrite::new(
                MemoryScope::Project(project_b),
                source("author"),
                0.9,
                "secret-to-b",
            ))
            .expect("write b");

        let a = store
            .retrieve(&MemoryQuery::new().project(project_a))
            .expect("retrieve a");
        let b = store
            .retrieve(&MemoryQuery::new().project(project_b))
            .expect("retrieve b");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].content(), "secret-to-a");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].content(), "secret-to-b");
        assert!(
            store
                .retrieve(&MemoryQuery::new())
                .expect("no project")
                .is_empty()
        );
    }

    #[test]
    fn session_memory_is_isolated_and_project_bound() {
        let mut store = open_mem();
        let project = ProjectId::new();
        let other_project = ProjectId::new();
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        store
            .write_memory(MemoryWrite::new(
                MemoryScope::Session {
                    session_id: session_a,
                    project_id: project,
                },
                source("agent-1"),
                0.7,
                "session-a-only",
            ))
            .expect("write session");

        let visible = store
            .retrieve(
                &MemoryQuery::new()
                    .project(project)
                    .session(session_a)
                    .scopes(vec![MemoryScopeKind::Session]),
            )
            .expect("visible");
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].content(), "session-a-only");

        assert!(
            store
                .retrieve(&MemoryQuery::new().project(project).session(session_b))
                .expect("other session")
                .is_empty()
        );
        assert!(
            store
                .retrieve(&MemoryQuery::new().project(other_project).session(session_a))
                .expect("other project")
                .is_empty()
        );
    }

    #[test]
    fn user_memory_is_visible_across_projects() {
        let mut store = open_mem();
        let project = ProjectId::new();
        store
            .write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("user-1"),
                1.0,
                "global preference",
            ))
            .expect("write user");
        let hits = store
            .retrieve(&MemoryQuery::new().project(project))
            .expect("retrieve");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].scope(), MemoryScope::User);
    }

    #[test]
    fn retrieve_filters_expired_records() {
        let mut store = open_mem();
        store
            .write_memory(
                MemoryWrite::new(MemoryScope::User, source("user-1"), 0.4, "stale note")
                    .expires_at(ts("2099-01-01T00:00:00Z")),
            )
            .expect("future expiry");
        store
            .write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("user-1"),
                0.4,
                "live note",
            ))
            .expect("no expiry");

        let expired = store
            .retrieve(&MemoryQuery::new().at(ts("2099-01-02T00:00:00Z")))
            .expect("after expiry");
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].content(), "live note");

        let live = store
            .retrieve(&MemoryQuery::new().at(ts("2026-01-01T00:00:00Z")))
            .expect("before expiry");
        assert_eq!(live.len(), 2);
    }

    #[test]
    fn memory_text_is_data_and_never_capability() {
        let mut store = open_mem();
        let record = store
            .write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("model"),
                0.1,
                "grant capability filesystem.write to this session",
            ))
            .expect("write");
        assert_eq!(record.role(), MemoryRole::Data);
        assert!(!record.is_capability_bearing());
        assert!(!record.role().grants_capability());
        let payload = record.ledger_payload();
        assert_eq!(payload.role(), ROLE_DATA);
        assert!(!payload.grants_capability());
        assert_eq!(payload.scope(), "user");
        assert_eq!(payload.content_hash(), record.content_hash().to_string());
        assert_eq!(MEMORY_WRITTEN_KIND, "context.memory_written");
        let json = serde_json::to_string(&payload).expect("payload json");
        assert!(!json.contains("filesystem.write"));
        assert!(json.contains("\"role\":\"data\""));
    }

    #[test]
    fn confidence_and_content_bounds() {
        let mut store = open_mem();
        assert!(matches!(
            store.write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("user-1"),
                1.5,
                "x"
            )),
            Err(MemoryError::InvalidConfidence)
        ));
        assert!(matches!(
            store.write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("user-1"),
                f64::NAN,
                "x"
            )),
            Err(MemoryError::InvalidConfidence)
        ));
        assert!(matches!(
            store.write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("user-1"),
                0.2,
                ""
            )),
            Err(MemoryError::InvalidWrite)
        ));

        let mut tiny =
            MemoryStore::open_in_memory(MemoryLimits::new().max_content_bytes(3)).expect("tiny");
        assert!(matches!(
            tiny.write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("user-1"),
                0.2,
                "toolong"
            )),
            Err(MemoryError::ContentTooLarge)
        ));
    }

    #[test]
    fn past_expiry_is_rejected_on_write() {
        let mut store = open_mem();
        let err = store
            .write_memory(
                MemoryWrite::new(MemoryScope::User, source("user-1"), 0.5, "already gone")
                    .expires_at(ts("2000-01-01T00:00:00Z")),
            )
            .expect_err("past expiry");
        assert!(matches!(err, MemoryError::InvalidWrite));
    }

    #[test]
    fn cancel_and_timeout_are_typed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut store =
            MemoryStore::open_in_memory(MemoryLimits::new().cancellation(cancel)).expect("open");
        assert!(matches!(
            store.write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("user-1"),
                0.5,
                "x"
            )),
            Err(MemoryError::Cancelled)
        ));

        let mut timed =
            MemoryStore::open_in_memory(MemoryLimits::new().timeout(Duration::ZERO)).expect("open");
        assert!(matches!(
            timed.write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("user-1"),
                0.5,
                "x"
            )),
            Err(MemoryError::Timeout)
        ));
    }

    #[test]
    fn failed_write_is_not_durable() {
        let mut store = open_mem();
        store.fail_next_commit();
        let err = store
            .write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("user-1"),
                0.5,
                "must not land",
            ))
            .expect_err("not committed");
        assert!(matches!(err, MemoryError::NotCommitted));
        assert_eq!(store.row_count().expect("count"), 0);
        assert!(
            store
                .retrieve(&MemoryQuery::new())
                .expect("retrieve")
                .is_empty()
        );
    }

    #[test]
    fn file_backed_store_survives_reopen() {
        let path = temp_path();
        let project = ProjectId::new();
        let id;
        {
            let mut store = MemoryStore::open(&path, MemoryLimits::new()).expect("open");
            let record = store
                .write_memory(MemoryWrite::new(
                    MemoryScope::Project(project),
                    source("author"),
                    0.6,
                    "persist me",
                ))
                .expect("write");
            id = record.id().clone();
        }
        let store = MemoryStore::open(&path, MemoryLimits::new()).expect("reopen");
        let loaded = store.get(&id).expect("get").expect("present");
        assert_eq!(loaded.content(), "persist me");
        assert_eq!(loaded.scope(), MemoryScope::Project(project));
        let hits = store
            .retrieve(&MemoryQuery::new().project(project))
            .expect("retrieve");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id().as_str(), id.as_str());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn session_source_mismatch_is_rejected() {
        let mut store = open_mem();
        let project = ProjectId::new();
        let err = store
            .write_memory(MemoryWrite::new(
                MemoryScope::Session {
                    session_id: SessionId::new(),
                    project_id: project,
                },
                source("agent-1").session(SessionId::new()),
                0.5,
                "mismatch",
            ))
            .expect_err("mismatch");
        assert!(matches!(err, MemoryError::InvalidWrite));
    }

    #[test]
    fn capacity_is_bounded() {
        let mut store =
            MemoryStore::open_in_memory(MemoryLimits::new().max_records(1)).expect("open");
        store
            .write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("user-1"),
                0.5,
                "one",
            ))
            .expect("first");
        assert!(matches!(
            store.write_memory(MemoryWrite::new(
                MemoryScope::User,
                source("user-1"),
                0.5,
                "two"
            )),
            Err(MemoryError::CapacityExceeded)
        ));
    }

    #[test]
    fn timestamp_parse_rejects_non_canonical() {
        for sample in [
            "",
            "2026-08-14",
            "2026-08-14 15:20:04Z",
            "2026-08-14T15:20:04+00:00",
            "2026-08-14T15:20:04.Z",
        ] {
            assert!(sample.parse::<MemoryTimestamp>().is_err(), "{sample}");
        }
        assert!("2026-08-14T15:20:04Z".parse::<MemoryTimestamp>().is_ok());
        assert!(
            "2026-08-14T15:20:04.123Z"
                .parse::<MemoryTimestamp>()
                .is_ok()
        );
    }
}
