//! Scope, trust, and freshness filters applied after candidate generation.
//!
//! Out-of-scope and policy-excluded hits are omitted, not errors. Pathless
//! hits fail closed when a non-empty scope is set. Cancellation fails closed.

use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::RepoPath;

use crate::need::ScopeSet;
use crate::repo_manifest::CancellationToken;
use crate::retrieval::candidates::{CandidatePools, ContextCandidate, Freshness, TrustClass};
use crate::retrieval::rank::RankedContextHit;

/// Default wall-clock budget for one filter pass.
pub const DEFAULT_FILTER_TIMEOUT: Duration = Duration::from_secs(1);

const CANCEL_STRIDE: usize = 16;

/// Policy for keeping retrieved candidates/hits.
#[derive(Clone, Debug)]
pub struct RetrievalFilter {
    scope: ScopeSet,
    allow_untrusted: bool,
    allow_stale: bool,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Typed filter failure. Display never echoes paths or queries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterError {
    Cancelled,
    Timeout,
}

impl RetrievalFilter {
    /// Unrestricted scope; untrusted and stale hits stay visible.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn scope(mut self, scope: ScopeSet) -> Self {
        self.scope = scope;
        self
    }

    pub fn allow_untrusted(mut self, value: bool) -> Self {
        self.allow_untrusted = value;
        self
    }

    pub fn allow_stale(mut self, value: bool) -> Self {
        self.allow_stale = value;
        self
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn cancellation(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn scope_set(&self) -> &ScopeSet {
        &self.scope
    }

    pub fn allows_untrusted(&self) -> bool {
        self.allow_untrusted
    }

    pub fn allows_stale(&self) -> bool {
        self.allow_stale
    }

    pub fn allows(&self, path: Option<&RepoPath>, trust: TrustClass, freshness: Freshness) -> bool {
        self.allows_path(path) && self.allows_trust(trust) && self.allows_freshness(freshness)
    }

    fn allows_path(&self, path: Option<&RepoPath>) -> bool {
        let prefixes = self.scope.paths();
        if prefixes.is_empty() {
            return true;
        }
        let Some(path) = path else {
            return false;
        };
        prefixes
            .iter()
            .any(|prefix| path_matches_prefix(path.as_str(), prefix))
    }

    fn allows_trust(&self, trust: TrustClass) -> bool {
        self.allow_untrusted || trust != TrustClass::Untrusted
    }

    fn allows_freshness(&self, freshness: Freshness) -> bool {
        self.allow_stale || freshness != Freshness::Stale
    }
}

impl Default for RetrievalFilter {
    fn default() -> Self {
        Self {
            scope: ScopeSet::empty(),
            allow_untrusted: true,
            allow_stale: true,
            timeout: DEFAULT_FILTER_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }
}

impl FilterError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
        }
    }
}

impl fmt::Display for FilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for FilterError {}

/// Filter generator pools. Status rows are preserved; dropped hits are omitted.
pub fn filter_pools(
    pools: CandidatePools,
    filter: &RetrievalFilter,
) -> Result<CandidatePools, FilterError> {
    let started = Instant::now();
    check_bounds(&filter.cancel, started, filter.timeout)?;
    pools.filter(filter, started)
}

/// Filter fused/MMR hits. Rank numbers are left as produced by [`crate::rank`].
pub fn filter_ranked(
    hits: Vec<RankedContextHit>,
    filter: &RetrievalFilter,
) -> Result<Vec<RankedContextHit>, FilterError> {
    let started = Instant::now();
    filter_items(hits, filter, started, |hit| {
        filter.allows(hit.path(), hit.trust(), hit.freshness())
    })
}

pub(crate) fn filter_candidates(
    candidates: Vec<ContextCandidate>,
    filter: &RetrievalFilter,
    started: Instant,
) -> Result<Vec<ContextCandidate>, FilterError> {
    filter_items(candidates, filter, started, |candidate| {
        filter.allows(candidate.path(), candidate.trust(), candidate.freshness())
    })
}

fn filter_items<T>(
    items: Vec<T>,
    filter: &RetrievalFilter,
    started: Instant,
    keep: impl Fn(&T) -> bool,
) -> Result<Vec<T>, FilterError> {
    let mut out = Vec::with_capacity(items.len());
    for (step, item) in items.into_iter().enumerate() {
        if step.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(&filter.cancel, started, filter.timeout)?;
        }
        if keep(&item) {
            out.push(item);
        }
    }
    check_bounds(&filter.cancel, started, filter.timeout)?;
    Ok(out)
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return false;
    }
    path == prefix || (path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/'))
}

