//! Isolated Git worktrees for write-capable subagents.
//!
//! Each view owns a distinct directory and an internal ref under
//! [`GIT_WORKTREE_REF_NAMESPACE`]. Creation never checks out the user's
//! current branch. Cleanup never uses `--force`; a dirty worktree is left
//! intact and reported as [`GitWorktreeError::CleanupFailed`].

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use protocol::WorkspaceViewId;
use serde::{Deserialize, Serialize};

use crate::view::{CancellationToken, WorkspaceBackend, WorkspaceState, WorkspaceView};

/// Wire schema name for persisted worktree metadata.
pub const GIT_WORKTREE_SCHEMA: &str = "rapidlm.git_worktree_view";

/// v1 schema version for persisted worktree metadata.
pub const GIT_WORKTREE_SCHEMA_VERSION: u16 = 1;

/// Internal ref prefix. Per-view refs are `{namespace}/{view_id}`.
pub const GIT_WORKTREE_REF_NAMESPACE: &str = "refs/rapidlm/views";

/// Maximum captured stdout+stderr bytes from one Git invocation.
pub const MAX_GIT_OUTPUT_BYTES: usize = 1024 * 1024;

/// Maximum live RapidLM worktrees managed by one store.
pub const MAX_GIT_WORKTREES: usize = 256;

/// Default Git subprocess deadline.
pub const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(30);

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const CANCEL_STRIDE: u32 = 8;
const TMP_SUFFIX: &str = ".tmp";
const RAPIDLM_DIR: &str = "rapidlm";
const VIEWS_DIR: &str = "views";
const WORKTREES_DIR: &str = "worktrees";
const HOOKS_DIR: &str = "disabled-hooks";
const MAX_FILTER_OVERRIDES: usize = 256;

static OPEN_REPOS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// Open flags for a Git worktree store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitWorktreeOptions {
    timeout: Duration,
    max_output_bytes: usize,
    max_worktrees: usize,
    git_program: PathBuf,
}

/// Durable record of one isolated worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitWorktreeRecord {
    view: WorkspaceView,
    git_ref: String,
    worktree_path: PathBuf,
    resolved_commit: String,
    record_state: GitWorktreeRecordState,
}

/// Lifecycle of persisted worktree metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitWorktreeRecordState {
    Creating,
    Active,
    CleanupFailed,
}

/// Manages isolated worktrees for one Git repository.
pub struct GitWorktreeStore {
    user_root: PathBuf,
    git_common_dir: PathBuf,
    rapidlm_dir: PathBuf,
    hooks_dir: PathBuf,
    timeout: Duration,
    max_output_bytes: usize,
    max_worktrees: usize,
    git_program: PathBuf,
    inner: Mutex<Inner>,
    _lease: RepoLease,
}

/// Typed Git-worktree failure. Display never echoes paths, refs, or Git text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitWorktreeError {
    Cancelled,
    WrongBackend,
    ReadOnlyView,
    InvalidState,
    InvalidRoot,
    NotAGitRepository,
    InvalidRevision,
    PathEscape,
    AlreadyExists,
    AlreadyOpen,
    ViewNotFound,
    CleanupFailed,
    WorktreeLimit,
    BoundExceeded,
    Timeout,
    GitFailed,
    GitNotFound,
    MetadataCorrupt,
    LockPoisoned,
    Io,
}

struct Inner {}

struct RepoLease {
    root: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedWorktree {
    schema: String,
    schema_version: u16,
    view: WorkspaceView,
    git_ref: String,
    worktree_relpath: String,
    resolved_commit: String,
    record_state: GitWorktreeRecordState,
}

struct UserHead {
    symbolic: Option<String>,
    sha: String,
}

struct GitOutput {
    stdout: Vec<u8>,
    status_ok: bool,
}

impl GitWorktreeOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    pub fn with_max_worktrees(mut self, max_worktrees: usize) -> Self {
        self.max_worktrees = max_worktrees;
        self
    }

    pub fn with_git_program(mut self, git_program: impl Into<PathBuf>) -> Self {
        self.git_program = git_program.into();
        self
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    pub fn max_worktrees(&self) -> usize {
        self.max_worktrees
    }
}

impl Default for GitWorktreeOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_GIT_TIMEOUT,
            max_output_bytes: MAX_GIT_OUTPUT_BYTES,
            max_worktrees: MAX_GIT_WORKTREES,
            git_program: PathBuf::from("git"),
        }
    }
}

impl GitWorktreeRecord {
    pub fn view(&self) -> &WorkspaceView {
        &self.view
    }

    pub fn view_id(&self) -> WorkspaceViewId {
        self.view.id()
    }

    pub fn git_ref(&self) -> &str {
        &self.git_ref
    }

    pub fn worktree_path(&self) -> &Path {
        &self.worktree_path
    }

    pub fn resolved_commit(&self) -> &str {
        &self.resolved_commit
    }

    pub fn record_state(&self) -> GitWorktreeRecordState {
        self.record_state
    }
}

impl GitWorktreeStore {
    /// Open the Git repository that will own RapidLM worktrees.
    pub fn open(
        repo: impl AsRef<Path>,
        cancel: &CancellationToken,
    ) -> Result<Self, GitWorktreeError> {
        Self::open_with(repo, GitWorktreeOptions::default(), cancel)
    }

    pub fn open_with(
        repo: impl AsRef<Path>,
        options: GitWorktreeOptions,
        cancel: &CancellationToken,
    ) -> Result<Self, GitWorktreeError> {
        cancel.check().map_err(|_| GitWorktreeError::Cancelled)?;
        if options.timeout.is_zero() || options.max_output_bytes == 0 || options.max_worktrees == 0
        {
            return Err(GitWorktreeError::BoundExceeded);
        }
        let requested = canonicalize_dir(repo.as_ref())?;
        let user_root = git_show_toplevel(&options, &requested, cancel)?;
        let git_common_dir = git_common_dir(&options, &user_root, cancel)?;
        confine_dir(&git_common_dir, &git_common_dir)?;
        cancel.check().map_err(|_| GitWorktreeError::Cancelled)?;
        let rapidlm_dir = git_common_dir.join(RAPIDLM_DIR);
        ensure_real_dir(&rapidlm_dir, &git_common_dir)?;
        let views_dir = rapidlm_dir.join(VIEWS_DIR);
        let worktrees_dir = rapidlm_dir.join(WORKTREES_DIR);
        let hooks_dir = rapidlm_dir.join(HOOKS_DIR);
        ensure_real_dir(&views_dir, &git_common_dir)?;
        ensure_real_dir(&worktrees_dir, &git_common_dir)?;
        ensure_real_dir(&hooks_dir, &git_common_dir)?;
        let lease = RepoLease::acquire(git_common_dir.clone())?;
        Ok(Self {
            user_root,
            git_common_dir,
            rapidlm_dir,
            hooks_dir,
            timeout: options.timeout,
            max_output_bytes: options.max_output_bytes,
            max_worktrees: options.max_worktrees,
            git_program: options.git_program,
            inner: Mutex::new(Inner {}),
            _lease: lease,
        })
    }

