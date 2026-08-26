#![forbid(unsafe_code)]

pub mod chunk;
pub mod compact;
pub mod compile;
pub mod need;
pub mod index {
    pub mod fts;
    pub mod graph;
    pub mod vector;
}
pub mod ingest {
    pub mod content;
    pub mod pipeline;
    pub mod walk;
    pub mod watch;
}
pub mod lsp;
pub mod memory;
pub mod parse {
    pub mod registry;
    pub mod symbols;
}
pub mod read;
pub mod read_set;
pub mod repo_manifest;
pub mod scout;
pub mod token_estimate;
pub mod retrieval {
    pub mod candidates;
    pub mod filter;
    pub mod grep;
    pub mod links;
    pub mod rank;
}

pub use chunk::{
    ChunkError, ChunkId, ChunkKind, ChunkPolicy, ChunkRecord, DEFAULT_CHUNK_TIMEOUT,
    DEFAULT_MAX_CHUNK_BYTES, DEFAULT_MAX_CHUNK_TOKENS, DEFAULT_MAX_CHUNKS,
    DEFAULT_MAX_SOURCE_BYTES, DEFAULT_OVERLAP_BYTES, DEFAULT_OVERLAP_TOKENS, Document, chunk,
};
pub use compact::{
    CompactError, CompactMethod, CompactedContext, PacketSummarizer, compact_packet,
};
pub use compile::{
    CompileContext, CompileError, CompileInput, CompileLimits, CompileReason, ContextBlock,
    ContextExplain, ContextPacket, ContextSource, DEFAULT_COMPILE_TIMEOUT, DEFAULT_MAX_BLOCK_BYTES,
    DEFAULT_MAX_COMPILE_BLOCKS, DEFAULT_MEMORY_SHARE_BPS, DEFAULT_READ_SET_SHARE_BPS,
    DEFAULT_RETRIEVED_SHARE_BPS, DEFAULT_SAFETY_MARGIN, DropReason, DroppedBlock, PartitionBudget,
    TokenPartitions, compile, explain_packet,
};
pub use index::fts::{
    DEFAULT_FTS_TIMEOUT, DEFAULT_MAX_FTS_RESULTS, DEFAULT_MAX_QUERY_BYTES, DEFAULT_MAX_QUERY_TERMS,
    DEFAULT_SEARCH_LIMIT, FtsChunk, FtsDocument, FtsError, FtsHit, FtsIndex, FtsLimits, FtsQuery,
    FtsWriteStats,
};
pub use index::graph::{
    CodeGraph, DEFAULT_GRAPH_TIMEOUT, DEFAULT_MAX_GRAPH_EDGES, DEFAULT_MAX_GRAPH_HOPS,
    DEFAULT_MAX_GRAPH_NODES, DEFAULT_MAX_GRAPH_RESULTS, DEFAULT_MAX_GRAPH_SYMBOLS, GraphDocument,
    GraphEdge, GraphEdgeKind, GraphEdgeSource, GraphError, GraphLimits, GraphWriteStats,
    MAX_LOCATOR_BYTES, SymbolLocator,
};
pub use index::vector::{
    DEFAULT_MAX_VECTOR_DIMS, DEFAULT_MAX_VECTOR_RESULTS, DEFAULT_VECTOR_TIMEOUT, EmbeddingProvider,
    EmbeddingVersion, EnabledVectorIndex, MAX_VERSION_BYTES, VectorChunk, VectorDegradeReason,
    VectorDocument, VectorError, VectorHealth, VectorHit, VectorIndex, VectorLimits, VectorQuery,
    VectorWriteStats, embedding_text,
};
pub use ingest::content::{
    CONTENT_HASH_HEX_LEN, CONTENT_HASH_LEN, CONTENT_HASH_PREFIX, ContentClass, ContentError,
    ContentHash, ContentLimits, HASH_CHUNK_BYTES, LoadedCandidate, SourceLanguage, load_candidate,
    load_file_candidate,
};
pub use ingest::pipeline::{
    DEFAULT_PIPELINE_TIMEOUT, FileIndexState, GenerationStatus, IndexOutcome, IndexPipeline,
    PipelineError, PipelineLimits,
};
pub use ingest::walk::{
    DEFAULT_BINARY_PROBE_BYTES, DEFAULT_MAX_DEPTH, DEFAULT_MAX_FILE_BYTES, FileCandidate,
    FileMetadata, FileWalker, IndexEligibility, MAX_IGNORE_FILE_BYTES, MetadataOnlyReason,
    RepoScope, WalkError, WalkLimits, walk_manifest, walk_repo,
};
pub use ingest::watch::{
    DEFAULT_DEBOUNCE, DEFAULT_MAX_EVENTS, DEFAULT_MAX_HOLD, DEFAULT_MAX_PENDING_PATHS,
    DEFAULT_WATCH_TIMEOUT, IndexJob, PathAction, WatchCoalescer, WatchError, WatchEvent,
    WatchLimits, WatchScope, apply_index_jobs,
};
pub use lsp::{
    DEFAULT_CRASH_BACKOFF, DEFAULT_LSP_TIMEOUT, DEFAULT_MAX_LSP_DETAIL_BYTES,
    DEFAULT_MAX_LSP_FACTS, DEFAULT_MAX_LSP_NAME_BYTES, DEFAULT_MAX_LSP_RESPONSE_BYTES,
    DEFAULT_MAX_LSP_URI_BYTES, DocumentSnapshot, LspClient, LspConfigOrigin, LspDegradeReason,
    LspEnricher, LspEnrichment, LspError, LspFact, LspFactKind, LspFactSource, LspHealth,
    LspHealthEvent, LspLimits, LspQueryKinds, LspRange, LspRawBatch, LspRawLocation, LspRequest,
    LspServerConfig, LspTransportError, ProjectTrust, SymbolQuery,
};
pub use memory::{
    DEFAULT_MAX_MEMORY_BYTES, DEFAULT_MAX_MEMORY_RECORDS, DEFAULT_MAX_MEMORY_RESULTS,
    DEFAULT_MEMORY_LIMIT, DEFAULT_MEMORY_TIMEOUT, MAX_SOURCE_ID_BYTES, MEMORY_SOURCE_SCHEMA,
    MEMORY_WRITTEN_KIND, MemoryError, MemoryId, MemoryLimits, MemoryQuery, MemoryRecord,
    MemoryRole, MemoryScope, MemoryScopeKind, MemorySource, MemorySourceKind, MemoryStore,
    MemoryTimestamp, MemoryTimestampParseError, MemoryWrite, MemoryWritten,
};
pub use need::{
    CodeRef, CompletenessRequirement, InformationNeed, MAX_ANCHORS, MAX_NEED_QUESTIONS,
    MAX_QUESTION_BYTES, MAX_REF_BYTES, MAX_SCOPE_PATHS, NeedError, NegativeClaimPolicy, ScopeSet,
};
pub use parse::registry::{
    DEFAULT_MAX_PARSE_BYTES, DEFAULT_MAX_TREE_NODES, DEFAULT_PARSE_TIMEOUT, ParseBudget,
    ParseDegradeReason, ParseOutcome, ParseStrategy, ParseTree, ParserEntry, ParserRegistry,
    StructuralConfidence,
};
pub use parse::symbols::{
    DEFAULT_EXTRACT_TIMEOUT, DEFAULT_MAX_EXTRACT_BYTES, DEFAULT_MAX_NAME_BYTES,
    DEFAULT_MAX_REFERENCES, DEFAULT_MAX_SYMBOLS, DEFAULT_MAX_WALK_DEPTH, SourceRange, SymbolBudget,
    SymbolError, SymbolKind, SymbolRecord, extract_from_parse, extract_symbols,
};
pub use read::{
    DEFAULT_MAX_CHARS_PER_LINE, DEFAULT_MAX_READ_BYTES, DEFAULT_MAX_READ_LINES,
    DEFAULT_MAX_READ_TOKENS, DEFAULT_READ_TIMEOUT, ReadCompleteness, ReadCursor, ReadError,
    ReadLimits, ReadRequest, ReadSlice, TruncationReason, read_repo,
};
pub use read_set::{
    ContextLocator, DEFAULT_MAX_READ_RECORDS, DEFAULT_READ_SET_TIMEOUT, ReadObservation,
    ReadRecord, ReadSet, ReadSetError, ReadSetLimits, RecordOutcome,
};
pub use repo_manifest::{
    CancellationToken, CanonicalRoot, MANIFEST_SCHEMA, MAX_ALIAS_BYTES, MAX_MANIFEST_BYTES,
    MAX_REPOS, MAX_ROOT_BYTES, ManifestError, RepoAccessMode, RepoAlias, RepoSpec,
    WorkspaceManifest,
};
pub use retrieval::candidates::{
    CandidateError, CandidateLimits, CandidatePools, CandidateSource, CandidateSources,
    ContextCandidate, ContextQuery, DEFAULT_CANDIDATE_TIMEOUT, DEFAULT_GRAPH_CANDIDATE_HOPS,
    DEFAULT_MAX_CANDIDATES, DEFAULT_PER_SOURCE_LIMIT, DiffErrorEvidence, DiffErrorKind,
    ExplicitPin, Freshness, GeneratorDegradeReason, GeneratorKind, GeneratorStatus,
    InclusionReason, LspCandidateSource, ReadSetItem, SymbolHint, TrustClass, generate_candidates,
};
pub use retrieval::filter::{
    DEFAULT_FILTER_TIMEOUT, FilterError, RetrievalFilter, filter_pools, filter_ranked,
};
pub use retrieval::grep::{
    DEFAULT_GREP_TIMEOUT, DEFAULT_MAX_GREP_HITS, DEFAULT_MAX_LINE_BYTES, DEFAULT_MAX_PATTERN_BYTES,
    GrepError, GrepHit, GrepLimits, GrepMode, GrepQuery, grep_search,
};
pub use retrieval::links::{
    CodeLink, DEFAULT_LINK_TIMEOUT, DEFAULT_MAX_LINK_BYTES, DEFAULT_MAX_LINKS, LinkError, LinkKind,
    LinkLimits, extract_links,
};
pub use retrieval::rank::{
    DEFAULT_RRF_K, RankError, RankWeights, RankedContextHit, rank, rank_cancelled,
};
pub use scout::{
    ContextScoutReport, DEFAULT_MAX_OPEN_QUESTIONS, DEFAULT_MAX_SCOUT_REFERENCES,
    DEFAULT_MAX_SCOUT_SNIPPETS, DEFAULT_SCOUT_TIMEOUT, NegativeConfidence, NegativeFinding,
    Reference, RetrievalMethod, ScoutError, ScoutLimits, ScoutSources, SearchScopeReport,
    SnippetRef, scout,
};
pub use token_estimate::{
    DEFAULT_ESTIMATE_TIMEOUT, DEFAULT_MAX_CACHE_ENTRIES, DEFAULT_MAX_ESTIMATE_BYTES,
    EstimateCacheKey, EstimateConfidence, FamilyTokenizer, TokenEstimate, TokenEstimateError,
    TokenEstimateLimits, TokenEstimator, TokenizerFamily,
};
