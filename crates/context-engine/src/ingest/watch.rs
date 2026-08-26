//! Debounced filesystem-watcher coalescer.
//!
//! Raw watcher events are merged by `(repo, path)` until a quiet debounce
//! window elapses (or `max_hold` bounds a continuous burst). Rename becomes a
//! remove of the source plus an upsert of the destination. Overflow never
//! drops work: pending path jobs collapse into a scoped content-hash rescan.
//!
//! The pending set is bounded. Crossing `max_pending_paths` or `max_events`
//! is backpressure, not a silent drop.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::{RepoId, RepoPath};

use crate::ingest::pipeline::{IndexOutcome, IndexPipeline, PipelineError};
use crate::ingest::walk::{FileCandidate, WalkError, WalkLimits, walk_repo};
use crate::repo_manifest::CancellationToken;

/// Quiet period after the last event before unique jobs are emitted.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(50);

/// Maximum time a burst may be held even if events keep arriving.
pub const DEFAULT_MAX_HOLD: Duration = Duration::from_secs(1);

/// Unique `(repo, path)` entries retained before collapse to a scoped rescan.
pub const DEFAULT_MAX_PENDING_PATHS: usize = 4096;

/// Raw events accepted in one burst before collapse to a scoped rescan.
pub const DEFAULT_MAX_EVENTS: u32 = 8192;

/// Wall-clock budget for applying a flushed job batch.
pub const DEFAULT_WATCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Resource bounds for one coalescer. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct WatchLimits {
    debounce: Duration,
    max_hold: Duration,
    max_pending_paths: usize,
    max_events: u32,
    timeout: Duration,
    walk: WalkLimits,
    cancel: CancellationToken,
}

/// One notify-style filesystem event after repo-path normalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WatchEvent {
    Created {
        repo_id: RepoId,
        path: RepoPath,
    },
    Modified {
        repo_id: RepoId,
        path: RepoPath,
    },
    Deleted {
        repo_id: RepoId,
        path: RepoPath,
    },
    Renamed {
        repo_id: RepoId,
        from: RepoPath,
        to: RepoPath,
    },
    Overflow {
        repo_id: RepoId,
        scope: WatchScope,
    },
}

/// How far a reconciliation scan must walk.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum WatchScope {
    Repo,
    Prefix(RepoPath),
}

/// Last-writer action for one unique repo path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PathAction {
    Upsert,
    Remove,
}

/// Bounded indexing work produced after debounce.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexJob {
    Upsert { repo_id: RepoId, path: RepoPath },
    Remove { repo_id: RepoId, path: RepoPath },
    Rescan { repo_id: RepoId, scope: WatchScope },
}

/// Typed coalescer/apply failure. Display never echoes host or repository paths.
#[derive(Debug)]
pub enum WatchError {
    Cancelled,
    Timeout,
    UnknownRepo,
    Walk(WalkError),
    Pipeline(PipelineError),
}

/// Merges watcher bursts into unique path invalidations or scoped rescans.
pub struct WatchCoalescer {
    limits: WatchLimits,
    pending: BTreeMap<(RepoId, RepoPath), PathAction>,
    rescans: BTreeMap<RepoId, WatchScope>,
    first_event_at: Option<Instant>,
    last_event_at: Option<Instant>,
    event_count: u32,
}