    pub fn user_root(&self) -> &Path {
        &self.user_root
    }

    pub fn git_common_dir(&self) -> &Path {
        &self.git_common_dir
    }

    /// Create an isolated worktree and persist view metadata.
    ///
    /// Uses `refs/rapidlm/views/{view_id}` and a detached checkout so the
    /// user's current branch is not changed.
    pub fn create_view(
        &self,
        view: &WorkspaceView,
        cancel: &CancellationToken,
    ) -> Result<GitWorktreeRecord, GitWorktreeError> {
        cancel.check().map_err(|_| GitWorktreeError::Cancelled)?;
        if view.backend() != WorkspaceBackend::GitWorktree {
            return Err(GitWorktreeError::WrongBackend);
        }
        if !view.access().is_writable() {
            return Err(GitWorktreeError::ReadOnlyView);
        }
        if view.state() != WorkspaceState::Active {
            return Err(GitWorktreeError::InvalidState);
        }
        validate_revision(view.base_revision())?;

        let _guard = self.lock()?;
        cancel.check().map_err(|_| GitWorktreeError::Cancelled)?;
        self.ensure_layout()?;
        if self.metadata_count()? >= self.max_worktrees {
            return Err(GitWorktreeError::WorktreeLimit);
        }

        let view_id = view.id();
        let git_ref = view_ref(view_id);
        let relpath = view_relpath(view_id);
        let worktree_path = self.worktree_abs(&relpath)?;
        let meta_path = self.metadata_path(view_id);
        if meta_path.exists() || worktree_path.exists() || self.ref_exists(&git_ref, cancel)? {
            return Err(GitWorktreeError::AlreadyExists);
        }

        let head_before = self.user_head(cancel)?;
        let sha = self.resolve_commit(view.base_revision(), cancel)?;
        let creating = GitWorktreeRecord {
            view: view.clone(),
            git_ref: git_ref.clone(),
            worktree_path: worktree_path.clone(),
            resolved_commit: sha.clone(),
            record_state: GitWorktreeRecordState::Creating,
        };
        self.write_metadata(&creating, &relpath)?;

        let created = self.finish_create(creating, &head_before, cancel);
        match created {
            Ok(record) => {
                self.write_metadata(&record, &relpath)?;
                Ok(record)
            }
            Err(err) => {
                self.rollback_create(view_id, &git_ref, &worktree_path, cancel);
                Err(err)
            }
        }
    }

    /// Remove a worktree and its internal ref. Never force-deletes.
    pub fn remove_view(
        &self,
        view_id: WorkspaceViewId,
        cancel: &CancellationToken,
    ) -> Result<(), GitWorktreeError> {
        cancel.check().map_err(|_| GitWorktreeError::Cancelled)?;
        let _guard = self.lock()?;
        cancel.check().map_err(|_| GitWorktreeError::Cancelled)?;
        self.ensure_layout()?;
        let mut record = self.read_metadata(view_id)?;
        let relpath = view_relpath(view_id);
        let worktree_path = self.worktree_abs(&relpath)?;
        if record.worktree_path != worktree_path || record.git_ref != view_ref(view_id) {
            return Err(GitWorktreeError::MetadataCorrupt);
        }

        if worktree_path.exists() {
            match self.git(
                &["worktree", "remove", "--", path_arg(&worktree_path)?],
                cancel,
            ) {
                Ok(_) => {}
                Err(GitWorktreeError::GitFailed) => {
                    record.record_state = GitWorktreeRecordState::CleanupFailed;
                    self.write_metadata(&record, &relpath)?;
                    return Err(GitWorktreeError::CleanupFailed);
                }
                Err(err) => return Err(err),
            }
            if worktree_path.exists() {
                record.record_state = GitWorktreeRecordState::CleanupFailed;
                self.write_metadata(&record, &relpath)?;
                return Err(GitWorktreeError::CleanupFailed);
            }
        }

        match self.delete_ref(&record.git_ref, cancel) {
            Ok(()) => {}
            Err(GitWorktreeError::GitFailed) => {
                record.record_state = GitWorktreeRecordState::CleanupFailed;
                self.write_metadata(&record, &relpath)?;
                return Err(GitWorktreeError::CleanupFailed);
            }
            Err(err) => return Err(err),
        }

        remove_metadata(&self.metadata_path(view_id))?;
        Ok(())
    }

    pub fn load_view(
        &self,
        view_id: WorkspaceViewId,
        cancel: &CancellationToken,
    ) -> Result<GitWorktreeRecord, GitWorktreeError> {
        cancel.check().map_err(|_| GitWorktreeError::Cancelled)?;
        let _guard = self.lock()?;
        cancel.check().map_err(|_| GitWorktreeError::Cancelled)?;
        self.read_metadata(view_id)
    }

