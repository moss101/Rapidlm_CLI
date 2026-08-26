//! Gold-query runner for context retrieval and compile.
//!
//! Consumes JSONL cases, measures recall/nDCG/redundant tokens/latency against
//! hybrid, rg-only, vector-only, and vector-disabled baselines, and writes a
//! machine-readable metrics artifact. Vector-disabled keeps lexical retrieval.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use context_engine::{
    CancellationToken, CandidateLimits, CandidateSources, ChunkPolicy, CompileContext,
    CompileError, CompileInput, CompileReason, ContextPacket, ContextQuery, Document,
    EmbeddingProvider, EmbeddingVersion, FtsDocument, FtsIndex, FtsLimits, GeneratorDegradeReason,
    GeneratorKind, MemoryLimits, MemoryQuery, MemoryRecord, MemoryScope, MemorySource,
    MemorySourceKind, MemoryStore, MemoryWrite, RankWeights, RankedContextHit, SourceLanguage,
    TokenEstimateError, TokenEstimator, TokenizerFamily, VectorDocument, VectorError, VectorIndex,
    VectorLimits, chunk, compile, generate_candidates, rank_cancelled,
};
use protocol::{RedactionClass, RepoId, RepoPath};
use serde::{Deserialize, Serialize};

const CASE_SCHEMA: &str = "rapidlm.context.eval.case.v1";
const CORPUS_SCHEMA: &str = "rapidlm.context.eval.corpus.v1";
const METRICS_SCHEMA: &str = "rapidlm.context.eval.metrics.v1";
const PACKET_SCHEMA: &str = "rapidlm.context.packet.v1";

const MAX_LINE_BYTES: usize = 1_048_576;
const MAX_CASES: usize = 10_000;
const EMBED_DIMS: usize = 32;
const DEFAULT_LIMIT: u32 = 20;
const DEFAULT_LAMBDA: f32 = 0.7;
const DEFAULT_CONTEXT_LIMIT: u32 = 4_096;
const DEFAULT_OUTPUT_RESERVE: u32 = 512;
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

const BUILTIN_COMPILE: &str = "\
pub fn compile_partition_budget(limit: u32) -> u32 {\n\
    // leftover_share is reserved and never borrowed.\n\
    limit.saturating_sub(64)\n\
}\n";

const BUILTIN_MEMORY: &str = "\
pub fn durable_scoped_memory(scope: &str) -> &'static str {\n\
    // provenance_ttl expires user/project/session rows.\n\
    match scope {\n\
        \"user\" => \"user\",\n\
        _ => \"session\",\n\
    }\n\
}\n";

const BUILTIN_WATCH: &str = "\
pub fn watcher_debounce_overflow() {\n\
    // coalesced watcher events do not change lexical retrieval.\n\
}\n";

