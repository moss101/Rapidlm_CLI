//! Persistent symbol/code graph for local neighborhood retrieval.
//!
//! Definition, import, and reference edges are stored per source path. Reindex
//! of one document deletes that path's prior symbols and edges in a single
//! IMMEDIATE transaction. Traversal is BFS with hard hop and node budgets.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use protocol::{DEFAULT_MAX_INDEX_BYTES, RepoId, RepoPath};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::ingest::content::{ContentHash, SourceLanguage};
use crate::parse::symbols::{DEFAULT_MAX_SYMBOLS, SourceRange, SymbolKind, SymbolRecord};
use crate::repo_manifest::CancellationToken;

/// Default wall-clock budget for one writer or traversal call.
pub const DEFAULT_GRAPH_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard default hop cap applied even when a caller asks for more.
pub const DEFAULT_MAX_GRAPH_HOPS: u32 = 8;

/// Hard default node-expansion cap for one neighborhood walk.
pub const DEFAULT_MAX_GRAPH_NODES: u32 = 256;

/// Hard default edge-return cap for one neighborhood walk.
pub const DEFAULT_MAX_GRAPH_RESULTS: u32 = 256;

/// Default maximum symbols accepted from one source path.
pub const DEFAULT_MAX_GRAPH_SYMBOLS: usize = DEFAULT_MAX_SYMBOLS;

/// Default maximum edges emitted for one source path.
pub const DEFAULT_MAX_GRAPH_EDGES: usize = 8_192;

/// UTF-8 byte cap for a stored locator.
pub const MAX_LOCATOR_BYTES: usize = 1_024;

/// Bounded SQLite lock wait. Matches the ledger busy-timeout recovery rule.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

const CANCEL_STRIDE: usize = 16;
const FILE_KIND: &str = "file";
const NAME_PREFIX: &str = "#name:";
const FQ_PREFIX: &str = "#fq:";

const CONF_DEFINITION: f32 = 1.0;
const CONF_IMPORT: f32 = 0.90;
const CONF_REFERENCE_RESOLVED: f32 = 0.85;
const CONF_REFERENCE_NAME: f32 = 0.65;
const CONF_FQ: f32 = 0.95;

const SCHEMA_SYMBOLS: &str = "
CREATE TABLE IF NOT EXISTS context_graph_symbols (
  repo_id TEXT NOT NULL,
  locator TEXT NOT NULL,
  path TEXT NOT NULL,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  fq_name TEXT NOT NULL,
  start_byte INTEGER NOT NULL,
  end_byte INTEGER NOT NULL,
  start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  container TEXT,
  signature TEXT,
  content_hash TEXT NOT NULL,
  PRIMARY KEY (repo_id, locator)
);
";

const SCHEMA_EDGES: &str = "
CREATE TABLE IF NOT EXISTS context_graph_edges (
  from_repo TEXT NOT NULL,
  from_id TEXT NOT NULL,
  to_repo TEXT NOT NULL,
  to_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  confidence REAL NOT NULL,
  source TEXT NOT NULL,
  source_hash TEXT NOT NULL,
  path TEXT NOT NULL,
  PRIMARY KEY (from_repo, from_id, to_repo, to_id, kind, source)
);
";

const SCHEMA_INDEXES: &str = "
CREATE INDEX IF NOT EXISTS context_graph_symbols_repo_path
  ON context_graph_symbols(repo_id, path);
CREATE INDEX IF NOT EXISTS context_graph_symbols_repo_name
  ON context_graph_symbols(repo_id, name);
CREATE INDEX IF NOT EXISTS context_graph_edges_from
  ON context_graph_edges(from_repo, from_id);
CREATE INDEX IF NOT EXISTS context_graph_edges_to
  ON context_graph_edges(to_repo, to_id);
CREATE INDEX IF NOT EXISTS context_graph_edges_path
  ON context_graph_edges(from_repo, path);
";

/// Per-index resource bounds. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct GraphLimits {
    timeout: Duration,
    max_hops: u32,
    max_nodes: u32,
    max_results: u32,
    max_symbols: usize,
    max_edges: usize,
    max_index_bytes: u64,
    cancel: CancellationToken,
}

/// One source path being written. Empty `symbols` means “remove this path”.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphDocument {
    repo_id: RepoId,
    path: RepoPath,
    language: Option<SourceLanguage>,
    content_hash: ContentHash,
    symbols: Vec<SymbolRecord>,
}

/// Stable symbol identity inside one repository.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct SymbolLocator {
    repo_id: RepoId,
    locator: String,
}

/// Edge classification persisted for neighborhood retrieval.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum GraphEdgeKind {
    Definition,
    Import,
    Reference,
}

/// Provenance of an edge. Syntax facts are the only writer in this module.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum GraphEdgeSource {
    Syntax,
}

/// Typed graph edge with the producing document's content hash.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphEdge {
    from: SymbolLocator,
    to: SymbolLocator,
    kind: GraphEdgeKind,
    confidence: f32,
    source: GraphEdgeSource,
    source_hash: String,
}

/// Rows removed and inserted by one transactional write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphWriteStats {
    removed_symbols: u64,
    inserted_symbols: u64,
    removed_edges: u64,
    inserted_edges: u64,
}

/// File-backed or in-memory symbol/code graph.
pub struct CodeGraph {
    conn: Connection,
    limits: GraphLimits,
    fail_before_commit: AtomicBool,
}

/// Typed graph failure. Display never echoes source, locators, or host paths.
#[derive(Debug)]
pub enum GraphError {
    Cancelled,
    Timeout,
    InvalidLocator,
    InvalidDocument,
    InvalidPolicy,
    InvalidLimit,
    TooManySymbols,
    TooManyEdges,
    IndexTooLarge,
    /// Write ran but the transaction did not commit; prior rows are intact.
    NotCommitted,
    Corrupt(&'static str),
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct EdgeKey {
    from: String,
    to: String,
    kind: GraphEdgeKind,
}

struct PlannedEdge {
    from: String,
    to: String,
    kind: GraphEdgeKind,
    confidence: f32,
}

impl GraphLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn max_hops(mut self, value: u32) -> Self {
        self.max_hops = value;
        self
    }

    pub fn max_nodes(mut self, value: u32) -> Self {
        self.max_nodes = value;
        self
    }

    pub fn max_results(mut self, value: u32) -> Self {
        self.max_results = value;
        self
    }

    pub fn max_symbols(mut self, value: usize) -> Self {
        self.max_symbols = value;
        self
    }

    pub fn max_edges(mut self, value: usize) -> Self {
        self.max_edges = value;
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

    pub fn max_hops_value(&self) -> u32 {
        self.max_hops
    }

    pub fn max_nodes_value(&self) -> u32 {
        self.max_nodes
    }

    pub fn max_results_value(&self) -> u32 {
        self.max_results
    }

    pub fn max_symbols_value(&self) -> usize {
        self.max_symbols
    }

    pub fn max_edges_value(&self) -> usize {
        self.max_edges
    }

    pub fn max_index_bytes_value(&self) -> u64 {
        self.max_index_bytes
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }

    fn validate(&self) -> Result<(), GraphError> {
        if self.max_hops == 0
            || self.max_nodes == 0
            || self.max_results == 0
            || self.max_symbols == 0
            || self.max_edges == 0
        {
            return Err(GraphError::InvalidPolicy);
        }
        Ok(())
    }
}

impl Default for GraphLimits {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_GRAPH_TIMEOUT,
            max_hops: DEFAULT_MAX_GRAPH_HOPS,
            max_nodes: DEFAULT_MAX_GRAPH_NODES,
            max_results: DEFAULT_MAX_GRAPH_RESULTS,
            max_symbols: DEFAULT_MAX_GRAPH_SYMBOLS,
            max_edges: DEFAULT_MAX_GRAPH_EDGES,
            max_index_bytes: DEFAULT_MAX_INDEX_BYTES,
            cancel: CancellationToken::new(),
        }
    }
}