impl WatchLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn debounce(mut self, value: Duration) -> Self {
        self.debounce = value;
        self
    }

    pub fn max_hold(mut self, value: Duration) -> Self {
        self.max_hold = value;
        self
    }

    pub fn max_pending_paths(mut self, value: usize) -> Self {
        self.max_pending_paths = value;
        self
    }

    pub fn max_events(mut self, value: u32) -> Self {
        self.max_events = value;
        self
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn walk(mut self, value: WalkLimits) -> Self {
        self.walk = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }

    pub fn debounce_value(&self) -> Duration {
        self.debounce
    }

    pub fn max_hold_value(&self) -> Duration {
        self.max_hold
    }

    pub fn max_pending_paths_value(&self) -> usize {
        self.max_pending_paths
    }

    pub fn max_events_value(&self) -> u32 {
        self.max_events
    }

    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }

    pub fn walk_limits(&self) -> &WalkLimits {
        &self.walk
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl Default for WatchLimits {
    fn default() -> Self {
        Self {
            debounce: DEFAULT_DEBOUNCE,
            max_hold: DEFAULT_MAX_HOLD,
            max_pending_paths: DEFAULT_MAX_PENDING_PATHS,
            max_events: DEFAULT_MAX_EVENTS,
            timeout: DEFAULT_WATCH_TIMEOUT,
            walk: WalkLimits::default(),
            cancel: CancellationToken::new(),
        }
    }
}

impl WatchEvent {
    pub fn created(repo_id: RepoId, path: RepoPath) -> Self {
        Self::Created { repo_id, path }
    }

    pub fn modified(repo_id: RepoId, path: RepoPath) -> Self {
        Self::Modified { repo_id, path }
    }

    pub fn deleted(repo_id: RepoId, path: RepoPath) -> Self {
        Self::Deleted { repo_id, path }
    }

    pub fn renamed(repo_id: RepoId, from: RepoPath, to: RepoPath) -> Self {
        Self::Renamed { repo_id, from, to }
    }

    pub fn overflow(repo_id: RepoId, scope: WatchScope) -> Self {
        Self::Overflow { repo_id, scope }
    }

    pub fn repo_id(&self) -> RepoId {
        match self {
            Self::Created { repo_id, .. }
            | Self::Modified { repo_id, .. }
            | Self::Deleted { repo_id, .. }
            | Self::Renamed { repo_id, .. }
            | Self::Overflow { repo_id, .. } => *repo_id,
        }
    }
}

impl WatchScope {
    pub fn repo() -> Self {
        Self::Repo
    }

    pub fn prefix(path: RepoPath) -> Self {
        Self::Prefix(path)
    }

    pub fn contains(&self, path: &RepoPath) -> bool {
        match self {
            Self::Repo => true,
            Self::Prefix(prefix) => path_under(path, prefix),
        }
    }
}

impl IndexJob {
    pub fn repo_id(&self) -> RepoId {
        match self {
            Self::Upsert { repo_id, .. }
            | Self::Remove { repo_id, .. }
            | Self::Rescan { repo_id, .. } => *repo_id,
        }
    }

    pub fn path(&self) -> Option<&RepoPath> {
        match self {
            Self::Upsert { path, .. } | Self::Remove { path, .. } => Some(path),
            Self::Rescan { .. } => None,
        }
    }

    pub fn scope(&self) -> Option<&WatchScope> {
        match self {
            Self::Rescan { scope, .. } => Some(scope),
            Self::Upsert { .. } | Self::Remove { .. } => None,
        }
    }

    pub fn action(&self) -> Option<PathAction> {
        match self {
            Self::Upsert { .. } => Some(PathAction::Upsert),
            Self::Remove { .. } => Some(PathAction::Remove),
            Self::Rescan { .. } => None,
        }
    }

    pub fn is_rescan(&self) -> bool {
        matches!(self, Self::Rescan { .. })
    }
}

impl WatchCoalescer {
    pub fn new(limits: WatchLimits) -> Self {
        Self {
            limits,
            pending: BTreeMap::new(),
            rescans: BTreeMap::new(),
            first_event_at: None,
            last_event_at: None,
            event_count: 0,
        }
    }

    pub fn limits(&self) -> &WatchLimits {
        &self.limits
    }

    /// True when no unflushed path jobs or rescans remain.
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.rescans.is_empty()
    }

    /// Ingest one watcher event. Overflow and cap breaches schedule a scoped
    /// rescan instead of dropping the event.
    pub fn push(&mut self, event: WatchEvent, now: Instant) -> Result<(), WatchError> {
        self.check_bounds()?;
        if self.first_event_at.is_none() {
            self.first_event_at = Some(now);
        }
        self.last_event_at = Some(now);
        self.event_count = self.event_count.saturating_add(1);

        let repo_id = event.repo_id();
        match event {
            WatchEvent::Created { repo_id, path } | WatchEvent::Modified { repo_id, path } => {
                self.record_path(repo_id, path, PathAction::Upsert);
            }
            WatchEvent::Deleted { repo_id, path } => {
                self.record_path(repo_id, path, PathAction::Remove);
            }
            WatchEvent::Renamed { repo_id, from, to } => {
                if from == to {
                    self.record_path(repo_id, from, PathAction::Upsert);
                } else {
                    self.record_path(repo_id, from, PathAction::Remove);
                    self.record_path(repo_id, to, PathAction::Upsert);
                }
            }
            WatchEvent::Overflow { repo_id, scope } => {
                self.schedule_rescan(repo_id, scope);
            }
        }

        if self.event_count > self.limits.max_events {
            let scope = self.repo_overflow_scope(repo_id);
            self.schedule_rescan(repo_id, scope);
        }
        self.enforce_pending_cap(repo_id);
        Ok(())
    }

    /// Emit unique jobs when the debounce window or max-hold bound has elapsed.
    pub fn poll(&mut self, now: Instant) -> Result<Vec<IndexJob>, WatchError> {
        self.check_bounds()?;
        if self.is_idle() {
            return Ok(Vec::new());
        }
        let last = match self.last_event_at {
            Some(last) => last,
            None => return Ok(Vec::new()),
        };
        let first = self.first_event_at.unwrap_or(last);
        let quiet = now.saturating_duration_since(last);
        let held = now.saturating_duration_since(first);
        if quiet >= self.limits.debounce || held >= self.limits.max_hold {
            Ok(self.take_jobs())
        } else {
            Ok(Vec::new())
        }
    }

    /// Emit unique jobs immediately. Used on shutdown and by apply helpers.
    pub fn drain(&mut self) -> Result<Vec<IndexJob>, WatchError> {
        self.check_bounds()?;
        Ok(self.take_jobs())
    }

    fn check_bounds(&self) -> Result<(), WatchError> {
        if self.limits.cancel.is_cancelled() {
            return Err(WatchError::Cancelled);
        }
        if self.limits.timeout.is_zero() {
            return Err(WatchError::Timeout);
        }
        Ok(())
    }

    fn record_path(&mut self, repo_id: RepoId, path: RepoPath, action: PathAction) {
        if self.covered_by_rescan(repo_id, &path) {
            return;
        }
        self.pending.insert((repo_id, path), action);
    }

    fn covered_by_rescan(&self, repo_id: RepoId, path: &RepoPath) -> bool {
        self.rescans
            .get(&repo_id)
            .is_some_and(|scope| scope.contains(path))
    }

    fn schedule_rescan(&mut self, repo_id: RepoId, incoming: WatchScope) {
        let merged = match self.rescans.get(&repo_id) {
            Some(existing) => merge_scopes(existing, &incoming),
            None => incoming,
        };
        self.pending
            .retain(|(pending_repo, path), _| *pending_repo != repo_id || !merged.contains(path));
        self.rescans.insert(repo_id, merged);
    }

    fn enforce_pending_cap(&mut self, repo_id: RepoId) {
        if self.covered_path_count(repo_id) <= self.limits.max_pending_paths {
            return;
        }
        let scope = self.repo_overflow_scope(repo_id);
        self.schedule_rescan(repo_id, scope);
    }

    fn covered_path_count(&self, repo_id: RepoId) -> usize {
        self.pending
            .keys()
            .filter(|(pending_repo, _)| *pending_repo == repo_id)
            .count()
    }

    fn repo_overflow_scope(&self, repo_id: RepoId) -> WatchScope {
        if let Some(existing) = self.rescans.get(&repo_id) {
            return existing.clone();
        }
        common_dir_prefix(
            self.pending
                .keys()
                .filter(|(pending_repo, _)| *pending_repo == repo_id)
                .map(|(_, path)| path),
        )
    }

    fn take_jobs(&mut self) -> Vec<IndexJob> {
        let pending = std::mem::take(&mut self.pending);
        let rescans = std::mem::take(&mut self.rescans);
        self.first_event_at = None;
        self.last_event_at = None;
        self.event_count = 0;

        let mut jobs = Vec::with_capacity(rescans.len().saturating_add(pending.len()));
        for (repo_id, scope) in rescans {
            jobs.push(IndexJob::Rescan { repo_id, scope });
        }
        for ((repo_id, path), action) in pending {
            jobs.push(match action {
                PathAction::Upsert => IndexJob::Upsert { repo_id, path },
                PathAction::Remove => IndexJob::Remove { repo_id, path },
            });
        }
        jobs
    }
}

