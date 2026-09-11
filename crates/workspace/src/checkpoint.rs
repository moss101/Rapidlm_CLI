//! Persist view checkpoints and previewable rewind.
//!
//! Preview never mutates. Apply refuses when a newer external user
//! modification would be overwritten. Files RapidLM never journaled are
//! left untouched.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};

use protocol::{ArtifactId, RepoPath, WorkspaceViewId};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::backends::direct::{
    DirectBackend, DirectError, DirectResolveMode, JournalEntry, MAX_CHECKPOINT_LABEL_BYTES,
    MAX_CHECKPOINTS, MAX_DIRECT_FILE_BYTES,
};
use crate::external_mutation::{MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_FILES};
use crate::view::{CancellationToken, WorkspaceState, WorkspaceView};

/// Wire schema name for [`Checkpoint`].
pub const CHECKPOINT_SCHEMA: &str = "rapidlm.workspace_checkpoint";

/// v1 schema version for workspace checkpoints.
pub const CHECKPOINT_SCHEMA_VERSION: u16 = 1;

/// Wire schema name for [`RewindPreview`].
pub const REWIND_PREVIEW_SCHEMA: &str = "rapidlm.rewind_preview";

/// v1 schema version for rewind previews.
pub const REWIND_PREVIEW_SCHEMA_VERSION: u16 = 1;

/// Maximum checkpoints retained by one manager.
pub const MAX_MANAGER_CHECKPOINTS: usize = MAX_CHECKPOINTS;

/// Maximum journaled files snapshotted into one checkpoint.
pub const MAX_CHECKPOINT_FILES: usize = MAX_SNAPSHOT_FILES;

/// Maximum combined file bytes stored in one checkpoint.
pub const MAX_CHECKPOINT_BYTES: usize = MAX_SNAPSHOT_BYTES;

const CANCEL_STRIDE: usize = 8;
const CHECKPOINT_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "id",
    "view_id",
    "label",
    "journal_seq",
    "snapshot_hash",
    "files",
];
const PREVIEW_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "checkpoint_id",
    "view_id",
    "mode",
    "applied",
    "ops",
    "conflicts",
];

/// Opaque identifier for a persisted view checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct CheckpointId(u64);

/// Whether rewind only reports work or attempts to restore.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RewindMode {
    Preview,
    Apply,
}

/// Restore or delete one journaled path back to the checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RewindOpKind {
    Restore,
    Delete,
}

/// Persisted snapshot of RapidLM-owned files for one view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Checkpoint {
    id: CheckpointId,
    view_id: WorkspaceViewId,
    label: String,
    journal_seq: u64,
    snapshot_hash: ArtifactId,
    files: BTreeMap<RepoPath, Option<ArtifactId>>,
}

/// One non-conflicting change rewind would apply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindOp {
    path: RepoPath,
    kind: RewindOpKind,
    before: Option<ArtifactId>,
    after: Option<ArtifactId>,
}

/// Newer external user modification that would be destroyed by restore.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindConflict {
    path: RepoPath,
    current: Option<ArtifactId>,
    expected: Option<ArtifactId>,
    target: Option<ArtifactId>,
}

/// Preview (and optional apply result) of restoring a checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindPreview {
    checkpoint_id: CheckpointId,
    view_id: WorkspaceViewId,
    mode: RewindMode,
    applied: bool,
    ops: Vec<RewindOp>,
    conflicts: Vec<RewindConflict>,
}

/// In-process checkpoint catalog bound to explicit resource caps.
pub struct CheckpointManager {
    inner: Mutex<Inner>,
    max_checkpoints: usize,
    max_files: usize,
    max_bytes: usize,
}

/// Typed checkpoint/rewind failure. Display never echoes paths or bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckpointError {
    Cancelled,
    ViewMismatch,
    CheckpointNotFound {
        checkpoint_id: CheckpointId,
    },
    ReadOnlyView,
    InvalidState,
    WrongBackend,
    NotInteractive,
    InvalidLabel,
    CheckpointLimit {
        limit: usize,
    },
    BoundExceeded,
    ExternalConflict {
        checkpoint_id: CheckpointId,
        conflicts: usize,
    },
    PathEscape,
    GitScopeRequired,
    Io,
    LockPoisoned,
    UnknownVariant,
    UnsupportedSchema,
    UnsupportedSchemaVersion,
}

struct Inner {
    next_id: u64,
    checkpoints: BTreeMap<CheckpointId, StoredCheckpoint>,
}

struct StoredCheckpoint {
    meta: Checkpoint,
    blobs: BTreeMap<RepoPath, Option<Vec<u8>>>,
}

struct PlannedOp {
    path: RepoPath,
    kind: RewindOpKind,
    before: Option<ArtifactId>,
    after: Option<ArtifactId>,
    previous: Option<Vec<u8>>,
    target: Option<Vec<u8>>,
}

struct CurrentFile {
    hash: Option<ArtifactId>,
    bytes: Option<Vec<u8>>,
}

type SnapshotFiles = BTreeMap<RepoPath, Option<ArtifactId>>;
type SnapshotBlobs = BTreeMap<RepoPath, Option<Vec<u8>>>;

impl CheckpointId {
    pub const fn from_raw(id: u64) -> Self {
        Self(id)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl RewindMode {
    pub const ALL: &'static [Self] = &[Self::Preview, Self::Apply];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Apply => "apply",
        }
    }

    pub const fn mutates(self) -> bool {
        matches!(self, Self::Apply)
    }
}

impl RewindOpKind {
    pub const ALL: &'static [Self] = &[Self::Restore, Self::Delete];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Restore => "restore",
            Self::Delete => "delete",
        }
    }
}

impl Checkpoint {
    pub fn id(&self) -> CheckpointId {
        self.id
    }

