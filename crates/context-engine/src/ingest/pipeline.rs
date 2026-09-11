//! Incremental hash→parse→chunk→index transaction.
//!
//! One source path is one generation. Canonical inventory advances only after
//! FTS, graph, and vector writes succeed. A crash between those stages leaves
//! a pending generation that can be replayed; mixed index caches are not the
//! committed view. `index_file` is a no-op when the committed content hash
//! already matches.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use protocol::{RedactionClass, RepoId, RepoPath};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::chunk::{ChunkError, ChunkPolicy, Document, chunk};
use crate::index::fts::{FtsDocument, FtsError, FtsIndex, FtsLimits};
use crate::index::graph::{CodeGraph, GraphDocument, GraphError, GraphLimits};
use crate::index::vector::{
    EmbeddingProvider, VectorDocument, VectorError, VectorIndex, VectorLimits,
};
use crate::ingest::content::{
    ContentClass, ContentError, ContentHash, ContentLimits, LoadedCandidate, SourceLanguage,
    load_file_candidate,
};
use crate::ingest::walk::{FileCandidate, WalkError, WalkLimits, walk_repo};
use crate::parse::registry::{ParseBudget, ParserRegistry};
use crate::parse::symbols::{SymbolBudget, SymbolError, extract_from_parse};
use crate::repo_manifest::{CancellationToken, RepoSpec, WorkspaceManifest};

/// Default wall-clock budget for one file transaction, including recovery.
pub const DEFAULT_PIPELINE_TIMEOUT: Duration = Duration::from_secs(15);

/// Bounded SQLite lock wait. Matches the ledger busy-timeout recovery rule.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

const FAIL_NONE: u8 = 0;
const FAIL_AFTER_FTS: u8 = 1;
const FAIL_AFTER_GRAPH: u8 = 2;

const STATUS_PENDING: &str = "pending";
const STATUS_COMMITTED: &str = "committed";
const STATUS_ABORTED: &str = "aborted";

const INVENTORY_NAME: &str = "inventory.sqlite";
const FTS_NAME: &str = "fts.sqlite";
const GRAPH_NAME: &str = "graph.sqlite";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS context_file_canonical (
  repo_id TEXT NOT NULL,
  path TEXT NOT NULL,
  content_hash TEXT NOT NULL,
  generation INTEGER NOT NULL,
  language TEXT,
  class TEXT NOT NULL,
  PRIMARY KEY (repo_id, path)
);
CREATE TABLE IF NOT EXISTS context_index_generations (
  repo_id TEXT NOT NULL,
  path TEXT NOT NULL,
  generation INTEGER NOT NULL,
  content_hash TEXT NOT NULL,
  status TEXT NOT NULL,
  PRIMARY KEY (repo_id, path, generation)
);
CREATE INDEX IF NOT EXISTS context_index_generations_pending
  ON context_index_generations(status);
";

/// Per-pipeline resource bounds. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct PipelineLimits {
    content: ContentLimits,
    walk: WalkLimits,
    parse: ParseBudget,
    symbols: SymbolBudget,
    chunk: ChunkPolicy,
    fts: FtsLimits,
    graph: GraphLimits,
    vector: VectorLimits,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Durable incremental indexer over one workspace manifest.
pub struct IndexPipeline {
    manifest: WorkspaceManifest,
    inventory: Connection,
    fts: FtsIndex,
    graph: CodeGraph,
    vectors: VectorIndex,
    embedder: Option<Box<dyn EmbeddingProvider>>,
    limits: PipelineLimits,
    fail_after: AtomicU8,
}

/// Result of one `index_file` / `remove_file` attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexOutcome {
    /// Canonical hash already matches; no parse or index write ran.
    Unchanged {
        generation: u64,
        content_hash: ContentHash,
    },
    /// A generation was prepared and committed (or recovered) for this path.
    Applied {
        generation: u64,
        content_hash: ContentHash,
        class: ContentClass,
        chunks: u64,
        symbols: u64,
    },
}

/// Whether a stored generation is the committed view or still recoverable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GenerationStatus {
    Pending,
    Committed,
}

/// Inventory row for one repo path. `content_hash` is the `sha256:` wire form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileIndexState {
    repo_id: RepoId,
    path: RepoPath,
    content_hash: String,
    generation: u64,
    status: GenerationStatus,
}

/// Typed pipeline failure. Display never echoes host or repository paths.
#[derive(Debug)]
pub enum PipelineError {
    Cancelled,
    Timeout,
    UnknownRepo,
    InvalidPolicy,
    /// Write stages ran but the generation was not committed.
    NotCommitted,
    Walk(WalkError),
    Content(ContentError),
    Chunk(ChunkError),
    Symbol(SymbolError),
    Fts(FtsError),
    Graph(GraphError),
    Vector(VectorError),
    Corrupt(&'static str),
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

struct InventoryRow {
    repo_id: RepoId,
    path: RepoPath,
    content_hash: String,
    generation: u64,
    status: GenerationStatus,
}

struct PreparedFile {
    content_hash: ContentHash,
    class: ContentClass,
    language: Option<SourceLanguage>,
    fts: FtsDocument,
    graph: GraphDocument,
    vector: VectorDocument,
    chunks: u64,
    symbols: u64,
}

impl PipelineLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value.clone();
        self.parse = self.parse.cancellation(value.clone());
        self.symbols = self.symbols.cancellation(value.clone());
        self.chunk = self.chunk.cancellation(value.clone());
        self.fts = self.fts.cancellation(value.clone());
        self.graph = self.graph.cancellation(value.clone());
        self.vector = self.vector.cancellation(value);
        self
    }

