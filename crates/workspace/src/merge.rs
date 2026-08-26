//! Child-view merge preview into a parent staging view.
//!
//! Preview is observational: it never mutates the parent checkout, overlay,
//! or view record. Overlapping sibling path/op edits are reported as
//! machine-readable conflicts. No silent textual rebase is performed.
//!
//! `preview_hash` binds a canonical encoding of ops, conflicts, and
//! verification checks. Decode re-applies git-scope and patch bounds and
//! rejects a payload whose hash does not match that encoding.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

use protocol::{ArtifactId, ErrorCode, RepoPath, WorkspaceViewId};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::patch::model::{
    MAX_OP_CONTENT_BYTES, MAX_PATCH_CONTENT_BYTES, MAX_PATCH_OPS, PatchError, PatchOp,
    SemanticPatch,
};
use crate::view::{CancellationToken, MAX_BASE_REVISION_BYTES, WorkspaceState, WorkspaceView};

/// Wire schema name for [`MergePreview`].
pub const MERGE_PREVIEW_SCHEMA: &str = "rapidlm.merge_preview";

/// v1 schema version for merge previews.
pub const MERGE_PREVIEW_SCHEMA_VERSION: u16 = 1;

/// Maximum child ops accepted into one preview.
pub const MAX_MERGE_OPS: usize = MAX_PATCH_OPS;

/// Maximum machine-readable conflicts retained in one preview.
pub const MAX_MERGE_CONFLICTS: usize = MAX_PATCH_OPS;

const CANCEL_STRIDE: usize = 8;
const PREVIEW_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "child_view_id",
    "parent_view_id",
    "parent_revision",
    "ops",
    "conflicts",
    "verification_plan",
    "preview_hash",
];
const CONFLICT_FIELDS: &[&str] = &["path", "kind", "child_op", "parent_op"];
const PLAN_FIELDS: &[&str] = &[
    "parent_view_id",
    "child_view_id",
    "parent_revision",
    "child_patch_hash",
    "parent_patch_hash",
    "checks",
];

/// Request to preview merging a quiescent child into a parent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeView {
    child: WorkspaceView,
    parent: WorkspaceView,
    child_patch: SemanticPatch,
    parent_patch: SemanticPatch,
}

/// Observational merge result. Conflicts never auto-resolve.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergePreview {
    child_view_id: WorkspaceViewId,
    parent_view_id: WorkspaceViewId,
    parent_revision: String,
    ops: Vec<PatchOp>,
    conflicts: Vec<MergeConflict>,
    verification_plan: VerificationPlan,
    preview_hash: ArtifactId,
}

/// One path/op collision between child and parent/sibling edits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeConflict {
    path: RepoPath,
    kind: MergeConflictKind,
    child_op: MergeOpKind,
    parent_op: MergeOpKind,
}

/// Machine-readable conflict class.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum MergeConflictKind {
    Path,
    Op,
}

/// Closed operation kind used on the conflict wire form.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum MergeOpKind {
    ReplaceRange,
    CreateFile,
    DeleteFile,
    MoveFile,
}

/// Checks the later transactional commit must re-verify.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationPlan {
    parent_view_id: WorkspaceViewId,
    child_view_id: WorkspaceViewId,
    parent_revision: String,
    child_patch_hash: ArtifactId,
    parent_patch_hash: ArtifactId,
    checks: Vec<VerificationCheck>,
}

/// Required commit-time check. Preview never treats these as satisfied.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum VerificationCheck {
    ParentRevision,
    ConflictFree,
    Preimage,
}

/// Typed merge-preview failure. Display never echoes paths or payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeError {
    Cancelled,
    SameView,
    RepoMismatch,
    BaseRevisionMismatch,
    InvalidState,
    ReadOnlyView,
    GitScopeRequired,
    BoundExceeded,
    Patch(PatchError),
    UnknownVariant,
    UnsupportedSchema,
    UnsupportedSchemaVersion,
}

impl MergeView {
    pub fn new(
        child: WorkspaceView,
        parent: WorkspaceView,
        child_patch: SemanticPatch,
        parent_patch: SemanticPatch,
    ) -> Self {
        Self {
            child,
            parent,
            child_patch,
            parent_patch,
        }
    }

    pub fn child(&self) -> &WorkspaceView {
        &self.child
    }

    pub fn parent(&self) -> &WorkspaceView {
        &self.parent
    }

    pub fn child_patch(&self) -> &SemanticPatch {
        &self.child_patch
    }

    pub fn parent_patch(&self) -> &SemanticPatch {
        &self.parent_patch
    }

    pub fn preview(&self, cancel: &CancellationToken) -> Result<MergePreview, MergeError> {
        preview_merge(
            &self.child,
            &self.parent,
            &self.child_patch,
            &self.parent_patch,
            cancel,
        )
    }
}

impl MergePreview {
    pub fn child_view_id(&self) -> WorkspaceViewId {
        self.child_view_id
    }

    pub fn parent_view_id(&self) -> WorkspaceViewId {
        self.parent_view_id
    }

    pub fn parent_revision(&self) -> &str {
        &self.parent_revision
    }

    pub fn ops(&self) -> &[PatchOp] {
        &self.ops
    }

    pub fn conflicts(&self) -> &[MergeConflict] {
        &self.conflicts
    }

    pub fn verification_plan(&self) -> &VerificationPlan {
        &self.verification_plan
    }

    pub fn preview_hash(&self) -> ArtifactId {
        self.preview_hash
    }

    pub fn is_conflict_free(&self) -> bool {
        self.conflicts.is_empty()
    }