    pub fn list_views(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<GitWorktreeRecord>, GitWorktreeError> {
        cancel.check().map_err(|_| GitWorktreeError::Cancelled)?;
        let _guard = self.lock()?;
        cancel.check().map_err(|_| GitWorktreeError::Cancelled)?;
        let mut records = Vec::new();
        for id in self.metadata_ids()? {
            if records.len().is_multiple_of(CANCEL_STRIDE as usize) {
                cancel.check().map_err(|_| GitWorktreeError::Cancelled)?;
            }
            records.push(self.read_metadata(id)?);
        }
        records.sort_by_key(|record| record.view_id().to_string());
        Ok(records)
    }

    fn finish_create(
        &self,
        record: GitWorktreeRecord,
        head_before: &UserHead,
        cancel: &CancellationToken,
    ) -> Result<GitWorktreeRecord, GitWorktreeError> {
        self.update_ref(&record.git_ref, &record.resolved_commit, cancel)?;
        self.git(
            &[
                "worktree",
                "add",
                "--detach",
                "--",
                path_arg(&record.worktree_path)?,
                &record.resolved_commit,
            ],
            cancel,
        )?;
        if !is_real_dir(&record.worktree_path)? {
            return Err(GitWorktreeError::PathEscape);
        }
        let canon = fs::canonicalize(&record.worktree_path).map_err(|_| GitWorktreeError::Io)?;
        confine_dir(&canon, &self.rapidlm_dir.join(WORKTREES_DIR))?;
        let head_after = self.user_head(cancel)?;
        if head_after.sha != head_before.sha || head_after.symbolic != head_before.symbolic {
            return Err(GitWorktreeError::GitFailed);
        }
        let mut active = record;
        active.worktree_path = canon;
        active.record_state = GitWorktreeRecordState::Active;
        Ok(active)
    }

    /// Undo a failed `create_view`. Mirrors `remove_view`'s own contract
    /// ("a dirty worktree is left intact and reported as `CleanupFailed`"):
    /// if the worktree can't actually be removed, the ref and metadata
    /// record must survive too, marked `CleanupFailed`, so the directory
    /// stays discoverable via `list_views` instead of becoming a permanently
    /// orphaned worktree with no record pointing at it.
    /// Undo a failed `create_view`. Mirrors `remove_view`'s own contract
    /// ("a dirty worktree is left intact and reported as `CleanupFailed`"):
    /// if the worktree can't actually be removed, the ref and metadata
    /// record must survive too, marked `CleanupFailed`, so the directory
    /// stays discoverable via `list_views` instead of becoming a permanently
    /// orphaned worktree with no record pointing at it.
    fn rollback_create(
        &self,
        view_id: WorkspaceViewId,
        git_ref: &str,
        worktree_path: &Path,
        cancel: &CancellationToken,
    ) {
        let relpath = view_relpath(view_id);
        if worktree_path.exists() {
            let removed = path_arg(worktree_path)
                .ok()
                .and_then(|path| self.git(&["worktree", "remove", "--", path], cancel).ok());
            if removed.is_none() || worktree_path.exists() {
                self.mark_cleanup_failed(view_id, &relpath);
                return;
            }
        }
        if self.delete_ref(git_ref, cancel).is_err() {
            self.mark_cleanup_failed(view_id, &relpath);
            return;
        }
        let _ = remove_metadata(&self.metadata_path(view_id));
    }

    /// Best-effort: mark the still-persisted record `CleanupFailed` instead
    /// of leaving cleanup's caller to silently drop it.
    fn mark_cleanup_failed(&self, view_id: WorkspaceViewId, relpath: &str) {
        if let Ok(mut record) = self.read_metadata(view_id) {
            record.record_state = GitWorktreeRecordState::CleanupFailed;
            let _ = self.write_metadata(&record, relpath);
        }
    }

    fn resolve_commit(
        &self,
        revision: &str,
        cancel: &CancellationToken,
    ) -> Result<String, GitWorktreeError> {
        validate_revision(revision)?;
        let spec = format!("{revision}^{{commit}}");
        let out = self
            .git(
                &["rev-parse", "--verify", "--end-of-options", &spec],
                cancel,
            )
            .map_err(|err| match err {
                GitWorktreeError::GitFailed => GitWorktreeError::InvalidRevision,
                other => other,
            })?;
        parse_sha(&stdout_line(&out)?)
    }

    fn user_head(&self, cancel: &CancellationToken) -> Result<UserHead, GitWorktreeError> {
        let sha_out = self.git(&["rev-parse", "HEAD"], cancel)?;
        let sha = parse_sha(&stdout_line(&sha_out)?)?;
        let symbolic = match self.git(&["symbolic-ref", "--quiet", "HEAD"], cancel) {
            Ok(out) => Some(parse_symbolic_ref(&stdout_line(&out)?)?),
            Err(GitWorktreeError::GitFailed) => None,
            Err(err) => return Err(err),
        };
        Ok(UserHead { symbolic, sha })
    }

    fn ref_exists(
        &self,
        git_ref: &str,
        cancel: &CancellationToken,
    ) -> Result<bool, GitWorktreeError> {
        match self.git(&["show-ref", "--verify", "--quiet", "--", git_ref], cancel) {
            Ok(_) => Ok(true),
            Err(GitWorktreeError::GitFailed) => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn update_ref(
        &self,
        git_ref: &str,
        sha: &str,
        cancel: &CancellationToken,
    ) -> Result<(), GitWorktreeError> {
        self.git(&["update-ref", git_ref, sha], cancel)?;
        Ok(())
    }

    fn delete_ref(
        &self,
        git_ref: &str,
        cancel: &CancellationToken,
    ) -> Result<(), GitWorktreeError> {
        match self.git(&["update-ref", "-d", git_ref], cancel) {
            Ok(_) => Ok(()),
            Err(GitWorktreeError::GitFailed) => {
                if self.ref_exists(git_ref, cancel)? {
                    Err(GitWorktreeError::GitFailed)
                } else {
                    Ok(())
                }
            }
            Err(err) => Err(err),
        }
    }

    fn git(&self, args: &[&str], cancel: &CancellationToken) -> Result<Vec<u8>, GitWorktreeError> {
        run_git(
            &self.git_program,
            &self.user_root,
            &self.hooks_dir,
            args,
            self.timeout,
            self.max_output_bytes,
            cancel,
        )
        .map(|out| {
            if out.status_ok {
                Ok(out.stdout)
            } else {
                Err(GitWorktreeError::GitFailed)
            }
        })?
    }

    fn ensure_layout(&self) -> Result<(), GitWorktreeError> {
        ensure_real_dir(&self.rapidlm_dir, &self.git_common_dir)?;
        ensure_real_dir(&self.rapidlm_dir.join(VIEWS_DIR), &self.git_common_dir)?;
        ensure_real_dir(&self.rapidlm_dir.join(WORKTREES_DIR), &self.git_common_dir)?;
        ensure_real_dir(&self.hooks_dir, &self.git_common_dir)?;
        Ok(())
    }

    fn worktree_abs(&self, relpath: &str) -> Result<PathBuf, GitWorktreeError> {
        let parent = self.rapidlm_dir.join(WORKTREES_DIR);
        let path = parent.join(
            relpath
                .strip_prefix("worktrees/")
                .ok_or(GitWorktreeError::MetadataCorrupt)?,
        );
        confine_child(&path, &parent)?;
        Ok(path)
    }

    fn metadata_path(&self, view_id: WorkspaceViewId) -> PathBuf {
        self.rapidlm_dir
            .join(VIEWS_DIR)
            .join(format!("{view_id}.json"))
    }

    fn write_metadata(
        &self,
        record: &GitWorktreeRecord,
        relpath: &str,
    ) -> Result<(), GitWorktreeError> {
        if record.git_ref != view_ref(record.view_id()) || relpath != view_relpath(record.view_id())
        {
            return Err(GitWorktreeError::MetadataCorrupt);
        }
        let persisted = PersistedWorktree {
            schema: GIT_WORKTREE_SCHEMA.to_owned(),
            schema_version: GIT_WORKTREE_SCHEMA_VERSION,
            view: record.view.clone(),
            git_ref: record.git_ref.clone(),
            worktree_relpath: relpath.to_owned(),
            resolved_commit: record.resolved_commit.clone(),
            record_state: record.record_state,
        };
        let bytes = serde_json::to_vec(&persisted).map_err(|_| GitWorktreeError::Io)?;
        if bytes.len() > self.max_output_bytes {
            return Err(GitWorktreeError::BoundExceeded);
        }
        atomic_write(
            &self.metadata_path(record.view_id()),
            &bytes,
            &self.git_common_dir,
        )
    }

    fn read_metadata(
        &self,
        view_id: WorkspaceViewId,
    ) -> Result<GitWorktreeRecord, GitWorktreeError> {
        let path = self.metadata_path(view_id);
        let bytes = read_confined_file(&path, &self.git_common_dir, self.max_output_bytes)?;
        let persisted: PersistedWorktree =
            serde_json::from_slice(&bytes).map_err(|_| GitWorktreeError::MetadataCorrupt)?;
        if persisted.schema != GIT_WORKTREE_SCHEMA
            || persisted.schema_version != GIT_WORKTREE_SCHEMA_VERSION
        {
            return Err(GitWorktreeError::MetadataCorrupt);
        }
        if persisted.view.id() != view_id
            || persisted.view.backend() != WorkspaceBackend::GitWorktree
            || persisted.git_ref != view_ref(view_id)
            || persisted.worktree_relpath != view_relpath(view_id)
        {
            return Err(GitWorktreeError::MetadataCorrupt);
        }
        parse_sha(&persisted.resolved_commit)?;
        let worktree_path = self.worktree_abs(&persisted.worktree_relpath)?;
        Ok(GitWorktreeRecord {
            view: persisted.view,
            git_ref: persisted.git_ref,
            worktree_path,
            resolved_commit: persisted.resolved_commit,
            record_state: persisted.record_state,
        })
    }

    fn metadata_ids(&self) -> Result<Vec<WorkspaceViewId>, GitWorktreeError> {
        let dir = self.rapidlm_dir.join(VIEWS_DIR);
        let entries = fs::read_dir(&dir).map_err(|_| GitWorktreeError::Io)?;
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|_| GitWorktreeError::Io)?;
            let name = entry.file_name();
            let name = name.to_str().ok_or(GitWorktreeError::MetadataCorrupt)?;
            if name.ends_with(TMP_SUFFIX) {
                continue;
            }
            let Some(stem) = name.strip_suffix(".json") else {
                return Err(GitWorktreeError::MetadataCorrupt);
            };
            let id = stem
                .parse::<WorkspaceViewId>()
                .map_err(|_| GitWorktreeError::MetadataCorrupt)?;
            ids.push(id);
        }
        Ok(ids)
    }

    fn metadata_count(&self) -> Result<usize, GitWorktreeError> {
        Ok(self.metadata_ids()?.len())
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, GitWorktreeError> {
        self.inner
            .lock()
            .map_err(|_| GitWorktreeError::LockPoisoned)
    }
}

impl fmt::Debug for GitWorktreeStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitWorktreeStore")
            .field("user_root", &self.user_root)
            .finish()
    }
}