    pub fn view_id(&self) -> WorkspaceViewId {
        self.view_id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn journal_seq(&self) -> u64 {
        self.journal_seq
    }

    pub fn snapshot_hash(&self) -> ArtifactId {
        self.snapshot_hash
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn file_hash(&self, path: &RepoPath) -> Option<Option<ArtifactId>> {
        self.files.get(path).copied()
    }

    pub fn files(&self) -> impl Iterator<Item = (&RepoPath, Option<ArtifactId>)> {
        self.files.iter().map(|(path, hash)| (path, *hash))
    }
}

impl RewindOp {
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn kind(&self) -> RewindOpKind {
        self.kind
    }

    pub fn before(&self) -> Option<ArtifactId> {
        self.before
    }

    pub fn after(&self) -> Option<ArtifactId> {
        self.after
    }
}

impl RewindConflict {
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn current(&self) -> Option<ArtifactId> {
        self.current
    }

    pub fn expected(&self) -> Option<ArtifactId> {
        self.expected
    }

    pub fn target(&self) -> Option<ArtifactId> {
        self.target
    }
}

impl RewindPreview {
    pub fn checkpoint_id(&self) -> CheckpointId {
        self.checkpoint_id
    }

    pub fn view_id(&self) -> WorkspaceViewId {
        self.view_id
    }

    pub fn mode(&self) -> RewindMode {
        self.mode
    }

    pub fn applied(&self) -> bool {
        self.applied
    }

    pub fn ops(&self) -> &[RewindOp] {
        &self.ops
    }

    pub fn conflicts(&self) -> &[RewindConflict] {
        &self.conflicts
    }

    pub fn is_safe(&self) -> bool {
        self.conflicts.is_empty()
    }
}

impl CheckpointManager {
    pub fn new() -> Self {
        Self::with_limits(
            MAX_MANAGER_CHECKPOINTS,
            MAX_CHECKPOINT_FILES,
            MAX_CHECKPOINT_BYTES,
        )
    }

    pub fn with_limits(max_checkpoints: usize, max_files: usize, max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                next_id: 1,
                checkpoints: BTreeMap::new(),
            }),
            max_checkpoints,
            max_files,
            max_bytes,
        }
    }

    /// Snapshot RapidLM-owned files for `view`. Unrelated user files are omitted.
    pub fn checkpoint(
        &self,
        view: &WorkspaceView,
        source: &DirectBackend,
        label: impl AsRef<str>,
        cancel: &CancellationToken,
    ) -> Result<Checkpoint, CheckpointError> {
        check_cancel(cancel)?;
        check_view_binding(view, source)?;
        reject_closed(view)?;
        if self.max_checkpoints == 0 || self.max_files == 0 || self.max_bytes == 0 {
            return Err(CheckpointError::BoundExceeded);
        }
        let label = parse_label(label.as_ref())?;
        let journal = source.journal().map_err(map_direct)?;
        check_cancel(cancel)?;
        let (files, blobs) =
            snapshot_owned(source, &journal, self.max_files, self.max_bytes, cancel)?;
        let journal_seq = journal.last().map(JournalEntry::seq).unwrap_or(0);
        let snapshot_hash = hash_files(&files);
        let mut inner = self.lock()?;
        check_cancel(cancel)?;
        if inner.checkpoints.len() >= self.max_checkpoints {
            return Err(CheckpointError::CheckpointLimit {
                limit: self.max_checkpoints,
            });
        }
        let id = CheckpointId(inner.next_id);
        inner.next_id = inner.next_id.saturating_add(1);
        let meta = Checkpoint {
            id,
            view_id: view.id(),
            label,
            journal_seq,
            snapshot_hash,
            files,
        };
        inner.checkpoints.insert(
            id,
            StoredCheckpoint {
                meta: meta.clone(),
                blobs,
            },
        );
        Ok(meta)
    }

    /// Preview or apply restore of `checkpoint` onto `view`.
    ///
    /// Preview never writes. Apply is refused when any newer external user
    /// modification overlaps a path rewind would change.
    pub fn rewind(
        &self,
        view: &WorkspaceView,
        source: &DirectBackend,
        checkpoint: CheckpointId,
        mode: RewindMode,
        cancel: &CancellationToken,
    ) -> Result<RewindPreview, CheckpointError> {
        check_cancel(cancel)?;
        check_view_binding(view, source)?;
        reject_closed(view)?;
        if mode.mutates() {
            ensure_writable(view, source)?;
        }

        let stored = {
            let inner = self.lock()?;
            inner.checkpoints.get(&checkpoint).cloned().ok_or(
                CheckpointError::CheckpointNotFound {
                    checkpoint_id: checkpoint,
                },
            )?
        };
        if stored.meta.view_id != view.id() {
            return Err(CheckpointError::ViewMismatch);
        }

        let journal = source.journal().map_err(map_direct)?;
        check_cancel(cancel)?;
        let (ops, conflicts) = plan_rewind(source, &stored, &journal, cancel)?;
        let preview = RewindPreview {
            checkpoint_id: stored.meta.id,
            view_id: view.id(),
            mode,
            applied: false,
            ops: ops
                .iter()
                .map(|op| RewindOp {
                    path: op.path.clone(),
                    kind: op.kind,
                    before: op.before,
                    after: op.after,
                })
                .collect(),
            conflicts,
        };
        if !preview.conflicts.is_empty() {
            if mode.mutates() {
                return Err(CheckpointError::ExternalConflict {
                    checkpoint_id: stored.meta.id,
                    conflicts: preview.conflicts.len(),
                });
            }
            return Ok(preview);
        }
        if !mode.mutates() {
            return Ok(preview);
        }

        apply_plan(source, ops, cancel)?;
        Ok(RewindPreview {
            applied: true,
            ..preview
        })
    }

    pub fn get(
        &self,
        checkpoint_id: CheckpointId,
        cancel: &CancellationToken,
    ) -> Result<Checkpoint, CheckpointError> {
        check_cancel(cancel)?;
        let inner = self.lock()?;
        check_cancel(cancel)?;
        inner
            .checkpoints
            .get(&checkpoint_id)
            .map(|stored| stored.meta.clone())
            .ok_or(CheckpointError::CheckpointNotFound { checkpoint_id })
    }

    pub fn list(
        &self,
        view_id: WorkspaceViewId,
        cancel: &CancellationToken,
    ) -> Result<Vec<Checkpoint>, CheckpointError> {
        check_cancel(cancel)?;
        let inner = self.lock()?;
        check_cancel(cancel)?;
        Ok(inner
            .checkpoints
            .values()
            .filter(|stored| stored.meta.view_id == view_id)
            .map(|stored| stored.meta.clone())
            .collect())
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, CheckpointError> {
        self.inner.lock().map_err(|_| CheckpointError::LockPoisoned)
    }
}

