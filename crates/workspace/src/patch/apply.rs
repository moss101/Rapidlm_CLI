//! Apply a semantic patch to an in-memory staging overlay.
//!
//! Every preimage is checked against the current overlay/source bytes before
//! any slot is updated. A single mismatch leaves the overlay unchanged.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use protocol::{ArtifactId, ErrorCode, RepoPath, WorkspaceViewId};

use crate::backends::direct::{
    DirectBackend, DirectError, DirectResolveMode, MAX_DIRECT_FILE_BYTES,
};
use crate::patch::model::{
    MAX_PATCH_CONTENT_BYTES, MAX_PATCH_OPS, PatchError, PatchOp, SemanticPatch,
};
use crate::view::{CancellationToken, WorkspaceBackend, WorkspaceState, WorkspaceView};

/// Maximum present files retained in one overlay.
pub const MAX_OVERLAY_FILES: usize = MAX_PATCH_OPS;

/// Maximum combined present-file bytes retained in one overlay.
pub const MAX_OVERLAY_BYTES: usize = MAX_PATCH_CONTENT_BYTES;

const CANCEL_STRIDE: usize = 8;

/// One present file in a staging overlay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayFile {
    bytes: Vec<u8>,
    hash: ArtifactId,
    executable: bool,
}

/// In-memory staging overlay bound to one [`WorkspaceView`].
///
/// Overlay mutations never write the source checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagingOverlay {
    view_id: WorkspaceViewId,
    base_revision: String,
    slots: BTreeMap<RepoPath, OverlaySlot>,
}

/// Receipt for one atomic apply. Contains the overlay after the patch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedPatch {
    overlay: StagingOverlay,
    patch_hash: ArtifactId,
    changes: Vec<StagedChange>,
}

/// One logical path mutation produced by a successful apply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StagedChange {
    Create {
        path: RepoPath,
        after: ArtifactId,
        executable: bool,
    },
    Replace {
        path: RepoPath,
        before: ArtifactId,
        after: ArtifactId,
        executable: bool,
    },
    Delete {
        path: RepoPath,
        before: ArtifactId,
        executable: bool,
    },
    Move {
        from: RepoPath,
        to: RepoPath,
        preimage: ArtifactId,
        executable: bool,
    },
}