    /// Public machine code when the preview is not committable as-is.
    pub fn code(&self) -> Option<ErrorCode> {
        if self.conflicts.is_empty() {
            None
        } else {
            Some(ErrorCode::WorkspaceMergeConflict)
        }
    }
}

impl MergeConflict {
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn kind(&self) -> MergeConflictKind {
        self.kind
    }

    pub fn child_op(&self) -> MergeOpKind {
        self.child_op
    }

    pub fn parent_op(&self) -> MergeOpKind {
        self.parent_op
    }
}

impl VerificationPlan {
    pub fn parent_view_id(&self) -> WorkspaceViewId {
        self.parent_view_id
    }

    pub fn child_view_id(&self) -> WorkspaceViewId {
        self.child_view_id
    }

    pub fn parent_revision(&self) -> &str {
        &self.parent_revision
    }

    pub fn child_patch_hash(&self) -> ArtifactId {
        self.child_patch_hash
    }

    pub fn parent_patch_hash(&self) -> ArtifactId {
        self.parent_patch_hash
    }

    pub fn checks(&self) -> &[VerificationCheck] {
        &self.checks
    }
}

impl MergeConflictKind {
    pub const ALL: &'static [Self] = &[Self::Path, Self::Op];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Op => "op",
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::Path => 1,
            Self::Op => 2,
        }
    }
}

impl MergeOpKind {
    pub const ALL: &'static [Self] = &[
        Self::ReplaceRange,
        Self::CreateFile,
        Self::DeleteFile,
        Self::MoveFile,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReplaceRange => "replace_range",
            Self::CreateFile => "create_file",
            Self::DeleteFile => "delete_file",
            Self::MoveFile => "move_file",
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::ReplaceRange => 1,
            Self::CreateFile => 2,
            Self::DeleteFile => 3,
            Self::MoveFile => 4,
        }
    }

    fn from_op(op: &PatchOp) -> Self {
        match op {
            PatchOp::ReplaceRange { .. } => Self::ReplaceRange,
            PatchOp::CreateFile { .. } => Self::CreateFile,
            PatchOp::DeleteFile { .. } => Self::DeleteFile,
            PatchOp::MoveFile { .. } => Self::MoveFile,
        }
    }
}

impl VerificationCheck {
    pub const ALL: &'static [Self] = &[Self::ParentRevision, Self::ConflictFree, Self::Preimage];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ParentRevision => "parent_revision",
            Self::ConflictFree => "conflict_free",
            Self::Preimage => "preimage",
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::ParentRevision => 1,
            Self::ConflictFree => 2,
            Self::Preimage => 3,
        }
    }
}

fn required_checks() -> [VerificationCheck; 3] {
    [
        VerificationCheck::ParentRevision,
        VerificationCheck::ConflictFree,
        VerificationCheck::Preimage,
    ]
}

/// Compute a semantic/textual merge preview. Parent is never written.
pub fn preview_merge(
    child: &WorkspaceView,
    parent: &WorkspaceView,
    child_patch: &SemanticPatch,
    parent_patch: &SemanticPatch,
    cancel: &CancellationToken,
) -> Result<MergePreview, MergeError> {
    check_cancel(cancel)?;
    check_views(child, parent)?;
    validate_patch(child, child_patch, cancel)?;
    validate_patch(parent, parent_patch, cancel)?;
    reject_git_ops(child_patch.ops(), cancel)?;
    reject_git_ops(parent_patch.ops(), cancel)?;

    let mut ops = Vec::new();
    let mut conflicts = Vec::new();
    let mut seen = BTreeSet::new();

    for (index, child_op) in child_patch.ops().iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        let overlapping: Vec<&PatchOp> = parent_patch
            .ops()
            .iter()
            .filter(|parent_op| paths_overlap(child_op, parent_op))
            .collect();
        if overlapping.is_empty() {
            if ops.len() >= MAX_MERGE_OPS {
                return Err(MergeError::BoundExceeded);
            }
            ops.push(child_op.clone());
            continue;
        }
        if overlapping.iter().all(|parent_op| *parent_op == child_op) {
            continue;
        }
        record_conflicts(child_op, &overlapping, &mut conflicts, &mut seen)?;
    }

    check_cancel(cancel)?;
    let child_patch_hash = patch_hash(child_patch, cancel)?;
    let parent_patch_hash = patch_hash(parent_patch, cancel)?;
    let verification_plan = VerificationPlan {
        parent_view_id: parent.id(),
        child_view_id: child.id(),
        parent_revision: parent.base_revision().to_owned(),
        child_patch_hash,
        parent_patch_hash,
        checks: required_checks().to_vec(),
    };
    accept_preview(
        child.id(),
        parent.id(),
        parent.base_revision().to_owned(),
        ops,
        conflicts,
        verification_plan,
        None,
        cancel,
    )
}

impl fmt::Display for MergeConflictKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for MergeOpKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for VerificationCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("workspace merge preview cancelled"),
            Self::SameView => f.write_str("cannot merge a workspace view into itself"),
            Self::RepoMismatch => f.write_str("child and parent views belong to different repos"),
            Self::BaseRevisionMismatch => f.write_str("merge preview base revision does not match"),
            Self::InvalidState => f.write_str("workspace view is not in a valid merge state"),
            Self::ReadOnlyView => f.write_str("workspace view is read-only"),
            Self::GitScopeRequired => {
                f.write_str("mutating .git requires a dedicated git capability")
            }
            Self::BoundExceeded => f.write_str("workspace merge preview resource bound exceeded"),
            Self::Patch(err) => write!(f, "{err}"),
            Self::UnknownVariant => f.write_str("unknown workspace merge enumeration value"),
            Self::UnsupportedSchema => f.write_str("unsupported workspace merge preview schema"),
            Self::UnsupportedSchemaVersion => {
                f.write_str("unsupported workspace merge preview schema version")
            }
        }
    }
}