/// Apply coalesced jobs to the incremental index.
///
/// Missing upsert targets become removals so rename/delete races still
/// converge. A rescan walks the declared scope and reindexes by content hash.
pub fn apply_index_jobs(
    pipeline: &mut IndexPipeline,
    jobs: &[IndexJob],
    limits: &WatchLimits,
) -> Result<Vec<IndexOutcome>, WatchError> {
    let started = Instant::now();
    let mut outcomes = Vec::with_capacity(jobs.len());
    for job in jobs {
        check_apply_bounds(limits, started)?;
        match job {
            IndexJob::Remove { repo_id, path } => {
                outcomes.push(pipeline.remove_file(*repo_id, path)?);
            }
            IndexJob::Upsert { repo_id, path } => {
                outcomes.push(apply_upsert(pipeline, *repo_id, path, limits, started)?);
            }
            IndexJob::Rescan { repo_id, scope } => {
                outcomes.extend(apply_rescan(pipeline, *repo_id, scope, limits, started)?);
            }
        }
    }
    Ok(outcomes)
}

fn apply_upsert(
    pipeline: &mut IndexPipeline,
    repo_id: RepoId,
    path: &RepoPath,
    limits: &WatchLimits,
    started: Instant,
) -> Result<IndexOutcome, WatchError> {
    match locate_candidate(pipeline, repo_id, path, limits, started)? {
        Some(candidate) => Ok(pipeline.index_file(&candidate)?),
        None => Ok(pipeline.remove_file(repo_id, path)?),
    }
}

