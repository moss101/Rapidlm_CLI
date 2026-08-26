//! Snapshot/hash workspace files around supervised commands and journal
//! mutations that did not come from a semantic patch.
//!
//! Detection compares content hashes only. mtime is ignored so a preserved
//! timestamp cannot hide a byte-level change. Fresh detections are always
//! [`MutationAttribution::Unreconciled`]; a matching reconcile hint is
//! required before any mutation may be labeled as a semantic patch.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use protocol::{ArtifactId, RepoPath};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::patch::apply::StagedChange;
use crate::patch::model::{PatchOp, SemanticPatch};
use crate::view::{CancellationToken, WorkspaceState, WorkspaceView};

/// Wire schema name for [`ExternalMutation`].
pub const MUTATION_SCHEMA: &str = "rapidlm.external_mutation";

/// v1 schema version for external-mutation objects.
pub const MUTATION_SCHEMA_VERSION: u16 = 1;

/// Wire schema name for [`WorkspaceSnapshot`].
pub const SNAPSHOT_SCHEMA: &str = "rapidlm.workspace_snapshot";

/// v1 schema version for workspace snapshots.
pub const SNAPSHOT_SCHEMA_VERSION: u16 = 1;

/// Maximum regular files accepted in one snapshot.
pub const MAX_SNAPSHOT_FILES: usize = 4096;

/// Maximum bytes hashed for one file.
pub const MAX_SNAPSHOT_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum combined file bytes hashed in one snapshot.
pub const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;

/// Maximum directory depth walked while capturing a snapshot.
pub const MAX_WALK_DEPTH: usize = 64;

/// Maximum journaled external mutations retained by one detector.
pub const MAX_MUTATION_JOURNAL: usize = 4096;

const CANCEL_STRIDE: usize = 8;
const DIRECT_TMP_PREFIX: &str = ".rapidlm-direct-";
const DIRECT_TMP_SUFFIX: &str = ".tmp";
const MUTATION_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "path",
    "kind",
    "before",
    "after",
    "attribution",
];
const SNAPSHOT_FIELDS: &[&str] = &["schema", "schema_version", "files"];

/// How a path changed between two content-hash snapshots.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MutationKind {
    Added,
    Modified,
    Deleted,
}

/// Provenance of a detected mutation. Detect never emits `SemanticPatch`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MutationAttribution {
    Unreconciled,
    SemanticPatch,
}

/// One added, modified, or deleted path that is not a semantic patch until
/// an explicit reconcile matches its hashes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalMutation {
    path: RepoPath,
    kind: MutationKind,
    before: Option<ArtifactId>,
    after: Option<ArtifactId>,
    attribution: MutationAttribution,
}

/// Content-addressed file set. mtime and other metadata are intentionally
/// omitted so timestamp-preserving rewrites still compare as different.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSnapshot {
    files: BTreeMap<RepoPath, ArtifactId>,
}

/// Expected before/after hashes from a first-party patch. Used only by
/// [`reconcile`]; detection never consults these.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconcileHint {
    path: RepoPath,
    before: Option<ArtifactId>,
    after: Option<ArtifactId>,
}

/// Capture bounds for a confined tree walk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotOptions {
    max_files: usize,
    max_file_bytes: usize,
    max_total_bytes: usize,
    max_depth: usize,
    max_journal: usize,
}

/// Snapshot/detect/journal helper bound to one set of resource caps.
#[derive(Clone, Debug)]
pub struct MutationDetector {
    options: SnapshotOptions,
    journal: Vec<ExternalMutation>,
}

/// Typed mutation-detector failure. Display never echoes paths or bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationError {
    Cancelled,
    InvalidRoot,
    PathEscape,
    BoundExceeded,
    JournalLimit,
    InvalidPath,
    NotADirectory,
    InvalidState,
    Io,
    UnknownVariant,
    UnsupportedSchema,
    UnsupportedSchemaVersion,
}

impl MutationKind {
    pub const ALL: &'static [Self] = &[Self::Added, Self::Modified, Self::Deleted];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }
}

impl MutationAttribution {
    pub const ALL: &'static [Self] = &[Self::Unreconciled, Self::SemanticPatch];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unreconciled => "unreconciled",
            Self::SemanticPatch => "semantic_patch",
        }
    }
}

impl ExternalMutation {
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn kind(&self) -> MutationKind {
        self.kind
    }

    pub fn before(&self) -> Option<ArtifactId> {
        self.before
    }

    pub fn after(&self) -> Option<ArtifactId> {
        self.after
    }

    pub fn attribution(&self) -> MutationAttribution {
        self.attribution
    }

    pub fn is_unreconciled(&self) -> bool {
        self.attribution == MutationAttribution::Unreconciled
    }
}

impl WorkspaceSnapshot {
    pub fn empty() -> Self {
        Self {
            files: BTreeMap::new(),
        }
    }

    /// Build a snapshot from already-hashed files. Duplicate paths keep the
    /// last hash. Count is bounded.
    pub fn from_hashed(
        files: impl IntoIterator<Item = (RepoPath, ArtifactId)>,
    ) -> Result<Self, MutationError> {
        let mut snapshot = Self::empty();
        for (path, hash) in files {
            if snapshot.files.len() >= MAX_SNAPSHOT_FILES && !snapshot.files.contains_key(&path) {
                return Err(MutationError::BoundExceeded);
            }
            snapshot.files.insert(path, hash);
        }
        Ok(snapshot)
    }

    pub fn insert(&mut self, path: RepoPath, hash: ArtifactId) -> Result<(), MutationError> {
        if self.files.len() >= MAX_SNAPSHOT_FILES && !self.files.contains_key(&path) {
            return Err(MutationError::BoundExceeded);
        }
        self.files.insert(path, hash);
        Ok(())
    }

