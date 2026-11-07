//! Journal-backed publication coordinator for a future multi-file tool.
//!
//! The caller must hold the exclusive journal lock and all workspace path
//! locks throughout this operation. This module has no production I/O
//! adapter and is not yet exposed to a model or CLI.
use std::collections::BTreeMap;

use crate::multi_edit_journal::{JournalError, JournalStore};
use crate::multi_edit_recover::{self, RecoverError, RecoveryIo, RecoveryResult};
use crate::multi_edit_recovery::{FileImage, MAX_RECOVERY_BYTES, RecoveryRecord, RecoveryState};
use crate::view::CancellationToken;

/// Filesystem publication. The caller must already have validated every
/// path, permission, approval and hook. `prepare` must stage and sync all
/// replacements without touching their targets; on failure it must clean up
/// any partial staging. `publish_if_matches` must recheck the full expected
/// image immediately before publishing and durably sync the target directory.
/// Implementations own cleanup of staging, including after an interruption.
pub trait PublishIo: RecoveryIo {
    fn prepare(&mut self, record: &RecoveryRecord) -> Result<(), Self::Error>;
    /// Revalidate mutable authorization immediately before committing.
    fn before_commit(&mut self, _record: &RecoveryRecord) -> Result<(), Self::Error> {
        Ok(())
    }
    fn publish_if_matches(
        &mut self,
        path: &str,
        expected: &Option<FileImage>,
        after: &Option<FileImage>,
    ) -> Result<bool, Self::Error>;
}

/// A journal-first adapter creates each stage exclusively, durably records
/// its identity before writing any bytes, and updates `intent` after each
/// successful registration. Recovery must not delete unregistered collisions.
pub trait JournaledPublishIo: PublishIo {
    fn prepare_journaled(
        &mut self,
        journal: &JournalStore,
        intent: &mut RecoveryRecord,
    ) -> Result<(), JournaledPrepareError<Self::Error>>;
}

#[derive(Debug, Eq, PartialEq)]
pub enum JournaledPrepareError<E> {
    Io(E),
    Journal(JournalError),
}

#[derive(Debug, Eq, PartialEq)]
pub enum PublishFailure<E> {
    Cancelled,
    InvalidRecord,
    Conflict { path: String },
    Io(E),
    Journal(JournalError),
}

#[derive(Debug, Eq, PartialEq)]
pub enum PublishOutcome<E> {
    /// No target was published. Staging cleanup is the adapter's duty.
    Rejected(PublishFailure<E>),
    /// An intent was published and recovery restored every original image.
    RolledBack(PublishFailure<E>),
    /// Recovery could not settle the intent; the journal must be inspected.
    RecoveryPending {
        reason: PublishFailure<E>,
        recovery: Option<RecoverError<E>>,
        conflicts: Vec<String>,
    },
    /// The commit record was published, but verification or cleanup remains.
    CommittedCleanupPending {
        recovery: Option<RecoverError<E>>,
        conflicts: Vec<String>,
    },
    /// The committed record may or may not have reached durable storage.
    CommitUncertain(JournalError),
    Committed,
}

/// Publish a fully approved and staged intent. No filesystem-wide atomic
/// visibility is claimed: separate files are published one at a time.
pub fn publish<B: PublishIo>(
    journal: &JournalStore,
    io: &mut B,
    record: &RecoveryRecord,
    cancel: &CancellationToken,
) -> PublishOutcome<B::Error> {
    publish_with_cancel(journal, io, record, || cancel.is_cancelled())
}

