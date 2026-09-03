//! Isolated overlay/sandbox view over a read-only base checkout.
//!
//! Writes never mutate the parent tree. Reads prefer overlay slots, then the
//! confined base. View scope is enforced on every path.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use protocol::{ArtifactId, RepoPath};

use crate::backends::direct::{
    DirectError, DirectResolveMode, MAX_DIRECT_FILE_BYTES, canonicalize_root, read_confined,
    resolve_under_root,
};
use crate::view::{CancellationToken, WorkspaceBackend, WorkspaceState, WorkspaceView};

/// Maximum bytes accepted for one overlay file.
pub const MAX_OVERLAY_FILE_BYTES: usize = MAX_DIRECT_FILE_BYTES;

/// Maximum present or deleted overlay slots.
pub const MAX_OVERLAY_SLOTS: usize = 4096;

/// Open flags for an overlay/sandbox view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayOptions {
    max_file_bytes: usize,
    max_slots: usize,
}

/// Copy-on-write overlay bound to one [`WorkspaceView`].
pub struct OverlayBackend {
    base: PathBuf,
    view: WorkspaceView,
    max_file_bytes: usize,
    max_slots: usize,
    inner: Mutex<Inner>,
}

/// Typed overlay failure. Display never echoes paths or file bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayError {
    Cancelled,
    WrongBackend,
    ReadOnlyView,
    InvalidState,
    InvalidRoot,
    PathEscape,
    NotFound,
    OutOfScope,
    GitScopeRequired,
    PreexistingChange,
    PreimageMismatch,
    BoundExceeded,
    SlotLimit,
    Io,
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

impl OverlayOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_file_bytes(mut self, max_file_bytes: usize) -> Self {
        self.max_file_bytes = max_file_bytes;
        self
    }

    pub fn with_max_slots(mut self, max_slots: usize) -> Self {
        self.max_slots = max_slots;
        self
    }
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_OVERLAY_FILE_BYTES,
            max_slots: MAX_OVERLAY_SLOTS,
        }
    }
}

impl OverlayBackend {
    pub fn open(
        base: impl AsRef<Path>,
        view: WorkspaceView,
        cancel: &CancellationToken,
    ) -> Result<Self, OverlayError> {
        Self::open_with(base, view, OverlayOptions::default(), cancel)
    }

    pub fn open_with(
        base: impl AsRef<Path>,
        view: WorkspaceView,
        options: OverlayOptions,
        cancel: &CancellationToken,
    ) -> Result<Self, OverlayError> {
        cancel.check().map_err(|_| OverlayError::Cancelled)?;
        if view.backend() != WorkspaceBackend::Overlay {
            return Err(OverlayError::WrongBackend);
        }
        if options.max_file_bytes == 0 || options.max_slots == 0 {
            return Err(OverlayError::BoundExceeded);
        }
        let canonical = canonicalize_root(base.as_ref()).map_err(map_direct)?;
        Ok(Self {
            base: canonical,
            view,
            max_file_bytes: options.max_file_bytes,
            max_slots: options.max_slots,
            inner: Mutex::new(Inner {
                slots: BTreeMap::new(),
            }),
        })
    }