impl GraphDocument {
    pub fn new(
        repo_id: RepoId,
        path: RepoPath,
        content_hash: ContentHash,
        language: Option<SourceLanguage>,
    ) -> Self {
        Self {
            repo_id,
            path,
            language,
            content_hash,
            symbols: Vec::new(),
        }
    }

    pub fn from_symbols(
        repo_id: RepoId,
        path: RepoPath,
        content_hash: ContentHash,
        language: Option<SourceLanguage>,
        symbols: Vec<SymbolRecord>,
    ) -> Self {
        Self {
            repo_id,
            path,
            language,
            content_hash,
            symbols,
        }
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

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn symbols(&self) -> &[SymbolRecord] {
        &self.symbols
    }
}

impl SymbolLocator {
    pub fn new(repo_id: RepoId, locator: impl Into<String>) -> Result<Self, GraphError> {
        let locator = locator.into();
        validate_locator(&locator)?;
        Ok(Self { repo_id, locator })
    }

    pub fn from_symbol(repo_id: RepoId, symbol: &SymbolRecord) -> Result<Self, GraphError> {
        Self::new(repo_id, symbol.locator())
    }

    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn as_str(&self) -> &str {
        &self.locator
    }
}

impl GraphEdgeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::Import => "import",
            Self::Reference => "reference",
        }
    }
}

impl fmt::Display for GraphEdgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl GraphEdgeSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
        }
    }
}

impl fmt::Display for GraphEdgeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl GraphEdge {
    pub fn from(&self) -> &SymbolLocator {
        &self.from
    }

    pub fn to(&self) -> &SymbolLocator {
        &self.to
    }

    pub fn kind(&self) -> GraphEdgeKind {
        self.kind
    }

    pub fn confidence(&self) -> f32 {
        self.confidence
    }

    pub fn source(&self) -> GraphEdgeSource {
        self.source
    }

    pub fn source_hash(&self) -> &str {
        &self.source_hash
    }
}

impl GraphWriteStats {
    pub fn removed_symbols(self) -> u64 {
        self.removed_symbols
    }

    pub fn inserted_symbols(self) -> u64 {
        self.inserted_symbols
    }

    pub fn removed_edges(self) -> u64 {
        self.removed_edges
    }

    pub fn inserted_edges(self) -> u64 {
        self.inserted_edges
    }
}