    pub fn get(&self, path: &RepoPath) -> Option<ArtifactId> {
        self.files.get(path).copied()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn files(&self) -> impl Iterator<Item = (&RepoPath, ArtifactId)> {
        self.files.iter().map(|(path, hash)| (path, *hash))
    }

    /// Walk `root` and hash regular files. Symlink leaves are omitted so a
    /// redirected target cannot be reported as in-repo content.
    pub fn capture(
        root: impl AsRef<Path>,
        options: &SnapshotOptions,
        cancel: &CancellationToken,
    ) -> Result<Self, MutationError> {
        check_cancel(cancel)?;
        let root = canonicalize_root(root.as_ref())?;
        check_cancel(cancel)?;
        let mut files = BTreeMap::new();
        let mut total_bytes = 0usize;
        walk_dir(
            &root,
            &root,
            Path::new(""),
            0,
            options,
            cancel,
            &mut files,
            &mut total_bytes,
        )?;
        Ok(Self { files })
    }

    /// Same as [`Self::capture`] but rejects a closed view.
    pub fn capture_for_view(
        view: &WorkspaceView,
        root: impl AsRef<Path>,
        options: &SnapshotOptions,
        cancel: &CancellationToken,
    ) -> Result<Self, MutationError> {
        check_cancel(cancel)?;
        if view.state() == WorkspaceState::Closed {
            return Err(MutationError::InvalidState);
        }
        Self::capture(root, options, cancel)
    }
}

impl ReconcileHint {
    pub fn new(path: RepoPath, before: Option<ArtifactId>, after: Option<ArtifactId>) -> Self {
        Self {
            path,
            before,
            after,
        }
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn before(&self) -> Option<ArtifactId> {
        self.before
    }

    pub fn after(&self) -> Option<ArtifactId> {
        self.after
    }

    /// Hints that can be derived from a patch op without applying it.
    ///
    /// [`PatchOp::ReplaceRange`] is omitted: the after-hash is not known
    /// until apply, so a range op alone must not reconcile a shell rewrite.
    pub fn from_patch_op(op: &PatchOp) -> Vec<Self> {
        match op {
            PatchOp::CreateFile { path, content, .. } => vec![Self::new(
                path.clone(),
                None,
                Some(ArtifactId::from_bytes(content)),
            )],
            PatchOp::DeleteFile { path, preimage } => {
                vec![Self::new(path.clone(), Some(*preimage), None)]
            }
            PatchOp::MoveFile { from, to, preimage } => vec![
                Self::new(from.clone(), Some(*preimage), None),
                Self::new(to.clone(), None, Some(*preimage)),
            ],
            PatchOp::ReplaceRange { .. } => Vec::new(),
        }
    }

    pub fn from_semantic_patch(patch: &SemanticPatch) -> Vec<Self> {
        patch.ops().iter().flat_map(Self::from_patch_op).collect()
    }

    pub fn from_staged_change(change: &StagedChange) -> Vec<Self> {
        match change {
            StagedChange::Create { path, after, .. } => {
                vec![Self::new(path.clone(), None, Some(*after))]
            }
            StagedChange::Replace {
                path,
                before,
                after,
                ..
            } => vec![Self::new(path.clone(), Some(*before), Some(*after))],
            StagedChange::Delete { path, before, .. } => {
                vec![Self::new(path.clone(), Some(*before), None)]
            }
            StagedChange::Move {
                from, to, preimage, ..
            } => vec![
                Self::new(from.clone(), Some(*preimage), None),
                Self::new(to.clone(), None, Some(*preimage)),
            ],
        }
    }
}

impl SnapshotOptions {
    pub fn new() -> Self {
        Self {
            max_files: MAX_SNAPSHOT_FILES,
            max_file_bytes: MAX_SNAPSHOT_FILE_BYTES,
            max_total_bytes: MAX_SNAPSHOT_BYTES,
            max_depth: MAX_WALK_DEPTH,
            max_journal: MAX_MUTATION_JOURNAL,
        }
    }

    pub fn with_max_files(mut self, max_files: usize) -> Self {
        self.max_files = max_files;
        self
    }

    pub fn with_max_file_bytes(mut self, max_file_bytes: usize) -> Self {
        self.max_file_bytes = max_file_bytes;
        self
    }

    pub fn with_max_total_bytes(mut self, max_total_bytes: usize) -> Self {
        self.max_total_bytes = max_total_bytes;
        self
    }

    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    pub fn with_max_journal(mut self, max_journal: usize) -> Self {
        self.max_journal = max_journal;
        self
    }

    pub fn max_files(&self) -> usize {
        self.max_files
    }

    pub fn max_file_bytes(&self) -> usize {
        self.max_file_bytes
    }

    pub fn max_total_bytes(&self) -> usize {
        self.max_total_bytes
    }

    pub fn max_depth(&self) -> usize {
        self.max_depth
    }

