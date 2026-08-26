//! Direct-checkout view for interactive single-writer use.
//!
//! Reads and writes go only through [`RepoPath`] plus a confined root
//! resolver. Pre-existing user bytes are hashed and never replaced unless
//! the caller supplies a matching preimage. Mutations are journaled and
//! can be snapshotted as checkpoints.

use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use protocol::{ArtifactId, RepoPath, RepoPathError};

use crate::view::{CancellationToken, WorkspaceBackend, WorkspaceState, WorkspaceView};

/// Maximum bytes accepted for one file read, write, or checkpoint copy.
pub const MAX_DIRECT_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum journaled mutations retained by one backend.
pub const MAX_JOURNAL_ENTRIES: usize = 4096;

/// Maximum checkpoints retained by one backend.
pub const MAX_CHECKPOINTS: usize = 256;

/// Maximum UTF-8 bytes accepted in a checkpoint label.
pub const MAX_CHECKPOINT_LABEL_BYTES: usize = 128;

/// Maximum symlink hops followed while resolving one path.
pub const MAX_SYMLINK_HOPS: usize = 32;

const CANCEL_STRIDE: usize = 8;
const TMP_PREFIX: &str = ".rapidlm-direct-";
const TMP_SUFFIX: &str = ".tmp";

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);
static OPEN_ROOTS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// How [`DirectBackend::resolve`] treats a missing leaf and a leaf symlink.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DirectResolveMode {
    Read,
    Write,
    Delete,
}

/// Open flags for a direct checkout. Default is fail-closed (not interactive).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectOptions {
    interactive: bool,
    max_file_bytes: usize,
    max_journal_entries: usize,
    max_checkpoints: usize,
}

/// Confined host path produced from a [`RepoPath`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRepoPath {
    logical: RepoPath,
    host: PathBuf,
    existed: bool,
}

/// One journaled mutation. Payloads are content hashes, not file bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalEntry {
    seq: u64,
    path: RepoPath,
    op: JournalOp,
    before: Option<ArtifactId>,
    after: Option<ArtifactId>,
}

/// Journaled mutation class.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum JournalOp {
    Write,
    Delete,
}

/// Snapshot of journaled file bytes at a journal sequence.
#[derive(Clone, Eq, PartialEq)]
pub struct DirectCheckpoint {
    id: u64,
    label: String,
    journal_seq: u64,
    files: BTreeMap<String, Option<CheckpointBlob>>,
}

/// Interactive single-writer checkout bound to one [`WorkspaceView`].
pub struct DirectBackend {
    root: PathBuf,
    view: WorkspaceView,
    interactive: bool,
    max_file_bytes: usize,
    max_journal_entries: usize,
    max_checkpoints: usize,
    inner: Mutex<Inner>,
    _lease: RootLease,
}

/// Typed direct-backend failure. Display never echoes path or file bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectError {
    Cancelled,
    NotInteractive,
    WrongBackend,
    ReadOnlyView,
    InvalidState,
    AlreadyOpen,
    InvalidRoot,
    PathEscape,
    UnresolvedPath,
    UnresolvedParent,
    NotFound,
    NotAFile,
    NotADirectory,
    GitScopeRequired,
    OutOfScope,
    PreexistingChange,
    PreimageMismatch,
    BoundExceeded,
    JournalLimit,
    CheckpointLimit,
    InvalidLabel,
    CheckpointNotFound,
    SymlinkLoop,
    Io,
    LockPoisoned,
}

struct Inner {
    journal: Vec<JournalEntry>,
    checkpoints: Vec<DirectCheckpoint>,
    tracked: BTreeMap<String, Option<TrackedBlob>>,
    next_seq: u64,
    next_checkpoint: u64,
}

#[derive(Clone, Eq, PartialEq)]
struct TrackedBlob {
    hash: ArtifactId,
    bytes: Vec<u8>,
}

#[derive(Clone, Eq, PartialEq)]
struct CheckpointBlob {
    hash: ArtifactId,
    bytes: Vec<u8>,
}

struct RootLease {
    root: PathBuf,
}

struct WalkRules {
    missing_ok: bool,
    follow_last: bool,
}

impl DirectOptions {
    /// Interactive single-writer defaults.
    pub fn interactive() -> Self {
        Self {
            interactive: true,
            max_file_bytes: MAX_DIRECT_FILE_BYTES,
            max_journal_entries: MAX_JOURNAL_ENTRIES,
            max_checkpoints: MAX_CHECKPOINTS,
        }
    }

    pub fn with_interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    pub fn with_max_file_bytes(mut self, max_file_bytes: usize) -> Self {
        self.max_file_bytes = max_file_bytes;
        self
    }

    pub fn with_max_journal_entries(mut self, max_journal_entries: usize) -> Self {
        self.max_journal_entries = max_journal_entries;
        self
    }

    pub fn with_max_checkpoints(mut self, max_checkpoints: usize) -> Self {
        self.max_checkpoints = max_checkpoints;
        self
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    pub fn max_file_bytes(&self) -> usize {
        self.max_file_bytes
    }
}

impl Default for DirectOptions {
    fn default() -> Self {
        Self {
            interactive: false,
            max_file_bytes: MAX_DIRECT_FILE_BYTES,
            max_journal_entries: MAX_JOURNAL_ENTRIES,
            max_checkpoints: MAX_CHECKPOINTS,
        }
    }
}

impl DirectResolveMode {
    fn rules(self) -> WalkRules {
        match self {
            Self::Read => WalkRules {
                missing_ok: false,
                follow_last: true,
            },
            Self::Write => WalkRules {
                missing_ok: true,
                follow_last: true,
            },
            Self::Delete => WalkRules {
                missing_ok: false,
                follow_last: false,
            },
        }
    }

    fn is_mutating(self) -> bool {
        !matches!(self, Self::Read)
    }
}

impl ResolvedRepoPath {
    pub fn logical(&self) -> &RepoPath {
        &self.logical
    }

    pub fn host(&self) -> &Path {
        &self.host
    }

    pub fn existed(&self) -> bool {
        self.existed
    }
}

impl JournalEntry {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn op(&self) -> JournalOp {
        self.op
    }

    pub fn before(&self) -> Option<ArtifactId> {
        self.before
    }

    pub fn after(&self) -> Option<ArtifactId> {
        self.after
    }
}

impl DirectCheckpoint {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn journal_seq(&self) -> u64 {
        self.journal_seq
    }

