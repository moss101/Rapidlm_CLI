//! Exact per-session/agent read tracking for context compilation.
//!
//! Each observation records the file, byte range, and content hash shown to
//! one actor. Repeated unchanged reads occupy one slot. Freshness is hash
//! comparison: an identical path with a new range or digest is stale.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::{AgentId, RepoId, RepoPath, SessionId};

use crate::index::graph::MAX_LOCATOR_BYTES;
use crate::ingest::content::ContentHash;
use crate::repo_manifest::CancellationToken;
use crate::retrieval::candidates::{Freshness, ReadSetItem};

/// Default wall-clock budget for one record or freshness call.
pub const DEFAULT_READ_SET_TIMEOUT: Duration = Duration::from_secs(1);

/// Default unique locator cap for one session/agent read-set.
pub const DEFAULT_MAX_READ_RECORDS: usize = 8_192;

const CANCEL_STRIDE: usize = 16;

/// Resource bounds for one [`ReadSet`]. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct ReadSetLimits {
    max_records: usize,
    timeout: Duration,
    cancel: CancellationToken,
}

/// File/range identity of a model-visible read. Path alone is not identity.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ContextLocator {
    repo_id: Option<RepoId>,
    path: RepoPath,
    start_byte: u32,
    end_byte: u32,
    chunk_id: Option<String>,
}

/// One file/range/content-hash read attributed to a session/agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadObservation {
    session_id: SessionId,
    agent_id: AgentId,
    locator: ContextLocator,
    content_hash: ContentHash,
    seq: u64,
}

/// Durable projection of a locator last shown to one actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadRecord {
    session_id: SessionId,
    agent_id: AgentId,
    locator: ContextLocator,
    content_hash: ContentHash,
    first_seen_seq: u64,
    last_seen_seq: u64,
    visibility_generation: u64,
}

/// Whether [`ReadSet::record`] inserted, reused, or replaced a locator slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RecordOutcome {
    Inserted,
    Deduplicated,
    Replaced,
}

/// Exact reads previously shown to one session/agent.
#[derive(Clone, Debug)]
pub struct ReadSet {
    session_id: SessionId,
    agent_id: AgentId,
    records: BTreeMap<ContextLocator, ReadRecord>,
    limits: ReadSetLimits,
    visibility_generation: u64,
}

/// Typed read-set failure. Display never echoes paths, hashes, or IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadSetError {
    Cancelled,
    Timeout,
    InvalidObservation,
    SessionMismatch,
    AgentMismatch,
    CapacityExceeded,
}