    pub fn max_journal(&self) -> usize {
        self.max_journal
    }
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl MutationDetector {
    pub fn new() -> Self {
        Self::with_options(SnapshotOptions::new())
    }

    pub fn with_options(options: SnapshotOptions) -> Self {
        Self {
            options,
            journal: Vec::new(),
        }
    }

    pub fn options(&self) -> &SnapshotOptions {
        &self.options
    }

    /// All journaled mutations, including any later marked reconciled.
    pub fn journal(&self) -> &[ExternalMutation] {
        &self.journal
    }

    /// Mutations still not attributed to a semantic patch.
    pub fn unreconciled(&self) -> impl Iterator<Item = &ExternalMutation> {
        self.journal.iter().filter(|item| item.is_unreconciled())
    }

    pub fn snapshot(
        &self,
        root: impl AsRef<Path>,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceSnapshot, MutationError> {
        WorkspaceSnapshot::capture(root, &self.options, cancel)
    }

    pub fn snapshot_for_view(
        &self,
        view: &WorkspaceView,
        root: impl AsRef<Path>,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceSnapshot, MutationError> {
        WorkspaceSnapshot::capture_for_view(view, root, &self.options, cancel)
    }

    /// Compare two snapshots. Every result is unreconciled.
    pub fn detect(
        &self,
        before: &WorkspaceSnapshot,
        after: &WorkspaceSnapshot,
        cancel: &CancellationToken,
    ) -> Result<Vec<ExternalMutation>, MutationError> {
        detect_checked(before, after, cancel)
    }

    /// Snapshot, run `command`, snapshot again, journal unreconciled diffs.
    pub fn around<F, T>(
        &mut self,
        root: impl AsRef<Path>,
        cancel: &CancellationToken,
        command: F,
    ) -> Result<(T, Vec<ExternalMutation>), MutationError>
    where
        F: FnOnce() -> T,
    {
        let root = root.as_ref();
        let before = self.snapshot(root, cancel)?;
        let output = command();
        let after = self.snapshot(root, cancel)?;
        let mutations = detect_checked(&before, &after, cancel)?;
        self.record(mutations.clone(), cancel)?;
        Ok((output, mutations))
    }

    /// Append detections. Attribution is forced to unreconciled so a caller
    /// cannot smuggle a semantic-patch label into the journal.
    pub fn record(
        &mut self,
        mutations: impl IntoIterator<Item = ExternalMutation>,
        cancel: &CancellationToken,
    ) -> Result<(), MutationError> {
        check_cancel(cancel)?;
        if self.options.max_journal == 0 {
            return Err(MutationError::BoundExceeded);
        }
        for (index, mut mutation) in mutations.into_iter().enumerate() {
            if index.is_multiple_of(CANCEL_STRIDE) {
                check_cancel(cancel)?;
            }
            if self.journal.len() >= self.options.max_journal {
                return Err(MutationError::JournalLimit);
            }
            mutation.attribution = MutationAttribution::Unreconciled;
            self.journal.push(mutation);
        }
        Ok(())
    }

    /// Mark journal entries whose path and hashes exactly match a hint.
    /// Path-only overlap is not enough.
    pub fn reconcile(
        &mut self,
        hints: &[ReconcileHint],
        cancel: &CancellationToken,
    ) -> Result<usize, MutationError> {
        reconcile_slice(&mut self.journal, hints, cancel)
    }
}

impl Default for MutationDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Compare content hashes and record added, modified, and deleted paths.
///
/// Every mutation is [`MutationAttribution::Unreconciled`].
pub fn detect(before: &WorkspaceSnapshot, after: &WorkspaceSnapshot) -> Vec<ExternalMutation> {
    let mut out = Vec::new();
    for (path, before_hash) in &before.files {
        match after.files.get(path) {
            None => out.push(mutation(
                path.clone(),
                MutationKind::Deleted,
                Some(*before_hash),
                None,
            )),
            Some(after_hash) if after_hash != before_hash => out.push(mutation(
                path.clone(),
                MutationKind::Modified,
                Some(*before_hash),
                Some(*after_hash),
            )),
            Some(_) => {}
        }
    }
    for (path, after_hash) in &after.files {
        if !before.files.contains_key(path) {
            out.push(mutation(
                path.clone(),
                MutationKind::Added,
                None,
                Some(*after_hash),
            ));
        }
    }
    out
}

/// Cancellable form of [`detect`].
pub fn detect_checked(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    cancel: &CancellationToken,
) -> Result<Vec<ExternalMutation>, MutationError> {
    check_cancel(cancel)?;
    let mut seen = 0usize;
    for _ in before.files.keys().chain(after.files.keys()) {
        if seen.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        seen += 1;
    }
    Ok(detect(before, after))
}

/// Attribute mutations whose path and both hashes match a hint. Hints that
/// only share a path do not change attribution.
pub fn reconcile(
    mutations: &mut [ExternalMutation],
    hints: &[ReconcileHint],
    cancel: &CancellationToken,
) -> Result<usize, MutationError> {
    reconcile_slice(mutations, hints, cancel)
}

fn mutation(
    path: RepoPath,
    kind: MutationKind,
    before: Option<ArtifactId>,
    after: Option<ArtifactId>,
) -> ExternalMutation {
    ExternalMutation {
        path,
        kind,
        before,
        after,
        attribution: MutationAttribution::Unreconciled,
    }
}

fn reconcile_slice(
    mutations: &mut [ExternalMutation],
    hints: &[ReconcileHint],
    cancel: &CancellationToken,
) -> Result<usize, MutationError> {
    check_cancel(cancel)?;
    let mut attributed = 0usize;
    for (index, mutation) in mutations.iter_mut().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        if mutation.attribution == MutationAttribution::SemanticPatch {
            continue;
        }
        if hints.iter().any(|hint| hint_matches(hint, mutation)) {
            mutation.attribution = MutationAttribution::SemanticPatch;
            attributed += 1;
        }
    }
    Ok(attributed)
}

fn hint_matches(hint: &ReconcileHint, mutation: &ExternalMutation) -> bool {
    hint.path == mutation.path && hint.before == mutation.before && hint.after == mutation.after
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    logical: &Path,
    depth: usize,
    options: &SnapshotOptions,
    cancel: &CancellationToken,
    files: &mut BTreeMap<RepoPath, ArtifactId>,
    total_bytes: &mut usize,
) -> Result<(), MutationError> {
    check_cancel(cancel)?;
    if options.max_files == 0
        || options.max_file_bytes == 0
        || options.max_total_bytes == 0
        || options.max_depth == 0
    {
        return Err(MutationError::BoundExceeded);
    }
    if depth > options.max_depth {
        return Err(MutationError::BoundExceeded);
    }
    confine_dir(dir, root)?;
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|_| MutationError::Io)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| MutationError::Io)?;
    entries.sort_by_key(|entry| entry.file_name());
    for (index, entry) in entries.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            return Err(MutationError::InvalidPath);
        };
        if name_str == "." || name_str == ".." {
            continue;
        }
        if is_git_component(name_str) || is_direct_tmp(name_str) {
            continue;
        }
        if name_str.contains('\0') {
            return Err(MutationError::InvalidPath);
        }
        // A Unix leaf may contain '\'. RepoPath::parse would split it and
        // could overwrite src/lib.rs with a planted src\lib.rs. Fail closed.
        if leaf_name_has_separator(name_str) {
            return Err(MutationError::InvalidPath);
        }
        let child = dir.join(&name);
        confine_child(&child, root)?;
        let meta = match fs::symlink_metadata(&child) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(MutationError::Io),
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            let next_logical = logical.join(name_str);
            walk_dir(
                root,
                &child,
                &next_logical,
                depth + 1,
                options,
                cancel,
                files,
                total_bytes,
            )?;
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let path = logical_repo_path(logical, name_str)?;
        let bytes = read_confined(root, &child, options.max_file_bytes)?;
        *total_bytes = total_bytes
            .checked_add(bytes.len())
            .ok_or(MutationError::BoundExceeded)?;
        if *total_bytes > options.max_total_bytes {
            return Err(MutationError::BoundExceeded);
        }
        if files.len() >= options.max_files {
            return Err(MutationError::BoundExceeded);
        }
        files.insert(path, ArtifactId::from_bytes(&bytes));
    }
    Ok(())
}

