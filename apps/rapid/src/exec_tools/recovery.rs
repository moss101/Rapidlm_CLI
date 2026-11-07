//! Workspace-backed recovery adapter and ordinary-write journal gate.
//! The journal lock and host-shared path locks coordinate participating RapidLM
//! writers; an external editor can still race the final filesystem operation.
use super::*;
use workspace::multi_edit_journal::{JournalReadGuard, JournalStore};
use workspace::multi_edit_recover::{self, RecoverError, RecoveryIo, RecoveryResult};
use workspace::multi_edit_recovery::{FileImage, MAX_RECOVERY_BYTES, RecoveryRecord};

pub(super) const JOURNAL_PATH: &str = ".rapidlm/multi-edit-journal";
const SHADOW_MARKER: &str = "shadow-running";
const EXTERNAL_MARKER: &str = "external-command-running";

pub(super) fn ensure_no_shadow_marker(directory: &Path) -> Result<(), WorkspaceRecoveryError> {
    match fs::symlink_metadata(directory.join(SHADOW_MARKER)) {
        Ok(_) => Err(WorkspaceRecoveryError::ShadowUncertain),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(WorkspaceRecoveryError::Io),
    }
}

pub(super) fn ensure_no_external_marker(directory: &Path) -> Result<(), WorkspaceRecoveryError> {
    match fs::symlink_metadata(directory.join(EXTERNAL_MARKER)) {
        Ok(_) => Err(WorkspaceRecoveryError::ExternalUncertain),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(WorkspaceRecoveryError::Io),
    }
}

/// Persist a workspace-wide gate under the already-held exclusive journal
/// lock before launching an external hook or formatter.
#[cfg(unix)]
pub(super) fn ensure_external_can_start(
    journal: &JournalStore,
) -> Result<(), WorkspaceRecoveryError> {
    ensure_no_shadow_marker(journal.directory())?;
    ensure_no_external_marker(journal.directory())?;
    if journal
        .load()
        .map_err(|_| WorkspaceRecoveryError::Io)?
        .is_some()
    {
        return Err(WorkspaceRecoveryError::Pending);
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn begin_external(
    journal: &JournalStore,
    stage: &str,
) -> Result<String, WorkspaceRecoveryError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if !matches!(stage, "hook" | "formatter") {
        return Err(WorkspaceRecoveryError::Io);
    }
    ensure_external_can_start(journal)?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| WorkspaceRecoveryError::Io)?;
    let token = format!(
        "{stage}:{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(journal.directory().join(EXTERNAL_MARKER))
        .map_err(|_| WorkspaceRecoveryError::ExternalUncertain)?;
    file.write_all(token.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|_| WorkspaceRecoveryError::ExternalUncertain)?;
    fs::File::open(journal.directory())
        .and_then(|dir| dir.sync_all())
        .map_err(|_| WorkspaceRecoveryError::ExternalUncertain)?;
    Ok(token)
}

/// Remove only this invocation's gate, after its result or cancellation has
/// been durably checkpointed and its subprocess termination confirmed.
#[cfg(unix)]
pub(super) fn finish_external(
    journal: &JournalStore,
    token: &str,
) -> Result<(), WorkspaceRecoveryError> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let path = journal.directory().join(EXTERNAL_MARKER);
    let metadata =
        fs::symlink_metadata(&path).map_err(|_| WorkspaceRecoveryError::ExternalUncertain)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > 64
    {
        return Err(WorkspaceRecoveryError::ExternalUncertain);
    }
    let file = fs::File::open(&path).map_err(|_| WorkspaceRecoveryError::ExternalUncertain)?;
    let opened = file
        .metadata()
        .map_err(|_| WorkspaceRecoveryError::ExternalUncertain)?;
    if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
        return Err(WorkspaceRecoveryError::ExternalUncertain);
    }
    let mut contents = String::new();
    file.take(65)
        .read_to_string(&mut contents)
        .map_err(|_| WorkspaceRecoveryError::ExternalUncertain)?;
    if contents != token {
        return Err(WorkspaceRecoveryError::ExternalUncertain);
    }
    fs::remove_file(path).map_err(|_| WorkspaceRecoveryError::ExternalUncertain)?;
    fs::File::open(journal.directory())
        .and_then(|dir| dir.sync_all())
        .map_err(|_| WorkspaceRecoveryError::ExternalUncertain)
}

#[cfg(unix)]
pub(super) fn begin_shadow(root: &Path) -> Result<String, WorkspaceRecoveryError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let journal = lock_journal_for_publication(root)?;
    ensure_no_shadow_marker(journal.directory())?;
    ensure_no_external_marker(journal.directory())?;
    if journal
        .load()
        .map_err(|_| WorkspaceRecoveryError::Io)?
        .is_some()
    {
        return Err(WorkspaceRecoveryError::Pending);
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| WorkspaceRecoveryError::Io)?;
    let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(journal.directory().join(SHADOW_MARKER))
        .map_err(|_| WorkspaceRecoveryError::ShadowUncertain)?;
    file.write_all(token.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|_| WorkspaceRecoveryError::ShadowUncertain)?;
    fs::File::open(journal.directory())
        .and_then(|dir| dir.sync_all())
        .map_err(|_| WorkspaceRecoveryError::ShadowUncertain)?;
    Ok(token)
}

