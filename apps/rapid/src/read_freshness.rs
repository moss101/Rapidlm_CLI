//! Session/actor-scoped read authority for one workspace view.
//!
//! This is a thin synchronized adapter over the context engine's `ReadSet`.
//! A caller must record only a read actually delivered to the model, and must
//! hold the existing per-path write lock while checking and applying a write.
//! This module alone does not authorize or perform filesystem operations.
use context_engine::{
    ContentHash, ContextLocator, Freshness, ReadObservation, ReadSet, ReadSetError,
};
use protocol::{AgentId, RepoPath, SessionId};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

#[derive(Clone)]
pub struct ReadAuthority {
    root: Arc<PathBuf>,
    state: Arc<Mutex<State>>,
    pending: Arc<PendingReads>,
}

/// Non-owning handle used by a shared write-lock registry. It cannot keep a
/// completed agent or its read proofs alive.
pub(crate) struct WeakReadAuthority {
    root: Weak<PathBuf>,
    state: Weak<Mutex<State>>,
    pending: Weak<PendingReads>,
}

impl WeakReadAuthority {
    pub(crate) fn upgrade(&self) -> Option<ReadAuthority> {
        Some(ReadAuthority {
            root: self.root.upgrade()?,
            state: self.state.upgrade()?,
            pending: self.pending.upgrade()?,
        })
    }
}
struct State {
    reads: ReadSet,
    seq: u64,
    invalidations: BTreeMap<RepoPath, u64>,
    visible: Option<HashSet<ContextLocator>>,
    file_stamps: BTreeMap<ContextLocator, FileStamp>,
}

/// Filesystem identity and change metadata are additional evidence, not a
/// substitute for the raw-content hash or an atomic compare-and-swap.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileStamp {
    len: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanos: i64,
}

impl FileStamp {
    pub(crate) fn capture(path: &Path) -> Result<Self, ReadAuthorityError> {
        let metadata =
            std::fs::symlink_metadata(path).map_err(|_| ReadAuthorityError::Unreadable)?;
        Self::from_metadata(&metadata)
    }