impl Default for CheckpointManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for StoredCheckpoint {
    fn clone(&self) -> Self {
        Self {
            meta: self.meta.clone(),
            blobs: self.blobs.clone(),
        }
    }
}

fn snapshot_owned(
    source: &DirectBackend,
    journal: &[JournalEntry],
    max_files: usize,
    max_bytes: usize,
    cancel: &CancellationToken,
) -> Result<(SnapshotFiles, SnapshotBlobs), CheckpointError> {
    let mut latest: SnapshotFiles = BTreeMap::new();
    for (index, entry) in journal.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        latest.insert(entry.path().clone(), entry.after());
    }
    if latest.len() > max_files {
        return Err(CheckpointError::BoundExceeded);
    }

    let mut files = BTreeMap::new();
    let mut blobs = BTreeMap::new();
    let mut total = 0usize;
    for (index, (path, after)) in latest.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        reject_git_mutation(path)?;
        let current = read_current(source, path, cancel)?;
        let (hash, blob) = match after {
            Some(_) => match current.bytes {
                Some(bytes) => {
                    total = total
                        .checked_add(bytes.len())
                        .ok_or(CheckpointError::BoundExceeded)?;
                    if total > max_bytes {
                        return Err(CheckpointError::BoundExceeded);
                    }
                    (Some(ArtifactId::from_bytes(&bytes)), Some(bytes))
                }
                None => (None, None),
            },
            None => (None, None),
        };
        files.insert(path.clone(), hash);
        blobs.insert(path.clone(), blob);
    }
    Ok((files, blobs))
}

fn plan_rewind(
    source: &DirectBackend,
    stored: &StoredCheckpoint,
    journal: &[JournalEntry],
    cancel: &CancellationToken,
) -> Result<(Vec<PlannedOp>, Vec<RewindConflict>), CheckpointError> {
    let mut last_owned = stored.meta.files.clone();
    for (index, entry) in journal.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        if entry.seq() > stored.meta.journal_seq {
            last_owned.insert(entry.path().clone(), entry.after());
        }
    }

    let mut ops = Vec::new();
    let mut conflicts = Vec::new();
    for (index, (path, expected)) in last_owned.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        reject_git_mutation(path)?;
        reject_leaf_symlink(source, path, cancel)?;
        let current = read_current(source, path, cancel)?;
        let target_blob = stored.blobs.get(path).cloned().flatten();
        let target = target_blob.as_deref().map(ArtifactId::from_bytes);
        if current.hash != *expected && current.hash != target {
            conflicts.push(RewindConflict {
                path: path.clone(),
                current: current.hash,
                expected: *expected,
                target,
            });
            continue;
        }
        if current.hash == target {
            continue;
        }
        let kind = if target.is_some() {
            RewindOpKind::Restore
        } else {
            RewindOpKind::Delete
        };
        ops.push(PlannedOp {
            path: path.clone(),
            kind,
            before: current.hash,
            after: target,
            previous: current.bytes,
            target: target_blob,
        });
    }
    Ok((ops, conflicts))
}

fn apply_plan(
    source: &DirectBackend,
    ops: Vec<PlannedOp>,
    cancel: &CancellationToken,
) -> Result<(), CheckpointError> {
    for (applied, op) in ops.iter().enumerate() {
        if index_cancel(applied, cancel).is_err() {
            rollback_applied(source, &ops[..applied]);
            return Err(CheckpointError::Cancelled);
        }
        if let Err(err) = apply_one(source, op, cancel) {
            rollback_applied(source, &ops[..applied]);
            return Err(err);
        }
    }
    Ok(())
}

fn index_cancel(index: usize, cancel: &CancellationToken) -> Result<(), CheckpointError> {
    if index.is_multiple_of(CANCEL_STRIDE) {
        check_cancel(cancel)
    } else {
        Ok(())
    }
}

fn apply_one(
    source: &DirectBackend,
    op: &PlannedOp,
    cancel: &CancellationToken,
) -> Result<(), CheckpointError> {
    reject_leaf_symlink(source, &op.path, cancel)?;
    match op.kind {
        RewindOpKind::Restore => {
            let bytes = op.target.as_deref().ok_or(CheckpointError::Io)?;
            source
                .write(&op.path, bytes, op.before.as_ref(), cancel)
                .map_err(map_apply_direct)?;
        }
        RewindOpKind::Delete => {
            source
                .delete(&op.path, op.before.as_ref(), cancel)
                .map_err(map_apply_direct)?;
        }
    }
    Ok(())
}

/// Undo an already-applied prefix of a rewind plan. Deliberately uses a
/// fresh, never-cancelled token for every restore/delete rather than the
/// caller's own `cancel` — the one call site this fires from
/// (`apply_plan`'s cancellation branch) is only reached once that token is
/// *already* cancelled, and `DirectBackend::write`/`delete` both check
/// cancellation internally too, so reusing it here would make rollback a
/// guaranteed no-op on the exact path that needs it most: the already-
/// applied ops would stay mixed into the workspace with no indication
/// anything went wrong, since the caller only ever sees `Cancelled`, which
/// (per this module's own "apply is refused..." framing) implies nothing
/// happened. Rollback is a cleanup/compensating action, not discretionary
/// work — it must run to completion regardless of why the forward pass
/// stopped.
fn rollback_applied(source: &DirectBackend, applied: &[PlannedOp]) {
    let cancel = CancellationToken::new();
    for op in applied.iter().rev() {
        let _ = match (&op.previous, op.before) {
            (Some(bytes), _) => source.write(&op.path, bytes, op.after.as_ref(), &cancel),
            (None, _) => match source.delete(&op.path, op.after.as_ref(), &cancel) {
                Ok(_) => Ok(None),
                Err(DirectError::NotFound | DirectError::UnresolvedPath) => Ok(None),
                Err(err) => Err(err),
            },
        };
    }
}

