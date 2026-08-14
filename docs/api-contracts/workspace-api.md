# Workspace / VCS Transaction API

```rust
#[async_trait]
pub trait WorkspaceManager {
    async fn create_view(&self, req: CreateView) -> Result<WorkspaceView, WorkspaceError>;
    async fn stage_patch(&self, view: WorkspaceViewId, patch: SemanticPatch) -> Result<StagedPatch, WorkspaceError>;
    async fn detect_external_mutations(&self, view: WorkspaceViewId) -> Result<Vec<ExternalMutation>, WorkspaceError>;
    async fn checkpoint(&self, view: WorkspaceViewId, label: String) -> Result<Checkpoint, WorkspaceError>;
    async fn merge_view(&self, req: MergeView) -> Result<MergePreview, WorkspaceError>;
    async fn commit_transaction(&self, tx: TransactionId) -> Result<CommitReceipt, WorkspaceError>;
    async fn rollback(&self, tx: TransactionId) -> Result<(), WorkspaceError>;
}
```

`stage_patch` is atomic: all preimage hashes are checked before any logical patch becomes staged. Merge into a parent runs in an overlay/staging view first. Conflicts return machine-readable path/op conflicts; no lossy auto-resolution is hidden.
