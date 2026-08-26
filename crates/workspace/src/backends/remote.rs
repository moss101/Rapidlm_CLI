//! Remote snapshot workspace backend.
//!
//! The snapshot is content-addressed (`application/vnd.rapidlm.snapshot`).
//! Workspace never dials a worker: blobs come from an injected
//! [`SnapshotBlobStore`]. Missing/corrupt blobs fail closed. Writes land in a
//! local overlay and never mutate snapshot bytes.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use protocol::{ArtifactId, RepoPath};

use crate::view::{CancellationToken, WorkspaceBackend, WorkspaceState, WorkspaceView};

/// Wire media type for a workspace snapshot artifact.
pub const SNAPSHOT_MEDIA_TYPE: &str = "application/vnd.rapidlm.snapshot";

/// Maximum files in one snapshot manifest.
pub const MAX_SNAPSHOT_FILES: usize = 4096;

/// Maximum bytes accepted for one snapshot file.
pub const MAX_SNAPSHOT_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum overlay slots over a snapshot.
pub const MAX_REMOTE_OVERLAY_SLOTS: usize = 4096;

/// Injected blob source. Implementations must not return host paths.
pub trait SnapshotBlobStore {
    fn get(
        &self,
        id: ArtifactId,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, RemoteSnapshotError>;
}

/// Content-addressed file list that identifies one snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotManifest {
    id: ArtifactId,
    files: BTreeMap<RepoPath, ArtifactId>,
}

/// Remote snapshot view: immutable snapshot + local overlay.
pub struct RemoteSnapshotBackend {
    view: WorkspaceView,
    snapshot_id: ArtifactId,
    files: BTreeMap<String, ArtifactId>,
    store: Box<dyn SnapshotBlobStore + Send + Sync>,
    max_file_bytes: usize,
    max_slots: usize,
    inner: Mutex<Inner>,
}

/// Typed remote-snapshot failure. Display never echoes paths or blob bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteSnapshotError {
    Cancelled,
    WrongBackend,
    ReadOnlyView,
    InvalidState,
    NotFound,
    OutOfScope,
    GitScopeRequired,
    PreexistingChange,
    PreimageMismatch,
    BoundExceeded,
    SlotLimit,
    Unavailable,
    Corrupt,
    SnapshotMismatch,
    InvalidManifest,
    LockPoisoned,
}

struct Inner {
    slots: BTreeMap<String, OverlaySlot>,
}

#[derive(Clone)]
enum OverlaySlot {
    Present { hash: ArtifactId, bytes: Vec<u8> },
    Deleted,
}

impl SnapshotManifest {
    pub fn new(files: BTreeMap<RepoPath, ArtifactId>) -> Result<Self, RemoteSnapshotError> {
        if files.len() > MAX_SNAPSHOT_FILES {
            return Err(RemoteSnapshotError::BoundExceeded);
        }
        for path in files.keys() {
            reject_git(path)?;
        }
        let id = snapshot_id(&files);
        Ok(Self { id, files })
    }

    pub fn id(&self) -> ArtifactId {
        self.id
    }

    pub fn files(&self) -> &BTreeMap<RepoPath, ArtifactId> {
        &self.files
    }
}

impl RemoteSnapshotBackend {
    pub fn open(
        view: WorkspaceView,
        manifest: SnapshotManifest,
        store: Box<dyn SnapshotBlobStore + Send + Sync>,
        cancel: &CancellationToken,
    ) -> Result<Self, RemoteSnapshotError> {
        Self::open_with(
            view,
            manifest,
            store,
            MAX_SNAPSHOT_FILE_BYTES,
            MAX_REMOTE_OVERLAY_SLOTS,
            cancel,
        )
    }

    pub fn open_with(
        view: WorkspaceView,
        manifest: SnapshotManifest,
        store: Box<dyn SnapshotBlobStore + Send + Sync>,
        max_file_bytes: usize,
        max_slots: usize,
        cancel: &CancellationToken,
    ) -> Result<Self, RemoteSnapshotError> {
        cancel.check().map_err(|_| RemoteSnapshotError::Cancelled)?;
        if view.backend() != WorkspaceBackend::Remote {
            return Err(RemoteSnapshotError::WrongBackend);
        }
        if max_file_bytes == 0 || max_slots == 0 {
            return Err(RemoteSnapshotError::BoundExceeded);
        }
        let expected = snapshot_id(&manifest.files);
        if expected != manifest.id {
            return Err(RemoteSnapshotError::SnapshotMismatch);
        }
        let mut files = BTreeMap::new();
        for (path, blob) in manifest.files {
            reject_git(&path)?;
            files.insert(path.as_str().to_owned(), blob);
        }
        Ok(Self {
            view,
            snapshot_id: expected,
            files,
            store,
            max_file_bytes,
            max_slots,
            inner: Mutex::new(Inner {
                slots: BTreeMap::new(),
            }),
        })
    }