fn logical_repo_path(parent: &Path, name: &str) -> Result<RepoPath, MutationError> {
    if leaf_name_has_separator(name) {
        return Err(MutationError::InvalidPath);
    }
    let mut raw = String::new();
    for component in parent.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(MutationError::PathEscape);
        };
        let part = part.to_str().ok_or(MutationError::InvalidPath)?;
        if leaf_name_has_separator(part) {
            return Err(MutationError::InvalidPath);
        }
        if !raw.is_empty() {
            raw.push('/');
        }
        raw.push_str(part);
    }
    if !raw.is_empty() {
        raw.push('/');
    }
    raw.push_str(name);
    RepoPath::parse(&raw).map_err(|_| MutationError::InvalidPath)
}

fn leaf_name_has_separator(name: &str) -> bool {
    name.contains('/') || name.contains('\\')
}

fn canonicalize_root(root: &Path) -> Result<PathBuf, MutationError> {
    if root.as_os_str().is_empty() {
        return Err(MutationError::InvalidRoot);
    }
    let meta = fs::symlink_metadata(root).map_err(|_| MutationError::InvalidRoot)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(MutationError::InvalidRoot);
    }
    let canonical = fs::canonicalize(root).map_err(|_| MutationError::InvalidRoot)?;
    let again = fs::symlink_metadata(&canonical).map_err(|_| MutationError::InvalidRoot)?;
    if again.file_type().is_symlink() || !again.is_dir() {
        return Err(MutationError::InvalidRoot);
    }
    Ok(canonical)
}

fn confine_dir(path: &Path, root: &Path) -> Result<(), MutationError> {
    if path == root || path.starts_with(root) {
        Ok(())
    } else {
        Err(MutationError::PathEscape)
    }
}

fn confine_child(path: &Path, root: &Path) -> Result<(), MutationError> {
    if path == root {
        return Err(MutationError::PathEscape);
    }
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(MutationError::PathEscape)
    }
}

fn confine_canon(path: &Path, root: &Path) -> Result<(), MutationError> {
    if path == root {
        return Err(MutationError::PathEscape);
    }
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(MutationError::PathEscape)
    }
}

fn read_confined(
    root: &Path,
    host: &Path,
    max_file_bytes: usize,
) -> Result<Vec<u8>, MutationError> {
    let meta = fs::symlink_metadata(host).map_err(|_| MutationError::Io)?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(MutationError::Io);
    }
    if meta.len() > max_file_bytes as u64 {
        return Err(MutationError::BoundExceeded);
    }
    let canon = fs::canonicalize(host).map_err(|_| MutationError::Io)?;
    confine_canon(&canon, root)?;
    let again = fs::symlink_metadata(&canon).map_err(|_| MutationError::Io)?;
    if again.file_type().is_symlink() || !again.is_file() {
        return Err(MutationError::Io);
    }
    let file = File::open(&canon).map_err(|_| MutationError::Io)?;
    let mut bytes = Vec::new();
    file.take(max_file_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| MutationError::Io)?;
    if bytes.len() > max_file_bytes {
        return Err(MutationError::BoundExceeded);
    }
    let verify = fs::canonicalize(host).map_err(|_| MutationError::Io)?;
    confine_canon(&verify, root)?;
    Ok(bytes)
}

fn is_git_component(part: &str) -> bool {
    part.eq_ignore_ascii_case(".git")
}

fn is_direct_tmp(name: &str) -> bool {
    name.starts_with(DIRECT_TMP_PREFIX) && name.ends_with(DIRECT_TMP_SUFFIX)
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), MutationError> {
    if cancel.is_cancelled() {
        Err(MutationError::Cancelled)
    } else {
        Ok(())
    }
}

impl fmt::Display for MutationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for MutationAttribution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for MutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "workspace mutation operation cancelled",
            Self::InvalidRoot => "workspace snapshot root is invalid",
            Self::PathEscape => "resolved path escapes the snapshot root",
            Self::BoundExceeded => "workspace snapshot resource bound exceeded",
            Self::JournalLimit => "external mutation journal limit reached",
            Self::InvalidPath => "workspace snapshot path is invalid",
            Self::NotADirectory => "workspace snapshot root is not a directory",
            Self::InvalidState => "workspace view is not in a valid state",
            Self::Io => "workspace snapshot I/O failed",
            Self::UnknownVariant => "unknown external mutation enumeration value",
            Self::UnsupportedSchema => "unsupported external mutation schema",
            Self::UnsupportedSchemaVersion => "unsupported external mutation schema version",
        })
    }
}

impl Error for MutationError {}