fn check_bounds(
    cancel: &CancellationToken,
    started: Instant,
    timeout: Duration,
) -> Result<(), FilterError> {
    if cancel.is_cancelled() {
        return Err(FilterError::Cancelled);
    }
    if timeout.is_zero() || started.elapsed() > timeout {
        return Err(FilterError::Timeout);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::content::ContentHash;
    use crate::need::ScopeSet;
    use crate::retrieval::candidates::{
        CandidateLimits, CandidateSources, ContextQuery, DiffErrorEvidence, DiffErrorKind,
        ExplicitPin, ReadSetItem, generate_candidates,
    };
    use crate::retrieval::rank::{RankWeights, rank};
    use protocol::RepoPath;

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn hash(text: &str) -> ContentHash {
        ContentHash::from_bytes(text.as_bytes())
    }

    fn pools(query: ContextQuery) -> CandidatePools {
        generate_candidates(&query, CandidateSources::new(), &CandidateLimits::new())
            .expect("pools")
    }

    fn hit_path(hit: &RankedContextHit) -> &str {
        hit.path().expect("path").as_str()
    }

    #[test]
    fn empty_scope_keeps_all_hits() {
        let query = ContextQuery::new("")
            .pin(ExplicitPin::new(path("src/a.rs")).range(0, 8))
            .pin(ExplicitPin::new(path("crates/b.rs")).range(0, 8));
        let ranked = rank(&pools(query), &RankWeights::new(), 1.0, 8).expect("rank");
        let kept = filter_ranked(ranked.clone(), &RetrievalFilter::new()).expect("filter");
        assert_eq!(kept.len(), ranked.len());
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn out_of_scope_paths_are_omitted() {
        let query = ContextQuery::new("")
            .pin(ExplicitPin::new(path("src/lib.rs")).range(0, 8))
            .pin(ExplicitPin::new(path("crates/kernel/src/lib.rs")).range(0, 8))
            .pin(ExplicitPin::new(path("src2/other.rs")).range(0, 8));
        let ranked = rank(&pools(query), &RankWeights::new(), 1.0, 8).expect("rank");
        let filter =
            RetrievalFilter::new().scope(ScopeSet::new(vec!["src".into()]).expect("scope"));
        let kept = filter_ranked(ranked, &filter).expect("filter");
        assert_eq!(kept.len(), 1);
        assert_eq!(hit_path(&kept[0]), "src/lib.rs");
    }

    #[test]
    fn prefix_does_not_match_sibling_names() {
        let query = ContextQuery::new("")
            .pin(ExplicitPin::new(path("src/lib.rs")).range(0, 8))
            .pin(ExplicitPin::new(path("srcfoo/lib.rs")).range(0, 8));
        let ranked = rank(&pools(query), &RankWeights::new(), 1.0, 8).expect("rank");
        let filter =
            RetrievalFilter::new().scope(ScopeSet::new(vec!["src".into()]).expect("scope"));
        let kept = filter_ranked(ranked, &filter).expect("filter");
        assert_eq!(kept.len(), 1);
        assert_eq!(hit_path(&kept[0]), "src/lib.rs");
    }

    #[test]
    fn project_only_drops_untrusted_repo_hits() {
        let query = ContextQuery::new("").pin(ExplicitPin::new(path("src/lib.rs")).range(0, 8));
        let ranked = rank(&pools(query), &RankWeights::new(), 1.0, 4).expect("rank");
        assert_eq!(ranked[0].trust(), TrustClass::Untrusted);
        let kept =
            filter_ranked(ranked, &RetrievalFilter::new().allow_untrusted(false)).expect("filter");
        assert!(kept.is_empty());
    }

    #[test]
    fn stale_only_dropped_when_policy_disallows() {
        let query = ContextQuery::new("")
            .pin(ExplicitPin::new(path("src/fresh.rs")).range(0, 8))
            .read_set_item(ReadSetItem::new(
                path("src/stale.rs"),
                0,
                8,
                hash("old"),
                hash("new"),
            ));
        let ranked = rank(&pools(query), &RankWeights::new(), 1.0, 8).expect("rank");
        assert!(ranked.iter().any(|h| h.freshness() == Freshness::Stale));
        let kept =
            filter_ranked(ranked, &RetrievalFilter::new().allow_stale(false)).expect("filter");
        assert_eq!(kept.len(), 1);
        assert_eq!(hit_path(&kept[0]), "src/fresh.rs");
        assert_ne!(kept[0].freshness(), Freshness::Stale);
    }

    #[test]
    fn filter_pools_drops_out_of_scope_before_rank() {
        let query = ContextQuery::new("")
            .pin(ExplicitPin::new(path("src/lib.rs")).range(0, 8))
            .diff_error(
                DiffErrorEvidence::new(path("docs/readme.md"), DiffErrorKind::Diff).range(0, 8),
            );
        let filtered = filter_pools(
            pools(query),
            &RetrievalFilter::new().scope(ScopeSet::new(vec!["src".into()]).expect("scope")),
        )
        .expect("filter");
        assert_eq!(filtered.explicit().len(), 1);
        assert!(filtered.diff_error().is_empty());
    }

    #[test]
    fn cancelled_filter_fails_closed() {
        let query = ContextQuery::new("").pin(ExplicitPin::new(path("src/lib.rs")).range(0, 8));
        let ranked = rank(&pools(query), &RankWeights::new(), 1.0, 4).expect("rank");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            filter_ranked(ranked, &RetrievalFilter::new().cancellation(cancel)),
            Err(FilterError::Cancelled)
        );
    }

    #[test]
    fn timeout_fails_closed() {
        let query = ContextQuery::new("").pin(ExplicitPin::new(path("src/lib.rs")).range(0, 8));
        let ranked = rank(&pools(query), &RankWeights::new(), 1.0, 4).expect("rank");
        assert_eq!(
            filter_ranked(ranked, &RetrievalFilter::new().timeout(Duration::ZERO),),
            Err(FilterError::Timeout)
        );
    }

    #[test]
    fn error_display_is_safe() {
        assert_eq!(FilterError::Cancelled.to_string(), "cancelled");
        assert_eq!(FilterError::Timeout.to_string(), "timeout");
        assert!(!FilterError::Cancelled.to_string().contains('/'));
    }
}
