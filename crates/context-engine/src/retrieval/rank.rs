//! Weighted reciprocal-rank fusion and MMR redundancy suppression.
//!
//! Candidate pools are fused by identity (chunk, then exact range, then
//! path/symbol). Overlapping ranges of the same file are not merged; MMR
//! penalizes them so a requested limit prefers diverse hits.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use protocol::{RepoId, RepoPath};

use crate::ingest::content::SourceLanguage;
use crate::repo_manifest::CancellationToken;
use crate::retrieval::candidates::{
    CandidatePools, CandidateSource, ContextCandidate, Freshness, InclusionReason, TrustClass,
};

/// Standard RRF smoothing constant. Rank 1 scores `1 / (k + 1)`.
pub const DEFAULT_RRF_K: f32 = 60.0;

const CANCEL_STRIDE: usize = 16;

/// Per-source fusion weights. Defaults match the context-engine architecture.
#[derive(Clone, Debug, PartialEq)]
pub struct RankWeights {
    explicit: f32,
    symbol: f32,
    lexical: f32,
    vector: f32,
    graph: f32,
    recency: f32,
    diff_error: f32,
    explicit_boost: f32,
    stale_boost: f32,
}

/// One fused, MMR-selected hit. `score` is the pre-MMR fused relevance.
#[derive(Clone, Debug, PartialEq)]
pub struct RankedContextHit {
    rank: u32,
    fused_score: f32,
    mmr_score: f32,
    sources: Vec<CandidateSource>,
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

/// Typed ranking failure. Display never echoes paths or source text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RankError {
    Cancelled,
    InvalidConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum FusionKey {
    Chunk {
        repo: Option<RepoId>,
        chunk_id: String,
    },
    Range {
        repo: Option<RepoId>,
        path: String,
        start: u32,
        end: u32,
    },
    Symbol {
        repo: Option<RepoId>,
        path: Option<String>,
        symbol: String,
    },
    Path {
        repo: Option<RepoId>,
        path: String,
    },
    Singleton(u32),
}

struct FusedCandidate {
    fused_score: f32,
    relevance: f32,
    mmr_score: f32,
    sources: Vec<CandidateSource>,
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
    primary_rank: u32,
    primary_source: CandidateSource,
}

impl RankWeights {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn explicit(mut self, value: f32) -> Self {
        self.explicit = value;
        self
    }

    pub fn symbol(mut self, value: f32) -> Self {
        self.symbol = value;
        self
    }

    pub fn lexical(mut self, value: f32) -> Self {
        self.lexical = value;
        self
    }

    pub fn vector(mut self, value: f32) -> Self {
        self.vector = value;
        self
    }

    pub fn graph(mut self, value: f32) -> Self {
        self.graph = value;
        self
    }

    pub fn recency(mut self, value: f32) -> Self {
        self.recency = value;
        self
    }

    pub fn diff_error(mut self, value: f32) -> Self {
        self.diff_error = value;
        self
    }

    pub fn explicit_boost(mut self, value: f32) -> Self {
        self.explicit_boost = value;
        self
    }

    pub fn stale_boost(mut self, value: f32) -> Self {
        self.stale_boost = value;
        self
    }

    pub fn explicit_value(&self) -> f32 {
        self.explicit
    }

    pub fn symbol_value(&self) -> f32 {
        self.symbol
    }

    pub fn lexical_value(&self) -> f32 {
        self.lexical
    }

    pub fn vector_value(&self) -> f32 {
        self.vector
    }

    pub fn graph_value(&self) -> f32 {
        self.graph
    }

    pub fn recency_value(&self) -> f32 {
        self.recency
    }

    pub fn diff_error_value(&self) -> f32 {
        self.diff_error
    }

    pub fn weight(&self, source: CandidateSource) -> f32 {
        match source {
            CandidateSource::Explicit => self.explicit,
            CandidateSource::Symbol => self.symbol,
            CandidateSource::Lexical => self.lexical,
            CandidateSource::Vector => self.vector,
            CandidateSource::Graph => self.graph,
            CandidateSource::ReadSet => self.recency,
            CandidateSource::DiffError => self.diff_error,
        }
    }

    fn explicit_boost_for(&self, source: CandidateSource) -> f32 {
        if source == CandidateSource::Explicit {
            self.explicit_boost
        } else {
            0.0
        }
    }