fn read_current(
    source: &DirectBackend,
    path: &RepoPath,
    cancel: &CancellationToken,
) -> Result<CurrentFile, CheckpointError> {
    reject_leaf_symlink(source, path, cancel)?;
    match source.read(path, cancel) {
        Ok(bytes) => {
            if bytes.len() > MAX_DIRECT_FILE_BYTES {
                return Err(CheckpointError::BoundExceeded);
            }
            Ok(CurrentFile {
                hash: Some(ArtifactId::from_bytes(&bytes)),
                bytes: Some(bytes),
            })
        }
        Err(
            DirectError::NotFound | DirectError::UnresolvedPath | DirectError::UnresolvedParent,
        ) => Ok(CurrentFile {
            hash: None,
            bytes: None,
        }),
        Err(err) => Err(map_direct(err)),
    }
}

fn hash_files(files: &BTreeMap<RepoPath, Option<ArtifactId>>) -> ArtifactId {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"rapidlm.workspace_checkpoint.files.v1\n");
    for (path, hash) in files {
        buf.extend_from_slice(path.as_str().as_bytes());
        buf.push(0);
        match hash {
            None => buf.extend_from_slice(b"none\n"),
            Some(hash) => {
                buf.extend_from_slice(hash.to_string().as_bytes());
                buf.push(b'\n');
            }
        }
    }
    ArtifactId::from_bytes(&buf)
}

fn check_view_binding(view: &WorkspaceView, source: &DirectBackend) -> Result<(), CheckpointError> {
    if view.id() != source.view().id() {
        return Err(CheckpointError::ViewMismatch);
    }
    if view.backend() != source.view().backend() {
        return Err(CheckpointError::WrongBackend);
    }
    Ok(())
}

fn reject_closed(view: &WorkspaceView) -> Result<(), CheckpointError> {
    if view.state() == WorkspaceState::Closed {
        Err(CheckpointError::InvalidState)
    } else {
        Ok(())
    }
}

fn ensure_writable(view: &WorkspaceView, source: &DirectBackend) -> Result<(), CheckpointError> {
    if !source.is_interactive() {
        return Err(CheckpointError::NotInteractive);
    }
    if !view.access().is_writable() {
        return Err(CheckpointError::ReadOnlyView);
    }
    if view.state() != WorkspaceState::Active {
        return Err(CheckpointError::InvalidState);
    }
    Ok(())
}

fn parse_label(raw: &str) -> Result<String, CheckpointError> {
    if raw.is_empty() || raw.len() > MAX_CHECKPOINT_LABEL_BYTES {
        return Err(CheckpointError::InvalidLabel);
    }
    if raw.contains('\0') || raw.chars().any(char::is_control) {
        return Err(CheckpointError::InvalidLabel);
    }
    Ok(raw.to_owned())
}

fn reject_git_mutation(path: &RepoPath) -> Result<(), CheckpointError> {
    if path
        .components()
        .any(|part| part.eq_ignore_ascii_case(".git"))
    {
        Err(CheckpointError::GitScopeRequired)
    } else {
        Ok(())
    }
}

/// Fail closed on a rewind/snapshot leaf symlink, including in-workspace
/// redirects. Never follow the last hop or overwrite the referent.
fn reject_leaf_symlink(
    source: &DirectBackend,
    path: &RepoPath,
    cancel: &CancellationToken,
) -> Result<(), CheckpointError> {
    check_cancel(cancel)?;
    let resolved = match source.resolve(path, DirectResolveMode::Delete, cancel) {
        Ok(resolved) => resolved,
        Err(
            DirectError::NotFound | DirectError::UnresolvedPath | DirectError::UnresolvedParent,
        ) => {
            return Ok(());
        }
        Err(err) => return Err(map_direct(err)),
    };
    let meta = match fs::symlink_metadata(resolved.host()) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(CheckpointError::Io),
    };
    if meta.file_type().is_symlink() {
        Err(CheckpointError::PathEscape)
    } else {
        Ok(())
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), CheckpointError> {
    if cancel.is_cancelled() {
        Err(CheckpointError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_direct(err: DirectError) -> CheckpointError {
    match err {
        DirectError::Cancelled => CheckpointError::Cancelled,
        DirectError::NotInteractive => CheckpointError::NotInteractive,
        DirectError::WrongBackend => CheckpointError::WrongBackend,
        DirectError::ReadOnlyView => CheckpointError::ReadOnlyView,
        DirectError::InvalidState => CheckpointError::InvalidState,
        DirectError::PathEscape => CheckpointError::PathEscape,
        DirectError::GitScopeRequired => CheckpointError::GitScopeRequired,
        DirectError::BoundExceeded | DirectError::JournalLimit | DirectError::CheckpointLimit => {
            CheckpointError::BoundExceeded
        }
        DirectError::PreexistingChange | DirectError::PreimageMismatch => {
            CheckpointError::ExternalConflict {
                checkpoint_id: CheckpointId(0),
                conflicts: 1,
            }
        }
        DirectError::LockPoisoned => CheckpointError::LockPoisoned,
        DirectError::Io => CheckpointError::Io,
        _ => CheckpointError::Io,
    }
}

fn map_apply_direct(err: DirectError) -> CheckpointError {
    match err {
        DirectError::PreexistingChange | DirectError::PreimageMismatch => {
            CheckpointError::ExternalConflict {
                checkpoint_id: CheckpointId(0),
                conflicts: 1,
            }
        }
        other => map_direct(other),
    }
}

impl fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for RewindMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for RewindOpKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "workspace checkpoint operation cancelled",
            Self::ViewMismatch => "workspace checkpoint does not belong to this view",
            Self::CheckpointNotFound { .. } => "workspace checkpoint not found",
            Self::ReadOnlyView => "workspace view is read-only",
            Self::InvalidState => "workspace view is not in a valid state",
            Self::WrongBackend => "workspace view is not a direct checkout",
            Self::NotInteractive => "workspace rewind writes require interactive mode",
            Self::InvalidLabel => "workspace checkpoint label is invalid",
            Self::CheckpointLimit { .. } => "workspace checkpoint limit reached",
            Self::BoundExceeded => "workspace checkpoint resource bound exceeded",
            Self::ExternalConflict { .. } => {
                "newer external user modification refuses destructive restore"
            }
            Self::PathEscape => "resolved path escapes the checkout root",
            Self::GitScopeRequired => "mutating .git requires a dedicated git capability",
            Self::Io => "workspace checkpoint I/O failed",
            Self::LockPoisoned => "workspace checkpoint manager lock poisoned",
            Self::UnknownVariant => "unknown workspace checkpoint enumeration value",
            Self::UnsupportedSchema => "unsupported workspace checkpoint schema",
            Self::UnsupportedSchemaVersion => "unsupported workspace checkpoint schema version",
        })
    }
}