impl Error for MergeError {}

impl FromStr for MergeConflictKind {
    type Err = MergeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl FromStr for MergeOpKind {
    type Err = MergeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl FromStr for VerificationCheck {
    type Err = MergeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl Serialize for MergeConflictKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Serialize for MergeOpKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Serialize for VerificationCheck {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MergeConflictKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl<'de> Deserialize<'de> for MergeOpKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl<'de> Deserialize<'de> for VerificationCheck {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl Serialize for MergeConflict {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("MergeConflict", CONFLICT_FIELDS.len())?;
        state.serialize_field("path", &self.path)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("child_op", &self.child_op)?;
        state.serialize_field("parent_op", &self.parent_op)?;
        state.end()
    }
}

impl Serialize for VerificationPlan {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("VerificationPlan", PLAN_FIELDS.len())?;
        state.serialize_field("parent_view_id", &self.parent_view_id)?;
        state.serialize_field("child_view_id", &self.child_view_id)?;
        state.serialize_field("parent_revision", &self.parent_revision)?;
        state.serialize_field("child_patch_hash", &self.child_patch_hash)?;
        state.serialize_field("parent_patch_hash", &self.parent_patch_hash)?;
        state.serialize_field("checks", &self.checks)?;
        state.end()
    }
}

impl Serialize for MergePreview {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("MergePreview", PREVIEW_FIELDS.len())?;
        state.serialize_field("schema", MERGE_PREVIEW_SCHEMA)?;
        state.serialize_field("schema_version", &MERGE_PREVIEW_SCHEMA_VERSION)?;
        state.serialize_field("child_view_id", &self.child_view_id)?;
        state.serialize_field("parent_view_id", &self.parent_view_id)?;
        state.serialize_field("parent_revision", &self.parent_revision)?;
        state.serialize_field("ops", &self.ops)?;
        state.serialize_field("conflicts", &self.conflicts)?;
        state.serialize_field("verification_plan", &self.verification_plan)?;
        state.serialize_field("preview_hash", &self.preview_hash)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for MergeConflict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawMergeConflict::deserialize(deserializer)?;
        Ok(MergeConflict {
            path: raw.path,
            kind: raw.kind,
            child_op: raw.child_op,
            parent_op: raw.parent_op,
        })
    }
}

impl<'de> Deserialize<'de> for VerificationPlan {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawVerificationPlan::deserialize(deserializer)?;
        if raw.checks != required_checks() {
            return Err(de::Error::custom(MergeError::UnknownVariant));
        }
        Ok(VerificationPlan {
            parent_view_id: raw.parent_view_id,
            child_view_id: raw.child_view_id,
            parent_revision: raw.parent_revision,
            child_patch_hash: raw.child_patch_hash,
            parent_patch_hash: raw.parent_patch_hash,
            checks: raw.checks,
        })
    }
}

impl<'de> Deserialize<'de> for MergePreview {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawMergePreview::deserialize(deserializer)?;
        decode_preview(raw).map_err(de::Error::custom)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMergeConflict {
    path: RepoPath,
    kind: MergeConflictKind,
    child_op: MergeOpKind,
    parent_op: MergeOpKind,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVerificationPlan {
    parent_view_id: WorkspaceViewId,
    child_view_id: WorkspaceViewId,
    parent_revision: String,
    child_patch_hash: ArtifactId,
    parent_patch_hash: ArtifactId,
    checks: Vec<VerificationCheck>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMergePreview {
    schema: String,
    schema_version: u16,
    child_view_id: WorkspaceViewId,
    parent_view_id: WorkspaceViewId,
    parent_revision: String,
    ops: Vec<PatchOp>,
    conflicts: Vec<MergeConflict>,
    verification_plan: VerificationPlan,
    preview_hash: ArtifactId,
}

fn decode_preview(raw: RawMergePreview) -> Result<MergePreview, MergeError> {
    if raw.schema != MERGE_PREVIEW_SCHEMA {
        return Err(MergeError::UnsupportedSchema);
    }
    if raw.schema_version != MERGE_PREVIEW_SCHEMA_VERSION {
        return Err(MergeError::UnsupportedSchemaVersion);
    }
    if raw.verification_plan.child_view_id != raw.child_view_id
        || raw.verification_plan.parent_view_id != raw.parent_view_id
        || raw.verification_plan.parent_revision != raw.parent_revision
    {
        return Err(MergeError::UnknownVariant);
    }
    accept_preview(
        raw.child_view_id,
        raw.parent_view_id,
        raw.parent_revision,
        raw.ops,
        raw.conflicts,
        raw.verification_plan,
        Some(raw.preview_hash),
        &CancellationToken::new(),
    )
}

fn accept_preview(
    child_view_id: WorkspaceViewId,
    parent_view_id: WorkspaceViewId,
    parent_revision: String,
    ops: Vec<PatchOp>,
    conflicts: Vec<MergeConflict>,
    verification_plan: VerificationPlan,
    claimed_hash: Option<ArtifactId>,
    cancel: &CancellationToken,
) -> Result<MergePreview, MergeError> {
    validate_decoded_payload(&parent_revision, &ops, &conflicts, cancel)?;
    check_cancel(cancel)?;
    let preview_hash = hash_preview(
        child_view_id,
        parent_view_id,
        &parent_revision,
        verification_plan.child_patch_hash,
        verification_plan.parent_patch_hash,
        &ops,
        &conflicts,
        &verification_plan.checks,
        cancel,
    )?;
    if let Some(claimed) = claimed_hash {
        if claimed != preview_hash {
            return Err(MergeError::UnknownVariant);
        }
    }
    Ok(MergePreview {
        child_view_id,
        parent_view_id,
        parent_revision,
        ops,
        conflicts,
        verification_plan,
        preview_hash,
    })
}

fn validate_decoded_payload(
    parent_revision: &str,
    ops: &[PatchOp],
    conflicts: &[MergeConflict],
    cancel: &CancellationToken,
) -> Result<(), MergeError> {
    check_cancel(cancel)?;
    if parent_revision.is_empty() || parent_revision.len() > MAX_BASE_REVISION_BYTES {
        return Err(MergeError::BoundExceeded);
    }
    if ops.len() > MAX_MERGE_OPS || conflicts.len() > MAX_MERGE_CONFLICTS {
        return Err(MergeError::BoundExceeded);
    }
    reject_git_ops(ops, cancel)?;
    for (index, conflict) in conflicts.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        reject_git_mutation(&conflict.path)?;
    }
    recheck_ops_bounds(ops, cancel)
}

fn check_views(child: &WorkspaceView, parent: &WorkspaceView) -> Result<(), MergeError> {
    if child.id() == parent.id() {
        return Err(MergeError::SameView);
    }
    if child.repo_id() != parent.repo_id() {
        return Err(MergeError::RepoMismatch);
    }
    if child.base_revision() != parent.base_revision() {
        return Err(MergeError::BaseRevisionMismatch);
    }
    if child.state() != WorkspaceState::Quiescent {
        return Err(MergeError::InvalidState);
    }
    if parent.state() == WorkspaceState::Closed {
        return Err(MergeError::InvalidState);
    }
    if !child.access().is_writable() || !parent.access().is_writable() {
        return Err(MergeError::ReadOnlyView);
    }
    Ok(())
}

fn validate_patch(
    view: &WorkspaceView,
    patch: &SemanticPatch,
    cancel: &CancellationToken,
) -> Result<(), MergeError> {
    match patch.validate(cancel) {
        Ok(()) => {}
        Err(PatchError::Cancelled) => return Err(MergeError::Cancelled),
        Err(err) => return Err(MergeError::Patch(err)),
    }
    if patch.base_revision() != view.base_revision() {
        return Err(MergeError::BaseRevisionMismatch);
    }
    if patch.ops().len() > MAX_MERGE_OPS {
        return Err(MergeError::BoundExceeded);
    }
    Ok(())
}

fn reject_git_ops(ops: &[PatchOp], cancel: &CancellationToken) -> Result<(), MergeError> {
    for (index, op) in ops.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        for path in occupied_paths(op) {
            reject_git_mutation(path)?;
        }
    }
    Ok(())
}

fn recheck_ops_bounds(ops: &[PatchOp], cancel: &CancellationToken) -> Result<(), MergeError> {
    let mut total = 0usize;
    for (index, op) in ops.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        let content_len = match op {
            PatchOp::ReplaceRange {
                path,
                start,
                end,
                content,
                ..
            } => {
                if start.as_u64() > end.as_u64() {
                    return Err(MergeError::Patch(PatchError::InvalidRange {
                        path: path.clone(),
                    }));
                }
                content.len()
            }
            PatchOp::CreateFile { content, .. } => content.len(),
            PatchOp::MoveFile { from, to, .. } => {
                if from == to {
                    return Err(MergeError::Patch(PatchError::InvalidMove));
                }
                0
            }
            PatchOp::DeleteFile { .. } => 0,
        };
        if content_len > MAX_OP_CONTENT_BYTES {
            return Err(MergeError::BoundExceeded);
        }
        total = total
            .checked_add(content_len)
            .ok_or(MergeError::BoundExceeded)?;
        if total > MAX_PATCH_CONTENT_BYTES {
            return Err(MergeError::BoundExceeded);
        }
    }
    Ok(())
}

fn record_conflicts(
    child_op: &PatchOp,
    overlapping: &[&PatchOp],
    conflicts: &mut Vec<MergeConflict>,
    seen: &mut BTreeSet<(String, MergeConflictKind, MergeOpKind, MergeOpKind)>,
) -> Result<(), MergeError> {
    for parent_op in overlapping {
        if *parent_op == child_op {
            continue;
        }
        let child_kind = MergeOpKind::from_op(child_op);
        let parent_kind = MergeOpKind::from_op(parent_op);
        for path in shared_paths(child_op, parent_op) {
            let kind = classify_conflict(child_op, parent_op);
            let key = (path.as_str().to_owned(), kind, child_kind, parent_kind);
            if !seen.insert(key) {
                continue;
            }
            if conflicts.len() >= MAX_MERGE_CONFLICTS {
                return Err(MergeError::BoundExceeded);
            }
            conflicts.push(MergeConflict {
                path,
                kind,
                child_op: child_kind,
                parent_op: parent_kind,
            });
        }
    }
    Ok(())
}

fn classify_conflict(child: &PatchOp, parent: &PatchOp) -> MergeConflictKind {
    if MergeOpKind::from_op(child) == MergeOpKind::from_op(parent) {
        MergeConflictKind::Path
    } else {
        MergeConflictKind::Op
    }
}

fn occupied_paths(op: &PatchOp) -> Vec<&RepoPath> {
    match op {
        PatchOp::ReplaceRange { path, .. }
        | PatchOp::CreateFile { path, .. }
        | PatchOp::DeleteFile { path, .. } => vec![path],
        PatchOp::MoveFile { from, to, .. } => vec![from, to],
    }
}

fn paths_overlap(left: &PatchOp, right: &PatchOp) -> bool {
    occupied_paths(left)
        .into_iter()
        .any(|path| occupied_paths(right).into_iter().any(|other| other == path))
}

fn shared_paths(left: &PatchOp, right: &PatchOp) -> Vec<RepoPath> {
    let mut out = Vec::new();
    for path in occupied_paths(left) {
        if occupied_paths(right).into_iter().any(|other| other == path)
            && !out.iter().any(|seen| seen == path)
        {
            out.push(path.clone());
        }
    }
    out
}

fn reject_git_mutation(path: &RepoPath) -> Result<(), MergeError> {
    if path
        .components()
        .any(|part| part.eq_ignore_ascii_case(".git"))
    {
        Err(MergeError::GitScopeRequired)
    } else {
        Ok(())
    }
}

fn patch_hash(patch: &SemanticPatch, cancel: &CancellationToken) -> Result<ArtifactId, MergeError> {
    match patch.hash(cancel) {
        Ok(hash) => Ok(hash),
        Err(PatchError::Cancelled) => Err(MergeError::Cancelled),
        Err(err) => Err(MergeError::Patch(err)),
    }
}

fn hash_preview(
    child_view_id: WorkspaceViewId,
    parent_view_id: WorkspaceViewId,
    parent_revision: &str,
    child_patch_hash: ArtifactId,
    parent_patch_hash: ArtifactId,
    ops: &[PatchOp],
    conflicts: &[MergeConflict],
    checks: &[VerificationCheck],
    cancel: &CancellationToken,
) -> Result<ArtifactId, MergeError> {
    check_cancel(cancel)?;
    let mut buf = Vec::new();
    buf.extend_from_slice(MERGE_PREVIEW_SCHEMA.as_bytes());
    buf.extend_from_slice(&MERGE_PREVIEW_SCHEMA_VERSION.to_be_bytes());
    buf.extend_from_slice(child_view_id.as_uuid().as_bytes());
    buf.extend_from_slice(parent_view_id.as_uuid().as_bytes());
    append_bytes(&mut buf, parent_revision.as_bytes());
    buf.extend_from_slice(child_patch_hash.as_digest());
    buf.extend_from_slice(parent_patch_hash.as_digest());
    buf.extend_from_slice(&(ops.len() as u64).to_be_bytes());
    for (index, op) in ops.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        encode_op(&mut buf, op);
    }
    buf.extend_from_slice(&(conflicts.len() as u64).to_be_bytes());
    for (index, conflict) in conflicts.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        append_bytes(&mut buf, conflict.path.as_str().as_bytes());
        buf.push(conflict.kind.tag());
        buf.push(conflict.child_op.tag());
        buf.push(conflict.parent_op.tag());
    }
    buf.extend_from_slice(&(checks.len() as u64).to_be_bytes());
    for check in checks {
        buf.push(check.tag());
    }
    Ok(ArtifactId::from_bytes(&buf))
}