    fn freshness_boost(&self, freshness: Freshness) -> f32 {
        match freshness {
            Freshness::Stale => self.stale_boost,
            Freshness::Fresh | Freshness::Unknown => 0.0,
        }
    }

    fn validate(&self) -> Result<(), RankError> {
        let values = [
            self.explicit,
            self.symbol,
            self.lexical,
            self.vector,
            self.graph,
            self.recency,
            self.diff_error,
            self.explicit_boost,
            self.stale_boost,
        ];
        if values.iter().any(|v| !v.is_finite() || *v < 0.0) {
            return Err(RankError::InvalidConfig);
        }
        Ok(())
    }
}

impl Default for RankWeights {
    fn default() -> Self {
        Self {
            explicit: 1.0,
            symbol: 0.85,
            lexical: 0.75,
            vector: 0.70,
            graph: 0.65,
            recency: 0.35,
            diff_error: 0.90,
            explicit_boost: 0.0,
            stale_boost: 0.0,
        }
    }
}

impl RankedContextHit {
    pub fn rank(&self) -> u32 {
        self.rank
    }

    pub fn score(&self) -> f32 {
        self.fused_score
    }

    pub fn fused_score(&self) -> f32 {
        self.fused_score
    }

    pub fn mmr_score(&self) -> f32 {
        self.mmr_score
    }

    pub fn sources(&self) -> &[CandidateSource] {
        &self.sources
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

impl RankError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::InvalidConfig => "invalid_config",
        }
    }
}

impl fmt::Display for RankError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for RankError {}

/// Fuse `pools` with weighted RRF, then apply MMR under `limit`.
///
/// `lambda` is the MMR relevance weight in `[0, 1]`. `1.0` is pure fused
/// order; lower values penalize overlap with already selected hits.
pub fn rank(
    pools: &CandidatePools,
    weights: &RankWeights,
    lambda: f32,
    limit: u32,
) -> Result<Vec<RankedContextHit>, RankError> {
    rank_cancelled(pools, weights, lambda, limit, &CancellationToken::new())
}

/// [`rank`] with an explicit cancellation token. Cancellation fails closed.
pub fn rank_cancelled(
    pools: &CandidatePools,
    weights: &RankWeights,
    lambda: f32,
    limit: u32,
    cancel: &CancellationToken,
) -> Result<Vec<RankedContextHit>, RankError> {
    check_cancel(cancel)?;
    weights.validate()?;
    if !lambda.is_finite() || !(0.0..=1.0).contains(&lambda) {
        return Err(RankError::InvalidConfig);
    }
    if limit == 0 {
        return Ok(Vec::new());
    }

    let mut fused = fuse_pools(pools, weights, cancel)?;
    if fused.is_empty() {
        return Ok(Vec::new());
    }
    normalize_relevance(&mut fused);
    let selected = mmr_select(fused, lambda, limit, cancel)?;
    Ok(selected
        .into_iter()
        .enumerate()
        .map(|(idx, item)| item.into_hit((idx as u32).saturating_add(1)))
        .collect())
}

fn fuse_pools(
    pools: &CandidatePools,
    weights: &RankWeights,
    cancel: &CancellationToken,
) -> Result<Vec<FusedCandidate>, RankError> {
    let mut merged: BTreeMap<FusionKey, FusedCandidate> = BTreeMap::new();
    let mut next_singleton = 0_u32;
    let mut step = 0_usize;

    for candidate in all_candidates(pools) {
        if step.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        step = step.saturating_add(1);

        let contribution = source_score(candidate, weights);
        if !contribution.is_finite() || contribution < 0.0 {
            return Err(RankError::InvalidConfig);
        }

        let key = fusion_key(candidate, &mut next_singleton);
        match merged.get_mut(&key) {
            Some(existing) => existing.merge(candidate, contribution, weights),
            None => {
                merged.insert(key, FusedCandidate::from_candidate(candidate, contribution));
            }
        }
    }

    check_cancel(cancel)?;
    Ok(merged.into_values().collect())
}

fn all_candidates(pools: &CandidatePools) -> impl Iterator<Item = &ContextCandidate> {
    pools
        .explicit()
        .iter()
        .chain(pools.fts())
        .chain(pools.vector())
        .chain(pools.graph())
        .chain(pools.diff_error())
        .chain(pools.read_set())
}