fn apply_rescan(
    pipeline: &mut IndexPipeline,
    repo_id: RepoId,
    scope: &WatchScope,
    limits: &WatchLimits,
    started: Instant,
) -> Result<Vec<IndexOutcome>, WatchError> {
    let candidates = collect_scope_candidates(pipeline, repo_id, scope, limits, started)?;
    let mut outcomes = Vec::with_capacity(candidates.len());
    for candidate in &candidates {
        check_apply_bounds(limits, started)?;
        outcomes.push(pipeline.index_file(candidate)?);
    }
    Ok(outcomes)
}

fn collect_scope_candidates(
    pipeline: &IndexPipeline,
    repo_id: RepoId,
    scope: &WatchScope,
    limits: &WatchLimits,
    started: Instant,
) -> Result<Vec<FileCandidate>, WatchError> {
    let spec = pipeline
        .manifest()
        .repo_by_id(repo_id)
        .ok_or(WatchError::UnknownRepo)?;
    let mut candidates = Vec::new();
    for item in walk_repo(spec, &limits.walk, &limits.cancel) {
        check_apply_bounds(limits, started)?;
        let candidate = item?;
        if scope.contains(candidate.path()) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn locate_candidate(
    pipeline: &IndexPipeline,
    repo_id: RepoId,
    path: &RepoPath,
    limits: &WatchLimits,
    started: Instant,
) -> Result<Option<FileCandidate>, WatchError> {
    let spec = pipeline
        .manifest()
        .repo_by_id(repo_id)
        .ok_or(WatchError::UnknownRepo)?;
    for item in walk_repo(spec, &limits.walk, &limits.cancel) {
        check_apply_bounds(limits, started)?;
        let candidate = item?;
        if candidate.path() == path {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn check_apply_bounds(limits: &WatchLimits, started: Instant) -> Result<(), WatchError> {
    if limits.cancel.is_cancelled() {
        return Err(WatchError::Cancelled);
    }
    if limits.timeout.is_zero() || started.elapsed() > limits.timeout {
        return Err(WatchError::Timeout);
    }
    Ok(())
}

fn path_under(path: &RepoPath, prefix: &RepoPath) -> bool {
    let path = path.as_str();
    let prefix = prefix.as_str();
    path == prefix || (path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/'))
}

fn merge_scopes(left: &WatchScope, right: &WatchScope) -> WatchScope {
    match (left, right) {
        (WatchScope::Repo, _) | (_, WatchScope::Repo) => WatchScope::Repo,
        (WatchScope::Prefix(a), WatchScope::Prefix(b)) => {
            if path_under(b, a) {
                WatchScope::Prefix(a.clone())
            } else if path_under(a, b) {
                WatchScope::Prefix(b.clone())
            } else {
                WatchScope::Repo
            }
        }
    }
}

fn common_dir_prefix<'a>(paths: impl IntoIterator<Item = &'a RepoPath>) -> WatchScope {
    let mut iter = paths.into_iter();
    let Some(first) = iter.next() else {
        return WatchScope::Repo;
    };
    let mut prefix = dir_components(first);
    for path in iter {
        let comps = dir_components(path);
        let shared = prefix
            .iter()
            .zip(comps.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(shared);
        if prefix.is_empty() {
            return WatchScope::Repo;
        }
    }
    if prefix.is_empty() {
        return WatchScope::Repo;
    }
    match RepoPath::parse(&prefix.join("/")) {
        Ok(path) => WatchScope::Prefix(path),
        Err(_) => WatchScope::Repo,
    }
}

fn dir_components(path: &RepoPath) -> Vec<&str> {
    let mut comps: Vec<&str> = path.components().collect();
    let _ = comps.pop();
    comps
}

impl WatchError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::UnknownRepo => "unknown_repo",
            Self::Walk(_) => "walk",
            Self::Pipeline(_) => "pipeline",
        }
    }
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Walk(err) => write!(f, "{err}"),
            Self::Pipeline(err) => write!(f, "{err}"),
            other => f.write_str(other.as_str()),
        }
    }
}

impl Error for WatchError {}

impl From<WalkError> for WatchError {
    fn from(value: WalkError) -> Self {
        match value {
            WalkError::Cancelled => Self::Cancelled,
            WalkError::UnknownRepo => Self::UnknownRepo,
            other => Self::Walk(other),
        }
    }
}

impl From<PipelineError> for WatchError {
    fn from(value: PipelineError) -> Self {
        match value {
            PipelineError::Cancelled => Self::Cancelled,
            PipelineError::Timeout => Self::Timeout,
            PipelineError::UnknownRepo => Self::UnknownRepo,
            PipelineError::Walk(err) => Self::from(err),
            other => Self::Pipeline(other),
        }
    }
}

impl fmt::Debug for WatchCoalescer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatchCoalescer")
            .field("pending", &self.pending.len())
            .field("rescans", &self.rescans.len())
            .field("event_count", &self.event_count)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::fts::FtsQuery;
    use crate::ingest::pipeline::PipelineLimits;
    use crate::ingest::walk::walk_repo;
    use crate::repo_manifest::WorkspaceManifest;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
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
                "rapidlm-context-watch-{}-{nanos}-{seq}",
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

    fn parse_manifest(ws: &TempWorkspace) -> WorkspaceManifest {
        ws.write_file("core/.keep", b"");
        let src =
            "schema = 1\n[[repos]]\nalias = \"core\"\nroot = \"core\"\nmode = \"read_write\"\n";
        WorkspaceManifest::parse(src, &ws.path, &CancellationToken::new()).expect("manifest")
    }

    fn repo_id(manifest: &WorkspaceManifest) -> RepoId {
        manifest.repo_by_alias("core").expect("repo").id()
    }

    fn path(rel: &str) -> RepoPath {
        RepoPath::parse(rel).expect("repo path")
    }

    fn instant() -> Instant {
        Instant::now()
    }

    fn coalescer(limits: WatchLimits) -> WatchCoalescer {
        WatchCoalescer::new(limits)
    }

    fn open_mem(manifest: WorkspaceManifest) -> IndexPipeline {
        IndexPipeline::open_in_memory(manifest, PipelineLimits::new()).expect("open memory")
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

    fn job_keys(jobs: &[IndexJob]) -> Vec<(String, &'static str, String)> {
        let mut out: Vec<(String, &'static str, String)> = jobs
            .iter()
            .map(|job| {
                let repo = job.repo_id().to_string();
                match job {
                    IndexJob::Upsert { path, .. } => (repo, "upsert", path.as_str().to_owned()),
                    IndexJob::Remove { path, .. } => (repo, "remove", path.as_str().to_owned()),
                    IndexJob::Rescan { scope, .. } => {
                        let label = match scope {
                            WatchScope::Repo => "repo".to_owned(),
                            WatchScope::Prefix(prefix) => format!("prefix:{}", prefix.as_str()),
                        };
                        (repo, "rescan", label)
                    }
                }
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn debounce_emits_unique_repo_path_invalidations() {
        let ws = TempWorkspace::new();
        let manifest = parse_manifest(&ws);
        let repo = repo_id(&manifest);
        let mut watch = coalescer(WatchLimits::new().debounce(Duration::from_millis(10)));
        let t0 = instant();
        watch
            .push(WatchEvent::modified(repo, path("src/a.rs")), t0)
            .expect("first");
        watch
            .push(
                WatchEvent::modified(repo, path("src/a.rs")),
                t0 + Duration::from_millis(3),
            )
            .expect("dup");
        watch
            .push(
                WatchEvent::created(repo, path("src/b.rs")),
                t0 + Duration::from_millis(4),
            )
            .expect("second");
        assert!(
            watch
                .poll(t0 + Duration::from_millis(13))
                .expect("still quiet")
                .is_empty()
        );
        let jobs = watch.poll(t0 + Duration::from_millis(15)).expect("flushed");
        assert_eq!(
            job_keys(&jobs),
            vec![
                (repo.to_string(), "upsert", "src/a.rs".to_owned()),
                (repo.to_string(), "upsert", "src/b.rs".to_owned()),
            ]
        );
        assert!(watch.is_idle());
    }

    #[test]
    fn watcher_content_change_bumps_generation_from_new_hash() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"fn watch_gen_v1() {}\n");
        let manifest = parse_manifest(&ws);
        let repo = repo_id(&manifest);
        let mut pipeline = open_mem(manifest.clone());
        let first = pipeline
            .index_file(&candidate_named(&manifest, "src/lib.rs"))
            .expect("index v1");
        assert_eq!(first.generation(), 1);
        let v1_hash = first.content_hash();

        ws.write_file("core/src/lib.rs", b"fn watch_gen_v2() {}\n");
        let mut watch = coalescer(WatchLimits::new().debounce(Duration::ZERO));
        let t0 = instant();
        watch
            .push(WatchEvent::modified(repo, path("src/lib.rs")), t0)
            .expect("modified");
        let jobs = watch.poll(t0).expect("flush");
        assert_eq!(
            job_keys(&jobs),
            vec![(repo.to_string(), "upsert", "src/lib.rs".to_owned())]
        );
        let outcomes = apply_index_jobs(&mut pipeline, &jobs, watch.limits()).expect("apply");
        assert_eq!(outcomes.len(), 1);
        assert!(!outcomes[0].is_unchanged());
        assert_eq!(outcomes[0].generation(), 2);
        assert_ne!(outcomes[0].content_hash(), v1_hash);

        let canonical = pipeline
            .canonical_state(repo, &path("src/lib.rs"))
            .expect("canonical")
            .expect("present");
        assert_eq!(canonical.generation(), 2);
        assert_eq!(
            canonical.content_hash(),
            outcomes[0].content_hash().to_string()
        );

        assert!(
            pipeline
                .fts()
                .search(&FtsQuery::new("watch_gen_v1").repo(repo))
                .expect("stale")
                .is_empty()
        );
        assert_eq!(
            pipeline
                .fts()
                .search(&FtsQuery::new("watch_gen_v2").repo(repo))
                .expect("fresh")
                .len(),
            1
        );
    }

    #[test]
    fn rename_and_delete_update_index() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/old.rs", b"fn watch_rename_marker() {}\n");
        ws.write_file("core/src/keep.rs", b"fn watch_keep_marker() {}\n");
        let manifest = parse_manifest(&ws);
        let repo = repo_id(&manifest);
        let mut pipeline = open_mem(manifest.clone());
        pipeline
            .index_file(&candidate_named(&manifest, "src/old.rs"))
            .expect("index old");
        pipeline
            .index_file(&candidate_named(&manifest, "src/keep.rs"))
            .expect("index keep");

        fs::rename(
            ws.path.join("core/src/old.rs"),
            ws.path.join("core/src/new.rs"),
        )
        .expect("rename");
        fs::remove_file(ws.path.join("core/src/keep.rs")).expect("delete");

        let mut watch = coalescer(WatchLimits::new().debounce(Duration::ZERO));
        let t0 = instant();
        watch
            .push(
                WatchEvent::renamed(repo, path("src/old.rs"), path("src/new.rs")),
                t0,
            )
            .expect("rename event");
        watch
            .push(WatchEvent::deleted(repo, path("src/keep.rs")), t0)
            .expect("delete event");
        let jobs = watch.poll(t0).expect("flush");
        assert_eq!(
            job_keys(&jobs),
            vec![
                (repo.to_string(), "remove", "src/keep.rs".to_owned()),
                (repo.to_string(), "remove", "src/old.rs".to_owned()),
                (repo.to_string(), "upsert", "src/new.rs".to_owned()),
            ]
        );
        apply_index_jobs(&mut pipeline, &jobs, watch.limits()).expect("apply");

        let renamed = pipeline
            .fts()
            .search(&FtsQuery::new("watch_rename_marker").repo(repo))
            .expect("renamed search");
        assert_eq!(renamed.len(), 1);
        assert_eq!(renamed[0].path().as_str(), "src/new.rs");
        assert!(
            pipeline
                .canonical_state(repo, &path("src/old.rs"))
                .expect("old state")
                .is_none()
        );
        assert!(
            pipeline
                .canonical_state(repo, &path("src/new.rs"))
                .expect("new state")
                .is_some()
        );

        let deleted = pipeline
            .fts()
            .search(&FtsQuery::new("watch_keep_marker").repo(repo))
            .expect("deleted search");
        assert!(deleted.is_empty());
        assert!(
            pipeline
                .canonical_state(repo, &path("src/keep.rs"))
                .expect("keep state")
                .is_none()
        );
    }

    #[test]
    fn overflow_schedules_scoped_rescan_instead_of_dropping() {
        let ws = TempWorkspace::new();
        let manifest = parse_manifest(&ws);
        let repo = repo_id(&manifest);
        let mut watch = coalescer(
            WatchLimits::new()
                .debounce(Duration::ZERO)
                .max_pending_paths(2)
                .max_events(100),
        );
        let t0 = instant();
        watch
            .push(WatchEvent::modified(repo, path("src/a.rs")), t0)
            .expect("a");
        watch
            .push(WatchEvent::modified(repo, path("src/b.rs")), t0)
            .expect("b");
        watch
            .push(WatchEvent::modified(repo, path("src/c.rs")), t0)
            .expect("c");
        let jobs = watch.drain().expect("drain");
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].is_rescan());
        assert_eq!(jobs[0].repo_id(), repo);
        assert_eq!(jobs[0].scope(), Some(&WatchScope::Prefix(path("src"))));
        assert!(watch.is_idle());
    }

    #[test]
    fn overflow_event_absorbs_in_scope_paths_and_keeps_out_of_scope() {
        let ws = TempWorkspace::new();
        let manifest = parse_manifest(&ws);
        let repo = repo_id(&manifest);
        let mut watch = coalescer(WatchLimits::new().debounce(Duration::ZERO));
        let t0 = instant();
        watch
            .push(WatchEvent::modified(repo, path("src/a.rs")), t0)
            .expect("src");
        watch
            .push(WatchEvent::modified(repo, path("docs/readme.md")), t0)
            .expect("docs");
        watch
            .push(
                WatchEvent::overflow(repo, WatchScope::Prefix(path("src"))),
                t0,
            )
            .expect("overflow");
        watch
            .push(WatchEvent::deleted(repo, path("src/gone.rs")), t0)
            .expect("absorbed");
        let jobs = watch.drain().expect("drain");
        assert_eq!(
            job_keys(&jobs),
            vec![
                (repo.to_string(), "rescan", "prefix:src".to_owned()),
                (repo.to_string(), "upsert", "docs/readme.md".to_owned()),
            ]
        );
    }

    #[test]
    fn event_storm_collapses_to_rescan_not_unbounded_jobs() {
        let ws = TempWorkspace::new();
        let manifest = parse_manifest(&ws);
        let repo = repo_id(&manifest);
        let mut watch = coalescer(
            WatchLimits::new()
                .debounce(Duration::ZERO)
                .max_events(8)
                .max_pending_paths(10_000),
        );
        let t0 = instant();
        for i in 0..32 {
            let rel = format!("src/f{i}.rs");
            watch
                .push(WatchEvent::modified(repo, path(&rel)), t0)
                .expect("storm");
        }
        let jobs = watch.drain().expect("drain");
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].is_rescan());
        assert_eq!(jobs[0].scope(), Some(&WatchScope::Prefix(path("src"))));
    }

    #[test]
    fn overflow_rescan_reindexes_by_content_hash() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"fn watch_before() {}\n");
        let manifest = parse_manifest(&ws);
        let repo = repo_id(&manifest);
        let mut pipeline = open_mem(manifest.clone());
        pipeline
            .index_file(&candidate_named(&manifest, "src/lib.rs"))
            .expect("index");
        ws.write_file("core/src/lib.rs", b"fn watch_after() {}\n");

        let mut watch = coalescer(WatchLimits::new().debounce(Duration::ZERO));
        let t0 = instant();
        watch
            .push(
                WatchEvent::overflow(repo, WatchScope::Prefix(path("src"))),
                t0,
            )
            .expect("overflow");
        let jobs = watch.poll(t0).expect("flush");
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].is_rescan());
        apply_index_jobs(&mut pipeline, &jobs, watch.limits()).expect("apply rescan");

        assert!(
            pipeline
                .fts()
                .search(&FtsQuery::new("watch_before").repo(repo))
                .expect("stale")
                .is_empty()
        );
        assert_eq!(
            pipeline
                .fts()
                .search(&FtsQuery::new("watch_after").repo(repo))
                .expect("fresh")
                .len(),
            1
        );
    }

    #[test]
    fn max_hold_flushes_during_continuous_burst() {
        let ws = TempWorkspace::new();
        let manifest = parse_manifest(&ws);
        let repo = repo_id(&manifest);
        let mut watch = coalescer(
            WatchLimits::new()
                .debounce(Duration::from_millis(50))
                .max_hold(Duration::from_millis(20)),
        );
        let t0 = instant();
        watch
            .push(WatchEvent::modified(repo, path("src/a.rs")), t0)
            .expect("t0");
        watch
            .push(
                WatchEvent::modified(repo, path("src/b.rs")),
                t0 + Duration::from_millis(15),
            )
            .expect("t15");
        let jobs = watch
            .poll(t0 + Duration::from_millis(20))
            .expect("held long enough");
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn cancelled_push_fails_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut watch = coalescer(WatchLimits::new().cancellation(cancel));
        let err = watch
            .push(
                WatchEvent::modified(RepoId::new(), path("src/a.rs")),
                instant(),
            )
            .expect_err("cancelled");
        assert_eq!(err.as_str(), "cancelled");
    }

    #[test]
    fn zero_timeout_fails_closed() {
        let mut watch = coalescer(WatchLimits::new().timeout(Duration::ZERO));
        let err = watch
            .push(
                WatchEvent::modified(RepoId::new(), path("src/a.rs")),
                instant(),
            )
            .expect_err("timeout");
        assert_eq!(err.as_str(), "timeout");
    }

    #[test]
    fn disjoint_prefix_overflows_upgrade_to_repo_scope() {
        let ws = TempWorkspace::new();
        let manifest = parse_manifest(&ws);
        let repo = repo_id(&manifest);
        let mut watch = coalescer(WatchLimits::new().debounce(Duration::ZERO));
        let t0 = instant();
        watch
            .push(
                WatchEvent::overflow(repo, WatchScope::Prefix(path("src"))),
                t0,
            )
            .expect("src");
        watch
            .push(
                WatchEvent::overflow(repo, WatchScope::Prefix(path("docs"))),
                t0,
            )
            .expect("docs");
        let jobs = watch.drain().expect("drain");
        assert_eq!(
            job_keys(&jobs),
            vec![(repo.to_string(), "rescan", "repo".to_owned())]
        );
    }
}