impl ReadSetLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_records(mut self, value: usize) -> Self {
        self.max_records = value;
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

    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl Default for ReadSetLimits {
    fn default() -> Self {
        Self {
            max_records: DEFAULT_MAX_READ_RECORDS,
            timeout: DEFAULT_READ_SET_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }
}

impl ContextLocator {
    pub fn new(path: RepoPath, start_byte: u32, end_byte: u32) -> Self {
        Self {
            repo_id: None,
            path,
            start_byte,
            end_byte,
            chunk_id: None,
        }
    }

    /// Whole-file locator. Distinct from any exact byte range on the same path.
    pub fn file(path: RepoPath) -> Self {
        Self::new(path, 0, u32::MAX)
    }

    pub fn repo(mut self, repo_id: RepoId) -> Self {
        self.repo_id = Some(repo_id);
        self
    }

    pub fn chunk_id(mut self, value: impl Into<String>) -> Self {
        self.chunk_id = Some(value.into());
        self
    }

    pub fn repo_id(&self) -> Option<RepoId> {
        self.repo_id
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn start_byte(&self) -> u32 {
        self.start_byte
    }

    pub fn end_byte(&self) -> u32 {
        self.end_byte
    }

    pub fn chunk_id_value(&self) -> Option<&str> {
        self.chunk_id.as_deref()
    }

    fn validate(&self) -> Result<(), ReadSetError> {
        if self.start_byte > self.end_byte {
            return Err(ReadSetError::InvalidObservation);
        }
        if let Some(chunk_id) = self.chunk_id.as_deref() {
            if chunk_id.is_empty() || chunk_id.len() > MAX_LOCATOR_BYTES || chunk_id.contains('\0')
            {
                return Err(ReadSetError::InvalidObservation);
            }
        }
        Ok(())
    }
}

impl ReadObservation {
    pub fn new(
        session_id: SessionId,
        agent_id: AgentId,
        locator: ContextLocator,
        content_hash: ContentHash,
        seq: u64,
    ) -> Self {
        Self {
            session_id,
            agent_id,
            locator,
            content_hash,
            seq,
        }
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    pub fn locator(&self) -> &ContextLocator {
        &self.locator
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }
}

impl ReadRecord {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    pub fn locator(&self) -> &ContextLocator {
        &self.locator
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn first_seen_seq(&self) -> u64 {
        self.first_seen_seq
    }

    pub fn last_seen_seq(&self) -> u64 {
        self.last_seen_seq
    }

    pub fn visibility_generation(&self) -> u64 {
        self.visibility_generation
    }

    pub fn to_item(&self, current_hash: ContentHash) -> ReadSetItem {
        let mut item = ReadSetItem::new(
            self.locator.path.clone(),
            self.locator.start_byte,
            self.locator.end_byte,
            self.content_hash,
            current_hash,
        );
        if let Some(repo_id) = self.locator.repo_id {
            item = item.repo(repo_id);
        }
        if let Some(chunk_id) = self.locator.chunk_id.as_deref() {
            item = item.chunk_id(chunk_id);
        }
        item
    }
}

impl RecordOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inserted => "inserted",
            Self::Deduplicated => "deduplicated",
            Self::Replaced => "replaced",
        }
    }
}

impl ReadSet {
    pub fn new(session_id: SessionId, agent_id: AgentId) -> Self {
        Self::with_limits(session_id, agent_id, ReadSetLimits::new())
    }

    pub fn with_limits(session_id: SessionId, agent_id: AgentId, limits: ReadSetLimits) -> Self {
        Self {
            session_id,
            agent_id,
            records: BTreeMap::new(),
            limits,
            visibility_generation: 0,
        }
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn records(&self) -> impl Iterator<Item = &ReadRecord> {
        self.records.values()
    }

    pub fn get(&self, locator: &ContextLocator) -> Option<&ReadRecord> {
        self.records.get(locator)
    }

    pub fn visibility_generation(&self) -> u64 {
        self.visibility_generation
    }

    /// Compaction/rebuild: prior visibility refs are no longer in the active packet.
    pub fn advance_generation(&mut self) {
        self.visibility_generation = self.visibility_generation.saturating_add(1);
    }

    /// True only when this exact locator+hash is reachable in the current generation.
    pub fn is_currently_visible(
        &self,
        locator: &ContextLocator,
        current_hash: ContentHash,
    ) -> Result<bool, ReadSetError> {
        let started = Instant::now();
        self.check_ready(started)?;
        locator.validate()?;
        Ok(self.records.get(locator).is_some_and(|record| {
            record.content_hash == current_hash
                && record.visibility_generation == self.visibility_generation
        }))
    }

    /// Drop locators for `path` after a write/invalidation event.
    pub fn invalidate_path(&mut self, path: &RepoPath) -> Result<u32, ReadSetError> {
        let started = Instant::now();
        self.check_ready(started)?;
        let before = self.records.len();
        self.records.retain(|locator, _| locator.path != *path);
        Ok((before.saturating_sub(self.records.len())) as u32)
    }

    /// Record a model-visible read. Unchanged repeats reuse the existing slot.
    pub fn record(&mut self, observation: ReadObservation) -> Result<RecordOutcome, ReadSetError> {
        let started = Instant::now();
        self.check_ready(started)?;
        observation.locator.validate()?;
        if observation.session_id != self.session_id {
            return Err(ReadSetError::SessionMismatch);
        }
        if observation.agent_id != self.agent_id {
            return Err(ReadSetError::AgentMismatch);
        }

        if let Some(existing) = self.records.get_mut(&observation.locator) {
            if existing.content_hash == observation.content_hash
                && existing.visibility_generation == self.visibility_generation
            {
                existing.first_seen_seq = existing.first_seen_seq.min(observation.seq);
                existing.last_seen_seq = existing.last_seen_seq.max(observation.seq);
                return Ok(RecordOutcome::Deduplicated);
            }
            existing.content_hash = observation.content_hash;
            existing.first_seen_seq = observation.seq;
            existing.last_seen_seq = observation.seq;
            existing.visibility_generation = self.visibility_generation;
            return Ok(RecordOutcome::Replaced);
        }

        if self.records.len() >= self.limits.max_records {
            return Err(ReadSetError::CapacityExceeded);
        }
        self.records.insert(
            observation.locator.clone(),
            ReadRecord {
                session_id: observation.session_id,
                agent_id: observation.agent_id,
                locator: observation.locator,
                content_hash: observation.content_hash,
                first_seen_seq: observation.seq,
                last_seen_seq: observation.seq,
                visibility_generation: self.visibility_generation,
            },
        );
        Ok(RecordOutcome::Inserted)
    }

    /// Compare `current_hash` to the last recorded digest for `locator`.
    pub fn freshness(
        &self,
        locator: &ContextLocator,
        current_hash: ContentHash,
    ) -> Result<Freshness, ReadSetError> {
        let started = Instant::now();
        self.check_ready(started)?;
        locator.validate()?;
        Ok(self.freshness_unchecked(locator, current_hash))
    }

    /// True when this locator was read and the current digest differs.
    pub fn changed_since_read(
        &self,
        locator: &ContextLocator,
        current_hash: ContentHash,
    ) -> Result<bool, ReadSetError> {
        Ok(self.freshness(locator, current_hash)? == Freshness::Stale)
    }

    /// Records whose current digest no longer matches the last shown hash.
    pub fn detect_changes(
        &self,
        current: &[(ContextLocator, ContentHash)],
    ) -> Result<Vec<ReadRecord>, ReadSetError> {
        let started = Instant::now();
        self.check_ready(started)?;
        let mut stale = Vec::new();
        for (step, (locator, hash)) in current.iter().enumerate() {
            self.check_stride(step, started)?;
            locator.validate()?;
            if self.freshness_unchecked(locator, *hash) == Freshness::Stale {
                if let Some(record) = self.records.get(locator) {
                    stale.push(record.clone());
                }
            }
        }
        Ok(stale)
    }

    fn freshness_unchecked(
        &self,
        locator: &ContextLocator,
        current_hash: ContentHash,
    ) -> Freshness {
        match self.records.get(locator) {
            Some(record) if record.content_hash == current_hash => Freshness::Fresh,
            Some(_) => Freshness::Stale,
            None => Freshness::Unknown,
        }
    }

    fn check_ready(&self, started: Instant) -> Result<(), ReadSetError> {
        if self.limits.cancel.is_cancelled() {
            return Err(ReadSetError::Cancelled);
        }
        if self.limits.timeout.is_zero() || started.elapsed() > self.limits.timeout {
            return Err(ReadSetError::Timeout);
        }
        Ok(())
    }

    fn check_stride(&self, step: usize, started: Instant) -> Result<(), ReadSetError> {
        if step == 0 || step.is_multiple_of(CANCEL_STRIDE) {
            self.check_ready(started)?;
        }
        Ok(())
    }
}

impl ReadSetError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::InvalidObservation => "invalid observation",
            Self::SessionMismatch => "session mismatch",
            Self::AgentMismatch => "agent mismatch",
            Self::CapacityExceeded => "read-set capacity exceeded",
        }
    }
}

