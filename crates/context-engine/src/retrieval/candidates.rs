//! Hybrid candidate generators for one context query.
//!
//! Explicit pins, FTS, vector, graph, current diff/error, and read-set each
//! run under their own timeout and result cap. Optional generators (vector,
//! graph, LSP) degrade in place so lexical and explicit pools stay available.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::{RepoId, RepoPath};

use crate::index::fts::{FtsError, FtsHit, FtsIndex, FtsQuery};
use crate::index::graph::{CodeGraph, GraphEdge, GraphError, SymbolLocator};
use crate::index::vector::{VectorError, VectorHit, VectorIndex, VectorQuery};
use crate::ingest::content::{ContentHash, SourceLanguage};
use crate::lsp::{LspEnricher, LspEnrichment, LspError, LspFact, SymbolQuery};
use crate::repo_manifest::CancellationToken;

/// Default wall-clock budget for one generator.
pub const DEFAULT_CANDIDATE_TIMEOUT: Duration = Duration::from_secs(2);

/// Default per-source page size before the query-wide cap.
pub const DEFAULT_PER_SOURCE_LIMIT: u32 = 20;

/// Default query-wide candidate cap applied on top of each source limit.
pub const DEFAULT_MAX_CANDIDATES: u32 = 64;

/// Default BFS depth for the graph generator.
pub const DEFAULT_GRAPH_CANDIDATE_HOPS: u32 = 2;

const NAME_PREFIX: &str = "#name:";
const FQ_PREFIX: &str = "#fq:";
const CANCEL_STRIDE: usize = 16;

/// Per-source timeouts and caps. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct CandidateLimits {
    explicit_timeout: Duration,
    explicit_limit: u32,
    fts_timeout: Duration,
    fts_limit: u32,
    vector_timeout: Duration,
    vector_limit: u32,
    graph_timeout: Duration,
    graph_limit: u32,
    graph_hops: u32,
    diff_timeout: Duration,
    diff_limit: u32,
    read_set_timeout: Duration,
    read_set_limit: u32,
    lsp_timeout: Duration,
    lsp_limit: u32,
    cancel: CancellationToken,
}

/// Indexes and optional LSP used by [`generate_candidates`].
pub struct CandidateSources<'a> {
    fts: Option<&'a FtsIndex>,
    vector: Option<&'a VectorIndex>,
    graph: Option<&'a CodeGraph>,
    lsp: Option<&'a mut dyn LspCandidateSource>,
}

/// Live LSP adapter. Timeout and transport failures degrade, they do not abort.
pub trait LspCandidateSource {
    fn enrich(&mut self, query: &SymbolQuery) -> Result<LspEnrichment, LspError>;
}

/// Lexical query plus optional pins, embeddings, graph hints, and evidence.
#[derive(Clone, Debug)]
pub struct ContextQuery {
    lexical: String,
    embedding: Option<Vec<f32>>,
    repo_id: Option<RepoId>,
    path: Option<RepoPath>,
    languages: Vec<SourceLanguage>,
    symbol_hints: Vec<SymbolHint>,
    explicit_pins: Vec<ExplicitPin>,
    diff_errors: Vec<DiffErrorEvidence>,
    read_set: Vec<ReadSetItem>,
    lsp_query: Option<SymbolQuery>,
    max_candidates: u32,
    cancel: CancellationToken,
}

/// Caller-pinned file or symbol that must be considered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExplicitPin {
    repo_id: Option<RepoId>,
    path: RepoPath,
    start_byte: Option<u32>,
    end_byte: Option<u32>,
    symbol: Option<String>,
    content_hash: Option<ContentHash>,
    chunk_id: Option<String>,
}

/// Symbol name or locator used to seed graph neighborhood retrieval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolHint {
    repo_id: Option<RepoId>,
    name: String,
}

/// Current working-tree diff or test/diagnostic failure link.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffErrorEvidence {
    repo_id: Option<RepoId>,
    path: RepoPath,
    start_byte: Option<u32>,
    end_byte: Option<u32>,
    kind: DiffErrorKind,
    content_hash: Option<ContentHash>,
    chunk_id: Option<String>,
}

/// Classification of a current-diff or failure locator.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DiffErrorKind {
    Diff,
    TestFailure,
    Diagnostic,
}

/// Previously shown range. Only changed hashes become read-set candidates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadSetItem {
    repo_id: Option<RepoId>,
    path: RepoPath,
    start_byte: u32,
    end_byte: u32,
    recorded_hash: ContentHash,
    current_hash: ContentHash,
    chunk_id: Option<String>,
}

/// One generator's ranked hit. `reason` is the inclusion provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextCandidate {
    source: CandidateSource,
    rank: u32,
    score: f32,
    reason: InclusionReason,
    repo_id: Option<RepoId>,
    path: Option<RepoPath>,
    language: Option<SourceLanguage>,
    chunk_id: Option<String>,
    content_hash: Option<String>,
    start_byte: Option<u32>,
    end_byte: Option<u32>,
    symbol: Option<String>,
    freshness: Freshness,
    trust: TrustClass,
}

/// Which generator produced the candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum CandidateSource {
    Explicit,
    Lexical,
    Vector,
    Graph,
    DiffError,
    ReadSet,
    Symbol,
}

/// Why the candidate was included. Recorded for later compile/rank.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum InclusionReason {
    Explicit,
    Lexical,
    Vector,
    Graph,
    Diff,
    Error,
    ReadSetChanged,
    Lsp,
}

/// Content freshness relative to the last observed hash, when known.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Freshness {
    Fresh,
    Stale,
    Unknown,
}

/// Trust label for model-visible retrieved text. Repo hits stay untrusted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TrustClass {
    Untrusted,
    Project,
}

/// Per-source output plus degrade status for optional generators.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidatePools {
    explicit: Vec<ContextCandidate>,
    fts: Vec<ContextCandidate>,
    vector: Vec<ContextCandidate>,
    graph: Vec<ContextCandidate>,
    diff_error: Vec<ContextCandidate>,
    read_set: Vec<ContextCandidate>,
    statuses: Vec<GeneratorStatus>,
}

/// Outcome of one generator. Optional failures are statuses, not hard errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GeneratorStatus {
    kind: GeneratorKind,
    produced: u32,
    degrade: Option<GeneratorDegradeReason>,
}

/// Generator identity used in health/degrade events.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GeneratorKind {
    Explicit,
    Fts,
    Vector,
    Graph,
    DiffError,
    ReadSet,
    Lsp,
}

/// Why an optional generator produced no (or partial) hits.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GeneratorDegradeReason {
    Disabled,
    Unavailable,
    Timeout,
    Failed,
}

/// Typed generation failure. Display never echoes queries, paths, or source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateError {
    Cancelled,
    InvalidPolicy,
    InvalidQuery,
}