    pub fn content(mut self, value: ContentLimits) -> Self {
        self.content = value;
        self
    }

    pub fn walk(mut self, value: WalkLimits) -> Self {
        self.walk = value;
        self
    }

    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl Default for PipelineLimits {
    fn default() -> Self {
        let cancel = CancellationToken::new();
        Self {
            content: ContentLimits::default(),
            walk: WalkLimits::default(),
            parse: ParseBudget::new().cancellation(cancel.clone()),
            symbols: SymbolBudget::new().cancellation(cancel.clone()),
            chunk: ChunkPolicy::new().cancellation(cancel.clone()),
            fts: FtsLimits::new().cancellation(cancel.clone()),
            graph: GraphLimits::new().cancellation(cancel.clone()),
            vector: VectorLimits::new().cancellation(cancel.clone()),
            timeout: DEFAULT_PIPELINE_TIMEOUT,
            cancel,
        }
    }
}

impl IndexOutcome {
    pub fn is_unchanged(&self) -> bool {
        matches!(self, Self::Unchanged { .. })
    }

    pub fn generation(&self) -> u64 {
        match self {
            Self::Unchanged { generation, .. } | Self::Applied { generation, .. } => *generation,
        }
    }

    pub fn content_hash(&self) -> ContentHash {
        match self {
            Self::Unchanged { content_hash, .. } | Self::Applied { content_hash, .. } => {
                *content_hash
            }
        }
    }
}

impl GenerationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => STATUS_PENDING,
            Self::Committed => STATUS_COMMITTED,
        }
    }
}

impl FileIndexState {
    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn status(&self) -> GenerationStatus {
        self.status
    }
}

impl IndexPipeline {
    /// Open (or create) a durable pipeline under `dir`.
    pub fn open(
        dir: impl AsRef<Path>,
        manifest: WorkspaceManifest,
        limits: PipelineLimits,
    ) -> Result<Self, PipelineError> {
        let dir = dir.as_ref();
        if dir.as_os_str().is_empty() {
            return Err(PipelineError::InvalidPolicy);
        }
        std::fs::create_dir_all(dir)?;
        let inventory = Connection::open(dir.join(INVENTORY_NAME))?;
        configure_connection(&inventory, true)?;
        ensure_schema(&inventory)?;
        let fts = FtsIndex::open(dir.join(FTS_NAME), limits.fts.clone())?;
        let graph = CodeGraph::open(dir.join(GRAPH_NAME), limits.graph.clone())?;
        Ok(Self {
            manifest,
            inventory,
            fts,
            graph,
            vectors: VectorIndex::disabled(),
            embedder: None,
            limits,
            fail_after: AtomicU8::new(FAIL_NONE),
        })
    }

    /// Process-private in-memory pipeline. Rebuildable; not durable.
    pub fn open_in_memory(
        manifest: WorkspaceManifest,
        limits: PipelineLimits,
    ) -> Result<Self, PipelineError> {
        let inventory = Connection::open_in_memory()?;
        configure_connection(&inventory, false)?;
        ensure_schema(&inventory)?;
        let fts = FtsIndex::open_in_memory(limits.fts.clone())?;
        let graph = CodeGraph::open_in_memory(limits.graph.clone())?;
        Ok(Self {
            manifest,
            inventory,
            fts,
            graph,
            vectors: VectorIndex::disabled(),
            embedder: None,
            limits,
            fail_after: AtomicU8::new(FAIL_NONE),
        })
    }

    /// Attach a rebuildable vector cache. Absence of an embedder leaves vectors disabled.
    pub fn set_vector_index(&mut self, index: VectorIndex, provider: Box<dyn EmbeddingProvider>) {
        self.vectors = index;
        self.embedder = Some(provider);
    }

    pub fn manifest(&self) -> &WorkspaceManifest {
        &self.manifest
    }

    pub fn fts(&self) -> &FtsIndex {
        &self.fts
    }

    pub fn graph(&self) -> &CodeGraph {
        &self.graph
    }

    pub fn vectors(&self) -> &VectorIndex {
        &self.vectors
    }

    /// Hash, parse, chunk, and update FTS/graph/vector for `candidate`.
    ///
    /// No-ops when the committed content hash already matches this file.
    pub fn index_file(&mut self, candidate: &FileCandidate) -> Result<IndexOutcome, PipelineError> {
        let started = Instant::now();
        self.check_bounds(started)?;
        let spec = self.repo(candidate.repo_id())?;
        let loaded = load_file_candidate(
            spec.root().as_path(),
            candidate,
            &self.limits.content,
            &self.limits.cancel,
        )?;
        self.check_bounds(started)?;
        self.ingest_loaded(candidate.repo_id(), loaded, started)
    }