impl fmt::Display for RecordOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ReadSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ReadSetError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(rel: &str) -> RepoPath {
        RepoPath::parse(rel).expect("repo path")
    }

    fn hash(text: &str) -> ContentHash {
        ContentHash::from_bytes(text.as_bytes())
    }

    fn locator(rel: &str, start: u32, end: u32) -> ContextLocator {
        ContextLocator::new(path(rel), start, end)
    }

    fn observation(
        session: SessionId,
        agent: AgentId,
        loc: ContextLocator,
        digest: ContentHash,
        seq: u64,
    ) -> ReadObservation {
        ReadObservation::new(session, agent, loc, digest, seq)
    }

    #[test]
    fn repeated_unchanged_reads_deduplicate() {
        let session = SessionId::new();
        let agent = AgentId::new();
        let loc = locator("src/lib.rs", 0, 16);
        let digest = hash("fn main() {}\n");
        let mut reads = ReadSet::new(session, agent);

        assert_eq!(
            reads
                .record(observation(session, agent, loc.clone(), digest, 3))
                .expect("insert"),
            RecordOutcome::Inserted
        );
        assert_eq!(
            reads
                .record(observation(session, agent, loc.clone(), digest, 9))
                .expect("dedup"),
            RecordOutcome::Deduplicated
        );

        assert_eq!(reads.len(), 1);
        let record = reads.get(&loc).expect("record");
        assert_eq!(record.content_hash(), digest);
        assert_eq!(record.first_seen_seq(), 3);
        assert_eq!(record.last_seen_seq(), 9);
        assert_eq!(
            reads.freshness(&loc, digest).expect("fresh"),
            Freshness::Fresh
        );
        assert!(!reads.changed_since_read(&loc, digest).expect("unchanged"));
    }

    #[test]
    fn changed_range_is_stale_when_path_is_identical() {
        let session = SessionId::new();
        let agent = AgentId::new();
        let loc = locator("src/lib.rs", 10, 40);
        let mut reads = ReadSet::new(session, agent);
        reads
            .record(observation(
                session,
                agent,
                loc.clone(),
                hash("old range"),
                1,
            ))
            .expect("record");

        let current = hash("new range");
        assert_eq!(
            reads.freshness(&loc, current).expect("stale"),
            Freshness::Stale
        );
        assert!(reads.changed_since_read(&loc, current).expect("changed"));
        assert_eq!(
            reads
                .detect_changes(&[(loc.clone(), current)])
                .expect("detect")
                .len(),
            1
        );
    }

    #[test]
    fn changed_file_is_stale_when_path_is_identical() {
        let session = SessionId::new();
        let agent = AgentId::new();
        let loc = ContextLocator::file(path("src/lib.rs"));
        let mut reads = ReadSet::new(session, agent);
        reads
            .record(observation(
                session,
                agent,
                loc.clone(),
                hash("old file"),
                1,
            ))
            .expect("record");

        assert_eq!(
            reads.freshness(&loc, hash("new file")).expect("file stale"),
            Freshness::Stale
        );
    }

    #[test]
    fn distinct_ranges_on_the_same_path_are_independent() {
        let session = SessionId::new();
        let agent = AgentId::new();
        let first = locator("src/lib.rs", 0, 8);
        let second = locator("src/lib.rs", 8, 16);
        let mut reads = ReadSet::new(session, agent);
        reads
            .record(observation(session, agent, first.clone(), hash("head"), 1))
            .expect("first");
        reads
            .record(observation(session, agent, second.clone(), hash("tail"), 2))
            .expect("second");

        assert_eq!(reads.len(), 2);
        assert_eq!(
            reads.freshness(&first, hash("head")).expect("first fresh"),
            Freshness::Fresh
        );
        assert_eq!(
            reads
                .freshness(&second, hash("changed tail"))
                .expect("second stale"),
            Freshness::Stale
        );
        assert_eq!(
            reads
                .freshness(&locator("src/lib.rs", 16, 24), hash("unseen"))
                .expect("unknown range"),
            Freshness::Unknown
        );
    }

    #[test]
    fn hash_change_replaces_recorded_digest() {
        let session = SessionId::new();
        let agent = AgentId::new();
        let loc = locator("src/a.rs", 0, 4);
        let mut reads = ReadSet::new(session, agent);
        reads
            .record(observation(session, agent, loc.clone(), hash("v1"), 4))
            .expect("v1");
        assert_eq!(
            reads
                .record(observation(session, agent, loc.clone(), hash("v2"), 8))
                .expect("v2"),
            RecordOutcome::Replaced
        );

        assert_eq!(reads.len(), 1);
        let record = reads.get(&loc).expect("record");
        assert_eq!(record.content_hash(), hash("v2"));
        assert_eq!(record.first_seen_seq(), 8);
        assert_eq!(record.last_seen_seq(), 8);
        assert_eq!(
            reads.freshness(&loc, hash("v2")).expect("now fresh"),
            Freshness::Fresh
        );
        assert_eq!(
            reads.freshness(&loc, hash("v1")).expect("old stale"),
            Freshness::Stale
        );
    }

    #[test]
    fn foreign_session_or_agent_is_rejected() {
        let session = SessionId::new();
        let agent = AgentId::new();
        let mut reads = ReadSet::new(session, agent);
        let loc = locator("src/lib.rs", 0, 1);

        assert_eq!(
            reads.record(observation(
                SessionId::new(),
                agent,
                loc.clone(),
                hash("x"),
                1
            )),
            Err(ReadSetError::SessionMismatch)
        );
        assert_eq!(
            reads.record(observation(session, AgentId::new(), loc, hash("x"), 1)),
            Err(ReadSetError::AgentMismatch)
        );
        assert!(reads.is_empty());
    }

    #[test]
    fn inverted_range_is_rejected() {
        let session = SessionId::new();
        let agent = AgentId::new();
        let mut reads = ReadSet::new(session, agent);
        let loc = locator("src/lib.rs", 8, 2);
        assert_eq!(
            reads.record(observation(session, agent, loc.clone(), hash("x"), 1)),
            Err(ReadSetError::InvalidObservation)
        );
        assert_eq!(
            reads.freshness(&loc, hash("x")),
            Err(ReadSetError::InvalidObservation)
        );
    }

    #[test]
    fn capacity_rejects_new_locator_but_allows_dedup() {
        let session = SessionId::new();
        let agent = AgentId::new();
        let limits = ReadSetLimits::new().max_records(1);
        let mut reads = ReadSet::with_limits(session, agent, limits);
        let first = locator("src/a.rs", 0, 1);
        reads
            .record(observation(session, agent, first.clone(), hash("a"), 1))
            .expect("first");
        assert_eq!(
            reads.record(observation(
                session,
                agent,
                locator("src/b.rs", 0, 1),
                hash("b"),
                2
            )),
            Err(ReadSetError::CapacityExceeded)
        );
        assert_eq!(
            reads
                .record(observation(session, agent, first, hash("a"), 3))
                .expect("dedup under cap"),
            RecordOutcome::Deduplicated
        );
        assert_eq!(reads.len(), 1);
    }

    #[test]
    fn cancelled_and_zero_timeout_fail_closed() {
        let session = SessionId::new();
        let agent = AgentId::new();
        let loc = locator("src/lib.rs", 0, 1);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut cancelled =
            ReadSet::with_limits(session, agent, ReadSetLimits::new().cancellation(cancel));
        assert_eq!(
            cancelled.record(observation(session, agent, loc.clone(), hash("x"), 1)),
            Err(ReadSetError::Cancelled)
        );

        let timed_out =
            ReadSet::with_limits(session, agent, ReadSetLimits::new().timeout(Duration::ZERO));
        assert_eq!(
            timed_out.freshness(&loc, hash("x")),
            Err(ReadSetError::Timeout)
        );
    }

    #[test]
    fn item_snapshot_marks_changed_hash() {
        let session = SessionId::new();
        let agent = AgentId::new();
        let repo = RepoId::new();
        let loc = locator("src/lib.rs", 0, 4).repo(repo).chunk_id("chunk-1");
        let mut reads = ReadSet::new(session, agent);
        reads
            .record(observation(session, agent, loc.clone(), hash("old"), 1))
            .expect("record");
        let item = reads.get(&loc).expect("record").to_item(hash("new"));
        assert!(item.is_changed());
        assert_eq!(item.path().as_str(), "src/lib.rs");
    }

    #[test]
    fn advance_generation_makes_prior_read_not_currently_visible() {
        let session = SessionId::new();
        let agent = AgentId::new();
        let loc = locator("src/lib.rs", 0, 16);
        let digest = hash("fn main() {}\n");
        let mut reads = ReadSet::new(session, agent);
        reads
            .record(observation(session, agent, loc.clone(), digest, 1))
            .expect("record");
        assert!(reads.is_currently_visible(&loc, digest).expect("visible"));
        reads.advance_generation();
        assert!(!reads.is_currently_visible(&loc, digest).expect("hidden"));
        assert_eq!(
            reads
                .record(observation(session, agent, loc.clone(), digest, 2))
                .expect("resend"),
            RecordOutcome::Replaced
        );
        assert!(
            reads
                .is_currently_visible(&loc, digest)
                .expect("visible again")
        );
    }

    #[test]
    fn invalidate_path_drops_matching_locators() {
        let session = SessionId::new();
        let agent = AgentId::new();
        let mut reads = ReadSet::new(session, agent);
        reads
            .record(observation(
                session,
                agent,
                locator("src/a.rs", 0, 4),
                hash("a"),
                1,
            ))
            .expect("a");
        reads
            .record(observation(
                session,
                agent,
                locator("src/b.rs", 0, 4),
                hash("b"),
                2,
            ))
            .expect("b");
        assert_eq!(
            reads
                .invalidate_path(&path("src/a.rs"))
                .expect("invalidate"),
            1
        );
        assert_eq!(reads.len(), 1);
        assert!(reads.get(&locator("src/a.rs", 0, 4)).is_none());
        assert!(reads.get(&locator("src/b.rs", 0, 4)).is_some());
    }
}