    pub fn path_count(&self) -> usize {
        self.files.len()
    }
}

impl fmt::Debug for DirectCheckpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectCheckpoint")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("journal_seq", &self.journal_seq)
            .field("path_count", &self.files.len())
            .finish()
    }
}

impl DirectBackend {
    /// Open an interactive direct checkout. The view must use the direct backend.
    pub fn open(
        root: impl AsRef<Path>,
        view: WorkspaceView,
        cancel: &CancellationToken,
    ) -> Result<Self, DirectError> {
        Self::open_with(root, view, DirectOptions::interactive(), cancel)
    }

    pub fn open_with(
        root: impl AsRef<Path>,
        view: WorkspaceView,
        options: DirectOptions,
        cancel: &CancellationToken,
    ) -> Result<Self, DirectError> {
        cancel.check().map_err(|_| DirectError::Cancelled)?;
        if view.backend() != WorkspaceBackend::Direct {
            return Err(DirectError::WrongBackend);
        }
        if options.max_file_bytes == 0
            || options.max_journal_entries == 0
            || options.max_checkpoints == 0
        {
            return Err(DirectError::BoundExceeded);
        }
        let canonical = canonicalize_root(root.as_ref())?;
        cancel.check().map_err(|_| DirectError::Cancelled)?;
        let lease = RootLease::acquire(canonical.clone())?;
        Ok(Self {
            root: canonical,
            view,
            interactive: options.interactive,
            max_file_bytes: options.max_file_bytes,
            max_journal_entries: options.max_journal_entries,
            max_checkpoints: options.max_checkpoints,
            inner: Mutex::new(Inner {
                journal: Vec::new(),
                checkpoints: Vec::new(),
                tracked: BTreeMap::new(),
                next_seq: 1,
                next_checkpoint: 1,
            }),
            _lease: lease,
        })
    }