    /// Remove a path from every index and drop the canonical inventory row.
    pub fn remove_file(
        &mut self,
        repo_id: RepoId,
        path: &RepoPath,
    ) -> Result<IndexOutcome, PipelineError> {
        let started = Instant::now();
        self.check_bounds(started)?;
        let _ = self.repo(repo_id)?;
        let prepared = PreparedFile::empty(repo_id, path.clone(), ContentHash::from_bytes(b""));
        let hash = prepared.content_hash;
        if self.canonical(repo_id, path)?.is_none() && self.pending(repo_id, path)?.is_none() {
            return Ok(IndexOutcome::Unchanged {
                generation: 0,
                content_hash: hash,
            });
        }
        let generation = self.begin_generation(repo_id, path, &hash.to_string())?;
        match self.apply_prepared(&prepared, started) {
            Ok(()) => {
                self.commit_removal(repo_id, path, generation)?;
                Ok(IndexOutcome::Applied {
                    generation,
                    content_hash: hash,
                    class: ContentClass::MetadataOnly(
                        crate::ingest::walk::MetadataOnlyReason::Binary,
                    ),
                    chunks: 0,
                    symbols: 0,
                })
            }
            Err(err) => Err(err),
        }
    }

    /// Replay every pending generation. Missing files become removals.
    pub fn recover(&mut self) -> Result<u32, PipelineError> {
        let started = Instant::now();
        self.check_bounds(started)?;
        let pending = self.all_pending()?;
        let mut recovered = 0u32;
        for row in pending {
            self.check_bounds(started)?;
            match self.manifest.repo_by_id(row.repo_id) {
                None => {
                    self.abort_pending(row.repo_id, &row.path)?;
                }
                Some(spec) => match self.find_candidate(spec, &row.path, started)? {
                    Some(candidate) => {
                        let _ = self.index_file(&candidate)?;
                        recovered = recovered.saturating_add(1);
                    }
                    None => {
                        self.complete_missing(&row, started)?;
                        recovered = recovered.saturating_add(1);
                    }
                },
            }
        }
        Ok(recovered)
    }

    /// Last committed inventory row, if any. Pending generations are excluded.
    pub fn canonical_state(
        &self,
        repo_id: RepoId,
        path: &RepoPath,
    ) -> Result<Option<FileIndexState>, PipelineError> {
        Ok(self.canonical(repo_id, path)?.map(FileIndexState::from))
    }

    /// Open recoverable generation for this path, if indexing crashed mid-write.
    pub fn pending_state(
        &self,
        repo_id: RepoId,
        path: &RepoPath,
    ) -> Result<Option<FileIndexState>, PipelineError> {
        Ok(self.pending(repo_id, path)?.map(FileIndexState::from))
    }

    fn ingest_loaded(
        &mut self,
        repo_id: RepoId,
        loaded: LoadedCandidate,
        started: Instant,
    ) -> Result<IndexOutcome, PipelineError> {
        let hash = loaded.content_hash();
        let hash_wire = hash.to_string();
        let path = loaded.path().clone();
        if let Some(canonical) = self.canonical(repo_id, &path)?
            && canonical.content_hash == hash_wire
            && self.pending(repo_id, &path)?.is_none()
        {
            return Ok(IndexOutcome::Unchanged {
                generation: canonical.generation,
                content_hash: hash,
            });
        }
        self.check_bounds(started)?;
        let prepared = prepare_file(repo_id, &loaded, &self.limits, started)?;
        self.check_bounds(started)?;
        let generation = if let Some(pending) = self.pending(repo_id, &path)? {
            if pending.content_hash == hash_wire {
                pending.generation
            } else {
                self.abort_pending(repo_id, &path)?;
                self.begin_generation(repo_id, &path, &hash_wire)?
            }
        } else {
            self.begin_generation(repo_id, &path, &hash_wire)?
        };
        match self.apply_prepared(&prepared, started) {
            Ok(()) => {
                self.commit_generation(repo_id, &path, generation, &prepared)?;
                Ok(IndexOutcome::Applied {
                    generation,
                    content_hash: hash,
                    class: prepared.class,
                    chunks: prepared.chunks,
                    symbols: prepared.symbols,
                })
            }
            Err(err) => Err(err),
        }
    }

    fn apply_prepared(
        &mut self,
        prepared: &PreparedFile,
        started: Instant,
    ) -> Result<(), PipelineError> {
        self.check_bounds(started)?;
        self.fts.upsert_document(&prepared.fts)?;
        self.check_fail_hook(FAIL_AFTER_FTS)?;
        self.check_bounds(started)?;
        self.graph.upsert_document(&prepared.graph)?;
        self.check_fail_hook(FAIL_AFTER_GRAPH)?;
        self.check_bounds(started)?;
        if let Some(provider) = self.embedder.as_deref() {
            self.vectors.upsert_document(&prepared.vector, provider)?;
        }
        Ok(())
    }

    fn complete_missing(
        &mut self,
        row: &InventoryRow,
        started: Instant,
    ) -> Result<(), PipelineError> {
        let prepared =
            PreparedFile::empty(row.repo_id, row.path.clone(), ContentHash::from_bytes(b""));
        self.apply_prepared(&prepared, started)?;
        self.commit_removal(row.repo_id, &row.path, row.generation)
    }