impl Error for CheckpointError {}

impl FromStr for RewindMode {
    type Err = CheckpointError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl FromStr for RewindOpKind {
    type Err = CheckpointError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl Serialize for RewindMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Serialize for RewindOpKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RewindMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl<'de> Deserialize<'de> for RewindOpKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl Serialize for Checkpoint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Checkpoint", CHECKPOINT_FIELDS.len())?;
        state.serialize_field("schema", CHECKPOINT_SCHEMA)?;
        state.serialize_field("schema_version", &CHECKPOINT_SCHEMA_VERSION)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("view_id", &self.view_id)?;
        state.serialize_field("label", &self.label)?;
        state.serialize_field("journal_seq", &self.journal_seq)?;
        state.serialize_field("snapshot_hash", &self.snapshot_hash)?;
        state.serialize_field("files", &self.files)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for Checkpoint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawCheckpoint::deserialize(deserializer)?;
        if raw.schema != CHECKPOINT_SCHEMA {
            return Err(de::Error::custom(CheckpointError::UnsupportedSchema));
        }
        if raw.schema_version != CHECKPOINT_SCHEMA_VERSION {
            return Err(de::Error::custom(CheckpointError::UnsupportedSchemaVersion));
        }
        if raw.files.len() > MAX_CHECKPOINT_FILES {
            return Err(de::Error::custom(CheckpointError::BoundExceeded));
        }
        let label = parse_label(&raw.label).map_err(de::Error::custom)?;
        let snapshot_hash = hash_files(&raw.files);
        if snapshot_hash != raw.snapshot_hash {
            return Err(de::Error::custom(CheckpointError::UnknownVariant));
        }
        Ok(Checkpoint {
            id: raw.id,
            view_id: raw.view_id,
            label,
            journal_seq: raw.journal_seq,
            snapshot_hash,
            files: raw.files,
        })
    }
}

impl Serialize for RewindOp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("RewindOp", 4)?;
        state.serialize_field("path", &self.path)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("before", &self.before)?;
        state.serialize_field("after", &self.after)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for RewindOp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawRewindOp::deserialize(deserializer)?;
        match (raw.kind, raw.before, raw.after) {
            (RewindOpKind::Restore, _, Some(_)) | (RewindOpKind::Delete, Some(_), None) => {}
            _ => return Err(de::Error::custom(CheckpointError::UnknownVariant)),
        }
        Ok(RewindOp {
            path: raw.path,
            kind: raw.kind,
            before: raw.before,
            after: raw.after,
        })
    }
}

impl Serialize for RewindConflict {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("RewindConflict", 4)?;
        state.serialize_field("path", &self.path)?;
        state.serialize_field("current", &self.current)?;
        state.serialize_field("expected", &self.expected)?;
        state.serialize_field("target", &self.target)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for RewindConflict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawRewindConflict::deserialize(deserializer)?;
        Ok(RewindConflict {
            path: raw.path,
            current: raw.current,
            expected: raw.expected,
            target: raw.target,
        })
    }
}

