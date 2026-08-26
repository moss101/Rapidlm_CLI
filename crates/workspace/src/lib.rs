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