/// Typed runner failure. Display never echoes query text or source bodies.
#[derive(Debug)]
enum EvalError {
    Cancelled,
    Timeout,
    InvalidCase(&'static str),
    InvalidConfig(&'static str),
    CapacityExceeded,
    LineTooLarge,
    Io(io::Error),
    Json(serde_json::Error),
    Index(&'static str),
    Compile(CompileError),
    Tokens(TokenEstimateError),
    Memory,
    Encode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
enum RetrievalMode {
    Hybrid,
    RgOnly,
    VectorOnly,
    VectorDisabled,
}

#[derive(Clone, Debug, Deserialize)]
struct GoldCase {
    #[serde(default)]
    #[allow(dead_code)]
    schema: Option<String>,
    id: String,
    query: String,
    #[serde(default)]
    task: String,
    #[serde(default)]
    relevant: Vec<RelevanceJudgment>,
    #[serde(default)]
    documents: Vec<CorpusDocument>,
    #[serde(default)]
    memory: Vec<MemorySeed>,
}

#[derive(Clone, Debug, Deserialize)]
struct CorpusLine {
    #[serde(default)]
    documents: Vec<CorpusDocument>,
    #[serde(default)]
    memory: Vec<MemorySeed>,
}

#[derive(Clone, Debug, Deserialize)]
struct CorpusDocument {
    path: String,
    text: String,
    #[serde(default)]
    language: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct MemorySeed {
    content: String,
    #[serde(default = "default_confidence")]
    confidence: f64,
}

#[derive(Clone, Debug, Deserialize)]
struct RelevanceJudgment {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    chunk_id: Option<String>,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default)]
    start_byte: Option<u32>,
    #[serde(default)]
    end_byte: Option<u32>,
}

#[derive(Clone, Debug)]
struct LoadedSuite {
    cases: Vec<GoldCase>,
    documents: Vec<CorpusDocument>,
    memory: Vec<MemorySeed>,
}

#[derive(Clone, Debug)]
struct IndexedChunk {
    chunk_id: String,
    path: String,
    symbol: Option<String>,
    start_byte: u32,
    end_byte: u32,
    start_line: u32,
    end_line: u32,
    text: String,
    tokens: u32,
}

struct IndexedCorpus {
    repo_id: RepoId,
    fts: FtsIndex,
    vector: VectorIndex,
    memory: Vec<MemoryRecord>,
    chunks: Vec<IndexedChunk>,
}

struct HashEmbedder {
    version: EmbeddingVersion,
}

#[derive(Clone, Debug, Serialize)]
struct MetricsArtifact {
    schema: &'static str,
    generated_unix_ms: u64,
    case_count: usize,
    modes: Vec<RetrievalMode>,
    aggregates: BTreeMap<String, AggregateMetrics>,
    queries: Vec<QueryMetrics>,
}

#[derive(Clone, Debug, Serialize)]
struct QueryMetrics {
    id: String,
    mode: RetrievalMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    recall_at_5: f64,
    recall_at_10: f64,
    recall_at_20: f64,
    file_recall_at_5: f64,
    file_recall_at_10: f64,
    file_recall_at_20: f64,
    ndcg_at_5: f64,
    ndcg_at_10: f64,
    ndcg_at_20: f64,
    mrr: f64,
    redundant_tokens: f64,
    redundant_token_ratio: f64,
    retrieval_latency_ms: f64,
    compile_latency_ms: f64,
    context_tokens: u32,
    packet_bytes: u64,
    hit_count: u32,
    relevant_count: u32,
    vector_status: String,
}

#[derive(Clone, Debug, Serialize)]
struct AggregateMetrics {
    recall_at_5: f64,
    recall_at_10: f64,
    recall_at_20: f64,
    file_recall_at_5: f64,
    ndcg_at_5: f64,
    ndcg_at_10: f64,
    ndcg_at_20: f64,
    mrr: f64,
    redundant_token_ratio: f64,
    retrieval_latency_ms_p50: f64,
    retrieval_latency_ms_p95: f64,
    compile_latency_ms_p50: f64,
    context_tokens_mean: f64,
    packet_bytes_mean: f64,
    query_count: u32,
    error_count: u32,
}

#[derive(Serialize)]
struct DownstreamPacket<'a> {
    schema: &'static str,
    included_tokens: u32,
    block_count: usize,
    blocks: Vec<DownstreamBlock<'a>>,
}

#[derive(Serialize)]
struct DownstreamBlock<'a> {
    locator: &'a str,
    source: &'static str,
    tokens: u32,
    bytes: usize,
    content_hash: String,
    reason: &'static str,
    freshness: &'static str,
    trust: &'static str,
    text: &'a str,
}

struct Cli {
    cases: Option<PathBuf>,
    out: Option<PathBuf>,
    limit: u32,
}

fn main() {
    if let Err(err) = run() {
        let _ = writeln!(io::stderr(), "eval_queries: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), EvalError> {
    let cancel = CancellationToken::new();
    self_check(&cancel)?;
    let cli = Cli::parse()?;
    let suite = match cli.cases.as_deref() {
        Some(path) => load_jsonl(path, &cancel)?,
        None => builtin_suite(),
    };
    let artifact = evaluate_suite(&suite, cli.limit, &cancel)?;
    emit_artifact(&artifact, cli.out.as_deref())?;
    Ok(())
}

impl Cli {
    fn parse() -> Result<Self, EvalError> {
        let mut cases = None;
        let mut out = None;
        let mut limit = DEFAULT_LIMIT;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--cases" => {
                    cases = Some(PathBuf::from(
                        args.next()
                            .ok_or(EvalError::InvalidConfig("missing --cases path"))?,
                    ));
                }
                "--out" => {
                    out = Some(PathBuf::from(
                        args.next()
                            .ok_or(EvalError::InvalidConfig("missing --out path"))?,
                    ));
                }
                "--limit" => {
                    let raw = args
                        .next()
                        .ok_or(EvalError::InvalidConfig("missing --limit value"))?;
                    limit = raw
                        .parse()
                        .map_err(|_| EvalError::InvalidConfig("invalid --limit"))?;
                    if limit == 0 {
                        return Err(EvalError::InvalidConfig("limit must be > 0"));
                    }
                }
                "--bench" | "--nocapture" | "--exact" => {}
                other if other.starts_with('-') => {
                    return Err(EvalError::InvalidConfig("unknown flag"));
                }
                _ => return Err(EvalError::InvalidConfig("unexpected argument")),
            }
        }
        Ok(Self { cases, out, limit })
    }
}

fn default_confidence() -> f64 {
    1.0
}

fn load_jsonl(path: &Path, cancel: &CancellationToken) -> Result<LoadedSuite, EvalError> {
    check_cancel(cancel)?;
    let raw = fs::read_to_string(path)?;
    parse_jsonl(&raw, cancel)
}

fn parse_jsonl(raw: &str, cancel: &CancellationToken) -> Result<LoadedSuite, EvalError> {
    let mut cases = Vec::new();
    let mut documents = Vec::new();
    let mut memory = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        if idx.is_multiple_of(16) {
            check_cancel(cancel)?;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.len() > MAX_LINE_BYTES {
            return Err(EvalError::LineTooLarge);
        }
        let value: serde_json::Value = serde_json::from_str(trimmed)?;
        let schema = value.get("schema").and_then(|v| v.as_str()).unwrap_or("");
        if schema == CORPUS_SCHEMA || (schema.is_empty() && value.get("query").is_none()) {
            let corpus: CorpusLine = serde_json::from_value(value)?;
            documents.extend(corpus.documents);
            memory.extend(corpus.memory);
            continue;
        }
        if !schema.is_empty() && schema != CASE_SCHEMA {
            return Err(EvalError::InvalidCase("unsupported eval schema"));
        }
        let case: GoldCase = serde_json::from_value(value)?;
        if case.id.is_empty() || case.query.is_empty() {
            return Err(EvalError::InvalidCase("case requires id and query"));
        }
        documents.extend(case.documents.iter().cloned());
        memory.extend(case.memory.iter().cloned());
        cases.push(case);
        if cases.len() > MAX_CASES {
            return Err(EvalError::CapacityExceeded);
        }
    }
    if cases.is_empty() {
        return Err(EvalError::InvalidCase("no gold-query cases"));
    }
    Ok(LoadedSuite {
        cases,
        documents,
        memory,
    })
}

