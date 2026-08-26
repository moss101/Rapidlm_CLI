//! Transactional merge commit of a child preview into a staging parent.
//!
//! A conflict-free [`MergePreview`] is applied to an isolated parent overlay,
//! required checks and verification hooks run, and only then does the overlay
//! become parent-visible. Failure or cancellation rolls the staged parent
//! back. The source checkout is never written.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};

use protocol::{AgentId, ArtifactId, ErrorCode, WorkspaceViewId};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::backends::direct::DirectBackend;
use crate::merge::{MergePreview, VerificationCheck};
use crate::patch::apply::{ApplyError, StagedPatch, StagingOverlay, apply_patch};
use crate::patch::model::{PatchError, SemanticPatch};
use crate::view::{CancellationToken, WorkspaceState, WorkspaceView};

/// Wire schema name for [`CommitReceipt`].
pub const COMMIT_RECEIPT_SCHEMA: &str = "rapidlm.commit_receipt";

/// v1 schema version for commit receipts.
pub const COMMIT_RECEIPT_SCHEMA_VERSION: u16 = 1;

/// Maximum in-flight plus finalized transactions retained by one manager.
pub const MAX_TRANSACTIONS: usize = 256;

/// Maximum verification hooks accepted on one commit.
pub const MAX_VERIFICATION_HOOKS: usize = 16;

const RECEIPT_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "transaction_id",
    "parent_view_id",
    "child_view_id",
    "parent_revision",
    "preview_hash",
    "patch_hash",
    "change_count",
];

/// Opaque identifier for one merge transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct TransactionId(u64);

/// Lifecycle of a merge transaction. Only [`TransactionState::Staged`] can
/// become parent-visible.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TransactionState {
    Staged,
    Committed,
    RolledBack,
}

/// Observational handle for a stored merge transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceTransaction {
    id: TransactionId,
    parent_view_id: WorkspaceViewId,
    child_view_id: WorkspaceViewId,
    parent_revision: String,
    preview_hash: ArtifactId,
    patch_hash: ArtifactId,
    change_count: usize,
    author: AgentId,
    state: TransactionState,
}

/// Proof that a preview passed required checks and became parent-visible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitReceipt {
    transaction_id: TransactionId,
    parent_view_id: WorkspaceViewId,
    child_view_id: WorkspaceViewId,
    parent_revision: String,
    preview_hash: ArtifactId,
    patch_hash: ArtifactId,
    change_count: usize,
}

/// In-process store for staged and committed parent overlays.
pub struct TransactionManager {
    inner: Mutex<Inner>,
    max_transactions: usize,
}

/// Observational input to a verification hook. Hooks cannot mutate staging.
#[derive(Clone, Copy, Debug)]
pub struct HookContext<'a> {
    transaction_id: TransactionId,
    preview: &'a MergePreview,
    staged: &'a StagedPatch,
}

/// Bounded, fail-closed hook run after staging and before parent visibility.
pub trait VerificationHook {
    fn name(&self) -> &'static str;
    fn verify(
        &self,
        ctx: &HookContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), TransactionError>;
}

/// Hook that performs no extra work. Required checks still run in code.
#[derive(Clone, Copy, Debug, Default)]
pub struct AcceptHook;

/// Typed transaction failure. Display never echoes paths, revisions, or payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionError {
    Cancelled,
    PreviewConflict,
    StaleParentRevision,
    Preimage,
    HookFailed,
    RequiredCheckFailed {
        check: VerificationCheck,
    },
    TransactionNotFound {
        transaction_id: TransactionId,
    },
    AlreadyFinalized {
        transaction_id: TransactionId,
        state: TransactionState,
    },
    ParentBusy {
        parent_view_id: WorkspaceViewId,
    },
    ViewMismatch,
    ReadOnlyView,
    InvalidState,
    GitScopeRequired,
    BoundExceeded,
    TransactionLimit {
        limit: usize,
    },
    Apply(ApplyError),
    Patch(PatchError),
    LockPoisoned,
    UnknownVariant,
    UnsupportedSchema,
    UnsupportedSchemaVersion,
}

struct Inner {
    next_id: u64,
    txs: BTreeMap<TransactionId, StoredTx>,
    staged_by_parent: BTreeMap<WorkspaceViewId, TransactionId>,
    visible: BTreeMap<WorkspaceViewId, VisibleCommit>,
}

#[derive(Clone)]
struct StoredTx {
    meta: WorkspaceTransaction,
    preview: MergePreview,
    staged: Option<StagedPatch>,
}

#[derive(Clone)]
struct VisibleCommit {
    receipt: CommitReceipt,
    overlay: StagingOverlay,
}

impl TransactionId {
    pub const fn from_raw(id: u64) -> Self {
        Self(id)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl TransactionState {
    pub const ALL: &'static [Self] = &[Self::Staged, Self::Committed, Self::RolledBack];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Committed => "committed",
            Self::RolledBack => "rolled_back",
        }
    }

    pub const fn is_final(self) -> bool {
        matches!(self, Self::Committed | Self::RolledBack)
    }
}