    pub fn view(&self) -> &WorkspaceView {
        &self.view
    }

    pub fn snapshot_id(&self) -> ArtifactId {
        self.snapshot_id
    }

    pub fn read(
        &self,
        path: &RepoPath,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, RemoteSnapshotError> {
        cancel.check().map_err(|_| RemoteSnapshotError::Cancelled)?;
        self.ensure_in_scope(path)?;
        reject_git(path)?;
        let inner = self.lock()?;
        match inner.slots.get(path.as_str()) {
            Some(OverlaySlot::Present { bytes, .. }) => Ok(bytes.clone()),
            Some(OverlaySlot::Deleted) => Err(RemoteSnapshotError::NotFound),
            None => self.read_snapshot(path, cancel),
        }
    }

    pub fn write(
        &self,
        path: &RepoPath,
        bytes: &[u8],
        expected: Option<&ArtifactId>,
        cancel: &CancellationToken,
    ) -> Result<(), RemoteSnapshotError> {
        cancel.check().map_err(|_| RemoteSnapshotError::Cancelled)?;
        self.ensure_writable()?;
        self.ensure_in_scope(path)?;
        reject_git(path)?;
        if bytes.len() > self.max_file_bytes {
            return Err(RemoteSnapshotError::BoundExceeded);
        }
        let new_hash = ArtifactId::from_bytes(bytes);
        let visible = self.visible(path, cancel)?;
        match (visible, expected) {
            (Some(current), _) if current == new_hash => return Ok(()),
            (Some(_), None) => return Err(RemoteSnapshotError::PreexistingChange),
            (Some(current), Some(exp)) if current != *exp => {
                return Err(RemoteSnapshotError::PreimageMismatch);
            }
            (None, Some(_)) => return Err(RemoteSnapshotError::PreimageMismatch),
            (None, None) | (Some(_), Some(_)) => {}
        }
        let mut inner = self.lock()?;
        if !inner.slots.contains_key(path.as_str()) && inner.slots.len() >= self.max_slots {
            return Err(RemoteSnapshotError::SlotLimit);
        }
        inner.slots.insert(
            path.as_str().to_owned(),
            OverlaySlot::Present {
                hash: new_hash,
                bytes: bytes.to_vec(),
            },
        );
        Ok(())
    }

    pub fn delete(
        &self,
        path: &RepoPath,
        expected: Option<&ArtifactId>,
        cancel: &CancellationToken,
    ) -> Result<(), RemoteSnapshotError> {
        cancel.check().map_err(|_| RemoteSnapshotError::Cancelled)?;
        self.ensure_writable()?;
        self.ensure_in_scope(path)?;
        reject_git(path)?;
        let visible = self
            .visible(path, cancel)?
            .ok_or(RemoteSnapshotError::NotFound)?;
        match expected {
            None => return Err(RemoteSnapshotError::PreexistingChange),
            Some(exp) if visible != *exp => return Err(RemoteSnapshotError::PreimageMismatch),
            Some(_) => {}
        }
        let mut inner = self.lock()?;
        if !inner.slots.contains_key(path.as_str()) && inner.slots.len() >= self.max_slots {
            return Err(RemoteSnapshotError::SlotLimit);
        }
        inner
            .slots
            .insert(path.as_str().to_owned(), OverlaySlot::Deleted);
        Ok(())
    }

    fn visible(
        &self,
        path: &RepoPath,
        cancel: &CancellationToken,
    ) -> Result<Option<ArtifactId>, RemoteSnapshotError> {
        let inner = self.lock()?;
        match inner.slots.get(path.as_str()) {
            Some(OverlaySlot::Present { hash, .. }) => Ok(Some(*hash)),
            Some(OverlaySlot::Deleted) => Ok(None),
            None => match self.read_snapshot(path, cancel) {
                Ok(bytes) => Ok(Some(ArtifactId::from_bytes(&bytes))),
                Err(RemoteSnapshotError::NotFound) => Ok(None),
                Err(err) => Err(err),
            },
        }
    }

    fn read_snapshot(
        &self,
        path: &RepoPath,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, RemoteSnapshotError> {
        let Some(blob_id) = self.files.get(path.as_str()).copied() else {
            return Err(RemoteSnapshotError::NotFound);
        };
        let bytes = self.store.get(blob_id, cancel)?;
        if bytes.len() > self.max_file_bytes {
            return Err(RemoteSnapshotError::BoundExceeded);
        }
        if ArtifactId::from_bytes(&bytes) != blob_id {
            return Err(RemoteSnapshotError::Corrupt);
        }
        Ok(bytes)
    }

    fn ensure_writable(&self) -> Result<(), RemoteSnapshotError> {
        if !self.view.access().is_writable() {
            return Err(RemoteSnapshotError::ReadOnlyView);
        }
        if self.view.state() != WorkspaceState::Active {
            return Err(RemoteSnapshotError::InvalidState);
        }
        Ok(())
    }

    fn ensure_in_scope(&self, path: &RepoPath) -> Result<(), RemoteSnapshotError> {
        if self.view.scope().contains(path) {
            Ok(())
        } else {
            Err(RemoteSnapshotError::OutOfScope)
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, RemoteSnapshotError> {
        self.inner
            .lock()
            .map_err(|_| RemoteSnapshotError::LockPoisoned)
    }
}

impl fmt::Debug for RemoteSnapshotBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteSnapshotBackend")
            .field("view_id", &self.view.id())
            .field("snapshot_id", &self.snapshot_id)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for RemoteSnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "remote snapshot operation cancelled",
            Self::WrongBackend => "workspace view is not a remote snapshot",
            Self::ReadOnlyView => "remote snapshot view is read-only",
            Self::InvalidState => "remote snapshot view is not writable in this state",
            Self::NotFound => "remote snapshot path not found",
            Self::OutOfScope => "path is outside the workspace view scope",
            Self::GitScopeRequired => "mutating .git requires a dedicated git capability",
            Self::PreexistingChange => "snapshot file would be overwritten without a preimage",
            Self::PreimageMismatch => "remote snapshot preimage does not match visible bytes",
            Self::BoundExceeded => "remote snapshot resource bound exceeded",
            Self::SlotLimit => "remote snapshot overlay slot limit reached",
            Self::Unavailable => "remote snapshot blob is unavailable",
            Self::Corrupt => "remote snapshot blob digest mismatch",
            Self::SnapshotMismatch => "remote snapshot identity does not match the file list",
            Self::InvalidManifest => "remote snapshot manifest is invalid",
            Self::LockPoisoned => "remote snapshot lock poisoned",
        })
    }
}