fn builtin_suite() -> LoadedSuite {
    let documents = vec![
        CorpusDocument {
            path: "src/compile.rs".to_string(),
            text: BUILTIN_COMPILE.to_string(),
            language: Some("rust".to_string()),
        },
        CorpusDocument {
            path: "src/memory.rs".to_string(),
            text: BUILTIN_MEMORY.to_string(),
            language: Some("rust".to_string()),
        },
        CorpusDocument {
            path: "src/watch.rs".to_string(),
            text: BUILTIN_WATCH.to_string(),
            language: Some("rust".to_string()),
        },
    ];
    let cases = vec![
        GoldCase {
            schema: Some(CASE_SCHEMA.to_string()),
            id: "compile-budget".to_string(),
            query: "compile_partition_budget leftover_share".to_string(),
            task: "find compile token budgeting".to_string(),
            relevant: vec![RelevanceJudgment {
                path: Some("src/compile.rs".to_string()),
                symbol: Some("compile_partition_budget".to_string()),
                chunk_id: None,
                start_line: Some(1),
                end_line: Some(6),
                start_byte: None,
                end_byte: None,
            }],
            documents: Vec::new(),
            memory: Vec::new(),
        },
        GoldCase {
            schema: Some(CASE_SCHEMA.to_string()),
            id: "durable-memory".to_string(),
            query: "durable_scoped_memory provenance_ttl".to_string(),
            task: "find scoped memory expiry".to_string(),
            relevant: vec![RelevanceJudgment {
                path: Some("src/memory.rs".to_string()),
                symbol: Some("durable_scoped_memory".to_string()),
                chunk_id: None,
                start_line: Some(1),
                end_line: Some(8),
                start_byte: None,
                end_byte: None,
            }],
            documents: Vec::new(),
            memory: Vec::new(),
        },
    ];
    LoadedSuite {
        cases,
        documents,
        memory: vec![MemorySeed {
            content: "Prefer leftover_share hard caps over borrowing output reserve.".to_string(),
            confidence: 0.9,
        }],
    }
}

fn evaluate_suite(
    suite: &LoadedSuite,
    limit: u32,
    cancel: &CancellationToken,
) -> Result<MetricsArtifact, EvalError> {
    check_cancel(cancel)?;
    let corpus = index_suite(suite, cancel)?;
    let mut queries = Vec::new();
    for mode in RetrievalMode::ALL {
        for case in &suite.cases {
            check_cancel(cancel)?;
            queries.push(evaluate_case(&corpus, case, mode, limit, cancel)?);
        }
    }
    Ok(MetricsArtifact {
        schema: METRICS_SCHEMA,
        generated_unix_ms: unix_millis(),
        case_count: suite.cases.len(),
        modes: RetrievalMode::ALL.to_vec(),
        aggregates: aggregate(&queries),
        queries,
    })
}

fn index_suite(
    suite: &LoadedSuite,
    cancel: &CancellationToken,
) -> Result<IndexedCorpus, EvalError> {
    check_cancel(cancel)?;
    if suite.documents.is_empty() {
        return Err(EvalError::InvalidCase("suite has no documents"));
    }
    let repo_id = RepoId::new();
    let mut fts =
        FtsIndex::open_in_memory(FtsLimits::new()).map_err(|_| EvalError::Index("fts"))?;
    let mut vector =
        VectorIndex::open_in_memory(VectorLimits::new()).map_err(|_| EvalError::Index("vector"))?;
    let embedder = HashEmbedder::new()?;
    let mut estimator = TokenEstimator::new();
    let policy = ChunkPolicy::new().overlap_bytes(0).overlap_tokens(0);
    let mut chunks = Vec::new();
    let mut seen_paths = BTreeSet::new();

    for document in &suite.documents {
        check_cancel(cancel)?;
        if !seen_paths.insert(document.path.clone()) {
            continue;
        }
        let path = RepoPath::parse(&document.path).map_err(|_| EvalError::InvalidCase("path"))?;
        let language = document
            .language
            .as_deref()
            .and_then(parse_language)
            .or_else(|| infer_language(&document.path));
        let source = Document::new(repo_id, path, language, document.text.clone());
        let records = chunk(&source, &[], &policy).map_err(|_| EvalError::Index("chunk"))?;
        if records.is_empty() {
            return Err(EvalError::InvalidCase("empty document"));
        }
        let fts_doc =
            FtsDocument::from_records(&records, &[]).map_err(|_| EvalError::Index("fts_doc"))?;
        fts.upsert_document(&fts_doc)
            .map_err(|_| EvalError::Index("fts_write"))?;
        let vector_doc = VectorDocument::from_records(&records, RedactionClass::Public)
            .map_err(|_| EvalError::Index("vector_doc"))?;
        vector
            .upsert_document(&vector_doc, &embedder)
            .map_err(|_| EvalError::Index("vector_write"))?;
        for record in records {
            let tokens = estimator
                .estimate(TokenizerFamily::Unknown, record.text())?
                .tokens();
            chunks.push(IndexedChunk {
                chunk_id: record.id().to_string(),
                path: record.path().as_str().to_string(),
                symbol: record.symbol_id().map(str::to_string),
                start_byte: record.start_byte(),
                end_byte: record.end_byte(),
                start_line: record.start_line(),
                end_line: record.end_line(),
                text: record.text().to_string(),
                tokens,
            });
        }
    }

    let mut store =
        MemoryStore::open_in_memory(MemoryLimits::new()).map_err(|_| EvalError::Memory)?;
    let mut memory = Vec::new();
    for seed in &suite.memory {
        check_cancel(cancel)?;
        let record = store
            .write_memory(MemoryWrite::new(
                MemoryScope::User,
                MemorySource::new(MemorySourceKind::System, "eval"),
                seed.confidence,
                seed.content.clone(),
            ))
            .map_err(|_| EvalError::Memory)?;
        memory.push(record);
    }
    if memory.is_empty() {
        memory = store
            .retrieve(&MemoryQuery::new())
            .map_err(|_| EvalError::Memory)?;
    }

    Ok(IndexedCorpus {
        repo_id,
        fts,
        vector,
        memory,
        chunks,
    })
}