    pub fn view(&self) -> &WorkspaceView {
        &self.view
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    /// Resolve `path` under the checkout root. Escapes fail closed.
    pub fn resolve(
        &self,
        path: &RepoPath,
        mode: DirectResolveMode,
        cancel: &CancellationToken,
    ) -> Result<ResolvedRepoPath, DirectError> {
        cancel.check().map_err(|_| DirectError::Cancelled)?;
        self.ensure_in_scope(path)?;
        resolve_under_root(&self.root, path, mode, cancel)
    }

    pub fn read(
        &self,
        path: &RepoPath,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, DirectError> {
        cancel.check().map_err(|_| DirectError::Cancelled)?;
        self.ensure_in_scope(path)?;
        let _guard = self.lock()?;
        let resolved = resolve_under_root(&self.root, path, DirectResolveMode::Read, cancel)?;
        read_confined(&self.root, &resolved.host, self.max_file_bytes)
    }

    /// Write `bytes` at `path`.
    ///
    /// Existing user content is replaced only when `expected` matches the
    /// current disk hash, or when the disk still matches the last journaled
    /// hash for that path. Same-bytes writes are a no-op.
    pub fn write(
        &self,
        path: &RepoPath,
        bytes: &[u8],
        expected: Option<&ArtifactId>,
        cancel: &CancellationToken,
    ) -> Result<Option<JournalEntry>, DirectError> {
        cancel.check().map_err(|_| DirectError::Cancelled)?;
        self.ensure_writable()?;
        self.ensure_in_scope(path)?;
        if bytes.len() > self.max_file_bytes {
            return Err(DirectError::BoundExceeded);
        }
        let mut inner = self.lock()?;
        let resolved = resolve_under_root(&self.root, path, DirectResolveMode::Write, cancel)?;
        reject_git_mutation(resolved.logical())?;
        let new_hash = ArtifactId::from_bytes(bytes);
        let disk = disk_blob(&self.root, &resolved, self.max_file_bytes)?;
        let key = resolved.logical().as_str();
        let tracked = inner.tracked.get(key).cloned();
        match decide_write(disk.as_ref(), tracked.as_ref(), expected, new_hash)? {
            WriteDecision::Noop => Ok(None),
            WriteDecision::Apply { before } => {
                reserve_journal(&inner, self.max_journal_entries)?;
                write_confined(&self.root, &resolved.host, bytes)?;
                let entry = push_journal(
                    &mut inner,
                    resolved.logical().clone(),
                    JournalOp::Write,
                    before,
                    Some(new_hash),
                    self.max_journal_entries,
                )?;
                inner.tracked.insert(
                    key.to_owned(),
                    Some(TrackedBlob {
                        hash: new_hash,
                        bytes: bytes.to_vec(),
                    }),
                );
                Ok(Some(entry))
            }
        }
    }

    pub fn delete(
        &self,
        path: &RepoPath,
        expected: Option<&ArtifactId>,
        cancel: &CancellationToken,
    ) -> Result<JournalEntry, DirectError> {
        cancel.check().map_err(|_| DirectError::Cancelled)?;
        self.ensure_writable()?;
        self.ensure_in_scope(path)?;
        let mut inner = self.lock()?;
        let resolved = resolve_under_root(&self.root, path, DirectResolveMode::Delete, cancel)?;
        reject_git_mutation(resolved.logical())?;
        let disk = disk_blob(&self.root, &resolved, self.max_file_bytes)?;
        let Some(disk) = disk else {
            return Err(DirectError::NotFound);
        };
        let key = resolved.logical().as_str();
        let tracked = inner.tracked.get(key).cloned();
        let before = decide_delete(&disk, tracked.as_ref(), expected)?;
        reserve_journal(&inner, self.max_journal_entries)?;
        delete_confined(&self.root, &resolved.host)?;
        let entry = push_journal(
            &mut inner,
            resolved.logical().clone(),
            JournalOp::Delete,
            Some(before),
            None,
            self.max_journal_entries,
        )?;
        inner.tracked.insert(key.to_owned(), None);
        Ok(entry)
    }

    /// True when disk bytes differ from the last journaled state (or exist
    /// and have never been journaled).
    pub fn has_preexisting_change(
        &self,
        path: &RepoPath,
        cancel: &CancellationToken,
    ) -> Result<bool, DirectError> {
        cancel.check().map_err(|_| DirectError::Cancelled)?;
        self.ensure_in_scope(path)?;
        let inner = self.lock()?;
        let resolved = match resolve_under_root(&self.root, path, DirectResolveMode::Read, cancel) {
            Ok(resolved) => resolved,
            Err(DirectError::NotFound | DirectError::UnresolvedPath) => {
                let tracked = inner.tracked.get(path.as_str());
                return Ok(matches!(tracked, Some(Some(_))));
            }
            Err(err) => return Err(err),
        };
        let disk = disk_blob(&self.root, &resolved, self.max_file_bytes)?;
        let tracked = inner.tracked.get(resolved.logical().as_str());
        Ok(match (disk, tracked) {
            (None, Some(Some(_))) => true,
            (None, _) => false,
            (Some(_), None) => true,
            (Some(_disk), Some(None)) => true,
            (Some(disk), Some(Some(tracked))) => disk.hash != tracked.hash,
        })
    }

    pub fn checkpoint(
        &self,
        label: impl AsRef<str>,
        cancel: &CancellationToken,
    ) -> Result<DirectCheckpoint, DirectError> {
        cancel.check().map_err(|_| DirectError::Cancelled)?;
        if !self.interactive {
            return Err(DirectError::NotInteractive);
        }
        let label = parse_label(label.as_ref())?;
        let mut inner = self.lock()?;
        cancel.check().map_err(|_| DirectError::Cancelled)?;
        if inner.checkpoints.len() >= self.max_checkpoints {
            return Err(DirectError::CheckpointLimit);
        }
        let files = inner
            .tracked
            .iter()
            .map(|(path, blob)| {
                let copy = blob.as_ref().map(|tracked| CheckpointBlob {
                    hash: tracked.hash,
                    bytes: tracked.bytes.clone(),
                });
                (path.clone(), copy)
            })
            .collect();
        let checkpoint = DirectCheckpoint {
            id: inner.next_checkpoint,
            label,
            journal_seq: inner.next_seq.saturating_sub(1),
            files,
        };
        inner.next_checkpoint += 1;
        inner.checkpoints.push(checkpoint.clone());
        Ok(checkpoint)
    }

    /// Restore journaled files to `checkpoint`. Unrelated user files are left
    /// untouched. Newer external edits of journaled paths refuse restore.
    pub fn restore_checkpoint(
        &self,
        checkpoint_id: u64,
        cancel: &CancellationToken,
    ) -> Result<DirectCheckpoint, DirectError> {
        cancel.check().map_err(|_| DirectError::Cancelled)?;
        self.ensure_writable()?;
        let mut inner = self.lock()?;
        let index = inner
            .checkpoints
            .iter()
            .position(|item| item.id == checkpoint_id)
            .ok_or(DirectError::CheckpointNotFound)?;
        let snapshot = inner.checkpoints[index].clone();
        let mut keys: HashSet<String> = inner.tracked.keys().cloned().collect();
        keys.extend(snapshot.files.keys().cloned());
        for key in &keys {
            let path = RepoPath::parse(key).map_err(map_repo_path)?;
            self.ensure_in_scope(&path)?;
        }
        for key in &keys {
            cancel.check().map_err(|_| DirectError::Cancelled)?;
            let path = RepoPath::parse(key).map_err(map_repo_path)?;
            let resolved =
                match resolve_under_root(&self.root, &path, DirectResolveMode::Write, cancel) {
                    Ok(resolved) => resolved,
                    Err(DirectError::UnresolvedParent)
                        if snapshot.files.get(key) == Some(&None) =>
                    {
                        continue;
                    }
                    Err(err) => return Err(err),
                };
            let disk = disk_blob(&self.root, &resolved, self.max_file_bytes)?;
            let expected = inner.tracked.get(key).and_then(|blob| blob.as_ref());
            match (disk.as_ref(), expected) {
                (None, _) => {}
                (Some(disk), Some(tracked)) if disk.hash == tracked.hash => {}
                (Some(_), _) => return Err(DirectError::PreexistingChange),
            }
        }
        for key in &keys {
            cancel.check().map_err(|_| DirectError::Cancelled)?;
            let path = RepoPath::parse(key).map_err(map_repo_path)?;
            let target = snapshot.files.get(key);
            match target {
                Some(Some(blob)) => {
                    let resolved =
                        resolve_under_root(&self.root, &path, DirectResolveMode::Write, cancel)?;
                    reject_git_mutation(resolved.logical())?;
                    write_confined(&self.root, &resolved.host, &blob.bytes)?;
                }
                Some(None) | None => {
                    let resolved = match resolve_under_root(
                        &self.root,
                        &path,
                        DirectResolveMode::Delete,
                        cancel,
                    ) {
                        Ok(resolved) => resolved,
                        Err(DirectError::NotFound | DirectError::UnresolvedPath) => continue,
                        Err(err) => return Err(err),
                    };
                    if resolved.existed() {
                        delete_confined(&self.root, &resolved.host)?;
                    }
                }
            }
        }
        inner.tracked = snapshot
            .files
            .iter()
            .map(|(path, blob)| {
                (
                    path.clone(),
                    blob.as_ref().map(|item| TrackedBlob {
                        hash: item.hash,
                        bytes: item.bytes.clone(),
                    }),
                )
            })
            .collect();
        inner
            .journal
            .retain(|entry| entry.seq <= snapshot.journal_seq);
        inner.next_seq = snapshot.journal_seq.saturating_add(1);
        inner.checkpoints.truncate(index + 1);
        Ok(snapshot)
    }

    pub fn journal(&self) -> Result<Vec<JournalEntry>, DirectError> {
        Ok(self.lock()?.journal.clone())
    }

    pub fn checkpoints(&self) -> Result<Vec<DirectCheckpoint>, DirectError> {
        Ok(self.lock()?.checkpoints.clone())
    }

    fn ensure_writable(&self) -> Result<(), DirectError> {
        if !self.interactive {
            return Err(DirectError::NotInteractive);
        }
        if !self.view.access().is_writable() {
            return Err(DirectError::ReadOnlyView);
        }
        if self.view.state() != WorkspaceState::Active {
            return Err(DirectError::InvalidState);
        }
        Ok(())
    }

    fn ensure_in_scope(&self, path: &RepoPath) -> Result<(), DirectError> {
        if self.view.scope().contains(path) {
            Ok(())
        } else {
            Err(DirectError::OutOfScope)
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, DirectError> {
        self.inner.lock().map_err(|_| DirectError::LockPoisoned)
    }
}

impl fmt::Debug for DirectBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectBackend")
            .field("root", &self.root)
            .field("view_id", &self.view.id())
            .field("interactive", &self.interactive)
            .finish()
    }
}

impl Drop for RootLease {
    fn drop(&mut self) {
        if let Ok(mut roots) = open_roots().lock() {
            roots.remove(&self.root);
        }
    }
}

impl RootLease {
    fn acquire(root: PathBuf) -> Result<Self, DirectError> {
        let mut roots = open_roots().lock().map_err(|_| DirectError::LockPoisoned)?;
        if !roots.insert(root.clone()) {
            return Err(DirectError::AlreadyOpen);
        }
        Ok(Self { root })
    }
}

impl fmt::Display for JournalOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Write => "write",
            Self::Delete => "delete",
        })
    }
}