impl CodeGraph {
    /// Open (or create) a file-backed graph and ensure the schema exists.
    pub fn open(path: impl AsRef<Path>, limits: GraphLimits) -> Result<Self, GraphError> {
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

    /// Open a process-private in-memory graph. Rebuildable; not durable.
    pub fn open_in_memory(limits: GraphLimits) -> Result<Self, GraphError> {
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

    /// Replace every symbol and edge for `document.path` in one transaction.
    pub fn upsert_document(
        &mut self,
        document: &GraphDocument,
    ) -> Result<GraphWriteStats, GraphError> {
        let started = Instant::now();
        let Self {
            conn,
            limits,
            fail_before_commit,
        } = self;
        check_bounds(&limits.cancel, started, limits.timeout)?;
        validate_document(document, limits)?;
        let planned = plan_edges(document, limits)?;
        check_bounds(&limits.cancel, started, limits.timeout)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_bounds(&limits.cancel, started, limits.timeout)?;
        let stats = replace_path(&tx, document, &planned, limits, started)?;
        ensure_index_budget(&tx, limits.max_index_bytes)?;
        check_bounds(&limits.cancel, started, limits.timeout)?;
        if fail_before_commit.swap(false, Ordering::SeqCst) {
            return Err(GraphError::NotCommitted);
        }
        tx.commit()?;
        Ok(stats)
    }

    /// BFS neighborhood. `hops` and `limit` are clamped to the index hard caps.
    pub fn neighbors(
        &self,
        locator: &SymbolLocator,
        hops: u32,
        limit: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let started = Instant::now();
        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        if limit == 0 {
            return Err(GraphError::InvalidLimit);
        }
        validate_locator(locator.as_str())?;
        if hops == 0 {
            return Ok(Vec::new());
        }

        let hops = hops.min(self.limits.max_hops);
        let limit = limit.min(self.limits.max_results);
        let max_nodes = self.limits.max_nodes as usize;

        let mut stmt = self.conn.prepare(
            "SELECT from_repo, from_id, to_repo, to_id, kind, confidence, source, source_hash
             FROM context_graph_edges
             WHERE (from_repo = ?1 AND from_id = ?2)
                OR (to_repo = ?1 AND to_id = ?2)
             ORDER BY from_id ASC, to_id ASC, kind ASC",
        )?;

        let start_key = node_key(locator.repo_id, locator.as_str());
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(start_key);
        let mut frontier: VecDeque<(RepoId, String, u32)> = VecDeque::new();
        frontier.push_back((locator.repo_id, locator.locator.clone(), 0));

        let mut seen_edges: BTreeSet<(String, String, GraphEdgeKind)> = BTreeSet::new();
        let mut out = Vec::new();
        let mut steps = 0usize;

        while let Some((repo, id, depth)) = frontier.pop_front() {
            steps = steps.saturating_add(1);
            if steps.is_multiple_of(CANCEL_STRIDE) {
                check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
            }
            if depth >= hops || out.len() as u32 >= limit {
                continue;
            }

            let repo_s = repo.to_string();
            let mut rows = stmt.query(params![repo_s, id])?;
            while let Some(row) = rows.next()? {
                steps = steps.saturating_add(1);
                if steps.is_multiple_of(CANCEL_STRIDE) {
                    check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
                }
                let edge = edge_from_row(row)?;
                let edge_key = (
                    format!("{}:{}", edge.from.repo_id, edge.from.locator),
                    format!("{}:{}", edge.to.repo_id, edge.to.locator),
                    edge.kind,
                );
                if !seen_edges.insert(edge_key) {
                    continue;
                }
                let other = if edge.from.repo_id == repo && edge.from.locator == id {
                    edge.to.clone()
                } else {
                    edge.from.clone()
                };
                out.push(edge);
                if out.len() as u32 >= limit {
                    break;
                }
                if depth + 1 >= hops {
                    continue;
                }
                let other_key = node_key(other.repo_id, other.as_str());
                if visited.contains(&other_key) {
                    continue;
                }
                if visited.len() >= max_nodes {
                    continue;
                }
                visited.insert(other_key);
                frontier.push_back((other.repo_id, other.locator, depth + 1));
            }
            if out.len() as u32 >= limit {
                break;
            }
        }

        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        Ok(out)
    }

    /// Incoming-only BFS: callers/importers that depend on `locator`.
    ///
    /// Hop and result caps match [`Self::neighbors`]. Outgoing callees are
    /// not expanded.
    pub fn impact(
        &self,
        locator: &SymbolLocator,
        hops: u32,
        limit: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let started = Instant::now();
        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        if limit == 0 {
            return Err(GraphError::InvalidLimit);
        }
        validate_locator(locator.as_str())?;
        if hops == 0 {
            return Ok(Vec::new());
        }

        let hops = hops.min(self.limits.max_hops);
        let limit = limit.min(self.limits.max_results);
        let max_nodes = self.limits.max_nodes as usize;

        let mut stmt = self.conn.prepare(
            "SELECT from_repo, from_id, to_repo, to_id, kind, confidence, source, source_hash
             FROM context_graph_edges
             WHERE to_repo = ?1 AND to_id = ?2
             ORDER BY from_id ASC, to_id ASC, kind ASC",
        )?;

        let start_key = node_key(locator.repo_id, locator.as_str());
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(start_key);
        let mut frontier: VecDeque<(RepoId, String, u32)> = VecDeque::new();
        frontier.push_back((locator.repo_id, locator.locator.clone(), 0));
        if let Ok((name, fq)) = self.conn.query_row(
            "SELECT name, fq_name FROM context_graph_symbols
             WHERE repo_id = ?1 AND locator = ?2",
            params![locator.repo_id.to_string(), locator.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ) {
            for alias in [name_locator(primary_name(&name)), fq_locator(&fq)] {
                if alias.is_empty() {
                    continue;
                }
                let key = node_key(locator.repo_id, &alias);
                if visited.insert(key) {
                    frontier.push_back((locator.repo_id, alias, 0));
                }
            }
        }

        let mut seen_edges: BTreeSet<(String, String, GraphEdgeKind)> = BTreeSet::new();
        let mut out = Vec::new();
        let mut steps = 0usize;

        while let Some((repo, id, depth)) = frontier.pop_front() {
            steps = steps.saturating_add(1);
            if steps.is_multiple_of(CANCEL_STRIDE) {
                check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
            }
            if depth >= hops || out.len() as u32 >= limit {
                continue;
            }

            let repo_s = repo.to_string();
            let mut targets = vec![id.clone()];
            if let Ok((name, fq)) = self.conn.query_row(
                "SELECT name, fq_name FROM context_graph_symbols
                 WHERE repo_id = ?1 AND locator = ?2",
                params![repo_s, id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            ) {
                for alias in [name_locator(primary_name(&name)), fq_locator(&fq)] {
                    if !alias.is_empty() {
                        targets.push(alias);
                    }
                }
            }
            for target in targets {
                let mut rows = stmt.query(params![repo_s, target])?;
                while let Some(row) = rows.next()? {
                    steps = steps.saturating_add(1);
                    if steps.is_multiple_of(CANCEL_STRIDE) {
                        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
                    }
                    let edge = edge_from_row(row)?;
                    let edge_key = (
                        format!("{}:{}", edge.from.repo_id, edge.from.locator),
                        format!("{}:{}", edge.to.repo_id, edge.to.locator),
                        edge.kind,
                    );
                    if !seen_edges.insert(edge_key) {
                        continue;
                    }
                    let caller = edge.from.clone();
                    out.push(edge);
                    if out.len() as u32 >= limit {
                        break;
                    }
                    if depth + 1 >= hops {
                        continue;
                    }
                    let caller_key = node_key(caller.repo_id, caller.as_str());
                    if visited.contains(&caller_key) {
                        continue;
                    }
                    if visited.len() >= max_nodes {
                        continue;
                    }
                    visited.insert(caller_key);
                    frontier.push_back((caller.repo_id, caller.locator, depth + 1));
                }
                if out.len() as u32 >= limit {
                    break;
                }
            }
            if out.len() as u32 >= limit {
                break;
            }
        }

        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        Ok(out)
    }

    /// Symbols defined in `path` (Modbit `CTX-017` Next-Edit-Ripple: the
    /// step that resolves a file-level edit to the symbol(s) [`Self::impact`]
    /// actually needs). A direct indexed lookup, not a traversal — no
    /// hop/node budget applies.
    pub fn symbols_at_path(
        &self,
        repo_id: RepoId,
        path: &RepoPath,
    ) -> Result<Vec<SymbolLocator>, GraphError> {
        let started = Instant::now();
        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        let mut stmt = self.conn.prepare(
            "SELECT locator FROM context_graph_symbols
             WHERE repo_id = ?1 AND path = ?2
             ORDER BY locator ASC",
        )?;
        let repo_s = repo_id.to_string();
        let mut rows = stmt.query(params![repo_s, path.as_str()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let locator: String = row.get(0)?;
            out.push(SymbolLocator::new(repo_id, locator)?);
        }
        check_bounds(&self.limits.cancel, started, self.limits.timeout)?;
        Ok(out)
    }

    /// Best-effort display label (fully-qualified name, defining path) for
    /// one locator. [`Self::impact`]'s edges carry only opaque
    /// [`SymbolLocator`]s; this resolves one back to something a caller can
    /// show a user. `None` on any lookup failure or unindexed locator —
    /// display is advisory, never a hard error.
    pub fn symbol_label(&self, locator: &SymbolLocator) -> Option<(String, RepoPath)> {
        let repo_s = locator.repo_id().to_string();
        let row: Result<(String, String), _> = self.conn.query_row(
            "SELECT fq_name, path FROM context_graph_symbols
             WHERE repo_id = ?1 AND locator = ?2",
            params![repo_s, locator.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
        let (fq_name, path) = row.ok()?;
        let path = RepoPath::parse(&path).ok()?;
        Some((fq_name, path))
    }

    #[cfg(test)]
    fn fail_next_commit(&self) {
        self.fail_before_commit.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn symbol_count(&self) -> Result<u64, GraphError> {
        let n: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM context_graph_symbols", [], |row| {
                    row.get(0)
                })?;
        u64::try_from(n).map_err(|_| GraphError::Corrupt("negative symbol count"))
    }

    #[cfg(test)]
    fn edge_count(&self) -> Result<u64, GraphError> {
        let n: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM context_graph_edges", [], |row| {
                    row.get(0)
                })?;
        u64::try_from(n).map_err(|_| GraphError::Corrupt("negative edge count"))
    }
}

impl GraphError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::InvalidLocator => "invalid_locator",
            Self::InvalidDocument => "invalid_document",
            Self::InvalidPolicy => "invalid_policy",
            Self::InvalidLimit => "invalid_limit",
            Self::TooManySymbols => "too_many_symbols",
            Self::TooManyEdges => "too_many_edges",
            Self::IndexTooLarge => "index_too_large",
            Self::NotCommitted => "not_committed",
            Self::Corrupt(_) => "corrupt",
            Self::Sqlite(_) => "sqlite",
            Self::Io(_) => "io",
        }
    }
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt(_) => f.write_str("corrupt code graph"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::Io(_) => f.write_str("io error"),
            other => f.write_str(other.as_str()),
        }
    }
}