impl Drop for RepoLease {
    fn drop(&mut self) {
        if let Ok(mut roots) = open_repos().lock() {
            roots.remove(&self.root);
        }
    }
}

impl RepoLease {
    fn acquire(root: PathBuf) -> Result<Self, GitWorktreeError> {
        let mut roots = open_repos()
            .lock()
            .map_err(|_| GitWorktreeError::LockPoisoned)?;
        if !roots.insert(root.clone()) {
            return Err(GitWorktreeError::AlreadyOpen);
        }
        Ok(Self { root })
    }
}

impl fmt::Display for GitWorktreeRecordState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Creating => "creating",
            Self::Active => "active",
            Self::CleanupFailed => "cleanup_failed",
        })
    }
}

impl fmt::Display for GitWorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "git worktree operation cancelled",
            Self::WrongBackend => "workspace view is not a git worktree",
            Self::ReadOnlyView => "git worktree views must be write-capable",
            Self::InvalidState => "workspace view is not writable in this state",
            Self::InvalidRoot => "git worktree repository root is invalid",
            Self::NotAGitRepository => "path is not a git working tree",
            Self::InvalidRevision => "git worktree base revision is invalid",
            Self::PathEscape => "resolved path escapes the git worktree root",
            Self::AlreadyExists => "git worktree view already exists",
            Self::AlreadyOpen => "git worktree store is already open for this repository",
            Self::ViewNotFound => "git worktree view metadata not found",
            Self::CleanupFailed => "git worktree cleanup failed and was not forced",
            Self::WorktreeLimit => "git worktree limit reached",
            Self::BoundExceeded => "git worktree resource bound exceeded",
            Self::Timeout => "git worktree operation timed out",
            Self::GitFailed => "git worktree operation failed",
            Self::GitNotFound => "git executable was not found",
            Self::MetadataCorrupt => "git worktree metadata is corrupt",
            Self::LockPoisoned => "git worktree store lock poisoned",
            Self::Io => "git worktree I/O failed",
        })
    }
}

impl Error for GitWorktreeError {}

fn view_ref(view_id: WorkspaceViewId) -> String {
    format!("{GIT_WORKTREE_REF_NAMESPACE}/{view_id}")
}

fn view_relpath(view_id: WorkspaceViewId) -> String {
    format!("worktrees/{view_id}")
}

fn validate_revision(raw: &str) -> Result<(), GitWorktreeError> {
    if raw.is_empty() || raw.starts_with('-') {
        return Err(GitWorktreeError::InvalidRevision);
    }
    if raw.contains('\0')
        || raw.chars().any(char::is_control)
        || raw.chars().any(char::is_whitespace)
    {
        return Err(GitWorktreeError::InvalidRevision);
    }
    if raw.contains(':') || raw.contains('\\') {
        return Err(GitWorktreeError::InvalidRevision);
    }
    if !raw.chars().all(|ch| {
        ch.is_ascii_alphanumeric()
            || matches!(ch, '/' | '_' | '.' | '-' | '^' | '~' | '{' | '}' | '@')
    }) {
        return Err(GitWorktreeError::InvalidRevision);
    }
    Ok(())
}

fn parse_sha(raw: &str) -> Result<String, GitWorktreeError> {
    if raw.len() != 40 && raw.len() != 64 {
        return Err(GitWorktreeError::InvalidRevision);
    }
    if !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(GitWorktreeError::InvalidRevision);
    }
    Ok(raw.to_ascii_lowercase())
}

fn parse_symbolic_ref(raw: &str) -> Result<String, GitWorktreeError> {
    if raw.is_empty() || raw.len() > 256 || !raw.starts_with("refs/") {
        return Err(GitWorktreeError::GitFailed);
    }
    if raw.contains('\0')
        || raw.chars().any(char::is_control)
        || raw.chars().any(char::is_whitespace)
    {
        return Err(GitWorktreeError::GitFailed);
    }
    Ok(raw.to_owned())
}

fn stdout_line(bytes: &[u8]) -> Result<String, GitWorktreeError> {
    let text = std::str::from_utf8(bytes).map_err(|_| GitWorktreeError::GitFailed)?;
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return Err(GitWorktreeError::GitFailed);
    }
    Ok(line.to_owned())
}

fn path_arg(path: &Path) -> Result<&str, GitWorktreeError> {
    path.to_str().ok_or(GitWorktreeError::PathEscape)
}