impl WorkspaceTransaction {
    pub fn id(&self) -> TransactionId {
        self.id
    }

    pub fn parent_view_id(&self) -> WorkspaceViewId {
        self.parent_view_id
    }

    pub fn child_view_id(&self) -> WorkspaceViewId {
        self.child_view_id
    }

    pub fn parent_revision(&self) -> &str {
        &self.parent_revision
    }

    pub fn preview_hash(&self) -> ArtifactId {
        self.preview_hash
    }

    pub fn patch_hash(&self) -> ArtifactId {
        self.patch_hash
    }

    pub fn change_count(&self) -> usize {
        self.change_count
    }

    pub fn author(&self) -> AgentId {
        self.author
    }

    pub fn state(&self) -> TransactionState {
        self.state
    }
}

impl CommitReceipt {
    pub fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    pub fn parent_view_id(&self) -> WorkspaceViewId {
        self.parent_view_id
    }

    pub fn child_view_id(&self) -> WorkspaceViewId {
        self.child_view_id
    }

    pub fn parent_revision(&self) -> &str {
        &self.parent_revision
    }

    pub fn preview_hash(&self) -> ArtifactId {
        self.preview_hash
    }

    pub fn patch_hash(&self) -> ArtifactId {
        self.patch_hash
    }

    pub fn change_count(&self) -> usize {
        self.change_count
    }
}

impl HookContext<'_> {
    pub fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    pub fn preview(&self) -> &MergePreview {
        self.preview
    }

    pub fn staged(&self) -> &StagedPatch {
        self.staged
    }
}

impl VerificationHook for AcceptHook {
    fn name(&self) -> &'static str {
        "accept"
    }

    fn verify(
        &self,
        _ctx: &HookContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), TransactionError> {
        check_cancel(cancel)
    }
}

impl TransactionManager {
    pub fn new() -> Self {
        Self::with_limit(MAX_TRANSACTIONS)
    }

