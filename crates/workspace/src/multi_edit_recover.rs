//! Recovery orchestration over a locked journal and an explicit workspace I/O
//! adapter. This is not connected to a production workspace yet.
use std::collections::BTreeMap;

use crate::multi_edit_journal::{JournalError, JournalStore};
use crate::multi_edit_recovery::{FileImage, RecoveryAction, RecoveryRecord};

/// Recovery storage operations. The caller must hold appropriate workspace
/// locks for the entire recovery. Implementations must confine every path,
/// refuse unsafe aliases, bound reads, durably sync changes and check the exact expected
/// image immediately before the mutation. A false result means the file no
/// longer matches; it must not have been modified by that call.
pub trait RecoveryIo {
    type Error;
    fn observe(&mut self, path: &str) -> Result<Option<FileImage>, Self::Error>;
    fn restore_if_matches(
        &mut self,
        path: &str,
        expected: &Option<FileImage>,
        before: &Option<FileImage>,
    ) -> Result<bool, Self::Error>;

    /// Complete any durable invalidation required before clearing the record.
    /// This is also called after a restart where no restoration was needed.
    fn before_settlement(&mut self, _record: &RecoveryRecord) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum RecoverError<E> {
    Journal(JournalError),
    Io(E),
    InvalidRecord,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryResult {
    NoRecord,
    Settled { restored: usize },
    Conflicts { restored: usize, paths: Vec<String> },
}

/// Restore interrupted intent or verify a committed transaction. Failed I/O
/// or any conflict leaves the record present for an explicit later retry.
pub fn recover<B: RecoveryIo>(
    journal: &JournalStore,
    io: &mut B,
) -> Result<RecoveryResult, RecoverError<B::Error>> {
    let Some(record) = journal.load().map_err(RecoverError::Journal)? else {
        return Ok(RecoveryResult::NoRecord);
    };
    let observed = observe_all(&record, io)?;
    let actions = record
        .plan(&observed)
        .map_err(|_| RecoverError::InvalidRecord)?;
    let mut restored = 0;
    for action in actions {
        if let RecoveryAction::Restore { path, before } = action {
            let entry = record
                .entries()
                .iter()
                .find(|entry| entry.path == path)
                .ok_or(RecoverError::InvalidRecord)?;
            if io
                .restore_if_matches(&path, &entry.after, &before)
                .map_err(RecoverError::Io)?
            {
                restored += 1;
            }
        }
    }
    // Re-observe every path after restoration: another writer may have
    // changed an already-checked path or a restore may have been refused.
    let current = observe_all(&record, io)?;
    let mut unresolved = Vec::new();
    for action in record
        .plan(&current)
        .map_err(|_| RecoverError::InvalidRecord)?
    {
        match action {
            RecoveryAction::AlreadyOriginal { .. } | RecoveryAction::Committed { .. } => {}
            RecoveryAction::Restore { path, .. } | RecoveryAction::Conflict { path } => {
                unresolved.push(path)
            }
        }
    }
    if !unresolved.is_empty() {
        return Ok(RecoveryResult::Conflicts {
            restored,
            paths: unresolved,
        });
    }
    io.before_settlement(&record).map_err(RecoverError::Io)?;
    journal
        .remove_settled(&record, &current)
        .map_err(RecoverError::Journal)?;
    Ok(RecoveryResult::Settled { restored })
}

fn observe_all<B: RecoveryIo>(
    record: &RecoveryRecord,
    io: &mut B,
) -> Result<BTreeMap<String, Option<FileImage>>, RecoverError<B::Error>> {
    let mut current = BTreeMap::new();
    let mut bytes = 0usize;
    for entry in record.entries() {
        let image = io.observe(&entry.path).map_err(RecoverError::Io)?;
        bytes = bytes
            .checked_add(image.as_ref().map_or(0, |image| image.bytes.len()))
            .ok_or(RecoverError::InvalidRecord)?;
        if bytes > crate::multi_edit_recovery::MAX_RECOVERY_BYTES {
            return Err(RecoverError::InvalidRecord);
        }
        current.insert(entry.path.clone(), image);
    }
    Ok(current)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::multi_edit_recovery::{RecoveryEntry, RecoveryState};
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);
    struct TestRoot(PathBuf);
    impl TestRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rapid-recover-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            fs::create_dir(path.join("journal")).unwrap();
            fs::set_permissions(path.join("journal"), fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn image(text: &str) -> Option<FileImage> {
        Some(FileImage {
            bytes: text.as_bytes().to_vec(),
            mode: Some(0o600),
        })
    }
    fn seed(root: &Path, path: &str, bytes: &str) {
        fs::write(root.join(path), bytes).unwrap();
        fs::set_permissions(root.join(path), fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn record(state: RecoveryState) -> RecoveryRecord {
        RecoveryRecord::new(
            state,
            vec![
                RecoveryEntry {
                    path: "a".into(),
                    before: image("a0"),
                    after: image("a1"),
                },
                RecoveryEntry {
                    path: "b".into(),
                    before: None,
                    after: image("b1"),
                },
            ],
        )
        .unwrap()
    }
    /// Deliberately test-only: production needs path confinement, shared
    /// workspace locks, and atomic compare-to-replace semantics.
    struct FixtureIo {
        root: PathBuf,
        fail_after: Option<usize>,
        restores: usize,
        restored_paths: Vec<String>,
        race_on: Option<String>,
    }
    impl RecoveryIo for FixtureIo {
        type Error = &'static str;
        fn observe(&mut self, path: &str) -> Result<Option<FileImage>, Self::Error> {
            let path = self.root.join(path);
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err("stat"),
            };
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err("type");
            }
            Ok(Some(FileImage {
                bytes: fs::read(path).map_err(|_| "read")?,
                mode: Some(meta.permissions().mode() & 0o7777),
            }))
        }
        fn restore_if_matches(
            &mut self,
            path: &str,
            expected: &Option<FileImage>,
            before: &Option<FileImage>,
        ) -> Result<bool, Self::Error> {
            if self.fail_after == Some(self.restores) {
                return Err("injected failure");
            }
            if self.race_on.as_deref() == Some(path) {
                seed(&self.root, path, "external-race");
                self.race_on = None;
            }
            if &self.observe(path)? != expected {
                return Ok(false);
            }
            let target = self.root.join(path);
            if let Some(before) = before {
                let mut file = fs::File::create(&target).map_err(|_| "create")?;
                file.set_permissions(fs::Permissions::from_mode(before.mode.ok_or("mode")?))
                    .map_err(|_| "mode")?;
                file.write_all(&before.bytes)
                    .and_then(|_| file.sync_all())
                    .map_err(|_| "write")?;
            } else {
                fs::remove_file(target).map_err(|_| "delete")?;
            }
            fs::File::open(&self.root)
                .and_then(|file| file.sync_all())
                .map_err(|_| "sync")?;
            self.restores += 1;
            self.restored_paths.push(path.to_owned());
            Ok(true)
        }
    }
    fn io(root: &TestRoot) -> FixtureIo {
        FixtureIo {
            root: root.0.clone(),
            fail_after: None,
            restores: 0,
            restored_paths: Vec::new(),
            race_on: None,
        }
    }

    #[test]
    fn recovery_restores_real_files_in_reverse_order_and_removes_settled_record() {
        let root = TestRoot::new();
        let journal = JournalStore::lock(&root.0.join("journal")).unwrap();
        let record = record(RecoveryState::Intent);
        journal.write_intent(&record).unwrap();
        seed(&root.0, "a", "a1");
        seed(&root.0, "b", "b1");
        let mut io = io(&root);
        assert_eq!(
            recover(&journal, &mut io).unwrap(),
            RecoveryResult::Settled { restored: 2 }
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(io.restored_paths, ["b", "a"]);
        assert!(!root.0.join("b").exists());
        assert_eq!(journal.load().unwrap(), None);
    }
    #[test]
    fn recovery_rechecks_postimage_after_planning_and_keeps_the_conflict() {
        let root = TestRoot::new();
        let journal = JournalStore::lock(&root.0.join("journal")).unwrap();
        let record = record(RecoveryState::Intent);
        journal.write_intent(&record).unwrap();
        seed(&root.0, "a", "a1");
        seed(&root.0, "b", "b1");
        let mut io = io(&root);
        io.race_on = Some("a".into());
        assert_eq!(
            recover(&journal, &mut io).unwrap(),
            RecoveryResult::Conflicts {
                restored: 1,
                paths: vec!["a".into()]
            }
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"external-race");
        assert_eq!(journal.load().unwrap(), Some(record));
    }
    #[test]
    fn recovery_preserves_external_conflict_and_retries_after_a_partial_failure() {
        let root = TestRoot::new();
        let journal = JournalStore::lock(&root.0.join("journal")).unwrap();
        let record = record(RecoveryState::Intent);
        journal.write_intent(&record).unwrap();
        seed(&root.0, "a", "external");
        seed(&root.0, "b", "b1");
        let mut io = io(&root);
        assert_eq!(
            recover(&journal, &mut io).unwrap(),
            RecoveryResult::Conflicts {
                restored: 1,
                paths: vec!["a".into()]
            }
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"external");
        assert!(!root.0.join("b").exists());
        assert_eq!(journal.load().unwrap(), Some(record));
        seed(&root.0, "a", "a1");
        io.fail_after = Some(io.restores);
        assert_eq!(
            recover(&journal, &mut io),
            Err(RecoverError::Io("injected failure"))
        );
        assert!(journal.load().unwrap().is_some());
        io.fail_after = None;
        assert_eq!(
            recover(&journal, &mut io).unwrap(),
            RecoveryResult::Settled { restored: 1 }
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
    }
    #[test]
    fn committed_record_never_rolls_back_an_external_edit() {
        let root = TestRoot::new();
        let journal = JournalStore::lock(&root.0.join("journal")).unwrap();
        let intent = record(RecoveryState::Intent);
        journal.write_intent(&intent).unwrap();
        seed(&root.0, "a", "a1");
        seed(&root.0, "b", "b1");
        let mut io = io(&root);
        let current = observe_all(&intent, &mut io).unwrap();
        let committed = intent.committed(&current).unwrap();
        journal.write_committed(&intent, &committed).unwrap();
        seed(&root.0, "a", "external");
        assert_eq!(
            recover(&journal, &mut io).unwrap(),
            RecoveryResult::Conflicts {
                restored: 0,
                paths: vec!["a".into()]
            }
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"external");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b1");
        assert_eq!(journal.load().unwrap(), Some(committed));
    }
}