impl Serialize for RewindPreview {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("RewindPreview", PREVIEW_FIELDS.len())?;
        state.serialize_field("schema", REWIND_PREVIEW_SCHEMA)?;
        state.serialize_field("schema_version", &REWIND_PREVIEW_SCHEMA_VERSION)?;
        state.serialize_field("checkpoint_id", &self.checkpoint_id)?;
        state.serialize_field("view_id", &self.view_id)?;
        state.serialize_field("mode", &self.mode)?;
        state.serialize_field("applied", &self.applied)?;
        state.serialize_field("ops", &self.ops)?;
        state.serialize_field("conflicts", &self.conflicts)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for RewindPreview {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawRewindPreview::deserialize(deserializer)?;
        if raw.schema != REWIND_PREVIEW_SCHEMA {
            return Err(de::Error::custom(CheckpointError::UnsupportedSchema));
        }
        if raw.schema_version != REWIND_PREVIEW_SCHEMA_VERSION {
            return Err(de::Error::custom(CheckpointError::UnsupportedSchemaVersion));
        }
        if raw.applied && raw.mode != RewindMode::Apply {
            return Err(de::Error::custom(CheckpointError::UnknownVariant));
        }
        if raw.applied && !raw.conflicts.is_empty() {
            return Err(de::Error::custom(CheckpointError::UnknownVariant));
        }
        Ok(RewindPreview {
            checkpoint_id: raw.checkpoint_id,
            view_id: raw.view_id,
            mode: raw.mode,
            applied: raw.applied,
            ops: raw.ops,
            conflicts: raw.conflicts,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCheckpoint {
    schema: String,
    schema_version: u16,
    id: CheckpointId,
    view_id: WorkspaceViewId,
    label: String,
    journal_seq: u64,
    snapshot_hash: ArtifactId,
    files: BTreeMap<RepoPath, Option<ArtifactId>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRewindOp {
    path: RepoPath,
    kind: RewindOpKind,
    before: Option<ArtifactId>,
    after: Option<ArtifactId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRewindConflict {
    path: RepoPath,
    current: Option<ArtifactId>,
    expected: Option<ArtifactId>,
    target: Option<ArtifactId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRewindPreview {
    schema: String,
    schema_version: u16,
    checkpoint_id: CheckpointId,
    view_id: WorkspaceViewId,
    mode: RewindMode,
    applied: bool,
    ops: Vec<RewindOp>,
    conflicts: Vec<RewindConflict>,
}

fn parse_closed<T: Copy>(
    raw: &str,
    all: &[T],
    as_str: fn(T) -> &'static str,
) -> Result<T, CheckpointError> {
    for item in all {
        if as_str(*item) == raw {
            return Ok(*item);
        }
    }
    Err(CheckpointError::UnknownVariant)
}

fn deserialize_closed<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: FromStr<Err = CheckpointError>,
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    raw.parse()
        .map_err(|_| de::Error::unknown_variant(&raw, &[]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{CreateView, ViewAccess, ViewRegistry, WorkspaceBackend};
    use protocol::{AgentId, RepoId};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

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
        let registry = ViewRegistry::new();
        let mut spec = CreateView::new(RepoId::new(), WorkspaceBackend::Direct, "base-rev", access);
        if access.is_writable() {
            spec = spec.with_write_owner(AgentId::new());
        }
        registry.create(spec, &cancel()).expect("create view")
    }

    fn fixture(access: ViewAccess) -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-checkpoint-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("src")).expect("mkdir");
        fs::write(dir.join("src/lib.rs"), b"fn main() {}\n").expect("seed");
        let backend = DirectBackend::open(&dir, view(access), &cancel()).expect("open");
        Fixture {
            dir,
            backend: Some(backend),
        }
    }

    fn backend(fx: &Fixture) -> &DirectBackend {
        fx.backend.as_ref().expect("backend")
    }

    #[test]
    fn rewind_preview_then_apply_restores_journaled_files_only() {
        let fx = fixture(ViewAccess::ReadWrite);
        let created = repo("src/new.rs");
        backend(&fx)
            .write(&created, b"one\n", None, &cancel())
            .expect("create")
            .expect("entry");
        let user = fx.dir.join("notes.txt");
        fs::write(&user, b"keep me\n").expect("user file");
        let manager = CheckpointManager::new();
        let snap = manager
            .checkpoint(backend(&fx).view(), backend(&fx), "after-create", &cancel())
            .expect("checkpoint");
        backend(&fx)
            .write(&created, b"two\n", None, &cancel())
            .expect("update")
            .expect("entry");
        backend(&fx)
            .write(&repo("src/other.rs"), b"later\n", None, &cancel())
            .expect("later")
            .expect("entry");

        let preview = manager
            .rewind(
                backend(&fx).view(),
                backend(&fx),
                snap.id(),
                RewindMode::Preview,
                &cancel(),
            )
            .expect("preview");
        assert!(!preview.applied());
        assert!(preview.is_safe());
        assert_eq!(preview.ops().len(), 2);
        assert_eq!(
            backend(&fx).read(&created, &cancel()).expect("still two"),
            b"two\n"
        );
        assert!(fx.dir.join("src/other.rs").exists());

        let applied = manager
            .rewind(
                backend(&fx).view(),
                backend(&fx),
                snap.id(),
                RewindMode::Apply,
                &cancel(),
            )
            .expect("apply");
        assert!(applied.applied());
        assert!(applied.is_safe());
        assert_eq!(
            backend(&fx).read(&created, &cancel()).expect("restored"),
            b"one\n"
        );
        assert!(!fx.dir.join("src/other.rs").exists());
        assert_eq!(fs::read(&user).expect("user"), b"keep me\n");
    }

    #[test]
    fn binary_rewind_is_byte_identical() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/blob.bin");
        let original = [0u8, 1, 2, 255, 0, 10, 13];
        backend(&fx)
            .write(&path, &original, None, &cancel())
            .expect("write")
            .expect("entry");
        let manager = CheckpointManager::new();
        let snap = manager
            .checkpoint(backend(&fx).view(), backend(&fx), "binary", &cancel())
            .expect("checkpoint");
        backend(&fx)
            .write(&path, b"changed", None, &cancel())
            .expect("change")
            .expect("entry");
        manager
            .rewind(
                backend(&fx).view(),
                backend(&fx),
                snap.id(),
                RewindMode::Apply,
                &cancel(),
            )
            .expect("apply");
        assert_eq!(backend(&fx).read(&path, &cancel()).expect("read"), original);
    }

    #[test]
    fn newer_external_edit_is_previewable_and_refuses_apply() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/new.rs");
        backend(&fx)
            .write(&path, b"owned\n", None, &cancel())
            .expect("create")
            .expect("entry");
        let manager = CheckpointManager::new();
        let snap = manager
            .checkpoint(backend(&fx).view(), backend(&fx), "owned", &cancel())
            .expect("checkpoint");
        fs::write(fx.dir.join("src/new.rs"), b"user changed\n").expect("external");

        let preview = manager
            .rewind(
                backend(&fx).view(),
                backend(&fx),
                snap.id(),
                RewindMode::Preview,
                &cancel(),
            )
            .expect("preview");
        assert!(!preview.applied());
        assert!(!preview.is_safe());
        assert_eq!(preview.conflicts().len(), 1);
        assert_eq!(preview.conflicts()[0].path(), &path);
        assert!(preview.ops().is_empty());

        let err = manager
            .rewind(
                backend(&fx).view(),
                backend(&fx),
                snap.id(),
                RewindMode::Apply,
                &cancel(),
            )
            .expect_err("apply refused");
        assert!(matches!(
            err,
            CheckpointError::ExternalConflict { conflicts: 1, .. }
        ));
        assert_eq!(
            fs::read(fx.dir.join("src/new.rs")).expect("disk"),
            b"user changed\n"
        );
        let shown = err.to_string();
        assert!(!shown.contains("src/new.rs"));
        assert!(!shown.contains("user changed"));
    }

    #[test]
    fn apply_after_preview_toctou_still_refuses_user_edit() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/new.rs");
        backend(&fx)
            .write(&path, b"owned\n", None, &cancel())
            .expect("create")
            .expect("entry");
        let manager = CheckpointManager::new();
        let snap = manager
            .checkpoint(backend(&fx).view(), backend(&fx), "owned", &cancel())
            .expect("checkpoint");
        backend(&fx)
            .write(&path, b"later\n", None, &cancel())
            .expect("later")
            .expect("entry");
        let preview = manager
            .rewind(
                backend(&fx).view(),
                backend(&fx),
                snap.id(),
                RewindMode::Preview,
                &cancel(),
            )
            .expect("preview");
        assert!(preview.is_safe());
        assert_eq!(preview.ops().len(), 1);
        fs::write(fx.dir.join("src/new.rs"), b"sneak\n").expect("toctou");
        let err = manager
            .rewind(
                backend(&fx).view(),
                backend(&fx),
                snap.id(),
                RewindMode::Apply,
                &cancel(),
            )
            .expect_err("toctou");
        assert!(matches!(err, CheckpointError::ExternalConflict { .. }));
        assert_eq!(
            fs::read(fx.dir.join("src/new.rs")).expect("disk"),
            b"sneak\n"
        );
    }

    #[test]
    fn user_file_created_on_deleted_path_is_not_clobbered() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/new.rs");
        backend(&fx)
            .write(&path, b"owned\n", None, &cancel())
            .expect("create")
            .expect("entry");
        let manager = CheckpointManager::new();
        let snap = manager
            .checkpoint(backend(&fx).view(), backend(&fx), "owned", &cancel())
            .expect("checkpoint");
        backend(&fx).delete(&path, None, &cancel()).expect("delete");
        fs::write(fx.dir.join("src/new.rs"), b"user recreated\n").expect("user");
        let preview = manager
            .rewind(
                backend(&fx).view(),
                backend(&fx),
                snap.id(),
                RewindMode::Preview,
                &cancel(),
            )
            .expect("preview");
        assert!(!preview.is_safe());
        manager
            .rewind(
                backend(&fx).view(),
                backend(&fx),
                snap.id(),
                RewindMode::Apply,
                &cancel(),
            )
            .expect_err("refuse");
        assert_eq!(
            fs::read(fx.dir.join("src/new.rs")).expect("disk"),
            b"user recreated\n"
        );
    }

    #[test]
    fn wrong_view_checkpoint_cannot_be_applied() {
        let fx = fixture(ViewAccess::ReadWrite);
        backend(&fx)
            .write(&repo("src/a.rs"), b"a\n", None, &cancel())
            .expect("write")
            .expect("entry");
        let manager = CheckpointManager::new();
        let snap = manager
            .checkpoint(backend(&fx).view(), backend(&fx), "a", &cancel())
            .expect("checkpoint");
        let other = fixture(ViewAccess::ReadWrite);
        let err = manager
            .rewind(
                backend(&other).view(),
                backend(&other),
                snap.id(),
                RewindMode::Apply,
                &cancel(),
            )
            .expect_err("cross-view");
        assert_eq!(err, CheckpointError::ViewMismatch);
        assert!(!other.dir.join("src/a.rs").exists());
    }

    #[test]
    fn read_only_and_cancelled_operations_fail_closed() {
        let ro = fixture(ViewAccess::ReadOnly);
        let manager = CheckpointManager::new();
        let snap = manager
            .checkpoint(backend(&ro).view(), backend(&ro), "ro", &cancel())
            .expect("snapshot read-only");
        let err = manager
            .rewind(
                backend(&ro).view(),
                backend(&ro),
                snap.id(),
                RewindMode::Apply,
                &cancel(),
            )
            .expect_err("read-only apply");
        assert!(matches!(err, CheckpointError::ReadOnlyView));

        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            manager.checkpoint(backend(&ro).view(), backend(&ro), "x", &token),
            Err(CheckpointError::Cancelled)
        );
    }