impl CandidateLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn explicit_timeout(mut self, value: Duration) -> Self {
        self.explicit_timeout = value;
        self
    }

    pub fn explicit_limit(mut self, value: u32) -> Self {
        self.explicit_limit = value;
        self
    }

    pub fn fts_timeout(mut self, value: Duration) -> Self {
        self.fts_timeout = value;
        self
    }

    pub fn fts_limit(mut self, value: u32) -> Self {
        self.fts_limit = value;
        self
    }

    pub fn vector_timeout(mut self, value: Duration) -> Self {
        self.vector_timeout = value;
        self
    }

    pub fn vector_limit(mut self, value: u32) -> Self {
        self.vector_limit = value;
        self
    }

    pub fn graph_timeout(mut self, value: Duration) -> Self {
        self.graph_timeout = value;
        self
    }

    pub fn graph_limit(mut self, value: u32) -> Self {
        self.graph_limit = value;
        self
    }

    pub fn graph_hops(mut self, value: u32) -> Self {
        self.graph_hops = value;
        self
    }

    pub fn diff_timeout(mut self, value: Duration) -> Self {
        self.diff_timeout = value;
        self
    }

    pub fn diff_limit(mut self, value: u32) -> Self {
        self.diff_limit = value;
        self
    }

    pub fn read_set_timeout(mut self, value: Duration) -> Self {
        self.read_set_timeout = value;
        self
    }

    pub fn read_set_limit(mut self, value: u32) -> Self {
        self.read_set_limit = value;
        self
    }

    pub fn lsp_timeout(mut self, value: Duration) -> Self {
        self.lsp_timeout = value;
        self
    }

    pub fn lsp_limit(mut self, value: u32) -> Self {
        self.lsp_limit = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }

    pub fn explicit_timeout_value(&self) -> Duration {
        self.explicit_timeout
    }

    pub fn explicit_limit_value(&self) -> u32 {
        self.explicit_limit
    }

    pub fn fts_timeout_value(&self) -> Duration {
        self.fts_timeout
    }

    pub fn fts_limit_value(&self) -> u32 {
        self.fts_limit
    }

    pub fn vector_timeout_value(&self) -> Duration {
        self.vector_timeout
    }

    pub fn vector_limit_value(&self) -> u32 {
        self.vector_limit
    }

    pub fn graph_timeout_value(&self) -> Duration {
        self.graph_timeout
    }

    pub fn graph_limit_value(&self) -> u32 {
        self.graph_limit
    }

    pub fn graph_hops_value(&self) -> u32 {
        self.graph_hops
    }

    pub fn diff_timeout_value(&self) -> Duration {
        self.diff_timeout
    }

    pub fn diff_limit_value(&self) -> u32 {
        self.diff_limit
    }

    pub fn read_set_timeout_value(&self) -> Duration {
        self.read_set_timeout
    }

    pub fn read_set_limit_value(&self) -> u32 {
        self.read_set_limit
    }

    pub fn lsp_timeout_value(&self) -> Duration {
        self.lsp_timeout
    }

    pub fn lsp_limit_value(&self) -> u32 {
        self.lsp_limit
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }

    fn validate(&self) -> Result<(), CandidateError> {
        if self.explicit_limit == 0
            || self.fts_limit == 0
            || self.vector_limit == 0
            || self.graph_limit == 0
            || self.diff_limit == 0
            || self.read_set_limit == 0
            || self.lsp_limit == 0
        {
            return Err(CandidateError::InvalidPolicy);
        }
        Ok(())
    }
}

impl Default for CandidateLimits {
    fn default() -> Self {
        Self {
            explicit_timeout: DEFAULT_CANDIDATE_TIMEOUT,
            explicit_limit: DEFAULT_PER_SOURCE_LIMIT,
            fts_timeout: DEFAULT_CANDIDATE_TIMEOUT,
            fts_limit: DEFAULT_PER_SOURCE_LIMIT,
            vector_timeout: DEFAULT_CANDIDATE_TIMEOUT,
            vector_limit: DEFAULT_PER_SOURCE_LIMIT,
            graph_timeout: DEFAULT_CANDIDATE_TIMEOUT,
            graph_limit: DEFAULT_PER_SOURCE_LIMIT,
            graph_hops: DEFAULT_GRAPH_CANDIDATE_HOPS,
            diff_timeout: DEFAULT_CANDIDATE_TIMEOUT,
            diff_limit: DEFAULT_PER_SOURCE_LIMIT,
            read_set_timeout: DEFAULT_CANDIDATE_TIMEOUT,
            read_set_limit: DEFAULT_PER_SOURCE_LIMIT,
            lsp_timeout: DEFAULT_CANDIDATE_TIMEOUT,
            lsp_limit: DEFAULT_PER_SOURCE_LIMIT,
            cancel: CancellationToken::new(),
        }
    }
}

impl<'a> CandidateSources<'a> {
    pub fn new() -> Self {
        Self {
            fts: None,
            vector: None,
            graph: None,
            lsp: None,
        }
    }

    pub fn fts(mut self, index: &'a FtsIndex) -> Self {
        self.fts = Some(index);
        self
    }

    pub fn vector(mut self, index: &'a VectorIndex) -> Self {
        self.vector = Some(index);
        self
    }

    pub fn graph(mut self, graph: &'a CodeGraph) -> Self {
        self.graph = Some(graph);
        self
    }

    pub fn lsp(mut self, source: &'a mut dyn LspCandidateSource) -> Self {
        self.lsp = Some(source);
        self
    }
}

impl Default for CandidateSources<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: crate::lsp::LspClient> LspCandidateSource for LspEnricher<C> {
    fn enrich(&mut self, query: &SymbolQuery) -> Result<LspEnrichment, LspError> {
        LspEnricher::enrich(self, query)
    }
}

impl ContextQuery {
    pub fn new(lexical: impl Into<String>) -> Self {
        Self {
            lexical: lexical.into(),
            embedding: None,
            repo_id: None,
            path: None,
            languages: Vec::new(),
            symbol_hints: Vec::new(),
            explicit_pins: Vec::new(),
            diff_errors: Vec::new(),
            read_set: Vec::new(),
            lsp_query: None,
            max_candidates: DEFAULT_MAX_CANDIDATES,
            cancel: CancellationToken::new(),
        }
    }

    pub fn embedding(mut self, vector: Vec<f32>) -> Self {
        self.embedding = Some(vector);
        self
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
        if !self.languages.contains(&language) {
            self.languages.push(language);
        }
        self
    }

    pub fn symbol_hint(mut self, hint: SymbolHint) -> Self {
        self.symbol_hints.push(hint);
        self
    }

    pub fn pin(mut self, pin: ExplicitPin) -> Self {
        self.explicit_pins.push(pin);
        self
    }

    pub fn diff_error(mut self, evidence: DiffErrorEvidence) -> Self {
        self.diff_errors.push(evidence);
        self
    }

    pub fn read_set_item(mut self, item: ReadSetItem) -> Self {
        self.read_set.push(item);
        self
    }

    pub fn lsp_query(mut self, query: SymbolQuery) -> Self {
        self.lsp_query = Some(query);
        self
    }

    pub fn max_candidates(mut self, value: u32) -> Self {
        self.max_candidates = value;
        self
    }

    pub fn cancellation(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn lexical(&self) -> &str {
        &self.lexical
    }

    pub fn embedding_vector(&self) -> Option<&[f32]> {
        self.embedding.as_deref()
    }

    pub fn repo_id(&self) -> Option<RepoId> {
        self.repo_id
    }

    pub fn path_filter(&self) -> Option<&RepoPath> {
        self.path.as_ref()
    }

    pub fn languages(&self) -> &[SourceLanguage] {
        &self.languages
    }

    pub fn symbol_hints(&self) -> &[SymbolHint] {
        &self.symbol_hints
    }

    pub fn explicit_pins(&self) -> &[ExplicitPin] {
        &self.explicit_pins
    }

    pub fn diff_errors(&self) -> &[DiffErrorEvidence] {
        &self.diff_errors
    }

    pub fn read_set(&self) -> &[ReadSetItem] {
        &self.read_set
    }

    pub fn max_candidates_value(&self) -> u32 {
        self.max_candidates
    }
}

impl ExplicitPin {
    pub fn new(path: RepoPath) -> Self {
        Self {
            repo_id: None,
            path,
            start_byte: None,
            end_byte: None,
            symbol: None,
            content_hash: None,
            chunk_id: None,
        }
    }

    pub fn repo(mut self, repo_id: RepoId) -> Self {
        self.repo_id = Some(repo_id);
        self
    }

    pub fn range(mut self, start_byte: u32, end_byte: u32) -> Self {
        self.start_byte = Some(start_byte);
        self.end_byte = Some(end_byte);
        self
    }

    pub fn symbol(mut self, value: impl Into<String>) -> Self {
        self.symbol = Some(value.into());
        self
    }

    pub fn content_hash(mut self, value: ContentHash) -> Self {
        self.content_hash = Some(value);
        self
    }