fn canonicalize_dir(path: &Path) -> Result<PathBuf, GitWorktreeError> {
    if path.as_os_str().is_empty() {
        return Err(GitWorktreeError::InvalidRoot);
    }
    let meta = fs::symlink_metadata(path).map_err(|_| GitWorktreeError::InvalidRoot)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(GitWorktreeError::InvalidRoot);
    }
    let canonical = fs::canonicalize(path).map_err(|_| GitWorktreeError::InvalidRoot)?;
    if !canonical.is_dir() {
        return Err(GitWorktreeError::InvalidRoot);
    }
    Ok(canonical)
}

fn git_show_toplevel(
    options: &GitWorktreeOptions,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<PathBuf, GitWorktreeError> {
    let out = discover_git(options, cwd, &["rev-parse", "--show-toplevel"], cancel)?;
    let line = stdout_line(&out)?;
    canonicalize_dir(Path::new(&line))
}

fn git_common_dir(
    options: &GitWorktreeOptions,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<PathBuf, GitWorktreeError> {
    let out = discover_git(options, cwd, &["rev-parse", "--git-common-dir"], cancel)?;
    let line = stdout_line(&out)?;
    let path = Path::new(&line);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    canonicalize_dir(&abs)
}

fn discover_git(
    options: &GitWorktreeOptions,
    cwd: &Path,
    args: &[&str],
    cancel: &CancellationToken,
) -> Result<Vec<u8>, GitWorktreeError> {
    // Discovery runs before the RapidLM hook-disable directory exists.
    let hooks = cwd;
    run_git(
        &options.git_program,
        cwd,
        hooks,
        args,
        options.timeout,
        options.max_output_bytes,
        cancel,
    )
    .and_then(|out| {
        if out.status_ok {
            Ok(out.stdout)
        } else {
            Err(GitWorktreeError::NotAGitRepository)
        }
    })
}

/// Lists every `filter.<name>.<subkey>` entry set in `cwd`'s own local
/// `.git/config` (never `--global`/`--system` — those belong to the
/// operator, not to whatever repository is being opened here). A tracked
/// `.gitattributes` can name an arbitrary filter driver (`path filter=x`);
/// the actual smudge/clean/process *command* for that name comes only from
/// git config, so a local config carrying `filter.x.smudge = <shell
/// command>` runs that command on any checkout touching a matching path —
/// including `git worktree add`. Unlike hooks, filter driver names are
/// attacker-chosen, so there is no single config key that disables them
/// all; the caller must instead override every key this reports to an
/// empty value.
///
/// Never fails: any error at any step (git not found, non-zero exit,
/// non-UTF-8 output) is treated the same as "no keys configured", since a
/// failed lookup here must fail toward finding nothing to override, not
/// toward blocking the caller or panicking. The real safety net is that
/// `run_git`'s explicit `-c` overrides always take precedence over
/// whatever this enumeration does or doesn't find.
fn local_filter_config_keys(git_program: &Path, cwd: &Path) -> Vec<String> {
    let output = Command::new(git_program)
        .arg("-C")
        .arg(cwd)
        .args([
            "config",
            "--local",
            "--name-only",
            "--get-regexp",
            r"^filter\.",
        ])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(MAX_FILTER_OVERRIDES)
        .map(str::to_owned)
        .collect()
}

fn run_git(
    git_program: &Path,
    cwd: &Path,
    hooks_dir: &Path,
    args: &[&str],
    timeout: Duration,
    max_output: usize,
    cancel: &CancellationToken,
) -> Result<GitOutput, GitWorktreeError> {
    cancel.check().map_err(|_| GitWorktreeError::Cancelled)?;
    let hooks = path_arg(hooks_dir)?;
    let hooks_cfg = format!("core.hooksPath={hooks}");
    let filter_overrides: Vec<String> = local_filter_config_keys(git_program, cwd)
        .into_iter()
        .map(|key| format!("{key}="))
        .collect();
    let mut command = Command::new(git_program);
    command
        .arg("-C")
        .arg(cwd)
        .arg("-c")
        .arg(&hooks_cfg)
        .arg("-c")
        .arg("advice.detachedHead=false")
        .arg("-c")
        .arg("core.fsmonitor=false");
    for override_arg in &filter_overrides {
        command.arg("-c").arg(override_arg);
    }
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_QUARANTINE_PATH");
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(GitWorktreeError::GitNotFound);
        }
        Err(_) => return Err(GitWorktreeError::GitFailed),
    };
    let stdout = child.stdout.take().ok_or(GitWorktreeError::GitFailed)?;
    let stderr = child.stderr.take().ok_or(GitWorktreeError::GitFailed)?;
    let cap = max_output;
    let stdout_thread = thread::spawn(move || read_capped(stdout, cap));
    let stderr_thread = thread::spawn(move || read_capped(stderr, cap));

    let started = Instant::now();
    let mut polls = 0u32;
    let status = loop {
        if polls.is_multiple_of(CANCEL_STRIDE) && cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(GitWorktreeError::Cancelled);
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(GitWorktreeError::Timeout);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(_) => {
                let _ = child.kill();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(GitWorktreeError::GitFailed);
            }
        }
        polls = polls.saturating_add(1);
    };

    let stdout = stdout_thread
        .join()
        .map_err(|_| GitWorktreeError::GitFailed)?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| GitWorktreeError::GitFailed)?;
    if stdout.1 || stderr.1 {
        return Err(GitWorktreeError::BoundExceeded);
    }
    Ok(GitOutput {
        stdout: stdout.0,
        status_ok: status.success(),
    })
}

fn read_capped(mut pipe: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        match pipe.read(&mut tmp) {
            Ok(0) => return (buf, false),
            Ok(n) => {
                if buf.len().saturating_add(n) > cap {
                    let keep = cap.saturating_sub(buf.len());
                    buf.extend_from_slice(&tmp[..keep]);
                    let mut drain = [0u8; 8192];
                    while let Ok(read) = pipe.read(&mut drain) {
                        if read == 0 {
                            break;
                        }
                    }
                    return (buf, true);
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(_) => return (buf, false),
        }
    }
}

fn ensure_real_dir(path: &Path, root: &Path) -> Result<(), GitWorktreeError> {
    confine_child(path, root)?;
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(GitWorktreeError::PathEscape);
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|_| GitWorktreeError::Io)?;
            if !is_real_dir(path)? {
                return Err(GitWorktreeError::PathEscape);
            }
        }
        Err(_) => return Err(GitWorktreeError::Io),
    }
    confine_dir(
        &fs::canonicalize(path).map_err(|_| GitWorktreeError::Io)?,
        root,
    )
}

fn is_real_dir(path: &Path) -> Result<bool, GitWorktreeError> {
    let meta = fs::symlink_metadata(path).map_err(|_| GitWorktreeError::Io)?;
    Ok(meta.is_dir() && !meta.file_type().is_symlink())
}

fn confine_dir(path: &Path, root: &Path) -> Result<(), GitWorktreeError> {
    if path == root || path.starts_with(root) {
        Ok(())
    } else {
        Err(GitWorktreeError::PathEscape)
    }
}

fn confine_child(path: &Path, root: &Path) -> Result<(), GitWorktreeError> {
    if path == root {
        return Err(GitWorktreeError::PathEscape);
    }
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(GitWorktreeError::PathEscape)
    }
}