/// Publish while consulting the caller's live cancellation source. This lets
/// runtimes use their own token without a polling thread or a stale snapshot.
pub fn publish_with_cancel<B: PublishIo>(
    journal: &JournalStore,
    io: &mut B,
    record: &RecoveryRecord,
    cancelled: impl Fn() -> bool,
) -> PublishOutcome<B::Error> {
    if record.state() != RecoveryState::Intent || record.encode().is_err() {
        return PublishOutcome::Rejected(PublishFailure::InvalidRecord);
    }
    if cancelled() {
        return PublishOutcome::Rejected(PublishFailure::Cancelled);
    }
    match journal.load() {
        Ok(None) => {}
        Ok(Some(_)) => {
            return PublishOutcome::Rejected(PublishFailure::Journal(JournalError::Exists));
        }
        Err(error) => return PublishOutcome::Rejected(PublishFailure::Journal(error)),
    }
    let before = match observe_all(record, io) {
        Ok(value) => value,
        Err(error) => return PublishOutcome::Rejected(error),
    };
    for entry in record.entries() {
        if before.get(&entry.path) != Some(&entry.before) {
            return PublishOutcome::Rejected(PublishFailure::Conflict {
                path: entry.path.clone(),
            });
        }
    }
    if cancelled() {
        return PublishOutcome::Rejected(PublishFailure::Cancelled);
    }
    if let Err(error) = io.prepare(record) {
        return PublishOutcome::Rejected(PublishFailure::Io(error));
    }
    if cancelled() {
        return PublishOutcome::Rejected(PublishFailure::Cancelled);
    }
    if let Err(error) = journal.write_intent(record) {
        // Uncertain intent publication has not changed a target, but the
        // journal must be inspected before any later transaction proceeds.
        if error == JournalError::Uncertain {
            return PublishOutcome::RecoveryPending {
                reason: PublishFailure::Journal(error),
                recovery: None,
                conflicts: Vec::new(),
            };
        }
        return PublishOutcome::Rejected(PublishFailure::Journal(error));
    }
    publish_effects(journal, io, record, &cancelled)
}

/// Journal-first publication for adapters that durably register each stage.
/// The caller must generate a fresh unpredictable staging id and hold both
/// the exclusive journal lock and target path locks for this entire call.
pub fn publish_journal_first<B: JournaledPublishIo>(
    journal: &JournalStore,
    io: &mut B,
    record: &RecoveryRecord,
    cancelled: impl Fn() -> bool,
) -> PublishOutcome<B::Error> {
    if record.state() != RecoveryState::Intent
        || record.encode().is_err()
        || !record.has_staging_plan()
        || record
            .entries()
            .iter()
            .enumerate()
            .any(|(index, _)| record.stage_identity(index).is_some())
    {
        return PublishOutcome::Rejected(PublishFailure::InvalidRecord);
    }
    if cancelled() {
        return PublishOutcome::Rejected(PublishFailure::Cancelled);
    }
    match journal.load() {
        Ok(None) => {}
        Ok(Some(_)) => {
            return PublishOutcome::Rejected(PublishFailure::Journal(JournalError::Exists));
        }
        Err(error) => return PublishOutcome::Rejected(PublishFailure::Journal(error)),
    }
    let before = match observe_all(record, io) {
        Ok(before) => before,
        Err(error) => return PublishOutcome::Rejected(error),
    };
    for entry in record.entries() {
        if before.get(&entry.path) != Some(&entry.before) {
            return PublishOutcome::Rejected(PublishFailure::Conflict {
                path: entry.path.clone(),
            });
        }
    }
    if cancelled() {
        return PublishOutcome::Rejected(PublishFailure::Cancelled);
    }
    if let Err(error) = journal.write_intent(record) {
        if error == JournalError::Uncertain {
            return PublishOutcome::RecoveryPending {
                reason: PublishFailure::Journal(error),
                recovery: None,
                conflicts: Vec::new(),
            };
        }
        return PublishOutcome::Rejected(PublishFailure::Journal(error));
    }
    let mut current = record.clone();
    if cancelled() {
        return rollback(journal, io, &current, PublishFailure::Cancelled);
    }
    if let Err(error) = io.prepare_journaled(journal, &mut current) {
        let reason = match error {
            JournaledPrepareError::Io(error) => PublishFailure::Io(error),
            JournaledPrepareError::Journal(error) => PublishFailure::Journal(error),
        };
        return rollback(journal, io, &current, reason);
    }
    // A buggy adapter or an uncertain registration must never publish using
    // an in-memory record that differs from the durable intent.
    match journal.load() {
        Ok(Some(durable)) if durable == current => {}
        Ok(_) => {
            return PublishOutcome::RecoveryPending {
                reason: PublishFailure::Journal(JournalError::Conflict),
                recovery: None,
                conflicts: Vec::new(),
            };
        }
        Err(error) => {
            return PublishOutcome::RecoveryPending {
                reason: PublishFailure::Journal(error),
                recovery: None,
                conflicts: Vec::new(),
            };
        }
    }
    if current.entries().iter().enumerate().any(|(index, _)| {
        current.staging_name(index).is_some() && current.stage_identity(index).is_none()
    }) {
        return rollback(journal, io, &current, PublishFailure::InvalidRecord);
    }
    if cancelled() {
        return rollback(journal, io, &current, PublishFailure::Cancelled);
    }
    publish_effects(journal, io, &current, &cancelled)
}