/// Typed apply failure. Display never echoes paths or payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyError {
    Cancelled,
    ReadOnlyView,
    InvalidState,
    BaseRevisionMismatch,
    ViewMismatch,
    WrongBackend,
    PreimageMismatch { path: RepoPath },
    NotFound { path: RepoPath },
    AlreadyExists { path: RepoPath },
    InvalidRange { path: RepoPath },
    PathEscape,
    GitScopeRequired,
    OutOfScope,
    BoundExceeded,
    Patch(PatchError),
    Source(DirectError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum OverlaySlot {
    Present(OverlayFile),
    Deleted { hash: ArtifactId, executable: bool },
}

impl OverlayFile {
    fn new(bytes: Vec<u8>, executable: bool) -> Result<Self, ApplyError> {
        if bytes.len() > MAX_DIRECT_FILE_BYTES {
            return Err(ApplyError::BoundExceeded);
        }
        let hash = ArtifactId::from_bytes(&bytes);
        Ok(Self {
            bytes,
            hash,
            executable,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn hash(&self) -> ArtifactId {
        self.hash
    }

    pub fn executable(&self) -> bool {
        self.executable
    }
}

impl StagingOverlay {
    /// Bind an empty overlay to `view`. The view must be an active write view.
    pub fn new(view: &WorkspaceView) -> Result<Self, ApplyError> {
        check_view(view)?;
        Ok(Self {
            view_id: view.id(),
            base_revision: view.base_revision().to_owned(),
            slots: BTreeMap::new(),
        })
    }

    pub fn view_id(&self) -> WorkspaceViewId {
        self.view_id
    }

    pub fn base_revision(&self) -> &str {
        &self.base_revision
    }

    pub fn get(&self, path: &RepoPath) -> Option<&OverlayFile> {
        match self.slots.get(path) {
            Some(OverlaySlot::Present(file)) => Some(file),
            Some(OverlaySlot::Deleted { .. }) | None => None,
        }
    }

    pub fn is_deleted(&self, path: &RepoPath) -> bool {
        matches!(self.slots.get(path), Some(OverlaySlot::Deleted { .. }))
    }

    pub fn staged_path_count(&self) -> usize {
        self.slots.len()
    }

    /// Validate every preimage, then apply `patch` or leave `self` unchanged.
    pub fn apply_patch(
        &mut self,
        patch: &SemanticPatch,
        source: &DirectBackend,
        cancel: &CancellationToken,
    ) -> Result<StagedPatch, ApplyError> {
        check_cancel(cancel)?;
        check_view(source.view())?;
        if source.view().id() != self.view_id {
            return Err(ApplyError::ViewMismatch);
        }
        if source.view().base_revision() != self.base_revision
            || patch.base_revision() != self.base_revision
        {
            return Err(ApplyError::BaseRevisionMismatch);
        }
        match patch.validate(cancel) {
            Ok(()) => {}
            Err(PatchError::Cancelled) => return Err(ApplyError::Cancelled),
            Err(err) => return Err(ApplyError::Patch(err)),
        }

        let mut scratch = self.slots.clone();
        let mut cache: BTreeMap<RepoPath, Option<OverlayFile>> = BTreeMap::new();
        let mut changes = Vec::with_capacity(patch.ops().len());

        for (index, op) in patch.ops().iter().enumerate() {
            if index.is_multiple_of(CANCEL_STRIDE) {
                check_cancel(cancel)?;
            }
            apply_op(
                op,
                source.view(),
                &mut scratch,
                &mut cache,
                source,
                cancel,
                &mut changes,
            )?;
            check_overlay_bounds(&scratch)?;
        }

        let patch_hash = match patch.hash(cancel) {
            Ok(hash) => hash,
            Err(PatchError::Cancelled) => return Err(ApplyError::Cancelled),
            Err(err) => return Err(ApplyError::Patch(err)),
        };

        self.slots = scratch;
        Ok(StagedPatch {
            overlay: self.clone(),
            patch_hash,
            changes,
        })
    }
}

/// Validate all preimages, then apply `patch` to a fresh overlay for `view`.
pub fn apply_patch(
    view: &WorkspaceView,
    patch: &SemanticPatch,
    source: &DirectBackend,
    cancel: &CancellationToken,
) -> Result<StagedPatch, ApplyError> {
    let mut overlay = StagingOverlay::new(view)?;
    overlay.apply_patch(patch, source, cancel)
}

impl StagedPatch {
    pub fn overlay(&self) -> &StagingOverlay {
        &self.overlay
    }

    pub fn view_id(&self) -> WorkspaceViewId {
        self.overlay.view_id
    }

    pub fn patch_hash(&self) -> ArtifactId {
        self.patch_hash
    }

    pub fn base_revision(&self) -> &str {
        self.overlay.base_revision()
    }

    pub fn changes(&self) -> &[StagedChange] {
        &self.changes
    }

    pub fn change_count(&self) -> usize {
        self.changes.len()
    }
}

impl ApplyError {
    /// Public machine code for preimage failures; other variants stay internal.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::PreimageMismatch { .. } => Some(ErrorCode::WorkspacePreimageMismatch),
            _ => None,
        }
    }
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("semantic patch apply cancelled"),
            Self::ReadOnlyView => f.write_str("workspace view is read-only"),
            Self::InvalidState => f.write_str("workspace view is not in a valid state"),
            Self::BaseRevisionMismatch => {
                f.write_str("semantic patch base revision does not match the view")
            }
            Self::ViewMismatch => f.write_str("semantic patch view does not match the source"),
            Self::WrongBackend => f.write_str("workspace view is not a direct checkout"),
            Self::PreimageMismatch { .. } => f.write_str("workspace preimage does not match"),
            Self::NotFound { .. } => f.write_str("semantic patch path not found"),
            Self::AlreadyExists { .. } => f.write_str("semantic patch path already exists"),
            Self::InvalidRange { .. } => f.write_str("semantic patch replace range is invalid"),
            Self::PathEscape => f.write_str("resolved path escapes the checkout root"),
            Self::GitScopeRequired => {
                f.write_str("mutating .git requires a dedicated git capability")
            }
            Self::OutOfScope => f.write_str("path is outside the workspace view scope"),
            Self::BoundExceeded => f.write_str("semantic patch overlay resource bound exceeded"),
            Self::Patch(err) => write!(f, "{err}"),
            Self::Source(err) => write!(f, "{err}"),
        }
    }
}