fn evaluate_case(
    corpus: &IndexedCorpus,
    case: &GoldCase,
    mode: RetrievalMode,
    limit: u32,
    cancel: &CancellationToken,
) -> Result<QueryMetrics, EvalError> {
    let started = Instant::now();
    if started.elapsed() > QUERY_TIMEOUT {
        return Err(EvalError::Timeout);
    }
    match evaluate_case_inner(corpus, case, mode, limit, cancel, started) {
        Ok(metrics) => Ok(metrics),
        Err(EvalError::Cancelled) => Err(EvalError::Cancelled),
        Err(err) => Ok(QueryMetrics::failed(&case.id, mode, err)),
    }
}

fn evaluate_case_inner(
    corpus: &IndexedCorpus,
    case: &GoldCase,
    mode: RetrievalMode,
    limit: u32,
    cancel: &CancellationToken,
    started: Instant,
) -> Result<QueryMetrics, EvalError> {
    check_cancel(cancel)?;
    if started.elapsed() > QUERY_TIMEOUT {
        return Err(EvalError::Timeout);
    }

    let embedder = HashEmbedder::new()?;
    let embedding = embedder.embed_one(&case.query, cancel)?;
    let disabled = VectorIndex::disabled();
    let mut query = ContextQuery::new(case.query.clone())
        .repo(corpus.repo_id)
        .max_candidates(limit.max(DEFAULT_LIMIT))
        .cancellation(cancel.clone());
    if mode.uses_embedding() {
        query = query.embedding(embedding);
    }
    let sources = mode.sources(corpus, &disabled);
    let retrieve_started = Instant::now();
    let pools = generate_candidates(&query, sources, &CandidateLimits::new())?;
    let fused = rank_cancelled(&pools, &RankWeights::new(), 1.0, limit, cancel)?;
    let ranked = rank_cancelled(&pools, &RankWeights::new(), DEFAULT_LAMBDA, limit, cancel)?;
    let retrieval_latency_ms = duration_ms(retrieve_started.elapsed());
    if started.elapsed() > QUERY_TIMEOUT {
        return Err(EvalError::Timeout);
    }

    let compile_started = Instant::now();
    let packet = compile_hits(corpus, case, &ranked, cancel)?;
    let compile_latency_ms = duration_ms(compile_started.elapsed());
    let context_tokens = packet.included_tokens();
    let packet_bytes = downstream_packet_bytes(&packet)?;

    let gains = relevance_gains(&ranked, &case.relevant, corpus);
    let file_gains = file_relevance_gains(&ranked, &case.relevant);
    let (redundant_tokens, redundant_token_ratio) = redundant_after_mmr(&fused, &ranked, corpus);

    Ok(QueryMetrics {
        id: case.id.clone(),
        mode,
        error: None,
        recall_at_5: recall_at(&gains, 5, case.relevant.len()),
        recall_at_10: recall_at(&gains, 10, case.relevant.len()),
        recall_at_20: recall_at(&gains, 20, case.relevant.len()),
        file_recall_at_5: recall_at(&file_gains, 5, relevant_paths(&case.relevant).len()),
        file_recall_at_10: recall_at(&file_gains, 10, relevant_paths(&case.relevant).len()),
        file_recall_at_20: recall_at(&file_gains, 20, relevant_paths(&case.relevant).len()),
        ndcg_at_5: ndcg_at(&gains, 5),
        ndcg_at_10: ndcg_at(&gains, 10),
        ndcg_at_20: ndcg_at(&gains, 20),
        mrr: mean_reciprocal_rank(&gains),
        redundant_tokens,
        redundant_token_ratio,
        retrieval_latency_ms,
        compile_latency_ms,
        context_tokens,
        packet_bytes,
        hit_count: ranked.len() as u32,
        relevant_count: case.relevant.len() as u32,
        vector_status: vector_status(&pools),
    })
}

fn compile_hits(
    corpus: &IndexedCorpus,
    case: &GoldCase,
    hits: &[RankedContextHit],
    cancel: &CancellationToken,
) -> Result<ContextPacket, EvalError> {
    check_cancel(cancel)?;
    let mut request = CompileContext::new(DEFAULT_CONTEXT_LIMIT, DEFAULT_OUTPUT_RESERVE)
        .task(case.task.clone())
        .goal(case.task.clone())
        .tokenizer(TokenizerFamily::Unknown)
        .user(CompileInput::new("eval/user", case.query.clone()).reason(CompileReason::User))
        .goal_block(CompileInput::new("eval/goal", case.task.clone()).reason(CompileReason::Goal));
    for (idx, hit) in hits.iter().enumerate() {
        if idx.is_multiple_of(16) {
            check_cancel(cancel)?;
        }
        let Some(chunk) = lookup_hit(corpus, hit) else {
            continue;
        };
        let locator = hit
            .path()
            .map(RepoPath::as_str)
            .unwrap_or(chunk.path.as_str());
        request = request.retrieved(
            CompileInput::new(locator, chunk.text.clone())
                .tokens(chunk.tokens)
                .score((1_000u32).saturating_sub(hit.rank().saturating_mul(10)))
                .reason(CompileReason::Retrieved),
        );
    }
    for record in &corpus.memory {
        request = request.memory(
            CompileInput::new(
                format!("memory/{}", record.id()),
                record.content().to_string(),
            )
            .reason(CompileReason::Memory),
        );
    }
    compile(&request).map_err(EvalError::Compile)
}