    pub fn chunk_id(mut self, value: impl Into<String>) -> Self {
        self.chunk_id = Some(value.into());
        self
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }
}

impl SymbolHint {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            repo_id: None,
            name: name.into(),
        }
    }

    pub fn repo(mut self, repo_id: RepoId) -> Self {
        self.repo_id = Some(repo_id);
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl DiffErrorEvidence {
    pub fn new(path: RepoPath, kind: DiffErrorKind) -> Self {
        Self {
            repo_id: None,
            path,
            start_byte: None,
            end_byte: None,
            kind,
            content_hash: None,
            chunk_id: None,
        }
    }

    pub fn repo(mut self, repo_id: RepoId) -> Self {
        self.repo_id = Some(repo_id);
        self
    }

    pub fn range(mut self, start_byte: u32, end_byte: u32) -> Self {
        self.start_byte = Some(start_byte);
        self.end_byte = Some(end_byte);
        self
    }

    pub fn content_hash(mut self, value: ContentHash) -> Self {
        self.content_hash = Some(value);
        self
    }

    pub fn chunk_id(mut self, value: impl Into<String>) -> Self {
        self.chunk_id = Some(value.into());
        self
    }

    pub fn kind(&self) -> DiffErrorKind {
        self.kind
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }
}

impl DiffErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Diff => "diff",
            Self::TestFailure => "test_failure",
            Self::Diagnostic => "diagnostic",
        }
    }
}

impl ReadSetItem {
    pub fn new(
        path: RepoPath,
        start_byte: u32,
        end_byte: u32,
        recorded_hash: ContentHash,
        current_hash: ContentHash,
    ) -> Self {
        Self {
            repo_id: None,
            path,
            start_byte,
            end_byte,
            recorded_hash,
            current_hash,
            chunk_id: None,
        }
    }

    pub fn repo(mut self, repo_id: RepoId) -> Self {
        self.repo_id = Some(repo_id);
        self
    }

    pub fn chunk_id(mut self, value: impl Into<String>) -> Self {
        self.chunk_id = Some(value.into());
        self
    }

    pub fn is_changed(&self) -> bool {
        self.recorded_hash != self.current_hash
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }
}

impl ContextCandidate {
    pub fn source(&self) -> CandidateSource {
        self.source
    }

    pub fn rank(&self) -> u32 {
        self.rank
    }

    pub fn score(&self) -> f32 {
        self.score
    }

    pub fn reason(&self) -> InclusionReason {
        self.reason
    }

    pub fn repo_id(&self) -> Option<RepoId> {
        self.repo_id
    }

    pub fn path(&self) -> Option<&RepoPath> {
        self.path.as_ref()
    }

    pub fn language(&self) -> Option<SourceLanguage> {
        self.language
    }

    pub fn chunk_id(&self) -> Option<&str> {
        self.chunk_id.as_deref()
    }

    pub fn content_hash(&self) -> Option<&str> {
        self.content_hash.as_deref()
    }

    pub fn start_byte(&self) -> Option<u32> {
        self.start_byte
    }

    pub fn end_byte(&self) -> Option<u32> {
        self.end_byte
    }

    pub fn symbol(&self) -> Option<&str> {
        self.symbol.as_deref()
    }

    pub fn freshness(&self) -> Freshness {
        self.freshness
    }

    pub fn trust(&self) -> TrustClass {
        self.trust
    }
}

impl CandidateSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Lexical => "lexical",
            Self::Vector => "vector",
            Self::Graph => "graph",
            Self::DiffError => "diff_error",
            Self::ReadSet => "read_set",
            Self::Symbol => "symbol",
        }
    }
}

impl InclusionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Lexical => "lexical",
            Self::Vector => "vector",
            Self::Graph => "graph",
            Self::Diff => "diff",
            Self::Error => "error",
            Self::ReadSetChanged => "read_set_changed",
            Self::Lsp => "lsp",
        }
    }
}

impl Freshness {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Unknown => "unknown",
        }
    }
}

impl TrustClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Project => "project",
        }
    }
}

impl CandidatePools {
    fn empty() -> Self {
        Self {
            explicit: Vec::new(),
            fts: Vec::new(),
            vector: Vec::new(),
            graph: Vec::new(),
            diff_error: Vec::new(),
            read_set: Vec::new(),
            statuses: Vec::new(),
        }
    }

    pub fn explicit(&self) -> &[ContextCandidate] {
        &self.explicit
    }

    pub fn fts(&self) -> &[ContextCandidate] {
        &self.fts
    }

    pub fn vector(&self) -> &[ContextCandidate] {
        &self.vector
    }

    pub fn graph(&self) -> &[ContextCandidate] {
        &self.graph
    }

    pub fn diff_error(&self) -> &[ContextCandidate] {
        &self.diff_error
    }

    pub fn read_set(&self) -> &[ContextCandidate] {
        &self.read_set
    }

    pub fn statuses(&self) -> &[GeneratorStatus] {
        &self.statuses
    }

    pub fn status(&self, kind: GeneratorKind) -> Option<GeneratorStatus> {
        self.statuses.iter().copied().find(|s| s.kind == kind)
    }

    /// Drop out-of-policy candidates. Generator status rows are unchanged.
    pub fn filter(
        self,
        policy: &crate::retrieval::filter::RetrievalFilter,
        started: Instant,
    ) -> Result<Self, crate::retrieval::filter::FilterError> {
        use crate::retrieval::filter::filter_candidates;
        Ok(Self {
            explicit: filter_candidates(self.explicit, policy, started)?,
            fts: filter_candidates(self.fts, policy, started)?,
            vector: filter_candidates(self.vector, policy, started)?,
            graph: filter_candidates(self.graph, policy, started)?,
            diff_error: filter_candidates(self.diff_error, policy, started)?,
            read_set: filter_candidates(self.read_set, policy, started)?,
            statuses: self.statuses,
        })
    }
}

impl GeneratorStatus {
    fn ok(kind: GeneratorKind, produced: u32) -> Self {
        Self {
            kind,
            produced,
            degrade: None,
        }
    }

    fn degraded(kind: GeneratorKind, reason: GeneratorDegradeReason) -> Self {
        Self {
            kind,
            produced: 0,
            degrade: Some(reason),
        }
    }

    pub fn kind(self) -> GeneratorKind {
        self.kind
    }

    pub fn produced(self) -> u32 {
        self.produced
    }

    pub fn degrade(self) -> Option<GeneratorDegradeReason> {
        self.degrade
    }

    pub fn is_degraded(self) -> bool {
        self.degrade.is_some()
    }
}

impl GeneratorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Fts => "fts",
            Self::Vector => "vector",
            Self::Graph => "graph",
            Self::DiffError => "diff_error",
            Self::ReadSet => "read_set",
            Self::Lsp => "lsp",
        }
    }
}

impl GeneratorDegradeReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::Failed => "failed",
        }
    }
}

impl CandidateError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::InvalidPolicy => "invalid_policy",
            Self::InvalidQuery => "invalid_query",
        }
    }
}

impl fmt::Display for CandidateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for CandidateError {}

impl fmt::Display for CandidateSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for InclusionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Run every generator. Optional failures are recorded; cancellation fails closed.
pub fn generate_candidates(
    query: &ContextQuery,
    mut sources: CandidateSources<'_>,
    limits: &CandidateLimits,
) -> Result<CandidatePools, CandidateError> {
    limits.validate()?;
    if query.max_candidates == 0 {
        return Err(CandidateError::InvalidQuery);
    }
    if query.lexical.contains('\0') {
        return Err(CandidateError::InvalidQuery);
    }
    check_cancel(&query.cancel, &limits.cancel)?;

    let mut pools = CandidatePools::empty();

    let (explicit, explicit_status) = generate_explicit(
        query,
        limits.explicit_timeout,
        cap(limits.explicit_limit, query),
    )?;
    pools.explicit = explicit;
    pools.statuses.push(explicit_status);

    let (fts_hits, fts_status) = generate_fts(
        query,
        sources.fts,
        limits.fts_timeout,
        cap(limits.fts_limit, query),
    )?;
    pools.fts = fts_hits;
    pools.statuses.push(fts_status);

    let (vector_hits, vector_status) = generate_vector(
        query,
        sources.vector,
        limits.vector_timeout,
        cap(limits.vector_limit, query),
    )?;
    pools.vector = vector_hits;
    pools.statuses.push(vector_status);

    let (graph_hits, graph_status) = generate_graph(
        query,
        sources.graph,
        limits.graph_timeout,
        cap(limits.graph_limit, query),
        limits.graph_hops,
    )?;
    pools.graph = graph_hits;
    pools.statuses.push(graph_status);

    let lsp = sources.lsp.take();
    let (lsp_hits, lsp_status) =
        generate_lsp(query, lsp, limits.lsp_timeout, cap(limits.lsp_limit, query))?;
    pools.statuses.push(lsp_status);
    append_capped(&mut pools.graph, lsp_hits, query.max_candidates);

    let (diff_hits, diff_status) =
        generate_diff_error(query, limits.diff_timeout, cap(limits.diff_limit, query))?;
    pools.diff_error = diff_hits;
    pools.statuses.push(diff_status);

    let (read_hits, read_status) = generate_read_set(
        query,
        limits.read_set_timeout,
        cap(limits.read_set_limit, query),
    )?;
    pools.read_set = read_hits;
    pools.statuses.push(read_status);

    check_cancel(&query.cancel, &limits.cancel)?;
    Ok(pools)
}