fn publish_effects<B: PublishIo>(
    journal: &JournalStore,
    io: &mut B,
    record: &RecoveryRecord,
    cancelled: &impl Fn() -> bool,
) -> PublishOutcome<B::Error> {
    for entry in record.entries() {
        if cancelled() {
            return rollback(journal, io, record, PublishFailure::Cancelled);
        }
        match io.publish_if_matches(&entry.path, &entry.before, &entry.after) {
            Ok(true) => {}
            Ok(false) => {
                return rollback(
                    journal,
                    io,
                    record,
                    PublishFailure::Conflict {
                        path: entry.path.clone(),
                    },
                );
            }
            Err(error) => return rollback(journal, io, record, PublishFailure::Io(error)),
        }
    }
    if cancelled() {
        return rollback(journal, io, record, PublishFailure::Cancelled);
    }
    let after = match observe_all(record, io) {
        Ok(value) => value,
        Err(error) => return rollback(journal, io, record, error),
    };
    let committed = match record.committed(&after) {
        Ok(record) => record,
        Err(_) => return rollback(journal, io, record, PublishFailure::InvalidRecord),
    };
    if cancelled() {
        return rollback(journal, io, record, PublishFailure::Cancelled);
    }
    if let Err(error) = io.before_commit(record) {
        return rollback(journal, io, record, PublishFailure::Io(error));
    }
    if cancelled() {
        return rollback(journal, io, record, PublishFailure::Cancelled);
    }
    if let Err(error) = journal.write_committed(record, &committed) {
        if error == JournalError::Uncertain {
            return PublishOutcome::CommitUncertain(error);
        }
        return rollback(journal, io, record, PublishFailure::Journal(error));
    }
    // Once committed, never roll back on cleanup failure or cancellation.
    match multi_edit_recover::recover(journal, io) {
        Ok(RecoveryResult::Settled { .. }) => PublishOutcome::Committed,
        Ok(RecoveryResult::Conflicts { paths, .. }) => PublishOutcome::CommittedCleanupPending {
            recovery: None,
            conflicts: paths,
        },
        Ok(RecoveryResult::NoRecord) => PublishOutcome::CommittedCleanupPending {
            recovery: None,
            conflicts: Vec::new(),
        },
        Err(error) => PublishOutcome::CommittedCleanupPending {
            recovery: Some(error),
            conflicts: Vec::new(),
        },
    }
}

fn rollback<B: PublishIo>(
    journal: &JournalStore,
    io: &mut B,
    expected: &RecoveryRecord,
    reason: PublishFailure<B::Error>,
) -> PublishOutcome<B::Error> {
    match journal.load() {
        Ok(Some(record)) if &record == expected => {}
        Ok(Some(record)) if record.state() == RecoveryState::Committed => {
            return PublishOutcome::CommitUncertain(JournalError::Conflict);
        }
        Ok(_) => {
            return PublishOutcome::RecoveryPending {
                reason,
                recovery: None,
                conflicts: Vec::new(),
            };
        }
        Err(error) => {
            return PublishOutcome::RecoveryPending {
                reason,
                recovery: Some(RecoverError::Journal(error)),
                conflicts: Vec::new(),
            };
        }
    }
    match multi_edit_recover::recover(journal, io) {
        Ok(RecoveryResult::Settled { .. }) => PublishOutcome::RolledBack(reason),
        Ok(RecoveryResult::Conflicts { paths, .. }) => PublishOutcome::RecoveryPending {
            reason,
            recovery: None,
            conflicts: paths,
        },
        Ok(RecoveryResult::NoRecord) => PublishOutcome::RecoveryPending {
            reason,
            recovery: None,
            conflicts: Vec::new(),
        },
        Err(error) => PublishOutcome::RecoveryPending {
            reason,
            recovery: Some(error),
            conflicts: Vec::new(),
        },
    }
}