    #[test]
    fn rollback_applied_always_restores_regardless_of_cancellation_state() {
        // `rollback_applied` used to take the caller's own `cancel` token and
        // both check it directly (`if cancel.is_cancelled() { return; }`)
        // and pass it into `DirectBackend::write`/`delete`, which check it
        // again internally. Its one real call site (`apply_plan`'s
        // cancellation branch) is only reached once that token is *already*
        // cancelled — meaning rollback was a guaranteed no-op on exactly the
        // path that needed it: an interrupted multi-file apply would leave
        // whatever prefix had already landed permanently mixed into the
        // workspace, with the caller only ever seeing `Cancelled` (implying
        // nothing happened). This constructs a real, two-file rewind plan,
        // manually applies the first op (simulating "already applied" when
        // the forward pass stopped), then confirms `rollback_applied` — now
        // signature-incapable of ever seeing an interfering cancellation —
        // actually restores it.
        let fx = fixture(ViewAccess::ReadWrite);
        let a = repo("src/a.rs");
        let b = repo("src/b.rs");
        backend(&fx)
            .write(&a, b"a1\n", None, &cancel())
            .expect("write a")
            .expect("entry");
        backend(&fx)
            .write(&b, b"b1\n", None, &cancel())
            .expect("write b")
            .expect("entry");
        let manager = CheckpointManager::new();
        let snap = manager
            .checkpoint(backend(&fx).view(), backend(&fx), "before", &cancel())
            .expect("checkpoint");
        backend(&fx)
            .write(&a, b"a2\n", None, &cancel())
            .expect("update a")
            .expect("entry");
        backend(&fx)
            .write(&b, b"b2\n", None, &cancel())
            .expect("update b")
            .expect("entry");

        let stored = {
            let inner = manager.lock().expect("lock");
            inner
                .checkpoints
                .get(&snap.id())
                .cloned()
                .expect("stored checkpoint")
        };
        let journal = backend(&fx).journal().expect("journal");
        let (ops, conflicts) =
            plan_rewind(backend(&fx), &stored, &journal, &cancel()).expect("plan");
        assert!(conflicts.is_empty());
        assert_eq!(ops.len(), 2, "both files need restoring");

        // Simulate "the forward pass applied op 0, then stopped" — actually
        // apply just the first op, exactly like `apply_plan`'s loop would.
        apply_one(backend(&fx), &ops[0], &cancel()).expect("apply first op");
        assert_eq!(
            backend(&fx)
                .read(&ops[0].path, &cancel())
                .expect("read after apply"),
            b"a1\n",
            "the first op's own restore must have landed"
        );

        rollback_applied(backend(&fx), &ops[..1]);
        assert_eq!(
            backend(&fx)
                .read(&ops[0].path, &cancel())
                .expect("read after rollback"),
            b"a2\n",
            "rollback must undo the applied op, restoring the pre-rewind (modified) content"
        );
        // The second file was never touched by either the simulated partial
        // apply or the rollback — it must be untouched throughout.
        assert_eq!(
            backend(&fx).read(&b, &cancel()).expect("b untouched"),
            b"b2\n"
        );
    }