fn finish(
    kind: GeneratorKind,
    out: Vec<ContextCandidate>,
    degrade: Option<GeneratorDegradeReason>,
) -> (Vec<ContextCandidate>, GeneratorStatus) {
    let produced = out.len() as u32;
    (
        out,
        GeneratorStatus {
            kind,
            produced,
            degrade,
        },
    )
}

fn generate_explicit(
    query: &ContextQuery,
    timeout: Duration,
    limit: u32,
) -> Result<(Vec<ContextCandidate>, GeneratorStatus), CandidateError> {
    let started = Instant::now();
    if let Some(reason) = begin_generator(&query.cancel, timeout, started)? {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::Explicit, reason),
        ));
    }
    let mut out = Vec::new();
    for (step, pin) in query.explicit_pins.iter().enumerate() {
        if let Some(reason) = stride_check(step, &query.cancel, timeout, started)? {
            return Ok(finish(GeneratorKind::Explicit, out, Some(reason)));
        }
        if out.len() as u32 >= limit {
            break;
        }
        out.push(ContextCandidate {
            source: CandidateSource::Explicit,
            rank: (out.len() as u32).saturating_add(1),
            score: 1.0,
            reason: InclusionReason::Explicit,
            repo_id: pin.repo_id.or(query.repo_id),
            path: Some(pin.path.clone()),
            language: None,
            chunk_id: pin.chunk_id.clone(),
            content_hash: pin.content_hash.map(|h| h.to_string()),
            start_byte: pin.start_byte,
            end_byte: pin.end_byte,
            symbol: pin.symbol.clone(),
            freshness: Freshness::Unknown,
            trust: TrustClass::Untrusted,
        });
    }
    Ok(finish(GeneratorKind::Explicit, out, None))
}

fn generate_fts(
    query: &ContextQuery,
    index: Option<&FtsIndex>,
    timeout: Duration,
    limit: u32,
) -> Result<(Vec<ContextCandidate>, GeneratorStatus), CandidateError> {
    let started = Instant::now();
    if query.lexical.trim().is_empty() {
        return Ok((Vec::new(), GeneratorStatus::ok(GeneratorKind::Fts, 0)));
    }
    if let Some(reason) = begin_generator(&query.cancel, timeout, started)? {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::Fts, reason),
        ));
    }
    let Some(index) = index else {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::Fts, GeneratorDegradeReason::Unavailable),
        ));
    };

    let languages = search_languages(query);
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for language in languages {
        if let Some(reason) = check_generator(&query.cancel, timeout, started)? {
            return Ok(finish(GeneratorKind::Fts, out, Some(reason)));
        }
        let remaining = limit.saturating_sub(out.len() as u32);
        if remaining == 0 {
            break;
        }
        let mut fts_query = FtsQuery::new(&query.lexical)
            .limit(remaining)
            .cancellation(query.cancel.clone());
        if let Some(repo) = query.repo_id {
            fts_query = fts_query.repo(repo);
        }
        if let Some(path) = query.path.clone() {
            fts_query = fts_query.path(path);
        }
        if let Some(language) = language {
            fts_query = fts_query.language(language);
        }
        let hits = match index.search(&fts_query) {
            Ok(hits) => hits,
            Err(FtsError::Cancelled) => return Err(CandidateError::Cancelled),
            Err(FtsError::Timeout) => {
                return Ok(finish(
                    GeneratorKind::Fts,
                    out,
                    Some(GeneratorDegradeReason::Timeout),
                ));
            }
            Err(FtsError::EmptyQuery | FtsError::QueryTooLarge | FtsError::InvalidLimit) => {
                return Ok(finish(
                    GeneratorKind::Fts,
                    out,
                    Some(GeneratorDegradeReason::Failed),
                ));
            }
            Err(_) => {
                return Ok((
                    Vec::new(),
                    GeneratorStatus::degraded(GeneratorKind::Fts, GeneratorDegradeReason::Failed),
                ));
            }
        };
        for hit in hits {
            if !seen.insert(hit.chunk_id().to_string()) {
                continue;
            }
            if out.len() as u32 >= limit {
                break;
            }
            out.push(candidate_from_fts(&hit, out.len() as u32));
        }
    }
    Ok(finish(GeneratorKind::Fts, out, None))
}

fn generate_vector(
    query: &ContextQuery,
    index: Option<&VectorIndex>,
    timeout: Duration,
    limit: u32,
) -> Result<(Vec<ContextCandidate>, GeneratorStatus), CandidateError> {
    let started = Instant::now();
    let Some(embedding) = query.embedding.as_ref() else {
        return Ok((Vec::new(), GeneratorStatus::ok(GeneratorKind::Vector, 0)));
    };
    if let Some(reason) = begin_generator(&query.cancel, timeout, started)? {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::Vector, reason),
        ));
    }
    let Some(index) = index else {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::Vector, GeneratorDegradeReason::Unavailable),
        ));
    };
    if index.is_disabled() {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::Vector, GeneratorDegradeReason::Disabled),
        ));
    }

    let languages = search_languages(query);
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for language in languages {
        if let Some(reason) = check_generator(&query.cancel, timeout, started)? {
            return Ok(finish(GeneratorKind::Vector, out, Some(reason)));
        }
        let remaining = limit.saturating_sub(out.len() as u32);
        if remaining == 0 {
            break;
        }
        let mut vector_query = VectorQuery::new(embedding.clone())
            .limit(remaining)
            .cancellation(query.cancel.clone());
        if let Some(repo) = query.repo_id {
            vector_query = vector_query.repo(repo);
        }
        if let Some(path) = query.path.clone() {
            vector_query = vector_query.path(path);
        }
        if let Some(language) = language {
            vector_query = vector_query.language(language);
        }
        let hits = match index.search(&vector_query) {
            Ok(hits) => hits,
            Err(VectorError::Cancelled) => return Err(CandidateError::Cancelled),
            Err(VectorError::Timeout) => {
                return Ok(finish(
                    GeneratorKind::Vector,
                    out,
                    Some(GeneratorDegradeReason::Timeout),
                ));
            }
            Err(
                VectorError::EmptyQuery | VectorError::InvalidEmbedding | VectorError::InvalidLimit,
            ) => {
                return Ok(finish(
                    GeneratorKind::Vector,
                    out,
                    Some(GeneratorDegradeReason::Failed),
                ));
            }
            Err(_) => {
                return Ok((
                    Vec::new(),
                    GeneratorStatus::degraded(
                        GeneratorKind::Vector,
                        GeneratorDegradeReason::Failed,
                    ),
                ));
            }
        };
        for hit in hits {
            if !seen.insert(hit.chunk_id().to_string()) {
                continue;
            }
            if out.len() as u32 >= limit {
                break;
            }
            out.push(candidate_from_vector(&hit, out.len() as u32));
        }
    }
    Ok(finish(GeneratorKind::Vector, out, None))
}

