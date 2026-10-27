#![forbid(unsafe_code)]

pub mod backends {
    pub mod direct;
    pub mod git_worktree;
    pub mod overlay;
    pub mod remote;
}
pub mod checkpoint;
pub mod external_mutation;
pub mod merge;
pub mod patch {
    pub mod apply;
    pub mod model;
}
pub mod transaction;
pub mod view;

/// The project's governance directory component: its settings (hooks,
/// permission rules), ledger, receipts and trust records. The host owns
/// these; a model write tool never does — a model that could rewrite the
/// project's hooks or permission rules could grant itself everything.
pub const GOVERNED_DIR_COMPONENT: &str = ".rapidlm";

/// Whether one path component names the governance directory
/// (case-insensitively, the same way the `.git` checks read theirs: the
/// two most common desktop filesystems treat `.RAPIDLM` and `.rapidlm` as
/// the same directory).
pub fn is_governance_component(part: &str) -> bool {
    part.eq_ignore_ascii_case(GOVERNED_DIR_COMPONENT)
}

/// Whether a workspace path is inside the governance directory at all.
pub fn is_governance_path(path: &protocol::RepoPath) -> bool {
    path.components().any(is_governance_component)
}

/// The governance-directory writes the product itself asks for: the plan
/// file, plan history, the todo list, and the team memory. Everything else
/// under `.rapidlm/` is host-owned and refused to model write tools.
pub fn is_sanctioned_governance_write(path: &protocol::RepoPath) -> bool {
    let parts: Vec<&str> = path.components().collect();
    if !parts
        .first()
        .is_some_and(|part| is_governance_component(part))
    {
        return false;
    }
    match parts.get(1).copied() {
        Some("plan.md") | Some("todos.json") | Some("MEMORY.md") => parts.len() == 2,
        Some("plans") => parts.len() == 3 && parts[2].ends_with(".md") && parts[2] != ".md",
        _ => false,
    }
}

/// Whether a mutating operation on `path` is refused as a governance-dir
/// write: inside `.rapidlm/` and not one of the sanctioned files.
pub fn is_refused_governance_write(path: &protocol::RepoPath) -> bool {
    is_governance_path(path) && !is_sanctioned_governance_write(path)
}

pub use backends::direct::{
    DirectBackend, DirectCheckpoint, DirectError, DirectOptions, DirectResolveMode, JournalEntry,
    JournalOp, MAX_CHECKPOINT_LABEL_BYTES, MAX_CHECKPOINTS, MAX_DIRECT_FILE_BYTES,
    MAX_JOURNAL_ENTRIES, MAX_SYMLINK_HOPS, ResolvedRepoPath,
};
pub use backends::git_worktree::{
    GIT_WORKTREE_REF_NAMESPACE, GIT_WORKTREE_SCHEMA, GIT_WORKTREE_SCHEMA_VERSION, GitWorktreeError,
    GitWorktreeOptions, GitWorktreeRecord, GitWorktreeRecordState, GitWorktreeStore,
    MAX_GIT_OUTPUT_BYTES, MAX_GIT_WORKTREES,
};
pub use backends::overlay::{
    MAX_OVERLAY_FILE_BYTES, MAX_OVERLAY_SLOTS, OverlayBackend, OverlayError, OverlayOptions,
};
pub use backends::remote::{
    RemoteSnapshotBackend, RemoteSnapshotError, SNAPSHOT_MEDIA_TYPE, SnapshotBlobStore,
    SnapshotManifest,
};
pub use checkpoint::{
    CHECKPOINT_SCHEMA, CHECKPOINT_SCHEMA_VERSION, Checkpoint, CheckpointError, CheckpointId,
    CheckpointManager, MAX_CHECKPOINT_BYTES, MAX_CHECKPOINT_FILES, MAX_MANAGER_CHECKPOINTS,
    REWIND_PREVIEW_SCHEMA, REWIND_PREVIEW_SCHEMA_VERSION, RewindConflict, RewindMode, RewindOp,
    RewindOpKind, RewindPreview,
};
pub use external_mutation::{
    ExternalMutation, MAX_MUTATION_JOURNAL, MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_FILE_BYTES,
    MAX_SNAPSHOT_FILES, MAX_WALK_DEPTH, MUTATION_SCHEMA, MUTATION_SCHEMA_VERSION,
    MutationAttribution, MutationDetector, MutationError, MutationKind, ReconcileHint,
    SNAPSHOT_SCHEMA, SNAPSHOT_SCHEMA_VERSION, SnapshotOptions, WorkspaceSnapshot, detect,
    detect_checked, reconcile,
};
pub use merge::{
    MAX_MERGE_CONFLICTS, MAX_MERGE_OPS, MERGE_PREVIEW_SCHEMA, MERGE_PREVIEW_SCHEMA_VERSION,
    MergeConflict, MergeConflictKind, MergeError, MergeOpKind, MergePreview, MergeView,
    VerificationCheck, VerificationPlan, preview_merge,
};
pub use patch::apply::{
    ApplyError, MAX_OVERLAY_BYTES, MAX_OVERLAY_FILES, OverlayFile, StagedChange, StagedPatch,
    StagingOverlay, apply_patch,
};
pub use patch::model::{
    BytePos, MAX_OP_CONTENT_BYTES, MAX_PATCH_CONTENT_BYTES, MAX_PATCH_OPS, PATCH_SCHEMA,
    PATCH_SCHEMA_VERSION, PatchError, PatchOp, SemanticPatch,
};
pub use transaction::{
    AcceptHook, COMMIT_RECEIPT_SCHEMA, COMMIT_RECEIPT_SCHEMA_VERSION, CommitReceipt, HookContext,
    MAX_TRANSACTIONS, MAX_VERIFICATION_HOOKS, TransactionError, TransactionId, TransactionManager,
    TransactionState, VerificationHook, WorkspaceTransaction, commit_transaction, rollback,
};
pub use view::{
    CancellationToken, CreateView, MAX_BASE_REVISION_BYTES, MAX_VIEW_SCOPE_PREFIXES,
    MAX_WORKSPACE_VIEWS, ViewAccess, ViewRegistry, ViewScope, WORKSPACE_VIEW_SCHEMA,
    WORKSPACE_VIEW_SCHEMA_VERSION, WorkspaceBackend, WorkspaceError, WorkspaceState, WorkspaceView,
};