    pub fn with_limit(max_transactions: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                next_id: 1,
                txs: BTreeMap::new(),
                staged_by_parent: BTreeMap::new(),
                visible: BTreeMap::new(),
            }),
            max_transactions,
        }
    }

    /// Bind a preview to a staging parent overlay. Parent checkout is not written.
    pub fn begin_transaction(
        &self,
        preview: &MergePreview,
        parent: &WorkspaceView,
        source: &DirectBackend,
        author: AgentId,
        cancel: &CancellationToken,
    ) -> Result<TransactionId, TransactionError> {
        check_cancel(cancel)?;
        check_parent_binding(preview, parent, source)?;
        verify_required_checks(preview, parent, None)?;
        let staged = stage_preview(preview, parent, source, author, cancel)?;
        check_cancel(cancel)?;

        let mut inner = self.lock()?;
        check_cancel(cancel)?;
        if inner.txs.len() >= self.max_transactions {
            return Err(TransactionError::TransactionLimit {
                limit: self.max_transactions,
            });
        }
        if inner
            .staged_by_parent
            .contains_key(&preview.parent_view_id())
        {
            return Err(TransactionError::ParentBusy {
                parent_view_id: preview.parent_view_id(),
            });
        }
        let id = TransactionId(inner.next_id);
        inner.next_id = inner.next_id.saturating_add(1);
        let meta = WorkspaceTransaction {
            id,
            parent_view_id: preview.parent_view_id(),
            child_view_id: preview.child_view_id(),
            parent_revision: preview.parent_revision().to_owned(),
            preview_hash: preview.preview_hash(),
            patch_hash: staged.patch_hash(),
            change_count: staged.change_count(),
            author,
            state: TransactionState::Staged,
        };
        inner.txs.insert(
            id,
            StoredTx {
                meta: meta.clone(),
                preview: preview.clone(),
                staged: Some(staged),
            },
        );
        inner.staged_by_parent.insert(preview.parent_view_id(), id);
        Ok(id)
    }

    /// Re-verify preview revision and required checks, run hooks, then publish.
    ///
    /// Verification or hook failure rolls back the staged parent. Parent
    /// visibility is assigned only after every required check passes.
    pub fn commit_transaction(
        &self,
        tx: TransactionId,
        parent: &WorkspaceView,
        hooks: &[&dyn VerificationHook],
        cancel: &CancellationToken,
    ) -> Result<CommitReceipt, TransactionError> {
        if cancel.is_cancelled() {
            self.rollback_on_failure(tx);
            return Err(TransactionError::Cancelled);
        }
        if hooks.len() > MAX_VERIFICATION_HOOKS {
            self.rollback_on_failure(tx);
            return Err(TransactionError::BoundExceeded);
        }

        let stored = {
            let inner = self.lock()?;
            load_staged(&inner, tx)?
        };

        let outcome = verify_and_hook(&stored, parent, hooks, cancel);
        let mut inner = self.lock()?;
        let current = match inner.txs.get_mut(&tx) {
            Some(current) => current,
            None => return Err(TransactionError::TransactionNotFound { transaction_id: tx }),
        };
        if current.meta.state != TransactionState::Staged {
            return Err(TransactionError::AlreadyFinalized {
                transaction_id: tx,
                state: current.meta.state,
            });
        }

        match outcome {
            Ok(()) => {
                if parent.base_revision() != current.preview.parent_revision()
                    || parent.id() != current.preview.parent_view_id()
                {
                    abort_staged(&mut inner, tx);
                    return Err(TransactionError::StaleParentRevision);
                }
                if cancel.is_cancelled() {
                    abort_staged(&mut inner, tx);
                    return Err(TransactionError::Cancelled);
                }
                let staged = current.staged.clone().ok_or(TransactionError::Preimage)?;
                let receipt = CommitReceipt {
                    transaction_id: tx,
                    parent_view_id: current.preview.parent_view_id(),
                    child_view_id: current.preview.child_view_id(),
                    parent_revision: current.preview.parent_revision().to_owned(),
                    preview_hash: current.preview.preview_hash(),
                    patch_hash: staged.patch_hash(),
                    change_count: staged.change_count(),
                };
                current.meta.state = TransactionState::Committed;
                inner.staged_by_parent.remove(&receipt.parent_view_id);
                inner.visible.insert(
                    receipt.parent_view_id,
                    VisibleCommit {
                        receipt: receipt.clone(),
                        overlay: staged.overlay().clone(),
                    },
                );
                Ok(receipt)
            }
            Err(err) => {
                abort_staged(&mut inner, tx);
                Err(err)
            }
        }
    }

    /// Discard a staged parent overlay. Committed visibility cannot be undone.
    pub fn rollback(
        &self,
        tx: TransactionId,
        cancel: &CancellationToken,
    ) -> Result<(), TransactionError> {
        check_cancel(cancel)?;
        let mut inner = self.lock()?;
        check_cancel(cancel)?;
        let current = inner
            .txs
            .get(&tx)
            .ok_or(TransactionError::TransactionNotFound { transaction_id: tx })?;
        match current.meta.state {
            TransactionState::RolledBack => Ok(()),
            TransactionState::Committed => Err(TransactionError::AlreadyFinalized {
                transaction_id: tx,
                state: TransactionState::Committed,
            }),
            TransactionState::Staged => {
                abort_staged(&mut inner, tx);
                Ok(())
            }
        }
    }

    pub fn get(
        &self,
        tx: TransactionId,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceTransaction, TransactionError> {
        check_cancel(cancel)?;
        let inner = self.lock()?;
        check_cancel(cancel)?;
        inner
            .txs
            .get(&tx)
            .map(|stored| stored.meta.clone())
            .ok_or(TransactionError::TransactionNotFound { transaction_id: tx })
    }

    /// Overlay published to the parent view, if any.
    pub fn parent_overlay(&self, parent_view_id: WorkspaceViewId) -> Option<StagingOverlay> {
        self.inner.lock().ok().and_then(|inner| {
            inner
                .visible
                .get(&parent_view_id)
                .map(|v| v.overlay.clone())
        })
    }

    /// Receipt published to the parent view, if any.
    pub fn parent_visibility(&self, parent_view_id: WorkspaceViewId) -> Option<CommitReceipt> {
        self.inner.lock().ok().and_then(|inner| {
            inner
                .visible
                .get(&parent_view_id)
                .map(|v| v.receipt.clone())
        })
    }

    fn rollback_on_failure(&self, tx: TransactionId) {
        if let Ok(mut inner) = self.lock() {
            if inner
                .txs
                .get(&tx)
                .is_some_and(|stored| stored.meta.state == TransactionState::Staged)
            {
                abort_staged(&mut inner, tx);
            }
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, TransactionError> {
        self.inner
            .lock()
            .map_err(|_| TransactionError::LockPoisoned)
    }
}

impl Default for TransactionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot apply, hook, and commit. Failure leaves the parent invisible.
pub fn commit_transaction(
    preview: &MergePreview,
    parent: &WorkspaceView,
    source: &DirectBackend,
    author: AgentId,
    hooks: &[&dyn VerificationHook],
    cancel: &CancellationToken,
) -> Result<CommitReceipt, TransactionError> {
    let manager = TransactionManager::new();
    let tx = manager.begin_transaction(preview, parent, source, author, cancel)?;
    manager.commit_transaction(tx, parent, hooks, cancel)
}

/// Discard a staged transaction. Committed receipts stay visible.
pub fn rollback(
    manager: &TransactionManager,
    tx: TransactionId,
    cancel: &CancellationToken,
) -> Result<(), TransactionError> {
    manager.rollback(tx, cancel)
}

impl TransactionError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::PreviewConflict => Some(ErrorCode::WorkspaceMergeConflict),
            Self::Preimage | Self::Apply(ApplyError::PreimageMismatch { .. }) => {
                Some(ErrorCode::WorkspacePreimageMismatch)
            }
            _ => None,
        }
    }
}

