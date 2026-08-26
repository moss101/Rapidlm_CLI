//! Read-only Context Scout over existing retrieval seams.
//!
//! The scout answers an [`InformationNeed`] with a typed report. It does not
//! mutate indexes. Vector search is optional; zero hits are broadened before
//! an absence claim is recorded.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::{ArtifactId, RepoPath};

use crate::index::fts::FtsIndex;
use crate::index::graph::CodeGraph;
use crate::index::vector::VectorIndex;
use crate::need::{CompletenessRequirement, InformationNeed, NegativeClaimPolicy};
use crate::repo_manifest::CancellationToken;
use crate::retrieval::candidates::{
    CandidateLimits, CandidateSource, CandidateSources, ContextQuery, ExplicitPin, SymbolHint,
    generate_candidates,
};
use crate::retrieval::filter::{RetrievalFilter, filter_pools, filter_ranked};
use crate::retrieval::rank::{RankWeights, RankedContextHit, rank_cancelled};

/// Default wall-clock budget for one scout call.
pub const DEFAULT_SCOUT_TIMEOUT: Duration = Duration::from_secs(4);

/// Default fused-hit page before snippet selection.
pub const DEFAULT_MAX_SCOUT_REFERENCES: u32 = 64;

/// Deep-read locations returned to the parent. Not a dump of the repo.
pub const DEFAULT_MAX_SCOUT_SNIPPETS: u32 = 4;

/// Cap on unanswered follow-ups.
pub const DEFAULT_MAX_OPEN_QUESTIONS: u32 = 16;

const CANCEL_STRIDE: usize = 16;
const MAX_SUMMARY_BYTES: usize = 512;
const MAX_LOG_BYTES: usize = 8_192;
const BROADEN_PREFIXES: &[&str] = &["vendor", "generated", "third_party"];

/// Timeouts and result caps for one [`scout`] call.
#[derive(Clone, Debug)]
pub struct ScoutLimits {
    timeout: Duration,
    max_references: u32,
    max_snippets: u32,
    max_open_questions: u32,
    cancel: CancellationToken,
}

/// Optional indexes. Absence degrades a generator; it does not fail the scout.
pub struct ScoutSources<'a> {
    fts: Option<&'a FtsIndex>,
    vector: Option<&'a VectorIndex>,
    graph: Option<&'a CodeGraph>,
    embedding: Option<Vec<f32>>,
}

/// Typed scout output matching the context-engineering protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextScoutReport {
    summary: String,
    scope: SearchScopeReport,
    references: Vec<Reference>,
    negative_findings: Vec<NegativeFinding>,
    open_questions: Vec<String>,
    snippets: Vec<SnippetRef>,
    search_log_ref: ArtifactId,
}

/// Scopes that were requested versus actually searched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchScopeReport {
    requested: Vec<String>,
    searched: Vec<String>,
    broadened: bool,
}

/// One located symbol or range. Exhaustive mode lists every unique hit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reference {
    path: RepoPath,
    symbol: Option<String>,
    start_byte: Option<u32>,
    end_byte: Option<u32>,
    method: RetrievalMethod,
}

/// How the scout found a reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RetrievalMethod {
    Explicit,
    Lexical,
    Vector,
    Graph,
    Grep,
}

/// Checked absence claim. Confidence is never High without a broadened search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegativeFinding {
    question: String,
    scopes: Vec<String>,
    patterns: Vec<String>,
    confidence: NegativeConfidence,
}

/// Confidence that the checked scopes really lack a match.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum NegativeConfidence {
    Low,
    Medium,
    High,
}

/// Small parent-read locator. Body text is not inlined.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnippetRef {
    path: RepoPath,
    start_byte: Option<u32>,
    end_byte: Option<u32>,
    reason: String,
}

/// Typed scout failure. Display never echoes questions or paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScoutError {
    Cancelled,
    Timeout,
    InvalidPolicy,
}

impl ScoutLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn max_references(mut self, value: u32) -> Self {
        self.max_references = value;
        self
    }

    pub fn max_snippets(mut self, value: u32) -> Self {
        self.max_snippets = value;
        self
    }

    pub fn max_open_questions(mut self, value: u32) -> Self {
        self.max_open_questions = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }
}

impl Default for ScoutLimits {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_SCOUT_TIMEOUT,
            max_references: DEFAULT_MAX_SCOUT_REFERENCES,
            max_snippets: DEFAULT_MAX_SCOUT_SNIPPETS,
            max_open_questions: DEFAULT_MAX_OPEN_QUESTIONS,
            cancel: CancellationToken::new(),
        }
    }
}