fn source_score(candidate: &ContextCandidate, weights: &RankWeights) -> f32 {
    weights.weight(candidate.source()) * reciprocal_rank(candidate.rank())
        + weights.explicit_boost_for(candidate.source())
        + weights.freshness_boost(candidate.freshness())
}

fn reciprocal_rank(rank: u32) -> f32 {
    1.0 / (DEFAULT_RRF_K + rank.max(1) as f32)
}

fn fusion_key(candidate: &ContextCandidate, next_singleton: &mut u32) -> FusionKey {
    if let Some(chunk_id) = candidate.chunk_id() {
        return FusionKey::Chunk {
            repo: candidate.repo_id(),
            chunk_id: chunk_id.to_string(),
        };
    }
    match (
        candidate.path(),
        candidate.start_byte(),
        candidate.end_byte(),
        candidate.symbol(),
    ) {
        (Some(path), Some(start), Some(end), _) => FusionKey::Range {
            repo: candidate.repo_id(),
            path: path.as_str().to_string(),
            start,
            end,
        },
        (path, _, _, Some(symbol)) => FusionKey::Symbol {
            repo: candidate.repo_id(),
            path: path.map(|p| p.as_str().to_string()),
            symbol: symbol.to_string(),
        },
        (Some(path), _, _, None) => FusionKey::Path {
            repo: candidate.repo_id(),
            path: path.as_str().to_string(),
        },
        (None, _, _, None) => {
            let id = *next_singleton;
            *next_singleton = next_singleton.saturating_add(1);
            FusionKey::Singleton(id)
        }
    }
}

fn normalize_relevance(fused: &mut [FusedCandidate]) {
    let mut max = 0.0_f32;
    for item in fused.iter() {
        if item.fused_score > max {
            max = item.fused_score;
        }
    }
    if max > 0.0 && max.is_finite() {
        for item in fused.iter_mut() {
            item.relevance = item.fused_score / max;
        }
    } else {
        for item in fused.iter_mut() {
            item.relevance = 0.0;
        }
    }
}

fn mmr_select(
    mut remaining: Vec<FusedCandidate>,
    lambda: f32,
    limit: u32,
    cancel: &CancellationToken,
) -> Result<Vec<FusedCandidate>, RankError> {
    let mut selected = Vec::new();
    let take = (limit as usize).min(remaining.len());
    while selected.len() < take && !remaining.is_empty() {
        if selected.len().is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }

        let mut best_idx = 0_usize;
        let mut best_mmr = mmr_score(&remaining[0], &selected, lambda);
        for idx in 1..remaining.len() {
            let score = mmr_score(&remaining[idx], &selected, lambda);
            match score.total_cmp(&best_mmr) {
                Ordering::Greater => {
                    best_idx = idx;
                    best_mmr = score;
                }
                Ordering::Equal => {
                    if remaining[idx].cmp_tiebreak(&remaining[best_idx]) == Ordering::Less {
                        best_idx = idx;
                        best_mmr = score;
                    }
                }
                Ordering::Less => {}
            }
        }

        let mut chosen = remaining.remove(best_idx);
        chosen.mmr_score = best_mmr;
        selected.push(chosen);
    }
    check_cancel(cancel)?;
    Ok(selected)
}

fn mmr_score(candidate: &FusedCandidate, selected: &[FusedCandidate], lambda: f32) -> f32 {
    let redundancy = selected
        .iter()
        .map(|item| similarity(candidate, item))
        .fold(0.0_f32, f32::max);
    lambda * candidate.relevance - (1.0 - lambda) * redundancy
}

fn similarity(left: &FusedCandidate, right: &FusedCandidate) -> f32 {
    if same_chunk(left, right) {
        return 1.0;
    }
    if !same_file(left, right) {
        return 0.0;
    }
    match (
        left.start_byte,
        left.end_byte,
        right.start_byte,
        right.end_byte,
    ) {
        (Some(ls), Some(le), Some(rs), Some(re)) => range_jaccard(ls, le, rs, re),
        _ => 0.5,
    }
}

fn same_chunk(left: &FusedCandidate, right: &FusedCandidate) -> bool {
    match (left.chunk_id.as_deref(), right.chunk_id.as_deref()) {
        (Some(a), Some(b)) => a == b && left.repo_id == right.repo_id,
        _ => false,
    }
}