impl fmt::Display for DirectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "direct workspace operation cancelled",
            Self::NotInteractive => "direct checkout writes require interactive mode",
            Self::WrongBackend => "workspace view is not a direct checkout",
            Self::ReadOnlyView => "direct checkout view is read-only",
            Self::InvalidState => "direct checkout view is not writable in this state",
            Self::AlreadyOpen => "direct checkout is already open for a writer",
            Self::InvalidRoot => "direct checkout root is invalid",
            Self::PathEscape => "resolved path escapes the checkout root",
            Self::UnresolvedPath => "direct checkout path could not be resolved",
            Self::UnresolvedParent => "direct checkout parent path could not be resolved",
            Self::NotFound => "direct checkout path not found",
            Self::NotAFile => "direct checkout path is not a regular file",
            Self::NotADirectory => "direct checkout parent is not a directory",
            Self::GitScopeRequired => "mutating .git requires a dedicated git capability",
            Self::OutOfScope => "path is outside the workspace view scope",
            Self::PreexistingChange => "pre-existing user change would be overwritten",
            Self::PreimageMismatch => "direct checkout preimage does not match disk",
            Self::BoundExceeded => "direct checkout resource bound exceeded",
            Self::JournalLimit => "direct checkout journal limit reached",
            Self::CheckpointLimit => "direct checkout checkpoint limit reached",
            Self::InvalidLabel => "direct checkout checkpoint label is invalid",
            Self::CheckpointNotFound => "direct checkout checkpoint not found",
            Self::SymlinkLoop => "direct checkout symlink loop",
            Self::Io => "direct checkout I/O failed",
            Self::LockPoisoned => "direct checkout lock poisoned",
        })
    }
}

impl Error for DirectError {}

enum WriteDecision {
    Noop,
    Apply { before: Option<ArtifactId> },
}

fn decide_write(
    disk: Option<&TrackedBlob>,
    tracked: Option<&Option<TrackedBlob>>,
    expected: Option<&ArtifactId>,
    new_hash: ArtifactId,
) -> Result<WriteDecision, DirectError> {
    match disk {
        None => {
            if expected.is_some() {
                return Err(DirectError::PreimageMismatch);
            }
            if let Some(Some(_)) = tracked {
                return Err(DirectError::PreexistingChange);
            }
            Ok(WriteDecision::Apply { before: None })
        }
        Some(disk) if disk.hash == new_hash => Ok(WriteDecision::Noop),
        Some(disk) => {
            let owned = matches!(tracked, Some(Some(t)) if t.hash == disk.hash);
            if owned {
                if expected.is_some_and(|exp| *exp != disk.hash) {
                    return Err(DirectError::PreimageMismatch);
                }
                return Ok(WriteDecision::Apply {
                    before: Some(disk.hash),
                });
            }
            match expected {
                Some(exp) if *exp == disk.hash => Ok(WriteDecision::Apply {
                    before: Some(disk.hash),
                }),
                Some(_) => Err(DirectError::PreimageMismatch),
                None => Err(DirectError::PreexistingChange),
            }
        }
    }
}

fn decide_delete(
    disk: &TrackedBlob,
    tracked: Option<&Option<TrackedBlob>>,
    expected: Option<&ArtifactId>,
) -> Result<ArtifactId, DirectError> {
    let owned = matches!(tracked, Some(Some(t)) if t.hash == disk.hash);
    if owned {
        if expected.is_some_and(|exp| *exp != disk.hash) {
            return Err(DirectError::PreimageMismatch);
        }
        return Ok(disk.hash);
    }
    match expected {
        Some(exp) if *exp == disk.hash => Ok(disk.hash),
        Some(_) => Err(DirectError::PreimageMismatch),
        None => Err(DirectError::PreexistingChange),
    }
}

fn reserve_journal(inner: &Inner, limit: usize) -> Result<(), DirectError> {
    if inner.journal.len() >= limit {
        Err(DirectError::JournalLimit)
    } else {
        Ok(())
    }
}

fn push_journal(
    inner: &mut Inner,
    path: RepoPath,
    op: JournalOp,
    before: Option<ArtifactId>,
    after: Option<ArtifactId>,
    limit: usize,
) -> Result<JournalEntry, DirectError> {
    reserve_journal(inner, limit)?;
    let entry = JournalEntry {
        seq: inner.next_seq,
        path,
        op,
        before,
        after,
    };
    inner.next_seq += 1;
    inner.journal.push(entry.clone());
    Ok(entry)
}

pub(crate) fn canonicalize_root(root: &Path) -> Result<PathBuf, DirectError> {
    if root.as_os_str().is_empty() {
        return Err(DirectError::InvalidRoot);
    }
    let meta = fs::symlink_metadata(root).map_err(|_| DirectError::InvalidRoot)?;
    if !meta.is_dir() {
        return Err(DirectError::InvalidRoot);
    }
    let canonical = fs::canonicalize(root).map_err(|_| DirectError::InvalidRoot)?;
    if !canonical.is_dir() {
        return Err(DirectError::InvalidRoot);
    }
    Ok(canonical)
}