    pub fn view(&self) -> &WorkspaceView {
        &self.view
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn read(
        &self,
        path: &RepoPath,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, OverlayError> {
        cancel.check().map_err(|_| OverlayError::Cancelled)?;
        self.ensure_in_scope(path)?;
        reject_git(path)?;
        let inner = self.lock()?;
        match inner.slots.get(path.as_str()) {
            Some(OverlaySlot::Present { bytes, .. }) => Ok(bytes.clone()),
            Some(OverlaySlot::Deleted) => Err(OverlayError::NotFound),
            None => self.read_base(path, cancel),
        }
    }

    /// Overlay write. Never mutates the parent checkout.
    pub fn write(
        &self,
        path: &RepoPath,
        bytes: &[u8],
        expected: Option<&ArtifactId>,
        cancel: &CancellationToken,
    ) -> Result<(), OverlayError> {
        cancel.check().map_err(|_| OverlayError::Cancelled)?;
        self.ensure_writable()?;
        self.ensure_in_scope(path)?;
        reject_git(path)?;
        if bytes.len() > self.max_file_bytes {
            return Err(OverlayError::BoundExceeded);
        }
        let new_hash = ArtifactId::from_bytes(bytes);
        // Held for the whole decide-then-mutate sequence: releasing it
        // between the preimage check and the insert let two concurrent
        // writers both pass the check against the same stale snapshot, the
        // second silently overwriting the first with no error — exactly
        // the lost-update `expected`/`PreimageMismatch` exists to prevent.
        let mut inner = self.lock()?;
        let visible = self.visible_locked(&inner, path, cancel)?;
        match (visible.as_ref(), expected) {
            (Some(current), _) if current.hash == new_hash => return Ok(()),
            (Some(_current), None) => return Err(OverlayError::PreexistingChange),
            (Some(current), Some(exp)) if current.hash != *exp => {
                return Err(OverlayError::PreimageMismatch);
            }
            (None, Some(_)) => return Err(OverlayError::PreimageMismatch),
            (None, None) | (Some(_), Some(_)) => {}
        }
        if !inner.slots.contains_key(path.as_str()) && inner.slots.len() >= self.max_slots {
            return Err(OverlayError::SlotLimit);
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
    ) -> Result<(), OverlayError> {
        cancel.check().map_err(|_| OverlayError::Cancelled)?;
        self.ensure_writable()?;
        self.ensure_in_scope(path)?;
        reject_git(path)?;
        let mut inner = self.lock()?;
        let visible = self
            .visible_locked(&inner, path, cancel)?
            .ok_or(OverlayError::NotFound)?;
        match expected {
            None => return Err(OverlayError::PreexistingChange),
            Some(exp) if visible.hash != *exp => return Err(OverlayError::PreimageMismatch),
            Some(_) => {}
        }
        if !inner.slots.contains_key(path.as_str()) && inner.slots.len() >= self.max_slots {
            return Err(OverlayError::SlotLimit);
        }
        inner
            .slots
            .insert(path.as_str().to_owned(), OverlaySlot::Deleted);
        Ok(())
    }

    /// Looks up a path's visible blob against an already-held guard so a
    /// caller can decide-then-mutate under one continuous lock instead of a
    /// stale snapshot from a separately acquired-and-released lock.
    fn visible_locked(
        &self,
        inner: &Inner,
        path: &RepoPath,
        cancel: &CancellationToken,
    ) -> Result<Option<VisibleBlob>, OverlayError> {
        match inner.slots.get(path.as_str()) {
            Some(OverlaySlot::Present { hash, .. }) => Ok(Some(VisibleBlob { hash: *hash })),
            Some(OverlaySlot::Deleted) => Ok(None),
            None => match self.read_base(path, cancel) {
                Ok(bytes) => Ok(Some(VisibleBlob {
                    hash: ArtifactId::from_bytes(&bytes),
                })),
                Err(OverlayError::NotFound) => Ok(None),
                Err(err) => Err(err),
            },
        }
    }

    fn read_base(
        &self,
        path: &RepoPath,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, OverlayError> {
        let resolved = resolve_under_root(&self.base, path, DirectResolveMode::Read, cancel)
            .map_err(map_direct)?;
        read_confined(&self.base, resolved.host(), self.max_file_bytes).map_err(map_direct)
    }

    fn ensure_writable(&self) -> Result<(), OverlayError> {
        if !self.view.access().is_writable() {
            return Err(OverlayError::ReadOnlyView);
        }
        if self.view.state() != WorkspaceState::Active {
            return Err(OverlayError::InvalidState);
        }
        Ok(())
    }

    fn ensure_in_scope(&self, path: &RepoPath) -> Result<(), OverlayError> {
        if self.view.scope().contains(path) {
            Ok(())
        } else {
            Err(OverlayError::OutOfScope)
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, OverlayError> {
        self.inner.lock().map_err(|_| OverlayError::LockPoisoned)
    }
}

struct VisibleBlob {
    hash: ArtifactId,
}

impl fmt::Debug for OverlayBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OverlayBackend")
            .field("base", &self.base)
            .field("view_id", &self.view.id())
            .finish()
    }
}

impl fmt::Display for OverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "overlay workspace operation cancelled",
            Self::WrongBackend => "workspace view is not an overlay",
            Self::ReadOnlyView => "overlay view is read-only",
            Self::InvalidState => "overlay view is not writable in this state",
            Self::InvalidRoot => "overlay base checkout is invalid",
            Self::PathEscape => "resolved path escapes the overlay base",
            Self::NotFound => "overlay path not found",
            Self::OutOfScope => "path is outside the workspace view scope",
            Self::GitScopeRequired => "mutating .git requires a dedicated git capability",
            Self::PreexistingChange => "pre-existing base file would be overwritten",
            Self::PreimageMismatch => "overlay preimage does not match visible bytes",
            Self::BoundExceeded => "overlay resource bound exceeded",
            Self::SlotLimit => "overlay slot limit reached",
            Self::Io => "overlay I/O failed",
            Self::LockPoisoned => "overlay lock poisoned",
        })
    }
}