impl ScoutSources<'_> {
    pub fn new() -> Self {
        Self {
            fts: None,
            vector: None,
            graph: None,
            embedding: None,
        }
    }
}

impl<'a> ScoutSources<'a> {
    pub fn fts(mut self, index: &'a FtsIndex) -> Self {
        self.fts = Some(index);
        self
    }

    pub fn vector(mut self, index: &'a VectorIndex, embedding: Vec<f32>) -> Self {
        self.vector = Some(index);
        self.embedding = Some(embedding);
        self
    }

    pub fn graph(mut self, graph: &'a CodeGraph) -> Self {
        self.graph = Some(graph);
        self
    }
}

impl Default for ScoutSources<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextScoutReport {
    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn scope(&self) -> &SearchScopeReport {
        &self.scope
    }

    pub fn references(&self) -> &[Reference] {
        &self.references
    }

    pub fn negative_findings(&self) -> &[NegativeFinding] {
        &self.negative_findings
    }

    pub fn open_questions(&self) -> &[String] {
        &self.open_questions
    }

    pub fn snippets(&self) -> &[SnippetRef] {
        &self.snippets
    }

    pub fn search_log_ref(&self) -> ArtifactId {
        self.search_log_ref
    }
}

impl SearchScopeReport {
    pub fn requested(&self) -> &[String] {
        &self.requested
    }

    pub fn searched(&self) -> &[String] {
        &self.searched
    }

    pub fn broadened(&self) -> bool {
        self.broadened
    }
}

impl Reference {
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn symbol(&self) -> Option<&str> {
        self.symbol.as_deref()
    }

    pub fn start_byte(&self) -> Option<u32> {
        self.start_byte
    }

    pub fn end_byte(&self) -> Option<u32> {
        self.end_byte
    }

    pub fn method(&self) -> RetrievalMethod {
        self.method
    }
}

impl NegativeFinding {
    pub fn question(&self) -> &str {
        &self.question
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    pub fn confidence(&self) -> NegativeConfidence {
        self.confidence
    }
}

impl SnippetRef {
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn start_byte(&self) -> Option<u32> {
        self.start_byte
    }

    pub fn end_byte(&self) -> Option<u32> {
        self.end_byte
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl RetrievalMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Lexical => "lexical",
            Self::Vector => "vector",
            Self::Graph => "graph",
            Self::Grep => "grep",
        }
    }
}

impl NegativeConfidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl ScoutError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::InvalidPolicy => "invalid_policy",
        }
    }
}

impl fmt::Display for RetrievalMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for NegativeConfidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ScoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ScoutError {}

/// Run a read-only scout for `need`. Indexes are borrowed, never written.
pub fn scout(
    need: &InformationNeed,
    sources: ScoutSources<'_>,
    limits: &ScoutLimits,
) -> Result<ContextScoutReport, ScoutError> {
    let started = Instant::now();
    check_ready(limits, started)?;
    if limits.max_references == 0 || limits.max_snippets == 0 {
        return Err(ScoutError::InvalidPolicy);
    }

    let mut log = String::new();
    let mut searched = BTreeSet::new();
    let mut broadened = false;
    let mut fused: Vec<RankedContextHit> = Vec::new();
    let mut negatives = Vec::new();
    let mut patterns_log = BTreeSet::new();

    for (step, question) in need.questions().iter().enumerate() {
        if step.is_multiple_of(CANCEL_STRIDE) {
            check_ready(limits, started)?;
        }
        let result = search_question(need, question, &sources, limits, started, &mut log)?;
        for scope in &result.searched {
            searched.insert(scope.clone());
        }
        for pattern in &result.patterns {
            patterns_log.insert(pattern.clone());
        }
        broadened |= result.broadened;
        if result.hits.is_empty() {
            negatives.push(NegativeFinding {
                question: question.clone(),
                scopes: result.searched,
                patterns: result.patterns,
                confidence: negative_confidence(need, result.broadened),
            });
        } else {
            fused.extend(result.hits);
        }
    }

    let snippets = select_snippets(&fused, need, limits, started)?;
    let references = select_references(&fused, &snippets, need, limits);
    let open_questions = open_questions(need, &negatives, &fused, limits.max_open_questions);
    let requested = need.scope().paths().to_vec();
    let searched: Vec<String> = if searched.is_empty() {
        requested.clone()
    } else {
        searched.into_iter().collect()
    };
    let summary = summarize(
        &references,
        &negatives,
        &open_questions,
        broadened,
        &snippets,
    );
    let _ = patterns_log;
    let search_log_ref = ArtifactId::from_bytes(truncate_log(&log).as_bytes());

    Ok(ContextScoutReport {
        summary,
        scope: SearchScopeReport {
            requested,
            searched,
            broadened,
        },
        references,
        negative_findings: negatives,
        open_questions,
        snippets,
        search_log_ref,
    })
}