fn encode_op(buf: &mut Vec<u8>, op: &PatchOp) {
    match op {
        PatchOp::ReplaceRange {
            path,
            preimage,
            start,
            end,
            content,
        } => {
            buf.push(1);
            append_bytes(buf, path.as_str().as_bytes());
            buf.extend_from_slice(preimage.as_digest());
            buf.extend_from_slice(&start.as_u64().to_be_bytes());
            buf.extend_from_slice(&end.as_u64().to_be_bytes());
            append_bytes(buf, content.as_bytes());
        }
        PatchOp::CreateFile {
            path,
            content,
            executable,
        } => {
            buf.push(2);
            append_bytes(buf, path.as_str().as_bytes());
            append_bytes(buf, content);
            buf.push(u8::from(*executable));
        }
        PatchOp::DeleteFile { path, preimage } => {
            buf.push(3);
            append_bytes(buf, path.as_str().as_bytes());
            buf.extend_from_slice(preimage.as_digest());
        }
        PatchOp::MoveFile { from, to, preimage } => {
            buf.push(4);
            append_bytes(buf, from.as_str().as_bytes());
            append_bytes(buf, to.as_str().as_bytes());
            buf.extend_from_slice(preimage.as_digest());
        }
    }
}

fn append_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(bytes);
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), MergeError> {
    if cancel.is_cancelled() {
        Err(MergeError::Cancelled)
    } else {
        Ok(())
    }
}