impl Error for RemoteSnapshotError {}

fn snapshot_id(files: &BTreeMap<RepoPath, ArtifactId>) -> ArtifactId {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(b"rapidlm.remote_snapshot.v1\0");
    for (path, blob) in files {
        let path_bytes = path.as_str().as_bytes();
        encoded.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
        encoded.extend_from_slice(path_bytes);
        encoded.extend_from_slice(blob.as_digest());
    }
    ArtifactId::from_bytes(&encoded)
}

fn reject_git(path: &RepoPath) -> Result<(), RemoteSnapshotError> {
    if path.as_str().split('/').any(|part| part == ".git") {
        Err(RemoteSnapshotError::GitScopeRequired)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{CreateView, ViewAccess, ViewRegistry, ViewScope};
    use protocol::{AgentId, RepoId};
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct MemoryStore {
        blobs: HashMap<ArtifactId, Vec<u8>>,
        fail: Mutex<bool>,
    }

    impl MemoryStore {
        fn new(files: &[(&str, &[u8])]) -> (SnapshotManifest, Self) {
            let mut blobs = HashMap::new();
            let mut manifest_files = BTreeMap::new();
            for (path, bytes) in files {
                let id = ArtifactId::from_bytes(bytes);
                blobs.insert(id, bytes.to_vec());
                manifest_files.insert(RepoPath::parse(path).expect("path"), id);
            }
            (
                SnapshotManifest::new(manifest_files).expect("manifest"),
                Self {
                    blobs,
                    fail: Mutex::new(false),
                },
            )
        }

        fn fail_next(&self) {
            *self.fail.lock().expect("lock") = true;
        }
    }

    impl SnapshotBlobStore for MemoryStore {
        fn get(
            &self,
            id: ArtifactId,
            cancel: &CancellationToken,
        ) -> Result<Vec<u8>, RemoteSnapshotError> {
            if cancel.is_cancelled() {
                return Err(RemoteSnapshotError::Cancelled);
            }
            if *self.fail.lock().expect("lock") {
                return Err(RemoteSnapshotError::Unavailable);
            }
            self.blobs
                .get(&id)
                .cloned()
                .ok_or(RemoteSnapshotError::Unavailable)
        }
    }

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn repo(path: &str) -> RepoPath {
        RepoPath::parse(path).expect("path")
    }

    fn remote_view(access: ViewAccess, scope: ViewScope) -> WorkspaceView {
        let registry = ViewRegistry::new();
        let mut spec = CreateView::new(RepoId::new(), WorkspaceBackend::Remote, "snap-rev", access)
            .with_scope(scope);
        if access.is_writable() {
            spec = spec.with_write_owner(AgentId::new());
        }
        registry.create(spec, &cancel()).expect("view")
    }

    fn open(
        access: ViewAccess,
        files: &[(&str, &[u8])],
    ) -> (RemoteSnapshotBackend, ArtifactId, MemoryStore) {
        let (manifest, store) = MemoryStore::new(files);
        let id = manifest.id();
        let backend = RemoteSnapshotBackend::open(
            remote_view(access, ViewScope::repo()),
            manifest,
            Box::new(MemoryStore {
                blobs: store.blobs.clone(),
                fail: Mutex::new(false),
            }),
            &cancel(),
        )
        .expect("open");
        (backend, id, store)
    }

    #[test]
    fn snapshot_read_is_content_addressed() {
        let (backend, id, _) = open(ViewAccess::ReadWrite, &[("src/lib.rs", b"fn main() {}\n")]);
        assert_eq!(backend.snapshot_id(), id);
        assert_eq!(SNAPSHOT_MEDIA_TYPE, "application/vnd.rapidlm.snapshot");
        assert_eq!(
            backend.read(&repo("src/lib.rs"), &cancel()).expect("read"),
            b"fn main() {}\n"
        );
    }

    #[test]
    fn overlay_write_does_not_mutate_snapshot_blob() {
        let (backend, _, store) = open(ViewAccess::ReadWrite, &[("src/lib.rs", b"fn main() {}\n")]);
        let expected = ArtifactId::from_bytes(b"fn main() {}\n");
        backend
            .write(
                &repo("src/lib.rs"),
                b"changed\n",
                Some(&expected),
                &cancel(),
            )
            .expect("write");
        assert_eq!(
            backend
                .read(&repo("src/lib.rs"), &cancel())
                .expect("overlay"),
            b"changed\n"
        );
        let original = store.get(expected, &cancel()).expect("snapshot blob");
        assert_eq!(original, b"fn main() {}\n");
    }

    #[test]
    fn missing_blob_is_unavailable_not_empty() {
        let mut files = BTreeMap::new();
        files.insert(repo("src/missing.rs"), ArtifactId::from_bytes(b"absent"));
        let manifest = SnapshotManifest::new(files).expect("manifest");
        let backend = RemoteSnapshotBackend::open(
            remote_view(ViewAccess::ReadWrite, ViewScope::repo()),
            manifest,
            Box::new(MemoryStore {
                blobs: HashMap::new(),
                fail: Mutex::new(false),
            }),
            &cancel(),
        )
        .expect("open");
        assert_eq!(
            backend
                .read(&repo("src/missing.rs"), &cancel())
                .expect_err("missing"),
            RemoteSnapshotError::Unavailable
        );
    }

    #[test]
    fn corrupt_blob_digest_fails_closed() {
        let blob_id = ArtifactId::from_bytes(b"expected");
        let mut files = BTreeMap::new();
        files.insert(repo("src/lib.rs"), blob_id);
        let manifest = SnapshotManifest::new(files).expect("manifest");
        let mut blobs = HashMap::new();
        blobs.insert(blob_id, b"tampered".to_vec());
        let backend = RemoteSnapshotBackend::open(
            remote_view(ViewAccess::ReadWrite, ViewScope::repo()),
            manifest,
            Box::new(MemoryStore {
                blobs,
                fail: Mutex::new(false),
            }),
            &cancel(),
        )
        .expect("open");
        assert_eq!(
            backend
                .read(&repo("src/lib.rs"), &cancel())
                .expect_err("corrupt"),
            RemoteSnapshotError::Corrupt
        );
    }

    #[test]
    fn snapshot_identity_is_stable_and_order_independent() {
        let a = MemoryStore::new(&[("b.rs", b"b"), ("a.rs", b"a")]).0;
        let b = MemoryStore::new(&[("a.rs", b"a"), ("b.rs", b"b")]).0;
        assert_eq!(a.id(), b.id());
        let c = MemoryStore::new(&[("a.rs", b"A"), ("b.rs", b"b")]).0;
        assert_ne!(a.id(), c.id());
    }

    #[test]
    fn unavailable_store_degrades_without_inventing_bytes() {
        let (manifest, store) = MemoryStore::new(&[("src/lib.rs", b"fn x() {}\n")]);
        store.fail_next();
        let backend = RemoteSnapshotBackend::open(
            remote_view(ViewAccess::ReadWrite, ViewScope::repo()),
            manifest,
            Box::new(store),
            &cancel(),
        )
        .expect("open");
        assert_eq!(
            backend
                .read(&repo("src/lib.rs"), &cancel())
                .expect_err("down"),
            RemoteSnapshotError::Unavailable
        );
    }

    #[test]
    fn view_scope_and_git_fail_closed() {
        let (manifest, store) =
            MemoryStore::new(&[("src/lib.rs", b"src\n"), ("docs/readme.md", b"docs\n")]);
        let scope = ViewScope::prefixes(vec![repo("src")]).expect("scope");
        let backend = RemoteSnapshotBackend::open(
            remote_view(ViewAccess::ReadWrite, scope),
            manifest,
            Box::new(store),
            &cancel(),
        )
        .expect("open");
        assert_eq!(
            backend
                .read(&repo("docs/readme.md"), &cancel())
                .expect_err("scope"),
            RemoteSnapshotError::OutOfScope
        );
        assert_eq!(
            backend.read(&repo("src/lib.rs"), &cancel()).expect("src"),
            b"src\n"
        );

        let (wide, _, _) = open(ViewAccess::ReadWrite, &[("src/lib.rs", b"src\n")]);
        assert_eq!(
            wide.write(&repo(".git/config"), b"x\n", None, &cancel())
                .expect_err("git"),
            RemoteSnapshotError::GitScopeRequired
        );
    }

    #[test]
    fn wrong_backend_and_read_only_fail_closed() {
        let registry = ViewRegistry::new();
        let overlay = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::Overlay,
                    "rev",
                    ViewAccess::ReadWrite,
                ),
                &cancel(),
            )
            .expect("overlay");
        let (manifest, store) = MemoryStore::new(&[("src/lib.rs", b"x\n")]);
        assert_eq!(
            RemoteSnapshotBackend::open(overlay, manifest, Box::new(store), &cancel())
                .expect_err("backend"),
            RemoteSnapshotError::WrongBackend
        );

        let (backend, _, _) = open(ViewAccess::ReadOnly, &[("src/lib.rs", b"x\n")]);
        assert_eq!(
            backend
                .write(&repo("src/new.rs"), b"y\n", None, &cancel())
                .expect_err("ro"),
            RemoteSnapshotError::ReadOnlyView
        );
        assert_eq!(
            backend.read(&repo("src/lib.rs"), &cancel()).expect("read"),
            b"x\n"
        );
    }

    #[test]
    fn cancelled_remote_fails_closed() {
        let (backend, _, _) = open(ViewAccess::ReadWrite, &[("src/lib.rs", b"x\n")]);
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            backend.read(&repo("src/lib.rs"), &token),
            Err(RemoteSnapshotError::Cancelled)
        );
    }

    #[test]
    fn error_display_is_safe() {
        for err in [
            RemoteSnapshotError::Unavailable,
            RemoteSnapshotError::Corrupt,
            RemoteSnapshotError::OutOfScope,
            RemoteSnapshotError::GitScopeRequired,
        ] {
            let text = err.to_string();
            for leaked in ["password", "/etc/passwd", "src/lib.rs", "sha256:"] {
                assert!(!text.contains(leaked), "{text}");
            }
        }
    }
}