impl Error for GraphError {}

impl From<rusqlite::Error> for GraphError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<std::io::Error> for GraphError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl fmt::Debug for CodeGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodeGraph").finish_non_exhaustive()
    }
}

fn configure_connection(conn: &Connection, require_wal: bool) -> Result<(), GraphError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if require_wal {
        let journal_mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(GraphError::Corrupt("journal_mode is not wal"));
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
    }
    conn.pragma_update(None, "foreign_keys", 1)?;
    Ok(())
}

fn ensure_schema(conn: &Connection) -> Result<(), GraphError> {
    conn.execute_batch(SCHEMA_SYMBOLS)?;
    conn.execute_batch(SCHEMA_EDGES)?;
    conn.execute_batch(SCHEMA_INDEXES)?;
    Ok(())
}

fn validate_locator(locator: &str) -> Result<(), GraphError> {
    if locator.is_empty() || locator.len() > MAX_LOCATOR_BYTES || locator.contains('\0') {
        return Err(GraphError::InvalidLocator);
    }
    Ok(())
}

fn validate_document(document: &GraphDocument, limits: &GraphLimits) -> Result<(), GraphError> {
    if document.symbols.len() > limits.max_symbols {
        return Err(GraphError::TooManySymbols);
    }
    let mut seen = BTreeSet::new();
    for symbol in &document.symbols {
        validate_locator(symbol.locator())?;
        if symbol.name().is_empty() || symbol.fq_name().is_empty() {
            return Err(GraphError::InvalidDocument);
        }
        if symbol.range().end_byte() < symbol.range().start_byte() {
            return Err(GraphError::InvalidDocument);
        }
        if !seen.insert(symbol.locator().to_string()) {
            return Err(GraphError::InvalidDocument);
        }
    }
    Ok(())
}

fn file_locator(path: &RepoPath) -> String {
    let mut out = String::with_capacity(path.as_str().len() + 1 + FILE_KIND.len());
    out.push_str(path.as_str());
    out.push('#');
    out.push_str(FILE_KIND);
    out
}

fn name_locator(name: &str) -> String {
    let mut out = String::with_capacity(NAME_PREFIX.len() + name.len());
    out.push_str(NAME_PREFIX);
    out.push_str(name);
    out
}

fn fq_locator(fq_name: &str) -> String {
    let mut out = String::with_capacity(FQ_PREFIX.len() + fq_name.len());
    out.push_str(FQ_PREFIX);
    out.push_str(fq_name);
    out
}