fn lookup_hit<'a>(corpus: &'a IndexedCorpus, hit: &RankedContextHit) -> Option<&'a IndexedChunk> {
    if let Some(chunk_id) = hit.chunk_id() {
        if let Some(found) = corpus.chunks.iter().find(|c| c.chunk_id == chunk_id) {
            return Some(found);
        }
    }
    let path = hit.path()?.as_str();
    corpus.chunks.iter().find(|chunk| {
        chunk.path == path
            && ranges_overlap(
                chunk.start_byte,
                chunk.end_byte,
                hit.start_byte().unwrap_or(chunk.start_byte),
                hit.end_byte().unwrap_or(chunk.end_byte),
            )
    })
}

fn relevance_gains(
    hits: &[RankedContextHit],
    judgments: &[RelevanceJudgment],
    corpus: &IndexedCorpus,
) -> Vec<f64> {
    let mut used = BTreeSet::new();
    hits.iter()
        .map(|hit| {
            match judgments.iter().enumerate().find(|(idx, judgment)| {
                !used.contains(idx) && judgment_matches(hit, judgment, corpus)
            }) {
                Some((idx, _)) => {
                    used.insert(idx);
                    1.0
                }
                None => 0.0,
            }
        })
        .collect()
}

fn file_relevance_gains(hits: &[RankedContextHit], judgments: &[RelevanceJudgment]) -> Vec<f64> {
    let relevant = relevant_paths(judgments);
    let mut seen = BTreeSet::new();
    hits.iter()
        .map(|hit| match hit.path().map(RepoPath::as_str) {
            Some(path) if relevant.contains(path) && seen.insert(path.to_string()) => 1.0,
            _ => 0.0,
        })
        .collect()
}

fn relevant_paths(judgments: &[RelevanceJudgment]) -> BTreeSet<String> {
    judgments.iter().filter_map(|j| j.path.clone()).collect()
}

fn judgment_matches(
    hit: &RankedContextHit,
    judgment: &RelevanceJudgment,
    corpus: &IndexedCorpus,
) -> bool {
    if let (Some(expected), Some(actual)) = (judgment.chunk_id.as_deref(), hit.chunk_id()) {
        return expected == actual;
    }
    if let Some(expected) = judgment.path.as_deref() {
        match hit.path() {
            Some(actual) if actual.as_str() == expected => {}
            _ => return false,
        }
    }
    if let Some(expected) = judgment.symbol.as_deref() {
        if let Some(actual) = hit
            .symbol()
            .or_else(|| lookup_hit(corpus, hit).and_then(|chunk| chunk.symbol.as_deref()))
        {
            if actual != expected {
                return false;
            }
        }
    }
    if let (Some(start), Some(end)) = (judgment.start_line, judgment.end_line) {
        if let Some(chunk) = lookup_hit(corpus, hit) {
            return ranges_overlap(start, end, chunk.start_line, chunk.end_line);
        }
    }
    if let (Some(start), Some(end)) = (judgment.start_byte, judgment.end_byte) {
        return match (hit.start_byte(), hit.end_byte()) {
            (Some(hs), Some(he)) => ranges_overlap(start, end, hs, he),
            _ => false,
        };
    }
    true
}

fn ranges_overlap(a_start: u32, a_end: u32, b_start: u32, b_end: u32) -> bool {
    a_end.min(b_end) > a_start.max(b_start)
}

fn recall_at(gains: &[f64], k: usize, relevant: usize) -> f64 {
    if relevant == 0 {
        return 0.0;
    }
    let found: f64 = gains.iter().take(k).sum();
    found / relevant as f64
}