pub(crate) fn resolve_under_root(
    root: &Path,
    path: &RepoPath,
    mode: DirectResolveMode,
    cancel: &CancellationToken,
) -> Result<ResolvedRepoPath, DirectError> {
    cancel.check().map_err(|_| DirectError::Cancelled)?;
    let rules = mode.rules();
    let mut current = root.to_path_buf();
    let mut hops = 0usize;
    let components: Vec<&str> = path.components().collect();
    if components.is_empty() {
        return Err(DirectError::UnresolvedPath);
    }
    let mut i = 0usize;
    while i < components.len() {
        if i.is_multiple_of(CANCEL_STRIDE) {
            cancel.check().map_err(|_| DirectError::Cancelled)?;
        }
        let name = components[i];
        if name == ".." {
            return Err(DirectError::PathEscape);
        }
        if is_git_component(name) && mode.is_mutating() {
            return Err(DirectError::GitScopeRequired);
        }
        let last = i + 1 == components.len();
        let candidate = current.join(name);
        confine(&candidate, root)?;
        let meta = match fs::symlink_metadata(&candidate) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if last && rules.missing_ok {
                    if !is_real_dir(&current)? {
                        return Err(DirectError::NotADirectory);
                    }
                    confine(&current, root)?;
                    let logical = relativize(&candidate, root)?;
                    return Ok(ResolvedRepoPath {
                        logical,
                        host: candidate,
                        existed: false,
                    });
                }
                return Err(if last {
                    if mode == DirectResolveMode::Read {
                        DirectError::NotFound
                    } else {
                        DirectError::UnresolvedPath
                    }
                } else {
                    DirectError::UnresolvedParent
                });
            }
            Err(_) => return Err(DirectError::Io),
        };
        if meta.file_type().is_symlink() {
            if last && !rules.follow_last {
                let logical = relativize(&candidate, root)?;
                return Ok(ResolvedRepoPath {
                    logical,
                    host: candidate,
                    existed: true,
                });
            }
            hops += 1;
            if hops > MAX_SYMLINK_HOPS {
                return Err(DirectError::SymlinkLoop);
            }
            let target = fs::read_link(&candidate).map_err(|_| DirectError::Io)?;
            let next = if target.is_absolute() {
                target
            } else {
                current.join(target)
            };
            let followed = match fs::canonicalize(&next) {
                Ok(canon) => canon,
                Err(_) if last && rules.missing_ok => {
                    confine_logical(&next, root)?;
                    return Err(DirectError::PathEscape);
                }
                Err(_) => return Err(DirectError::UnresolvedPath),
            };
            confine(&followed, root)?;
            if last {
                let logical = relativize(&followed, root)?;
                if mode.is_mutating() {
                    reject_git_mutation(&logical)?;
                }
                return Ok(ResolvedRepoPath {
                    logical,
                    host: followed,
                    existed: true,
                });
            }
            if !followed.is_dir() {
                return Err(DirectError::NotADirectory);
            }
            current = followed;
            i += 1;
            continue;
        }
        if last {
            if !meta.is_file() && mode != DirectResolveMode::Delete {
                return Err(DirectError::NotAFile);
            }
            let host = fs::canonicalize(&candidate).map_err(|_| DirectError::Io)?;
            confine(&host, root)?;
            let logical = relativize(&host, root)?;
            if mode.is_mutating() {
                reject_git_mutation(&logical)?;
            }
            return Ok(ResolvedRepoPath {
                logical,
                host,
                existed: true,
            });
        }
        if !meta.is_dir() {
            return Err(DirectError::NotADirectory);
        }
        current = fs::canonicalize(&candidate).map_err(|_| DirectError::Io)?;
        confine(&current, root)?;
        i += 1;
    }
    Err(DirectError::UnresolvedPath)
}

fn disk_blob(
    root: &Path,
    resolved: &ResolvedRepoPath,
    max_file_bytes: usize,
) -> Result<Option<TrackedBlob>, DirectError> {
    if !resolved.existed() {
        return Ok(None);
    }
    let meta = match fs::symlink_metadata(&resolved.host) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(DirectError::Io),
    };
    if meta.file_type().is_symlink() {
        return Ok(None);
    }
    if meta.is_dir() {
        return Err(DirectError::NotAFile);
    }
    let bytes = read_confined(root, &resolved.host, max_file_bytes)?;
    Ok(Some(TrackedBlob {
        hash: ArtifactId::from_bytes(&bytes),
        bytes,
    }))
}

pub(crate) fn read_confined(
    root: &Path,
    host: &Path,
    max_file_bytes: usize,
) -> Result<Vec<u8>, DirectError> {
    let canon = fs::canonicalize(host).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            DirectError::NotFound
        } else {
            DirectError::Io
        }
    })?;
    confine(&canon, root)?;
    let meta = fs::symlink_metadata(&canon).map_err(|_| DirectError::Io)?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(DirectError::NotAFile);
    }
    let size = meta.len();
    if size > max_file_bytes as u64 {
        return Err(DirectError::BoundExceeded);
    }
    let file = File::open(&canon).map_err(|_| DirectError::Io)?;
    let mut bytes = Vec::new();
    file.take(max_file_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| DirectError::Io)?;
    if bytes.len() > max_file_bytes {
        return Err(DirectError::BoundExceeded);
    }
    let again = fs::canonicalize(host).map_err(|_| DirectError::Io)?;
    confine(&again, root)?;
    Ok(bytes)
}

fn write_confined(root: &Path, dest: &Path, bytes: &[u8]) -> Result<(), DirectError> {
    let parent = dest.parent().ok_or(DirectError::UnresolvedParent)?;
    let parent_canon = fs::canonicalize(parent).map_err(|_| DirectError::UnresolvedParent)?;
    confine(&parent_canon, root)?;
    if !is_real_dir(&parent_canon)? {
        return Err(DirectError::NotADirectory);
    }
    let name = dest.file_name().ok_or(DirectError::UnresolvedPath)?;
    let dest_path = parent_canon.join(name);
    confine(&dest_path, root)?;
    if let Ok(meta) = fs::symlink_metadata(&dest_path) {
        if meta.file_type().is_symlink() {
            return Err(DirectError::PathEscape);
        }
        if meta.is_dir() {
            return Err(DirectError::NotAFile);
        }
    }
    let tmp = parent_canon.join(format!(
        "{TMP_PREFIX}{}-{}{TMP_SUFFIX}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    confine(&tmp, root)?;
    let written = (|| -> Result<(), DirectError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|_| DirectError::Io)?;
        let meta = fs::symlink_metadata(&tmp).map_err(|_| DirectError::Io)?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(DirectError::PathEscape);
        }
        file.write_all(bytes).map_err(|_| DirectError::Io)?;
        file.sync_all().map_err(|_| DirectError::Io)?;
        drop(file);
        if !is_real_dir(&parent_canon)? {
            return Err(DirectError::PathEscape);
        }
        confine(
            &fs::canonicalize(&parent_canon).map_err(|_| DirectError::Io)?,
            root,
        )?;
        fs::rename(&tmp, &dest_path).map_err(|_| DirectError::Io)?;
        let final_meta = fs::symlink_metadata(&dest_path).map_err(|_| DirectError::Io)?;
        if final_meta.file_type().is_symlink() {
            return Err(DirectError::PathEscape);
        }
        let final_canon = fs::canonicalize(&dest_path).map_err(|_| DirectError::Io)?;
        confine(&final_canon, root)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

fn delete_confined(root: &Path, host: &Path) -> Result<(), DirectError> {
    let parent = host.parent().ok_or(DirectError::UnresolvedParent)?;
    let parent_canon = fs::canonicalize(parent).map_err(|_| DirectError::Io)?;
    confine(&parent_canon, root)?;
    if !is_real_dir(&parent_canon)? {
        return Err(DirectError::PathEscape);
    }
    let name = host.file_name().ok_or(DirectError::UnresolvedPath)?;
    let dest = parent_canon.join(name);
    confine(&dest, root)?;
    let meta = fs::symlink_metadata(&dest).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            DirectError::NotFound
        } else {
            DirectError::Io
        }
    })?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        return Err(DirectError::NotAFile);
    }
    fs::remove_file(&dest).map_err(|_| DirectError::Io)?;
    Ok(())
}