fn same_file(left: &FusedCandidate, right: &FusedCandidate) -> bool {
    match (left.path.as_ref(), right.path.as_ref()) {
        (Some(a), Some(b)) => a == b && left.repo_id == right.repo_id,
        _ => false,
    }
}

fn range_jaccard(a_start: u32, a_end: u32, b_start: u32, b_end: u32) -> f32 {
    let a_end = a_end.max(a_start);
    let b_end = b_end.max(b_start);
    let inter_start = a_start.max(b_start);
    let inter_end = a_end.min(b_end);
    if inter_end <= inter_start {
        return 0.0;
    }
    let union_start = a_start.min(b_start);
    let union_end = a_end.max(b_end);
    let union = union_end.saturating_sub(union_start);
    if union == 0 {
        return 0.0;
    }
    inter_end.saturating_sub(inter_start) as f32 / union as f32
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), RankError> {
    if cancel.is_cancelled() {
        Err(RankError::Cancelled)
    } else {
        Ok(())
    }
}

impl FusedCandidate {
    fn from_candidate(candidate: &ContextCandidate, fused_score: f32) -> Self {
        Self {
            fused_score,
            relevance: 0.0,
            mmr_score: 0.0,
            sources: vec![candidate.source()],
            reason: candidate.reason(),
            repo_id: candidate.repo_id(),
            path: candidate.path().cloned(),
            language: candidate.language(),
            chunk_id: candidate.chunk_id().map(str::to_string),
            content_hash: candidate.content_hash().map(str::to_string),
            start_byte: candidate.start_byte(),
            end_byte: candidate.end_byte(),
            symbol: candidate.symbol().map(str::to_string),
            freshness: candidate.freshness(),
            trust: candidate.trust(),
            primary_rank: candidate.rank(),
            primary_source: candidate.source(),
        }
    }

    fn merge(&mut self, candidate: &ContextCandidate, contribution: f32, weights: &RankWeights) {
        self.fused_score += contribution;
        if !self.sources.contains(&candidate.source()) {
            self.sources.push(candidate.source());
            self.sources.sort_unstable();
        }
        if self.trust == TrustClass::Untrusted || candidate.trust() == TrustClass::Untrusted {
            self.trust = TrustClass::Untrusted;
        }
        self.freshness = merge_freshness(self.freshness, candidate.freshness());
        if candidate.language().is_some() && self.language.is_none() {
            self.language = candidate.language();
        }
        if candidate.content_hash().is_some() && self.content_hash.is_none() {
            self.content_hash = candidate.content_hash().map(str::to_string);
        }
        if candidate.symbol().is_some() && self.symbol.is_none() {
            self.symbol = candidate.symbol().map(str::to_string);
        }
        if candidate.chunk_id().is_some() && self.chunk_id.is_none() {
            self.chunk_id = candidate.chunk_id().map(str::to_string);
        }
        if candidate.path().is_some() && self.path.is_none() {
            self.path = candidate.path().cloned();
        }
        if (self.start_byte.is_none() || self.end_byte.is_none())
            && candidate.start_byte().is_some()
            && candidate.end_byte().is_some()
        {
            self.start_byte = candidate.start_byte();
            self.end_byte = candidate.end_byte();
        }

        let incoming_better = match weights
            .weight(candidate.source())
            .total_cmp(&weights.weight(self.primary_source))
        {
            Ordering::Greater => true,
            Ordering::Equal => {
                candidate.rank() < self.primary_rank
                    || (candidate.rank() == self.primary_rank
                        && candidate.source() < self.primary_source)
            }
            Ordering::Less => false,
        };
        if incoming_better {
            self.reason = candidate.reason();
            self.primary_rank = candidate.rank();
            self.primary_source = candidate.source();
        }
    }

    fn cmp_tiebreak(&self, other: &Self) -> Ordering {
        path_key(self.path.as_ref())
            .cmp(path_key(other.path.as_ref()))
            .then_with(|| {
                self.start_byte
                    .unwrap_or(u32::MAX)
                    .cmp(&other.start_byte.unwrap_or(u32::MAX))
            })
            .then_with(|| {
                self.end_byte
                    .unwrap_or(u32::MAX)
                    .cmp(&other.end_byte.unwrap_or(u32::MAX))
            })
            .then_with(|| {
                self.chunk_id
                    .as_deref()
                    .unwrap_or("")
                    .cmp(other.chunk_id.as_deref().unwrap_or(""))
            })
            .then_with(|| self.primary_source.cmp(&other.primary_source))
            .then_with(|| {
                self.symbol
                    .as_deref()
                    .unwrap_or("")
                    .cmp(other.symbol.as_deref().unwrap_or(""))
            })
            .then_with(|| {
                self.content_hash
                    .as_deref()
                    .unwrap_or("")
                    .cmp(other.content_hash.as_deref().unwrap_or(""))
            })
            .then_with(|| self.repo_id.cmp(&other.repo_id))
    }