struct QuestionSearch {
    hits: Vec<RankedContextHit>,
    searched: Vec<String>,
    patterns: Vec<String>,
    broadened: bool,
}

fn search_question(
    need: &InformationNeed,
    question: &str,
    sources: &ScoutSources<'_>,
    limits: &ScoutLimits,
    started: Instant,
    log: &mut String,
) -> Result<QuestionSearch, ScoutError> {
    let mut patterns = vec![question.to_string()];
    let requested_scope = need.scope().paths().to_vec();
    let mut searched = requested_scope.clone();
    let mut broadened = false;

    let hits = retrieve(need, question, sources, &requested_scope, limits, started)?;
    append_log(
        log,
        &format!(
            "q={} scope={} hits={}\n",
            patterns.len(),
            requested_scope.len(),
            hits.len()
        ),
    );
    if !hits.is_empty() {
        return Ok(QuestionSearch {
            hits,
            searched,
            patterns,
            broadened,
        });
    }

    let extra = broaden_scope_paths(&requested_scope);
    if extra != requested_scope {
        broadened = true;
        searched.extend(extra.iter().cloned());
        searched.sort();
        searched.dedup();
        let hits = retrieve(need, question, sources, &extra, limits, started)?;
        append_log(log, &format!("broaden_scope hits={}\n", hits.len()));
        if !hits.is_empty() {
            return Ok(QuestionSearch {
                hits,
                searched,
                patterns,
                broadened,
            });
        }
    }

    let repo_wide: Vec<String> = Vec::new();
    if !requested_scope.is_empty() {
        broadened = true;
        searched.push("*".to_string());
        let hits = retrieve(need, question, sources, &repo_wide, limits, started)?;
        append_log(log, &format!("broaden_repo hits={}\n", hits.len()));
        if !hits.is_empty() {
            return Ok(QuestionSearch {
                hits,
                searched,
                patterns,
                broadened,
            });
        }
    }

    for alt in alternate_patterns(question) {
        if alt == question {
            continue;
        }
        patterns.push(alt.clone());
        broadened = true;
        let hits = retrieve(need, &alt, sources, &repo_wide, limits, started)?;
        append_log(log, &format!("broaden_pattern hits={}\n", hits.len()));
        if !hits.is_empty() {
            return Ok(QuestionSearch {
                hits,
                searched,
                patterns,
                broadened,
            });
        }
    }

    Ok(QuestionSearch {
        hits: Vec::new(),
        searched,
        patterns,
        broadened,
    })
}

fn retrieve(
    need: &InformationNeed,
    lexical: &str,
    sources: &ScoutSources<'_>,
    scope_paths: &[String],
    limits: &ScoutLimits,
    started: Instant,
) -> Result<Vec<RankedContextHit>, ScoutError> {
    check_ready(limits, started)?;
    let mut query = ContextQuery::new(lexical)
        .max_candidates(limits.max_references)
        .cancellation(limits.cancel.clone());
    if let Some(embedding) = sources.embedding.as_ref() {
        query = query.embedding(embedding.clone());
    }
    for anchor in need.known_anchors() {
        if let Ok(path) = RepoPath::parse(anchor.path()) {
            let mut pin = ExplicitPin::new(path);
            if let Some(symbol) = anchor.symbol() {
                pin = pin.symbol(symbol);
                query = query.symbol_hint(SymbolHint::new(symbol));
            }
            query = query.pin(pin);
        }
    }

    let mut candidate_sources = CandidateSources::new();
    if let Some(fts) = sources.fts {
        candidate_sources = candidate_sources.fts(fts);
    }
    if let Some(vector) = sources.vector {
        candidate_sources = candidate_sources.vector(vector);
    }
    if let Some(graph) = sources.graph {
        candidate_sources = candidate_sources.graph(graph);
    }

    let candidate_limits = CandidateLimits::new()
        .explicit_timeout(limits.timeout)
        .fts_timeout(limits.timeout)
        .vector_timeout(limits.timeout)
        .graph_timeout(limits.timeout)
        .cancellation(limits.cancel.clone());
    let pools =
        generate_candidates(&query, candidate_sources, &candidate_limits).map_err(map_candidate)?;
    let filter = RetrievalFilter::new()
        .scope(
            crate::need::ScopeSet::new(scope_paths.to_vec())
                .unwrap_or_else(|_| crate::need::ScopeSet::empty()),
        )
        .allow_untrusted(true)
        .allow_stale(true)
        .timeout(limits.timeout)
        .cancellation(limits.cancel.clone());
    let pools = filter_pools(pools, &filter).map_err(map_filter)?;
    let weights = RankWeights::new()
        .explicit(1.1)
        .vector(1.0)
        .lexical(0.55)
        .graph(0.75)
        .symbol(0.9);
    let ranked = rank_cancelled(&pools, &weights, 0.7, limits.max_references, &limits.cancel)
        .map_err(map_rank)?;
    filter_ranked(ranked, &filter).map_err(map_filter)
}