impl FromStr for MutationKind {
    type Err = MutationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl FromStr for MutationAttribution {
    type Err = MutationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl Serialize for MutationKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Serialize for MutationAttribution {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MutationKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl<'de> Deserialize<'de> for MutationAttribution {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl Serialize for ExternalMutation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ExternalMutation", MUTATION_FIELDS.len())?;
        state.serialize_field("schema", MUTATION_SCHEMA)?;
        state.serialize_field("schema_version", &MUTATION_SCHEMA_VERSION)?;
        state.serialize_field("path", &self.path)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("before", &self.before)?;
        state.serialize_field("after", &self.after)?;
        state.serialize_field("attribution", &self.attribution)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ExternalMutation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawExternalMutation::deserialize(deserializer)?;
        if raw.schema != MUTATION_SCHEMA {
            return Err(de::Error::custom(MutationError::UnsupportedSchema));
        }
        if raw.schema_version != MUTATION_SCHEMA_VERSION {
            return Err(de::Error::custom(MutationError::UnsupportedSchemaVersion));
        }
        match (raw.kind, raw.before, raw.after) {
            (MutationKind::Added, None, Some(_))
            | (MutationKind::Deleted, Some(_), None)
            | (MutationKind::Modified, Some(_), Some(_)) => {}
            _ => return Err(de::Error::custom(MutationError::UnknownVariant)),
        }
        // Wire cannot grant semantic-patch attribution. Only exact-hash
        // reconcile() may produce MutationAttribution::SemanticPatch.
        if raw.attribution == MutationAttribution::SemanticPatch {
            return Err(de::Error::custom(MutationError::UnknownVariant));
        }
        Ok(ExternalMutation {
            path: raw.path,
            kind: raw.kind,
            before: raw.before,
            after: raw.after,
            attribution: MutationAttribution::Unreconciled,
        })
    }
}

impl Serialize for WorkspaceSnapshot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("WorkspaceSnapshot", SNAPSHOT_FIELDS.len())?;
        state.serialize_field("schema", SNAPSHOT_SCHEMA)?;
        state.serialize_field("schema_version", &SNAPSHOT_SCHEMA_VERSION)?;
        state.serialize_field("files", &self.files)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for WorkspaceSnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawWorkspaceSnapshot::deserialize(deserializer)?;
        if raw.schema != SNAPSHOT_SCHEMA {
            return Err(de::Error::custom(MutationError::UnsupportedSchema));
        }
        if raw.schema_version != SNAPSHOT_SCHEMA_VERSION {
            return Err(de::Error::custom(MutationError::UnsupportedSchemaVersion));
        }
        if raw.files.len() > MAX_SNAPSHOT_FILES {
            return Err(de::Error::custom(MutationError::BoundExceeded));
        }
        Ok(WorkspaceSnapshot { files: raw.files })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExternalMutation {
    schema: String,
    schema_version: u16,
    path: RepoPath,
    kind: MutationKind,
    before: Option<ArtifactId>,
    after: Option<ArtifactId>,
    attribution: MutationAttribution,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspaceSnapshot {
    schema: String,
    schema_version: u16,
    files: BTreeMap<RepoPath, ArtifactId>,
}

fn parse_closed<T: Copy>(
    raw: &str,
    all: &[T],
    as_str: fn(T) -> &'static str,
) -> Result<T, MutationError> {
    for item in all {
        if as_str(*item) == raw {
            return Ok(*item);
        }
    }
    Err(MutationError::UnknownVariant)
}

fn deserialize_closed<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: FromStr<Err = MutationError>,
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    raw.parse()
        .map_err(|_| de::Error::unknown_variant(&raw, &[]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    use protocol::{AgentId, RepoId};

    use crate::view::{CreateView, ViewAccess, ViewRegistry, WorkspaceBackend};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    const HELLO: &[u8] = b"hello";
    const WORLD: &[u8] = b"world";
    const HELLO_HASH: &str =
        "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    const WORLD_HASH: &str =
        "sha256:486ea46224d1bb4fb680f34f7c9ad96a8f24ec88be73ea8e5a6c65260e9cb8a7";
    const GOLDEN_MUTATION: &str = r#"{"schema":"rapidlm.external_mutation","schema_version":1,"path":"src/lib.rs","kind":"modified","before":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","after":"sha256:486ea46224d1bb4fb680f34f7c9ad96a8f24ec88be73ea8e5a6c65260e9cb8a7","attribution":"unreconciled"}"#;

    struct Fixture {
        dir: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn repo(path: &str) -> RepoPath {
        RepoPath::parse(path).expect("repo path")
    }

    fn hash(bytes: &[u8]) -> ArtifactId {
        ArtifactId::from_bytes(bytes)
    }

    fn fixture() -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-extmut-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("src")).expect("mkdir");
        fs::write(dir.join("src/lib.rs"), HELLO).expect("seed");
        Fixture { dir }
    }

    fn view(state_closed: bool) -> WorkspaceView {
        let registry = ViewRegistry::new();
        let created = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::Direct,
                    "base-rev",
                    ViewAccess::ReadWrite,
                )
                .with_write_owner(AgentId::new()),
                &cancel(),
            )
            .expect("create view");
        if state_closed {
            registry
                .release_write(
                    created.id(),
                    created.write_owner().expect("owner"),
                    &cancel(),
                )
                .expect("release");
            registry.close(created.id(), &cancel()).expect("close");
            registry.get(created.id(), &cancel()).expect("get closed")
        } else {
            created
        }
    }

    fn hashed(pairs: &[(&str, &[u8])]) -> WorkspaceSnapshot {
        WorkspaceSnapshot::from_hashed(pairs.iter().map(|(path, bytes)| (repo(path), hash(bytes))))
            .expect("snapshot")
    }

    fn kinds(mutations: &[ExternalMutation]) -> Vec<(String, MutationKind, MutationAttribution)> {
        mutations
            .iter()
            .map(|item| {
                (
                    item.path().as_str().to_owned(),
                    item.kind(),
                    item.attribution(),
                )
            })
            .collect()
    }

    #[test]
    fn detect_records_added_modified_and_deleted_paths() {
        let before = hashed(&[("src/a.rs", HELLO), ("src/b.rs", HELLO)]);
        let after = hashed(&[("src/b.rs", WORLD), ("src/c.rs", HELLO)]);
        let found = detect(&before, &after);
        assert_eq!(
            kinds(&found),
            vec![
                (
                    "src/a.rs".to_owned(),
                    MutationKind::Deleted,
                    MutationAttribution::Unreconciled
                ),
                (
                    "src/b.rs".to_owned(),
                    MutationKind::Modified,
                    MutationAttribution::Unreconciled
                ),
                (
                    "src/c.rs".to_owned(),
                    MutationKind::Added,
                    MutationAttribution::Unreconciled
                ),
            ]
        );
        assert_eq!(found[0].before(), Some(hash(HELLO)));
        assert_eq!(found[0].after(), None);
        assert_eq!(found[1].before(), Some(hash(HELLO)));
        assert_eq!(found[1].after(), Some(hash(WORLD)));
        assert_eq!(found[2].before(), None);
        assert_eq!(found[2].after(), Some(hash(HELLO)));
        assert!(found.iter().all(ExternalMutation::is_unreconciled));
    }

    #[test]
    fn content_change_is_detected_when_mtime_is_preserved() {
        let fx = fixture();
        let path = fx.dir.join("src/lib.rs");
        let before = WorkspaceSnapshot::capture(&fx.dir, &SnapshotOptions::new(), &cancel())
            .expect("before");
        let original_mtime = fs::metadata(&path)
            .expect("meta")
            .modified()
            .expect("mtime");
        fs::write(&path, WORLD).expect("rewrite");
        File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(original_mtime)
            .expect("restore mtime");
        let restored = fs::metadata(&path)
            .expect("meta")
            .modified()
            .expect("mtime");
        assert_eq!(restored, original_mtime);
        let after =
            WorkspaceSnapshot::capture(&fx.dir, &SnapshotOptions::new(), &cancel()).expect("after");
        let found = detect(&before, &after);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path(), &repo("src/lib.rs"));
        assert_eq!(found[0].kind(), MutationKind::Modified);
        assert_eq!(found[0].before(), Some(hash(HELLO)));
        assert_eq!(found[0].after(), Some(hash(WORLD)));
        assert_eq!(found[0].attribution(), MutationAttribution::Unreconciled);
    }