    fn find_candidate(
        &self,
        spec: &RepoSpec,
        path: &RepoPath,
        started: Instant,
    ) -> Result<Option<FileCandidate>, PipelineError> {
        self.check_bounds(started)?;
        for item in walk_repo(spec, &self.limits.walk, &self.limits.cancel) {
            let candidate = item?;
            if candidate.path() == path {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    fn repo(&self, id: RepoId) -> Result<&RepoSpec, PipelineError> {
        self.manifest
            .repo_by_id(id)
            .ok_or(PipelineError::UnknownRepo)
    }

    fn check_bounds(&self, started: Instant) -> Result<(), PipelineError> {
        if self.limits.cancel.is_cancelled() {
            return Err(PipelineError::Cancelled);
        }
        if self.limits.timeout.is_zero() || started.elapsed() > self.limits.timeout {
            return Err(PipelineError::Timeout);
        }
        Ok(())
    }

    fn check_fail_hook(&self, stage: u8) -> Result<(), PipelineError> {
        if self.fail_after.swap(FAIL_NONE, Ordering::SeqCst) == stage {
            return Err(PipelineError::NotCommitted);
        }
        Ok(())
    }

    fn canonical(
        &self,
        repo_id: RepoId,
        path: &RepoPath,
    ) -> Result<Option<InventoryRow>, PipelineError> {
        load_row(
            &self.inventory,
            "SELECT repo_id, path, content_hash, generation
             FROM context_file_canonical
             WHERE repo_id = ?1 AND path = ?2",
            repo_id,
            path,
            GenerationStatus::Committed,
        )
    }

    fn pending(
        &self,
        repo_id: RepoId,
        path: &RepoPath,
    ) -> Result<Option<InventoryRow>, PipelineError> {
        self.inventory
            .query_row(
                "SELECT repo_id, path, content_hash, generation
                 FROM context_index_generations
                 WHERE repo_id = ?1 AND path = ?2 AND status = ?3",
                params![repo_id.to_string(), path.as_str(), STATUS_PENDING],
                |row| parse_row(row, GenerationStatus::Pending),
            )
            .optional()
            .map_err(PipelineError::from)
    }

    fn all_pending(&self) -> Result<Vec<InventoryRow>, PipelineError> {
        let mut stmt = self.inventory.prepare(
            "SELECT repo_id, path, content_hash, generation
             FROM context_index_generations
             WHERE status = ?1
             ORDER BY generation ASC",
        )?;
        let mut rows = stmt.query(params![STATUS_PENDING])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(parse_row(row, GenerationStatus::Pending)?);
        }
        Ok(out)
    }

    fn begin_generation(
        &mut self,
        repo_id: RepoId,
        path: &RepoPath,
        content_hash: &str,
    ) -> Result<u64, PipelineError> {
        let tx = self
            .inventory
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE context_index_generations
             SET status = ?1
             WHERE repo_id = ?2 AND path = ?3 AND status = ?4",
            params![
                STATUS_ABORTED,
                repo_id.to_string(),
                path.as_str(),
                STATUS_PENDING
            ],
        )?;
        let next: i64 = tx.query_row(
            "SELECT COALESCE(MAX(generation), 0) + 1
             FROM context_index_generations
             WHERE repo_id = ?1 AND path = ?2",
            params![repo_id.to_string(), path.as_str()],
            |row| row.get(0),
        )?;
        let generation = u64::try_from(next).map_err(|_| PipelineError::Corrupt("generation"))?;
        tx.execute(
            "INSERT INTO context_index_generations
             (repo_id, path, generation, content_hash, status)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                repo_id.to_string(),
                path.as_str(),
                next,
                content_hash,
                STATUS_PENDING
            ],
        )?;
        tx.commit()?;
        Ok(generation)
    }

    fn abort_pending(&mut self, repo_id: RepoId, path: &RepoPath) -> Result<(), PipelineError> {
        self.inventory.execute(
            "UPDATE context_index_generations
             SET status = ?1
             WHERE repo_id = ?2 AND path = ?3 AND status = ?4",
            params![
                STATUS_ABORTED,
                repo_id.to_string(),
                path.as_str(),
                STATUS_PENDING
            ],
        )?;
        Ok(())
    }