impl Error for ApplyError {}

fn apply_op(
    op: &PatchOp,
    view: &WorkspaceView,
    scratch: &mut BTreeMap<RepoPath, OverlaySlot>,
    cache: &mut BTreeMap<RepoPath, Option<OverlayFile>>,
    source: &DirectBackend,
    cancel: &CancellationToken,
    changes: &mut Vec<StagedChange>,
) -> Result<(), ApplyError> {
    match op {
        PatchOp::ReplaceRange {
            path,
            preimage,
            start,
            end,
            content,
        } => {
            reject_git_mutation(path)?;
            reject_out_of_scope(view, path)?;
            let current = require_file(path, scratch, cache, source, cancel)?;
            if current.hash != *preimage {
                return Err(ApplyError::PreimageMismatch { path: path.clone() });
            }
            let next = splice_range(
                path,
                &current,
                start.as_u64(),
                end.as_u64(),
                content.as_bytes(),
            )?;
            changes.push(StagedChange::Replace {
                path: path.clone(),
                before: current.hash,
                after: next.hash,
                executable: next.executable,
            });
            scratch.insert(path.clone(), OverlaySlot::Present(next));
        }
        PatchOp::CreateFile {
            path,
            content,
            executable,
        } => {
            reject_git_mutation(path)?;
            reject_out_of_scope(view, path)?;
            if current_file(path, scratch, cache, source, cancel)?.is_some() {
                return Err(ApplyError::AlreadyExists { path: path.clone() });
            }
            let file = OverlayFile::new(content.clone(), *executable)?;
            changes.push(StagedChange::Create {
                path: path.clone(),
                after: file.hash,
                executable: file.executable,
            });
            scratch.insert(path.clone(), OverlaySlot::Present(file));
        }
        PatchOp::DeleteFile { path, preimage } => {
            reject_git_mutation(path)?;
            reject_out_of_scope(view, path)?;
            let current = require_file(path, scratch, cache, source, cancel)?;
            if current.hash != *preimage {
                return Err(ApplyError::PreimageMismatch { path: path.clone() });
            }
            changes.push(StagedChange::Delete {
                path: path.clone(),
                before: current.hash,
                executable: current.executable,
            });
            scratch.insert(
                path.clone(),
                OverlaySlot::Deleted {
                    hash: current.hash,
                    executable: current.executable,
                },
            );
        }
        PatchOp::MoveFile { from, to, preimage } => {
            reject_git_mutation(from)?;
            reject_git_mutation(to)?;
            reject_out_of_scope(view, from)?;
            reject_out_of_scope(view, to)?;
            let current = require_file(from, scratch, cache, source, cancel)?;
            if current.hash != *preimage {
                return Err(ApplyError::PreimageMismatch { path: from.clone() });
            }
            if current_file(to, scratch, cache, source, cancel)?.is_some() {
                return Err(ApplyError::AlreadyExists { path: to.clone() });
            }
            changes.push(StagedChange::Move {
                from: from.clone(),
                to: to.clone(),
                preimage: current.hash,
                executable: current.executable,
            });
            scratch.insert(
                from.clone(),
                OverlaySlot::Deleted {
                    hash: current.hash,
                    executable: current.executable,
                },
            );
            scratch.insert(to.clone(), OverlaySlot::Present(current));
        }
    }
    Ok(())
}