fn generate_graph(
    query: &ContextQuery,
    graph: Option<&CodeGraph>,
    timeout: Duration,
    limit: u32,
    hops: u32,
) -> Result<(Vec<ContextCandidate>, GeneratorStatus), CandidateError> {
    let started = Instant::now();
    if query.symbol_hints.is_empty() {
        return Ok((Vec::new(), GeneratorStatus::ok(GeneratorKind::Graph, 0)));
    }
    if let Some(reason) = begin_generator(&query.cancel, timeout, started)? {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::Graph, reason),
        ));
    }
    let Some(graph) = graph else {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::Graph, GeneratorDegradeReason::Unavailable),
        ));
    };

    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for (step, hint) in query.symbol_hints.iter().enumerate() {
        if let Some(reason) = stride_check(step, &query.cancel, timeout, started)? {
            return Ok(finish(GeneratorKind::Graph, out, Some(reason)));
        }
        if out.len() as u32 >= limit {
            break;
        }
        let Some(repo) = hint.repo_id.or(query.repo_id) else {
            continue;
        };
        for locator in hint_locators(&hint.name) {
            if out.len() as u32 >= limit {
                break;
            }
            let seed = match SymbolLocator::new(repo, locator) {
                Ok(seed) => seed,
                Err(GraphError::Cancelled) => return Err(CandidateError::Cancelled),
                Err(_) => continue,
            };
            push_graph_node(&mut out, &mut seen, repo, seed.as_str(), 1.0, limit);
            if hops == 0 || out.len() as u32 >= limit {
                continue;
            }
            let remaining = limit.saturating_sub(out.len() as u32);
            let edges = match graph.neighbors(&seed, hops, remaining.max(1)) {
                Ok(edges) => edges,
                Err(GraphError::Cancelled) => return Err(CandidateError::Cancelled),
                Err(GraphError::Timeout) => {
                    return Ok(finish(
                        GeneratorKind::Graph,
                        out,
                        Some(GeneratorDegradeReason::Timeout),
                    ));
                }
                Err(GraphError::InvalidLimit | GraphError::InvalidLocator) => continue,
                Err(_) => {
                    return Ok(finish(
                        GeneratorKind::Graph,
                        out,
                        Some(GeneratorDegradeReason::Failed),
                    ));
                }
            };
            push_graph_edges(&mut out, &mut seen, &edges, limit);
        }
    }
    Ok(finish(GeneratorKind::Graph, out, None))
}

fn generate_lsp(
    query: &ContextQuery,
    lsp: Option<&mut dyn LspCandidateSource>,
    timeout: Duration,
    limit: u32,
) -> Result<(Vec<ContextCandidate>, GeneratorStatus), CandidateError> {
    let started = Instant::now();
    if query.lsp_query.is_none() {
        return Ok((Vec::new(), GeneratorStatus::ok(GeneratorKind::Lsp, 0)));
    }
    if let Some(reason) = begin_generator(&query.cancel, timeout, started)? {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::Lsp, reason),
        ));
    }
    let Some(lsp) = lsp else {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::Lsp, GeneratorDegradeReason::Unavailable),
        ));
    };
    let Some(lsp_query) = query.lsp_query.as_ref() else {
        return Ok((Vec::new(), GeneratorStatus::ok(GeneratorKind::Lsp, 0)));
    };
    let enrichment = match lsp.enrich(lsp_query) {
        Ok(enrichment) => enrichment,
        Err(LspError::Cancelled) => return Err(CandidateError::Cancelled),
        Err(LspError::InvalidQuery) => {
            return Ok((
                Vec::new(),
                GeneratorStatus::degraded(GeneratorKind::Lsp, GeneratorDegradeReason::Failed),
            ));
        }
    };
    if let Some(reason) = check_generator(&query.cancel, timeout, started)? {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::Lsp, reason),
        ));
    }
    if enrichment.is_degraded() && enrichment.facts().is_empty() {
        let reason = if matches!(enrichment.health(), crate::lsp::LspHealth::Disabled) {
            GeneratorDegradeReason::Disabled
        } else {
            GeneratorDegradeReason::Unavailable
        };
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::Lsp, reason),
        ));
    }
    let mut out = Vec::new();
    for fact in enrichment.facts() {
        if out.len() as u32 >= limit {
            break;
        }
        out.push(candidate_from_lsp(fact, out.len() as u32));
    }
    let degrade = if enrichment.is_degraded() {
        Some(GeneratorDegradeReason::Unavailable)
    } else {
        None
    };
    Ok(finish(GeneratorKind::Lsp, out, degrade))
}

fn generate_diff_error(
    query: &ContextQuery,
    timeout: Duration,
    limit: u32,
) -> Result<(Vec<ContextCandidate>, GeneratorStatus), CandidateError> {
    let started = Instant::now();
    if let Some(reason) = begin_generator(&query.cancel, timeout, started)? {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::DiffError, reason),
        ));
    }
    let mut out = Vec::new();
    for (step, item) in query.diff_errors.iter().enumerate() {
        if let Some(reason) = stride_check(step, &query.cancel, timeout, started)? {
            return Ok(finish(GeneratorKind::DiffError, out, Some(reason)));
        }
        if out.len() as u32 >= limit {
            break;
        }
        let reason = match item.kind {
            DiffErrorKind::Diff => InclusionReason::Diff,
            DiffErrorKind::TestFailure | DiffErrorKind::Diagnostic => InclusionReason::Error,
        };
        out.push(ContextCandidate {
            source: CandidateSource::DiffError,
            rank: (out.len() as u32).saturating_add(1),
            score: 1.0,
            reason,
            repo_id: item.repo_id.or(query.repo_id),
            path: Some(item.path.clone()),
            language: None,
            chunk_id: item.chunk_id.clone(),
            content_hash: item.content_hash.map(|h| h.to_string()),
            start_byte: item.start_byte,
            end_byte: item.end_byte,
            symbol: None,
            freshness: Freshness::Fresh,
            trust: TrustClass::Untrusted,
        });
    }
    Ok(finish(GeneratorKind::DiffError, out, None))
}

fn generate_read_set(
    query: &ContextQuery,
    timeout: Duration,
    limit: u32,
) -> Result<(Vec<ContextCandidate>, GeneratorStatus), CandidateError> {
    let started = Instant::now();
    if let Some(reason) = begin_generator(&query.cancel, timeout, started)? {
        return Ok((
            Vec::new(),
            GeneratorStatus::degraded(GeneratorKind::ReadSet, reason),
        ));
    }
    let mut out = Vec::new();
    for (step, item) in query.read_set.iter().enumerate() {
        if let Some(reason) = stride_check(step, &query.cancel, timeout, started)? {
            return Ok(finish(GeneratorKind::ReadSet, out, Some(reason)));
        }
        if !item.is_changed() {
            continue;
        }
        if out.len() as u32 >= limit {
            break;
        }
        out.push(ContextCandidate {
            source: CandidateSource::ReadSet,
            rank: (out.len() as u32).saturating_add(1),
            score: 0.35,
            reason: InclusionReason::ReadSetChanged,
            repo_id: item.repo_id.or(query.repo_id),
            path: Some(item.path.clone()),
            language: None,
            chunk_id: item.chunk_id.clone(),
            content_hash: Some(item.current_hash.to_string()),
            start_byte: Some(item.start_byte),
            end_byte: Some(item.end_byte),
            symbol: None,
            freshness: Freshness::Stale,
            trust: TrustClass::Untrusted,
        });
    }
    Ok(finish(GeneratorKind::ReadSet, out, None))
}

fn candidate_from_fts(hit: &FtsHit, rank0: u32) -> ContextCandidate {
    ContextCandidate {
        source: CandidateSource::Lexical,
        rank: rank0.saturating_add(1),
        score: fts_score(hit.score()),
        reason: InclusionReason::Lexical,
        repo_id: Some(hit.repo_id()),
        path: Some(hit.path().clone()),
        language: hit.language(),
        chunk_id: Some(hit.chunk_id().to_string()),
        content_hash: Some(hit.content_hash().to_string()),
        start_byte: Some(hit.start_byte()),
        end_byte: Some(hit.end_byte()),
        symbol: None,
        freshness: Freshness::Unknown,
        trust: TrustClass::Untrusted,
    }
}