#[cfg(unix)]
pub(super) fn finish_shadow(root: &Path, token: &str) -> Result<(), WorkspaceRecoveryError> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let journal = lock_journal_for_publication(root)?;
    let path = journal.directory().join(SHADOW_MARKER);
    let metadata =
        fs::symlink_metadata(&path).map_err(|_| WorkspaceRecoveryError::ShadowUncertain)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() != 32
    {
        return Err(WorkspaceRecoveryError::ShadowUncertain);
    }
    let file = fs::File::open(&path).map_err(|_| WorkspaceRecoveryError::ShadowUncertain)?;
    let opened = file
        .metadata()
        .map_err(|_| WorkspaceRecoveryError::ShadowUncertain)?;
    if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
        return Err(WorkspaceRecoveryError::ShadowUncertain);
    }
    let mut contents = String::new();
    file.take(33)
        .read_to_string(&mut contents)
        .map_err(|_| WorkspaceRecoveryError::ShadowUncertain)?;
    if contents != token {
        return Err(WorkspaceRecoveryError::ShadowUncertain);
    }
    fs::remove_file(path).map_err(|_| WorkspaceRecoveryError::ShadowUncertain)?;
    fs::File::open(journal.directory())
        .and_then(|dir| dir.sync_all())
        .map_err(|_| WorkspaceRecoveryError::ShadowUncertain)
}

/// Inspect an existing journal without creating workspace state at startup.
pub(super) fn lock_clear_journal(
    root: &Path,
) -> Result<Option<JournalReadGuard>, WorkspaceRecoveryError> {
    let path = root.join(JOURNAL_PATH);
    match fs::symlink_metadata(&path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(WorkspaceRecoveryError::Io),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(WorkspaceRecoveryError::UnsafeFile);
        }
        Ok(_) => {}
    }
    if has_symlink_component(root, JOURNAL_PATH) {
        return Err(WorkspaceRecoveryError::UnsafeFile);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    let journal = loop {
        match JournalReadGuard::lock(&path) {
            Ok(journal) => break journal,
            Err(workspace::multi_edit_journal::JournalError::Busy)
                if std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => return Err(WorkspaceRecoveryError::Lock),
        }
    };
    ensure_no_shadow_marker(&path)?;
    ensure_no_external_marker(&path)?;
    if journal
        .load()
        .map_err(|_| WorkspaceRecoveryError::Io)?
        .is_some()
    {
        return Err(WorkspaceRecoveryError::Pending);
    }
    Ok(Some(journal))
}

/// Initialize the shared coordination point before an ordinary write or a
/// transaction takes its lock. Concurrent initializers converge on the same
/// directory; none may bypass the lock merely because it was initially absent.
#[cfg(unix)]
fn initialize_journal(root: &Path) -> Result<PathBuf, WorkspaceRecoveryError> {
    use std::os::unix::fs::DirBuilderExt;

    let mut parent = root.to_path_buf();
    for component in [".rapidlm", "multi-edit-journal"] {
        let path = parent.join(component);
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(WorkspaceRecoveryError::Io),
        }
        let metadata = fs::symlink_metadata(&path).map_err(|_| WorkspaceRecoveryError::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(WorkspaceRecoveryError::UnsafeFile);
        }
        // Another process may have just created this directory without yet
        // syncing its parent. Make our own durability barrier before use.
        fs::File::open(&parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| WorkspaceRecoveryError::Io)?;
        parent = path;
    }
    Ok(parent)
}

/// Hold this guard over the entire ordinary replacement, including reporting.
pub(super) fn lock_journal_for_write(
    root: &Path,
) -> Result<Option<JournalReadGuard>, WorkspaceRecoveryError> {
    #[cfg(unix)]
    initialize_journal(root)?;
    lock_clear_journal(root)
}

/// Future publication entry points must acquire this before any path locks.
#[cfg(unix)]
#[allow(dead_code)]
pub(super) fn lock_journal_for_publication(
    root: &Path,
) -> Result<JournalStore, WorkspaceRecoveryError> {
    let path = initialize_journal(root)?;
    lock_exclusive_existing(&path)
}

fn lock_exclusive_existing(path: &Path) -> Result<JournalStore, WorkspaceRecoveryError> {
    // A short-lived concurrent holder (including a subprocess being spawned)
    // can transiently retain a journal descriptor. Give it a bounded chance
    // to close; a genuinely busy journal still fails closed.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    loop {
        match JournalStore::lock(path) {
            Ok(journal) => return Ok(journal),
            Err(workspace::multi_edit_journal::JournalError::Busy)
                if std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => return Err(WorkspaceRecoveryError::Lock),
        }
    }
}