impl fmt::Display for TransactionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("workspace transaction cancelled"),
            Self::PreviewConflict => f.write_str("workspace merge preview has conflicts"),
            Self::StaleParentRevision => {
                f.write_str("workspace parent revision is stale; a new merge preview is required")
            }
            Self::Preimage => f.write_str("workspace preimage does not match"),
            Self::HookFailed => f.write_str("workspace verification hook failed"),
            Self::RequiredCheckFailed { .. } => {
                f.write_str("workspace transaction required check failed")
            }
            Self::TransactionNotFound { .. } => f.write_str("workspace transaction not found"),
            Self::AlreadyFinalized { .. } => {
                f.write_str("workspace transaction is already finalized")
            }
            Self::ParentBusy { .. } => {
                f.write_str("workspace parent already has a staged transaction")
            }
            Self::ViewMismatch => f.write_str("workspace transaction view does not match"),
            Self::ReadOnlyView => f.write_str("workspace view is read-only"),
            Self::InvalidState => f.write_str("workspace view is not in a valid state"),
            Self::GitScopeRequired => {
                f.write_str("mutating .git requires a dedicated git capability")
            }
            Self::BoundExceeded => f.write_str("workspace transaction resource bound exceeded"),
            Self::TransactionLimit { .. } => f.write_str("workspace transaction limit reached"),
            Self::Apply(err) => write!(f, "{err}"),
            Self::Patch(err) => write!(f, "{err}"),
            Self::LockPoisoned => f.write_str("workspace transaction lock poisoned"),
            Self::UnknownVariant => f.write_str("unknown workspace transaction enumeration value"),
            Self::UnsupportedSchema => f.write_str("unsupported workspace commit receipt schema"),
            Self::UnsupportedSchemaVersion => {
                f.write_str("unsupported workspace commit receipt schema version")
            }
        }
    }
}

impl Error for TransactionError {}

impl FromStr for TransactionState {
    type Err = TransactionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for item in Self::ALL {
            if item.as_str() == s {
                return Ok(*item);
            }
        }
        Err(TransactionError::UnknownVariant)
    }
}

impl Serialize for TransactionState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TransactionState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse()
            .map_err(|_| de::Error::unknown_variant(&raw, &[]))
    }
}