fn candidate_from_vector(hit: &VectorHit, rank0: u32) -> ContextCandidate {
    ContextCandidate {
        source: CandidateSource::Vector,
        rank: rank0.saturating_add(1),
        score: hit.score(),
        reason: InclusionReason::Vector,
        repo_id: Some(hit.repo_id()),
        path: Some(hit.path().clone()),
        language: hit.language(),
        chunk_id: Some(hit.chunk_id().to_string()),
        content_hash: Some(hit.content_hash().to_string()),
        start_byte: Some(hit.start_byte()),
        end_byte: Some(hit.end_byte()),
        symbol: None,
        freshness: Freshness::Unknown,
        trust: TrustClass::Untrusted,
    }
}

fn candidate_from_lsp(fact: &LspFact, rank0: u32) -> ContextCandidate {
    ContextCandidate {
        source: CandidateSource::Symbol,
        rank: rank0.saturating_add(1),
        score: 0.85,
        reason: InclusionReason::Lsp,
        repo_id: Some(fact.repo_id()),
        path: Some(fact.path().clone()),
        language: None,
        chunk_id: None,
        content_hash: Some(fact.content_hash().to_string()),
        start_byte: Some(fact.range().start_byte()),
        end_byte: Some(fact.range().end_byte()),
        symbol: Some(fact.name().to_string()),
        freshness: Freshness::Unknown,
        trust: TrustClass::Untrusted,
    }
}

fn push_graph_edges(
    out: &mut Vec<ContextCandidate>,
    seen: &mut BTreeSet<(RepoId, String)>,
    edges: &[GraphEdge],
    limit: u32,
) {
    for edge in edges {
        if out.len() as u32 >= limit {
            return;
        }
        push_graph_node(
            out,
            seen,
            edge.to().repo_id(),
            edge.to().as_str(),
            edge.confidence(),
            limit,
        );
        if out.len() as u32 >= limit {
            return;
        }
        push_graph_node(
            out,
            seen,
            edge.from().repo_id(),
            edge.from().as_str(),
            edge.confidence(),
            limit,
        );
    }
}

fn push_graph_node(
    out: &mut Vec<ContextCandidate>,
    seen: &mut BTreeSet<(RepoId, String)>,
    repo_id: RepoId,
    locator: &str,
    score: f32,
    limit: u32,
) {
    if out.len() as u32 >= limit {
        return;
    }
    if !seen.insert((repo_id, locator.to_string())) {
        return;
    }
    let path = locator_path(locator);
    out.push(ContextCandidate {
        source: CandidateSource::Graph,
        rank: (out.len() as u32).saturating_add(1),
        score,
        reason: InclusionReason::Graph,
        repo_id: Some(repo_id),
        path,
        language: None,
        chunk_id: None,
        content_hash: None,
        start_byte: None,
        end_byte: None,
        symbol: Some(locator.to_string()),
        freshness: Freshness::Unknown,
        trust: TrustClass::Untrusted,
    });
}

fn hint_locators(name: &str) -> Vec<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.contains('\0') || trimmed.len() > crate::index::graph::MAX_LOCATOR_BYTES {
        return Vec::new();
    }
    let mut out = Vec::new();
    if trimmed.contains('#') {
        out.push(trimmed.to_string());
    } else {
        let mut prefixed = String::with_capacity(NAME_PREFIX.len() + trimmed.len());
        prefixed.push_str(NAME_PREFIX);
        prefixed.push_str(trimmed);
        out.push(prefixed);
        if trimmed.contains("::") {
            let mut fq = String::with_capacity(FQ_PREFIX.len() + trimmed.len());
            fq.push_str(FQ_PREFIX);
            fq.push_str(trimmed);
            out.push(fq);
        }
        out.push(trimmed.to_string());
    }
    out
}

fn locator_path(locator: &str) -> Option<RepoPath> {
    let raw = locator.split('#').next().unwrap_or(locator);
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    RepoPath::parse(raw).ok()
}

fn fts_score(bm25: f64) -> f32 {
    // SQLite BM25 is ascending (better is more negative). Invert for fusion.
    let inverted = if bm25.is_finite() { -bm25 } else { 0.0 };
    inverted as f32
}

fn search_languages(query: &ContextQuery) -> Vec<Option<SourceLanguage>> {
    if query.languages.is_empty() {
        vec![None]
    } else {
        query.languages.iter().copied().map(Some).collect()
    }
}

fn cap(source_limit: u32, query: &ContextQuery) -> u32 {
    source_limit.min(query.max_candidates)
}

fn append_capped(dst: &mut Vec<ContextCandidate>, extra: Vec<ContextCandidate>, max: u32) {
    for item in extra {
        if dst.len() as u32 >= max {
            break;
        }
        dst.push(item);
    }
}

fn check_cancel(
    query_cancel: &CancellationToken,
    limits_cancel: &CancellationToken,
) -> Result<(), CandidateError> {
    if query_cancel.is_cancelled() || limits_cancel.is_cancelled() {
        Err(CandidateError::Cancelled)
    } else {
        Ok(())
    }
}

fn begin_generator(
    cancel: &CancellationToken,
    timeout: Duration,
    started: Instant,
) -> Result<Option<GeneratorDegradeReason>, CandidateError> {
    check_generator(cancel, timeout, started)
}

fn stride_check(
    step: usize,
    cancel: &CancellationToken,
    timeout: Duration,
    started: Instant,
) -> Result<Option<GeneratorDegradeReason>, CandidateError> {
    if step.is_multiple_of(CANCEL_STRIDE) {
        check_generator(cancel, timeout, started)
    } else {
        Ok(None)
    }
}