fn require_file(
    path: &RepoPath,
    scratch: &BTreeMap<RepoPath, OverlaySlot>,
    cache: &mut BTreeMap<RepoPath, Option<OverlayFile>>,
    source: &DirectBackend,
    cancel: &CancellationToken,
) -> Result<OverlayFile, ApplyError> {
    current_file(path, scratch, cache, source, cancel)?
        .ok_or_else(|| ApplyError::NotFound { path: path.clone() })
}

fn current_file(
    path: &RepoPath,
    scratch: &BTreeMap<RepoPath, OverlaySlot>,
    cache: &mut BTreeMap<RepoPath, Option<OverlayFile>>,
    source: &DirectBackend,
    cancel: &CancellationToken,
) -> Result<Option<OverlayFile>, ApplyError> {
    match scratch.get(path) {
        Some(OverlaySlot::Present(file)) => return Ok(Some(file.clone())),
        Some(OverlaySlot::Deleted { .. }) => return Ok(None),
        None => {}
    }
    if let Some(cached) = cache.get(path) {
        return Ok(cached.clone());
    }
    let loaded = load_source(source, path, cancel)?;
    cache.insert(path.clone(), loaded.clone());
    Ok(loaded)
}

fn load_source(
    source: &DirectBackend,
    path: &RepoPath,
    cancel: &CancellationToken,
) -> Result<Option<OverlayFile>, ApplyError> {
    check_cancel(cancel)?;
    match source.read(path, cancel) {
        Ok(bytes) => {
            let resolved = source
                .resolve(path, DirectResolveMode::Read, cancel)
                .map_err(|err| map_source(err, path))?;
            let file = OverlayFile::new(bytes, file_executable(resolved.host()))?;
            Ok(Some(file))
        }
        Err(
            DirectError::NotFound | DirectError::UnresolvedPath | DirectError::UnresolvedParent,
        ) => Ok(None),
        Err(err) => Err(map_source(err, path)),
    }
}