impl Serialize for CommitReceipt {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CommitReceipt", RECEIPT_FIELDS.len())?;
        state.serialize_field("schema", COMMIT_RECEIPT_SCHEMA)?;
        state.serialize_field("schema_version", &COMMIT_RECEIPT_SCHEMA_VERSION)?;
        state.serialize_field("transaction_id", &self.transaction_id)?;
        state.serialize_field("parent_view_id", &self.parent_view_id)?;
        state.serialize_field("child_view_id", &self.child_view_id)?;
        state.serialize_field("parent_revision", &self.parent_revision)?;
        state.serialize_field("preview_hash", &self.preview_hash)?;
        state.serialize_field("patch_hash", &self.patch_hash)?;
        state.serialize_field("change_count", &self.change_count)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CommitReceipt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawCommitReceipt::deserialize(deserializer)?;
        if raw.schema != COMMIT_RECEIPT_SCHEMA {
            return Err(de::Error::custom(TransactionError::UnsupportedSchema));
        }
        if raw.schema_version != COMMIT_RECEIPT_SCHEMA_VERSION {
            return Err(de::Error::custom(
                TransactionError::UnsupportedSchemaVersion,
            ));
        }
        Ok(CommitReceipt {
            transaction_id: raw.transaction_id,
            parent_view_id: raw.parent_view_id,
            child_view_id: raw.child_view_id,
            parent_revision: raw.parent_revision,
            preview_hash: raw.preview_hash,
            patch_hash: raw.patch_hash,
            change_count: raw.change_count,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommitReceipt {
    schema: String,
    schema_version: u16,
    transaction_id: TransactionId,
    parent_view_id: WorkspaceViewId,
    child_view_id: WorkspaceViewId,
    parent_revision: String,
    preview_hash: ArtifactId,
    patch_hash: ArtifactId,
    change_count: usize,
}

fn stage_preview(
    preview: &MergePreview,
    parent: &WorkspaceView,
    source: &DirectBackend,
    author: AgentId,
    cancel: &CancellationToken,
) -> Result<StagedPatch, TransactionError> {
    let patch = match SemanticPatch::new(
        preview.ops().to_vec(),
        author,
        preview.parent_revision(),
        cancel,
    ) {
        Ok(patch) => patch,
        Err(PatchError::Cancelled) => return Err(TransactionError::Cancelled),
        Err(err) => return Err(TransactionError::Patch(err)),
    };
    match apply_patch(parent, &patch, source, cancel) {
        Ok(staged) => Ok(staged),
        Err(err) => Err(map_apply(err)),
    }
}

fn verify_and_hook(
    stored: &StoredTx,
    parent: &WorkspaceView,
    hooks: &[&dyn VerificationHook],
    cancel: &CancellationToken,
) -> Result<(), TransactionError> {
    check_cancel(cancel)?;
    if parent.id() != stored.preview.parent_view_id() {
        return Err(TransactionError::ViewMismatch);
    }
    let staged = stored.staged.as_ref().ok_or(TransactionError::Preimage)?;
    verify_required_checks(&stored.preview, parent, Some(staged))?;
    let ctx = HookContext {
        transaction_id: stored.meta.id,
        preview: &stored.preview,
        staged,
    };
    for hook in hooks {
        check_cancel(cancel)?;
        hook.verify(&ctx, cancel)?;
    }
    check_cancel(cancel)?;
    verify_required_checks(&stored.preview, parent, Some(staged))
}

fn verify_required_checks(
    preview: &MergePreview,
    parent: &WorkspaceView,
    staged: Option<&StagedPatch>,
) -> Result<(), TransactionError> {
    if !has_required_checks(preview) {
        return Err(TransactionError::RequiredCheckFailed {
            check: VerificationCheck::ParentRevision,
        });
    }
    for check in preview.verification_plan().checks() {
        match check {
            VerificationCheck::ParentRevision => {
                if parent.id() != preview.parent_view_id()
                    || parent.base_revision() != preview.parent_revision()
                {
                    return Err(TransactionError::StaleParentRevision);
                }
            }
            VerificationCheck::ConflictFree => {
                if !preview.is_conflict_free() {
                    return Err(TransactionError::PreviewConflict);
                }
            }
            VerificationCheck::Preimage => {
                if let Some(staged) = staged {
                    if staged.view_id() != preview.parent_view_id()
                        || staged.base_revision() != preview.parent_revision()
                    {
                        return Err(TransactionError::Preimage);
                    }
                }
            }
        }
    }
    Ok(())
}

fn has_required_checks(preview: &MergePreview) -> bool {
    let checks = preview.verification_plan().checks();
    VerificationCheck::ALL
        .iter()
        .all(|required| checks.contains(required))
}

fn check_parent_binding(
    preview: &MergePreview,
    parent: &WorkspaceView,
    source: &DirectBackend,
) -> Result<(), TransactionError> {
    if parent.id() != preview.parent_view_id() || source.view().id() != parent.id() {
        return Err(TransactionError::ViewMismatch);
    }
    if parent.id() == preview.child_view_id() {
        return Err(TransactionError::ViewMismatch);
    }
    if !parent.access().is_writable() {
        return Err(TransactionError::ReadOnlyView);
    }
    if parent.state() == WorkspaceState::Closed {
        return Err(TransactionError::InvalidState);
    }
    if parent.base_revision() != preview.parent_revision()
        || source.view().base_revision() != preview.parent_revision()
    {
        return Err(TransactionError::StaleParentRevision);
    }
    Ok(())
}

fn load_staged(inner: &Inner, tx: TransactionId) -> Result<StoredTx, TransactionError> {
    let stored = inner
        .txs
        .get(&tx)
        .ok_or(TransactionError::TransactionNotFound { transaction_id: tx })?;
    match stored.meta.state {
        TransactionState::Staged => Ok(stored.clone()),
        state => Err(TransactionError::AlreadyFinalized {
            transaction_id: tx,
            state,
        }),
    }
}

fn abort_staged(inner: &mut Inner, tx: TransactionId) {
    if let Some(stored) = inner.txs.get_mut(&tx) {
        let parent = stored.meta.parent_view_id;
        stored.staged = None;
        stored.meta.state = TransactionState::RolledBack;
        if inner.staged_by_parent.get(&parent) == Some(&tx) {
            inner.staged_by_parent.remove(&parent);
        }
    }
}

fn map_apply(err: ApplyError) -> TransactionError {
    match err {
        ApplyError::Cancelled => TransactionError::Cancelled,
        ApplyError::ReadOnlyView => TransactionError::ReadOnlyView,
        ApplyError::InvalidState => TransactionError::InvalidState,
        ApplyError::BaseRevisionMismatch => TransactionError::StaleParentRevision,
        ApplyError::ViewMismatch => TransactionError::ViewMismatch,
        ApplyError::GitScopeRequired => TransactionError::GitScopeRequired,
        ApplyError::BoundExceeded => TransactionError::BoundExceeded,
        ApplyError::PreimageMismatch { .. } => TransactionError::Preimage,
        other => TransactionError::Apply(other),
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), TransactionError> {
    if cancel.is_cancelled() {
        Err(TransactionError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::direct::DirectOptions;
    use crate::merge::preview_merge;
    use crate::patch::model::PatchOp;
    use crate::view::{CreateView, ViewAccess, ViewRegistry, WorkspaceBackend};
    use protocol::{RepoId, RepoPath};
    use std::fs;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, Ordering};

    const AUTHOR: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";
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

    struct RejectHook;

    impl VerificationHook for RejectHook {
        fn name(&self) -> &'static str {
            "reject"
        }

        fn verify(
            &self,
            _ctx: &HookContext<'_>,
            _cancel: &CancellationToken,
        ) -> Result<(), TransactionError> {
            Err(TransactionError::HookFailed)
        }
    }

    struct ApproveConflictHook;

    impl VerificationHook for ApproveConflictHook {
        fn name(&self) -> &'static str {
            "approve_conflict"
        }

        fn verify(
            &self,
            _ctx: &HookContext<'_>,
            _cancel: &CancellationToken,
        ) -> Result<(), TransactionError> {
            Ok(())
        }
    }

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn author() -> AgentId {
        AgentId::from_str(AUTHOR).expect("author")
    }

    fn repo_path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn hello_preimage() -> ArtifactId {
        ArtifactId::from_bytes(b"hello")
    }

    fn replace(path: &str, start: u64, end: u64, content: &str) -> PatchOp {
        PatchOp::replace_range(
            repo_path(path),
            hello_preimage(),
            start,
            end,
            content.to_owned(),
        )
        .expect("replace")
    }

    fn patch(ops: Vec<PatchOp>) -> SemanticPatch {
        SemanticPatch::new(ops, author(), "deadbeef", &cancel()).expect("patch")
    }

    fn empty_patch() -> SemanticPatch {
        patch(Vec::new())
    }

    fn live_pair() -> (ViewRegistry, WorkspaceView, WorkspaceView) {
        let registry = ViewRegistry::new();
        let repo = RepoId::new();
        let child = registry
            .create(
                CreateView::new(
                    repo,
                    WorkspaceBackend::GitWorktree,
                    "deadbeef",
                    ViewAccess::ReadWrite,
                ),
                &cancel(),
            )
            .expect("child");
        let parent = registry
            .create(
                CreateView::new(
                    repo,
                    WorkspaceBackend::Direct,
                    "deadbeef",
                    ViewAccess::ReadWrite,
                )
                .with_write_owner(AgentId::new()),
                &cancel(),
            )
            .expect("parent");
        let child = registry.quiesce(child.id(), &cancel()).expect("quiesce");
        (registry, child, parent)
    }

    fn fixture_parent() -> (ViewRegistry, WorkspaceView, WorkspaceView, Fixture) {
        let (registry, child, parent) = live_pair();
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-tx-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("src")).expect("mkdir");
        fs::write(dir.join("src/lib.rs"), b"hello").expect("seed");
        fs::write(dir.join("src/keep.rs"), b"keep").expect("seed keep");
        let backend = DirectBackend::open_with(
            &dir,
            parent.clone(),
            DirectOptions::interactive(),
            &cancel(),
        )
        .expect("open");
        (
            registry,
            child,
            parent,
            Fixture {
                dir,
                backend: Some(backend),
            },
        )
    }

    fn conflict_free_preview(
        child: &WorkspaceView,
        parent: &WorkspaceView,
        ops: Vec<PatchOp>,
    ) -> MergePreview {
        let preview =
            preview_merge(child, parent, &patch(ops), &empty_patch(), &cancel()).expect("preview");
        assert!(preview.is_conflict_free());
        preview
    }

    fn with_revision(view: &WorkspaceView, revision: &str) -> WorkspaceView {
        let mut value = serde_json::to_value(view).expect("value");
        value["base_revision"] = serde_json::Value::String(revision.to_owned());
        serde_json::from_value(value).expect("stale view")
    }

    #[test]
    fn commit_publishes_overlay_without_touching_parent_checkout() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview =
            conflict_free_preview(&child, &parent, vec![replace("src/lib.rs", 0, 5, "world")]);
        let receipt = commit_transaction(
            &preview,
            &parent,
            source,
            author(),
            &[&AcceptHook],
            &cancel(),
        )
        .expect("commit");
        assert_eq!(receipt.parent_view_id(), parent.id());
        assert_eq!(receipt.child_view_id(), child.id());
        assert_eq!(receipt.parent_revision(), "deadbeef");
        assert_eq!(receipt.preview_hash(), preview.preview_hash());
        assert_eq!(receipt.change_count(), 1);
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        assert!(source.journal().expect("journal").is_empty());
    }

    #[test]
    fn manager_commit_makes_parent_visible_only_after_checks() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview =
            conflict_free_preview(&child, &parent, vec![replace("src/lib.rs", 0, 5, "world")]);
        let manager = TransactionManager::new();
        assert!(manager.parent_visibility(parent.id()).is_none());
        let tx = manager
            .begin_transaction(&preview, &parent, source, author(), &cancel())
            .expect("begin");
        assert_eq!(
            manager.get(tx, &cancel()).expect("get").state(),
            TransactionState::Staged
        );
        assert!(manager.parent_visibility(parent.id()).is_none());
        let receipt = manager
            .commit_transaction(tx, &parent, &[], &cancel())
            .expect("commit");
        assert_eq!(
            manager.get(tx, &cancel()).expect("get").state(),
            TransactionState::Committed
        );
        let visible = manager.parent_visibility(parent.id()).expect("visible");
        assert_eq!(visible, receipt);
        let overlay = manager.parent_overlay(parent.id()).expect("overlay");
        assert_eq!(
            overlay.get(&repo_path("src/lib.rs")).expect("file").bytes(),
            b"world"
        );
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        let stored = manager.get(tx, &cancel()).expect("tx");
        assert_eq!(stored.author(), author());
        assert_eq!(stored.patch_hash(), receipt.patch_hash());
        assert_eq!(stored.change_count(), 1);
        assert_eq!(stored.parent_revision(), parent.base_revision());
    }