    fn commit_generation(
        &mut self,
        repo_id: RepoId,
        path: &RepoPath,
        generation: u64,
        prepared: &PreparedFile,
    ) -> Result<(), PipelineError> {
        let tx = self
            .inventory
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let generation_i64 =
            i64::try_from(generation).map_err(|_| PipelineError::Corrupt("generation"))?;
        let n = tx.execute(
            "UPDATE context_index_generations
             SET status = ?1
             WHERE repo_id = ?2 AND path = ?3 AND generation = ?4 AND status = ?5",
            params![
                STATUS_COMMITTED,
                repo_id.to_string(),
                path.as_str(),
                generation_i64,
                STATUS_PENDING
            ],
        )?;
        if n != 1 {
            return Err(PipelineError::Corrupt("missing pending generation"));
        }
        tx.execute(
            "INSERT INTO context_file_canonical
             (repo_id, path, content_hash, generation, language, class)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(repo_id, path) DO UPDATE SET
               content_hash = excluded.content_hash,
               generation = excluded.generation,
               language = excluded.language,
               class = excluded.class",
            params![
                repo_id.to_string(),
                path.as_str(),
                prepared.content_hash.to_string(),
                generation_i64,
                prepared.language.map(SourceLanguage::as_str),
                class_label(prepared.class),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn commit_removal(
        &mut self,
        repo_id: RepoId,
        path: &RepoPath,
        generation: u64,
    ) -> Result<(), PipelineError> {
        let tx = self
            .inventory
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let generation_i64 =
            i64::try_from(generation).map_err(|_| PipelineError::Corrupt("generation"))?;
        tx.execute(
            "UPDATE context_index_generations
             SET status = ?1
             WHERE repo_id = ?2 AND path = ?3 AND generation = ?4",
            params![
                STATUS_COMMITTED,
                repo_id.to_string(),
                path.as_str(),
                generation_i64
            ],
        )?;
        tx.execute(
            "DELETE FROM context_file_canonical WHERE repo_id = ?1 AND path = ?2",
            params![repo_id.to_string(), path.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    fn fail_after_fts(&self) {
        self.fail_after.store(FAIL_AFTER_FTS, Ordering::SeqCst);
    }
}

impl PreparedFile {
    fn empty(repo_id: RepoId, path: RepoPath, content_hash: ContentHash) -> Self {
        Self {
            content_hash,
            class: ContentClass::MetadataOnly(crate::ingest::walk::MetadataOnlyReason::Binary),
            language: None,
            fts: FtsDocument::new(repo_id, path.clone(), None),
            graph: GraphDocument::new(repo_id, path.clone(), content_hash, None),
            vector: VectorDocument::new(repo_id, path, None),
            chunks: 0,
            symbols: 0,
        }
    }
}

impl From<InventoryRow> for FileIndexState {
    fn from(row: InventoryRow) -> Self {
        Self {
            repo_id: row.repo_id,
            path: row.path,
            content_hash: row.content_hash,
            generation: row.generation,
            status: row.status,
        }
    }
}

fn prepare_file(
    repo_id: RepoId,
    loaded: &LoadedCandidate,
    limits: &PipelineLimits,
    started: Instant,
) -> Result<PreparedFile, PipelineError> {
    if limits.cancel.is_cancelled() {
        return Err(PipelineError::Cancelled);
    }
    if limits.timeout.is_zero() || started.elapsed() > limits.timeout {
        return Err(PipelineError::Timeout);
    }
    let path = loaded.path().clone();
    let hash = loaded.content_hash();
    let language = loaded.language();
    let Some(text) = loaded.text() else {
        return Ok(PreparedFile {
            content_hash: hash,
            class: loaded.class(),
            language,
            fts: FtsDocument::new(repo_id, path.clone(), language),
            graph: GraphDocument::new(repo_id, path.clone(), hash, language),
            vector: VectorDocument::new(repo_id, path, language),
            chunks: 0,
            symbols: 0,
        });
    };
    let symbols = match language {
        Some(lang) => {
            let outcome = ParserRegistry::parse(lang, text.as_bytes(), &limits.parse);
            match extract_from_parse(&outcome, &path, text.as_bytes(), &limits.symbols) {
                Ok(symbols) => symbols,
                Err(SymbolError::Cancelled) => return Err(PipelineError::Cancelled),
                Err(SymbolError::Timeout) => Vec::new(),
                Err(err) => return Err(PipelineError::Symbol(err)),
            }
        }
        None => Vec::new(),
    };
    if limits.cancel.is_cancelled() {
        return Err(PipelineError::Cancelled);
    }
    let document = Document::with_content_hash(repo_id, path.clone(), language, text, hash);
    let chunks = chunk(&document, &symbols, &limits.chunk)?;
    let fts = if chunks.is_empty() {
        FtsDocument::new(repo_id, path.clone(), language)
    } else {
        FtsDocument::from_records(&chunks, &symbols)?
    };
    let graph = GraphDocument::from_symbols(repo_id, path.clone(), hash, language, symbols.clone());
    let vector = if chunks.is_empty() {
        VectorDocument::new(repo_id, path, language)
    } else {
        VectorDocument::from_records(&chunks, RedactionClass::Public)?
    };
    Ok(PreparedFile {
        content_hash: hash,
        class: loaded.class(),
        language,
        fts,
        graph,
        vector,
        chunks: u64::try_from(chunks.len()).unwrap_or(u64::MAX),
        symbols: u64::try_from(symbols.len()).unwrap_or(u64::MAX),
    })
}

fn load_row(
    conn: &Connection,
    sql: &str,
    repo_id: RepoId,
    path: &RepoPath,
    status: GenerationStatus,
) -> Result<Option<InventoryRow>, PipelineError> {
    conn.query_row(sql, params![repo_id.to_string(), path.as_str()], |row| {
        parse_row(row, status)
    })
    .optional()
    .map_err(PipelineError::from)
}

fn parse_row(row: &rusqlite::Row<'_>, status: GenerationStatus) -> rusqlite::Result<InventoryRow> {
    let repo: String = row.get(0)?;
    let rel: String = row.get(1)?;
    let content_hash: String = row.get(2)?;
    let generation: i64 = row.get(3)?;
    let repo_id = RepoId::from_str(&repo).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(PipelineError::Corrupt("repo_id")),
        )
    })?;
    let path = RepoPath::parse(&rel).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(PipelineError::Corrupt("path")),
        )
    })?;
    let generation = u64::try_from(generation)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(3, generation))?;
    Ok(InventoryRow {
        repo_id,
        path,
        content_hash,
        generation,
        status,
    })
}

fn class_label(class: ContentClass) -> &'static str {
    match class {
        ContentClass::Text { .. } => "text",
        ContentClass::MetadataOnly(_) => "metadata",
    }
}