fn select_snippets(
    hits: &[RankedContextHit],
    need: &InformationNeed,
    limits: &ScoutLimits,
    started: Instant,
) -> Result<Vec<SnippetRef>, ScoutError> {
    check_ready(limits, started)?;
    let cap = limits.max_snippets.min(need.token_budget().max(1));
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for hit in hits {
        if out.len() as u32 >= cap {
            break;
        }
        let Some(path) = hit.path() else {
            continue;
        };
        let key = (
            path.as_str().to_string(),
            hit.start_byte().unwrap_or(0),
            hit.end_byte().unwrap_or(0),
        );
        if !seen.insert(key) {
            continue;
        }
        out.push(SnippetRef {
            path: path.clone(),
            start_byte: hit.start_byte(),
            end_byte: hit.end_byte(),
            reason: hit.reason().as_str().to_string(),
        });
    }
    Ok(out)
}

fn select_references(
    hits: &[RankedContextHit],
    snippets: &[SnippetRef],
    need: &InformationNeed,
    limits: &ScoutLimits,
) -> Vec<Reference> {
    let exhaustive = need.completeness() == CompletenessRequirement::Exhaustive;
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let source: Vec<&RankedContextHit> = if exhaustive {
        hits.iter().collect()
    } else {
        hits.iter()
            .filter(|hit| {
                hit.path()
                    .is_some_and(|path| snippets.iter().any(|snippet| snippet.path() == path))
            })
            .collect()
    };
    for hit in source {
        if out.len() as u32 >= limits.max_references {
            break;
        }
        let Some(path) = hit.path() else {
            continue;
        };
        let key = (
            path.as_str().to_string(),
            hit.symbol().unwrap_or("").to_string(),
            hit.start_byte().unwrap_or(u32::MAX),
            hit.end_byte().unwrap_or(u32::MAX),
        );
        if !seen.insert(key) {
            continue;
        }
        out.push(Reference {
            path: path.clone(),
            symbol: hit.symbol().map(str::to_string),
            start_byte: hit.start_byte(),
            end_byte: hit.end_byte(),
            method: retrieval_method(hit),
        });
    }
    out
}

fn open_questions(
    need: &InformationNeed,
    negatives: &[NegativeFinding],
    hits: &[RankedContextHit],
    cap: u32,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |value: String| {
        if (out.len() as u32) < cap && !out.contains(&value) {
            out.push(value);
        }
    };
    for finding in negatives {
        push(finding.question.clone());
    }
    if need.need_callers()
        && !hits
            .iter()
            .any(|h| h.sources().contains(&CandidateSource::Graph))
    {
        push("callers not enumerated".to_string());
    }
    if need.need_tests() && !hits.iter().any(|h| path_looks_like_test(h)) {
        push("tests not located".to_string());
    }
    if need.need_types()
        && !hits
            .iter()
            .any(|h| h.sources().contains(&CandidateSource::Symbol))
    {
        push("types not located".to_string());
    }
    if need.need_config() && !hits.iter().any(|h| path_looks_like_config(h)) {
        push("config not located".to_string());
    }
    out
}

fn path_looks_like_test(hit: &RankedContextHit) -> bool {
    hit.path()
        .map(|p| p.as_str().contains("test") || p.as_str().contains("spec"))
        .unwrap_or(false)
}