    pub(crate) fn from_metadata(metadata: &std::fs::Metadata) -> Result<Self, ReadAuthorityError> {
        if !metadata.file_type().is_file() {
            return Err(ReadAuthorityError::NotRegular);
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Ok(Self {
            len: metadata.len(),
            modified: metadata
                .modified()
                .map_err(|_| ReadAuthorityError::Unreadable)?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanos: metadata.ctime_nsec(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadVerdict {
    Fresh,
    Unread,
    Changed,
    NotVisible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadAuthorityError {
    InvalidRoot,
    ViewMismatch,
    InvalidPath,
    TooLarge,
    Poisoned,
    SequenceExhausted,
    TicketMismatch,
    TicketExpired,
    InvalidTicket,
    PendingCapacity,
    SessionCapacity,
    NotRegular,
    Unreadable,
    ReadSet(ReadSetError),
}

/// A bounded snapshot taken immediately before a workspace replacement.
pub struct GuardedWrite {
    pub relative: String,
    pub original: Option<Vec<u8>>,
}

/// A digest captured by a read but not yet granted to the model. It is tied
/// to the exact authority that staged it and carries no file contents.
#[derive(Clone)]
pub struct StagedRead {
    owner: Weak<Mutex<State>>,
    locator: ContextLocator,
    hash: ContentHash,
    generation: u64,
    seq: u64,
    file_stamp: Option<FileStamp>,
}

const MAX_PENDING_READS: usize = 256;
const MAX_PENDING_SUMMARY_BYTES: usize = 64 * 1024;

struct PendingRead {
    tool: String,
    arguments_hash: ContentHash,
    summary_hash: ContentHash,
    summary_len: usize,
    summary: String,
    delivered: bool,
    ticket: StagedRead,
}

/// Bounded read proofs for one turn and its approval continuations, keyed by
/// call id. A proof may be pending or already delivered. Text is retained
/// in memory only, subject to the per-result and entry bounds.
#[derive(Default)]
pub struct PendingReads {
    pending: Mutex<HashMap<String, PendingRead>>,
}

const MAX_SESSION_VIEWS: usize = 128;

struct SessionEntry {
    authority: ReadAuthority,
    compaction: Option<u64>,
}

/// In-memory authority registry for continuations in the same live session.
/// A fresh registry after restart conservatively starts with no reads.
#[derive(Clone, Default)]
pub struct SessionReads {
    entries: Arc<Mutex<BTreeMap<(SessionId, PathBuf), SessionEntry>>>,
}

/// Read authority held only while an interactive tool call is awaiting its
/// specific approval. Nothing is reconstructed from a durable transcript.
#[derive(Clone, Default)]
pub struct SuspendedReads {
    entries: Arc<Mutex<BTreeMap<(SessionId, String), ReadAuthority>>>,
    /// All workspace writers in this live host participate in the same
    /// invalidation group; a new host starts with neither locks nor proofs.
    write_locks: crate::exec_tools::WriteLocks,
    sessions: SessionReads,
    /// Exact media carried only by this live host across a specific wait.
    videos: Arc<Mutex<BTreeMap<(SessionId, String), SuspendedVideos>>>,
}

struct SuspendedVideos {
    root: PathBuf,
    call_id: String,
    task_hash: [u8; 32],
    videos: Vec<crate::file_refs::VideoAttachment>,
}

const MAX_SUSPENDED_VIDEOS: usize = 8;

fn video_task_hash(task: &str) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(task.as_bytes()).into()
}

impl SuspendedReads {
    pub(crate) fn insert_videos(
        &self,
        root: &Path,
        session: SessionId,
        token: &str,
        call_id: &str,
        task: &str,
        videos: &[crate::file_refs::VideoAttachment],
    ) {
        if videos.is_empty()
            || videos.len() > 1
            || token.is_empty()
            || token.len() > 512
            || call_id.is_empty()
            || call_id.len() > 512
            || videos.iter().any(|video| {
                video.bytes.len() as u64
                    > llm_router::providers::google_genai::MAX_UPLOADED_VIDEO_BYTES
            })
        {
            return;
        }
        let Ok(root) = std::fs::canonicalize(root) else {
            return;
        };
        let Ok(mut entries) = self.videos.lock() else {
            return;
        };
        let key = (session, token.to_owned());
        entries.remove(&key);
        let incoming = videos.iter().map(|video| video.bytes.len()).sum::<usize>();
        while !entries.is_empty()
            && (entries.len() >= MAX_SUSPENDED_VIDEOS
                || entries
                    .values()
                    .flat_map(|entry| &entry.videos)
                    .map(|video| video.bytes.len())
                    .sum::<usize>()
                    + incoming
                    > llm_router::providers::google_genai::MAX_UPLOADED_VIDEO_BYTES as usize)
        {
            entries.pop_first();
        }
        entries.insert(
            key,
            SuspendedVideos {
                root,
                call_id: call_id.to_owned(),
                task_hash: video_task_hash(task),
                videos: videos.to_vec(),
            },
        );
    }

    pub(crate) fn take_videos(
        &self,
        root: &Path,
        session: SessionId,
        token: &str,
        call_id: &str,
        task: &str,
    ) -> Option<Vec<crate::file_refs::VideoAttachment>> {
        let mut entries = self.videos.lock().ok()?;
        let entry = entries.remove(&(session, token.to_owned()))?;
        if entry.root != std::fs::canonicalize(root).ok()?
            || entry.call_id != call_id
            || entry.task_hash != video_task_hash(task)
        {
            return None;
        }
        Some(entry.videos)
    }
    pub(crate) fn write_locks(&self) -> crate::exec_tools::WriteLocks {
        self.write_locks.clone()
    }
    pub(crate) fn for_session(
        &self,
        root: &Path,
        session: SessionId,
        compaction: Option<u64>,
    ) -> Result<ReadAuthority, ReadAuthorityError> {
        self.sessions.for_session(root, session, compaction)
    }
    pub fn insert(&self, session: SessionId, token: &str, authority: ReadAuthority) {
        if token.is_empty() || token.len() > 512 {
            return;
        }
        if let Ok(mut entries) = self.entries.lock() {
            if entries.len() >= MAX_SESSION_VIEWS
                && !entries.contains_key(&(session, token.to_owned()))
            {
                // Losing an entry requires its approval to re-read; it
                // must not prevent all later approvals from retaining reads.
                entries.pop_first();
            }
            entries.insert((session, token.to_owned()), authority);
        }
    }

    /// Consume an entry once. A missing entry after restart fails closed.
    pub fn take(&self, session: SessionId, token: &str, root: &Path) -> Option<ReadAuthority> {
        let mut entries = self.entries.lock().ok()?;
        let authority = entries.remove(&(session, token.to_owned()))?;
        (std::fs::canonicalize(root).ok()? == *authority.root).then_some(authority)
    }
}

impl SessionReads {
    /// Reuse only the same session and canonical view. `compaction` is the
    /// ledger event sequence, not a summary hash; identical summaries may
    /// correspond to distinct compactions and must still invalidate reads.
    /// The caller must additionally prove that prior read results remain in
    /// the model's restored context; otherwise it must advance the returned
    /// authority's generation before use.
    pub fn for_session(
        &self,
        root: &Path,
        session: SessionId,
        compaction: Option<u64>,
    ) -> Result<ReadAuthority, ReadAuthorityError> {
        let root = std::fs::canonicalize(root).map_err(|_| ReadAuthorityError::InvalidRoot)?;
        if !root.is_dir() {
            return Err(ReadAuthorityError::InvalidRoot);
        }
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        let key = (session, root.clone());
        if let Some(entry) = entries.get_mut(&key) {
            // Concurrent ledger reads can finish out of order. Never roll an
            // authority back to a prior compaction snapshot.
            if entry
                .compaction
                .is_some_and(|old| compaction.is_none_or(|new| new < old))
            {
                return Err(ReadAuthorityError::TicketExpired);
            }
            if entry.compaction != compaction {
                entry.authority.advance_generation()?;
                entry
                    .authority
                    .pending
                    .pending
                    .lock()
                    .map_err(|_| ReadAuthorityError::Poisoned)?
                    .clear();
                entry.compaction = compaction;
            }
            return Ok(entry.authority.clone());
        }
        if entries.len() >= MAX_SESSION_VIEWS {
            return Err(ReadAuthorityError::SessionCapacity);
        }
        let authority = ReadAuthority::new(&root, session, AgentId::new())?;
        entries.insert(
            key,
            SessionEntry {
                authority: authority.clone(),
                compaction,
            },
        );
        Ok(authority)
    }
}

impl PendingReads {
    #[allow(clippy::too_many_arguments)]
    pub fn stage(
        &self,
        authority: &ReadAuthority,
        root: &Path,
        relative: &str,
        bytes: &[u8],
        call_id: &str,
        tool: &str,
        arguments: &str,
        summary: &str,
    ) -> Result<(), ReadAuthorityError> {
        self.stage_inner(
            authority, root, relative, bytes, call_id, tool, arguments, summary, None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn stage_verified(
        &self,
        authority: &ReadAuthority,
        root: &Path,
        relative: &str,
        bytes: &[u8],
        call_id: &str,
        tool: &str,
        arguments: &str,
        summary: &str,
        stamp: &FileStamp,
    ) -> Result<(), ReadAuthorityError> {
        self.stage_inner(
            authority,
            root,
            relative,
            bytes,
            call_id,
            tool,
            arguments,
            summary,
            Some(stamp),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn stage_inner(
        &self,
        authority: &ReadAuthority,
        root: &Path,
        relative: &str,
        bytes: &[u8],
        call_id: &str,
        tool: &str,
        arguments: &str,
        summary: &str,
        expected_stamp: Option<&FileStamp>,
    ) -> Result<(), ReadAuthorityError> {
        if call_id.is_empty()
            || call_id.len() > 256
            || tool.is_empty()
            || tool.len() > 256
            || arguments.len() > 64 * 1024
            || summary.len() > MAX_PENDING_SUMMARY_BYTES
        {
            return Err(ReadAuthorityError::InvalidTicket);
        }
        let ticket = authority.stage(root, relative, bytes)?;
        if expected_stamp.is_some_and(|stamp| ticket.file_stamp.as_ref() != Some(stamp)) {
            return Err(ReadAuthorityError::TicketExpired);
        }
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        if !pending.contains_key(call_id) && pending.len() >= MAX_PENDING_READS {
            // Eviction can only revoke authority; keep recent reads useful
            // during a long turn without retaining an unbounded transcript.
            if let Some(oldest) = pending
                .iter()
                .min_by_key(|(_, read)| read.ticket.seq)
                .map(|(id, _)| id.clone())
            {
                pending.remove(&oldest);
            }
        }
        pending.insert(
            call_id.to_owned(),
            PendingRead {
                tool: tool.to_owned(),
                arguments_hash: ContentHash::from_bytes(arguments.as_bytes()),
                summary_hash: ContentHash::from_bytes(summary.as_bytes()),
                summary_len: summary.len(),
                summary: summary.to_owned(),
                delivered: false,
                ticket,
            },
        );
        Ok(())
    }

    /// Reconcile visibility against an attested model-visible history. A
    /// reused call id is matched only against its most recent exchange.
    pub fn commit_history(
        &self,
        authority: &ReadAuthority,
        history: &[agent_runtime::ToolStepExchange],
    ) -> Result<usize, ReadAuthorityError> {
        // Fail closed if matching or recording any observation fails.
        authority.set_visible(HashSet::new())?;
        let pending = self
            .pending
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        let mut seen = HashSet::new();
        let mut ready = Vec::new();
        for exchange in history.iter().rev() {
            for call in exchange.calls().iter().rev() {
                if !seen.insert(call.call_id()) {
                    continue;
                }
                let Some(candidate) = pending.get(call.call_id()) else {
                    continue;
                };
                if candidate.tool != call.tool()
                    || candidate.arguments_hash
                        != ContentHash::from_bytes(call.arguments().as_bytes())
                {
                    continue;
                }
                let Some(agent_runtime::ToolStepResult::Succeeded { summary, .. }) = exchange
                    .results()
                    .iter()
                    .find(|result| result.call_id() == call.call_id())
                else {
                    continue;
                };
                let Some(prefix) = summary.as_bytes().get(..candidate.summary_len) else {
                    continue;
                };
                if candidate.summary_hash != ContentHash::from_bytes(prefix) {
                    continue;
                }
                ready.push((call.call_id().to_owned(), candidate.ticket.clone()));
            }
        }
        drop(pending);
        let mut committed = 0;
        ready.sort_by_key(|(_, ticket)| ticket.seq);
        let mut visible = HashSet::new();
        let mut delivered = Vec::new();
        for (id, ticket) in ready {
            let locator = ticket.locator.clone();
            let seq = ticket.seq;
            match authority.commit_visible(ticket) {
                Ok(()) => {
                    committed += 1;
                    visible.insert(locator);
                    delivered.push((id, seq));
                }
                Err(ReadAuthorityError::TicketExpired | ReadAuthorityError::TicketMismatch) => {}
                Err(error) => return Err(error),
            }
        }
        authority.set_visible(visible)?;
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        for (id, seq) in delivered {
            if let Some(candidate) = pending.get_mut(&id).filter(|entry| entry.ticket.seq == seq) {
                candidate.delivered = true;
            }
        }
        Ok(committed)
    }

    /// Candidate blocks for the context compiler. Only previously delivered
    /// results are eligible, and compilation may still drop any candidate.
    pub fn replay_blocks(
        &self,
    ) -> Result<Vec<context_engine::compile::CompileInput>, ReadAuthorityError> {
        let pending = self
            .pending
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        let mut candidates: Vec<_> = pending.values().filter(|read| read.delivered).collect();
        candidates.sort_by_key(|read| std::cmp::Reverse(read.ticket.seq));
        let mut bytes = 0usize;
        let mut blocks = Vec::new();
        for read in candidates {
            if bytes.saturating_add(read.summary.len()) > 512 * 1024 {
                continue;
            }
            bytes += read.summary.len();
            blocks.push(context_engine::compile::CompileInput::new(
                format!("session-read/{}", read.ticket.seq),
                read.summary.clone(),
            ));
        }
        Ok(blocks)
    }

    /// Add only receipts that match previously delivered, still-current
    /// tickets belonging to this authority. The history callback has already
    /// replaced the visible set for the current model step.
    pub(crate) fn commit_context(
        &self,
        authority: &ReadAuthority,
        receipts: &[agent_runtime::ModelContextReceipt],
    ) -> Result<usize, ReadAuthorityError> {
        let pending = self
            .pending
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        let mut tickets = Vec::new();
        for read in pending.values().filter(|read| read.delivered) {
            let locator = format!("session-read/{}", read.ticket.seq);
            if receipts.iter().any(|receipt| {
                receipt.locator == locator && receipt.sha256 == *read.summary_hash.as_digest()
            }) {
                tickets.push(read.ticket.clone());
            }
        }
        drop(pending);
        tickets.sort_by_key(|ticket| ticket.seq);
        let mut accepted = HashSet::new();
        for ticket in tickets {
            let locator = ticket.locator.clone();
            match authority.commit_visible(ticket) {
                Ok(()) => {
                    accepted.insert(locator);
                }
                Err(ReadAuthorityError::TicketExpired | ReadAuthorityError::TicketMismatch) => {}
                Err(error) => return Err(error),
            }
        }
        let count = accepted.len();
        let mut state = authority
            .state
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        state
            .visible
            .get_or_insert_with(HashSet::new)
            .extend(accepted);
        Ok(count)
    }
}

#[derive(Debug)]
pub enum WriteGuardError {
    Authority(ReadAuthorityError),
    Refused(ReadVerdict),
}

impl From<ReadAuthorityError> for WriteGuardError {
    fn from(value: ReadAuthorityError) -> Self {
        Self::Authority(value)
    }
}
impl std::fmt::Display for ReadAuthorityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "read authority: {self:?}")
    }
}
impl std::error::Error for ReadAuthorityError {}
impl From<ReadSetError> for ReadAuthorityError {
    fn from(value: ReadSetError) -> Self {
        Self::ReadSet(value)
    }
}

impl ReadAuthority {
    pub(crate) fn downgrade(&self) -> WeakReadAuthority {
        WeakReadAuthority {
            root: Arc::downgrade(&self.root),
            state: Arc::downgrade(&self.state),
            pending: Arc::downgrade(&self.pending),
        }
    }

    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub(crate) fn same_view(&self, root: &Path) -> bool {
        self.root.as_path() == root
    }
    /// Bind to an existing canonical workspace root and a single actor.
    /// Clones share observations; a different actor or view needs a new set.
    pub fn new(
        root: &Path,
        session: SessionId,
        agent: AgentId,
    ) -> Result<Self, ReadAuthorityError> {
        let root = std::fs::canonicalize(root).map_err(|_| ReadAuthorityError::InvalidRoot)?;
        if !root.is_dir() {
            return Err(ReadAuthorityError::InvalidRoot);
        }
        Ok(Self {
            root: Arc::new(root),
            state: Arc::new(Mutex::new(State {
                reads: ReadSet::new(session, agent),
                seq: 0,
                invalidations: BTreeMap::new(),
                visible: None,
                file_stamps: BTreeMap::new(),
            })),
            pending: Arc::new(PendingReads::default()),
        })
    }

    pub(crate) fn pending_reads(&self) -> &PendingReads {
        &self.pending
    }

    fn set_visible(&self, visible: HashSet<ContextLocator>) -> Result<(), ReadAuthorityError> {
        self.state
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?
            .visible = Some(visible);
        Ok(())
    }

    fn locator(&self, root: &Path, relative: &str) -> Result<ContextLocator, ReadAuthorityError> {
        let root = std::fs::canonicalize(root).map_err(|_| ReadAuthorityError::InvalidRoot)?;
        if root != *self.root {
            return Err(ReadAuthorityError::ViewMismatch);
        }
        let path = RepoPath::parse(relative).map_err(|_| ReadAuthorityError::InvalidPath)?;
        // RepoPath normalizes separators and dot components. Treating two
        // different spellings as one identity could authorize an unrelated
        // Unix filename containing a backslash; callers must pass the exact
        // normalized workspace-relative path they resolved.
        if path.as_str() != relative {
            return Err(ReadAuthorityError::InvalidPath);
        }
        Ok(ContextLocator::file(path))
    }

    /// Bind a successfully read existing file to its resolved identity in
    /// this view. This does not itself record visibility or authorize writes.
    pub fn identity_for_read(
        &self,
        root: &Path,
        target: &Path,
    ) -> Result<String, ReadAuthorityError> {
        let root = std::fs::canonicalize(root).map_err(|_| ReadAuthorityError::InvalidRoot)?;
        if root != *self.root {
            return Err(ReadAuthorityError::ViewMismatch);
        }
        let target = std::fs::canonicalize(target).map_err(|_| ReadAuthorityError::Unreadable)?;
        if !target.is_file() {
            return Err(ReadAuthorityError::NotRegular);
        }
        let path = target
            .strip_prefix(&root)
            .map_err(|_| ReadAuthorityError::ViewMismatch)?;
        let mut components = Vec::new();
        for component in path.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(ReadAuthorityError::InvalidPath);
            };
            let name = name.to_str().ok_or(ReadAuthorityError::InvalidPath)?;
            if name.contains('\\') {
                return Err(ReadAuthorityError::InvalidPath);
            }
            components.push(name);
        }
        let relative = components.join("/");
        self.locator(&root, &relative)?;
        Ok(relative)
    }

    /// Record the original bytes only after the corresponding content was
    /// delivered to this actor. Display transcoding must never change the hash.
    pub fn record_visible(
        &self,
        root: &Path,
        relative: &str,
        bytes: &[u8],
    ) -> Result<(), ReadAuthorityError> {
        let ticket = self.stage(root, relative, bytes)?;
        self.commit_visible(ticket)
    }

    /// Capture a digest without granting write authority. The caller commits
    /// it only after the corresponding read result actually reached the model.
    pub fn stage(
        &self,
        root: &Path,
        relative: &str,
        bytes: &[u8],
    ) -> Result<StagedRead, ReadAuthorityError> {
        if bytes.len() > crate::exec_tools::MAX_FILE_READ_BYTES {
            return Err(ReadAuthorityError::TooLarge);
        }
        let locator = self.locator(root, relative)?;
        let hash = ContentHash::from_bytes(bytes);
        let file_stamp = FileStamp::capture(&root.join(relative)).ok();
        let mut state = self
            .state
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        let seq = state
            .seq
            .checked_add(1)
            .ok_or(ReadAuthorityError::SequenceExhausted)?;
        state.seq = seq;
        Ok(StagedRead {
            owner: Arc::downgrade(&self.state),
            locator,
            hash,
            generation: state.reads.visibility_generation(),
            seq,
            file_stamp,
        })
    }

    /// Grant a staged read to this actor after confirmed model delivery.
    pub fn commit_visible(&self, ticket: StagedRead) -> Result<(), ReadAuthorityError> {
        let owner = ticket
            .owner
            .upgrade()
            .ok_or(ReadAuthorityError::TicketMismatch)?;
        if !Arc::ptr_eq(&owner, &self.state) {
            return Err(ReadAuthorityError::TicketMismatch);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        if state.reads.visibility_generation() != ticket.generation {
            return Err(ReadAuthorityError::TicketExpired);
        }
        if state
            .invalidations
            .get(ticket.locator.path())
            .is_some_and(|seq| *seq > ticket.seq)
            || state
                .reads
                .get(&ticket.locator)
                .is_some_and(|record| record.last_seen_seq() > ticket.seq)
        {
            return Err(ReadAuthorityError::TicketExpired);
        }
        let locator = ticket.locator.clone();
        let observation = ReadObservation::new(
            state.reads.session_id(),
            state.reads.agent_id(),
            ticket.locator,
            ticket.hash,
            ticket.seq,
        );
        state.reads.record(observation)?;
        if let Some(stamp) = ticket.file_stamp {
            state.file_stamps.insert(locator, stamp);
        } else {
            state.file_stamps.remove(&locator);
        }
        Ok(())
    }

    /// Compare current raw file bytes with the last currently visible read.
    pub fn check(
        &self,
        root: &Path,
        relative: &str,
        bytes: &[u8],
    ) -> Result<ReadVerdict, ReadAuthorityError> {
        if bytes.len() > crate::exec_tools::MAX_FILE_READ_BYTES {
            return Err(ReadAuthorityError::TooLarge);
        }
        let locator = self.locator(root, relative)?;
        let state = self
            .state
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        let hash = ContentHash::from_bytes(bytes);
        Ok(match state.reads.freshness(&locator, hash)? {
            Freshness::Unknown => ReadVerdict::Unread,
            Freshness::Stale => ReadVerdict::Changed,
            Freshness::Fresh
                if state.reads.is_currently_visible(&locator, hash)?
                    && state
                        .visible
                        .as_ref()
                        .is_none_or(|visible| visible.contains(&locator)) =>
            {
                ReadVerdict::Fresh
            }
            Freshness::Fresh => ReadVerdict::NotVisible,
        })
    }

    /// Inspect the exact target under the caller's workspace write lock.
    /// A missing leaf is a creation; all other read/metadata errors fail
    /// closed. The caller must repeat this immediately before replacement.
    pub fn guard_write(&self, root: &Path, target: &Path) -> Result<GuardedWrite, WriteGuardError> {
        let root = std::fs::canonicalize(root).map_err(|_| ReadAuthorityError::InvalidRoot)?;
        if root != *self.root {
            return Err(ReadAuthorityError::ViewMismatch.into());
        }
        let parent = target.parent().ok_or(ReadAuthorityError::InvalidPath)?;
        let parent = std::fs::canonicalize(parent).map_err(|_| ReadAuthorityError::InvalidPath)?;
        if !parent.starts_with(&root) {
            return Err(ReadAuthorityError::ViewMismatch.into());
        }
        let leaf = target.file_name().ok_or(ReadAuthorityError::InvalidPath)?;
        let actual = parent.join(leaf);
        let relative_path = actual
            .strip_prefix(&root)
            .map_err(|_| ReadAuthorityError::ViewMismatch)?;
        let mut components = Vec::new();
        for component in relative_path.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(ReadAuthorityError::InvalidPath.into());
            };
            let name = name.to_str().ok_or(ReadAuthorityError::InvalidPath)?;
            if name.contains('\\') {
                return Err(ReadAuthorityError::InvalidPath.into());
            }
            components.push(name);
        }
        let relative = components.join("/");
        // Validate the resulting identity even for a newly created leaf.
        self.locator(&root, &relative)?;
        let metadata = match std::fs::symlink_metadata(&actual) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(GuardedWrite {
                    relative,
                    original: None,
                });
            }
            Err(_) => return Err(ReadAuthorityError::Unreadable.into()),
        };
        if !metadata.file_type().is_file() {
            return Err(ReadAuthorityError::NotRegular.into());
        }
        let before_stamp = FileStamp::capture(&actual)?;
        let bytes = match crate::exec_tools::read_file_bounded(
            &actual,
            crate::exec_tools::MAX_FILE_READ_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(crate::exec_tools::BoundedReadError::TooLarge) => {
                return Err(ReadAuthorityError::TooLarge.into());
            }
            Err(_) => return Err(ReadAuthorityError::Unreadable.into()),
        };
        let verdict = self.check(&root, &relative, &bytes)?;
        if verdict != ReadVerdict::Fresh {
            return Err(WriteGuardError::Refused(verdict));
        }
        let after_stamp = FileStamp::capture(&actual)?;
        let locator = self.locator(&root, &relative)?;
        let state = self
            .state
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        if before_stamp != after_stamp || state.file_stamps.get(&locator) != Some(&after_stamp) {
            return Err(WriteGuardError::Refused(ReadVerdict::Changed));
        }
        Ok(GuardedWrite {
            relative,
            original: Some(bytes),
        })
    }

    /// Remove all prior observations for a successfully changed file.
    pub fn invalidate(&self, root: &Path, relative: &str) -> Result<(), ReadAuthorityError> {
        let locator = self.locator(root, relative)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        let seq = state
            .seq
            .checked_add(1)
            .ok_or(ReadAuthorityError::SequenceExhausted)?;
        if !state.invalidations.contains_key(locator.path())
            && state.invalidations.len() >= context_engine::DEFAULT_MAX_READ_RECORDS
        {
            // Bound the path tombstones. Advancing visibility first makes
            // every outstanding ticket and older observation fail closed.
            state.reads.advance_generation();
            state.invalidations.clear();
            state.file_stamps.clear();
        }
        state.reads.invalidate_path(locator.path())?;
        state
            .file_stamps
            .retain(|key, _| key.path() != locator.path());
        state.invalidations.insert(locator.path().clone(), seq);
        state.seq = seq;
        Ok(())
    }

    /// Discard visibility after context replacement; a new read is required.
    pub fn advance_generation(&self) -> Result<(), ReadAuthorityError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ReadAuthorityError::Poisoned)?;
        state.reads.advance_generation();
        state.invalidations.clear();
        state.file_stamps.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TempRoot(PathBuf);

    #[test]
    fn suspended_videos_are_bounded_single_use_and_bound_to_wait_and_view() {
        let root = TempRoot::new();
        let other_root = TempRoot::new();
        let session = SessionId::new();
        let other_session = SessionId::new();
        let cache = SuspendedReads::default();
        let video = crate::file_refs::VideoAttachment {
            path: "clip.mp4".into(),
            mime_type: "video/mp4".into(),
            bytes: b"original".as_slice().into(),
        };
        assert!(!format!("{video:?}").contains("original"));
        let insert = |token: &str| {
            cache.insert_videos(
                &root.0,
                session,
                token,
                "call",
                "prompt",
                std::slice::from_ref(&video),
            )
        };
        insert("one");
        assert!(
            cache
                .take_videos(&root.0, other_session, "one", "call", "prompt")
                .is_none()
        );
        assert!(
            cache
                .take_videos(&root.0, session, "wrong", "call", "prompt")
                .is_none()
        );
        let retained = cache
            .take_videos(&root.0, session, "one", "call", "prompt")
            .unwrap();
        assert_eq!(retained, vec![video.clone()]);
        assert!(std::sync::Arc::ptr_eq(&retained[0].bytes, &video.bytes));
        assert!(
            cache
                .take_videos(&root.0, session, "one", "call", "prompt")
                .is_none()
        );
        for (root_to_try, call, task) in [
            (&other_root.0, "call", "prompt"),
            (&root.0, "other", "prompt"),
            (&root.0, "call", "changed"),
        ] {
            insert("one");
            assert!(
                cache
                    .take_videos(root_to_try, session, "one", call, task)
                    .is_none()
            );
        }
        for index in 0..=MAX_SUSPENDED_VIDEOS {
            insert(&format!("token-{index:02}"));
        }
        assert!(
            cache
                .take_videos(&root.0, session, "token-00", "call", "prompt")
                .is_none()
        );
        assert!(
            cache
                .take_videos(&root.0, session, "token-08", "call", "prompt")
                .is_some()
        );
        assert!(
            SuspendedReads::default()
                .take_videos(&root.0, session, "token-01", "call", "prompt")
                .is_none()
        );
    }
    impl TempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rapid-read-authority-{}",
                protocol::SessionId::new()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn actor_view_and_visibility_are_bound_to_original_bytes() {
        let root = TempRoot::new();
        let other_root = TempRoot::new();
        let session = SessionId::new();
        let reads = ReadAuthority::new(&root.0, session, AgentId::new()).unwrap();
        let child = ReadAuthority::new(&root.0, session, AgentId::new()).unwrap();
        let other_session = ReadAuthority::new(&root.0, SessionId::new(), AgentId::new()).unwrap();
        let shared = reads.clone();
        let utf16 = [0xff, 0xfe, b'a', 0];
        assert_eq!(
            reads.check(&root.0, "a.txt", &utf16).unwrap(),
            ReadVerdict::Unread
        );
        reads.record_visible(&root.0, "a.txt", &utf16).unwrap();
        assert_eq!(
            shared.check(&root.0, "a.txt", &utf16).unwrap(),
            ReadVerdict::Fresh
        );
        assert_eq!(
            reads.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Changed
        );
        assert_eq!(
            child.check(&root.0, "a.txt", &utf16).unwrap(),
            ReadVerdict::Unread
        );
        assert_eq!(
            other_session.check(&root.0, "a.txt", &utf16).unwrap(),
            ReadVerdict::Unread
        );
        assert_eq!(
            shared.check(&other_root.0, "a.txt", &utf16).unwrap_err(),
            ReadAuthorityError::ViewMismatch
        );
        assert_eq!(
            shared.record_visible(&root.0, "../a", &utf16).unwrap_err(),
            ReadAuthorityError::InvalidPath
        );
        assert_eq!(
            shared.record_visible(&root.0, "a\\b", &utf16).unwrap_err(),
            ReadAuthorityError::InvalidPath
        );
        reads.advance_generation().unwrap();
        assert_eq!(
            reads.check(&root.0, "a.txt", &utf16).unwrap(),
            ReadVerdict::NotVisible
        );
        reads.record_visible(&root.0, "a.txt", &utf16).unwrap();
        reads.invalidate(&root.0, "a.txt").unwrap();
        assert_eq!(
            shared.check(&root.0, "a.txt", &utf16).unwrap(),
            ReadVerdict::Unread
        );
    }

    #[test]
    fn rejects_unbounded_source_bytes_before_recording_or_hashing() {
        let root = TempRoot::new();
        let reads = ReadAuthority::new(&root.0, SessionId::new(), AgentId::new()).unwrap();
        let bytes = vec![b'x'; crate::exec_tools::MAX_FILE_READ_BYTES + 1];
        assert_eq!(
            reads.record_visible(&root.0, "a.txt", &bytes).unwrap_err(),
            ReadAuthorityError::TooLarge
        );
        assert_eq!(
            reads.check(&root.0, "a.txt", &bytes).unwrap_err(),
            ReadAuthorityError::TooLarge
        );
        assert_eq!(
            reads.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Unread
        );
    }

    #[test]
    fn guarded_write_requires_a_current_read_but_allows_a_new_file() {
        let root = TempRoot::new();
        let reads = ReadAuthority::new(&root.0, SessionId::new(), AgentId::new()).unwrap();
        let path = root.0.join("file.txt");
        let creation = reads.guard_write(&root.0, &path).unwrap();
        assert!(creation.original.is_none());
        assert_eq!(creation.relative, "file.txt");
        std::fs::write(&path, b"before").unwrap();
        assert!(matches!(
            reads.guard_write(&root.0, &path),
            Err(WriteGuardError::Refused(ReadVerdict::Unread))
        ));
        reads
            .record_visible(&root.0, "file.txt", b"before")
            .unwrap();
        let permitted = reads.guard_write(&root.0, &path).unwrap();
        assert_eq!(permitted.original.as_deref(), Some(b"before".as_slice()));
        std::fs::write(&path, b"changed").unwrap();
        assert!(matches!(
            reads.guard_write(&root.0, &path),
            Err(WriteGuardError::Refused(ReadVerdict::Changed))
        ));
        #[cfg(unix)]
        {
            let link = root.0.join("link.txt");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(matches!(
                reads.guard_write(&root.0, &link),
                Err(WriteGuardError::Authority(ReadAuthorityError::NotRegular))
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn identical_external_replacement_revokes_the_previous_file_identity() {
        let root = TempRoot::new();
        let path = root.0.join("file.txt");
        std::fs::write(&path, b"same").unwrap();
        let reads = ReadAuthority::new(&root.0, SessionId::new(), AgentId::new()).unwrap();
        let before = FileStamp::capture(&path).unwrap();
        reads.record_visible(&root.0, "file.txt", b"same").unwrap();
        let replacement = root.0.join("replacement");
        std::fs::write(&replacement, b"same").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_ne!(before, FileStamp::capture(&path).unwrap());
        assert!(matches!(
            reads.guard_write(&root.0, &path),
            Err(WriteGuardError::Refused(ReadVerdict::Changed))
        ));
        reads.record_visible(&root.0, "file.txt", b"same").unwrap();
        assert!(reads.guard_write(&root.0, &path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn metadata_change_between_read_and_staging_cannot_grant_a_proof() {
        let root = TempRoot::new();
        let path = root.0.join("file.txt");
        std::fs::write(&path, b"same").unwrap();
        let reads = ReadAuthority::new(&root.0, SessionId::new(), AgentId::new()).unwrap();
        let before = FileStamp::capture(&path).unwrap();
        let replacement = root.0.join("replacement");
        std::fs::write(&replacement, b"same").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(
            reads
                .pending_reads()
                .stage_verified(
                    &reads,
                    &root.0,
                    "file.txt",
                    b"same",
                    "c1",
                    "repo_read",
                    r#"{"path":"file.txt"}"#,
                    "same",
                    &before
                )
                .unwrap_err(),
            ReadAuthorityError::TicketExpired
        );
        assert_eq!(
            reads.check(&root.0, "file.txt", b"same").unwrap(),
            ReadVerdict::Unread
        );
    }

    #[test]
    fn staged_reads_require_delivery_and_cannot_cross_actors_or_invalidations() {
        let root = TempRoot::new();
        let session = SessionId::new();
        let reads = ReadAuthority::new(&root.0, session, AgentId::new()).unwrap();
        let other = ReadAuthority::new(&root.0, session, AgentId::new()).unwrap();
        let ticket = reads.stage(&root.0, "a.txt", b"a").unwrap();
        assert_eq!(
            reads.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Unread
        );
        assert_eq!(
            other.commit_visible(ticket).unwrap_err(),
            ReadAuthorityError::TicketMismatch
        );

        let ticket = reads.stage(&root.0, "a.txt", b"a").unwrap();
        reads.invalidate(&root.0, "other.txt").unwrap();
        reads.commit_visible(ticket).unwrap();
        assert_eq!(
            reads.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Fresh
        );
        let stale = reads.stage(&root.0, "a.txt", b"a").unwrap();
        reads.invalidate(&root.0, "a.txt").unwrap();
        assert_eq!(
            reads.commit_visible(stale).unwrap_err(),
            ReadAuthorityError::TicketExpired
        );
        let stale = reads.stage(&root.0, "a.txt", b"a").unwrap();
        reads.advance_generation().unwrap();
        assert_eq!(
            reads.commit_visible(stale).unwrap_err(),
            ReadAuthorityError::TicketExpired
        );

        let older = reads.stage(&root.0, "a.txt", b"old").unwrap();
        let newer = reads.stage(&root.0, "a.txt", b"new").unwrap();
        reads.commit_visible(newer).unwrap();
        assert_eq!(
            reads.commit_visible(older).unwrap_err(),
            ReadAuthorityError::TicketExpired
        );
        assert_eq!(
            reads.check(&root.0, "a.txt", b"new").unwrap(),
            ReadVerdict::Fresh
        );
    }

    #[test]
    fn replay_receipts_require_original_delivery_and_current_matching_proof() {
        use agent_runtime::{
            ModelContextReceipt, ProposedToolCall, ToolStepExchange, ToolStepResult,
        };
        let root = TempRoot::new();
        let reads = ReadAuthority::new(&root.0, SessionId::new(), AgentId::new()).unwrap();
        let pending = reads.pending_reads();
        let args = r#"{"path":"a.txt"}"#;
        pending
            .stage(
                &reads,
                &root.0,
                "a.txt",
                b"original",
                "read",
                "repo_read",
                args,
                "visible result",
            )
            .unwrap();
        assert!(pending.replay_blocks().unwrap().is_empty());
        let exchange = ToolStepExchange::new(
            vec![ProposedToolCall::new("read", "repo_read", args).unwrap()],
            vec![ToolStepResult::Succeeded {
                call_id: "read".into(),
                summary: "visible result".into(),
            }],
        );
        pending.commit_history(&reads, &[exchange]).unwrap();
        let blocks = pending.replay_blocks().unwrap();
        assert_eq!(blocks.len(), 1);
        let receipt = ModelContextReceipt {
            locator: blocks[0].locator().into(),
            sha256: *ContentHash::from_bytes(blocks[0].text().as_bytes()).as_digest(),
        };
        pending.commit_history(&reads, &[]).unwrap();
        assert_eq!(
            reads.check(&root.0, "a.txt", b"original").unwrap(),
            ReadVerdict::NotVisible
        );
        let mut wrong = receipt.clone();
        wrong.sha256[0] ^= 1;
        assert_eq!(pending.commit_context(&reads, &[wrong]).unwrap(), 0);
        assert_eq!(
            pending
                .commit_context(&reads, std::slice::from_ref(&receipt))
                .unwrap(),
            1
        );
        assert_eq!(
            reads.check(&root.0, "a.txt", b"original").unwrap(),
            ReadVerdict::Fresh
        );
        assert_eq!(
            reads.check(&root.0, "a.txt", b"changed").unwrap(),
            ReadVerdict::Changed
        );
        let other = ReadAuthority::new(&root.0, SessionId::new(), AgentId::new()).unwrap();
        assert_eq!(
            pending
                .commit_context(&other, std::slice::from_ref(&receipt))
                .unwrap(),
            0
        );
        reads.invalidate(&root.0, "a.txt").unwrap();
        assert_eq!(pending.commit_context(&reads, &[receipt]).unwrap(), 0);
        assert_eq!(
            reads.check(&root.0, "a.txt", b"original").unwrap(),
            ReadVerdict::Unread
        );
    }

    #[test]
    fn trimmed_history_only_keeps_the_read_results_it_contains() {
        use agent_runtime::{ProposedToolCall, ToolStepExchange, ToolStepResult};
        let root = TempRoot::new();
        let reads = ReadAuthority::new(&root.0, SessionId::new(), AgentId::new()).unwrap();
        let pending = reads.pending_reads();
        let make = |id: &str, path: &str| {
            let args = format!(r#"{{"path":"{path}"}}"#);
            pending
                .stage(
                    &reads,
                    &root.0,
                    path,
                    path.as_bytes(),
                    id,
                    "repo_read",
                    &args,
                    path,
                )
                .unwrap();
            ToolStepExchange::new(
                vec![ProposedToolCall::new(id, "repo_read", args).unwrap()],
                vec![ToolStepResult::Succeeded {
                    call_id: id.to_owned(),
                    summary: path.to_owned(),
                }],
            )
        };
        let first = make("one", "a.txt");
        let second = make("two", "b.txt");
        assert_eq!(
            pending
                .commit_history(&reads, &[first, second.clone()])
                .unwrap(),
            2
        );
        assert_eq!(
            reads.check(&root.0, "a.txt", b"a.txt").unwrap(),
            ReadVerdict::Fresh
        );
        assert_eq!(
            reads.check(&root.0, "b.txt", b"b.txt").unwrap(),
            ReadVerdict::Fresh
        );
        assert_eq!(pending.commit_history(&reads, &[second]).unwrap(), 1);
        assert_eq!(
            reads.check(&root.0, "a.txt", b"a.txt").unwrap(),
            ReadVerdict::NotVisible
        );
        assert_eq!(
            reads.check(&root.0, "b.txt", b"b.txt").unwrap(),
            ReadVerdict::Fresh
        );
    }

    #[test]
    fn visible_reads_follow_attested_history_and_cannot_survive_a_write() {
        use agent_runtime::{ProposedToolCall, ToolStepExchange, ToolStepResult};
        let root = TempRoot::new();
        let reads = ReadAuthority::new(&root.0, SessionId::new(), AgentId::new()).unwrap();
        let pending = reads.pending_reads();
        let args = r#"{"path":"a.txt"}"#;
        let exchange = |id: &str, summary: &str| {
            ToolStepExchange::new(
                vec![ProposedToolCall::new(id, "repo_read", args).unwrap()],
                vec![ToolStepResult::Succeeded {
                    call_id: id.to_owned(),
                    summary: summary.to_owned(),
                }],
            )
        };
        pending
            .stage(
                &reads,
                &root.0,
                "a.txt",
                b"a",
                "one",
                "repo_read",
                args,
                "a",
            )
            .unwrap();
        let one = exchange("one", "a");
        assert_eq!(
            pending
                .commit_history(&reads, std::slice::from_ref(&one))
                .unwrap(),
            1
        );
        assert_eq!(
            reads.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Fresh
        );
        assert_eq!(pending.commit_history(&reads, &[]).unwrap(), 0);
        assert_eq!(
            reads.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::NotVisible
        );
        // A retained exact result is visible again if the model receives it.
        assert_eq!(
            pending
                .commit_history(&reads, std::slice::from_ref(&one))
                .unwrap(),
            1
        );
        assert_eq!(
            reads.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Fresh
        );
        reads.invalidate(&root.0, "a.txt").unwrap();
        assert_eq!(
            pending
                .commit_history(&reads, std::slice::from_ref(&one))
                .unwrap(),
            0
        );
        assert_eq!(
            reads.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Unread
        );
        // A newly staged result in a trimmed suffix can grant authority.
        pending
            .stage(
                &reads,
                &root.0,
                "a.txt",
                b"a",
                "two",
                "repo_read",
                args,
                "a",
            )
            .unwrap();
        assert_eq!(
            pending
                .commit_history(&reads, &[exchange("two", "a")])
                .unwrap(),
            1
        );
        assert_eq!(
            reads.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Fresh
        );
        assert_eq!(
            pending
                .commit_history(&reads, &[exchange("two", "changed")])
                .unwrap(),
            0
        );
        assert_eq!(
            reads.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::NotVisible
        );
    }

    #[test]
    fn pending_reads_commit_only_matching_successful_delivered_results() {
        use agent_runtime::{ProposedToolCall, ToolStepExchange, ToolStepResult};
        let root = TempRoot::new();
        let reads = ReadAuthority::new(&root.0, SessionId::new(), AgentId::new()).unwrap();
        let pending = PendingReads::default();
        let args = r#"{"path":"a.txt"}"#;
        pending
            .stage(
                &reads,
                &root.0,
                "a.txt",
                b"source",
                "r1",
                "repo_read",
                args,
                "visible",
            )
            .unwrap();
        assert_eq!(pending.commit_history(&reads, &[]).unwrap(), 0);
        assert_eq!(
            reads.check(&root.0, "a.txt", b"source").unwrap(),
            ReadVerdict::Unread
        );
        let exchange = |arguments: &str, result| {
            ToolStepExchange::new(
                vec![ProposedToolCall::new("r1", "repo_read", arguments).unwrap()],
                vec![result],
            )
        };
        let failed = exchange(
            args,
            ToolStepResult::Failed {
                call_id: "r1".into(),
                handled: true,
                detail: None,
            },
        );
        assert_eq!(pending.commit_history(&reads, &[failed]).unwrap(), 0);
        let wrong = exchange(
            r#"{"path":"other"}"#,
            ToolStepResult::Succeeded {
                call_id: "r1".into(),
                summary: "visible".into(),
            },
        );
        assert_eq!(pending.commit_history(&reads, &[wrong]).unwrap(), 0);
        let altered = exchange(
            args,
            ToolStepResult::Succeeded {
                call_id: "r1".into(),
                summary: "different".into(),
            },
        );
        assert_eq!(pending.commit_history(&reads, &[altered]).unwrap(), 0);
        let visible = exchange(
            args,
            ToolStepResult::Succeeded {
                call_id: "r1".into(),
                summary: "visible\n[hook context]".into(),
            },
        );
        assert_eq!(pending.commit_history(&reads, &[visible]).unwrap(), 1);
        assert_eq!(
            reads.check(&root.0, "a.txt", b"source").unwrap(),
            ReadVerdict::Fresh
        );
    }

    #[test]
    fn pending_reads_do_not_match_an_older_reused_call_id() {
        use agent_runtime::{ProposedToolCall, ToolStepExchange, ToolStepResult};
        let root = TempRoot::new();
        let reads = ReadAuthority::new(&root.0, SessionId::new(), AgentId::new()).unwrap();
        let pending = PendingReads::default();
        let args = r#"{"path":"a.txt"}"#;
        pending
            .stage(
                &reads,
                &root.0,
                "a.txt",
                b"new",
                "r1",
                "repo_read",
                args,
                "same visible prefix",
            )
            .unwrap();
        let call = || ProposedToolCall::new("r1", "repo_read", args).unwrap();
        let old = ToolStepExchange::new(
            vec![call()],
            vec![ToolStepResult::Succeeded {
                call_id: "r1".into(),
                summary: "same visible prefix".into(),
            }],
        );
        let newer = ToolStepExchange::new(
            vec![call()],
            vec![ToolStepResult::Failed {
                call_id: "r1".into(),
                handled: true,
                detail: None,
            }],
        );
        assert_eq!(pending.commit_history(&reads, &[old, newer]).unwrap(), 0);
        assert_eq!(
            reads.check(&root.0, "a.txt", b"new").unwrap(),
            ReadVerdict::Unread
        );
    }

    #[test]
    fn suspended_reads_are_token_session_and_view_scoped_and_single_use() {
        let root = TempRoot::new();
        let other = TempRoot::new();
        let session = SessionId::new();
        let authority = ReadAuthority::new(&root.0, session, AgentId::new()).unwrap();
        authority.record_visible(&root.0, "a.txt", b"a").unwrap();
        let registry = SuspendedReads::default();
        registry.insert(session, "approval-1", authority.clone());
        assert!(
            registry
                .take(SessionId::new(), "approval-1", &root.0)
                .is_none()
        );
        assert!(registry.take(session, "approval-2", &root.0).is_none());
        let restored = registry.take(session, "approval-1", &root.0).unwrap();
        assert_eq!(
            restored.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Fresh
        );
        assert!(registry.take(session, "approval-1", &root.0).is_none());
        registry.insert(session, "approval-3", authority);
        assert!(registry.take(session, "approval-3", &other.0).is_none());
        assert!(registry.take(session, "approval-3", &root.0).is_none());
        assert!(
            SuspendedReads::default()
                .take(session, "approval-1", &root.0)
                .is_none()
        );
    }

    #[test]
    fn suspended_read_capacity_evicts_without_granting_an_unrelated_token() {
        let root = TempRoot::new();
        let session = SessionId::new();
        let authority = ReadAuthority::new(&root.0, session, AgentId::new()).unwrap();
        let registry = SuspendedReads::default();
        for index in 0..=MAX_SESSION_VIEWS {
            registry.insert(session, &format!("token-{index:04}"), authority.clone());
        }
        assert!(registry.take(session, "token-0000", &root.0).is_none());
        assert!(registry.take(session, "not-inserted", &root.0).is_none());
        assert!(
            registry
                .take(session, &format!("token-{MAX_SESSION_VIEWS:04}"), &root.0)
                .is_some()
        );
    }

    #[test]
    fn session_registry_separates_views_and_invalidates_on_each_compaction_event() {
        let root = TempRoot::new();
        let other_root = TempRoot::new();
        let session = SessionId::new();
        let registry = SessionReads::default();
        let first = registry.for_session(&root.0, session, None).unwrap();
        first.record_visible(&root.0, "a.txt", b"a").unwrap();
        let same = registry.for_session(&root.0, session, None).unwrap();
        assert_eq!(
            same.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Fresh
        );
        let different_view = registry.for_session(&other_root.0, session, None).unwrap();
        assert_eq!(
            different_view.check(&other_root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Unread
        );
        let different_session = registry
            .for_session(&root.0, SessionId::new(), None)
            .unwrap();
        assert_eq!(
            different_session.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Unread
        );
        let compacted = registry.for_session(&root.0, session, Some(10)).unwrap();
        assert_eq!(
            compacted.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::NotVisible
        );
        compacted.record_visible(&root.0, "a.txt", b"a").unwrap();
        let compacted_again = registry.for_session(&root.0, session, Some(11)).unwrap();
        assert!(matches!(
            registry.for_session(&root.0, session, Some(10)),
            Err(ReadAuthorityError::TicketExpired)
        ));
        assert!(matches!(
            registry.for_session(&root.0, session, None),
            Err(ReadAuthorityError::TicketExpired)
        ));
        assert_eq!(
            compacted_again.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::NotVisible
        );
        let restarted = SessionReads::default()
            .for_session(&root.0, session, Some(11))
            .unwrap();
        assert_eq!(
            restarted.check(&root.0, "a.txt", b"a").unwrap(),
            ReadVerdict::Unread
        );
    }
}