fn configure_connection(conn: &Connection, require_wal: bool) -> Result<(), PipelineError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if require_wal {
        let journal_mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(PipelineError::Corrupt("journal_mode is not wal"));
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
    }
    conn.pragma_update(None, "foreign_keys", 1)?;
    Ok(())
}

fn ensure_schema(conn: &Connection) -> Result<(), PipelineError> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

impl PipelineError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::UnknownRepo => "unknown_repo",
            Self::InvalidPolicy => "invalid_policy",
            Self::NotCommitted => "not_committed",
            Self::Walk(_) => "walk",
            Self::Content(_) => "content",
            Self::Chunk(_) => "chunk",
            Self::Symbol(_) => "symbol",
            Self::Fts(_) => "fts",
            Self::Graph(_) => "graph",
            Self::Vector(_) => "vector",
            Self::Corrupt(_) => "corrupt",
            Self::Sqlite(_) => "sqlite",
            Self::Io(_) => "io",
        }
    }
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt(_) => f.write_str("corrupt index pipeline inventory"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::Io(_) => f.write_str("io error"),
            Self::Walk(err) => write!(f, "{err}"),
            Self::Content(err) => write!(f, "{err}"),
            Self::Chunk(err) => write!(f, "{err}"),
            Self::Symbol(err) => write!(f, "{err}"),
            Self::Fts(err) => write!(f, "{err}"),
            Self::Graph(err) => write!(f, "{err}"),
            Self::Vector(err) => write!(f, "{err}"),
            other => f.write_str(other.as_str()),
        }
    }
}

impl Error for PipelineError {}

impl From<WalkError> for PipelineError {
    fn from(value: WalkError) -> Self {
        match value {
            WalkError::Cancelled => Self::Cancelled,
            other => Self::Walk(other),
        }
    }
}

impl From<ContentError> for PipelineError {
    fn from(value: ContentError) -> Self {
        match value {
            ContentError::Cancelled => Self::Cancelled,
            other => Self::Content(other),
        }
    }
}

impl From<ChunkError> for PipelineError {
    fn from(value: ChunkError) -> Self {
        match value {
            ChunkError::Cancelled => Self::Cancelled,
            ChunkError::Timeout => Self::Timeout,
            other => Self::Chunk(other),
        }
    }
}

impl From<SymbolError> for PipelineError {
    fn from(value: SymbolError) -> Self {
        match value {
            SymbolError::Cancelled => Self::Cancelled,
            SymbolError::Timeout => Self::Timeout,
            other => Self::Symbol(other),
        }
    }
}

impl From<FtsError> for PipelineError {
    fn from(value: FtsError) -> Self {
        match value {
            FtsError::Cancelled => Self::Cancelled,
            FtsError::Timeout => Self::Timeout,
            FtsError::NotCommitted => Self::NotCommitted,
            other => Self::Fts(other),
        }
    }
}

impl From<GraphError> for PipelineError {
    fn from(value: GraphError) -> Self {
        match value {
            GraphError::Cancelled => Self::Cancelled,
            GraphError::Timeout => Self::Timeout,
            GraphError::NotCommitted => Self::NotCommitted,
            other => Self::Graph(other),
        }
    }
}

impl From<VectorError> for PipelineError {
    fn from(value: VectorError) -> Self {
        match value {
            VectorError::Cancelled => Self::Cancelled,
            VectorError::Timeout => Self::Timeout,
            VectorError::NotCommitted => Self::NotCommitted,
            other => Self::Vector(other),
        }
    }
}