    #[test]
    fn conflicting_preview_cannot_stage_or_become_visible() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview = preview_merge(
            &child,
            &parent,
            &patch(vec![replace("src/lib.rs", 0, 5, "child")]),
            &patch(vec![replace("src/lib.rs", 0, 5, "sib")]),
            &cancel(),
        )
        .expect("preview");
        assert!(!preview.is_conflict_free());
        let manager = TransactionManager::new();
        let err = manager
            .begin_transaction(&preview, &parent, source, author(), &cancel())
            .expect_err("conflict");
        assert_eq!(err, TransactionError::PreviewConflict);
        assert_eq!(err.code(), Some(ErrorCode::WorkspaceMergeConflict));
        assert!(manager.parent_visibility(parent.id()).is_none());
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        let shown = err.to_string();
        assert_eq!(shown, "workspace merge preview has conflicts");
        assert!(!shown.contains("child"));
        assert!(!shown.contains("sib"));
    }

    #[test]
    fn approving_hook_cannot_bypass_required_conflict_check() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview = preview_merge(
            &child,
            &parent,
            &patch(vec![replace("src/lib.rs", 0, 5, "SECRET")]),
            &patch(vec![replace("src/lib.rs", 0, 5, "LEAK")]),
            &cancel(),
        )
        .expect("preview");
        let err = commit_transaction(
            &preview,
            &parent,
            source,
            author(),
            &[&ApproveConflictHook],
            &cancel(),
        )
        .expect_err("hook cannot skip");
        assert_eq!(err, TransactionError::PreviewConflict);
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        assert!(!err.to_string().contains("SECRET"));
        assert!(!err.to_string().contains("LEAK"));
    }

    #[test]
    fn stale_parent_revision_requires_new_preview() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview =
            conflict_free_preview(&child, &parent, vec![replace("src/lib.rs", 0, 5, "world")]);
        let stale = with_revision(&parent, "cafebabe");
        let manager = TransactionManager::new();
        let err = manager
            .begin_transaction(&preview, &stale, source, author(), &cancel())
            .expect_err("begin stale");
        assert_eq!(err, TransactionError::StaleParentRevision);
        assert!(manager.parent_visibility(parent.id()).is_none());

        let tx = manager
            .begin_transaction(&preview, &parent, source, author(), &cancel())
            .expect("begin");
        let err = manager
            .commit_transaction(tx, &stale, &[&AcceptHook], &cancel())
            .expect_err("commit stale");
        assert_eq!(err, TransactionError::StaleParentRevision);
        assert_eq!(
            manager.get(tx, &cancel()).expect("get").state(),
            TransactionState::RolledBack
        );
        assert!(manager.parent_visibility(parent.id()).is_none());
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        assert_eq!(
            err.to_string(),
            "workspace parent revision is stale; a new merge preview is required"
        );
        assert!(!err.to_string().contains("deadbeef"));
        assert!(!err.to_string().contains("cafebabe"));
    }

    #[test]
    fn hook_failure_rolls_back_staged_parent() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview =
            conflict_free_preview(&child, &parent, vec![replace("src/lib.rs", 0, 5, "world")]);
        let manager = TransactionManager::new();
        let tx = manager
            .begin_transaction(&preview, &parent, source, author(), &cancel())
            .expect("begin");
        let err = manager
            .commit_transaction(tx, &parent, &[&RejectHook], &cancel())
            .expect_err("hook");
        assert_eq!(err, TransactionError::HookFailed);
        assert_eq!(
            manager.get(tx, &cancel()).expect("get").state(),
            TransactionState::RolledBack
        );
        assert!(manager.parent_visibility(parent.id()).is_none());
        assert!(manager.parent_overlay(parent.id()).is_none());
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        assert!(source.journal().expect("journal").is_empty());
        assert!(!err.to_string().contains("world"));
        assert!(!err.to_string().contains("src/lib.rs"));
    }

    #[test]
    fn preimage_failure_does_not_stage_parent() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview = preview_merge(
            &child,
            &parent,
            &patch(vec![PatchOp::delete_file(
                repo_path("src/lib.rs"),
                ArtifactId::from_bytes(b"nope"),
            )]),
            &empty_patch(),
            &cancel(),
        )
        .expect("preview");
        let manager = TransactionManager::new();
        let err = manager
            .begin_transaction(&preview, &parent, source, author(), &cancel())
            .expect_err("preimage");
        assert_eq!(err, TransactionError::Preimage);
        assert_eq!(err.code(), Some(ErrorCode::WorkspacePreimageMismatch));
        assert!(manager.parent_visibility(parent.id()).is_none());
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        assert!(!err.to_string().contains("nope"));
        assert!(!err.to_string().contains("src/lib.rs"));
    }

    #[test]
    fn explicit_rollback_discards_staged_parent() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview =
            conflict_free_preview(&child, &parent, vec![replace("src/lib.rs", 0, 5, "world")]);
        let manager = TransactionManager::new();
        let tx = manager
            .begin_transaction(&preview, &parent, source, author(), &cancel())
            .expect("begin");
        rollback(&manager, tx, &cancel()).expect("rollback");
        assert_eq!(
            manager.get(tx, &cancel()).expect("get").state(),
            TransactionState::RolledBack
        );
        assert!(manager.parent_visibility(parent.id()).is_none());
        let err = manager
            .commit_transaction(tx, &parent, &[&AcceptHook], &cancel())
            .expect_err("finalized");
        assert!(matches!(
            err,
            TransactionError::AlreadyFinalized {
                state: TransactionState::RolledBack,
                ..
            }
        ));
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
    }

    #[test]
    fn committed_transaction_cannot_roll_back_visibility() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview =
            conflict_free_preview(&child, &parent, vec![replace("src/lib.rs", 0, 5, "world")]);
        let manager = TransactionManager::new();
        let tx = manager
            .begin_transaction(&preview, &parent, source, author(), &cancel())
            .expect("begin");
        manager
            .commit_transaction(tx, &parent, &[&AcceptHook], &cancel())
            .expect("commit");
        let err = manager.rollback(tx, &cancel()).expect_err("committed");
        assert!(matches!(
            err,
            TransactionError::AlreadyFinalized {
                state: TransactionState::Committed,
                ..
            }
        ));
        assert!(manager.parent_visibility(parent.id()).is_some());
    }

    #[test]
    fn git_path_cannot_be_committed() {
        let (_registry, child, parent, fx) = fixture_parent();
        fs::create_dir_all(fx.dir.join(".git")).expect("git");
        fs::write(fx.dir.join(".git/config"), b"[core]\n").expect("config");
        let source = fx.backend.as_ref().expect("backend");
        let err = preview_merge(
            &child,
            &parent,
            &patch(vec![
                PatchOp::create_file(repo_path(".git/hooks/pre-commit"), b"evil".to_vec(), true)
                    .expect("create"),
            ]),
            &empty_patch(),
            &cancel(),
        )
        .expect_err("preview git");
        assert!(!err.to_string().contains("pre-commit"));
        assert!(!err.to_string().contains("evil"));
        assert_eq!(
            fs::read(fx.dir.join(".git/config")).expect("config"),
            b"[core]\n"
        );
        let _ = source;
    }

    #[test]
    fn cancelled_commit_rolls_back_staged_parent() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview =
            conflict_free_preview(&child, &parent, vec![replace("src/lib.rs", 0, 5, "world")]);
        let manager = TransactionManager::new();
        let tx = manager
            .begin_transaction(&preview, &parent, source, author(), &cancel())
            .expect("begin");
        let token = CancellationToken::new();
        token.cancel();
        let err = manager
            .commit_transaction(tx, &parent, &[&AcceptHook], &token)
            .expect_err("cancelled");
        assert_eq!(err, TransactionError::Cancelled);
        assert_eq!(
            manager.get(tx, &cancel()).expect("get").state(),
            TransactionState::RolledBack
        );
        assert!(manager.parent_visibility(parent.id()).is_none());
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
    }

    #[test]
    fn second_staged_transaction_on_same_parent_is_rejected() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview =
            conflict_free_preview(&child, &parent, vec![replace("src/lib.rs", 0, 5, "world")]);
        let manager = TransactionManager::new();
        manager
            .begin_transaction(&preview, &parent, source, author(), &cancel())
            .expect("first");
        let err = manager
            .begin_transaction(&preview, &parent, source, author(), &cancel())
            .expect_err("busy");
        assert!(matches!(err, TransactionError::ParentBusy { .. }));
        assert!(manager.parent_visibility(parent.id()).is_none());
    }

    #[test]
    fn commit_receipt_round_trips_and_rejects_unknown_fields() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview =
            conflict_free_preview(&child, &parent, vec![replace("src/lib.rs", 0, 5, "world")]);
        let receipt = commit_transaction(&preview, &parent, source, author(), &[], &cancel())
            .expect("commit");
        let json = serde_json::to_string(&receipt).expect("json");
        let decoded = serde_json::from_str::<CommitReceipt>(&json).expect("decode");
        assert_eq!(decoded, receipt);
        assert!(json.contains("\"schema\":\"rapidlm.commit_receipt\""));
        assert!(json.contains("\"schema_version\":1"));
        let extra = json.replacen('{', r#"{"extra":true,"#, 1);
        assert!(serde_json::from_str::<CommitReceipt>(&extra).is_err());
    }

    #[test]
    fn hook_bound_exceeded_rolls_back_without_visibility() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let preview =
            conflict_free_preview(&child, &parent, vec![replace("src/lib.rs", 0, 5, "world")]);
        let manager = TransactionManager::new();
        let tx = manager
            .begin_transaction(&preview, &parent, source, author(), &cancel())
            .expect("begin");
        let hook = AcceptHook;
        let hooks: Vec<&dyn VerificationHook> = (0..MAX_VERIFICATION_HOOKS + 1)
            .map(|_| &hook as &dyn VerificationHook)
            .collect();
        let err = manager
            .commit_transaction(tx, &parent, &hooks, &cancel())
            .expect_err("bound");
        assert_eq!(err, TransactionError::BoundExceeded);
        assert_eq!(
            manager.get(tx, &cancel()).expect("get").state(),
            TransactionState::RolledBack
        );
        assert!(manager.parent_visibility(parent.id()).is_none());
    }
}