    #[test]
    fn invalid_label_and_limit_fail_closed() {
        let fx = fixture(ViewAccess::ReadWrite);
        backend(&fx)
            .write(&repo("src/a.rs"), b"a\n", None, &cancel())
            .expect("write")
            .expect("entry");
        let manager = CheckpointManager::with_limits(1, MAX_CHECKPOINT_FILES, MAX_CHECKPOINT_BYTES);
        assert_eq!(
            manager.checkpoint(backend(&fx).view(), backend(&fx), "bad\0label", &cancel()),
            Err(CheckpointError::InvalidLabel)
        );
        manager
            .checkpoint(backend(&fx).view(), backend(&fx), "one", &cancel())
            .expect("first");
        let err = manager
            .checkpoint(backend(&fx).view(), backend(&fx), "two", &cancel())
            .expect_err("limit");
        assert!(matches!(err, CheckpointError::CheckpointLimit { limit: 1 }));
    }

    #[cfg(unix)]
    #[test]
    fn rewind_does_not_follow_symlink_escape() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/new.rs");
        backend(&fx)
            .write(&path, b"owned\n", None, &cancel())
            .expect("create")
            .expect("entry");
        let manager = CheckpointManager::new();
        let snap = manager
            .checkpoint(backend(&fx).view(), backend(&fx), "owned", &cancel())
            .expect("checkpoint");
        backend(&fx)
            .write(&path, b"later\n", None, &cancel())
            .expect("later")
            .expect("entry");
        let outside = fx.dir.join("../checkpoint-outside-target");
        fs::write(&outside, b"outside\n").expect("outside");
        fs::remove_file(fx.dir.join("src/new.rs")).expect("remove");
        std::os::unix::fs::symlink(&outside, fx.dir.join("src/new.rs")).expect("symlink");
        let err = manager
            .rewind(
                backend(&fx).view(),
                backend(&fx),
                snap.id(),
                RewindMode::Apply,
                &cancel(),
            )
            .expect_err("escape");
        assert_eq!(err, CheckpointError::PathEscape);
        assert_eq!(fs::read(&outside).expect("outside intact"), b"outside\n");
        let shown = err.to_string();
        assert!(!shown.contains("src/new.rs"));
        assert!(!shown.contains("checkpoint-outside-target"));
        let _ = fs::remove_file(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn rewind_does_not_follow_in_workspace_leaf_symlink() {
        let fx = fixture(ViewAccess::ReadWrite);
        let path = repo("src/new.rs");
        backend(&fx)
            .write(&path, b"owned\n", None, &cancel())
            .expect("create")
            .expect("entry");
        let manager = CheckpointManager::new();
        let snap = manager
            .checkpoint(backend(&fx).view(), backend(&fx), "owned", &cancel())
            .expect("checkpoint");
        backend(&fx)
            .write(&path, b"later\n", None, &cancel())
            .expect("later")
            .expect("entry");
        let user = fx.dir.join("notes.txt");
        fs::write(&user, b"later\n").expect("user file matching last RapidLM hash");
        fs::remove_file(fx.dir.join("src/new.rs")).expect("remove journaled leaf");
        std::os::unix::fs::symlink(&user, fx.dir.join("src/new.rs")).expect("in-workspace symlink");

        let err = manager
            .rewind(
                backend(&fx).view(),
                backend(&fx),
                snap.id(),
                RewindMode::Apply,
                &cancel(),
            )
            .expect_err("leaf symlink must fail closed");
        assert!(
            matches!(
                err,
                CheckpointError::PathEscape | CheckpointError::ExternalConflict { .. }
            ),
            "{err:?}"
        );
        assert_eq!(fs::read(&user).expect("referent intact"), b"later\n");
        assert!(
            fx.dir
                .join("src/new.rs")
                .symlink_metadata()
                .expect("leaf")
                .file_type()
                .is_symlink()
        );
        let shown = err.to_string();
        assert!(!shown.contains("notes.txt"));
        assert!(!shown.contains("src/new.rs"));
        assert!(!shown.contains("later"));
    }

    #[test]
    fn error_display_does_not_echo_paths_or_contents() {
        for err in [
            CheckpointError::PathEscape,
            CheckpointError::ExternalConflict {
                checkpoint_id: CheckpointId(3),
                conflicts: 2,
            },
            CheckpointError::GitScopeRequired,
            CheckpointError::InvalidLabel,
        ] {
            let text = err.to_string();
            for leaked in ["password", "hunter2", "/etc/passwd", "secret", "src/new.rs"] {
                assert!(!text.contains(leaked), "{text}");
            }
        }
    }

    #[test]
    fn checkpoint_round_trips_schema_and_rejects_unknown_fields() {
        let mut files = BTreeMap::new();
        files.insert(repo("src/lib.rs"), Some(ArtifactId::from_bytes(b"one\n")));
        let snapshot_hash = hash_files(&files);
        let view_id: WorkspaceViewId = "018f3c8a-7e2b-7a10-8c4d-0123456789ab"
            .parse()
            .expect("view id");
        let checkpoint = Checkpoint {
            id: CheckpointId(1),
            view_id,
            label: "after-create".to_owned(),
            journal_seq: 1,
            snapshot_hash,
            files,
        };
        let encoded = serde_json::to_string(&checkpoint).expect("encode");
        let decoded: Checkpoint = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, checkpoint);
        assert!(encoded.contains(CHECKPOINT_SCHEMA));
        assert!(
            serde_json::from_str::<Checkpoint>(&encoded.replace(
                "\"label\":\"after-create\"",
                "\"label\":\"after-create\",\"extra\":true"
            ))
            .is_err()
        );
    }

    #[test]
    fn persist_get_and_list_are_view_scoped() {
        let fx = fixture(ViewAccess::ReadWrite);
        backend(&fx)
            .write(&repo("src/a.rs"), b"a\n", None, &cancel())
            .expect("write")
            .expect("entry");
        let manager = CheckpointManager::new();
        let snap = manager
            .checkpoint(backend(&fx).view(), backend(&fx), "keep", &cancel())
            .expect("checkpoint");
        let loaded = manager.get(snap.id(), &cancel()).expect("get");
        assert_eq!(loaded.id(), snap.id());
        assert_eq!(loaded.label(), "keep");
        assert_eq!(
            manager
                .list(backend(&fx).view().id(), &cancel())
                .expect("list")
                .len(),
            1
        );
        assert!(
            manager
                .list(WorkspaceViewId::new(), &cancel())
                .expect("other")
                .is_empty()
        );
    }
}