/// The lock and intent are internal state and cannot be model-edited.
pub(super) fn is_journal_path(path: &str) -> bool {
    let Ok(path) = protocol::RepoPath::parse(path) else {
        return false;
    };
    let normal = path.as_str().to_ascii_lowercase();
    normal == JOURNAL_PATH || normal.starts_with(&format!("{JOURNAL_PATH}/"))
}

/// Catch in-workspace symlink aliases to the journal as well as direct paths.
pub(super) fn is_journal_target(
    root: &Path,
    target: &Path,
) -> Result<bool, WorkspaceRecoveryError> {
    let journal = root.join(JOURNAL_PATH);
    let journal = match protocol::host_path::canonicalize(&journal) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(WorkspaceRecoveryError::UnsafeFile),
    };
    let parent = target.parent().ok_or(WorkspaceRecoveryError::InvalidPath)?;
    let parent = protocol::host_path::canonicalize(parent)
        .map_err(|_| WorkspaceRecoveryError::UnsafeFile)?;
    Ok(parent.starts_with(journal))
}

impl WorkspaceTools {
    /// Resolve an interrupted journal before an authorized workspace edit.
    /// The journal's exclusive lock is acquired before any path lock.
    pub(super) fn recover_before_edit(&self) -> Result<(), WorkspaceRecoveryError> {
        match lock_clear_journal(self.root()) {
            Ok(_) => return Ok(()),
            Err(WorkspaceRecoveryError::Pending) => {}
            Err(error) => return Err(error),
        }
        let path = self.root.join(JOURNAL_PATH);
        let journal = lock_exclusive_existing(&path)?;
        match self.recover_multi_edit(&journal) {
            Ok(RecoveryResult::NoRecord | RecoveryResult::Settled { .. }) => Ok(()),
            Ok(RecoveryResult::Conflicts { .. }) => Err(WorkspaceRecoveryError::Pending),
            Err(RecoverError::Io(error)) => Err(error),
            Err(_) => Err(WorkspaceRecoveryError::Io),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceRecoveryError {
    Unsupported,
    InvalidPath,
    UnsafeFile,
    ChangedDuringRead,
    Io,
    Invalidation,
    Lock,
    Pending,
    ShadowUncertain,
    ExternalUncertain,
    ReadRequired,
    PermissionDenied,
    ApprovalRequired,
    HooksRequireIntegration,
    WriteStepsRequireIntegration,
    LimitExceeded,
    AlreadyExists,
    MissingFile,
}

impl WorkspaceTools {
    /// Recover a locked journal under the same per-path locks used by this
    /// host's workspace writers. The caller must provide a trusted journal
    /// and run this before enabling any conflicting operation.
    pub fn recover_multi_edit(
        &self,
        journal: &JournalStore,
    ) -> Result<RecoveryResult, RecoverError<WorkspaceRecoveryError>> {
        if !cfg!(unix) {
            return Err(RecoverError::Io(WorkspaceRecoveryError::Unsupported));
        }
        if has_symlink_component(self.root(), JOURNAL_PATH) {
            return Err(RecoverError::Io(WorkspaceRecoveryError::UnsafeFile));
        }
        let expected = protocol::host_path::canonicalize(self.root.join(JOURNAL_PATH))
            .map_err(|_| RecoverError::Io(WorkspaceRecoveryError::InvalidPath))?;
        if expected != journal.directory() || !expected.starts_with(self.root()) {
            return Err(RecoverError::Io(WorkspaceRecoveryError::InvalidPath));
        }
        let Some(record) = journal.load().map_err(RecoverError::Journal)? else {
            return Ok(RecoveryResult::NoRecord);
        };
        if !self.write_locks.healthy() {
            return Err(RecoverError::Io(WorkspaceRecoveryError::Lock));
        }
        let mut paths: Vec<_> = record
            .entries()
            .iter()
            .map(|entry| self.root.join(&entry.path))
            .collect();
        paths.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
        let locks: Vec<_> = paths
            .iter()
            .map(|path| self.write_locks.lock_for(path))
            .collect();
        let _guards: Vec<_> = locks
            .iter()
            .map(|lock| {
                lock.lock()
                    .map_err(|_| RecoverError::Io(WorkspaceRecoveryError::Lock))
            })
            .collect::<Result<_, _>>()?;
        let mut adapter = WorkspaceRecovery { tools: self };
        multi_edit_recover::recover(journal, &mut adapter)
    }
}

pub(super) struct WorkspaceRecovery<'a> {
    pub(super) tools: &'a WorkspaceTools,
}

impl WorkspaceRecovery<'_> {
    pub(super) fn target(&self, relative: &str) -> Result<PathBuf, WorkspaceRecoveryError> {
        if is_journal_path(relative) {
            return Err(WorkspaceRecoveryError::UnsafeFile);
        }
        let path =
            protocol::RepoPath::parse(relative).map_err(|_| WorkspaceRecoveryError::InvalidPath)?;
        if path.as_str() != relative {
            return Err(WorkspaceRecoveryError::InvalidPath);
        }
        let parts: Vec<_> = path.components().collect();
        let mut target = self.tools.root.clone();
        for (index, part) in parts.iter().enumerate() {
            target.push(part);
            match fs::symlink_metadata(&target) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(WorkspaceRecoveryError::UnsafeFile);
                }
                Ok(meta) if index + 1 < parts.len() && !meta.is_dir() => {
                    return Err(WorkspaceRecoveryError::UnsafeFile);
                }
                Ok(_) => {}
                Err(err)
                    if err.kind() == std::io::ErrorKind::NotFound && index + 1 == parts.len() => {}
                Err(_) => return Err(WorkspaceRecoveryError::Io),
            }
        }
        Ok(target)
    }
    pub(super) fn invalidate(&self, path: &str) -> Result<(), WorkspaceRecoveryError> {
        self.tools
            .write_locks
            .0
            .had_writes
            .store(true, Ordering::SeqCst);
        let shared_ok = self.tools.write_locks.invalidate(self.tools.root(), path);
        let own_ok = self.tools.read_authority.as_ref().is_none_or(|authority| {
            if authority.invalidate(self.tools.root(), path).is_ok() {
                true
            } else {
                let _ = authority.advance_generation();
                false
            }
        });
        if !shared_ok || !own_ok {
            return Err(WorkspaceRecoveryError::Invalidation);
        }
        if let Some(hook) = &self.tools.evidence_invalidate {
            hook(path).map_err(|_| WorkspaceRecoveryError::Invalidation)?;
        }
        Ok(())
    }
}