fn parse_closed<T: Copy>(
    raw: &str,
    all: &[T],
    as_str: fn(T) -> &'static str,
) -> Result<T, MergeError> {
    for item in all {
        if as_str(*item) == raw {
            return Ok(*item);
        }
    }
    Err(MergeError::UnknownVariant)
}

fn deserialize_closed<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: FromStr<Err = MergeError>,
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    raw.parse()
        .map_err(|_| de::Error::unknown_variant(&raw, &[]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::direct::{DirectBackend, DirectOptions};
    use crate::view::{CreateView, ViewAccess, ViewRegistry, WorkspaceBackend};
    use protocol::{AgentId, RepoId};
    use std::fs;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, Ordering};

    const AUTHOR: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";
    const CHILD_JSON: &str = r#"{"schema":"rapidlm.workspace_view","schema_version":2,"id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","repo_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ac","backend":"git_worktree","base_revision":"deadbeef","write_owner":null,"state":"quiescent","access":"read_write","generation":1,"scope":[]}"#;
    const PARENT_JSON: &str = r#"{"schema":"rapidlm.workspace_view","schema_version":2,"id":"018f3c8a-7e2b-7a10-8c4d-0123456789ae","repo_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ac","backend":"direct","base_revision":"deadbeef","write_owner":null,"state":"active","access":"read_write","generation":1,"scope":[]}"#;
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

    fn golden_pair() -> (WorkspaceView, WorkspaceView) {
        let child = serde_json::from_str(CHILD_JSON).expect("child");
        let parent = serde_json::from_str(PARENT_JSON).expect("parent");
        (child, parent)
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
                ),
                &cancel(),
            )
            .expect("parent");
        let child = registry.quiesce(child.id(), &cancel()).expect("quiesce");
        (registry, child, parent)
    }

    fn fixture_parent() -> (ViewRegistry, WorkspaceView, WorkspaceView, Fixture) {
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
        let parent_view = registry
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
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-merge-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("src")).expect("mkdir");
        fs::write(dir.join("src/lib.rs"), b"hello").expect("seed");
        let backend = DirectBackend::open_with(
            &dir,
            parent_view.clone(),
            DirectOptions::interactive(),
            &cancel(),
        )
        .expect("open");
        (
            registry,
            child,
            parent_view,
            Fixture {
                dir,
                backend: Some(backend),
            },
        )
    }

    #[test]
    fn disjoint_child_ops_preview_without_conflicts() {
        let (_registry, child, parent) = live_pair();
        let child_patch = patch(vec![replace("src/lib.rs", 0, 5, "world")]);
        let parent_patch = patch(vec![replace("src/keep.rs", 0, 4, "held")]);
        let preview = preview_merge(&child, &parent, &child_patch, &parent_patch, &cancel())
            .expect("preview");
        assert!(preview.is_conflict_free());
        assert_eq!(preview.ops(), child_patch.ops());
        assert_eq!(preview.parent_revision(), "deadbeef");
        assert_eq!(preview.child_view_id(), child.id());
        assert_eq!(preview.parent_view_id(), parent.id());
        assert_eq!(preview.code(), None);
        assert_eq!(
            preview.verification_plan().checks(),
            &[
                VerificationCheck::ParentRevision,
                VerificationCheck::ConflictFree,
                VerificationCheck::Preimage,
            ]
        );
        assert_eq!(
            preview.verification_plan().child_patch_hash(),
            child_patch.hash(&cancel()).expect("child hash")
        );
    }

    #[test]
    fn overlapping_sibling_replaces_are_path_conflicts() {
        let (_registry, child, parent) = live_pair();
        let child_patch = patch(vec![replace("src/lib.rs", 0, 5, "child")]);
        let sibling = patch(vec![replace("src/lib.rs", 0, 5, "sib")]);
        let preview =
            preview_merge(&child, &parent, &child_patch, &sibling, &cancel()).expect("preview");
        assert!(!preview.is_conflict_free());
        assert!(preview.ops().is_empty());
        assert_eq!(preview.code(), Some(ErrorCode::WorkspaceMergeConflict));
        assert_eq!(preview.conflicts().len(), 1);
        let conflict = &preview.conflicts()[0];
        assert_eq!(conflict.path().as_str(), "src/lib.rs");
        assert_eq!(conflict.kind(), MergeConflictKind::Path);
        assert_eq!(conflict.child_op(), MergeOpKind::ReplaceRange);
        assert_eq!(conflict.parent_op(), MergeOpKind::ReplaceRange);
    }

    #[test]
    fn delete_versus_replace_is_an_op_conflict() {
        let (_registry, child, parent) = live_pair();
        let child_patch = patch(vec![PatchOp::delete_file(
            repo_path("src/lib.rs"),
            hello_preimage(),
        )]);
        let sibling = patch(vec![replace("src/lib.rs", 0, 5, "sib")]);
        let preview =
            preview_merge(&child, &parent, &child_patch, &sibling, &cancel()).expect("preview");
        assert_eq!(preview.conflicts().len(), 1);
        assert_eq!(preview.conflicts()[0].kind(), MergeConflictKind::Op);
        assert_eq!(preview.conflicts()[0].child_op(), MergeOpKind::DeleteFile);
        assert_eq!(
            preview.conflicts()[0].parent_op(),
            MergeOpKind::ReplaceRange
        );
        assert!(preview.ops().is_empty());
    }

    #[test]
    fn identical_already_applied_ops_are_not_conflicts() {
        let (_registry, child, parent) = live_pair();
        let op = replace("src/lib.rs", 0, 5, "world");
        let child_patch = patch(vec![op.clone()]);
        let parent_patch = patch(vec![op]);
        let preview = preview_merge(&child, &parent, &child_patch, &parent_patch, &cancel())
            .expect("preview");
        assert!(preview.is_conflict_free());
        assert!(preview.ops().is_empty());
    }

    #[test]
    fn move_destination_versus_create_is_an_op_conflict() {
        let (_registry, child, parent) = live_pair();
        let child_patch = patch(vec![
            PatchOp::move_file(
                repo_path("src/a.rs"),
                repo_path("src/b.rs"),
                hello_preimage(),
            )
            .expect("move"),
        ]);
        let sibling = patch(vec![
            PatchOp::create_file(repo_path("src/b.rs"), b"other".to_vec(), false).expect("create"),
        ]);
        let preview =
            preview_merge(&child, &parent, &child_patch, &sibling, &cancel()).expect("preview");
        assert_eq!(preview.conflicts().len(), 1);
        assert_eq!(preview.conflicts()[0].path().as_str(), "src/b.rs");
        assert_eq!(preview.conflicts()[0].kind(), MergeConflictKind::Op);
        assert!(preview.ops().is_empty());
    }

    #[test]
    fn preview_does_not_mutate_parent_checkout() {
        let (_registry, child, parent, fx) = fixture_parent();
        let source = fx.backend.as_ref().expect("backend");
        let before = fs::read(fx.dir.join("src/lib.rs")).expect("read");
        let preview = preview_merge(
            &child,
            &parent,
            &patch(vec![replace("src/lib.rs", 0, 5, "world")]),
            &empty_patch(),
            &cancel(),
        )
        .expect("preview");
        assert!(preview.is_conflict_free());
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("after"), before);
        assert_eq!(before, b"hello");
        assert!(source.journal().expect("journal").is_empty());
        assert_eq!(parent.state(), crate::view::WorkspaceState::Active);
        assert_eq!(parent.base_revision(), "deadbeef");
    }

    #[test]
    fn active_child_is_rejected() {
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
                ),
                &cancel(),
            )
            .expect("parent");
        let err = preview_merge(&child, &parent, &empty_patch(), &empty_patch(), &cancel())
            .expect_err("active");
        assert_eq!(err, MergeError::InvalidState);
    }

    #[test]
    fn read_only_parent_is_rejected() {
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
                    ViewAccess::ReadOnly,
                ),
                &cancel(),
            )
            .expect("parent");
        let child = registry.quiesce(child.id(), &cancel()).expect("quiesce");
        let err = preview_merge(&child, &parent, &empty_patch(), &empty_patch(), &cancel())
            .expect_err("ro");
        assert_eq!(err, MergeError::ReadOnlyView);
    }

    #[test]
    fn git_path_is_rejected_without_preview_ops() {
        let (_registry, child, parent) = live_pair();
        let child_patch = patch(vec![
            PatchOp::create_file(repo_path(".git/hooks/pre-commit"), b"evil".to_vec(), true)
                .expect("create"),
        ]);
        let err = preview_merge(&child, &parent, &child_patch, &empty_patch(), &cancel())
            .expect_err("git");
        assert_eq!(err, MergeError::GitScopeRequired);
        let shown = err.to_string();
        assert_eq!(shown, "mutating .git requires a dedicated git capability");
        assert!(!shown.contains("pre-commit"));
        assert!(!shown.contains("evil"));
    }

    #[test]
    fn path_traversal_cannot_enter_a_preview() {
        assert!(RepoPath::parse("../secret").is_err());
        let (child, parent) = golden_pair();
        let preview = preview_merge(
            &child,
            &parent,
            &patch(vec![replace("src/lib.rs", 0, 5, "world")]),
            &empty_patch(),
            &cancel(),
        )
        .expect("preview");
        let json = serde_json::to_string(&preview).expect("json");
        let tampered = json.replace("src/lib.rs", "../secret");
        assert!(serde_json::from_str::<MergePreview>(&tampered).is_err());
    }

    #[test]
    fn cancelled_preview_fails_closed() {
        let (_registry, child, parent) = live_pair();
        let token = CancellationToken::new();
        token.cancel();
        let err = preview_merge(
            &child,
            &parent,
            &patch(vec![replace("src/lib.rs", 0, 5, "world")]),
            &empty_patch(),
            &token,
        )
        .expect_err("cancelled");
        assert_eq!(err, MergeError::Cancelled);
    }

    #[test]
    fn golden_preview_round_trips() {
        let (child, parent) = golden_pair();
        let child_patch = patch(vec![replace("src/lib.rs", 0, 5, "world")]);
        let preview = preview_merge(&child, &parent, &child_patch, &empty_patch(), &cancel())
            .expect("preview");
        let json = serde_json::to_string(&preview).expect("serialize");
        let decoded = serde_json::from_str::<MergePreview>(&json).expect("deserialize");
        assert_eq!(decoded, preview);
        assert!(json.contains("\"schema\":\"rapidlm.merge_preview\""));
        assert!(json.contains("\"schema_version\":1"));
        assert_eq!(
            decoded.preview_hash(),
            hash_preview(
                child.id(),
                parent.id(),
                "deadbeef",
                child_patch.hash(&cancel()).expect("hash"),
                empty_patch().hash(&cancel()).expect("empty"),
                preview.ops(),
                preview.conflicts(),
                preview.verification_plan().checks(),
                &cancel(),
            )
            .expect("hash")
        );
        let extra = json.replacen('{', r#"{"extra":true,"#, 1);
        assert!(serde_json::from_str::<MergePreview>(&extra).is_err());
        let bad_hash = json.replace(
            &preview.preview_hash().to_string(),
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        );
        assert!(serde_json::from_str::<MergePreview>(&bad_hash).is_err());
    }

    #[test]
    fn merge_view_request_matches_function() {
        let (_registry, child, parent) = live_pair();
        let child_patch = patch(vec![replace("src/a.rs", 0, 1, "A")]);
        let parent_patch = patch(vec![replace("src/b.rs", 0, 1, "B")]);
        let req = MergeView::new(
            child.clone(),
            parent.clone(),
            child_patch.clone(),
            parent_patch.clone(),
        );
        let preview = req.preview(&cancel()).expect("preview");
        let direct =
            preview_merge(&child, &parent, &child_patch, &parent_patch, &cancel()).expect("direct");
        assert_eq!(preview, direct);
        assert!(preview.is_conflict_free());
    }

    #[test]
    fn same_view_and_repo_mismatch_fail_closed() {
        let (child, _parent) = golden_pair();
        let err = preview_merge(&child, &child, &empty_patch(), &empty_patch(), &cancel())
            .expect_err("same");
        assert_eq!(err, MergeError::SameView);
        let other = serde_json::from_str::<WorkspaceView>(
            r#"{"schema":"rapidlm.workspace_view","schema_version":2,"id":"018f3c8a-7e2b-7a10-8c4d-0123456789af","repo_id":"018f3c8a-7e2b-7a10-8c4d-0123456789b0","backend":"direct","base_revision":"deadbeef","write_owner":null,"state":"active","access":"read_write","generation":1,"scope":[]}"#,
        )
        .expect("other");
        let err = preview_merge(&child, &other, &empty_patch(), &empty_patch(), &cancel())
            .expect_err("repo");
        assert_eq!(err, MergeError::RepoMismatch);
        assert!(!err.to_string().contains("018f3c8a"));
    }

    #[test]
    fn platform_separator_overlap_is_still_a_conflict() {
        let (_registry, child, parent) = live_pair();
        let child_patch = patch(vec![replace("crates/workspace/src/lib.rs", 0, 5, "SECRET")]);
        let sibling = patch(vec![replace(
            "crates\\workspace\\src\\lib.rs",
            7,
            12,
            "LEAK",
        )]);
        let preview =
            preview_merge(&child, &parent, &child_patch, &sibling, &cancel()).expect("preview");
        assert_eq!(preview.conflicts().len(), 1);
        assert_eq!(
            preview.conflicts()[0].path().as_str(),
            "crates/workspace/src/lib.rs"
        );
        let shown = MergeError::GitScopeRequired.to_string();
        assert!(!shown.contains("SECRET"));
        assert!(!shown.contains("LEAK"));
    }

    #[test]
    fn tampered_git_path_or_swapped_op_content_fails_deserialize() {
        let (child, parent) = golden_pair();
        let preview = preview_merge(
            &child,
            &parent,
            &patch(vec![replace("src/lib.rs", 0, 5, "world")]),
            &empty_patch(),
            &cancel(),
        )
        .expect("preview");
        let json = serde_json::to_string(&preview).expect("json");

        let git_path = json.replace("src/lib.rs", ".git/hooks/pre-commit");
        assert_ne!(git_path, json);
        assert!(serde_json::from_str::<MergePreview>(&git_path).is_err());

        let swapped = json.replace("world", "other");
        assert_ne!(swapped, json);
        assert!(serde_json::from_str::<MergePreview>(&swapped).is_err());

        let recomputed = hash_preview(
            preview.child_view_id(),
            preview.parent_view_id(),
            preview.parent_revision(),
            preview.verification_plan().child_patch_hash(),
            preview.verification_plan().parent_patch_hash(),
            &[replace("src/lib.rs", 0, 5, "other")],
            preview.conflicts(),
            preview.verification_plan().checks(),
            &cancel(),
        )
        .expect("recomputed");
        assert_ne!(recomputed, preview.preview_hash());
    }

    #[test]
    fn bound_exceeded_and_base_revision_mismatch_fail_closed() {
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
        let mismatched_parent = registry
            .create(
                CreateView::new(
                    repo,
                    WorkspaceBackend::Direct,
                    "cafebabe",
                    ViewAccess::ReadWrite,
                ),
                &cancel(),
            )
            .expect("parent");
        let child = registry.quiesce(child.id(), &cancel()).expect("quiesce");
        let err = preview_merge(
            &child,
            &mismatched_parent,
            &empty_patch(),
            &SemanticPatch::new(Vec::new(), author(), "cafebabe", &cancel()).expect("parent patch"),
            &cancel(),
        )
        .expect_err("view revision");
        assert_eq!(err, MergeError::BaseRevisionMismatch);
        assert!(!err.to_string().contains("deadbeef"));
        assert!(!err.to_string().contains("cafebabe"));

        let (_registry, child, parent) = live_pair();
        let stale = SemanticPatch::new(Vec::new(), author(), "cafebabe", &cancel()).expect("stale");
        let err = preview_merge(&child, &parent, &stale, &empty_patch(), &cancel())
            .expect_err("patch revision");
        assert_eq!(err, MergeError::BaseRevisionMismatch);
        assert_eq!(
            err.to_string(),
            "merge preview base revision does not match"
        );

        let (child, parent) = golden_pair();
        let preview = preview_merge(
            &child,
            &parent,
            &patch(vec![replace("src/lib.rs", 0, 5, "world")]),
            &empty_patch(),
            &cancel(),
        )
        .expect("preview");
        let oversized = vec![replace("src/lib.rs", 0, 5, "world"); MAX_MERGE_OPS + 1];
        let err = accept_preview(
            preview.child_view_id(),
            preview.parent_view_id(),
            preview.parent_revision().to_owned(),
            oversized,
            preview.conflicts().to_vec(),
            preview.verification_plan().clone(),
            Some(preview.preview_hash()),
            &cancel(),
        )
        .expect_err("ops bound");
        assert_eq!(err, MergeError::BoundExceeded);
        assert_eq!(
            err.to_string(),
            "workspace merge preview resource bound exceeded"
        );

        let mut value = serde_json::to_value(&preview).expect("value");
        let too_long = "a".repeat(MAX_BASE_REVISION_BYTES + 1);
        value["parent_revision"] = serde_json::Value::String(too_long.clone());
        value["verification_plan"]["parent_revision"] = serde_json::Value::String(too_long);
        let err = serde_json::from_value::<MergePreview>(value).expect_err("revision bound");
        assert!(err.to_string().contains("resource bound exceeded"));
        assert!(!err.to_string().contains("deadbeef"));
    }
}