impl From<rusqlite::Error> for PipelineError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<std::io::Error> for PipelineError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl fmt::Debug for IndexPipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexPipeline").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::fts::FtsQuery;
    use crate::index::graph::{GraphEdgeKind, SymbolLocator};
    use crate::index::vector::{EmbeddingVersion, VectorQuery};
    use crate::parse::symbols::SymbolKind;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempWorkspace {
        path: PathBuf,
    }

    impl TempWorkspace {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-context-pipeline-{}-{nanos}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("temp workspace");
            Self { path }
        }

        fn write_file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
            let path = self.path.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent");
            }
            fs::write(&path, bytes).expect("write");
            path
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct HashEmbedder {
        version: EmbeddingVersion,
        dims: u32,
    }

    impl HashEmbedder {
        fn new(dims: u32) -> Self {
            Self {
                version: EmbeddingVersion::new("hash-v1").expect("version"),
                dims,
            }
        }
    }

    impl EmbeddingProvider for HashEmbedder {
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
            Ok(texts
                .iter()
                .map(|text| {
                    let digest = ContentHash::from_bytes(text.as_bytes());
                    let bytes = digest.as_digest();
                    let mut values = vec![0.0f32; self.dims as usize];
                    for (i, slot) in values.iter_mut().enumerate() {
                        *slot = f32::from(bytes[i % bytes.len()]) - 127.5;
                    }
                    values
                })
                .collect())
        }
    }

    fn parse_manifest(ws: &TempWorkspace) -> WorkspaceManifest {
        ws.write_file("core/.keep", b"");
        let src =
            "schema = 1\n[[repos]]\nalias = \"core\"\nroot = \"core\"\nmode = \"read_write\"\n";
        WorkspaceManifest::parse(src, &ws.path, &CancellationToken::new()).expect("manifest")
    }

    fn candidate_named(manifest: &WorkspaceManifest, want: &str) -> FileCandidate {
        let repo = manifest.repo_by_alias("core").expect("repo");
        let limits = WalkLimits::default();
        let cancel = CancellationToken::new();
        for item in walk_repo(repo, &limits, &cancel) {
            let candidate = item.expect("candidate");
            if candidate.path().as_str() == want {
                return candidate;
            }
        }
        panic!("missing {want}");
    }

    fn open_mem(manifest: WorkspaceManifest) -> IndexPipeline {
        IndexPipeline::open_in_memory(manifest, PipelineLimits::new()).expect("open memory")
    }

    fn locator(repo: RepoId, rel: &str, kind: SymbolKind, fq: &str) -> SymbolLocator {
        SymbolLocator::new(repo, format!("{rel}#{}:{fq}", kind.as_str())).expect("locator")
    }

    #[test]
    fn index_file_uses_tree_sitter_symbols_in_graph_and_fts() {
        let ws = TempWorkspace::new();
        ws.write_file(
            "core/src/lib.rs",
            b"fn ts_extract_marker() { helper(); }\nfn helper() {}\n",
        );
        let manifest = parse_manifest(&ws);
        let mut pipeline = open_mem(manifest.clone());
        let candidate = candidate_named(&manifest, "src/lib.rs");
        let applied = pipeline.index_file(&candidate).expect("index");
        assert!(!applied.is_unchanged());
        let repo = candidate.repo_id();

        let marker = locator(
            repo,
            "src/lib.rs",
            SymbolKind::Function,
            "ts_extract_marker",
        );
        let helper = locator(repo, "src/lib.rs", SymbolKind::Function, "helper");
        let from_marker = pipeline
            .graph()
            .neighbors(&marker, 2, 64)
            .expect("marker neighbors");
        assert!(
            from_marker
                .iter()
                .any(|e| e.kind() == GraphEdgeKind::Reference),
            "tree-sitter extraction must emit a call edge from ts_extract_marker"
        );
        let to_helper = pipeline
            .graph()
            .neighbors(&helper, 2, 64)
            .expect("helper neighbors");
        assert!(
            !to_helper.is_empty(),
            "helper definition must be a graph node from the CST extractor"
        );

        let hits = pipeline
            .fts()
            .search(&FtsQuery::new("ts_extract_marker").repo(repo))
            .expect("fts");
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.path().as_str() == "src/lib.rs"));
    }

    #[test]
    fn index_file_noops_when_canonical_hash_already_indexed() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"fn unique_alpha() { 1 }\n");
        let manifest = parse_manifest(&ws);
        let mut pipeline = open_mem(manifest.clone());
        let candidate = candidate_named(&manifest, "src/lib.rs");
        let first = pipeline.index_file(&candidate).expect("index");
        assert!(!first.is_unchanged());
        assert_eq!(first.generation(), 1);
        let second = pipeline.index_file(&candidate).expect("reindex");
        assert!(second.is_unchanged());
        assert_eq!(second.generation(), 1);
        assert_eq!(second.content_hash(), first.content_hash());
        let hits = pipeline
            .fts()
            .search(&FtsQuery::new("unique_alpha").repo(candidate.repo_id()))
            .expect("search");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn changed_file_removes_stale_chunks_and_edges() {
        let ws = TempWorkspace::new();
        ws.write_file(
            "core/src/lib.rs",
            b"fn keep() { helper(); }\nfn helper() {}\n",
        );
        let manifest = parse_manifest(&ws);
        let mut pipeline = open_mem(manifest.clone());
        let candidate = candidate_named(&manifest, "src/lib.rs");
        let repo = candidate.repo_id();
        pipeline.index_file(&candidate).expect("first");
        let before = pipeline
            .fts()
            .search(&FtsQuery::new("helper").repo(repo))
            .expect("old fts");
        assert!(!before.is_empty());
        let helper = locator(repo, "src/lib.rs", SymbolKind::Function, "helper");
        let old_edges = pipeline
            .graph()
            .neighbors(&helper, 2, 64)
            .expect("old graph");
        assert!(!old_edges.is_empty());

        ws.write_file(
            "core/src/lib.rs",
            b"fn keep() { other(); }\nfn other() {}\n",
        );
        let candidate = candidate_named(&manifest, "src/lib.rs");
        let applied = pipeline.index_file(&candidate).expect("changed");
        assert!(!applied.is_unchanged());
        assert_eq!(applied.generation(), 2);

        let stale = pipeline
            .fts()
            .search(&FtsQuery::new("helper").repo(repo))
            .expect("stale");
        assert!(stale.is_empty());
        let fresh = pipeline
            .fts()
            .search(&FtsQuery::new("other").repo(repo))
            .expect("fresh");
        assert!(!fresh.is_empty());
        let gone = pipeline.graph().neighbors(&helper, 4, 64).expect("gone");
        assert!(gone.is_empty());
        let keep = locator(repo, "src/lib.rs", SymbolKind::Function, "keep");
        let after = pipeline.graph().neighbors(&keep, 2, 64).expect("after");
        assert!(after.iter().any(|e| e.kind() == GraphEdgeKind::Reference));
        assert!(
            after
                .iter()
                .all(|e| e.source_hash() == applied.content_hash().to_string())
        );
    }

    #[test]
    fn crash_between_stages_leaves_recoverable_generation() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"fn keep_me() {}\n");
        let manifest = parse_manifest(&ws);
        let index_dir = ws.path.join("index");
        let mut pipeline = IndexPipeline::open(&index_dir, manifest.clone(), PipelineLimits::new())
            .expect("open durable");
        let candidate = candidate_named(&manifest, "src/lib.rs");
        let first = pipeline.index_file(&candidate).expect("first");
        let first_hash = first.content_hash().to_string();

        ws.write_file("core/src/lib.rs", b"fn must_land() {}\n");
        let candidate = candidate_named(&manifest, "src/lib.rs");
        pipeline.fail_after_fts();
        let err = pipeline.index_file(&candidate).expect_err("crash");
        assert!(matches!(err, PipelineError::NotCommitted));
        assert_eq!(err.to_string(), "not_committed");
        assert!(!err.to_string().contains("lib.rs"));

        let canonical = pipeline
            .canonical_state(candidate.repo_id(), candidate.path())
            .expect("canonical")
            .expect("committed");
        assert_eq!(canonical.generation(), 1);
        assert_eq!(canonical.content_hash(), first_hash);
        assert_eq!(canonical.status(), GenerationStatus::Committed);
        let pending = pipeline
            .pending_state(candidate.repo_id(), candidate.path())
            .expect("pending")
            .expect("recoverable");
        assert_eq!(pending.generation(), 2);
        assert_eq!(pending.status(), GenerationStatus::Pending);
        assert_ne!(pending.content_hash(), first_hash);

        drop(pipeline);
        let mut pipeline = IndexPipeline::open(&index_dir, manifest.clone(), PipelineLimits::new())
            .expect("reopen");
        let pending = pipeline
            .pending_state(candidate.repo_id(), candidate.path())
            .expect("reopen pending")
            .expect("still pending");
        assert_eq!(pending.generation(), 2);
        let recovered = pipeline.index_file(&candidate).expect("recover");
        assert!(!recovered.is_unchanged());
        assert_eq!(recovered.generation(), 2);
        assert!(
            pipeline
                .pending_state(candidate.repo_id(), candidate.path())
                .expect("cleared")
                .is_none()
        );
        let stale = pipeline
            .fts()
            .search(&FtsQuery::new("keep_me").repo(candidate.repo_id()))
            .expect("stale");
        assert!(stale.is_empty());
        let fresh = pipeline
            .fts()
            .search(&FtsQuery::new("must_land").repo(candidate.repo_id()))
            .expect("fresh");
        assert_eq!(fresh.len(), 1);
        let keep = locator(
            candidate.repo_id(),
            "src/lib.rs",
            SymbolKind::Function,
            "keep_me",
        );
        assert!(
            pipeline
                .graph()
                .neighbors(&keep, 2, 16)
                .expect("old symbol")
                .is_empty()
        );
        let land = locator(
            candidate.repo_id(),
            "src/lib.rs",
            SymbolKind::Function,
            "must_land",
        );
        assert!(
            !pipeline
                .graph()
                .neighbors(&land, 2, 16)
                .expect("new symbol")
                .is_empty()
        );
    }

    #[test]
    fn metadata_only_change_clears_prior_chunks() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"fn visible_token() {}\n");
        let manifest = parse_manifest(&ws);
        let mut pipeline = open_mem(manifest.clone());
        let candidate = candidate_named(&manifest, "src/lib.rs");
        pipeline.index_file(&candidate).expect("text");
        assert_eq!(
            pipeline
                .fts()
                .search(&FtsQuery::new("visible_token"))
                .expect("hit")
                .len(),
            1
        );

        ws.write_file("core/src/lib.rs", &[0x00, 0x01, 0x02, 0x00]);
        let candidate = candidate_named(&manifest, "src/lib.rs");
        let applied = pipeline.index_file(&candidate).expect("binary");
        assert!(!applied.is_unchanged());
        assert!(
            pipeline
                .fts()
                .search(&FtsQuery::new("visible_token"))
                .expect("cleared")
                .is_empty()
        );
    }

    #[test]
    fn cancelled_index_fails_closed() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"fn x() {}\n");
        let manifest = parse_manifest(&ws);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut pipeline = IndexPipeline::open_in_memory(
            manifest.clone(),
            PipelineLimits::new().cancellation(cancel),
        )
        .expect("open");
        let candidate = candidate_named(&manifest, "src/lib.rs");
        let err = pipeline.index_file(&candidate).expect_err("cancelled");
        assert!(matches!(err, PipelineError::Cancelled));
        assert_eq!(err.to_string(), "cancelled");
        assert!(!err.to_string().contains("lib.rs"));
    }

    #[test]
    fn vector_index_updates_with_embedder() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"fn vector_alpha() {}\n");
        let manifest = parse_manifest(&ws);
        let mut pipeline = open_mem(manifest.clone());
        let embedder = HashEmbedder::new(8);
        pipeline.set_vector_index(
            VectorIndex::open_in_memory(VectorLimits::new()).expect("vectors"),
            Box::new(embedder),
        );
        let candidate = candidate_named(&manifest, "src/lib.rs");
        pipeline.index_file(&candidate).expect("index");
        let provider = HashEmbedder::new(8);
        let hits = pipeline
            .vectors()
            .search(
                &VectorQuery::from_text(
                    "fn vector_alpha() {}\n",
                    &provider,
                    &CancellationToken::new(),
                )
                .expect("query")
                .repo(candidate.repo_id()),
            )
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].score() > 0.99);
    }
}