fn atomic_write(dest: &Path, bytes: &[u8], root: &Path) -> Result<(), GitWorktreeError> {
    let parent = dest.parent().ok_or(GitWorktreeError::Io)?;
    let parent_canon = fs::canonicalize(parent).map_err(|_| GitWorktreeError::Io)?;
    confine_dir(&parent_canon, root)?;
    if !is_real_dir(&parent_canon)? {
        return Err(GitWorktreeError::PathEscape);
    }
    let name = dest.file_name().ok_or(GitWorktreeError::Io)?;
    let dest_path = parent_canon.join(name);
    confine_child(&dest_path, &parent_canon)?;
    let tmp = parent_canon.join(format!(
        ".{}.{}{TMP_SUFFIX}",
        name.to_string_lossy(),
        std::process::id()
    ));
    confine_child(&tmp, &parent_canon)?;
    let written = (|| -> Result<(), GitWorktreeError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|_| GitWorktreeError::Io)?;
        file.write_all(bytes).map_err(|_| GitWorktreeError::Io)?;
        file.sync_all().map_err(|_| GitWorktreeError::Io)?;
        drop(file);
        fs::rename(&tmp, &dest_path).map_err(|_| GitWorktreeError::Io)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

fn read_confined_file(
    path: &Path,
    root: &Path,
    max_bytes: usize,
) -> Result<Vec<u8>, GitWorktreeError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(GitWorktreeError::ViewNotFound);
        }
        Err(_) => return Err(GitWorktreeError::Io),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(GitWorktreeError::MetadataCorrupt);
    }
    let canon = fs::canonicalize(path).map_err(|_| GitWorktreeError::Io)?;
    confine_child(&canon, root)?;
    if meta.len() > max_bytes as u64 {
        return Err(GitWorktreeError::BoundExceeded);
    }
    let file = File::open(&canon).map_err(|_| GitWorktreeError::Io)?;
    let mut bytes = Vec::new();
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| GitWorktreeError::Io)?;
    if bytes.len() > max_bytes {
        return Err(GitWorktreeError::BoundExceeded);
    }
    Ok(bytes)
}

fn remove_metadata(path: &Path) -> Result<(), GitWorktreeError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(GitWorktreeError::Io),
    }
}