fn ndcg_at(gains: &[f64], k: usize) -> f64 {
    let dcg = dcg_at(gains, k);
    let mut ideal: Vec<f64> = gains.iter().copied().filter(|g| *g > 0.0).collect();
    ideal.sort_by(|a, b| b.total_cmp(a));
    let idcg = dcg_at(&ideal, k);
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

fn dcg_at(gains: &[f64], k: usize) -> f64 {
    gains
        .iter()
        .take(k)
        .enumerate()
        .map(|(idx, gain)| *gain / ((idx as f64) + 2.0).log2())
        .sum()
}

fn mean_reciprocal_rank(gains: &[f64]) -> f64 {
    gains
        .iter()
        .enumerate()
        .find(|(_, gain)| **gain > 0.0)
        .map(|(idx, _)| 1.0 / (idx as f64 + 1.0))
        .unwrap_or(0.0)
}

fn redundant_after_mmr(
    fused: &[RankedContextHit],
    selected: &[RankedContextHit],
    corpus: &IndexedCorpus,
) -> (f64, f64) {
    let _ = fused;
    let mut total = 0.0;
    let mut redundant = 0.0;
    for (idx, hit) in selected.iter().enumerate() {
        let tokens = hit_tokens(hit, corpus);
        total += tokens;
        let overlap = selected[..idx]
            .iter()
            .map(|prior| similarity(hit, prior) * tokens)
            .fold(0.0, f64::max);
        redundant += overlap;
    }
    let ratio = if total > 0.0 { redundant / total } else { 0.0 };
    (redundant, ratio)
}

fn similarity(left: &RankedContextHit, right: &RankedContextHit) -> f64 {
    if let (Some(a), Some(b)) = (left.chunk_id(), right.chunk_id()) {
        if a == b && left.repo_id() == right.repo_id() {
            return 1.0;
        }
    }
    match (left.path(), right.path()) {
        (Some(a), Some(b)) if a == b && left.repo_id() == right.repo_id() => {
            match (
                left.start_byte(),
                left.end_byte(),
                right.start_byte(),
                right.end_byte(),
            ) {
                (Some(ls), Some(le), Some(rs), Some(re)) => range_jaccard(ls, le, rs, re),
                _ => 0.5,
            }
        }
        _ => 0.0,
    }
}

fn range_jaccard(a_start: u32, a_end: u32, b_start: u32, b_end: u32) -> f64 {
    let a_end = a_end.max(a_start);
    let b_end = b_end.max(b_start);
    let inter_start = a_start.max(b_start);
    let inter_end = a_end.min(b_end);
    if inter_end <= inter_start {
        return 0.0;
    }
    let union = a_end.max(b_end).saturating_sub(a_start.min(b_start));
    if union == 0 {
        0.0
    } else {
        f64::from(inter_end.saturating_sub(inter_start)) / f64::from(union)
    }
}

fn hit_tokens(hit: &RankedContextHit, corpus: &IndexedCorpus) -> f64 {
    lookup_hit(corpus, hit)
        .map(|chunk| f64::from(chunk.tokens))
        .unwrap_or(1.0)
}

fn downstream_packet_bytes(packet: &ContextPacket) -> Result<u64, EvalError> {
    let ready = DownstreamPacket {
        schema: PACKET_SCHEMA,
        included_tokens: packet.included_tokens(),
        block_count: packet.blocks().len(),
        blocks: packet
            .blocks()
            .iter()
            .map(|block| DownstreamBlock {
                locator: block.locator(),
                source: block.source().as_str(),
                tokens: block.estimated_tokens(),
                bytes: block.text().len(),
                content_hash: block.content_hash().to_string(),
                reason: block.reason().as_str(),
                freshness: block.freshness().as_str(),
                trust: block.trust().as_str(),
                text: block.text(),
            })
            .collect(),
    };
    let encoded = serde_json::to_vec(&ready).map_err(|_| EvalError::Encode)?;
    Ok(encoded.len() as u64)
}

fn vector_status(pools: &context_engine::CandidatePools) -> String {
    match pools.status(GeneratorKind::Vector) {
        Some(status) => status
            .degrade()
            .map(|reason| reason.as_str().to_string())
            .unwrap_or_else(|| "ok".to_string()),
        None => "absent".to_string(),
    }
}

fn aggregate(queries: &[QueryMetrics]) -> BTreeMap<String, AggregateMetrics> {
    let mut out = BTreeMap::new();
    for mode in RetrievalMode::ALL {
        let rows: Vec<&QueryMetrics> = queries.iter().filter(|q| q.mode == mode).collect();
        let ok: Vec<&QueryMetrics> = rows.iter().copied().filter(|q| q.error.is_none()).collect();
        let mut retrieval: Vec<f64> = ok.iter().map(|q| q.retrieval_latency_ms).collect();
        let mut compile_ms: Vec<f64> = ok.iter().map(|q| q.compile_latency_ms).collect();
        retrieval.sort_by(|a, b| a.total_cmp(b));
        compile_ms.sort_by(|a, b| a.total_cmp(b));
        let n = ok.len().max(1) as f64;
        out.insert(
            mode.as_str().to_string(),
            AggregateMetrics {
                recall_at_5: ok.iter().map(|q| q.recall_at_5).sum::<f64>() / n,
                recall_at_10: ok.iter().map(|q| q.recall_at_10).sum::<f64>() / n,
                recall_at_20: ok.iter().map(|q| q.recall_at_20).sum::<f64>() / n,
                file_recall_at_5: ok.iter().map(|q| q.file_recall_at_5).sum::<f64>() / n,
                ndcg_at_5: ok.iter().map(|q| q.ndcg_at_5).sum::<f64>() / n,
                ndcg_at_10: ok.iter().map(|q| q.ndcg_at_10).sum::<f64>() / n,
                ndcg_at_20: ok.iter().map(|q| q.ndcg_at_20).sum::<f64>() / n,
                mrr: ok.iter().map(|q| q.mrr).sum::<f64>() / n,
                redundant_token_ratio: ok.iter().map(|q| q.redundant_token_ratio).sum::<f64>() / n,
                retrieval_latency_ms_p50: percentile(&retrieval, 0.50),
                retrieval_latency_ms_p95: percentile(&retrieval, 0.95),
                compile_latency_ms_p50: percentile(&compile_ms, 0.50),
                context_tokens_mean: ok.iter().map(|q| f64::from(q.context_tokens)).sum::<f64>()
                    / n,
                packet_bytes_mean: ok.iter().map(|q| q.packet_bytes as f64).sum::<f64>() / n,
                query_count: rows.len() as u32,
                error_count: rows.iter().filter(|q| q.error.is_some()).count() as u32,
            },
        );
    }
    out
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn emit_artifact(artifact: &MetricsArtifact, out: Option<&Path>) -> Result<(), EvalError> {
    let encoded = serde_json::to_string_pretty(artifact)?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(encoded.as_bytes())?;
    stdout.write_all(b"\n")?;
    if let Some(path) = out {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, encoded.as_bytes())?;
    }
    Ok(())
}

fn parse_language(raw: &str) -> Option<SourceLanguage> {
    SourceLanguage::ALL
        .iter()
        .copied()
        .find(|language| language.as_str() == raw)
}

fn infer_language(path: &str) -> Option<SourceLanguage> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "rs" => Some(SourceLanguage::Rust),
        "ts" | "tsx" => Some(SourceLanguage::TypeScript),
        "js" | "jsx" => Some(SourceLanguage::JavaScript),
        "py" => Some(SourceLanguage::Python),
        "go" => Some(SourceLanguage::Go),
        "java" => Some(SourceLanguage::Java),
        "c" | "h" => Some(SourceLanguage::C),
        "cc" | "cpp" | "cxx" | "hpp" => Some(SourceLanguage::Cpp),
        "cs" => Some(SourceLanguage::CSharp),
        "kt" => Some(SourceLanguage::Kotlin),
        "swift" => Some(SourceLanguage::Swift),
        "rb" => Some(SourceLanguage::Ruby),
        "sh" | "bash" => Some(SourceLanguage::Bash),
        "json" => Some(SourceLanguage::Json),
        "yml" | "yaml" => Some(SourceLanguage::Yaml),
        "toml" => Some(SourceLanguage::Toml),
        "md" => Some(SourceLanguage::Markdown),
        _ => None,
    }
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), EvalError> {
    if cancel.is_cancelled() {
        Err(EvalError::Cancelled)
    } else {
        Ok(())
    }
}