fn check_generator(
    cancel: &CancellationToken,
    timeout: Duration,
    started: Instant,
) -> Result<Option<GeneratorDegradeReason>, CandidateError> {
    if cancel.is_cancelled() {
        return Err(CandidateError::Cancelled);
    }
    if timeout.is_zero() || started.elapsed() > timeout {
        return Ok(Some(GeneratorDegradeReason::Timeout));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use protocol::RedactionClass;

    use crate::chunk::{ChunkPolicy, Document, chunk};
    use crate::index::fts::FtsDocument;
    use crate::index::graph::{GraphDocument, GraphLimits};
    use crate::index::vector::{EmbeddingProvider, EmbeddingVersion, VectorDocument, VectorLimits};
    use crate::lsp::{
        DocumentSnapshot, LspClient, LspConfigOrigin, LspFactKind, LspLimits, LspRawBatch,
        LspRawLocation, LspRequest, LspServerConfig, LspTransportError, ProjectTrust,
    };
    use crate::parse::registry::{ParseBudget, ParserRegistry};
    use crate::parse::symbols::{SymbolBudget, extract_from_parse};

    struct HashEmbeddingProvider {
        version: EmbeddingVersion,
        dims: u32,
    }

    impl HashEmbeddingProvider {
        fn new(dims: u32) -> Self {
            Self {
                version: EmbeddingVersion::new("hash-v1").expect("version"),
                dims,
            }
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
            Ok(texts
                .iter()
                .map(|text| hash_vector(text, self.dims as usize))
                .collect())
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

    struct ScriptedLsp {
        responses: Mutex<Vec<Result<LspRawBatch, LspTransportError>>>,
        calls: AtomicUsize,
    }

    impl ScriptedLsp {
        fn new(responses: Vec<Result<LspRawBatch, LspTransportError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl LspClient for ScriptedLsp {
        fn query(
            &self,
            _request: &LspRequest,
            cancel: &CancellationToken,
        ) -> Result<LspRawBatch, LspTransportError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if cancel.is_cancelled() {
                return Err(LspTransportError::Cancelled);
            }
            let mut queue = self.responses.lock().expect("script lock");
            if queue.is_empty() {
                return Err(LspTransportError::Unavailable);
            }
            queue.remove(0)
        }
    }

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn hash(text: &str) -> ContentHash {
        ContentHash::from_bytes(text.as_bytes())
    }

    fn rust_chunks(
        repo: RepoId,
        rel: &str,
        src: &str,
    ) -> (Document, Vec<crate::chunk::ChunkRecord>) {
        let document = Document::new(repo, path(rel), Some(SourceLanguage::Rust), src);
        let policy = ChunkPolicy::new().overlap_bytes(0).overlap_tokens(0);
        let chunks = chunk(&document, &[], &policy).expect("chunk");
        assert!(!chunks.is_empty());
        (document, chunks)
    }

    fn fts_from(repo: RepoId, rel: &str, src: &str) -> FtsDocument {
        let (_doc, chunks) = rust_chunks(repo, rel, src);
        let mut document = FtsDocument::new(repo, path(rel), Some(SourceLanguage::Rust));
        document.push_records(&chunks, &[]).expect("push");
        document
    }

    fn vector_from(repo: RepoId, rel: &str, src: &str) -> VectorDocument {
        let (_doc, chunks) = rust_chunks(repo, rel, src);
        let mut document = VectorDocument::new(repo, path(rel), Some(SourceLanguage::Rust));
        document
            .push_records(&chunks, RedactionClass::Public)
            .expect("push");
        document
    }

    fn graph_from(repo: RepoId, rel: &str, src: &str) -> GraphDocument {
        let outcome =
            ParserRegistry::parse(SourceLanguage::Rust, src.as_bytes(), &ParseBudget::new());
        let symbols =
            extract_from_parse(&outcome, &path(rel), src.as_bytes(), &SymbolBudget::new())
                .expect("extract");
        GraphDocument::from_symbols(
            repo,
            path(rel),
            hash(src),
            Some(SourceLanguage::Rust),
            symbols,
        )
    }

    fn embed_text(provider: &HashEmbeddingProvider, text: &str) -> Vec<f32> {
        provider
            .embed(&[text], &CancellationToken::new())
            .expect("embed")
            .remove(0)
    }

    #[test]
    fn generate_candidates_records_reason_for_each_source() {
        let repo = RepoId::new();
        let helper_src = "fn helper() { 1 }\n";
        let caller_src = "fn caller() { helper(); }\n";

        let mut fts = FtsIndex::open_in_memory(Default::default()).expect("fts");
        fts.upsert_document(&fts_from(repo, "src/helper.rs", helper_src))
            .expect("fts helper");
        fts.upsert_document(&fts_from(repo, "src/caller.rs", caller_src))
            .expect("fts caller");

        let provider = HashEmbeddingProvider::new(8);
        let mut vector = VectorIndex::open_in_memory(VectorLimits::new()).expect("vector");
        vector
            .upsert_document(&vector_from(repo, "src/helper.rs", helper_src), &provider)
            .expect("vector helper");

        let mut graph = CodeGraph::open_in_memory(GraphLimits::new()).expect("graph");
        graph
            .upsert_document(&graph_from(repo, "src/helper.rs", helper_src))
            .expect("graph helper");
        graph
            .upsert_document(&graph_from(repo, "src/caller.rs", caller_src))
            .expect("graph caller");

        let query = ContextQuery::new("helper")
            .repo(repo)
            .embedding(embed_text(&provider, helper_src))
            .symbol_hint(SymbolHint::new("helper").repo(repo))
            .pin(
                ExplicitPin::new(path("src/helper.rs"))
                    .repo(repo)
                    .symbol("helper"),
            )
            .diff_error(
                DiffErrorEvidence::new(path("src/helper.rs"), DiffErrorKind::TestFailure)
                    .repo(repo)
                    .range(0, 8),
            )
            .read_set_item(
                ReadSetItem::new(path("src/caller.rs"), 0, 8, hash("old"), hash(caller_src))
                    .repo(repo),
            );

        let pools = generate_candidates(
            &query,
            CandidateSources::new()
                .fts(&fts)
                .vector(&vector)
                .graph(&graph),
            &CandidateLimits::new(),
        )
        .expect("generate");

        assert!(!pools.explicit().is_empty());
        assert_eq!(pools.explicit()[0].reason(), InclusionReason::Explicit);
        assert_eq!(pools.explicit()[0].source(), CandidateSource::Explicit);

        assert!(!pools.fts().is_empty());
        assert_eq!(pools.fts()[0].reason(), InclusionReason::Lexical);
        assert_eq!(pools.fts()[0].rank(), 1);
        assert!(
            pools
                .fts()
                .iter()
                .any(|c| c.path().map(RepoPath::as_str) == Some("src/helper.rs")),
            "lexical pool should include the labeled helper file",
        );

        assert!(!pools.vector().is_empty());
        assert_eq!(pools.vector()[0].reason(), InclusionReason::Vector);

        assert!(!pools.graph().is_empty());
        assert!(
            pools
                .graph()
                .iter()
                .all(|c| c.reason() == InclusionReason::Graph)
        );
        assert!(
            pools
                .graph()
                .iter()
                .any(|c| c.symbol() == Some("#name:helper"))
        );

        assert_eq!(pools.diff_error().len(), 1);
        assert_eq!(pools.diff_error()[0].reason(), InclusionReason::Error);

        assert_eq!(pools.read_set().len(), 1);
        assert_eq!(
            pools.read_set()[0].reason(),
            InclusionReason::ReadSetChanged
        );
        assert_eq!(pools.read_set()[0].freshness(), Freshness::Stale);
    }

    #[test]
    fn per_source_caps_are_independent() {
        let repo = RepoId::new();
        let mut fts = FtsIndex::open_in_memory(Default::default()).expect("fts");
        for i in 0..4 {
            let src = format!("shared_token body_{i}\n");
            fts.upsert_document(&fts_from(repo, &format!("src/f{i}.rs"), &src))
                .expect("upsert");
        }

        let query = ContextQuery::new("shared_token")
            .repo(repo)
            .pin(ExplicitPin::new(path("src/f0.rs")))
            .pin(ExplicitPin::new(path("src/f1.rs")))
            .pin(ExplicitPin::new(path("src/f2.rs")))
            .diff_error(DiffErrorEvidence::new(
                path("src/f0.rs"),
                DiffErrorKind::Diff,
            ))
            .diff_error(DiffErrorEvidence::new(
                path("src/f1.rs"),
                DiffErrorKind::Diff,
            ))
            .max_candidates(32);

        let limits = CandidateLimits::new()
            .fts_limit(2)
            .explicit_limit(1)
            .diff_limit(1);
        let pools =
            generate_candidates(&query, CandidateSources::new().fts(&fts), &limits).expect("gen");
        assert_eq!(pools.fts().len(), 2);
        assert_eq!(pools.explicit().len(), 1);
        assert_eq!(pools.diff_error().len(), 1);
        assert!(pools.vector().is_empty());
    }

    #[test]
    fn optional_vector_timeout_preserves_fts() {
        let repo = RepoId::new();
        let src = "fn unique_alpha() { 1 }\n";
        let mut fts = FtsIndex::open_in_memory(Default::default()).expect("fts");
        fts.upsert_document(&fts_from(repo, "src/lib.rs", src))
            .expect("upsert");

        let provider = HashEmbeddingProvider::new(8);
        let mut vector = VectorIndex::open_in_memory(VectorLimits::new()).expect("vector");
        vector
            .upsert_document(&vector_from(repo, "src/lib.rs", src), &provider)
            .expect("vector");

        let query = ContextQuery::new("unique_alpha")
            .repo(repo)
            .embedding(embed_text(&provider, src));
        let limits = CandidateLimits::new().vector_timeout(Duration::ZERO);
        let pools = generate_candidates(
            &query,
            CandidateSources::new().fts(&fts).vector(&vector),
            &limits,
        )
        .expect("generate");

        assert!(!pools.fts().is_empty());
        assert!(pools.vector().is_empty());
        let vector_status = pools.status(GeneratorKind::Vector).expect("vector status");
        assert_eq!(
            vector_status.degrade(),
            Some(GeneratorDegradeReason::Timeout)
        );
        assert!(!pools.status(GeneratorKind::Fts).expect("fts").is_degraded());
    }

    #[test]
    fn disabled_vector_preserves_other_generators() {
        let repo = RepoId::new();
        let src = "fn unique_beta() { 2 }\n";
        let mut fts = FtsIndex::open_in_memory(Default::default()).expect("fts");
        fts.upsert_document(&fts_from(repo, "src/lib.rs", src))
            .expect("upsert");

        let vector = VectorIndex::disabled();
        let query = ContextQuery::new("unique_beta")
            .repo(repo)
            .embedding(vec![0.1, 0.2, 0.3]);
        let pools = generate_candidates(
            &query,
            CandidateSources::new().fts(&fts).vector(&vector),
            &CandidateLimits::new(),
        )
        .expect("generate");
        assert!(!pools.fts().is_empty());
        assert!(pools.vector().is_empty());
        assert_eq!(
            pools
                .status(GeneratorKind::Vector)
                .and_then(|s| s.degrade()),
            Some(GeneratorDegradeReason::Disabled)
        );
    }

    #[test]
    fn missing_optional_indexes_do_not_drop_explicit_or_read_set() {
        let query = ContextQuery::new("anything")
            .pin(ExplicitPin::new(path("src/lib.rs")))
            .read_set_item(ReadSetItem::new(
                path("src/lib.rs"),
                0,
                4,
                hash("a"),
                hash("b"),
            ));
        let pools = generate_candidates(&query, CandidateSources::new(), &CandidateLimits::new())
            .expect("generate");
        assert_eq!(pools.explicit().len(), 1);
        assert_eq!(pools.read_set().len(), 1);
        assert!(pools.fts().is_empty());
        assert_eq!(
            pools.status(GeneratorKind::Fts).and_then(|s| s.degrade()),
            Some(GeneratorDegradeReason::Unavailable)
        );
        assert!(
            !pools
                .status(GeneratorKind::Explicit)
                .expect("explicit")
                .is_degraded()
        );
    }

    #[test]
    fn unchanged_read_set_items_are_skipped() {
        let same = hash("same");
        let query = ContextQuery::new("").read_set_item(ReadSetItem::new(
            path("src/a.rs"),
            0,
            1,
            same,
            same,
        ));
        let pools = generate_candidates(&query, CandidateSources::new(), &CandidateLimits::new())
            .expect("generate");
        assert!(pools.read_set().is_empty());
        assert_eq!(
            pools.status(GeneratorKind::ReadSet).map(|s| s.produced()),
            Some(0)
        );
    }

    #[test]
    fn lsp_failure_preserves_lexical_and_graph() {
        let repo = RepoId::new();
        let src = "fn helper() {}\nfn caller() { helper(); }\n";
        let mut fts = FtsIndex::open_in_memory(Default::default()).expect("fts");
        fts.upsert_document(&fts_from(repo, "src/lib.rs", src))
            .expect("fts");
        let mut graph = CodeGraph::open_in_memory(GraphLimits::new()).expect("graph");
        graph
            .upsert_document(&graph_from(repo, "src/lib.rs", src))
            .expect("graph");

        let client = ScriptedLsp::new(vec![Err(LspTransportError::Unavailable)]);
        let mut enricher = LspEnricher::new(
            client,
            vec![LspServerConfig::new(
                SourceLanguage::Rust,
                LspConfigOrigin::Host,
            )],
            ProjectTrust::Trusted,
            None,
            LspLimits::new(),
        );
        let snapshot = DocumentSnapshot::new(path("src/lib.rs"), hash(src), 1).with_text(src);
        let lsp_query = SymbolQuery::new(
            repo,
            path("src/lib.rs"),
            SourceLanguage::Rust,
            "helper",
            snapshot,
        );
        let query = ContextQuery::new("helper")
            .repo(repo)
            .symbol_hint(SymbolHint::new("helper").repo(repo))
            .lsp_query(lsp_query);

        let pools = generate_candidates(
            &query,
            CandidateSources::new()
                .fts(&fts)
                .graph(&graph)
                .lsp(&mut enricher),
            &CandidateLimits::new(),
        )
        .expect("generate");
        assert!(!pools.fts().is_empty());
        assert!(!pools.graph().is_empty());
        assert_eq!(
            pools.status(GeneratorKind::Lsp).and_then(|s| s.degrade()),
            Some(GeneratorDegradeReason::Unavailable)
        );
    }

    #[test]
    fn lsp_facts_are_appended_with_lsp_reason() {
        let repo = RepoId::new();
        let src = "fn helper() {}\n";
        let loc = LspRawLocation::new("file:///workspace/src/lib.rs", LspFactKind::Definition)
            .name("helper")
            .bytes(0, 12)
            .document_version(1)
            .content_hash(hash(src));
        let client = ScriptedLsp::new(vec![Ok(LspRawBatch::new(vec![loc]))]);
        let mut enricher = LspEnricher::new(
            client,
            vec![LspServerConfig::new(
                SourceLanguage::Rust,
                LspConfigOrigin::Host,
            )],
            ProjectTrust::Trusted,
            Some(std::path::PathBuf::from("/workspace")),
            LspLimits::new(),
        );
        let snapshot = DocumentSnapshot::new(path("src/lib.rs"), hash(src), 1).with_text(src);
        let lsp_query = SymbolQuery::new(
            repo,
            path("src/lib.rs"),
            SourceLanguage::Rust,
            "helper",
            snapshot,
        );
        let query = ContextQuery::new("").lsp_query(lsp_query);
        let pools = generate_candidates(
            &query,
            CandidateSources::new().lsp(&mut enricher),
            &CandidateLimits::new(),
        )
        .expect("generate");
        assert!(
            pools
                .graph()
                .iter()
                .any(|c| c.reason() == InclusionReason::Lsp)
        );
        assert_eq!(
            pools.status(GeneratorKind::Lsp).map(|s| s.produced()),
            Some(1)
        );
    }

    #[test]
    fn cancelled_query_fails_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = generate_candidates(
            &ContextQuery::new("x").cancellation(cancel),
            CandidateSources::new(),
            &CandidateLimits::new(),
        )
        .expect_err("cancelled");
        assert_eq!(err, CandidateError::Cancelled);
        assert_eq!(err.to_string(), "cancelled");
    }

    #[test]
    fn invalid_policy_and_query_are_typed() {
        assert_eq!(
            generate_candidates(
                &ContextQuery::new("x"),
                CandidateSources::new(),
                &CandidateLimits::new().fts_limit(0),
            )
            .expect_err("policy"),
            CandidateError::InvalidPolicy
        );
        assert_eq!(
            generate_candidates(
                &ContextQuery::new("x").max_candidates(0),
                CandidateSources::new(),
                &CandidateLimits::new(),
            )
            .expect_err("query"),
            CandidateError::InvalidQuery
        );
        assert_eq!(
            generate_candidates(
                &ContextQuery::new("bad\0term"),
                CandidateSources::new(),
                &CandidateLimits::new(),
            )
            .expect_err("nul"),
            CandidateError::InvalidQuery
        );
    }

    #[test]
    fn empty_lexical_skips_fts_without_degrade() {
        let pools = generate_candidates(
            &ContextQuery::new("   ").pin(ExplicitPin::new(path("src/a.rs"))),
            CandidateSources::new(),
            &CandidateLimits::new(),
        )
        .expect("generate");
        assert_eq!(pools.explicit().len(), 1);
        assert!(pools.fts().is_empty());
        assert!(!pools.status(GeneratorKind::Fts).expect("fts").is_degraded());
    }

    #[test]
    fn display_never_echoes_query_or_paths() {
        let err = CandidateError::InvalidQuery;
        assert_eq!(err.to_string(), "invalid_query");
        assert!(!err.to_string().contains('/'));
        assert_eq!(InclusionReason::Lexical.as_str(), "lexical");
        assert_eq!(CandidateSource::Graph.to_string(), "graph");
    }

    #[test]
    fn fts_score_inverts_sqlite_bm25_and_drops_non_finite() {
        assert!(fts_score(-12.0) > fts_score(-3.0));
        assert_eq!(fts_score(-4.0), 4.0);
        assert_eq!(fts_score(f64::NAN), 0.0);
        assert_eq!(fts_score(f64::INFINITY), 0.0);
        assert_eq!(fts_score(f64::NEG_INFINITY), 0.0);
    }
}