fn confine(path: &Path, root: &Path) -> Result<(), DirectError> {
    if path == root {
        return Err(DirectError::PathEscape);
    }
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(DirectError::PathEscape)
    }
}

fn confine_logical(path: &Path, root: &Path) -> Result<(), DirectError> {
    if path == root || path.starts_with(root) {
        Ok(())
    } else {
        Err(DirectError::PathEscape)
    }
}

fn relativize(path: &Path, root: &Path) -> Result<RepoPath, DirectError> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| DirectError::PathEscape)?;
    let mut logical = String::new();
    for component in rel.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(DirectError::PathEscape);
        };
        let part = part.to_str().ok_or(DirectError::UnresolvedPath)?;
        if !logical.is_empty() {
            logical.push('/');
        }
        logical.push_str(part);
    }
    if logical.is_empty() {
        return Err(DirectError::PathEscape);
    }
    RepoPath::parse(&logical).map_err(map_repo_path)
}

fn is_real_dir(path: &Path) -> Result<bool, DirectError> {
    let meta = fs::symlink_metadata(path).map_err(|_| DirectError::Io)?;
    Ok(meta.is_dir() && !meta.file_type().is_symlink())
}

fn is_git_component(part: &str) -> bool {
    part.eq_ignore_ascii_case(".git")
}

fn reject_git_mutation(path: &RepoPath) -> Result<(), DirectError> {
    if path.components().any(is_git_component) {
        Err(DirectError::GitScopeRequired)
    } else {
        Ok(())
    }
}

fn parse_label(raw: &str) -> Result<String, DirectError> {
    if raw.is_empty() || raw.len() > MAX_CHECKPOINT_LABEL_BYTES {
        return Err(DirectError::InvalidLabel);
    }
    if raw.contains('\0') || raw.chars().any(char::is_control) {
        return Err(DirectError::InvalidLabel);
    }
    Ok(raw.to_owned())
}

fn map_repo_path(err: RepoPathError) -> DirectError {
    match err {
        RepoPathError::Empty => DirectError::UnresolvedPath,
        RepoPathError::TooLong => DirectError::BoundExceeded,
        RepoPathError::Nul | RepoPathError::Control => DirectError::UnresolvedPath,
        RepoPathError::Absolute | RepoPathError::WindowsDrive | RepoPathError::Unc => {
            DirectError::PathEscape
        }
        RepoPathError::Traversal => DirectError::PathEscape,
    }
}