impl RetrievalMode {
    const ALL: [Self; 4] = [
        Self::Hybrid,
        Self::RgOnly,
        Self::VectorOnly,
        Self::VectorDisabled,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::RgOnly => "rg_only",
            Self::VectorOnly => "vector_only",
            Self::VectorDisabled => "vector_disabled",
        }
    }

    const fn uses_embedding(self) -> bool {
        !matches!(self, Self::RgOnly)
    }

    fn sources<'a>(
        self,
        corpus: &'a IndexedCorpus,
        disabled: &'a VectorIndex,
    ) -> CandidateSources<'a> {
        match self {
            Self::Hybrid => CandidateSources::new()
                .fts(&corpus.fts)
                .vector(&corpus.vector),
            Self::RgOnly => CandidateSources::new().fts(&corpus.fts),
            Self::VectorOnly => CandidateSources::new().vector(&corpus.vector),
            Self::VectorDisabled => CandidateSources::new().fts(&corpus.fts).vector(disabled),
        }
    }
}

impl QueryMetrics {
    fn failed(id: &str, mode: RetrievalMode, err: EvalError) -> Self {
        Self {
            id: id.to_string(),
            mode,
            error: Some(err.as_str().to_string()),
            recall_at_5: 0.0,
            recall_at_10: 0.0,
            recall_at_20: 0.0,
            file_recall_at_5: 0.0,
            file_recall_at_10: 0.0,
            file_recall_at_20: 0.0,
            ndcg_at_5: 0.0,
            ndcg_at_10: 0.0,
            ndcg_at_20: 0.0,
            mrr: 0.0,
            redundant_tokens: 0.0,
            redundant_token_ratio: 0.0,
            retrieval_latency_ms: 0.0,
            compile_latency_ms: 0.0,
            context_tokens: 0,
            packet_bytes: 0,
            hit_count: 0,
            relevant_count: 0,
            vector_status: "error".to_string(),
        }
    }
}

impl HashEmbedder {
    fn new() -> Result<Self, EvalError> {
        Ok(Self {
            version: EmbeddingVersion::new("eval-hash-v1")
                .map_err(|_| EvalError::Index("embed_version"))?,
        })
    }

    fn embed_one(&self, text: &str, cancel: &CancellationToken) -> Result<Vec<f32>, EvalError> {
        let mut vectors = self.embed(&[text], cancel)?;
        vectors.pop().ok_or(EvalError::Index("empty embedding"))
    }
}

impl EmbeddingProvider for HashEmbedder {
    fn version(&self) -> &EmbeddingVersion {
        &self.version
    }

    fn dimensions(&self) -> u32 {
        EMBED_DIMS as u32
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
            .map(|text| bag_of_words(text, EMBED_DIMS))
            .collect())
    }
}

fn bag_of_words(text: &str, dims: usize) -> Vec<f32> {
    let mut values = vec![0.0f32; dims];
    for token in tokenize(text) {
        let digest = context_engine::ContentHash::from_bytes(token.as_bytes());
        let bytes = digest.as_digest();
        let idx = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize % dims;
        let sign = if bytes[4] & 1 == 0 { 1.0 } else { -1.0 };
        values[idx] += sign;
    }
    let norm = values
        .iter()
        .map(|v| (*v as f64) * (*v as f64))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for value in &mut values {
            *value = (*value as f64 / norm) as f32;
        }
    }
    values
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

impl EvalError {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::InvalidCase(_) => "invalid_case",
            Self::InvalidConfig(_) => "invalid_config",
            Self::CapacityExceeded => "capacity_exceeded",
            Self::LineTooLarge => "line_too_large",
            Self::Io(_) => "io",
            Self::Json(_) => "json",
            Self::Index(_) => "index",
            Self::Compile(_) => "compile",
            Self::Tokens(_) => "tokens",
            Self::Memory => "memory",
            Self::Encode => "encode",
        }
    }
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCase(reason) | Self::InvalidConfig(reason) | Self::Index(reason) => {
                write!(f, "{}: {reason}", self.as_str())
            }
            other => f.write_str(other.as_str()),
        }
    }
}