impl Error for OverlayError {}

fn reject_git(path: &RepoPath) -> Result<(), OverlayError> {
    if path.as_str().split('/').any(|part| part == ".git") {
        Err(OverlayError::GitScopeRequired)
    } else {
        Ok(())
    }
}

fn map_direct(err: DirectError) -> OverlayError {
    match err {
        DirectError::Cancelled => OverlayError::Cancelled,
        DirectError::InvalidRoot => OverlayError::InvalidRoot,
        DirectError::PathEscape => OverlayError::PathEscape,
        DirectError::NotFound | DirectError::UnresolvedPath => OverlayError::NotFound,
        DirectError::GitScopeRequired => OverlayError::GitScopeRequired,
        DirectError::BoundExceeded => OverlayError::BoundExceeded,
        DirectError::Io => OverlayError::Io,
        DirectError::LockPoisoned => OverlayError::LockPoisoned,
        _ => OverlayError::Io,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{CreateView, ViewAccess, ViewRegistry, ViewScope};
    use protocol::{AgentId, RepoId};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    struct Fixture {
        dir: PathBuf,
        backend: OverlayBackend,
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

    fn overlay_view(access: ViewAccess, scope: ViewScope) -> WorkspaceView {
        let registry = ViewRegistry::new();
        let mut spec =
            CreateView::new(RepoId::new(), WorkspaceBackend::Overlay, "base-rev", access)
                .with_scope(scope);
        if access.is_writable() {
            spec = spec.with_write_owner(AgentId::new());
        }
        registry.create(spec, &cancel()).expect("create view")
    }

    fn fixture() -> Fixture {
        fixture_scoped(ViewAccess::ReadWrite, ViewScope::repo())
    }

    fn fixture_scoped(access: ViewAccess, scope: ViewScope) -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-overlay-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("src")).expect("mkdir");
        fs::create_dir_all(dir.join("docs")).expect("mkdir docs");
        fs::write(dir.join("src/lib.rs"), b"fn main() {}\n").expect("seed");
        fs::write(dir.join("docs/readme.md"), b"# docs\n").expect("docs");
        let backend = OverlayBackend::open(&dir, overlay_view(access, scope), &cancel())
            .expect("open overlay");
        Fixture { dir, backend }
    }

    #[test]
    fn overlay_write_does_not_mutate_parent_checkout() {
        let fx = fixture();
        let path = repo("src/lib.rs");
        let expected = ArtifactId::from_bytes(b"fn main() {}\n");
        fx.backend
            .write(&path, b"overlayed\n", Some(&expected), &cancel())
            .expect("write");
        assert_eq!(
            fx.backend.read(&path, &cancel()).expect("read"),
            b"overlayed\n"
        );
        assert_eq!(
            fs::read(fx.dir.join("src/lib.rs")).expect("base"),
            b"fn main() {}\n"
        );
    }

    #[test]
    fn concurrent_writes_to_a_new_path_never_both_report_success() {
        // Writers racing to create the same not-yet-existing path with
        // `expected: None` must not both succeed: exactly one wins, and
        // every loser must see `PreexistingChange` once it observes the
        // winner's write — never silently overwrite it while still
        // reporting `Ok`. A synchronized start with many contenders (rather
        // than two threads left to real scheduling) is needed to reliably
        // land inside the race window, which spans only a few in-memory
        // operations.
        const CONTENDERS: usize = 16;
        for _ in 0..50 {
            let fx = fixture();
            let path = repo("src/new.rs");
            let barrier = std::sync::Barrier::new(CONTENDERS);
            let results: Vec<Result<(), OverlayError>> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..CONTENDERS)
                    .map(|i| {
                        let barrier = &barrier;
                        let backend = &fx.backend;
                        let path = &path;
                        scope.spawn(move || {
                            let bytes = format!("from-{i}");
                            barrier.wait();
                            backend.write(path, bytes.as_bytes(), None, &cancel())
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().expect("thread")).collect()
            });
            let successes = results.iter().filter(|r| r.is_ok()).count();
            assert_eq!(
                successes, 1,
                "exactly one concurrent write to a brand-new path must win \
                 (got {results:?}) — more than one silently overwriting the \
                 winner instead of seeing PreexistingChange is the bug"
            );
        }
    }

    #[test]
    fn read_falls_through_to_base_until_overlaid() {
        let fx = fixture();
        assert_eq!(
            fx.backend
                .read(&repo("src/lib.rs"), &cancel())
                .expect("base read"),
            b"fn main() {}\n"
        );
        fx.backend
            .write(&repo("src/new.rs"), b"created\n", None, &cancel())
            .expect("create");
        assert_eq!(
            fx.backend
                .read(&repo("src/new.rs"), &cancel())
                .expect("overlay read"),
            b"created\n"
        );
        assert!(!fx.dir.join("src/new.rs").exists());
    }

    #[test]
    fn delete_hides_base_file_without_removing_it() {
        let fx = fixture();
        let path = repo("src/lib.rs");
        let expected = ArtifactId::from_bytes(b"fn main() {}\n");
        fx.backend
            .delete(&path, Some(&expected), &cancel())
            .expect("delete");
        assert_eq!(
            fx.backend.read(&path, &cancel()).expect_err("hidden"),
            OverlayError::NotFound
        );
        assert_eq!(
            fs::read(fx.dir.join("src/lib.rs")).expect("base remains"),
            b"fn main() {}\n"
        );
    }

    #[test]
    fn unacked_base_file_is_not_overwritten() {
        let fx = fixture();
        let err = fx
            .backend
            .write(&repo("src/lib.rs"), b"stolen\n", None, &cancel())
            .expect_err("silent");
        assert_eq!(err, OverlayError::PreexistingChange);
        assert_eq!(
            fs::read(fx.dir.join("src/lib.rs")).expect("base"),
            b"fn main() {}\n"
        );
    }

    #[test]
    fn view_scope_and_git_paths_fail_closed() {
        let scope = ViewScope::prefixes(vec![repo("src")]).expect("scope");
        let fx = fixture_scoped(ViewAccess::ReadWrite, scope);
        assert_eq!(
            fx.backend
                .read(&repo("docs/readme.md"), &cancel())
                .expect_err("docs"),
            OverlayError::OutOfScope
        );
        fx.backend
            .write(&repo("src/ok.rs"), b"ok\n", None, &cancel())
            .expect("in-scope");
        assert!(!fx.dir.join("src/ok.rs").exists());

        let wide = fixture();
        assert_eq!(
            wide.backend
                .write(&repo(".git/config"), b"x\n", None, &cancel())
                .expect_err("git"),
            OverlayError::GitScopeRequired
        );
        assert!(!wide.dir.join(".git/config").exists());
    }

    #[test]
    fn wrong_backend_and_read_only_fail_closed() {
        let registry = ViewRegistry::new();
        let direct = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::Direct,
                    "rev",
                    ViewAccess::ReadWrite,
                ),
                &cancel(),
            )
            .expect("direct");
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-overlay-wrong-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        assert_eq!(
            OverlayBackend::open(&dir, direct, &cancel()).expect_err("backend"),
            OverlayError::WrongBackend
        );
        let _ = fs::remove_dir_all(&dir);

        let ro = fixture_scoped(ViewAccess::ReadOnly, ViewScope::repo());
        assert_eq!(
            ro.backend
                .write(&repo("src/x.rs"), b"x\n", None, &cancel())
                .expect_err("ro"),
            OverlayError::ReadOnlyView
        );
        assert_eq!(
            ro.backend
                .read(&repo("src/lib.rs"), &cancel())
                .expect("read"),
            b"fn main() {}\n"
        );
    }

    #[test]
    fn cancelled_overlay_fails_closed() {
        let fx = fixture();
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            fx.backend.read(&repo("src/lib.rs"), &token),
            Err(OverlayError::Cancelled)
        );
    }

    #[test]
    fn error_display_is_safe() {
        for err in [
            OverlayError::PathEscape,
            OverlayError::OutOfScope,
            OverlayError::PreimageMismatch,
            OverlayError::GitScopeRequired,
        ] {
            let text = err.to_string();
            for leaked in ["password", "/etc/passwd", "src/lib.rs", "secret"] {
                assert!(!text.contains(leaked), "{text}");
            }
        }
    }
}