fn primary_name(raw: &str) -> &str {
    let trimmed = raw
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`');
    trimmed
        .rsplit([':', '.', '/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(trimmed)
}

fn is_definition(kind: SymbolKind) -> bool {
    !matches!(kind, SymbolKind::Import | SymbolKind::Reference)
}

fn range_contains(outer: SourceRange, inner: SourceRange) -> bool {
    outer.start_byte() <= inner.start_byte() && outer.end_byte() >= inner.end_byte()
}

fn range_span(range: SourceRange) -> u32 {
    range.end_byte().saturating_sub(range.start_byte())
}

fn find_container<'a>(defs: &[&'a SymbolRecord], child: &SymbolRecord) -> Option<&'a SymbolRecord> {
    defs.iter()
        .copied()
        .filter(|candidate| candidate.locator() != child.locator())
        .filter(|candidate| range_contains(candidate.range(), child.range()))
        .min_by_key(|candidate| (range_span(candidate.range()), candidate.locator()))
}

fn resolve_same_doc<'a>(
    defs: &[&'a SymbolRecord],
    name: &str,
    fq_name: &str,
) -> Option<&'a SymbolRecord> {
    if let Some(exact) = defs
        .iter()
        .copied()
        .filter(|d| d.fq_name() == fq_name)
        .min_by_key(|d| d.locator())
    {
        return Some(exact);
    }
    defs.iter()
        .copied()
        .filter(|d| d.name() == name || primary_name(d.name()) == name)
        .min_by_key(|d| d.locator())
}

fn plan_edges(
    document: &GraphDocument,
    limits: &GraphLimits,
) -> Result<Vec<PlannedEdge>, GraphError> {
    let defs: Vec<&SymbolRecord> = document
        .symbols
        .iter()
        .filter(|s| is_definition(s.kind()))
        .collect();
    let file_id = file_locator(&document.path);
    let mut planned = Vec::new();
    let mut seen = BTreeSet::new();

    let mut push = |from: String, to: String, kind: GraphEdgeKind, confidence: f32| {
        if from == to || from.is_empty() || to.is_empty() {
            return;
        }
        if from.len() > MAX_LOCATOR_BYTES || to.len() > MAX_LOCATOR_BYTES {
            return;
        }
        let key = EdgeKey {
            from: from.clone(),
            to: to.clone(),
            kind,
        };
        if !seen.insert(key) {
            return;
        }
        planned.push(PlannedEdge {
            from,
            to,
            kind,
            confidence,
        });
    };

    for symbol in &document.symbols {
        let locator = symbol.locator().to_string();
        match symbol.kind() {
            SymbolKind::Import => {
                let parent = find_container(&defs, symbol)
                    .map(|s| s.locator().to_string())
                    .unwrap_or_else(|| file_id.clone());
                push(
                    parent.clone(),
                    locator.clone(),
                    GraphEdgeKind::Import,
                    CONF_IMPORT,
                );
                let primary = primary_name(symbol.name());
                if !primary.is_empty() {
                    push(
                        parent.clone(),
                        name_locator(primary),
                        GraphEdgeKind::Import,
                        CONF_IMPORT,
                    );
                    push(
                        locator.clone(),
                        name_locator(primary),
                        GraphEdgeKind::Import,
                        CONF_IMPORT,
                    );
                }
                if symbol.fq_name() != primary && !symbol.fq_name().is_empty() {
                    push(
                        parent,
                        fq_locator(symbol.fq_name()),
                        GraphEdgeKind::Import,
                        CONF_FQ,
                    );
                }
            }
            SymbolKind::Reference => {
                let parent = find_container(&defs, symbol)
                    .map(|s| s.locator().to_string())
                    .unwrap_or_else(|| file_id.clone());
                if let Some(resolved) = resolve_same_doc(&defs, symbol.name(), symbol.fq_name()) {
                    push(
                        parent.clone(),
                        resolved.locator().to_string(),
                        GraphEdgeKind::Reference,
                        CONF_REFERENCE_RESOLVED,
                    );
                }
                push(
                    parent.clone(),
                    locator.clone(),
                    GraphEdgeKind::Reference,
                    CONF_REFERENCE_NAME,
                );
                let primary = primary_name(symbol.name());
                if !primary.is_empty() {
                    push(
                        parent,
                        name_locator(primary),
                        GraphEdgeKind::Reference,
                        CONF_REFERENCE_NAME,
                    );
                    push(
                        locator,
                        name_locator(primary),
                        GraphEdgeKind::Reference,
                        CONF_REFERENCE_NAME,
                    );
                }
            }
            _ => {
                let parent = find_container(&defs, symbol)
                    .map(|s| s.locator().to_string())
                    .unwrap_or_else(|| file_id.clone());
                push(
                    parent,
                    locator.clone(),
                    GraphEdgeKind::Definition,
                    CONF_DEFINITION,
                );
                let primary = primary_name(symbol.name());
                if !primary.is_empty() {
                    push(
                        locator.clone(),
                        name_locator(primary),
                        GraphEdgeKind::Definition,
                        CONF_DEFINITION,
                    );
                }
                if symbol.fq_name() != symbol.name() && !symbol.fq_name().is_empty() {
                    push(
                        locator,
                        fq_locator(symbol.fq_name()),
                        GraphEdgeKind::Definition,
                        CONF_FQ,
                    );
                }
            }
        }
    }

    if planned.len() > limits.max_edges {
        return Err(GraphError::TooManyEdges);
    }
    planned.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then_with(|| a.to.cmp(&b.to))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    Ok(planned)
}

fn replace_path(
    tx: &Transaction<'_>,
    document: &GraphDocument,
    planned: &[PlannedEdge],
    limits: &GraphLimits,
    started: Instant,
) -> Result<GraphWriteStats, GraphError> {
    let repo = document.repo_id.to_string();
    let path = document.path.as_str();
    check_bounds(&limits.cancel, started, limits.timeout)?;

    let removed_edges = delete_path_edges(tx, &repo, path)?;
    let removed_symbols = tx.execute(
        "DELETE FROM context_graph_symbols WHERE repo_id = ?1 AND path = ?2",
        params![repo, path],
    )? as u64;

    let source_hash = document.content_hash.to_string();
    let mut inserted_symbols = 0u64;
    if !document.symbols.is_empty() {
        insert_file_symbol(tx, document, &repo, path, &source_hash)?;
        inserted_symbols = inserted_symbols.saturating_add(1);
        for (step, symbol) in document.symbols.iter().enumerate() {
            if step.is_multiple_of(CANCEL_STRIDE) {
                check_bounds(&limits.cancel, started, limits.timeout)?;
            }
            insert_symbol(tx, symbol, &repo, path, &source_hash)?;
            inserted_symbols = inserted_symbols.saturating_add(1);
        }
    }

    let mut inserted_edges = 0u64;
    for (step, edge) in planned.iter().enumerate() {
        if step.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(&limits.cancel, started, limits.timeout)?;
        }
        tx.execute(
            "INSERT INTO context_graph_edges (
                from_repo, from_id, to_repo, to_id, kind, confidence, source, source_hash, path
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                repo,
                edge.from,
                repo,
                edge.to,
                edge.kind.as_str(),
                f64::from(edge.confidence),
                GraphEdgeSource::Syntax.as_str(),
                source_hash,
                path,
            ],
        )?;
        inserted_edges = inserted_edges.saturating_add(1);
    }

    Ok(GraphWriteStats {
        removed_symbols,
        inserted_symbols,
        removed_edges,
        inserted_edges,
    })
}

fn delete_path_edges(tx: &Transaction<'_>, repo: &str, path: &str) -> Result<u64, GraphError> {
    let produced = tx.execute(
        "DELETE FROM context_graph_edges WHERE from_repo = ?1 AND path = ?2",
        params![repo, path],
    )? as u64;
    let inbound = tx.execute(
        "DELETE FROM context_graph_edges
         WHERE (from_repo = ?1 AND from_id IN (
                 SELECT locator FROM context_graph_symbols WHERE repo_id = ?1 AND path = ?2
               ))
            OR (to_repo = ?1 AND to_id IN (
                 SELECT locator FROM context_graph_symbols WHERE repo_id = ?1 AND path = ?2
               ))",
        params![repo, path],
    )? as u64;
    Ok(produced.saturating_add(inbound))
}

fn insert_file_symbol(
    tx: &Transaction<'_>,
    document: &GraphDocument,
    repo: &str,
    path: &str,
    source_hash: &str,
) -> Result<(), GraphError> {
    let locator = file_locator(&document.path);
    let name = path.rsplit('/').next().unwrap_or(path);
    tx.execute(
        "INSERT INTO context_graph_symbols (
            repo_id, locator, path, kind, name, fq_name,
            start_byte, end_byte, start_line, end_line,
            container, signature, content_hash
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, 0, 0, NULL, NULL, ?7)",
        params![repo, locator, path, FILE_KIND, name, path, source_hash,],
    )?;
    Ok(())
}

fn insert_symbol(
    tx: &Transaction<'_>,
    symbol: &SymbolRecord,
    repo: &str,
    path: &str,
    source_hash: &str,
) -> Result<(), GraphError> {
    let range = symbol.range();
    tx.execute(
        "INSERT INTO context_graph_symbols (
            repo_id, locator, path, kind, name, fq_name,
            start_byte, end_byte, start_line, end_line,
            container, signature, content_hash
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            repo,
            symbol.locator(),
            path,
            symbol.kind().as_str(),
            symbol.name(),
            symbol.fq_name(),
            i64::from(range.start_byte()),
            i64::from(range.end_byte()),
            i64::from(range.start_line()),
            i64::from(range.end_line()),
            symbol.container(),
            symbol.signature(),
            source_hash,
        ],
    )?;
    Ok(())
}

fn ensure_index_budget(conn: &Transaction<'_>, max_bytes: u64) -> Result<(), GraphError> {
    if max_bytes == 0 {
        return Err(GraphError::IndexTooLarge);
    }
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let pages = u64::try_from(page_count).unwrap_or(u64::MAX);
    let size = u64::try_from(page_size).unwrap_or(u64::MAX);
    let bytes = pages.saturating_mul(size);
    if bytes > max_bytes {
        return Err(GraphError::IndexTooLarge);
    }
    Ok(())
}

fn check_bounds(
    cancel: &CancellationToken,
    started: Instant,
    timeout: Duration,
) -> Result<(), GraphError> {
    if cancel.is_cancelled() {
        return Err(GraphError::Cancelled);
    }
    if timeout.is_zero() || started.elapsed() > timeout {
        return Err(GraphError::Timeout);
    }
    Ok(())
}

fn node_key(repo: RepoId, locator: &str) -> String {
    let mut out = String::with_capacity(36 + 1 + locator.len());
    out.push_str(&repo.to_string());
    out.push(':');
    out.push_str(locator);
    out
}

fn parse_kind(raw: &str) -> Result<GraphEdgeKind, GraphError> {
    match raw {
        "definition" => Ok(GraphEdgeKind::Definition),
        "import" => Ok(GraphEdgeKind::Import),
        "reference" => Ok(GraphEdgeKind::Reference),
        _ => Err(GraphError::Corrupt("unknown edge kind")),
    }
}

fn parse_source(raw: &str) -> Result<GraphEdgeSource, GraphError> {
    match raw {
        "syntax" => Ok(GraphEdgeSource::Syntax),
        _ => Err(GraphError::Corrupt("unknown edge source")),
    }
}

fn edge_from_row(row: &rusqlite::Row<'_>) -> Result<GraphEdge, GraphError> {
    let from_repo_raw: String = row.get(0)?;
    let from_id: String = row.get(1)?;
    let to_repo_raw: String = row.get(2)?;
    let to_id: String = row.get(3)?;
    let kind_raw: String = row.get(4)?;
    let confidence: f64 = row.get(5)?;
    let source_raw: String = row.get(6)?;
    let source_hash: String = row.get(7)?;
    let from_repo =
        RepoId::from_str(&from_repo_raw).map_err(|_| GraphError::Corrupt("malformed from repo"))?;
    let to_repo =
        RepoId::from_str(&to_repo_raw).map_err(|_| GraphError::Corrupt("malformed to repo"))?;
    if source_hash.is_empty() {
        return Err(GraphError::Corrupt("empty source hash"));
    }
    if !(0.0..=1.0).contains(&confidence) {
        return Err(GraphError::Corrupt("confidence out of range"));
    }
    Ok(GraphEdge {
        from: SymbolLocator::new(from_repo, from_id)?,
        to: SymbolLocator::new(to_repo, to_id)?,
        kind: parse_kind(&kind_raw)?,
        confidence: confidence as f32,
        source: parse_source(&source_raw)?,
        source_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    use crate::parse::registry::{ParseBudget, ParserRegistry};
    use crate::parse::symbols::{SymbolBudget, extract_from_parse};

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn extract(language: SourceLanguage, rel: &str, src: &str) -> Vec<SymbolRecord> {
        let outcome = ParserRegistry::parse(language, src.as_bytes(), &ParseBudget::new());
        extract_from_parse(&outcome, &path(rel), src.as_bytes(), &SymbolBudget::new())
            .expect("extract")
    }

    fn document(repo: RepoId, rel: &str, language: SourceLanguage, src: &str) -> GraphDocument {
        let symbols = extract(language, rel, src);
        GraphDocument::from_symbols(
            repo,
            path(rel),
            ContentHash::from_bytes(src.as_bytes()),
            Some(language),
            symbols,
        )
    }

    fn open_mem() -> CodeGraph {
        CodeGraph::open_in_memory(GraphLimits::new()).expect("open memory graph")
    }

    fn temp_path() -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("rapidlm-graph-{}-{seq}.sqlite", std::process::id()))
    }

    fn locator_of(doc: &GraphDocument, kind: SymbolKind, name: &str) -> SymbolLocator {
        let symbol = doc
            .symbols()
            .iter()
            .find(|s| s.kind() == kind && s.name() == name)
            .unwrap_or_else(|| panic!("missing {kind:?} {name}"));
        SymbolLocator::from_symbol(doc.repo_id(), symbol).expect("locator")
    }

    fn edge_locators(edges: &[GraphEdge]) -> HashSet<String> {
        let mut out = HashSet::new();
        for edge in edges {
            out.insert(edge.from().as_str().to_string());
            out.insert(edge.to().as_str().to_string());
        }
        out
    }

    fn kinds_present(edges: &[GraphEdge]) -> HashSet<GraphEdgeKind> {
        edges.iter().map(GraphEdge::kind).collect()
    }

    #[test]
    fn neighbors_return_typed_edges_with_source_hashes() {
        let mut graph = open_mem();
        let repo = RepoId::new();
        let src = r#"
use std::fmt::Display;

pub struct Parser;

impl Parser {
    pub fn parse(&self) -> u32 {
        helper()
    }
}

fn helper() -> u32 { 1 }
"#;
        let doc = document(repo, "src/lib.rs", SourceLanguage::Rust, src);
        graph.upsert_document(&doc).expect("upsert");

        let parse = locator_of(&doc, SymbolKind::Method, "parse");
        let helper = locator_of(&doc, SymbolKind::Function, "helper");
        let edges = graph.neighbors(&parse, 2, 64).expect("neighbors");
        assert!(!edges.is_empty());
        assert!(kinds_present(&edges).contains(&GraphEdgeKind::Definition));
        assert!(kinds_present(&edges).contains(&GraphEdgeKind::Reference));
        assert!(
            edges.iter().any(|e| e.kind() == GraphEdgeKind::Import)
                || graph
                    .neighbors(
                        &SymbolLocator::new(repo, "src/lib.rs#file").expect("file"),
                        2,
                        64
                    )
                    .expect("file neighborhood")
                    .iter()
                    .any(|e| e.kind() == GraphEdgeKind::Import)
        );
        assert!(edge_locators(&edges).contains(helper.as_str()));
        let hash = doc.content_hash().to_string();
        assert!(
            edges
                .iter()
                .all(|e| { e.source() == GraphEdgeSource::Syntax && e.source_hash() == hash })
        );
    }

    #[test]
    fn stale_edges_are_removed_on_document_replacement() {
        let mut graph = open_mem();
        let repo = RepoId::new();
        let first = document(
            repo,
            "src/lib.rs",
            SourceLanguage::Rust,
            "fn keep() { helper(); }\nfn helper() {}\n",
        );
        let first_hash = first.content_hash().to_string();
        graph.upsert_document(&first).expect("first");
        let helper = locator_of(&first, SymbolKind::Function, "helper");
        let before = graph.neighbors(&helper, 2, 64).expect("before");
        assert!(before.iter().any(|e| e.source_hash() == first_hash));
        assert!(edge_locators(&before).iter().any(|l| l.contains("keep")));

        let second = document(
            repo,
            "src/lib.rs",
            SourceLanguage::Rust,
            "fn keep() { other(); }\nfn other() {}\n",
        );
        let stats = graph.upsert_document(&second).expect("replace");
        assert!(stats.removed_symbols() > 0);
        assert!(stats.removed_edges() > 0);
        assert!(stats.inserted_symbols() > 0);
        assert!(stats.inserted_edges() > 0);

        let gone = graph.neighbors(&helper, 4, 64).expect("old helper");
        assert!(gone.is_empty());
        let keep = locator_of(&second, SymbolKind::Function, "keep");
        let after = graph.neighbors(&keep, 2, 64).expect("after");
        let second_hash = second.content_hash().to_string();
        assert!(after.iter().all(|e| e.source_hash() == second_hash));
        assert!(!edge_locators(&after).iter().any(|l| l.contains("helper")));
        assert!(edge_locators(&after).iter().any(|l| l.contains("other")));
    }

    #[test]
    fn traversal_respects_hard_hop_budget() {
        let mut graph = open_mem();
        let repo = RepoId::new();
        let a = document(repo, "src/a.rs", SourceLanguage::Rust, "fn a() { b(); }\n");
        let b = document(repo, "src/b.rs", SourceLanguage::Rust, "fn b() { c(); }\n");
        let c = document(repo, "src/c.rs", SourceLanguage::Rust, "fn c() {}\n");
        graph.upsert_document(&a).expect("a");
        graph.upsert_document(&b).expect("b");
        graph.upsert_document(&c).expect("c");

        let a_fn = locator_of(&a, SymbolKind::Function, "a");
        let b_fn = locator_of(&b, SymbolKind::Function, "b");
        let c_fn = locator_of(&c, SymbolKind::Function, "c");

        let one = graph.neighbors(&a_fn, 1, 64).expect("1 hop");
        assert!(!edge_locators(&one).contains(b_fn.as_str()));
        assert!(!edge_locators(&one).contains(c_fn.as_str()));

        let two = graph.neighbors(&a_fn, 2, 64).expect("2 hops");
        assert!(edge_locators(&two).contains(b_fn.as_str()));
        assert!(!edge_locators(&two).contains(c_fn.as_str()));

        let four = graph.neighbors(&a_fn, 4, 64).expect("4 hops");
        assert!(edge_locators(&four).contains(c_fn.as_str()));

        let capped = CodeGraph::open_in_memory(GraphLimits::new().max_hops(1)).expect("capped");
        let mut capped = capped;
        capped.upsert_document(&a).expect("a");
        capped.upsert_document(&b).expect("b");
        let asked = capped.neighbors(&a_fn, 8, 64).expect("clamped hops");
        assert!(!edge_locators(&asked).contains(b_fn.as_str()));
    }

    #[test]
    fn traversal_respects_hard_node_budget() {
        let mut graph = CodeGraph::open_in_memory(GraphLimits::new().max_nodes(1)).expect("open");
        let repo = RepoId::new();
        let src = "fn a() { b(); c(); d(); }\nfn b() {}\nfn c() {}\nfn d() {}\n";
        let doc = document(repo, "src/star.rs", SourceLanguage::Rust, src);
        graph.upsert_document(&doc).expect("upsert");
        let a = locator_of(&doc, SymbolKind::Function, "a");
        let b = locator_of(&doc, SymbolKind::Function, "b");
        let edges = graph.neighbors(&a, 8, 64).expect("neighbors");
        assert!(!edges.is_empty());
        // Node budget still returns incident edges, but does not expand `b`.
        assert!(edges.iter().all(|e| e.from().as_str() != b.as_str()));
    }

    #[test]
    fn failed_reindex_leaves_prior_generation() {
        let mut graph = open_mem();
        let repo = RepoId::new();
        let first = document(repo, "src/lib.rs", SourceLanguage::Rust, "fn keep() {}\n");
        graph.upsert_document(&first).expect("first");
        let keep = locator_of(&first, SymbolKind::Function, "keep");
        let symbols_before = graph.symbol_count().expect("symbols");
        let edges_before = graph.edge_count().expect("edges");
        graph.fail_next_commit();
        let second = document(
            repo,
            "src/lib.rs",
            SourceLanguage::Rust,
            "fn must_not_land() {}\n",
        );
        let err = graph.upsert_document(&second).expect_err("not committed");
        assert!(matches!(err, GraphError::NotCommitted));
        assert_eq!(graph.symbol_count().expect("symbols"), symbols_before);
        assert_eq!(graph.edge_count().expect("edges"), edges_before);
        assert!(!graph.neighbors(&keep, 1, 16).expect("keep").is_empty());
        let missing = SymbolLocator::from_symbol(
            repo,
            second
                .symbols()
                .iter()
                .find(|s| s.name() == "must_not_land")
                .expect("new fn"),
        )
        .expect("locator");
        assert!(graph.neighbors(&missing, 1, 16).expect("new").is_empty());
    }

    #[test]
    fn empty_document_clears_path() {
        let mut graph = open_mem();
        let repo = RepoId::new();
        let first = document(
            repo,
            "src/lib.rs",
            SourceLanguage::Rust,
            "fn clear_me() {}\n",
        );
        graph.upsert_document(&first).expect("upsert");
        let empty = GraphDocument::new(
            repo,
            path("src/lib.rs"),
            ContentHash::from_bytes(b""),
            Some(SourceLanguage::Rust),
        );
        let stats = graph.upsert_document(&empty).expect("clear");
        assert!(stats.removed_symbols() > 0);
        assert_eq!(stats.inserted_symbols(), 0);
        assert_eq!(stats.inserted_edges(), 0);
        assert_eq!(graph.symbol_count().expect("symbols"), 0);
        assert_eq!(graph.edge_count().expect("edges"), 0);
    }

    #[test]
    fn cancellation_and_timeout_are_typed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut graph =
            CodeGraph::open_in_memory(GraphLimits::new().cancellation(cancel)).expect("open");
        let repo = RepoId::new();
        let doc = document(
            repo,
            "src/lib.rs",
            SourceLanguage::Rust,
            "fn cancel_me() {}\n",
        );
        assert!(matches!(
            graph.upsert_document(&doc).expect_err("cancel"),
            GraphError::Cancelled
        ));

        let mut timed =
            CodeGraph::open_in_memory(GraphLimits::new().timeout(Duration::ZERO)).expect("timed");
        assert!(matches!(
            timed.upsert_document(&doc).expect_err("timeout"),
            GraphError::Timeout
        ));
        let locator = SymbolLocator::new(repo, "src/lib.rs#function:cancel_me").expect("loc");
        assert!(matches!(
            timed.neighbors(&locator, 1, 8).expect_err("search timeout"),
            GraphError::Timeout
        ));
    }

    #[test]
    fn file_backed_graph_survives_reopen() {
        let path_buf = temp_path();
        let repo = RepoId::new();
        let doc = document(
            repo,
            "src/lib.rs",
            SourceLanguage::Rust,
            "fn persist() {}\n",
        );
        let persist = locator_of(&doc, SymbolKind::Function, "persist");
        {
            let mut graph = CodeGraph::open(&path_buf, GraphLimits::new()).expect("open");
            graph.upsert_document(&doc).expect("upsert");
        }
        let graph = CodeGraph::open(&path_buf, GraphLimits::new()).expect("reopen");
        let edges = graph.neighbors(&persist, 1, 16).expect("neighbors");
        assert!(!edges.is_empty());
        assert!(
            edges
                .iter()
                .all(|e| e.source_hash() == doc.content_hash().to_string())
        );
        let _ = std::fs::remove_file(&path_buf);
    }

    #[test]
    fn query_and_policy_bounds() {
        let graph = open_mem();
        let repo = RepoId::new();
        let locator = SymbolLocator::new(repo, "src/lib.rs#function:x").expect("loc");
        assert!(matches!(
            graph.neighbors(&locator, 1, 0).expect_err("limit"),
            GraphError::InvalidLimit
        ));
        assert!(matches!(
            SymbolLocator::new(repo, ""),
            Err(GraphError::InvalidLocator)
        ));
        assert!(matches!(
            CodeGraph::open_in_memory(GraphLimits::new().max_hops(0)),
            Err(GraphError::InvalidPolicy)
        ));
        assert!(matches!(
            CodeGraph::open_in_memory(GraphLimits::new().max_nodes(0)),
            Err(GraphError::InvalidPolicy)
        ));
    }

    #[test]
    fn cross_file_name_join_reaches_definition() {
        let mut graph = open_mem();
        let repo = RepoId::new();
        let caller = document(
            repo,
            "src/caller.rs",
            SourceLanguage::Rust,
            "fn caller() { helper(); }\n",
        );
        let def = document(
            repo,
            "src/helper.rs",
            SourceLanguage::Rust,
            "fn helper() {}\n",
        );
        graph.upsert_document(&caller).expect("caller");
        graph.upsert_document(&def).expect("def");
        let helper = locator_of(&def, SymbolKind::Function, "helper");
        let caller_fn = locator_of(&caller, SymbolKind::Function, "caller");
        let around_def = graph.neighbors(&helper, 2, 64).expect("from def");
        assert!(
            edge_locators(&around_def).contains(caller_fn.as_str())
                || edge_locators(&around_def)
                    .iter()
                    .any(|l| l == "#name:helper")
        );
        let around_caller = graph.neighbors(&caller_fn, 2, 64).expect("from caller");
        assert!(edge_locators(&around_caller).contains(helper.as_str()));
    }

    #[test]
    fn projection_is_deterministic_for_identical_source() {
        let repo = RepoId::new();
        let src = r#"
use std::fmt::Display;
pub struct Parser;
impl Parser {
    pub fn parse(&self) -> u32 { helper() }
}
fn helper() -> u32 { 1 }
"#;
        let mut first = open_mem();
        let mut second = open_mem();
        let doc_a = document(repo, "src/lib.rs", SourceLanguage::Rust, src);
        let doc_b = document(repo, "src/lib.rs", SourceLanguage::Rust, src);
        first.upsert_document(&doc_a).expect("first");
        second.upsert_document(&doc_b).expect("second");
        let parse = locator_of(&doc_a, SymbolKind::Method, "parse");
        let a = first.neighbors(&parse, 3, 64).expect("a");
        let b = second.neighbors(&parse, 3, 64).expect("b");
        let keys = |edges: &[GraphEdge]| {
            edges
                .iter()
                .map(|e| {
                    (
                        e.from().as_str().to_string(),
                        e.to().as_str().to_string(),
                        e.kind(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&a), keys(&b));
        assert!(!a.is_empty());
    }

    #[test]
    fn projection_is_independent_of_document_upsert_order() {
        let repo = RepoId::new();
        let caller = document(
            repo,
            "src/caller.rs",
            SourceLanguage::Rust,
            "fn caller() { helper(); }\n",
        );
        let def = document(
            repo,
            "src/helper.rs",
            SourceLanguage::Rust,
            "fn helper() {}\n",
        );
        let mut ab = open_mem();
        ab.upsert_document(&caller).expect("caller first");
        ab.upsert_document(&def).expect("def second");
        let mut ba = open_mem();
        ba.upsert_document(&def).expect("def first");
        ba.upsert_document(&caller).expect("caller second");

        let helper = locator_of(&def, SymbolKind::Function, "helper");
        let caller_fn = locator_of(&caller, SymbolKind::Function, "caller");
        let from_ab = edge_locators(&ab.neighbors(&caller_fn, 2, 64).expect("ab"));
        let from_ba = edge_locators(&ba.neighbors(&caller_fn, 2, 64).expect("ba"));
        assert_eq!(from_ab, from_ba);
        assert!(from_ab.contains(helper.as_str()));
    }

    #[test]
    fn impact_returns_incoming_callers_not_callees() {
        let mut graph = open_mem();
        let repo = RepoId::new();
        let a = document(repo, "src/a.rs", SourceLanguage::Rust, "fn a() { b(); }\n");
        let b = document(repo, "src/b.rs", SourceLanguage::Rust, "fn b() { c(); }\n");
        let c = document(repo, "src/c.rs", SourceLanguage::Rust, "fn c() {}\n");
        graph.upsert_document(&a).expect("a");
        graph.upsert_document(&b).expect("b");
        graph.upsert_document(&c).expect("c");

        let a_fn = locator_of(&a, SymbolKind::Function, "a");
        let b_fn = locator_of(&b, SymbolKind::Function, "b");
        let c_fn = locator_of(&c, SymbolKind::Function, "c");

        let impact_c = graph.impact(&c_fn, 1, 64).expect("impact c");
        assert!(edge_locators(&impact_c).contains(b_fn.as_str()));
        assert!(
            !edge_locators(&impact_c).contains(a_fn.as_str()),
            "one hop of incoming from c reaches b, not a"
        );
        let impact_two = graph.impact(&c_fn, 2, 64).expect("impact 2 hops");
        assert!(edge_locators(&impact_two).contains(a_fn.as_str()));

        let impact_a = graph.impact(&a_fn, 4, 64).expect("impact a");
        assert!(
            !edge_locators(&impact_a).contains(b_fn.as_str()),
            "impact must not follow outgoing callees"
        );
        assert!(matches!(
            graph.impact(&c_fn, 1, 0).expect_err("limit"),
            GraphError::InvalidLimit
        ));
    }

    #[test]
    fn symbols_at_path_finds_only_that_files_symbols() {
        let mut graph = open_mem();
        let repo = RepoId::new();
        let a = document(repo, "src/a.rs", SourceLanguage::Rust, "fn a() { b(); }\n");
        let b = document(repo, "src/b.rs", SourceLanguage::Rust, "fn b() {}\n");
        graph.upsert_document(&a).expect("a");
        graph.upsert_document(&b).expect("b");

        let a_fn = locator_of(&a, SymbolKind::Function, "a");
        let b_fn = locator_of(&b, SymbolKind::Function, "b");
        let found = graph
            .symbols_at_path(repo, &path("src/a.rs"))
            .expect("lookup");
        assert!(found.contains(&a_fn), "{found:?}");
        assert!(!found.contains(&b_fn), "b.rs's symbols must not leak in");

        let none = graph
            .symbols_at_path(repo, &path("src/missing.rs"))
            .expect("lookup missing");
        assert!(none.is_empty());
    }

    #[test]
    fn symbol_label_resolves_next_edit_ripple_end_to_end() {
        let mut graph = open_mem();
        let repo = RepoId::new();
        let a = document(repo, "src/a.rs", SourceLanguage::Rust, "fn a() { b(); }\n");
        let b = document(repo, "src/b.rs", SourceLanguage::Rust, "fn b() {}\n");
        graph.upsert_document(&a).expect("a");
        graph.upsert_document(&b).expect("b");

        // The Next-Edit-Ripple flow end to end: given the file just edited
        // (b.rs), find its symbols, then find what would be impacted by
        // changing them, then resolve those impacted symbols to a
        // human-showable (name, path).
        let symbols = graph
            .symbols_at_path(repo, &path("src/b.rs"))
            .expect("lookup");
        let b_fn = locator_of(&b, SymbolKind::Function, "b");
        assert!(symbols.contains(&b_fn), "{symbols:?}");
        let impacted = graph.impact(&b_fn, 1, 64).expect("impact");
        let labelled: Vec<(String, RepoPath)> = impacted
            .iter()
            .filter_map(|edge| graph.symbol_label(edge.from()))
            .collect();
        assert!(
            labelled
                .iter()
                .any(|(name, p)| name == "a" && *p == path("src/a.rs")),
            "{labelled:?}"
        );

        assert!(
            graph
                .symbol_label(&locator_of(&a, SymbolKind::Function, "a"))
                .is_some()
        );
        let bogus = SymbolLocator::new(repo, "nope").expect("locator");
        assert!(graph.symbol_label(&bogus).is_none());
    }

    #[test]
    fn hops_zero_returns_no_edges() {
        let mut graph = open_mem();
        let repo = RepoId::new();
        let doc = document(repo, "src/lib.rs", SourceLanguage::Rust, "fn only() {}\n");
        graph.upsert_document(&doc).expect("upsert");
        let only = locator_of(&doc, SymbolKind::Function, "only");
        let edges = graph.neighbors(&only, 0, 8).expect("zero hops");
        assert!(edges.is_empty());
    }
}