impl Error for EvalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Compile(err) => Some(err),
            Self::Tokens(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for EvalError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for EvalError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<TokenEstimateError> for EvalError {
    fn from(value: TokenEstimateError) -> Self {
        Self::Tokens(value)
    }
}

impl From<context_engine::CandidateError> for EvalError {
    fn from(value: context_engine::CandidateError) -> Self {
        match value {
            context_engine::CandidateError::Cancelled => Self::Cancelled,
            context_engine::CandidateError::InvalidPolicy
            | context_engine::CandidateError::InvalidQuery => {
                Self::InvalidConfig("candidate query")
            }
        }
    }
}

impl From<context_engine::RankError> for EvalError {
    fn from(value: context_engine::RankError) -> Self {
        match value {
            context_engine::RankError::Cancelled => Self::Cancelled,
            context_engine::RankError::InvalidConfig => Self::InvalidConfig("rank"),
        }
    }
}

impl From<VectorError> for EvalError {
    fn from(value: VectorError) -> Self {
        match value {
            VectorError::Cancelled => Self::Cancelled,
            VectorError::Timeout => Self::Timeout,
            _ => Self::Index("vector"),
        }
    }
}

fn self_check(cancel: &CancellationToken) -> Result<(), EvalError> {
    check_metric_math()?;
    check_jsonl_roundtrip(cancel)?;
    check_builtin_runner(cancel)?;
    check_cancel_fails_closed()?;
    Ok(())
}

fn check_metric_math() -> Result<(), EvalError> {
    let gains = [1.0, 0.0, 1.0];
    if (recall_at(&gains, 2, 2) - 0.5).abs() > 1e-9 {
        return Err(EvalError::InvalidConfig("recall@2"));
    }
    if (recall_at(&gains, 3, 2) - 1.0).abs() > 1e-9 {
        return Err(EvalError::InvalidConfig("recall@3"));
    }
    if (mean_reciprocal_rank(&gains) - 1.0).abs() > 1e-9 {
        return Err(EvalError::InvalidConfig("mrr"));
    }
    let expected_ndcg = {
        let dcg = 1.0 / 2.0_f64.log2() + 1.0 / 4.0_f64.log2();
        let idcg = 1.0 / 2.0_f64.log2() + 1.0 / 3.0_f64.log2();
        dcg / idcg
    };
    if (ndcg_at(&gains, 3) - expected_ndcg).abs() > 1e-9 {
        return Err(EvalError::InvalidConfig("ndcg"));
    }
    Ok(())
}

fn check_jsonl_roundtrip(cancel: &CancellationToken) -> Result<(), EvalError> {
    let raw = format!(
        "{}\n{}\n",
        r#"{"schema":"rapidlm.context.eval.corpus.v1","documents":[{"path":"src/a.rs","text":"fn unique_alpha() {}\n","language":"rust"}]}"#,
        r#"{"schema":"rapidlm.context.eval.case.v1","id":"alpha","query":"unique_alpha","relevant":[{"path":"src/a.rs"}]}"#
    );
    let suite = parse_jsonl(&raw, cancel)?;
    if suite.cases.len() != 1 || suite.documents.len() != 1 {
        return Err(EvalError::InvalidCase("jsonl parse"));
    }
    Ok(())
}

fn check_builtin_runner(cancel: &CancellationToken) -> Result<(), EvalError> {
    let artifact = evaluate_suite(&builtin_suite(), DEFAULT_LIMIT, cancel)?;
    if artifact.queries.len() != RetrievalMode::ALL.len() * 2 {
        return Err(EvalError::InvalidConfig("query rows"));
    }
    for mode in [
        RetrievalMode::Hybrid,
        RetrievalMode::RgOnly,
        RetrievalMode::VectorDisabled,
    ] {
        let rows: Vec<&QueryMetrics> = artifact
            .queries
            .iter()
            .filter(|q| q.mode == mode && q.error.is_none())
            .collect();
        if rows.len() != 2 {
            return Err(EvalError::InvalidConfig("mode rows"));
        }
        if rows
            .iter()
            .any(|q| q.recall_at_5 < 1.0 || q.ndcg_at_5 < 1.0)
        {
            return Err(EvalError::InvalidConfig("lexical recall"));
        }
        if rows
            .iter()
            .any(|q| q.context_tokens == 0 || q.packet_bytes == 0)
        {
            return Err(EvalError::InvalidConfig("packet accounting"));
        }
    }
    let disabled: Vec<&QueryMetrics> = artifact
        .queries
        .iter()
        .filter(|q| q.mode == RetrievalMode::VectorDisabled)
        .collect();
    if disabled
        .iter()
        .any(|q| q.vector_status != GeneratorDegradeReason::Disabled.as_str())
    {
        return Err(EvalError::InvalidConfig("vector-disabled status"));
    }
    if artifact
        .aggregates
        .get(RetrievalMode::VectorDisabled.as_str())
        .is_none_or(|agg| agg.recall_at_5 < 1.0)
    {
        return Err(EvalError::InvalidConfig("vector-disabled aggregate"));
    }
    Ok(())
}

fn check_cancel_fails_closed() -> Result<(), EvalError> {
    let cancel = CancellationToken::new();
    cancel.cancel();
    match evaluate_suite(&builtin_suite(), DEFAULT_LIMIT, &cancel) {
        Err(EvalError::Cancelled) => Ok(()),
        _ => Err(EvalError::InvalidConfig("cancel")),
    }
}