fn observe_all<B: RecoveryIo>(
    record: &RecoveryRecord,
    io: &mut B,
) -> Result<BTreeMap<String, Option<FileImage>>, PublishFailure<B::Error>> {
    let mut current = BTreeMap::new();
    let mut bytes = 0usize;
    for entry in record.entries() {
        let image = io.observe(&entry.path).map_err(PublishFailure::Io)?;
        bytes = bytes
            .checked_add(image.as_ref().map_or(0, |image| image.bytes.len()))
            .ok_or(PublishFailure::InvalidRecord)?;
        if bytes > MAX_RECOVERY_BYTES {
            return Err(PublishFailure::InvalidRecord);
        }
        current.insert(entry.path.clone(), image);
    }
    Ok(current)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::multi_edit_recovery::RecoveryEntry;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rapid-publish-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
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
    fn record() -> RecoveryRecord {
        RecoveryRecord::new(
            RecoveryState::Intent,
            vec![
                RecoveryEntry {
                    path: "a".into(),
                    before: image("a0"),
                    after: image("a1"),
                },
                RecoveryEntry {
                    path: "b".into(),
                    before: image("b0"),
                    after: image("b1"),
                },
            ],
        )
        .unwrap()
    }
    struct Fake {
        files: BTreeMap<String, Option<FileImage>>,
        prepared: bool,
        published: usize,
        fail_at: Option<usize>,
        fail_after_effect: bool,
        conflict_at: Option<usize>,
        mutate_first_at: Option<usize>,
        cancel_after: Option<(usize, CancellationToken)>,
        cancel_before_commit: Option<CancellationToken>,
        fail_journaled_after: Option<usize>,
        skip_registration: bool,
        uncertain_registration: bool,
        fail_settlement: bool,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                files: BTreeMap::from([("a".into(), image("a0")), ("b".into(), image("b0"))]),
                prepared: false,
                published: 0,
                fail_at: None,
                fail_after_effect: false,
                conflict_at: None,
                mutate_first_at: None,
                cancel_after: None,
                cancel_before_commit: None,
                fail_journaled_after: None,
                skip_registration: false,
                uncertain_registration: false,
                fail_settlement: false,
            }
        }
    }
    impl RecoveryIo for Fake {
        type Error = &'static str;
        fn observe(&mut self, path: &str) -> Result<Option<FileImage>, Self::Error> {
            Ok(self.files.get(path).cloned().unwrap_or(None))
        }
        fn restore_if_matches(
            &mut self,
            path: &str,
            expected: &Option<FileImage>,
            before: &Option<FileImage>,
        ) -> Result<bool, Self::Error> {
            if self.files.get(path) != Some(expected) {
                return Ok(false);
            }
            self.files.insert(path.into(), before.clone());
            Ok(true)
        }
        fn before_settlement(&mut self, _: &RecoveryRecord) -> Result<(), Self::Error> {
            if self.fail_settlement {
                Err("invalidation")
            } else {
                Ok(())
            }
        }
    }
    impl PublishIo for Fake {
        fn before_commit(&mut self, _: &RecoveryRecord) -> Result<(), Self::Error> {
            if let Some(cancel) = &self.cancel_before_commit {
                cancel.cancel();
            }
            Ok(())
        }
        fn prepare(&mut self, _: &RecoveryRecord) -> Result<(), Self::Error> {
            self.prepared = true;
            Ok(())
        }
        fn publish_if_matches(
            &mut self,
            path: &str,
            expected: &Option<FileImage>,
            after: &Option<FileImage>,
        ) -> Result<bool, Self::Error> {
            assert!(self.prepared);
            if self.mutate_first_at == Some(self.published) {
                self.files.insert("a".into(), image("external"));
            }
            if self.conflict_at == Some(self.published) {
                return Ok(false);
            }
            if self.fail_at == Some(self.published) && !self.fail_after_effect {
                return Err("publish");
            }
            if self.files.get(path) != Some(expected) {
                return Ok(false);
            }
            self.files.insert(path.into(), after.clone());
            self.published += 1;
            if let Some((at, token)) = &self.cancel_after
                && *at == self.published
            {
                token.cancel();
            }
            if self.fail_after_effect && self.fail_at == Some(self.published - 1) {
                return Err("publish");
            }
            Ok(true)
        }
    }

    impl JournaledPublishIo for Fake {
        fn prepare_journaled(
            &mut self,
            journal: &JournalStore,
            record: &mut RecoveryRecord,
        ) -> Result<(), JournaledPrepareError<Self::Error>> {
            use crate::multi_edit_recovery::StageIdentity;
            assert_eq!(journal.load().unwrap().as_ref(), Some(&*record));
            self.prepared = true;
            if self.skip_registration {
                return Ok(());
            }
            for index in 0..record.entries().len() {
                if self.fail_journaled_after == Some(index) {
                    return Err(JournaledPrepareError::Io("prepare"));
                }
                if record.staging_name(index).is_none() {
                    continue;
                }
                let next = journal
                    .register_stage(
                        record,
                        index,
                        StageIdentity {
                            device: 1,
                            inode: index as u64 + 1,
                        },
                    )
                    .map_err(JournaledPrepareError::Journal)?;
                if self.uncertain_registration {
                    return Err(JournaledPrepareError::Journal(JournalError::Uncertain));
                }
                *record = next;
            }
            Ok(())
        }
    }

    fn planned_record() -> RecoveryRecord {
        record()
            .with_staging_plan("0123456789abcdef0123456789abcdef".into())
            .unwrap()
    }

    #[test]
    fn journal_first_registers_every_stage_before_publishing() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let mut io = Fake::new();
        assert_eq!(
            publish_journal_first(&journal, &mut io, &planned_record(), || false),
            PublishOutcome::Committed
        );
        assert_eq!(io.files["a"], image("a1"));
        assert_eq!(io.files["b"], image("b1"));
        assert_eq!(journal.load().unwrap(), None);
    }

    #[test]
    fn journal_first_rolls_back_partial_or_missing_registration() {
        for skip in [false, true] {
            let temp = Temp::new();
            let journal = JournalStore::lock(&temp.0).unwrap();
            let mut io = Fake::new();
            io.skip_registration = skip;
            if !skip {
                io.fail_journaled_after = Some(1);
            }
            let expected = if skip {
                PublishFailure::InvalidRecord
            } else {
                PublishFailure::Io("prepare")
            };
            assert_eq!(
                publish_journal_first(&journal, &mut io, &planned_record(), || false),
                PublishOutcome::RolledBack(expected)
            );
            assert_eq!(io.files["a"], image("a0"));
            assert_eq!(io.files["b"], image("b0"));
            assert_eq!(journal.load().unwrap(), None);
        }
    }

    #[test]
    fn uncertain_registration_retains_the_newer_durable_intent() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let mut io = Fake::new();
        io.uncertain_registration = true;
        assert!(matches!(
            publish_journal_first(&journal, &mut io, &planned_record(), || false),
            PublishOutcome::RecoveryPending {
                reason: PublishFailure::Journal(JournalError::Uncertain),
                ..
            }
        ));
        assert!(journal.load().unwrap().unwrap().stage_identity(0).is_some());
        assert_eq!(io.files["a"], image("a0"));
    }
    #[test]
    fn preimage_conflict_rejects_without_staging_or_journal() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let mut io = Fake::new();
        io.files.insert("b".into(), image("changed"));
        assert_eq!(
            publish(&journal, &mut io, &record(), &CancellationToken::new()),
            PublishOutcome::Rejected(PublishFailure::Conflict { path: "b".into() })
        );
        assert!(!io.prepared);
        assert_eq!(io.published, 0);
        assert_eq!(journal.load().unwrap(), None);
    }
    #[test]
    fn commits_only_after_all_publication_and_settlement() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let mut io = Fake::new();
        assert_eq!(
            publish(&journal, &mut io, &record(), &CancellationToken::new()),
            PublishOutcome::Committed
        );
        assert_eq!(io.files["a"], image("a1"));
        assert_eq!(io.files["b"], image("b1"));
        assert_eq!(journal.load().unwrap(), None);
    }
    #[test]
    fn failure_after_a_published_effect_rolls_back_every_changed_file() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let mut io = Fake::new();
        io.fail_at = Some(1);
        io.fail_after_effect = true;
        assert_eq!(
            publish(&journal, &mut io, &record(), &CancellationToken::new()),
            PublishOutcome::RolledBack(PublishFailure::Io("publish"))
        );
        assert_eq!(io.files["a"], image("a0"));
        assert_eq!(io.files["b"], image("b0"));
        assert_eq!(journal.load().unwrap(), None);
    }
    #[test]
    fn cancellation_after_first_publication_rolls_back() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let mut io = Fake::new();
        let cancel = CancellationToken::new();
        io.cancel_after = Some((1, cancel.clone()));
        assert_eq!(
            publish(&journal, &mut io, &record(), &cancel),
            PublishOutcome::RolledBack(PublishFailure::Cancelled)
        );
        assert_eq!(io.files["a"], image("a0"));
        assert_eq!(io.files["b"], image("b0"));
        assert_eq!(journal.load().unwrap(), None);
    }
    #[test]
    fn cancellation_during_final_commit_check_rolls_back() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let mut io = Fake::new();
        let cancel = CancellationToken::new();
        io.cancel_before_commit = Some(cancel.clone());
        assert_eq!(
            publish_with_cancel(&journal, &mut io, &record(), || cancel.is_cancelled()),
            PublishOutcome::RolledBack(PublishFailure::Cancelled)
        );
        assert_eq!(io.files["a"], image("a0"));
        assert_eq!(io.files["b"], image("b0"));
        assert_eq!(journal.load().unwrap(), None);
    }
    #[test]
    fn cancelled_before_start_does_not_stage_or_write_a_journal() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let mut io = Fake::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            publish(&journal, &mut io, &record(), &cancel),
            PublishOutcome::Rejected(PublishFailure::Cancelled)
        );
        assert!(!io.prepared);
        assert_eq!(journal.load().unwrap(), None);
    }
    #[test]
    fn rollback_invalidation_failure_retains_intent_for_retry() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let mut io = Fake::new();
        io.fail_at = Some(1);
        io.fail_settlement = true;
        assert_eq!(
            publish(&journal, &mut io, &record(), &CancellationToken::new()),
            PublishOutcome::RecoveryPending {
                reason: PublishFailure::Io("publish"),
                recovery: Some(RecoverError::Io("invalidation")),
                conflicts: vec![],
            }
        );
        assert_eq!(io.files["a"], image("a0"));
        assert_eq!(io.files["b"], image("b0"));
        assert_eq!(journal.load().unwrap(), Some(record()));
        io.fail_settlement = false;
        assert_eq!(
            multi_edit_recover::recover(&journal, &mut io).unwrap(),
            RecoveryResult::Settled { restored: 0 }
        );
    }
    #[test]
    fn external_change_during_failed_publication_keeps_journal_and_conflict() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let mut io = Fake::new();
        io.mutate_first_at = Some(1);
        io.conflict_at = Some(1);
        assert_eq!(
            publish(&journal, &mut io, &record(), &CancellationToken::new()),
            PublishOutcome::RecoveryPending {
                reason: PublishFailure::Conflict { path: "b".into() },
                recovery: None,
                conflicts: vec!["a".into()]
            }
        );
        assert_eq!(io.files["a"], image("external"));
        assert_eq!(journal.load().unwrap(), Some(record()));
    }
    #[test]
    fn committed_cleanup_failure_never_rolls_back_the_commit() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let mut io = Fake::new();
        io.fail_settlement = true;
        assert_eq!(
            publish(&journal, &mut io, &record(), &CancellationToken::new()),
            PublishOutcome::CommittedCleanupPending {
                recovery: Some(RecoverError::Io("invalidation")),
                conflicts: vec![]
            }
        );
        assert_eq!(
            journal.load().unwrap().unwrap().state(),
            RecoveryState::Committed
        );
        assert_eq!(io.files["a"], image("a1"));
        assert_eq!(io.files["b"], image("b1"));
        io.fail_settlement = false;
        assert_eq!(
            multi_edit_recover::recover(&journal, &mut io).unwrap(),
            RecoveryResult::Settled { restored: 0 }
        );
    }

    /// Test-only filesystem adapter; it is deliberately not suitable for
    /// product use because it does not stage or atomically replace targets.
    struct FixtureIo {
        root: PathBuf,
        pause_after_first: bool,
    }
    impl RecoveryIo for FixtureIo {
        type Error = &'static str;
        fn observe(&mut self, path: &str) -> Result<Option<FileImage>, Self::Error> {
            let path = self.root.join(path);
            let metadata = fs::symlink_metadata(&path).map_err(|_| "stat")?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err("type");
            }
            Ok(Some(FileImage {
                bytes: fs::read(path).map_err(|_| "read")?,
                mode: Some(metadata.permissions().mode() & 0o7777),
            }))
        }
        fn restore_if_matches(
            &mut self,
            path: &str,
            expected: &Option<FileImage>,
            before: &Option<FileImage>,
        ) -> Result<bool, Self::Error> {
            if &self.observe(path)? != expected {
                return Ok(false);
            }
            self.write(path, before)?;
            Ok(true)
        }
    }
    impl FixtureIo {
        fn write(&self, path: &str, image: &Option<FileImage>) -> Result<(), &'static str> {
            use std::io::Write;
            let image = image.as_ref().ok_or("missing")?;
            let mut file = fs::File::create(self.root.join(path)).map_err(|_| "create")?;
            file.set_permissions(fs::Permissions::from_mode(image.mode.ok_or("mode")?))
                .map_err(|_| "mode")?;
            file.write_all(&image.bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| "write")?;
            fs::File::open(&self.root)
                .and_then(|dir| dir.sync_all())
                .map_err(|_| "sync")?;
            Ok(())
        }
    }
    impl PublishIo for FixtureIo {
        fn prepare(&mut self, _: &RecoveryRecord) -> Result<(), Self::Error> {
            Ok(())
        }
        fn publish_if_matches(
            &mut self,
            path: &str,
            expected: &Option<FileImage>,
            after: &Option<FileImage>,
        ) -> Result<bool, Self::Error> {
            if &self.observe(path)? != expected {
                return Ok(false);
            }
            self.write(path, after)?;
            if self.pause_after_first {
                use std::io::Write;
                println!("FIRST_EFFECT_SYNCED");
                std::io::stdout().flush().unwrap();
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
            Ok(true)
        }
    }
    #[test]
    fn child_publish_probe() {
        let Some(root) = std::env::var_os("RAPID_TEST_PUBLISH_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let journal = JournalStore::lock(&root.join("journal")).unwrap();
        let mut io = FixtureIo {
            root,
            pause_after_first: true,
        };
        let _ = publish(&journal, &mut io, &record(), &CancellationToken::new());
    }
    #[test]
    fn process_kill_after_first_effect_keeps_intent_for_restart_recovery() {
        use std::io::BufRead;
        let temp = Temp::new();
        fs::create_dir(temp.0.join("journal")).unwrap();
        fs::set_permissions(temp.0.join("journal"), fs::Permissions::from_mode(0o700)).unwrap();
        for (path, text) in [("a", "a0"), ("b", "b0")] {
            fs::write(temp.0.join(path), text).unwrap();
            fs::set_permissions(temp.0.join(path), fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "multi_edit_publish::tests::child_publish_probe",
                "--nocapture",
            ])
            .env("RAPID_TEST_PUBLISH_ROOT", &temp.0)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if line.contains("FIRST_EFFECT_SYNCED") {
                    let _ = tx.send(());
                    break;
                }
            }
        });
        let ready = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = child.kill();
        let status = child.wait().unwrap();
        assert!(ready.is_ok(), "child did not publish its first effect");
        assert!(!status.success());
        assert_eq!(fs::read(temp.0.join("a")).unwrap(), b"a1");
        assert_eq!(fs::read(temp.0.join("b")).unwrap(), b"b0");
        let journal = JournalStore::lock(&temp.0.join("journal")).unwrap();
        assert_eq!(journal.load().unwrap(), Some(record()));
        let mut io = FixtureIo {
            root: temp.0.clone(),
            pause_after_first: false,
        };
        assert_eq!(
            multi_edit_recover::recover(&journal, &mut io).unwrap(),
            RecoveryResult::Settled { restored: 1 }
        );
        assert_eq!(fs::read(temp.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(temp.0.join("b")).unwrap(), b"b0");
        assert_eq!(journal.load().unwrap(), None);
    }
}