fn open_repos() -> &'static Mutex<HashSet<PathBuf>> {
    OPEN_REPOS.get_or_init(|| Mutex::new(HashSet::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{CreateView, ViewAccess, ViewRegistry, ViewScope};
    use protocol::{AgentId, RepoId, RepoPath};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    struct Fixture {
        dir: PathBuf,
        store: Option<GitWorktreeStore>,
        head_sha: String,
        branch: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.store.take();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn git_user(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git")
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = git_user(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn view(access: ViewAccess, revision: &str) -> WorkspaceView {
        let registry = ViewRegistry::new();
        let mut spec = CreateView::new(
            RepoId::new(),
            WorkspaceBackend::GitWorktree,
            revision,
            access,
        );
        if access.is_writable() {
            spec = spec.with_write_owner(AgentId::new());
        }
        registry.create(spec, &cancel()).expect("create view")
    }

    fn fixture() -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-git-wt-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        git_ok(&dir, &["init", "-b", "user-main"]);
        git_ok(
            &dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        fs::write(dir.join("tracked.txt"), b"base\n").expect("seed");
        git_ok(&dir, &["add", "tracked.txt"]);
        git_ok(
            &dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-m",
                "seed",
            ],
        );
        fs::write(dir.join("dirty.txt"), b"user dirty\n").expect("dirty");
        let head = git_user(&dir, &["rev-parse", "HEAD"]);
        let sha = String::from_utf8(head.stdout)
            .expect("sha utf8")
            .trim()
            .to_owned();
        let store = GitWorktreeStore::open(&dir, &cancel()).expect("open store");
        Fixture {
            dir,
            store: Some(store),
            head_sha: sha,
            branch: "user-main".into(),
        }
    }

    fn store(fx: &Fixture) -> &GitWorktreeStore {
        fx.store.as_ref().expect("store")
    }

    fn user_branch(dir: &Path) -> String {
        let out = git_user(dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    }

    fn user_sha(dir: &Path) -> String {
        let out = git_user(dir, &["rev-parse", "HEAD"]);
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    }

    fn ref_exists(dir: &Path, name: &str) -> bool {
        git_user(dir, &["show-ref", "--verify", "--quiet", "--", name])
            .status
            .success()
    }

    #[test]
    fn parallel_worktrees_get_distinct_directories_and_refs() {
        let fx = fixture();
        let first = store(&fx)
            .create_view(&view(ViewAccess::ReadWrite, &fx.head_sha), &cancel())
            .expect("first");
        let second = store(&fx)
            .create_view(&view(ViewAccess::ReadWrite, &fx.head_sha), &cancel())
            .expect("second");
        assert_ne!(first.worktree_path(), second.worktree_path());
        assert_ne!(first.git_ref(), second.git_ref());
        assert!(first.git_ref().starts_with(GIT_WORKTREE_REF_NAMESPACE));
        assert!(second.git_ref().starts_with(GIT_WORKTREE_REF_NAMESPACE));
        assert!(
            first
                .worktree_path()
                .starts_with(store(&fx).git_common_dir())
        );
        assert!(
            second
                .worktree_path()
                .starts_with(store(&fx).git_common_dir())
        );
        assert!(first.worktree_path().join("tracked.txt").is_file());
        assert!(second.worktree_path().join("tracked.txt").is_file());
        fs::write(first.worktree_path().join("only-first.txt"), b"a\n").expect("write first");
        assert!(!second.worktree_path().join("only-first.txt").exists());
        assert_eq!(user_branch(&fx.dir), fx.branch);
        assert_eq!(user_sha(&fx.dir), fx.head_sha);
        assert!(ref_exists(&fx.dir, first.git_ref()));
        assert!(ref_exists(&fx.dir, second.git_ref()));
        let branches =
            String::from_utf8(git_user(&fx.dir, &["branch", "--list"]).stdout).expect("utf8");
        assert!(!branches.contains(&first.view_id().to_string()));
        assert!(!branches.contains(&second.view_id().to_string()));
    }

    #[test]
    fn create_view_does_not_change_user_branch_on_dirty_tree() {
        let fx = fixture();
        assert!(fx.dir.join("dirty.txt").is_file());
        let record = store(&fx)
            .create_view(&view(ViewAccess::ReadWrite, "HEAD"), &cancel())
            .expect("create");
        assert_eq!(user_branch(&fx.dir), "user-main");
        assert_eq!(user_sha(&fx.dir), fx.head_sha);
        assert_eq!(
            fs::read(fx.dir.join("dirty.txt")).expect("dirty"),
            b"user dirty\n"
        );
        assert!(!record.worktree_path().join("dirty.txt").exists());
        assert_eq!(record.resolved_commit(), fx.head_sha);
        assert_eq!(record.record_state(), GitWorktreeRecordState::Active);
    }

    #[test]
    fn metadata_persists_and_reloads_after_reopen() {
        let mut fx = fixture();
        let created = store(&fx)
            .create_view(&view(ViewAccess::ReadWrite, &fx.head_sha), &cancel())
            .expect("create");
        let loaded = store(&fx)
            .load_view(created.view_id(), &cancel())
            .expect("load");
        assert_eq!(loaded.view_id(), created.view_id());
        assert_eq!(loaded.git_ref(), created.git_ref());
        assert_eq!(loaded.resolved_commit(), created.resolved_commit());
        assert_eq!(loaded.record_state(), GitWorktreeRecordState::Active);
        let listed = store(&fx).list_views(&cancel()).expect("list");
        assert_eq!(listed.len(), 1);

        let view_id = created.view_id();
        let git_ref = created.git_ref().to_owned();
        fx.store.take();
        let reopened = GitWorktreeStore::open(&fx.dir, &cancel()).expect("reopen");
        let again = reopened.load_view(view_id, &cancel()).expect("reload");
        assert_eq!(again.git_ref(), git_ref);
        assert_eq!(again.view().base_revision(), fx.head_sha);
        reopened
            .remove_view(view_id, &cancel())
            .expect("remove after reopen");
        assert!(!ref_exists(&fx.dir, &git_ref));
        drop(reopened);
    }

    #[test]
    fn cleanup_failure_is_reported_and_not_destructive() {
        let fx = fixture();
        let record = store(&fx)
            .create_view(&view(ViewAccess::ReadWrite, &fx.head_sha), &cancel())
            .expect("create");
        let extra = record.worktree_path().join("agent.txt");
        fs::write(&extra, b"keep me\n").expect("dirty worktree");
        let err = store(&fx)
            .remove_view(record.view_id(), &cancel())
            .expect_err("dirty remove");
        assert_eq!(err, GitWorktreeError::CleanupFailed);
        assert!(extra.is_file());
        assert!(record.worktree_path().exists());
        assert!(ref_exists(&fx.dir, record.git_ref()));
        let loaded = store(&fx)
            .load_view(record.view_id(), &cancel())
            .expect("metadata remains");
        assert_eq!(loaded.record_state(), GitWorktreeRecordState::CleanupFailed);
        assert_eq!(user_branch(&fx.dir), fx.branch);
        assert_eq!(fs::read(&extra).expect("bytes"), b"keep me\n");

        fs::remove_file(&extra).expect("recover");
        store(&fx)
            .remove_view(record.view_id(), &cancel())
            .expect("retry remove");
        assert!(!record.worktree_path().exists());
        assert!(!ref_exists(&fx.dir, record.git_ref()));
        assert_eq!(
            store(&fx).load_view(record.view_id(), &cancel()),
            Err(GitWorktreeError::ViewNotFound)
        );
    }

    #[test]
    fn rollback_create_preserves_the_record_when_cleanup_cannot_remove_the_worktree() {
        // Simulates `create_view`'s error branch calling `rollback_create`
        // after `finish_create` fails post-`git worktree add` (e.g. a
        // concurrent HEAD move) — by that point a real worktree directory
        // exists on disk. Dirtying it here reproduces the same non-forced
        // `git worktree remove` refusal `cleanup_failure_is_reported_and_
        // not_destructive` already exercises for `remove_view`, but through
        // `rollback_create`'s own path instead.
        let fx = fixture();
        let st = store(&fx);
        let record = st
            .create_view(&view(ViewAccess::ReadWrite, &fx.head_sha), &cancel())
            .expect("create");
        let extra = record.worktree_path().join("dirty.txt");
        fs::write(&extra, b"uncommitted\n").expect("dirty worktree");

        st.rollback_create(
            record.view_id(),
            record.git_ref(),
            record.worktree_path(),
            &cancel(),
        );

        // The worktree, ref, and metadata must all survive cleanup failure,
        // marked `CleanupFailed` — not silently deleted out from under a
        // directory that still exists on disk with no record left pointing
        // at it.
        assert!(record.worktree_path().exists());
        assert!(ref_exists(&fx.dir, record.git_ref()));
        let loaded = st
            .load_view(record.view_id(), &cancel())
            .expect("metadata remains");
        assert_eq!(loaded.record_state(), GitWorktreeRecordState::CleanupFailed);
    }

    #[test]
    fn option_like_revision_is_rejected_without_git_flag_injection() {
        let fx = fixture();
        let marker = fx.dir.join("pwned");
        let registry = ViewRegistry::new();
        let crafted = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::GitWorktree,
                    "--output=pwned",
                    ViewAccess::ReadWrite,
                )
                .with_write_owner(AgentId::new()),
                &cancel(),
            )
            .expect("registry accepts token");
        let err = store(&fx)
            .create_view(&crafted, &cancel())
            .expect_err("injection");
        assert_eq!(err, GitWorktreeError::InvalidRevision);
        assert!(!marker.exists());
        assert_eq!(user_branch(&fx.dir), fx.branch);
        assert!(store(&fx).list_views(&cancel()).expect("list").is_empty());
    }

    #[test]
    fn wrong_backend_and_read_only_and_closed_fail_closed() {
        let fx = fixture();
        let registry = ViewRegistry::new();
        let direct = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::Direct,
                    "HEAD",
                    ViewAccess::ReadWrite,
                ),
                &cancel(),
            )
            .expect("direct");
        assert_eq!(
            store(&fx).create_view(&direct, &cancel()),
            Err(GitWorktreeError::WrongBackend)
        );
        assert_eq!(
            store(&fx).create_view(&view(ViewAccess::ReadOnly, &fx.head_sha), &cancel()),
            Err(GitWorktreeError::ReadOnlyView)
        );
        let closed_registry = ViewRegistry::new();
        let live = closed_registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::GitWorktree,
                    fx.head_sha.clone(),
                    ViewAccess::ReadWrite,
                ),
                &cancel(),
            )
            .expect("live");
        closed_registry.close(live.id(), &cancel()).expect("close");
        let closed = closed_registry
            .get(live.id(), &cancel())
            .expect("get closed");
        assert_eq!(
            store(&fx).create_view(&closed, &cancel()),
            Err(GitWorktreeError::InvalidState)
        );
    }

    #[test]
    fn second_store_and_duplicate_view_fail_closed() {
        let fx = fixture();
        assert_eq!(
            GitWorktreeStore::open(&fx.dir, &cancel()).err(),
            Some(GitWorktreeError::AlreadyOpen)
        );
        let spec = view(ViewAccess::ReadWrite, &fx.head_sha);
        store(&fx).create_view(&spec, &cancel()).expect("create");
        assert_eq!(
            store(&fx).create_view(&spec, &cancel()),
            Err(GitWorktreeError::AlreadyExists)
        );
    }

    #[test]
    fn non_repo_and_cancelled_operations_fail_closed() {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-git-wt-empty-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        assert_eq!(
            GitWorktreeStore::open(&dir, &cancel()).err(),
            Some(GitWorktreeError::NotAGitRepository)
        );
        let _ = fs::remove_dir_all(&dir);

        let fx = fixture();
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            store(&fx).create_view(&view(ViewAccess::ReadWrite, &fx.head_sha), &token),
            Err(GitWorktreeError::Cancelled)
        );
        assert!(store(&fx).list_views(&cancel()).expect("list").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_worktree_parent_is_rejected() {
        let fx = fixture();
        let parent = store(&fx)
            .git_common_dir()
            .join(RAPIDLM_DIR)
            .join(WORKTREES_DIR);
        fs::remove_dir(&parent).expect("remove worktrees");
        let outside = std::env::temp_dir().join(format!(
            "rapidlm-git-wt-out-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, &parent).expect("symlink");
        let err = store(&fx)
            .create_view(&view(ViewAccess::ReadWrite, &fx.head_sha), &cancel())
            .expect_err("symlink parent");
        assert_eq!(err, GitWorktreeError::PathEscape);
        assert!(fs::read_dir(&outside).expect("outside").next().is_none());
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn git_timeout_is_enforced() {
        let mut fx = fixture();
        let wrapper_dir = fx.dir.join("wrapper");
        fs::create_dir_all(&wrapper_dir).expect("wrapper dir");
        let wrapper = wrapper_dir.join("git");
        let real = "/usr/bin/git";
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nseen_worktree=0\nfor a in \"$@\"; do\n  if [ \"$a\" = worktree ]; then seen_worktree=1; fi\n  if [ \"$seen_worktree\" = 1 ] && [ \"$a\" = add ]; then sleep 8; exit 0; fi\ndone\nexec {real} \"$@\"\n"
            ),
        )
        .expect("wrapper");
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&wrapper).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&wrapper, perms).expect("chmod");

        fx.store.take();
        let store = GitWorktreeStore::open_with(
            &fx.dir,
            GitWorktreeOptions::default()
                .with_git_program(&wrapper)
                .with_timeout(Duration::from_millis(800)),
            &cancel(),
        )
        .expect("open wrapper");
        let err = store
            .create_view(&view(ViewAccess::ReadWrite, &fx.head_sha), &cancel())
            .expect_err("timeout");
        assert_eq!(err, GitWorktreeError::Timeout);
        assert_eq!(user_branch(&fx.dir), fx.branch);
        drop(store);
    }

    #[test]
    fn error_display_does_not_echo_paths_or_revisions() {
        for err in [
            GitWorktreeError::PathEscape,
            GitWorktreeError::InvalidRevision,
            GitWorktreeError::CleanupFailed,
            GitWorktreeError::MetadataCorrupt,
            GitWorktreeError::GitFailed,
        ] {
            let text = err.to_string();
            for leaked in [
                "password",
                "hunter2",
                "/etc/passwd",
                "refs/heads",
                "deadbeef",
            ] {
                assert!(!text.contains(leaked), "{text}");
            }
        }
    }

    #[test]
    fn worktree_limit_is_enforced() {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-git-wt-limit-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        git_ok(&dir, &["init", "-b", "user-main"]);
        git_ok(
            &dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let sha = user_sha(&dir);
        let store = GitWorktreeStore::open_with(
            &dir,
            GitWorktreeOptions::default().with_max_worktrees(1),
            &cancel(),
        )
        .expect("open");
        store
            .create_view(&view(ViewAccess::ReadWrite, &sha), &cancel())
            .expect("first");
        let err = store
            .create_view(&view(ViewAccess::ReadWrite, &sha), &cancel())
            .expect_err("limit");
        assert_eq!(err, GitWorktreeError::WorktreeLimit);
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persisted_view_keeps_scope_and_generation() {
        let fx = fixture();
        let registry = ViewRegistry::new();
        let scope = ViewScope::prefixes(vec![RepoPath::parse("src").expect("src")]).expect("scope");
        let spec = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::GitWorktree,
                    fx.head_sha.clone(),
                    ViewAccess::ReadWrite,
                )
                .with_write_owner(AgentId::new())
                .with_scope(scope.clone()),
                &cancel(),
            )
            .expect("registry view");
        assert_eq!(spec.generation(), 1);
        let record = store(&fx)
            .create_view(&spec, &cancel())
            .expect("create worktree");
        assert_eq!(record.view().scope(), &scope);
        assert_eq!(record.view().generation(), spec.generation());
        let loaded = store(&fx)
            .load_view(record.view_id(), &cancel())
            .expect("load");
        assert_eq!(loaded.view().scope(), &scope);
        assert!(!loaded.view().scope().is_repo_wide());
        assert_eq!(loaded.view().generation(), 1);
        assert_eq!(loaded.view().base_revision(), fx.head_sha);
    }

    #[cfg(unix)]
    #[test]
    fn user_git_hooks_do_not_run_on_worktree_create() {
        let fx = fixture();
        let hooks = fx.dir.join(".git/hooks");
        fs::create_dir_all(&hooks).expect("hooks dir");
        let marker = fx.dir.join("hook-fired");
        let hook = hooks.join("post-checkout");
        fs::write(
            &hook,
            format!("#!/bin/sh\nprintf fired > '{}'\n", marker.display()),
        )
        .expect("hook");
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook, perms).expect("chmod");

        store(&fx)
            .create_view(&view(ViewAccess::ReadWrite, &fx.head_sha), &cancel())
            .expect("create");
        assert!(
            !marker.exists(),
            "user post-checkout hook must not run during worktree add"
        );
        assert_eq!(user_branch(&fx.dir), fx.branch);
        assert_eq!(user_sha(&fx.dir), fx.head_sha);
    }

    #[test]
    fn worktree_create_does_not_run_a_local_filter_drivers_smudge_command() {
        let fx = fixture();
        let marker = fx.dir.join("filter-fired");
        git_ok(
            &fx.dir,
            &[
                "config",
                "filter.x.smudge",
                &format!("sh -c 'printf fired > \"{}\"; cat'", marker.display()),
            ],
        );
        fs::write(fx.dir.join(".gitattributes"), b"tracked.txt filter=x\n").expect("attrs");
        git_ok(&fx.dir, &["add", ".gitattributes"]);
        git_ok(
            &fx.dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-m",
                "attrs",
            ],
        );
        let sha = user_sha(&fx.dir);
        assert!(!marker.exists(), "filter must not fire before worktree add");

        let created = store(&fx)
            .create_view(&view(ViewAccess::ReadWrite, &sha), &cancel())
            .expect("create");

        assert!(
            !marker.exists(),
            "a local filter.x.smudge command must not run during worktree add"
        );
        assert_eq!(
            fs::read(created.worktree_path().join("tracked.txt")).expect("read checkout"),
            b"base\n",
            "content must still check out correctly once the filter is neutralized"
        );
    }
}