impl RecoveryIo for WorkspaceRecovery<'_> {
    type Error = WorkspaceRecoveryError;

    fn observe(&mut self, relative: &str) -> Result<Option<FileImage>, Self::Error> {
        let target = self.target(relative)?;
        let metadata = match fs::symlink_metadata(&target) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(WorkspaceRecoveryError::Io),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(WorkspaceRecoveryError::UnsafeFile);
        }
        let before = crate::read_freshness::FileStamp::capture(&target)
            .map_err(|_| WorkspaceRecoveryError::Io)?;
        let metadata = fs::symlink_metadata(&target).map_err(|_| WorkspaceRecoveryError::Io)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(WorkspaceRecoveryError::UnsafeFile);
        }
        let bytes = read_file_bounded(&target, MAX_RECOVERY_BYTES)
            .map_err(|_| WorkspaceRecoveryError::Io)?;
        let after = crate::read_freshness::FileStamp::capture(&target)
            .map_err(|_| WorkspaceRecoveryError::Io)?;
        if before != after {
            return Err(WorkspaceRecoveryError::ChangedDuringRead);
        }
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(metadata.permissions().mode() & 0o7777)
        };
        #[cfg(not(unix))]
        let mode = None;
        Ok(Some(FileImage { bytes, mode }))
    }

    fn restore_if_matches(
        &mut self,
        relative: &str,
        expected: &Option<FileImage>,
        before: &Option<FileImage>,
    ) -> Result<bool, Self::Error> {
        if &self.observe(relative)? != expected {
            return Ok(false);
        }
        let target = self.target(relative)?;
        if let Some(before) = before {
            let mode = before.mode.ok_or(WorkspaceRecoveryError::UnsafeFile)?;
            atomic_write_with_mode(&target, &before.bytes, Some(mode))
                .map_err(|_| WorkspaceRecoveryError::Io)?;
        } else {
            fs::remove_file(&target).map_err(|_| WorkspaceRecoveryError::Io)?;
        }
        #[cfg(all(test, unix))]
        super::publication::pause_at_crash_boundary(self.tools.root(), "rollback");
        // Invalidate immediately, including when directory fsync fails.
        self.invalidate(relative)?;
        let parent = target.parent().ok_or(WorkspaceRecoveryError::InvalidPath)?;
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| WorkspaceRecoveryError::Io)?;
        Ok(true)
    }

    fn before_settlement(&mut self, record: &RecoveryRecord) -> Result<(), Self::Error> {
        if record.has_staging_plan() {
            for (index, entry) in record.entries().iter().enumerate() {
                let Some(name) = record.staging_name(index) else {
                    continue;
                };
                let target = self.target(&entry.path)?;
                let parent = target.parent().ok_or(WorkspaceRecoveryError::InvalidPath)?;
                let stage = parent.join(name);
                let metadata = match fs::symlink_metadata(&stage) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => return Err(WorkspaceRecoveryError::Io),
                };
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(WorkspaceRecoveryError::UnsafeFile);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let identity = record
                        .stage_identity(index)
                        .ok_or(WorkspaceRecoveryError::UnsafeFile)?;
                    if identity.device != metadata.dev() || identity.inode != metadata.ino() {
                        return Err(WorkspaceRecoveryError::UnsafeFile);
                    }
                    let contents = read_file_bounded(&stage, MAX_RECOVERY_BYTES)
                        .map_err(|_| WorkspaceRecoveryError::UnsafeFile)?;
                    let intended = entry
                        .after
                        .as_ref()
                        .ok_or(WorkspaceRecoveryError::UnsafeFile)?;
                    if !intended.bytes.starts_with(&contents) {
                        return Err(WorkspaceRecoveryError::UnsafeFile);
                    }
                }
                #[cfg(not(unix))]
                return Err(WorkspaceRecoveryError::Unsupported);
                #[cfg(unix)]
                {
                    fs::remove_file(&stage).map_err(|_| WorkspaceRecoveryError::Io)?;
                    fs::File::open(parent)
                        .and_then(|dir| dir.sync_all())
                        .map_err(|_| WorkspaceRecoveryError::Io)?;
                    #[cfg(test)]
                    super::publication::pause_at_crash_boundary(self.tools.root(), "cleanup");
                }
            }
        }
        for entry in record.entries() {
            self.invalidate(&entry.path)?;
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use workspace::multi_edit_recovery::{RecoveryEntry, RecoveryState};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rapid-real-recovery-{}-{}",
                std::process::id(),
                ATOMIC_WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::create_dir(path.join(".rapidlm")).unwrap();
            fs::create_dir(path.join(JOURNAL_PATH)).unwrap();
            fs::set_permissions(path.join(JOURNAL_PATH), fs::Permissions::from_mode(0o700))
                .unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn shadow_marker_blocks_writes_until_exact_owner_finishes() {
        let root = Temp::new();
        let token = begin_shadow(&root.0).unwrap();
        assert_eq!(
            lock_clear_journal(&root.0).err(),
            Some(WorkspaceRecoveryError::ShadowUncertain)
        );
        assert_eq!(
            begin_shadow(&root.0),
            Err(WorkspaceRecoveryError::ShadowUncertain)
        );
        assert_eq!(
            finish_shadow(&root.0, &"0".repeat(32)),
            Err(WorkspaceRecoveryError::ShadowUncertain)
        );
        assert_eq!(
            lock_journal_for_write(&root.0).err(),
            Some(WorkspaceRecoveryError::ShadowUncertain)
        );
        finish_shadow(&root.0, &token).unwrap();
        assert!(lock_clear_journal(&root.0).is_ok());
    }

    #[test]
    fn external_command_marker_blocks_writes_and_shadow_until_owner_finishes() {
        let root = Temp::new();
        let journal = lock_journal_for_publication(&root.0).unwrap();
        let token = begin_external(&journal, "hook").unwrap();
        assert_eq!(
            finish_external(&journal, "hook:wrong"),
            Err(WorkspaceRecoveryError::ExternalUncertain)
        );
        drop(journal);
        assert_eq!(
            lock_clear_journal(&root.0).err(),
            Some(WorkspaceRecoveryError::ExternalUncertain)
        );
        assert_eq!(
            begin_shadow(&root.0),
            Err(WorkspaceRecoveryError::ExternalUncertain)
        );
        let journal = lock_journal_for_publication(&root.0).unwrap();
        finish_external(&journal, &token).unwrap();
        drop(journal);
        assert!(lock_clear_journal(&root.0).is_ok());
    }
    fn image(value: &str, mode: u32) -> Option<FileImage> {
        Some(FileImage {
            bytes: value.as_bytes().to_vec(),
            mode: Some(mode),
        })
    }
    fn seed(root: &Path, path: &str, value: &str, mode: u32) {
        fs::write(root.join(path), value).unwrap();
        fs::set_permissions(root.join(path), fs::Permissions::from_mode(mode)).unwrap();
    }
    fn intent() -> RecoveryRecord {
        RecoveryRecord::new(
            RecoveryState::Intent,
            vec![
                RecoveryEntry {
                    path: "a".into(),
                    before: image("a0", 0o666),
                    after: image("a1", 0o600),
                },
                RecoveryEntry {
                    path: "b".into(),
                    before: None,
                    after: image("b1", 0o600),
                },
            ],
        )
        .unwrap()
    }

    #[test]
    fn child_transaction_lock_probe() {
        let Some(root) = std::env::var_os("RAPID_TEST_INITIALIZATION_ROOT") else {
            return;
        };
        assert!(matches!(
            lock_journal_for_publication(Path::new(&root)),
            Err(WorkspaceRecoveryError::Lock)
        ));
    }

    #[test]
    fn initial_ordinary_write_guard_excludes_a_transaction_in_another_process() {
        let root = Temp::new();
        fs::remove_dir_all(root.0.join(".rapidlm")).unwrap();
        let guard = lock_journal_for_write(&root.0).unwrap().unwrap();
        assert_eq!(
            fs::metadata(root.0.join(JOURNAL_PATH))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("exec_tools::recovery::tests::child_transaction_lock_probe")
            .env("RAPID_TEST_INITIALIZATION_ROOT", &root.0)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"));
        drop(guard);
        assert!(lock_journal_for_publication(&root.0).is_ok());
    }

    #[test]
    fn journal_lock_retries_a_short_lived_holder_and_refuses_a_long_one() {
        let root = Temp::new();
        let guard = lock_journal_for_write(&root.0).unwrap().unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            drop(guard);
        });
        let exclusive = lock_journal_for_publication(&root.0).unwrap();
        releaser.join().unwrap();
        assert!(matches!(
            lock_clear_journal(&root.0),
            Err(WorkspaceRecoveryError::Lock)
        ));
        drop(exclusive);
        assert!(lock_clear_journal(&root.0).is_ok());
    }

    #[test]
    fn concurrent_initializers_share_the_same_lock_and_reject_symlinks() {
        let root = Temp::new();
        fs::remove_dir_all(root.0.join(".rapidlm")).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let path = root.0.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    lock_journal_for_write(&path).unwrap().unwrap()
                })
            })
            .collect();
        barrier.wait();
        let guards: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert!(matches!(
            lock_journal_for_publication(&root.0),
            Err(WorkspaceRecoveryError::Lock)
        ));
        drop(guards);
        assert!(lock_journal_for_publication(&root.0).is_ok());
        fs::remove_dir_all(root.0.join(".rapidlm")).unwrap();
        symlink(std::env::temp_dir(), root.0.join(".rapidlm")).unwrap();
        assert!(matches!(
            lock_journal_for_write(&root.0),
            Err(WorkspaceRecoveryError::UnsafeFile)
        ));
    }

    #[test]
    fn journal_files_cannot_be_transaction_targets() {
        let root = Temp::new();
        let tools = WorkspaceTools::open(&root.0).unwrap();
        let recovery = WorkspaceRecovery { tools: &tools };
        assert!(matches!(
            recovery.target(".rapidlm/multi-edit-journal/journal.lock"),
            Err(WorkspaceRecoveryError::UnsafeFile)
        ));
        assert!(is_journal_path("./.RapidLM/multi-edit-journal/active.json"));
        assert!(is_journal_path(
            ".rapidlm/./multi-edit-journal//journal.lock"
        ));
        assert!(!is_journal_path(".rapidlm/multi-edit-journal-other/file"));
        symlink(root.0.join(JOURNAL_PATH), root.0.join("alias")).unwrap();
        assert!(is_journal_target(&root.0, &root.0.join("alias/journal.lock")).unwrap());
    }

    #[test]
    fn recovery_preserves_an_unregistered_staging_name_collision() {
        let root = Temp::new();
        seed(&root.0, "a", "a0", 0o666);
        let journal = JournalStore::lock(&root.0.join(JOURNAL_PATH)).unwrap();
        let record = intent()
            .with_staging_plan("0123456789abcdef0123456789abcdef".into())
            .unwrap();
        let stage = root.0.join(record.staging_name(0).unwrap());
        fs::write(&stage, b"unrelated").unwrap();
        journal.write_intent(&record).unwrap();
        let tools = WorkspaceTools::open(&root.0).unwrap();
        assert!(matches!(
            tools.recover_multi_edit(&journal),
            Err(RecoverError::Io(WorkspaceRecoveryError::UnsafeFile))
        ));
        assert_eq!(fs::read(&stage).unwrap(), b"unrelated");
        assert_eq!(journal.load().unwrap(), Some(record));
    }

    #[test]
    fn recovery_preserves_a_replaced_stage_and_removes_a_registered_partial_stage() {
        use std::os::unix::fs::MetadataExt;
        use workspace::multi_edit_recovery::StageIdentity;
        let root = Temp::new();
        seed(&root.0, "a", "a0", 0o666);
        let journal = JournalStore::lock(&root.0.join(JOURNAL_PATH)).unwrap();
        let record = intent()
            .with_staging_plan("0123456789abcdef0123456789abcdef".into())
            .unwrap();
        let stage = root.0.join(record.staging_name(0).unwrap());
        fs::write(&stage, b"a").unwrap();
        let metadata = fs::metadata(&stage).unwrap();
        journal.write_intent(&record).unwrap();
        let record = journal
            .register_stage(
                &record,
                0,
                StageIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                },
            )
            .unwrap();
        let original = root.0.join("original-stage");
        fs::rename(&stage, &original).unwrap();
        fs::write(&stage, b"a").unwrap();
        let tools = WorkspaceTools::open(&root.0).unwrap();
        assert!(matches!(
            tools.recover_multi_edit(&journal),
            Err(RecoverError::Io(WorkspaceRecoveryError::UnsafeFile))
        ));
        assert_eq!(fs::read(&stage).unwrap(), b"a");
        assert_eq!(journal.load().unwrap(), Some(record.clone()));
        fs::remove_file(&stage).unwrap();
        fs::rename(&original, &stage).unwrap();
        fs::write(&stage, b"externally modified").unwrap();
        assert!(matches!(
            tools.recover_multi_edit(&journal),
            Err(RecoverError::Io(WorkspaceRecoveryError::UnsafeFile))
        ));
        assert_eq!(fs::read(&stage).unwrap(), b"externally modified");
        fs::write(&stage, b"a").unwrap();
        assert!(matches!(
            tools.recover_multi_edit(&journal),
            Ok(RecoveryResult::Settled { .. })
        ));
        assert!(!stage.exists());
        assert_eq!(journal.load().unwrap(), None);
    }

    #[test]
    fn ordinary_tools_cannot_replace_journal_lock_or_record() {
        let root = Temp::new();
        drop(lock_journal_for_write(&root.0).unwrap());
        symlink(root.0.join(JOURNAL_PATH), root.0.join("alias")).unwrap();
        let mut tools = WorkspaceTools::open_with_permissions(
            &root.0,
            PermissionLattice::new(PermissionMode::BypassPermissions),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        for (tool, args) in [
            (
                WORKSPACE_WRITE_TOOL,
                r#"{"path":".rapidlm/multi-edit-journal/journal.lock","content":"replace"}"#,
            ),
            (
                WORKSPACE_PATCH_TOOL,
                r#"{"path":".rapidlm/multi-edit-journal/journal.lock","old":"x","new":"y"}"#,
            ),
            (
                WORKSPACE_WRITE_TOOL,
                r#"{"path":"alias/journal.lock","content":"replace"}"#,
            ),
        ] {
            let call = ProposedToolCall::new("journal-edit", tool, args).unwrap();
            let validated = tools.validate(&call, &cancel).unwrap();
            let result = tools.execute(&validated, &cancel).unwrap();
            assert!(
                matches!(result, ToolStepResult::Failed { detail: Some(detail), .. } if detail.contains("internal workspace state"))
            );
        }
        assert_eq!(
            fs::read(root.0.join(JOURNAL_PATH).join("journal.lock")).unwrap(),
            b""
        );
        assert!(lock_journal_for_publication(&root.0).is_ok());
    }
    #[test]
    fn normal_write_recovers_pending_journal_and_requires_a_new_read() {
        let root = Temp::new();
        let journal = JournalStore::lock(&root.0.join(JOURNAL_PATH)).unwrap();
        journal.write_intent(&intent()).unwrap();
        seed(&root.0, "a", "a1", 0o600);
        seed(&root.0, "b", "b1", 0o600);
        drop(journal);
        let mut tools = WorkspaceTools::open_with_permissions(
            &root.0,
            PermissionLattice::new(PermissionMode::BypassPermissions),
        )
        .unwrap();
        tools
            .read_authority
            .as_ref()
            .unwrap()
            .record_visible(tools.root(), "a", b"a1")
            .unwrap();
        let cancel = CancellationToken::new();
        let call = ProposedToolCall::new(
            "write",
            WORKSPACE_WRITE_TOOL,
            r#"{"path":"a","content":"after"}"#,
        )
        .unwrap();
        let validated = tools.validate(&call, &cancel).unwrap();
        let result = tools.execute(&validated, &cancel).unwrap();
        assert!(
            matches!(&result, ToolStepResult::Failed { detail: Some(detail), .. } if detail.contains("read this file")),
            "{result:?}"
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert!(!root.0.join("b").exists());
        let journal = JournalStore::lock(&root.0.join(JOURNAL_PATH)).unwrap();
        assert_eq!(journal.load().unwrap(), None);
        drop(journal);
        let authority = tools.read_authority.as_ref().unwrap();
        authority.record_visible(tools.root(), "a", b"a0").unwrap();
        let result = tools.execute(&validated, &cancel).unwrap();
        assert!(
            matches!(result, ToolStepResult::Succeeded { .. }),
            "{result:?}"
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"after");
    }

    #[test]
    fn normal_writes_refuse_corrupt_or_busy_journals() {
        let root = Temp::new();
        let journal = JournalStore::lock(&root.0.join(JOURNAL_PATH)).unwrap();
        assert!(matches!(
            lock_clear_journal(&root.0),
            Err(WorkspaceRecoveryError::Lock)
        ));
        drop(journal);
        fs::write(root.0.join(JOURNAL_PATH).join("active.json"), b"broken").unwrap();
        assert!(matches!(
            lock_clear_journal(&root.0),
            Err(WorkspaceRecoveryError::Io)
        ));
        fs::remove_file(root.0.join(JOURNAL_PATH).join("active.json")).unwrap();
        assert!(lock_clear_journal(&root.0).unwrap().is_some());
    }

    #[test]
    fn workspace_recovery_waits_for_the_existing_host_path_lock() {
        let root = Temp::new();
        let journal = JournalStore::lock(&root.0.join(JOURNAL_PATH)).unwrap();
        journal.write_intent(&intent()).unwrap();
        seed(&root.0, "a", "a1", 0o600);
        seed(&root.0, "b", "b1", 0o600);
        let tools = WorkspaceTools::open(&root.0).unwrap();
        let lock = tools.write_locks.lock_for(&tools.root().join("a"));
        let held = lock.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx.send(tools.recover_multi_edit(&journal)).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(150)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
        drop(held);
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap(),
            RecoveryResult::Settled { restored: 2 }
        );
        worker.join().unwrap();
    }

    #[test]
    fn workspace_recovery_rejects_a_journal_from_another_workspace() {
        let one = Temp::new();
        let two = Temp::new();
        let journal = JournalStore::lock(&one.0.join(JOURNAL_PATH)).unwrap();
        journal.write_intent(&intent()).unwrap();
        seed(&two.0, "a", "a1", 0o600);
        seed(&two.0, "b", "b1", 0o600);
        let tools = WorkspaceTools::open(&two.0).unwrap();
        assert!(matches!(
            tools.recover_multi_edit(&journal),
            Err(RecoverError::Io(WorkspaceRecoveryError::InvalidPath))
        ));
        assert_eq!(fs::read(two.0.join("a")).unwrap(), b"a1");
        assert_eq!(fs::read(two.0.join("b")).unwrap(), b"b1");
        assert!(journal.load().unwrap().is_some());
    }

    #[test]
    fn workspace_recovery_restores_files_modes_and_invalidates_read_authority() {
        let root = Temp::new();
        let journal = JournalStore::lock(&root.0.join(JOURNAL_PATH)).unwrap();
        let record = intent();
        journal.write_intent(&record).unwrap();
        seed(&root.0, "a", "a1", 0o600);
        seed(&root.0, "b", "b1", 0o600);
        let tools = WorkspaceTools::open(&root.0).unwrap();
        let authority = tools.read_authority.as_ref().unwrap();
        authority.record_visible(tools.root(), "a", b"a1").unwrap();
        assert_eq!(
            tools.recover_multi_edit(&journal).unwrap(),
            RecoveryResult::Settled { restored: 2 }
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(
            fs::metadata(root.0.join("a")).unwrap().permissions().mode() & 0o7777,
            0o666
        );
        assert!(!root.0.join("b").exists());
        assert_eq!(
            authority.check(tools.root(), "a", b"a1").unwrap(),
            crate::read_freshness::ReadVerdict::Unread
        );
        assert_eq!(journal.load().unwrap(), None);
    }
    #[test]
    fn workspace_recovery_preserves_external_conflict_and_refuses_symlinks() {
        let root = Temp::new();
        let journal = JournalStore::lock(&root.0.join(JOURNAL_PATH)).unwrap();
        let record = intent();
        journal.write_intent(&record).unwrap();
        seed(&root.0, "a", "external", 0o600);
        seed(&root.0, "b", "b1", 0o600);
        let tools = WorkspaceTools::open(&root.0).unwrap();
        assert_eq!(
            tools.recover_multi_edit(&journal).unwrap(),
            RecoveryResult::Conflicts {
                restored: 1,
                paths: vec!["a".into()]
            }
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"external");
        assert!(!root.0.join("b").exists());
        assert_eq!(journal.load().unwrap(), Some(record));
        fs::remove_file(root.0.join("a")).unwrap();
        symlink("/etc/passwd", root.0.join("a")).unwrap();
        assert!(matches!(
            tools.recover_multi_edit(&journal),
            Err(RecoverError::Io(WorkspaceRecoveryError::UnsafeFile))
        ));
        assert!(journal.load().unwrap().is_some());
    }
    #[test]
    fn workspace_recovery_retries_evidence_invalidation_before_clearing_journal() {
        let root = Temp::new();
        let journal = JournalStore::lock(&root.0.join(JOURNAL_PATH)).unwrap();
        journal.write_intent(&intent()).unwrap();
        seed(&root.0, "a", "a1", 0o600);
        seed(&root.0, "b", "b1", 0o600);
        let mut tools = WorkspaceTools::open(&root.0).unwrap();
        let count = Arc::new(AtomicU64::new(0));
        let shared = Arc::clone(&count);
        tools.set_evidence_invalidator(Arc::new(move |_| {
            if shared.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("retry".into())
            } else {
                Ok(0)
            }
        }));
        assert!(matches!(
            tools.recover_multi_edit(&journal),
            Err(RecoverError::Io(WorkspaceRecoveryError::Invalidation))
        ));
        assert!(journal.load().unwrap().is_some());
        assert_eq!(
            tools.recover_multi_edit(&journal).unwrap(),
            RecoveryResult::Settled { restored: 1 }
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert!(!root.0.join("b").exists());
        assert!(count.load(Ordering::SeqCst) >= 3);
        assert_eq!(journal.load().unwrap(), None);
    }
}