    #[test]
    fn identical_bytes_with_changed_mtime_are_not_mutations() {
        let fx = fixture();
        let path = fx.dir.join("src/lib.rs");
        let before = WorkspaceSnapshot::capture(&fx.dir, &SnapshotOptions::new(), &cancel())
            .expect("before");
        File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(SystemTime::now())
            .expect("touch");
        let after =
            WorkspaceSnapshot::capture(&fx.dir, &SnapshotOptions::new(), &cancel()).expect("after");
        assert!(detect(&before, &after).is_empty());
    }

    #[test]
    fn around_supervised_command_journals_unreconciled_mutations() {
        let fx = fixture();
        let mut detector = MutationDetector::new();
        let ((), found) = detector
            .around(&fx.dir, &cancel(), || {
                fs::write(fx.dir.join("src/lib.rs"), WORLD).expect("shell write");
                fs::write(fx.dir.join("src/new.rs"), HELLO).expect("shell add");
                fs::remove_file(fx.dir.join("src/lib.rs")).ok();
                fs::write(fx.dir.join("src/lib.rs"), WORLD).expect("recreate");
            })
            .expect("around");
        assert!(found.iter().all(ExternalMutation::is_unreconciled));
        assert!(
            found.iter().any(
                |item| item.path() == &repo("src/new.rs") && item.kind() == MutationKind::Added
            )
        );
        assert!(found.iter().any(|item| {
            item.path() == &repo("src/lib.rs") && item.kind() == MutationKind::Modified
        }));
        assert_eq!(detector.unreconciled().count(), found.len());
        assert_eq!(detector.journal().len(), found.len());
    }

    #[test]
    fn semantic_patch_does_not_attribute_until_hashes_reconcile() {
        let before = hashed(&[("src/lib.rs", HELLO)]);
        let after = hashed(&[("src/lib.rs", WORLD)]);
        let mut found = detect(&before, &after);
        let patch = SemanticPatch::new(
            vec![
                PatchOp::replace_range(repo("src/lib.rs"), hash(HELLO), 0, 5, "world")
                    .expect("replace"),
            ],
            AgentId::new(),
            "base-rev",
            &cancel(),
        )
        .expect("patch");
        let from_patch = ReconcileHint::from_semantic_patch(&patch);
        assert!(
            from_patch.is_empty(),
            "replace-range must not yield a hint without an after hash"
        );
        let marked = reconcile(&mut found, &from_patch, &cancel()).expect("reconcile");
        assert_eq!(marked, 0);
        assert_eq!(found[0].attribution(), MutationAttribution::Unreconciled);

        let wrong = [ReconcileHint::new(
            repo("src/lib.rs"),
            Some(hash(HELLO)),
            Some(hash(b"other")),
        )];
        let marked = reconcile(&mut found, &wrong, &cancel()).expect("wrong hashes");
        assert_eq!(marked, 0);
        assert_eq!(found[0].attribution(), MutationAttribution::Unreconciled);

        let matching = [ReconcileHint::new(
            repo("src/lib.rs"),
            Some(hash(HELLO)),
            Some(hash(WORLD)),
        )];
        let marked = reconcile(&mut found, &matching, &cancel()).expect("match");
        assert_eq!(marked, 1);
        assert_eq!(found[0].attribution(), MutationAttribution::SemanticPatch);
    }

    #[test]
    fn record_strips_forged_semantic_patch_attribution() {
        let mut detector = MutationDetector::new();
        let forged = ExternalMutation {
            path: repo("src/lib.rs"),
            kind: MutationKind::Added,
            before: None,
            after: Some(hash(HELLO)),
            attribution: MutationAttribution::SemanticPatch,
        };
        detector.record([forged], &cancel()).expect("record");
        assert_eq!(detector.journal().len(), 1);
        assert_eq!(
            detector.journal()[0].attribution(),
            MutationAttribution::Unreconciled
        );
        assert_eq!(detector.unreconciled().count(), 1);
    }