fn open_roots() -> &'static Mutex<HashSet<PathBuf>> {
    OPEN_ROOTS.get_or_init(|| Mutex::new(HashSet::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{CreateView, ViewAccess, ViewRegistry, ViewScope};
    use protocol::{AgentId, RepoId};
    use std::sync::atomic::AtomicU64;

    static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    struct Fixture {
        dir: PathBuf,
        backend: Option<DirectBackend>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.backend.take();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn repo(path: &str) -> RepoPath {
        RepoPath::parse(path).expect("repo path")
    }

    fn view(access: ViewAccess) -> WorkspaceView {
        view_with_scope(access, ViewScope::repo())
    }

    fn view_with_scope(access: ViewAccess, scope: ViewScope) -> WorkspaceView {
        let registry = ViewRegistry::new();
        let mut spec = CreateView::new(RepoId::new(), WorkspaceBackend::Direct, "base-rev", access)
            .with_scope(scope);
        if access.is_writable() {
            spec = spec.with_write_owner(AgentId::new());
        }
        registry.create(spec, &cancel()).expect("create view")
    }

    fn fixture(access: ViewAccess) -> Fixture {
        fixture_with(access, DirectOptions::interactive())
    }

    fn fixture_with(access: ViewAccess, options: DirectOptions) -> Fixture {
        fixture_scoped(access, options, ViewScope::repo())
    }

    fn fixture_scoped(access: ViewAccess, options: DirectOptions, scope: ViewScope) -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-direct-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("src")).expect("mkdir");
        fs::create_dir_all(dir.join("docs")).expect("mkdir docs");
        fs::write(dir.join("src/lib.rs"), b"fn main() {}\n").expect("seed");
        fs::write(dir.join("docs/readme.md"), b"# docs\n").expect("docs");
        let backend =
            DirectBackend::open_with(&dir, view_with_scope(access, scope), options, &cancel())
                .expect("open backend");
        Fixture {
            dir,
            backend: Some(backend),
        }
    }

    fn backend(fx: &Fixture) -> &DirectBackend {
        fx.backend.as_ref().expect("backend")
    }

    #[test]
    fn writes_and_reads_through_repo_path() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/lib.rs");
        let expected = ArtifactId::from_bytes(b"fn main() {}\n");
        let entry = backend(&fx)
            .write(&path, b"pub fn f() {}\n", Some(&expected), &cancel())
            .expect("write")
            .expect("mutated");
        assert_eq!(entry.op(), JournalOp::Write);
        assert_eq!(entry.path(), &path);
        assert_eq!(
            backend(&fx).read(&path, &cancel()).expect("read"),
            b"pub fn f() {}\n"
        );
        assert_eq!(backend(&fx).journal().expect("journal").len(), 1);
    }

    #[test]
    fn preexisting_user_change_is_not_overwritten() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/lib.rs");
        let err = backend(&fx)
            .write(&path, b"stolen\n", None, &cancel())
            .expect_err("silent overwrite");
        assert_eq!(err, DirectError::PreexistingChange);
        assert_eq!(
            fs::read(fx.dir.join("src/lib.rs")).expect("disk"),
            b"fn main() {}\n"
        );
        assert!(
            backend(&fx)
                .has_preexisting_change(&path, &cancel())
                .expect("detect")
        );
        assert!(backend(&fx).journal().expect("journal").is_empty());
    }

    #[test]
    fn wrong_preimage_does_not_mutate_disk() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/lib.rs");
        let wrong = ArtifactId::from_bytes(b"nope");
        let err = backend(&fx)
            .write(&path, b"new\n", Some(&wrong), &cancel())
            .expect_err("wrong preimage");
        assert_eq!(err, DirectError::PreimageMismatch);
        assert_eq!(
            fs::read(fx.dir.join("src/lib.rs")).expect("disk"),
            b"fn main() {}\n"
        );
    }

    #[test]
    fn external_edit_after_journal_is_detected() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/new.rs");
        backend(&fx)
            .write(&path, b"owned\n", None, &cancel())
            .expect("create")
            .expect("entry");
        fs::write(fx.dir.join("src/new.rs"), b"user edit\n").expect("external");
        assert!(
            backend(&fx)
                .has_preexisting_change(&path, &cancel())
                .expect("detect")
        );
        let err = backend(&fx)
            .write(&path, b"agent again\n", None, &cancel())
            .expect_err("clobber");
        assert_eq!(err, DirectError::PreexistingChange);
        assert_eq!(
            fs::read(fx.dir.join("src/new.rs")).expect("disk"),
            b"user edit\n"
        );
    }

    #[test]
    fn acknowledged_preimage_replaces_user_file() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/lib.rs");
        let expected = ArtifactId::from_bytes(b"fn main() {}\n");
        backend(&fx)
            .write(&path, b"ok\n", Some(&expected), &cancel())
            .expect("ack")
            .expect("entry");
        assert_eq!(backend(&fx).read(&path, &cancel()).expect("read"), b"ok\n");
    }

    #[test]
    fn writes_outside_root_are_rejected() {
        let fx = fixture(ViewAccess::ReadWrite);
        for sample in [
            "../secret",
            "..",
            "src/../../etc/passwd",
            "/etc/passwd",
            r"C:\Windows\System32",
        ] {
            assert!(RepoPath::parse(sample).is_err(), "{sample}");
        }
        let err = backend(&fx)
            .resolve(&repo("src/lib.rs"), DirectResolveMode::Read, &cancel())
            .expect("in-root");
        assert!(err.host().starts_with(backend(&fx).root()));
        assert!(!err.host().starts_with("/etc"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_write_is_rejected() {
        let fx = fixture(ViewAccess::ReadWrite);
        let outside = fx.dir.join("../direct-outside-target");
        fs::write(&outside, b"outside\n").expect("outside");
        std::os::unix::fs::symlink(&outside, fx.dir.join("src/escape")).expect("symlink");
        let err = backend(&fx)
            .write(&repo("src/escape"), b"pwn\n", None, &cancel())
            .expect_err("escape");
        assert_eq!(err, DirectError::PathEscape);
        assert_eq!(fs::read(&outside).expect("outside intact"), b"outside\n");
        let shown = err.to_string();
        assert!(!shown.contains("src/escape"));
        assert!(!shown.contains("direct-outside-target"));
        assert!(!shown.contains("passwd"));
        let _ = fs::remove_file(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn parent_symlink_escape_is_rejected() {
        let fx = fixture(ViewAccess::ReadWrite);
        let outside = std::env::temp_dir().join(format!(
            "rapidlm-direct-out-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&outside).expect("outside dir");
        std::os::unix::fs::symlink(&outside, fx.dir.join("linkdir")).expect("dirlink");
        let err = backend(&fx)
            .write(&repo("linkdir/pwn.txt"), b"nope\n", None, &cancel())
            .expect_err("parent escape");
        assert_eq!(err, DirectError::PathEscape);
        assert!(!outside.join("pwn.txt").exists());
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn git_directory_writes_are_rejected() {
        let fx = fixture(ViewAccess::ReadWrite);
        fs::create_dir_all(fx.dir.join(".git")).expect("git");
        fs::write(fx.dir.join(".git/config"), b"[core]\n").expect("config");
        let err = backend(&fx)
            .write(&repo(".git/config"), b"evil\n", None, &cancel())
            .expect_err("git write");
        assert_eq!(err, DirectError::GitScopeRequired);
        assert_eq!(
            fs::read(fx.dir.join(".git/config")).expect("config"),
            b"[core]\n"
        );
    }

    #[test]
    fn non_interactive_and_read_only_cannot_write() {
        let fx = fixture_with(ViewAccess::ReadWrite, DirectOptions::default());
        let err = backend(&fx)
            .write(&repo("src/new.rs"), b"x\n", None, &cancel())
            .expect_err("non-interactive");
        assert_eq!(err, DirectError::NotInteractive);

        let ro = fixture(ViewAccess::ReadOnly);
        let err = backend(&ro)
            .write(&repo("src/new.rs"), b"x\n", None, &cancel())
            .expect_err("read-only");
        assert_eq!(err, DirectError::ReadOnlyView);
        assert_eq!(
            backend(&ro)
                .read(&repo("src/lib.rs"), &cancel())
                .expect("read"),
            b"fn main() {}\n"
        );
    }

    #[test]
    fn wrong_backend_and_second_open_fail_closed() {
        let registry = ViewRegistry::new();
        let other = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::GitWorktree,
                    "rev",
                    ViewAccess::ReadWrite,
                ),
                &cancel(),
            )
            .expect("other view");
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-direct-wrong-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        let err = DirectBackend::open(&dir, other, &cancel()).expect_err("backend");
        assert_eq!(err, DirectError::WrongBackend);

        let fx = fixture(ViewAccess::ReadWrite);
        let err = DirectBackend::open(&fx.dir, view(ViewAccess::ReadWrite), &cancel())
            .expect_err("already open");
        assert_eq!(err, DirectError::AlreadyOpen);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_restores_journaled_files_only() {
        let fx = fixture(ViewAccess::ReadWrite);
        let created = repo("src/new.rs");
        backend(&fx)
            .write(&created, b"one\n", None, &cancel())
            .expect("create")
            .expect("entry");
        let user = fx.dir.join("notes.txt");
        fs::write(&user, b"keep me\n").expect("user file");
        let snap = backend(&fx)
            .checkpoint("after-create", &cancel())
            .expect("checkpoint");
        backend(&fx)
            .write(&created, b"two\n", None, &cancel())
            .expect("update")
            .expect("entry");
        backend(&fx)
            .write(&repo("src/other.rs"), b"later\n", None, &cancel())
            .expect("later")
            .expect("entry");
        let restored = backend(&fx)
            .restore_checkpoint(snap.id(), &cancel())
            .expect("restore");
        assert_eq!(restored.id(), snap.id());
        assert_eq!(
            backend(&fx).read(&created, &cancel()).expect("read"),
            b"one\n"
        );
        assert!(!fx.dir.join("src/other.rs").exists());
        assert_eq!(fs::read(&user).expect("user"), b"keep me\n");
        assert_eq!(backend(&fx).journal().expect("journal").len(), 1);
    }

    #[test]
    fn restore_refuses_newer_external_edit() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/new.rs");
        backend(&fx)
            .write(&path, b"owned\n", None, &cancel())
            .expect("create")
            .expect("entry");
        let snap = backend(&fx)
            .checkpoint("owned", &cancel())
            .expect("checkpoint");
        fs::write(fx.dir.join("src/new.rs"), b"user changed\n").expect("external");
        let err = backend(&fx)
            .restore_checkpoint(snap.id(), &cancel())
            .expect_err("external");
        assert_eq!(err, DirectError::PreexistingChange);
        assert_eq!(
            fs::read(fx.dir.join("src/new.rs")).expect("disk"),
            b"user changed\n"
        );
    }

    #[test]
    fn cancelled_and_oversized_operations_fail_closed() {
        let fx = fixture(ViewAccess::ReadWrite);
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            backend(&fx).read(&repo("src/lib.rs"), &token),
            Err(DirectError::Cancelled)
        );
        let too_big = vec![b'x'; MAX_DIRECT_FILE_BYTES + 1];
        assert_eq!(
            backend(&fx).write(&repo("src/big.rs"), &too_big, None, &cancel()),
            Err(DirectError::BoundExceeded)
        );
    }

    #[test]
    fn journal_limit_and_invalid_label_fail_closed() {
        let fx = fixture_with(
            ViewAccess::ReadWrite,
            DirectOptions::interactive().with_max_journal_entries(1),
        );
        backend(&fx)
            .write(&repo("src/a.rs"), b"a\n", None, &cancel())
            .expect("first")
            .expect("entry");
        let err = backend(&fx)
            .write(&repo("src/b.rs"), b"b\n", None, &cancel())
            .expect_err("limit");
        assert_eq!(err, DirectError::JournalLimit);
        assert!(!fx.dir.join("src/b.rs").exists());

        let lib = fx.dir.join("src/lib.rs");
        let original = fs::read(&lib).expect("lib");
        let expected = ArtifactId::from_bytes(&original);
        let err = backend(&fx)
            .write(
                &repo("src/lib.rs"),
                b"replaced\n",
                Some(&expected),
                &cancel(),
            )
            .expect_err("limit overwrite");
        assert_eq!(err, DirectError::JournalLimit);
        assert_eq!(fs::read(&lib).expect("lib after"), original);

        let a = fx.dir.join("src/a.rs");
        let err = backend(&fx)
            .delete(&repo("src/a.rs"), None, &cancel())
            .expect_err("limit delete");
        assert_eq!(err, DirectError::JournalLimit);
        assert_eq!(fs::read(&a).expect("a after"), b"a\n");

        assert_eq!(
            backend(&fx).checkpoint("bad\0label", &cancel()),
            Err(DirectError::InvalidLabel)
        );
    }

    #[test]
    fn error_display_does_not_echo_paths_or_contents() {
        for err in [
            DirectError::PathEscape,
            DirectError::PreexistingChange,
            DirectError::PreimageMismatch,
            DirectError::GitScopeRequired,
            DirectError::OutOfScope,
        ] {
            let text = err.to_string();
            for leaked in ["password", "hunter2", "/etc/passwd", "secret", ".."] {
                assert!(!text.contains(leaked), "{text}");
            }
        }
    }

    #[test]
    fn same_bytes_write_is_noop() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/lib.rs");
        let entry = backend(&fx)
            .write(&path, b"fn main() {}\n", None, &cancel())
            .expect("same");
        assert!(entry.is_none());
        assert!(backend(&fx).journal().expect("journal").is_empty());
    }

    #[test]
    fn delete_requires_ack_for_user_file() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/lib.rs");
        assert_eq!(
            backend(&fx).delete(&path, None, &cancel()),
            Err(DirectError::PreexistingChange)
        );
        assert!(fx.dir.join("src/lib.rs").exists());
        let expected = ArtifactId::from_bytes(b"fn main() {}\n");
        backend(&fx)
            .delete(&path, Some(&expected), &cancel())
            .expect("delete");
        assert!(!fx.dir.join("src/lib.rs").exists());
        assert_eq!(
            backend(&fx).journal().expect("journal")[0].op(),
            JournalOp::Delete
        );
    }

    #[test]
    fn view_scope_blocks_out_of_prefix_reads_and_writes() {
        let scope = ViewScope::prefixes(vec![repo("src")]).expect("scope");
        let fx = fixture_scoped(ViewAccess::ReadWrite, DirectOptions::interactive(), scope);
        let in_scope = repo("src/lib.rs");
        let sibling = repo("srcfoo/lib.rs");
        let docs = repo("docs/readme.md");

        assert_eq!(
            backend(&fx).read(&in_scope, &cancel()).expect("in-scope"),
            b"fn main() {}\n"
        );
        assert_eq!(
            backend(&fx).read(&docs, &cancel()).expect_err("docs"),
            DirectError::OutOfScope
        );
        assert_eq!(
            backend(&fx)
                .write(&docs, b"stolen\n", None, &cancel())
                .expect_err("write docs"),
            DirectError::OutOfScope
        );
        assert_eq!(
            backend(&fx)
                .resolve(&sibling, DirectResolveMode::Read, &cancel())
                .expect_err("sibling"),
            DirectError::OutOfScope
        );
        assert_eq!(
            fs::read(fx.dir.join("docs/readme.md")).expect("docs"),
            b"# docs\n"
        );
        backend(&fx)
            .write(&repo("src/new.rs"), b"ok\n", None, &cancel())
            .expect("in-scope create")
            .expect("entry");
        assert_eq!(
            backend(&fx)
                .read(&repo("src/new.rs"), &cancel())
                .expect("read new"),
            b"ok\n"
        );
        assert_eq!(
            DirectError::OutOfScope.to_string(),
            "path is outside the workspace view scope"
        );
        assert!(!DirectError::OutOfScope.to_string().contains("docs"));
    }
}