fn path_looks_like_config(hit: &RankedContextHit) -> bool {
    hit.path()
        .map(|p| {
            let s = p.as_str();
            s.contains("config") || s.ends_with(".toml") || s.ends_with(".json")
        })
        .unwrap_or(false)
}

fn retrieval_method(hit: &RankedContextHit) -> RetrievalMethod {
    let sources = hit.sources();
    if sources.contains(&CandidateSource::Vector) {
        RetrievalMethod::Vector
    } else if sources.contains(&CandidateSource::Graph) {
        RetrievalMethod::Graph
    } else if sources.contains(&CandidateSource::Lexical) {
        RetrievalMethod::Lexical
    } else {
        RetrievalMethod::Explicit
    }
}

fn negative_confidence(need: &InformationNeed, broadened: bool) -> NegativeConfidence {
    match need.negative_claim_policy() {
        NegativeClaimPolicy::AllowUnchecked if !broadened => NegativeConfidence::Low,
        NegativeClaimPolicy::AllowUnchecked => NegativeConfidence::Medium,
        NegativeClaimPolicy::RequireCheckedScope if broadened => NegativeConfidence::High,
        NegativeClaimPolicy::RequireCheckedScope => NegativeConfidence::Medium,
    }
}

fn broaden_scope_paths(requested: &[String]) -> Vec<String> {
    let mut out: Vec<String> = requested.to_vec();
    for path in requested {
        if let Some((parent, _)) = path.rsplit_once('/') {
            if !parent.is_empty() {
                out.push(parent.to_string());
            }
        }
    }
    for prefix in BROADEN_PREFIXES {
        out.push((*prefix).to_string());
    }
    out.sort();
    out.dedup();
    out
}