    #[test]
    fn create_file_patch_can_reconcile_matching_add() {
        let before = hashed(&[]);
        let after = hashed(&[("src/new.rs", HELLO)]);
        let mut found = detect(&before, &after);
        let patch = SemanticPatch::new(
            vec![PatchOp::create_file(repo("src/new.rs"), HELLO.to_vec(), false).expect("create")],
            AgentId::new(),
            "base-rev",
            &cancel(),
        )
        .expect("patch");
        let marked = reconcile(
            &mut found,
            &ReconcileHint::from_semantic_patch(&patch),
            &cancel(),
        )
        .expect("reconcile");
        assert_eq!(marked, 1);
        assert_eq!(found[0].attribution(), MutationAttribution::SemanticPatch);
    }

    #[test]
    fn staged_replace_hint_reconciles_only_exact_hashes() {
        let change = StagedChange::Replace {
            path: repo("src/lib.rs"),
            before: hash(HELLO),
            after: hash(WORLD),
            executable: false,
        };
        let hints = ReconcileHint::from_staged_change(&change);
        let mut found = detect(
            &hashed(&[("src/lib.rs", HELLO)]),
            &hashed(&[("src/lib.rs", WORLD)]),
        );
        assert_eq!(reconcile(&mut found, &hints, &cancel()).expect("ok"), 1);
        let mut other = detect(
            &hashed(&[("src/lib.rs", HELLO)]),
            &hashed(&[("src/lib.rs", b"nope")]),
        );
        assert_eq!(reconcile(&mut other, &hints, &cancel()).expect("ok"), 0);
        assert!(other[0].is_unreconciled());
    }