fn splice_range(
    path: &RepoPath,
    current: &OverlayFile,
    start: u64,
    end: u64,
    content: &[u8],
) -> Result<OverlayFile, ApplyError> {
    let start =
        usize::try_from(start).map_err(|_| ApplyError::InvalidRange { path: path.clone() })?;
    let end = usize::try_from(end).map_err(|_| ApplyError::InvalidRange { path: path.clone() })?;
    if start > end || end > current.bytes.len() {
        return Err(ApplyError::InvalidRange { path: path.clone() });
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve(
            start
                .saturating_add(content.len())
                .saturating_add(current.bytes.len().saturating_sub(end)),
        )
        .map_err(|_| ApplyError::BoundExceeded)?;
    bytes.extend_from_slice(&current.bytes[..start]);
    bytes.extend_from_slice(content);
    bytes.extend_from_slice(&current.bytes[end..]);
    OverlayFile::new(bytes, current.executable)
}

fn check_overlay_bounds(slots: &BTreeMap<RepoPath, OverlaySlot>) -> Result<(), ApplyError> {
    if slots.len() > MAX_OVERLAY_FILES {
        return Err(ApplyError::BoundExceeded);
    }
    let mut total = 0usize;
    for slot in slots.values() {
        if let OverlaySlot::Present(file) = slot {
            total = total
                .checked_add(file.bytes.len())
                .ok_or(ApplyError::BoundExceeded)?;
            if total > MAX_OVERLAY_BYTES {
                return Err(ApplyError::BoundExceeded);
            }
        }
    }
    Ok(())
}

fn check_view(view: &WorkspaceView) -> Result<(), ApplyError> {
    if view.backend() != WorkspaceBackend::Direct {
        return Err(ApplyError::WrongBackend);
    }
    if !view.access().is_writable() {
        return Err(ApplyError::ReadOnlyView);
    }
    if view.state() != WorkspaceState::Active {
        return Err(ApplyError::InvalidState);
    }
    Ok(())
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), ApplyError> {
    if cancel.is_cancelled() {
        Err(ApplyError::Cancelled)
    } else {
        Ok(())
    }
}

fn reject_out_of_scope(view: &WorkspaceView, path: &RepoPath) -> Result<(), ApplyError> {
    if view.scope().contains(path) {
        Ok(())
    } else {
        Err(ApplyError::OutOfScope)
    }
}

fn reject_git_mutation(path: &RepoPath) -> Result<(), ApplyError> {
    if path
        .components()
        .any(|part| part.eq_ignore_ascii_case(".git"))
    {
        Err(ApplyError::GitScopeRequired)
    } else {
        Ok(())
    }
}

fn map_source(err: DirectError, path: &RepoPath) -> ApplyError {
    match err {
        DirectError::Cancelled => ApplyError::Cancelled,
        DirectError::NotFound | DirectError::UnresolvedPath => {
            ApplyError::NotFound { path: path.clone() }
        }
        DirectError::PathEscape => ApplyError::PathEscape,
        DirectError::GitScopeRequired => ApplyError::GitScopeRequired,
        DirectError::OutOfScope => ApplyError::OutOfScope,
        DirectError::BoundExceeded => ApplyError::BoundExceeded,
        DirectError::ReadOnlyView => ApplyError::ReadOnlyView,
        DirectError::WrongBackend => ApplyError::WrongBackend,
        DirectError::InvalidState => ApplyError::InvalidState,
        other => ApplyError::Source(other),
    }
}

fn file_executable(host: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(host)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = host;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::direct::DirectOptions;
    use crate::view::{CreateView, ViewAccess, ViewRegistry, ViewScope};
    use protocol::{AgentId, RepoId};
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

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn author() -> AgentId {
        AgentId::from_str(AUTHOR).expect("author")
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
            "rapidlm-apply-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("src")).expect("mkdir");
        fs::write(dir.join("src/lib.rs"), b"hello").expect("seed");
        fs::write(dir.join("src/keep.rs"), b"keep").expect("seed keep");
        let backend =
            DirectBackend::open_with(&dir, view(access), DirectOptions::interactive(), &cancel())
                .expect("open backend");
        Fixture {
            dir,
            backend: Some(backend),
        }
    }

    fn backend(fx: &Fixture) -> &DirectBackend {
        fx.backend.as_ref().expect("backend")
    }

    fn patch(ops: Vec<PatchOp>) -> SemanticPatch {
        SemanticPatch::new(ops, author(), "base-rev", &cancel()).expect("patch")
    }

    fn lib_preimage() -> ArtifactId {
        ArtifactId::from_bytes(b"hello")
    }

    #[test]
    fn apply_replace_stages_overlay_without_touching_disk() {
        let fx = fixture(ViewAccess::ReadWrite);
        let source = backend(&fx);
        let staged = apply_patch(
            source.view(),
            &patch(vec![
                PatchOp::replace_range(repo("src/lib.rs"), lib_preimage(), 0, 5, "world")
                    .expect("replace"),
            ]),
            source,
            &cancel(),
        )
        .expect("apply");
        assert_eq!(staged.change_count(), 1);
        assert_eq!(
            staged
                .overlay()
                .get(&repo("src/lib.rs"))
                .expect("file")
                .bytes(),
            b"world"
        );
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        assert!(source.journal().expect("journal").is_empty());
        match &staged.changes()[0] {
            StagedChange::Replace {
                before,
                after,
                executable,
                ..
            } => {
                assert_eq!(*before, lib_preimage());
                assert_eq!(*after, ArtifactId::from_bytes(b"world"));
                assert!(!*executable);
            }
            other => panic!("expected replace, got {other:?}"),
        }
    }

    #[test]
    fn one_bad_preimage_stages_zero_logical_changes() {
        let fx = fixture(ViewAccess::ReadWrite);
        let source = backend(&fx);
        let mut overlay = StagingOverlay::new(source.view()).expect("overlay");
        let bad = ArtifactId::from_bytes(b"nope");
        let err = overlay
            .apply_patch(
                &patch(vec![
                    PatchOp::replace_range(repo("src/lib.rs"), lib_preimage(), 0, 5, "world")
                        .expect("good"),
                    PatchOp::delete_file(repo("src/keep.rs"), bad),
                ]),
                source,
                &cancel(),
            )
            .expect_err("bad preimage");
        assert!(
            matches!(err, ApplyError::PreimageMismatch { ref path } if path.as_str() == "src/keep.rs")
        );
        assert_eq!(err.code(), Some(ErrorCode::WorkspacePreimageMismatch));
        assert_eq!(overlay.staged_path_count(), 0);
        assert!(overlay.get(&repo("src/lib.rs")).is_none());
        assert!(!overlay.is_deleted(&repo("src/keep.rs")));
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        assert_eq!(fs::read(fx.dir.join("src/keep.rs")).expect("disk"), b"keep");
        let shown = err.to_string();
        assert_eq!(shown, "workspace preimage does not match");
        assert!(!shown.contains("keep"));
        assert!(!shown.contains("world"));
        assert!(!shown.contains("nope"));
    }

    #[test]
    fn later_bad_preimage_does_not_drop_prior_staged_apply() {
        let fx = fixture(ViewAccess::ReadWrite);
        let source = backend(&fx);
        let mut overlay = StagingOverlay::new(source.view()).expect("overlay");
        overlay
            .apply_patch(
                &patch(vec![
                    PatchOp::create_file(repo("src/new.rs"), b"created".to_vec(), false)
                        .expect("create"),
                ]),
                source,
                &cancel(),
            )
            .expect("first");
        let before = overlay
            .get(&repo("src/new.rs"))
            .expect("staged")
            .bytes()
            .to_vec();
        let err = overlay
            .apply_patch(
                &patch(vec![
                    PatchOp::replace_range(
                        repo("src/new.rs"),
                        ArtifactId::from_bytes(b"created"),
                        0,
                        7,
                        "updated",
                    )
                    .expect("replace"),
                    PatchOp::delete_file(repo("src/lib.rs"), ArtifactId::from_bytes(b"wrong")),
                ]),
                source,
                &cancel(),
            )
            .expect_err("second");
        assert!(matches!(err, ApplyError::PreimageMismatch { .. }));
        assert_eq!(
            overlay.get(&repo("src/new.rs")).expect("kept").bytes(),
            before
        );
        assert!(!overlay.is_deleted(&repo("src/lib.rs")));
    }

    #[test]
    fn create_binary_and_delete_preserve_mode_bits() {
        let fx = fixture(ViewAccess::ReadWrite);
        let source = backend(&fx);
        let staged = apply_patch(
            source.view(),
            &patch(vec![
                PatchOp::create_file(repo("bin/tool"), b"\x7fELF".to_vec(), true).expect("create"),
                PatchOp::delete_file(repo("src/lib.rs"), lib_preimage()),
            ]),
            source,
            &cancel(),
        )
        .expect("apply");
        let created = staged.overlay().get(&repo("bin/tool")).expect("binary");
        assert_eq!(created.bytes(), b"\x7fELF");
        assert!(created.executable());
        assert!(staged.overlay().is_deleted(&repo("src/lib.rs")));
        match &staged.changes()[1] {
            StagedChange::Delete {
                executable, before, ..
            } => {
                assert!(!*executable);
                assert_eq!(*before, lib_preimage());
            }
            other => panic!("expected delete, got {other:?}"),
        }
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        assert!(!fx.dir.join("bin/tool").exists());
    }

    #[cfg(unix)]
    #[test]
    fn replace_and_move_preserve_disk_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let fx = fixture(ViewAccess::ReadWrite);
        fs::create_dir_all(fx.dir.join("bin")).expect("mkdir");
        fs::write(fx.dir.join("bin/tool"), b"\x7fELF").expect("bin");
        let mut perms = fs::metadata(fx.dir.join("bin/tool"))
            .expect("meta")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(fx.dir.join("bin/tool"), perms).expect("chmod");

        let source = backend(&fx);
        let preimage = ArtifactId::from_bytes(b"\x7fELF");
        let staged = apply_patch(
            source.view(),
            &patch(vec![
                PatchOp::replace_range(repo("bin/tool"), preimage, 1, 1, "").expect("touch"),
                PatchOp::move_file(repo("bin/tool"), repo("bin/moved"), preimage).expect("move"),
            ]),
            source,
            &cancel(),
        )
        .expect("apply");
        let moved = staged.overlay().get(&repo("bin/moved")).expect("moved");
        assert_eq!(moved.bytes(), b"\x7fELF");
        assert!(moved.executable());
        assert!(staged.overlay().is_deleted(&repo("bin/tool")));
        match &staged.changes()[1] {
            StagedChange::Move { executable, .. } => assert!(*executable),
            other => panic!("expected move, got {other:?}"),
        }
        let meta = fs::metadata(fx.dir.join("bin/tool")).expect("disk meta");
        assert_eq!(meta.permissions().mode() & 0o111, 0o111);
        assert!(!fx.dir.join("bin/moved").exists());
    }

    #[test]
    fn git_mutation_is_rejected_without_staging() {
        let fx = fixture(ViewAccess::ReadWrite);
        fs::create_dir_all(fx.dir.join(".git")).expect("git");
        fs::write(fx.dir.join(".git/config"), b"[core]\n").expect("config");
        let source = backend(&fx);
        let mut overlay = StagingOverlay::new(source.view()).expect("overlay");
        let err = overlay
            .apply_patch(
                &patch(vec![
                    PatchOp::create_file(repo(".git/hooks/pre-commit"), b"evil".to_vec(), true)
                        .expect("create"),
                ]),
                source,
                &cancel(),
            )
            .expect_err("git");
        assert_eq!(err, ApplyError::GitScopeRequired);
        assert_eq!(overlay.staged_path_count(), 0);
        assert_eq!(
            fs::read(fx.dir.join(".git/config")).expect("config"),
            b"[core]\n"
        );
        assert!(!err.to_string().contains("pre-commit"));
        assert!(!err.to_string().contains("evil"));
    }

    #[test]
    fn read_only_view_cannot_stage() {
        let fx = fixture(ViewAccess::ReadOnly);
        let err = StagingOverlay::new(backend(&fx).view()).expect_err("ro");
        assert_eq!(err, ApplyError::ReadOnlyView);
    }

    #[test]
    fn cancelled_apply_fails_closed() {
        let fx = fixture(ViewAccess::ReadWrite);
        let source = backend(&fx);
        let mut overlay = StagingOverlay::new(source.view()).expect("overlay");
        let token = CancellationToken::new();
        token.cancel();
        let err = overlay
            .apply_patch(
                &patch(vec![
                    PatchOp::replace_range(repo("src/lib.rs"), lib_preimage(), 0, 5, "world")
                        .expect("replace"),
                ]),
                source,
                &token,
            )
            .expect_err("cancelled");
        assert_eq!(err, ApplyError::Cancelled);
        assert_eq!(overlay.staged_path_count(), 0);
    }

    #[test]
    fn out_of_range_replace_does_not_stage() {
        let fx = fixture(ViewAccess::ReadWrite);
        let source = backend(&fx);
        let mut overlay = StagingOverlay::new(source.view()).expect("overlay");
        let err = overlay
            .apply_patch(
                &patch(vec![
                    PatchOp::replace_range(repo("src/lib.rs"), lib_preimage(), 0, 99, "x")
                        .expect("range in model"),
                ]),
                source,
                &cancel(),
            )
            .expect_err("range");
        assert!(matches!(err, ApplyError::InvalidRange { .. }));
        assert_eq!(overlay.staged_path_count(), 0);
        assert!(!err.to_string().contains("src/lib.rs"));
    }

    #[test]
    fn base_revision_mismatch_is_rejected() {
        let fx = fixture(ViewAccess::ReadWrite);
        let source = backend(&fx);
        let other = SemanticPatch::new(
            vec![PatchOp::delete_file(repo("src/lib.rs"), lib_preimage())],
            author(),
            "other-rev",
            &cancel(),
        )
        .expect("patch");
        let err = apply_patch(source.view(), &other, source, &cancel()).expect_err("rev");
        assert_eq!(err, ApplyError::BaseRevisionMismatch);
    }

    #[test]
    fn sequential_overlay_apply_uses_staged_bytes() {
        let fx = fixture(ViewAccess::ReadWrite);
        let source = backend(&fx);
        let mut overlay = StagingOverlay::new(source.view()).expect("overlay");
        overlay
            .apply_patch(
                &patch(vec![
                    PatchOp::replace_range(repo("src/lib.rs"), lib_preimage(), 0, 5, "world")
                        .expect("first"),
                ]),
                source,
                &cancel(),
            )
            .expect("first");
        overlay
            .apply_patch(
                &patch(vec![
                    PatchOp::replace_range(
                        repo("src/lib.rs"),
                        ArtifactId::from_bytes(b"world"),
                        0,
                        5,
                        "again",
                    )
                    .expect("second"),
                ]),
                source,
                &cancel(),
            )
            .expect("second");
        assert_eq!(
            overlay.get(&repo("src/lib.rs")).expect("file").bytes(),
            b"again"
        );
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
    }

    fn fixture_scoped(prefixes: &[&str]) -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-apply-scope-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("src")).expect("mkdir");
        fs::create_dir_all(dir.join("docs")).expect("mkdir docs");
        fs::write(dir.join("src/lib.rs"), b"hello").expect("seed");
        fs::write(dir.join("docs/readme.md"), b"# docs").expect("docs");
        let registry = ViewRegistry::new();
        let scope =
            ViewScope::prefixes(prefixes.iter().copied().map(repo).collect()).expect("scope");
        let spec = CreateView::new(
            RepoId::new(),
            WorkspaceBackend::Direct,
            "base-rev",
            ViewAccess::ReadWrite,
        )
        .with_write_owner(AgentId::new())
        .with_scope(scope);
        let view = registry.create(spec, &cancel()).expect("create view");
        let backend = DirectBackend::open_with(&dir, view, DirectOptions::interactive(), &cancel())
            .expect("open backend");
        Fixture {
            dir,
            backend: Some(backend),
        }
    }

    #[test]
    fn out_of_scope_path_does_not_stage() {
        let fx = fixture_scoped(&["src"]);
        let source = backend(&fx);
        let mut overlay = StagingOverlay::new(source.view()).expect("overlay");
        let err = overlay
            .apply_patch(
                &patch(vec![
                    PatchOp::replace_range(repo("src/lib.rs"), lib_preimage(), 0, 5, "world")
                        .expect("in-scope"),
                    PatchOp::create_file(repo("docs/readme.md"), b"stolen".to_vec(), false)
                        .expect("out-of-scope"),
                ]),
                source,
                &cancel(),
            )
            .expect_err("scope");
        assert_eq!(err, ApplyError::OutOfScope);
        assert_eq!(overlay.staged_path_count(), 0);
        assert!(overlay.get(&repo("src/lib.rs")).is_none());
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        assert_eq!(
            fs::read(fx.dir.join("docs/readme.md")).expect("docs"),
            b"# docs"
        );
        assert_eq!(err.to_string(), "path is outside the workspace view scope");
        assert!(!err.to_string().contains("docs"));
        assert!(!err.to_string().contains("stolen"));
    }

    #[test]
    fn wrong_preimage_hash_is_workspace_preimage_mismatch() {
        let fx = fixture(ViewAccess::ReadWrite);
        let source = backend(&fx);
        let err = apply_patch(
            source.view(),
            &patch(vec![PatchOp::delete_file(
                repo("src/lib.rs"),
                ArtifactId::from_bytes(b"not-hello"),
            )]),
            source,
            &cancel(),
        )
        .expect_err("hash");
        assert!(
            matches!(err, ApplyError::PreimageMismatch { ref path } if path.as_str() == "src/lib.rs")
        );
        assert_eq!(err.code(), Some(ErrorCode::WorkspacePreimageMismatch));
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
    }
}