    fn into_hit(self, rank: u32) -> RankedContextHit {
        RankedContextHit {
            rank,
            fused_score: self.fused_score,
            mmr_score: self.mmr_score,
            sources: self.sources,
            reason: self.reason,
            repo_id: self.repo_id,
            path: self.path,
            language: self.language,
            chunk_id: self.chunk_id,
            content_hash: self.content_hash,
            start_byte: self.start_byte,
            end_byte: self.end_byte,
            symbol: self.symbol,
            freshness: self.freshness,
            trust: self.trust,
        }
    }
}

fn merge_freshness(left: Freshness, right: Freshness) -> Freshness {
    match (left, right) {
        (Freshness::Stale, _) | (_, Freshness::Stale) => Freshness::Stale,
        (Freshness::Fresh, _) | (_, Freshness::Fresh) => Freshness::Fresh,
        (Freshness::Unknown, Freshness::Unknown) => Freshness::Unknown,
    }
}

fn path_key(path: Option<&RepoPath>) -> &str {
    path.map(RepoPath::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::content::ContentHash;
    use crate::retrieval::candidates::{
        CandidateLimits, CandidateSources, ContextQuery, DiffErrorEvidence, DiffErrorKind,
        ExplicitPin, ReadSetItem, generate_candidates,
    };

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn hash(text: &str) -> ContentHash {
        ContentHash::from_bytes(text.as_bytes())
    }

    fn pools_from_query(query: ContextQuery) -> CandidatePools {
        generate_candidates(&query, CandidateSources::new(), &CandidateLimits::new())
            .expect("pools")
    }

    fn hit_path(hit: &RankedContextHit) -> &str {
        hit.path().expect("path").as_str()
    }

    #[test]
    fn rank_is_deterministic_including_tie_breaks() {
        let weights = RankWeights::new().explicit(1.0).diff_error(1.0);
        let query = ContextQuery::new("")
            .pin(ExplicitPin::new(path("z.rs")).range(0, 10))
            .diff_error(DiffErrorEvidence::new(path("a.rs"), DiffErrorKind::Diff).range(0, 10));
        let pools = pools_from_query(query);

        let first = rank(&pools, &weights, 1.0, 8).expect("rank");
        let second = rank(&pools, &weights, 1.0, 8).expect("rank");
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert_eq!(hit_path(&first[0]), "a.rs");
        assert_eq!(hit_path(&first[1]), "z.rs");
        assert!((first[0].fused_score() - first[1].fused_score()).abs() < 1e-6);
    }

    #[test]
    fn overlapping_chunks_are_penalized_by_mmr() {
        let query = ContextQuery::new("")
            .pin(ExplicitPin::new(path("overlap.rs")).range(0, 100))
            .pin(ExplicitPin::new(path("overlap.rs")).range(50, 150))
            .pin(ExplicitPin::new(path("other.rs")).range(0, 100));
        let pools = pools_from_query(query);
        let weights = RankWeights::new();

        let diverse = rank(&pools, &weights, 0.5, 2).expect("mmr");
        assert_eq!(diverse.len(), 2);
        assert_eq!(hit_path(&diverse[0]), "overlap.rs");
        assert_eq!(diverse[0].start_byte(), Some(0));
        assert_eq!(hit_path(&diverse[1]), "other.rs");

        let relevance = rank(&pools, &weights, 1.0, 3).expect("relevance");
        assert_eq!(relevance.len(), 3);
        assert_eq!(hit_path(&relevance[0]), "overlap.rs");
        assert_eq!(relevance[0].start_byte(), Some(0));
        assert_eq!(hit_path(&relevance[1]), "overlap.rs");
        assert_eq!(relevance[1].start_byte(), Some(50));
        assert_eq!(hit_path(&relevance[2]), "other.rs");
    }

    #[test]
    fn mmr_keeps_fused_order_for_disjoint_files() {
        let query = ContextQuery::new("")
            .pin(ExplicitPin::new(path("src/a.rs")).range(0, 8))
            .pin(ExplicitPin::new(path("src/b.rs")).range(0, 8));
        let pools = pools_from_query(query);
        let weights = RankWeights::new();
        let fused = rank(&pools, &weights, 1.0, 2).expect("fused");
        let diverse = rank(&pools, &weights, 0.5, 2).expect("mmr");
        assert_eq!(
            fused.iter().map(hit_path).collect::<Vec<_>>(),
            diverse.iter().map(hit_path).collect::<Vec<_>>()
        );
        assert_eq!(hit_path(&diverse[0]), "src/a.rs");
        assert_eq!(hit_path(&diverse[1]), "src/b.rs");
    }

    #[test]
    fn mmr_lambda_zero_prefers_unrelated_file_over_overlap() {
        let query = ContextQuery::new("")
            .pin(ExplicitPin::new(path("overlap.rs")).range(0, 100))
            .pin(ExplicitPin::new(path("overlap.rs")).range(50, 150))
            .pin(ExplicitPin::new(path("other.rs")).range(0, 100));
        let pools = pools_from_query(query);
        let hits = rank(&pools, &RankWeights::new(), 0.0, 2).expect("mmr");
        assert_eq!(hits.len(), 2);
        let paths: Vec<&str> = hits.iter().map(hit_path).collect();
        assert!(paths.contains(&"overlap.rs"));
        assert!(paths.contains(&"other.rs"));
        assert_ne!(hits[0].path(), hits[1].path());
    }

    #[test]
    fn identical_chunk_identity_is_fused_across_pools() {
        let query = ContextQuery::new("")
            .pin(
                ExplicitPin::new(path("src/lib.rs"))
                    .range(0, 40)
                    .chunk_id("chunk-a"),
            )
            .diff_error(
                DiffErrorEvidence::new(path("src/lib.rs"), DiffErrorKind::Diagnostic)
                    .range(0, 40)
                    .chunk_id("chunk-a"),
            );
        let pools = pools_from_query(query);
        assert_eq!(pools.explicit().len(), 1);
        assert_eq!(pools.diff_error().len(), 1);

        let hits = rank(&pools, &RankWeights::new(), 1.0, 4).expect("rank");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id(), Some("chunk-a"));
        assert!(hits[0].sources().contains(&CandidateSource::Explicit));
        assert!(hits[0].sources().contains(&CandidateSource::DiffError));
        let expected = RankWeights::new().weight(CandidateSource::Explicit) * reciprocal_rank(1)
            + RankWeights::new().weight(CandidateSource::DiffError) * reciprocal_rank(1);
        assert!((hits[0].fused_score() - expected).abs() < 1e-6);
        assert_eq!(hits[0].trust(), TrustClass::Untrusted);
    }

    #[test]
    fn golden_fused_order_matches_weighted_rrf() {
        let query = ContextQuery::new("")
            .pin(ExplicitPin::new(path("src/a.rs")).range(0, 8))
            .pin(ExplicitPin::new(path("src/b.rs")).range(0, 8))
            .read_set_item(ReadSetItem::new(
                path("src/stale.rs"),
                0,
                8,
                hash("old"),
                hash("new"),
            ));
        let pools = pools_from_query(query);
        let hits = rank(&pools, &RankWeights::new(), 1.0, 10).expect("rank");
        let paths: Vec<&str> = hits.iter().map(hit_path).collect();
        assert_eq!(paths, ["src/a.rs", "src/b.rs", "src/stale.rs"]);
        assert_eq!(hits[0].rank(), 1);
        assert_eq!(hits[1].rank(), 2);
        assert_eq!(hits[2].rank(), 3);
        assert!(hits[0].fused_score() > hits[1].fused_score());
        assert!(hits[1].fused_score() > hits[2].fused_score());
        assert_eq!(hits[2].freshness(), Freshness::Stale);
        assert_eq!(hits[2].reason(), InclusionReason::ReadSetChanged);
    }

    #[test]
    fn cancelled_rank_fails_closed() {
        let query = ContextQuery::new("").pin(ExplicitPin::new(path("a.rs")).range(0, 4));
        let pools = pools_from_query(query);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            rank_cancelled(&pools, &RankWeights::new(), 0.7, 4, &cancel),
            Err(RankError::Cancelled)
        );
    }

    #[test]
    fn invalid_lambda_or_weight_is_rejected() {
        let query = ContextQuery::new("").pin(ExplicitPin::new(path("a.rs")).range(0, 4));
        let pools = pools_from_query(query);
        assert_eq!(
            rank(&pools, &RankWeights::new(), 1.5, 4),
            Err(RankError::InvalidConfig)
        );
        assert_eq!(
            rank(&pools, &RankWeights::new(), f32::NAN, 4),
            Err(RankError::InvalidConfig)
        );
        assert_eq!(
            rank(&pools, &RankWeights::new().lexical(-0.1), 0.5, 4),
            Err(RankError::InvalidConfig)
        );
        assert!(
            rank(&pools, &RankWeights::new(), 0.5, 0)
                .expect("zero limit")
                .is_empty()
        );
    }

    #[test]
    fn empty_pools_rank_to_empty() {
        let pools = pools_from_query(ContextQuery::new(""));
        let hits = rank(&pools, &RankWeights::new(), 1.0, 8).expect("rank");
        assert!(hits.is_empty());
    }

    #[test]
    fn path_identity_fuses_across_sources_without_chunk_id() {
        let query = ContextQuery::new("")
            .pin(ExplicitPin::new(path("src/lib.rs")))
            .diff_error(DiffErrorEvidence::new(
                path("src/lib.rs"),
                DiffErrorKind::Diff,
            ));
        let pools = pools_from_query(query);
        assert_eq!(pools.explicit().len(), 1);
        assert_eq!(pools.diff_error().len(), 1);

        let hits = rank(&pools, &RankWeights::new(), 1.0, 4).expect("rank");
        assert_eq!(hits.len(), 1);
        assert_eq!(hit_path(&hits[0]), "src/lib.rs");
        assert!(hits[0].sources().contains(&CandidateSource::Explicit));
        assert!(hits[0].sources().contains(&CandidateSource::DiffError));
        let expected = RankWeights::new().weight(CandidateSource::Explicit) * reciprocal_rank(1)
            + RankWeights::new().weight(CandidateSource::DiffError) * reciprocal_rank(1);
        assert!((hits[0].fused_score() - expected).abs() < 1e-6);
    }

    #[test]
    fn fused_score_is_weighted_rrf_not_raw_candidate_scores() {
        let query = ContextQuery::new("")
            .pin(
                ExplicitPin::new(path("src/lib.rs"))
                    .range(0, 40)
                    .chunk_id("chunk-n"),
            )
            .read_set_item(
                ReadSetItem::new(path("src/lib.rs"), 0, 40, hash("old"), hash("new"))
                    .chunk_id("chunk-n"),
            );
        let pools = pools_from_query(query);
        assert_eq!(pools.explicit()[0].score(), 1.0);
        assert_eq!(pools.read_set()[0].score(), 0.35);

        let hits = rank(&pools, &RankWeights::new(), 1.0, 4).expect("rank");
        assert_eq!(hits.len(), 1);
        let expected = RankWeights::new().weight(CandidateSource::Explicit) * reciprocal_rank(1)
            + RankWeights::new().weight(CandidateSource::ReadSet) * reciprocal_rank(1);
        assert!((hits[0].fused_score() - expected).abs() < 1e-6);
        let raw_sum = pools.explicit()[0].score() + pools.read_set()[0].score();
        assert!((hits[0].fused_score() - raw_sum).abs() > 0.5);
    }

    #[test]
    fn zero_weights_rank_by_stable_tiebreak() {
        let weights = RankWeights::new()
            .explicit(0.0)
            .diff_error(0.0)
            .lexical(0.0)
            .vector(0.0)
            .graph(0.0)
            .recency(0.0)
            .symbol(0.0);
        let query = ContextQuery::new("")
            .pin(ExplicitPin::new(path("z.rs")).range(0, 10))
            .diff_error(DiffErrorEvidence::new(path("a.rs"), DiffErrorKind::Diff).range(0, 10));
        let pools = pools_from_query(query);
        let hits = rank(&pools, &weights, 1.0, 8).expect("rank");
        assert_eq!(hits.len(), 2);
        assert_eq!(hit_path(&hits[0]), "a.rs");
        assert_eq!(hit_path(&hits[1]), "z.rs");
        assert_eq!(hits[0].fused_score(), 0.0);
        assert_eq!(hits[1].fused_score(), 0.0);
    }
}