    #[test]
    fn symlink_leaf_is_not_hashed_as_workspace_content() {
        let fx = fixture();
        let before = WorkspaceSnapshot::capture(&fx.dir, &SnapshotOptions::new(), &cancel())
            .expect("before");
        fs::remove_file(fx.dir.join("src/lib.rs")).expect("unlink");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", fx.dir.join("src/lib.rs")).expect("symlink");
        }
        #[cfg(not(unix))]
        {
            std::os::windows::fs::symlink_file("C:\\Windows\\win.ini", fx.dir.join("src/lib.rs"))
                .expect("symlink");
        }
        let after =
            WorkspaceSnapshot::capture(&fx.dir, &SnapshotOptions::new(), &cancel()).expect("after");
        assert!(after.get(&repo("src/lib.rs")).is_none());
        let found = detect(&before, &after);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind(), MutationKind::Deleted);
        assert_eq!(found[0].before(), Some(hash(HELLO)));
        assert_eq!(found[0].after(), None);
        if let Ok(outside) = fs::read("/etc/passwd") {
            assert_ne!(found[0].before(), Some(hash(&outside)));
        }
    }

    #[test]
    fn directory_symlink_escape_is_not_walked() {
        let fx = fixture();
        let outside = std::env::temp_dir().join(format!(
            "rapidlm-extmut-out-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("secret"), b"SECRET").expect("secret");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, fx.dir.join("leak")).expect("dir symlink");
        #[cfg(not(unix))]
        std::os::windows::fs::symlink_dir(&outside, fx.dir.join("leak")).expect("dir symlink");
        let snapshot =
            WorkspaceSnapshot::capture(&fx.dir, &SnapshotOptions::new(), &cancel()).expect("snap");
        assert!(snapshot.get(&repo("leak/secret")).is_none());
        assert!(snapshot.get(&repo("src/lib.rs")).is_some());
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn snapshot_skips_git_and_direct_tmp_files() {
        let fx = fixture();
        fs::create_dir_all(fx.dir.join(".git/hooks")).expect("git");
        fs::write(fx.dir.join(".git/hooks/pre-commit"), b"evil").expect("hook");
        fs::write(fx.dir.join(".rapidlm-direct-1.tmp"), b"tmp").expect("tmp");
        let snapshot =
            WorkspaceSnapshot::capture(&fx.dir, &SnapshotOptions::new(), &cancel()).expect("snap");
        assert!(snapshot.get(&repo(".git/hooks/pre-commit")).is_none());
        assert!(snapshot.get(&repo(".rapidlm-direct-1.tmp")).is_none());
        assert_eq!(snapshot.get(&repo("src/lib.rs")), Some(hash(HELLO)));
    }

    #[test]
    fn closed_view_cannot_be_snapshotted() {
        let fx = fixture();
        let closed = view(true);
        let err = WorkspaceSnapshot::capture_for_view(
            &closed,
            &fx.dir,
            &SnapshotOptions::new(),
            &cancel(),
        )
        .expect_err("closed");
        assert_eq!(err, MutationError::InvalidState);
    }

    #[test]
    fn cancelled_capture_and_reconcile_fail_closed() {
        let fx = fixture();
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            WorkspaceSnapshot::capture(&fx.dir, &SnapshotOptions::new(), &token),
            Err(MutationError::Cancelled)
        );
        let mut found = detect(&hashed(&[("a.rs", HELLO)]), &hashed(&[]));
        assert_eq!(
            reconcile(&mut found, &[], &token),
            Err(MutationError::Cancelled)
        );
        let mut detector = MutationDetector::new();
        assert_eq!(
            detector.record(found, &token),
            Err(MutationError::Cancelled)
        );
    }

    #[test]
    fn file_and_journal_bounds_fail_closed() {
        let fx = fixture();
        let tiny = SnapshotOptions::new().with_max_file_bytes(1);
        let err = WorkspaceSnapshot::capture(&fx.dir, &tiny, &cancel()).expect_err("file cap");
        assert_eq!(err, MutationError::BoundExceeded);
        let mut detector =
            MutationDetector::with_options(SnapshotOptions::new().with_max_journal(1));
        let first = mutation(repo("a.rs"), MutationKind::Added, None, Some(hash(HELLO)));
        let second = mutation(repo("b.rs"), MutationKind::Added, None, Some(hash(WORLD)));
        detector.record([first], &cancel()).expect("one");
        assert_eq!(
            detector.record([second], &cancel()),
            Err(MutationError::JournalLimit)
        );
        assert_eq!(detector.journal().len(), 1);
    }

    #[test]
    fn display_does_not_echo_paths_or_payloads() {
        let err = MutationError::PathEscape;
        let shown = err.to_string();
        assert!(!shown.contains("src"));
        assert!(!shown.contains("SECRET"));
        assert!(!shown.contains("passwd"));
        assert_eq!(shown, "resolved path escapes the snapshot root");
    }

    #[test]
    fn golden_mutation_round_trips() {
        let built = mutation(
            repo("src/lib.rs"),
            MutationKind::Modified,
            Some(hash(HELLO)),
            Some(hash(WORLD)),
        );
        let json = serde_json::to_string(&built).expect("serialize");
        assert_eq!(json, GOLDEN_MUTATION);
        let decoded = serde_json::from_str::<ExternalMutation>(GOLDEN_MUTATION).expect("decode");
        assert_eq!(decoded, built);
        assert_eq!(decoded.before().expect("before").to_string(), HELLO_HASH);
        assert_eq!(decoded.after().expect("after").to_string(), WORLD_HASH);
    }

    #[test]
    fn unknown_attribution_and_path_traversal_are_rejected() {
        let bad_attr = r#"{"schema":"rapidlm.external_mutation","schema_version":1,"path":"src/lib.rs","kind":"added","before":null,"after":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","attribution":"trusted_patch"}"#;
        assert!(serde_json::from_str::<ExternalMutation>(bad_attr).is_err());
        let traversal = r#"{"schema":"rapidlm.external_mutation","schema_version":1,"path":"../secret","kind":"deleted","before":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","after":null,"attribution":"unreconciled"}"#;
        assert!(serde_json::from_str::<ExternalMutation>(traversal).is_err());
        let extra = r#"{"schema":"rapidlm.external_mutation","schema_version":1,"path":"src/lib.rs","kind":"added","before":null,"after":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","attribution":"unreconciled","note":"x"}"#;
        assert!(serde_json::from_str::<ExternalMutation>(extra).is_err());
    }

    #[test]
    fn wire_semantic_patch_attribution_is_not_honored() {
        let forged = r#"{"schema":"rapidlm.external_mutation","schema_version":1,"path":"src/lib.rs","kind":"modified","before":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","after":"sha256:486ea46224d1bb4fb680f34f7c9ad96a8f24ec88be73ea8e5a6c65260e9cb8a7","attribution":"semantic_patch"}"#;
        let decoded = serde_json::from_str::<ExternalMutation>(forged);
        assert!(
            decoded
                .as_ref()
                .ok()
                .is_none_or(|item| { item.attribution() == MutationAttribution::Unreconciled }),
            "wire attribution=semantic_patch must not produce SemanticPatch"
        );
        assert!(
            decoded.is_err(),
            "wire attribution=semantic_patch is rejected unless reconcile produced it"
        );

        let mut found = detect(
            &hashed(&[("src/lib.rs", HELLO)]),
            &hashed(&[("src/lib.rs", WORLD)]),
        );
        let hints = [ReconcileHint::new(
            repo("src/lib.rs"),
            Some(hash(HELLO)),
            Some(hash(WORLD)),
        )];
        assert_eq!(reconcile(&mut found, &hints, &cancel()).expect("ok"), 1);
        assert_eq!(found[0].attribution(), MutationAttribution::SemanticPatch);
        let wire = serde_json::to_string(&found[0]).expect("serialize");
        let reloaded = serde_json::from_str::<ExternalMutation>(&wire);
        assert!(
            reloaded
                .as_ref()
                .ok()
                .is_none_or(|item| item.attribution() == MutationAttribution::Unreconciled)
        );
        assert!(reloaded.is_err());
    }

    #[test]
    fn planted_backslash_leaf_cannot_hide_slash_path_rewrite() {
        assert!(leaf_name_has_separator("src\\lib.rs"));
        assert!(leaf_name_has_separator("src/lib.rs"));
        assert!(!leaf_name_has_separator("lib.rs"));
        assert_eq!(
            RepoPath::parse("src\\lib.rs").expect("split"),
            repo("src/lib.rs")
        );
        assert_eq!(
            logical_repo_path(Path::new(""), "src\\lib.rs"),
            Err(MutationError::InvalidPath)
        );
        assert_eq!(
            logical_repo_path(Path::new(""), "src/lib.rs"),
            Err(MutationError::InvalidPath)
        );
        assert_eq!(
            logical_repo_path(Path::new("src"), "lib.rs"),
            Ok(repo("src/lib.rs"))
        );

        let fx = fixture();
        let before = WorkspaceSnapshot::capture(&fx.dir, &SnapshotOptions::new(), &cancel())
            .expect("before");
        assert_eq!(before.get(&repo("src/lib.rs")), Some(hash(HELLO)));
        fs::write(fx.dir.join("src/lib.rs"), WORLD).expect("rewrite");

        #[cfg(unix)]
        {
            // One Unix filename. Joining "src\\lib.rs" on Windows would hit
            // the real src/lib.rs and is not this attack.
            fs::write(fx.dir.join("src\\lib.rs"), HELLO).expect("plant");
            let captured = WorkspaceSnapshot::capture(&fx.dir, &SnapshotOptions::new(), &cancel());
            assert_eq!(captured.as_ref().err(), Some(&MutationError::InvalidPath));
            if let Ok(after) = captured {
                let found = detect(&before, &after);
                assert!(
                    found.iter().any(|item| {
                        item.path() == &repo("src/lib.rs")
                            && item.kind() == MutationKind::Modified
                            && item.before() == Some(hash(HELLO))
                            && item.after() == Some(hash(WORLD))
                    }),
                    "planted src\\lib.rs must not hide the rewrite of src/lib.rs"
                );
            }
        }
    }

    #[test]
    fn detect_checked_observes_cancellation() {
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            detect_checked(&hashed(&[("a.rs", HELLO)]), &hashed(&[]), &token),
            Err(MutationError::Cancelled)
        );
    }
}