fn alternate_patterns(question: &str) -> Vec<String> {
    let mut out = Vec::new();
    let replaced = question.replace('-', "_");
    if replaced != question {
        out.push(replaced);
    }
    for token in question.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if token.len() >= 3 {
            out.push(token.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn summarize(
    references: &[Reference],
    negatives: &[NegativeFinding],
    open_questions: &[String],
    broadened: bool,
    snippets: &[SnippetRef],
) -> String {
    let text = format!(
        "references={} snippets={} negatives={} open_questions={} broadened={}",
        references.len(),
        snippets.len(),
        negatives.len(),
        open_questions.len(),
        broadened
    );
    if text.len() <= MAX_SUMMARY_BYTES {
        text
    } else {
        text[..MAX_SUMMARY_BYTES].to_string()
    }
}

fn append_log(log: &mut String, line: &str) {
    if log.len() >= MAX_LOG_BYTES {
        return;
    }
    let remaining = MAX_LOG_BYTES.saturating_sub(log.len());
    if line.len() <= remaining {
        log.push_str(line);
    } else {
        log.push_str(&line[..remaining]);
    }
}

fn truncate_log(log: &str) -> &str {
    if log.len() <= MAX_LOG_BYTES {
        log
    } else {
        &log[..MAX_LOG_BYTES]
    }
}

fn check_ready(limits: &ScoutLimits, started: Instant) -> Result<(), ScoutError> {
    if limits.cancel.is_cancelled() {
        return Err(ScoutError::Cancelled);
    }
    if limits.timeout.is_zero() || started.elapsed() > limits.timeout {
        return Err(ScoutError::Timeout);
    }
    Ok(())
}

fn map_candidate(err: crate::retrieval::candidates::CandidateError) -> ScoutError {
    match err {
        crate::retrieval::candidates::CandidateError::Cancelled => ScoutError::Cancelled,
        crate::retrieval::candidates::CandidateError::InvalidPolicy
        | crate::retrieval::candidates::CandidateError::InvalidQuery => ScoutError::InvalidPolicy,
    }
}

fn map_filter(err: crate::retrieval::filter::FilterError) -> ScoutError {
    match err {
        crate::retrieval::filter::FilterError::Cancelled => ScoutError::Cancelled,
        crate::retrieval::filter::FilterError::Timeout => ScoutError::Timeout,
    }
}

fn map_rank(err: crate::retrieval::rank::RankError) -> ScoutError {
    match err {
        crate::retrieval::rank::RankError::Cancelled => ScoutError::Cancelled,
        crate::retrieval::rank::RankError::InvalidConfig => ScoutError::InvalidPolicy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{ChunkPolicy, Document, chunk};
    use crate::index::fts::FtsDocument;
    use crate::index::vector::{
        EmbeddingProvider, EmbeddingVersion, VectorDocument, VectorError, VectorLimits,
    };
    use crate::ingest::content::{ContentHash, SourceLanguage};
    use crate::need::{CodeRef, CompletenessRequirement, ScopeSet};
    use protocol::{RedactionClass, RepoId};

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

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn representative(questions: &[&str], scope: ScopeSet) -> InformationNeed {
        InformationNeed::new(
            questions.iter().map(|s| (*s).to_string()).collect(),
            scope,
            Vec::new(),
            CompletenessRequirement::Representative,
            false,
            false,
            false,
            false,
            NegativeClaimPolicy::RequireCheckedScope,
            256,
        )
        .expect("need")
    }

    fn fts_from(repo: RepoId, rel: &str, src: &str) -> FtsDocument {
        let document = Document::new(repo, path(rel), Some(SourceLanguage::Rust), src);
        let chunks = chunk(
            &document,
            &[],
            &ChunkPolicy::new().overlap_bytes(0).overlap_tokens(0),
        )
        .expect("chunk");
        FtsDocument::from_records(&chunks, &[]).expect("fts")
    }

    fn vector_from(repo: RepoId, rel: &str, src: &str) -> VectorDocument {
        let document = Document::new(repo, path(rel), Some(SourceLanguage::Rust), src);
        let chunks = chunk(
            &document,
            &[],
            &ChunkPolicy::new().overlap_bytes(0).overlap_tokens(0),
        )
        .expect("chunk");
        VectorDocument::from_records(&chunks, RedactionClass::Public).expect("vector")
    }

    #[test]
    fn scout_returns_typed_report_without_mutating_need() {
        let need = representative(
            &["where is helper"],
            ScopeSet::new(vec!["src".into()]).expect("scope"),
        );
        let report = scout(&need, ScoutSources::new(), &ScoutLimits::new()).expect("scout");
        assert!(report.summary().contains("references="));
        assert_eq!(report.scope().requested(), &["src".to_string()]);
        assert!(!report.search_log_ref().to_string().is_empty());
        assert_eq!(need.questions(), &["where is helper".to_string()]);
        assert!(!report.summary().contains("src/secret.rs"));
    }

    #[test]
    fn zero_hits_are_broadened_before_negative_finding() {
        let need = representative(
            &["unique_missing_symbol_xyz"],
            ScopeSet::new(vec!["src/core".into()]).expect("scope"),
        );
        let report = scout(&need, ScoutSources::new(), &ScoutLimits::new()).expect("scout");
        assert!(report.scope().broadened());
        assert_eq!(report.negative_findings().len(), 1);
        let finding = &report.negative_findings()[0];
        assert!(!finding.scopes().is_empty());
        assert!(!finding.patterns().is_empty());
        assert_eq!(finding.confidence(), NegativeConfidence::High);
        assert!(
            report
                .open_questions()
                .contains(&"unique_missing_symbol_xyz".to_string())
        );
    }

    #[test]
    fn allow_unchecked_negative_is_not_high_confidence() {
        let need = InformationNeed::new(
            vec!["absent_thing".into()],
            ScopeSet::empty(),
            Vec::new(),
            CompletenessRequirement::Representative,
            false,
            false,
            false,
            false,
            NegativeClaimPolicy::AllowUnchecked,
            128,
        )
        .expect("need");
        let report = scout(&need, ScoutSources::new(), &ScoutLimits::new()).expect("scout");
        assert_eq!(report.negative_findings().len(), 1);
        assert_ne!(
            report.negative_findings()[0].confidence(),
            NegativeConfidence::High
        );
    }

    #[test]
    fn explicit_anchor_becomes_snippet_and_reference() {
        let need = InformationNeed::new(
            vec!["inspect helper".into()],
            ScopeSet::new(vec!["src".into()]).expect("scope"),
            vec![CodeRef::new("src/lib.rs", Some("helper".into())).expect("anchor")],
            CompletenessRequirement::Representative,
            false,
            false,
            false,
            false,
            NegativeClaimPolicy::RequireCheckedScope,
            128,
        )
        .expect("need");
        let report = scout(&need, ScoutSources::new(), &ScoutLimits::new()).expect("scout");
        assert_eq!(report.references().len(), 1);
        assert_eq!(report.references()[0].path().as_str(), "src/lib.rs");
        assert_eq!(report.snippets().len(), 1);
        assert!(report.negative_findings().is_empty());
        assert!(report.open_questions().is_empty());
    }

    #[test]
    fn exhaustive_lists_all_unique_hits_not_just_snippets() {
        let repo = RepoId::new();
        let mut fts = FtsIndex::open_in_memory(Default::default()).expect("fts");
        fts.upsert_document(&fts_from(repo, "src/a.rs", "fn alpha_token() {}\n"))
            .expect("a");
        fts.upsert_document(&fts_from(repo, "src/b.rs", "fn alpha_token() {}\n"))
            .expect("b");
        fts.upsert_document(&fts_from(repo, "src/c.rs", "fn alpha_token() {}\n"))
            .expect("c");
        let need = InformationNeed::new(
            vec!["alpha_token".into()],
            ScopeSet::new(vec!["src".into()]).expect("scope"),
            Vec::new(),
            CompletenessRequirement::Exhaustive,
            false,
            false,
            false,
            false,
            NegativeClaimPolicy::RequireCheckedScope,
            256,
        )
        .expect("need");
        let report = scout(
            &need,
            ScoutSources::new().fts(&fts),
            &ScoutLimits::new().max_snippets(1),
        )
        .expect("scout");
        assert_eq!(report.snippets().len(), 1);
        assert!(report.references().len() >= 3);
        let paths: BTreeSet<_> = report
            .references()
            .iter()
            .map(|r| r.path().as_str())
            .collect();
        assert!(paths.contains("src/a.rs"));
        assert!(paths.contains("src/b.rs"));
        assert!(paths.contains("src/c.rs"));
    }

    #[test]
    fn semantic_first_includes_vector_hits_when_configured() {
        let repo = RepoId::new();
        let src = "fn vector_only_body() {}\n";
        let provider = HashEmbeddingProvider::new(8);
        let mut vector = VectorIndex::open_in_memory(VectorLimits::new()).expect("vector");
        vector
            .upsert_document(&vector_from(repo, "src/embed.rs", src), &provider)
            .expect("upsert");
        let embedding = provider
            .embed(&[src], &CancellationToken::new())
            .expect("embed")
            .remove(0);
        let need = representative(
            &["unrelated lexical query zzzz"],
            ScopeSet::new(vec!["src".into()]).expect("scope"),
        );
        let report = scout(
            &need,
            ScoutSources::new().vector(&vector, embedding),
            &ScoutLimits::new(),
        )
        .expect("scout");
        assert!(
            report
                .references()
                .iter()
                .any(|r| r.path().as_str() == "src/embed.rs"
                    && r.method() == RetrievalMethod::Vector)
        );
    }

    #[test]
    fn missing_optional_indexes_do_not_fail_scout() {
        let need = representative(&["anything"], ScopeSet::empty());
        let report = scout(&need, ScoutSources::new(), &ScoutLimits::new()).expect("scout");
        assert!(!report.negative_findings().is_empty());
        assert_eq!(report.references().len(), 0);
    }

    #[test]
    fn cancelled_scout_fails_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let need = representative(&["x"], ScopeSet::empty());
        assert_eq!(
            scout(
                &need,
                ScoutSources::new(),
                &ScoutLimits::new().cancellation(cancel)
            ),
            Err(ScoutError::Cancelled)
        );
    }

    #[test]
    fn error_display_is_safe() {
        assert_eq!(ScoutError::Cancelled.to_string(), "cancelled");
        assert!(!ScoutError::Timeout.to_string().contains('/'));
        assert_eq!(RetrievalMethod::Vector.as_str(), "vector");
    }

    #[test]
    fn broaden_finds_out_of_scope_pin() {
        let need = InformationNeed::new(
            vec!["helper".into()],
            ScopeSet::new(vec!["src/core".into()]).expect("scope"),
            vec![CodeRef::new("vendor/lib.rs", Some("helper".into())).expect("anchor")],
            CompletenessRequirement::Representative,
            false,
            false,
            false,
            false,
            NegativeClaimPolicy::RequireCheckedScope,
            128,
        )
        .expect("need");
        let report = scout(&need, ScoutSources::new(), &ScoutLimits::new()).expect("scout");
        assert!(report.scope().broadened());
        assert_eq!(report.references()[0].path().as_str(), "vendor/lib.rs");
        assert!(report.negative_findings().is_empty());
    }
}
