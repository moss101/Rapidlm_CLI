//! Durable, fail-closed formatter execution checkpoints.
//!
//! Operations require the exclusive workspace journal lock. A durable `running`
//! marker is written before starting external commands. An interrupted run is
//! intentionally uncertain and must not be executed again automatically.
//! The journal's private-directory and cooperating-writer assumptions apply.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};
use workspace::multi_edit_journal::JournalStore;
use workspace::multi_edit_recovery::{MAX_RECOVERY_JSON_BYTES, RecoveryRecord, RecoveryState};

use super::publication::FormattedMultiCandidate;

const RUNNING: &[u8] = b"rapidlm.multi-format.running.v1\n";
const RETIRED: &[u8] = b"rapidlm.multi-format.retired.v1\n";
const MAX_CHECKPOINT_BYTES: usize = MAX_RECOVERY_JSON_BYTES + 32 * 1024;
const MAX_NOTES_BYTES: usize = 8 * 1024 + super::TRUNCATION_MARKER.len();

/// Avoid scanning the ledger when only tombstones or running markers remain.
pub(super) fn has_reconciliation_candidates(root: &Path) -> Result<bool, CheckpointError> {
    let directory = root.join(super::recovery::JOURNAL_PATH);
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(CheckpointError::Io),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || super::has_symlink_component(root, super::recovery::JOURNAL_PATH)
    {
        return Err(CheckpointError::Invalid);
    }
    for (count, entry) in fs::read_dir(directory)
        .map_err(|_| CheckpointError::Io)?
        .enumerate()
    {
        if count >= 4096 {
            // The directory may contain a ready checkpoint beyond the scan
            // bound. Conservatively consult the session ledger; direct-name
            // checkpoints can then be resolved without enumerating it again.
            return Ok(true);
        }
        let entry = entry.map_err(|_| CheckpointError::Io)?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let hook_pending = name.starts_with("hook-") && name.ends_with(".pending");
        if !name.starts_with("format-") && !hook_pending {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|_| CheckpointError::Io)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(CheckpointError::Invalid);
        }
        if hook_pending {
            return Ok(true);
        }
        let file = File::open(entry.path()).map_err(|_| CheckpointError::Io)?;
        let opened = file.metadata().map_err(|_| CheckpointError::Io)?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(CheckpointError::Invalid);
        }
        let mut prefix = Vec::new();
        file.take((RUNNING.len().max(RETIRED.len()) + 1) as u64)
            .read_to_end(&mut prefix)
            .map_err(|_| CheckpointError::Io)?;
        if prefix != RUNNING && prefix != RETIRED {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum CheckpointError {
    Invalid,
    Io,
    Exists,
    Uncertain,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum CheckpointState {
    Missing,
    Running,
    Retired,
    Ready(FormattedMultiCandidate),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    schema: String,
    version: u8,
    key: String,
    #[serde(default)]
    formatter_binding: String,
    record: Option<RecoveryRecord>,
    original_indices: Vec<usize>,
    notes: String,
}

/// A checkpoint identity is generated from a pre-format approval chain, never
/// from a path supplied by a model or user.
pub(super) struct FormatCheckpoint<'a> {
    journal: &'a JournalStore,
    key: String,
    filename_key: String,
}

impl<'a> FormatCheckpoint<'a> {
    pub(super) fn new(journal: &'a JournalStore, key: &str) -> Result<Self, CheckpointError> {
        if key.len() != 96
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(CheckpointError::Invalid);
        }
        // Earlier versions used the full candidate key in the filename. A
        // marker or temporary file in either layout is authority: never treat
        // an interrupted legacy execution as a fresh call.
        let nonce = &key[..32];
        let present = |name: &str| -> Result<bool, CheckpointError> {
            match fs::symlink_metadata(journal.directory().join(name)) {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(_) => Err(CheckpointError::Io),
            }
        };
        let legacy =
            present(&format!("format-{key}.json"))? || present(&format!(".format-{key}.tmp"))?;
        let direct =
            present(&format!("format-{nonce}.json"))? || present(&format!(".format-{nonce}.tmp"))?;
        if legacy && direct {
            return Err(CheckpointError::Uncertain);
        }
        Ok(Self {
            journal,
            key: key.to_owned(),
            filename_key: if legacy {
                key.to_owned()
            } else {
                nonce.to_owned()
            },
        })
    }

    fn path(&self) -> std::path::PathBuf {
        self.journal
            .directory()
            .join(format!("format-{}.json", self.filename_key))
    }

    fn temp_path(&self) -> std::path::PathBuf {
        self.journal
            .directory()
            .join(format!(".format-{}.tmp", self.filename_key))
    }

    /// Must complete successfully before executing any formatter. A failure
    /// after file creation is uncertain and never permits an automatic retry.
    pub(super) fn start(&self) -> Result<(), CheckpointError> {
        match fs::symlink_metadata(self.temp_path()) {
            Ok(_) => return Err(CheckpointError::Uncertain),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(CheckpointError::Io),
        }
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.path())
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(CheckpointError::Exists);
            }
            Err(_) => return Err(CheckpointError::Io),
        };
        file.write_all(RUNNING)
            .and_then(|()| file.sync_all())
            .map_err(|_| CheckpointError::Uncertain)?;
        sync_dir(self.journal.directory()).map_err(|_| CheckpointError::Uncertain)
    }

    /// Store one complete candidate atomically. On an uncertain failure the
    /// marker and any temporary file remain; they are never blindly retried.
    pub(super) fn finish(
        &self,
        raw: &RecoveryRecord,
        result: &FormattedMultiCandidate,
        formatter_binding: &str,
    ) -> Result<(), CheckpointError> {
        validate(raw, result)?;
        if formatter_binding.len() != 64 || !formatter_binding.bytes().all(is_lower_hex) {
            return Err(CheckpointError::Invalid);
        }
        if !matches!(self.load(raw)?, CheckpointState::Running) {
            return Err(CheckpointError::Invalid);
        }
        let stored = Stored {
            schema: "rapidlm.multi_format_checkpoint".into(),
            version: 1,
            key: self.key.clone(),
            formatter_binding: formatter_binding.to_owned(),
            record: result.record.clone(),
            original_indices: result.original_indices.clone(),
            notes: result.notes.clone(),
        };
        let bytes = serde_json::to_vec(&stored).map_err(|_| CheckpointError::Invalid)?;
        if bytes.len() > MAX_CHECKPOINT_BYTES {
            return Err(CheckpointError::Invalid);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.temp_path())
            .map_err(|_| CheckpointError::Uncertain)?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| CheckpointError::Uncertain)?;
        fs::rename(self.temp_path(), self.path()).map_err(|_| CheckpointError::Uncertain)?;
        sync_dir(self.journal.directory()).map_err(|_| CheckpointError::Uncertain)
    }

    pub(super) fn load(&self, raw: &RecoveryRecord) -> Result<CheckpointState, CheckpointError> {
        let Some(bytes) = self.read_bytes()? else {
            return Ok(CheckpointState::Missing);
        };
        if bytes == RUNNING {
            return Ok(CheckpointState::Running);
        }
        if bytes == RETIRED {
            return Ok(CheckpointState::Retired);
        }
        let stored = self.decode_stored(&bytes)?;
        let result = FormattedMultiCandidate {
            record: stored.record,
            original_indices: stored.original_indices,
            notes: stored.notes,
        };
        validate(raw, &result)?;
        Ok(CheckpointState::Ready(result))
    }

    fn read_bytes(&self) -> Result<Option<Vec<u8>>, CheckpointError> {
        match fs::symlink_metadata(self.temp_path()) {
            Ok(_) => return Err(CheckpointError::Uncertain),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(CheckpointError::Io),
        }
        let path = self.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(_) => return Err(CheckpointError::Io),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.len() > MAX_CHECKPOINT_BYTES as u64
        {
            return Err(CheckpointError::Invalid);
        }
        let file = File::open(&path).map_err(|_| CheckpointError::Io)?;
        let opened = file.metadata().map_err(|_| CheckpointError::Io)?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(CheckpointError::Invalid);
        }
        let mut bytes = Vec::new();
        file.take(MAX_CHECKPOINT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| CheckpointError::Io)?;
        if bytes.len() > MAX_CHECKPOINT_BYTES {
            return Err(CheckpointError::Invalid);
        }
        Ok(Some(bytes))
    }

    fn decode_stored(&self, bytes: &[u8]) -> Result<Stored, CheckpointError> {
        let stored: Stored = serde_json::from_slice(bytes).map_err(|_| CheckpointError::Invalid)?;
        if stored.schema != "rapidlm.multi_format_checkpoint"
            || stored.version != 1
            || stored.key != self.key
        {
            return Err(CheckpointError::Invalid);
        }
        Ok(stored)
    }

    /// Retain a content-free tombstone after using or explicitly rejecting the
    /// exact result. This prevents replay from executing the formatter again.
    /// Running or uncertain executions cannot be retired through this method.
    pub(super) fn retire_ready(
        &self,
        raw: &RecoveryRecord,
        expected: &FormattedMultiCandidate,
    ) -> Result<(), CheckpointError> {
        match fs::symlink_metadata(self.temp_path()) {
            Ok(_) => return Err(CheckpointError::Uncertain),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(CheckpointError::Io),
        }
        if self.load(raw)?
            != CheckpointState::Ready(FormattedMultiCandidate {
                record: expected.record.clone(),
                original_indices: expected.original_indices.clone(),
                notes: expected.notes.clone(),
            })
        {
            return Err(CheckpointError::Invalid);
        }
        self.write_retired()
    }

    /// Only the live coordinator that started the command may call this,
    /// after the formatter invocation has returned without a candidate. A
    /// recovered running marker must remain uncertain and cannot use it.
    pub(super) fn retire_after_failed_execution(
        &self,
        raw: &RecoveryRecord,
    ) -> Result<(), CheckpointError> {
        if self.load(raw)? != CheckpointState::Running {
            return Err(CheckpointError::Invalid);
        }
        self.write_retired()
    }

    fn write_retired(&self) -> Result<(), CheckpointError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.temp_path())
            .map_err(|_| CheckpointError::Uncertain)?;
        file.write_all(RETIRED)
            .and_then(|()| file.sync_all())
            .map_err(|_| CheckpointError::Uncertain)?;
        fs::rename(self.temp_path(), self.path()).map_err(|_| CheckpointError::Uncertain)?;
        sync_dir(self.journal.directory()).map_err(|_| CheckpointError::Uncertain)
    }
}

fn validate(raw: &RecoveryRecord, result: &FormattedMultiCandidate) -> Result<(), CheckpointError> {
    if raw.state() != RecoveryState::Intent
        || raw.has_staging_plan()
        || raw.encode().is_err()
        || result.notes.len() > MAX_NOTES_BYTES
    {
        return Err(CheckpointError::Invalid);
    }
    let entries = match &result.record {
        Some(record) => {
            if record.state() != RecoveryState::Intent
                || record.has_staging_plan()
                || record.encode().is_err()
            {
                return Err(CheckpointError::Invalid);
            }
            record.entries()
        }
        None => &[],
    };
    if entries.len() != result.original_indices.len() {
        return Err(CheckpointError::Invalid);
    }
    let mut previous = None;
    for (entry, &index) in entries.iter().zip(&result.original_indices) {
        if previous.is_some_and(|prior| prior >= index) {
            return Err(CheckpointError::Invalid);
        }
        let original = raw.entries().get(index).ok_or(CheckpointError::Invalid)?;
        if entry.path != original.path
            || entry.before != original.before
            || entry.after.is_some() != original.after.is_some()
            || entry.after.as_ref().map(|image| image.mode)
                != original.after.as_ref().map(|image| image.mode)
            || entry.after.as_ref().is_some_and(|image| {
                image.bytes.len() > super::MAX_FILE_READ_BYTES
                    || std::str::from_utf8(&image.bytes).is_err()
            })
        {
            return Err(CheckpointError::Invalid);
        }
        previous = Some(index);
    }
    // Only an edit of an existing file can disappear as a formatter no-op.
    for (index, entry) in raw.entries().iter().enumerate() {
        if !result.original_indices.contains(&index)
            && (entry.before.is_none() || entry.after.is_none())
        {
            return Err(CheckpointError::Invalid);
        }
    }
    Ok(())
}

/// Retire the checkpoint only when the durable denied request exactly matches
/// its final-phase preview. The nonce identifies the private file even when
/// the working tree or current formatter configuration has since changed.
pub(super) fn retire_denied(
    journal: &JournalStore,
    request: &kernel::ApprovalRequestedPayload,
    arguments: &str,
) -> Result<bool, CheckpointError> {
    use super::multi_approval::{ApprovalPhase, PathApprovalChain};
    if request.tool != super::WORKSPACE_MULTI_PATCH_TOOL
        || request.arguments_digest.as_deref()
            != Some(crate::approvals::arguments_digest(arguments).as_str())
    {
        return Ok(false);
    }
    let Some(source) = request.source.as_deref() else {
        return Ok(false);
    };
    let pieces: Vec<_> = source.split(':').collect();
    if pieces.len() != 5
        || pieces[0] != "multi-edit"
        || pieces[1] != "v1"
        || pieces[2].len() != 32
        || !pieces[2].bytes().all(is_lower_hex)
        || pieces[3].len() != 64
        || !pieces[3].bytes().all(is_lower_hex)
    {
        return Ok(false);
    }
    let index: usize = pieces[4].parse().map_err(|_| CheckpointError::Invalid)?;
    if index.to_string() != pieces[4] {
        return Err(CheckpointError::Invalid);
    }
    let nonce = pieces[2];
    let direct = FormatCheckpoint {
        journal,
        key: String::new(),
        filename_key: nonce.to_owned(),
    };
    let mut found = match direct.read_bytes()? {
        Some(bytes) if bytes == RETIRED => return Ok(false),
        Some(bytes) if bytes == RUNNING => return Err(CheckpointError::Uncertain),
        Some(bytes) => {
            let stored: Stored =
                serde_json::from_slice(&bytes).map_err(|_| CheckpointError::Invalid)?;
            if stored.key.len() != 96
                || !stored.key.starts_with(nonce)
                || !stored.key.bytes().all(is_lower_hex)
            {
                return Err(CheckpointError::Invalid);
            }
            Some(stored.key)
        }
        None => None,
    };
    // Only older checkpoints need directory enumeration. New ones are found
    // directly even when the journal contains thousands of tombstones.
    if found.is_none() {
        let prefix = format!("format-{nonce}");
        for (count, entry) in fs::read_dir(journal.directory())
            .map_err(|_| CheckpointError::Io)?
            .enumerate()
        {
            if count >= 4096 {
                return Err(CheckpointError::Uncertain);
            }
            let entry = entry.map_err(|_| CheckpointError::Io)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(key) = name
                .strip_prefix("format-")
                .and_then(|name| name.strip_suffix(".json"))
                && name.starts_with(&prefix)
                && key.len() == 96
                && key.bytes().all(is_lower_hex)
                && found.replace(key.to_owned()).is_some()
            {
                return Err(CheckpointError::Invalid);
            }
        }
    }
    let Some(key) = found else { return Ok(false) };
    let checkpoint = FormatCheckpoint::new(journal, &key)?;
    let Some(bytes) = checkpoint.read_bytes()? else {
        return Ok(false);
    };
    if bytes == RETIRED {
        return Ok(false);
    }
    if bytes == RUNNING {
        return Err(CheckpointError::Uncertain);
    }
    let stored = checkpoint.decode_stored(&bytes)?;
    let record = stored.record.as_ref().ok_or(CheckpointError::Invalid)?;
    if record.state() != RecoveryState::Intent
        || record.has_staging_plan()
        || record.encode().is_err()
    {
        return Err(CheckpointError::Invalid);
    }
    let approved = crate::approvals::ApprovedAsk::from_payload(request);
    let chain = PathApprovalChain::from_resolved_for_phase(
        record,
        &request.call_id,
        arguments,
        &approved,
        ApprovalPhase::AfterFormat,
        &stored.formatter_binding,
    )
    .map_err(|_| CheckpointError::Invalid)?;
    let expected = chain
        .request(record, index, arguments)
        .map_err(|_| CheckpointError::Invalid)?;
    if request.call_id != expected.call_id
        || request.tool != expected.tool
        || request.summary != expected.summary
        || request.scope != expected.scope
        || request.diff != expected.diff
        || request.source != expected.source
        || request.arguments_digest != expected.arguments_digest
        || request.remember_as.is_some()
    {
        return Err(CheckpointError::Invalid);
    }
    checkpoint.write_retired()?;
    Ok(true)
}

fn sync_dir(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    use workspace::multi_edit_recovery::{FileImage, RecoveryEntry};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rapid-format-checkpoint-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn lock(&self) -> JournalStore {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
            loop {
                match JournalStore::lock(&self.0) {
                    Ok(journal) => return journal,
                    Err(workspace::multi_edit_journal::JournalError::Busy)
                        if std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => panic!("journal lock failed: {error:?}"),
                }
            }
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn image(bytes: &str) -> Option<FileImage> {
        Some(FileImage {
            bytes: bytes.as_bytes().to_vec(),
            mode: Some(0o600),
        })
    }
    fn raw() -> RecoveryRecord {
        RecoveryRecord::new(
            RecoveryState::Intent,
            vec![
                RecoveryEntry {
                    path: "a".into(),
                    before: image("old"),
                    after: image("new"),
                },
                RecoveryEntry {
                    path: "b".into(),
                    before: image("old-b"),
                    after: None,
                },
            ],
        )
        .unwrap()
    }
    fn formatted() -> FormattedMultiCandidate {
        let mut entries = raw().entries().to_vec();
        entries[0].after = image("NEW");
        FormattedMultiCandidate {
            record: Some(RecoveryRecord::new(RecoveryState::Intent, entries).unwrap()),
            original_indices: vec![0, 1],
            notes: "formatted".into(),
        }
    }
    fn key() -> String {
        "a".repeat(96)
    }

    #[test]
    fn checkpoint_survives_reopen_and_never_restarts_a_running_or_finished_command() {
        let root = Temp::new();
        {
            let journal = root.lock();
            let checkpoint = FormatCheckpoint::new(&journal, &key()).unwrap();
            assert_eq!(checkpoint.load(&raw()), Ok(CheckpointState::Missing));
            checkpoint.start().unwrap();
            assert_eq!(checkpoint.start(), Err(CheckpointError::Exists));
            assert_eq!(checkpoint.load(&raw()), Ok(CheckpointState::Running));
        }
        {
            let journal = root.lock();
            let checkpoint = FormatCheckpoint::new(&journal, &key()).unwrap();
            assert_eq!(checkpoint.load(&raw()), Ok(CheckpointState::Running));
            checkpoint
                .finish(&raw(), &formatted(), &"b".repeat(64))
                .unwrap();
            assert_eq!(checkpoint.start(), Err(CheckpointError::Exists));
        }
        let journal = root.lock();
        let checkpoint = FormatCheckpoint::new(&journal, &key()).unwrap();
        assert_eq!(
            checkpoint.load(&raw()),
            Ok(CheckpointState::Ready(formatted()))
        );
        assert_eq!(
            checkpoint.finish(&raw(), &formatted(), &"b".repeat(64)),
            Err(CheckpointError::Invalid)
        );
        assert_eq!(
            fs::metadata(checkpoint.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let mut wrong = formatted();
        wrong.notes = "different".into();
        assert_eq!(
            checkpoint.retire_ready(&raw(), &wrong),
            Err(CheckpointError::Invalid)
        );
        checkpoint.retire_ready(&raw(), &formatted()).unwrap();
        assert_eq!(checkpoint.load(&raw()), Ok(CheckpointState::Retired));
        assert_eq!(checkpoint.start(), Err(CheckpointError::Exists));
        assert_eq!(fs::read(checkpoint.path()).unwrap(), RETIRED);
    }

    #[test]
    fn malformed_or_rebound_results_are_refused_before_the_marker_is_replaced() {
        let root = Temp::new();
        let journal = root.lock();
        let checkpoint = FormatCheckpoint::new(&journal, &key()).unwrap();
        checkpoint.start().unwrap();
        let mut bad = formatted();
        bad.original_indices.swap(0, 1);
        assert_eq!(
            checkpoint.finish(&raw(), &bad, &"b".repeat(64)),
            Err(CheckpointError::Invalid)
        );
        let mut entries = formatted().record.unwrap().entries().to_vec();
        entries[0].before = image("other");
        let bad = FormattedMultiCandidate {
            record: Some(RecoveryRecord::new(RecoveryState::Intent, entries).unwrap()),
            original_indices: vec![0, 1],
            notes: String::new(),
        };
        assert_eq!(
            checkpoint.finish(&raw(), &bad, &"b".repeat(64)),
            Err(CheckpointError::Invalid)
        );
        let bad = FormattedMultiCandidate {
            record: None,
            original_indices: vec![],
            notes: String::new(),
        };
        assert_eq!(
            checkpoint.finish(&raw(), &bad, &"b".repeat(64)),
            Err(CheckpointError::Invalid)
        );
        assert_eq!(checkpoint.load(&raw()), Ok(CheckpointState::Running));
    }

    #[test]
    fn incomplete_temporary_files_and_symlinks_fail_closed() {
        let root = Temp::new();
        let journal = root.lock();
        let checkpoint = FormatCheckpoint::new(&journal, &key()).unwrap();
        symlink("missing", checkpoint.temp_path()).unwrap();
        assert_eq!(checkpoint.start(), Err(CheckpointError::Uncertain));
        assert_eq!(checkpoint.load(&raw()), Err(CheckpointError::Uncertain));
        fs::remove_file(checkpoint.temp_path()).unwrap();
        symlink("missing", checkpoint.path()).unwrap();
        assert_eq!(checkpoint.start(), Err(CheckpointError::Exists));
        assert_eq!(checkpoint.load(&raw()), Err(CheckpointError::Invalid));
        assert!(FormatCheckpoint::new(&journal, "../../other").is_err());
    }

    #[test]
    fn corrupted_or_world_readable_checkpoints_are_never_used() {
        let root = Temp::new();
        let journal = root.lock();
        let checkpoint = FormatCheckpoint::new(&journal, &key()).unwrap();
        checkpoint.start().unwrap();
        checkpoint
            .finish(&raw(), &formatted(), &"b".repeat(64))
            .unwrap();
        fs::set_permissions(checkpoint.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(checkpoint.load(&raw()), Err(CheckpointError::Invalid));
        fs::set_permissions(checkpoint.path(), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(checkpoint.path(), b"{broken").unwrap();
        assert_eq!(checkpoint.load(&raw()), Err(CheckpointError::Invalid));
    }

    #[test]
    fn denied_cleanup_requires_the_exact_final_preview_and_is_idempotent() {
        use super::super::multi_approval::{ApprovalPhase, PathApprovalChain};
        let root = Temp::new();
        let journal = root.lock();
        let binding = "b".repeat(64);
        let args = "{}";
        let before = PathApprovalChain::new_before_format(&raw(), "call", args, &binding).unwrap();
        let key = before.formatter_checkpoint_key().unwrap();
        let checkpoint = FormatCheckpoint::new(&journal, &key).unwrap();
        checkpoint.start().unwrap();
        checkpoint.finish(&raw(), &formatted(), &binding).unwrap();
        let candidate = formatted().record.unwrap();
        let final_chain = before
            .for_phase(&candidate, args, ApprovalPhase::AfterFormat)
            .unwrap();
        let expected = final_chain.request(&candidate, 0, args).unwrap();
        let payload: kernel::ApprovalRequestedPayload = serde_json::from_value(serde_json::json!({
            "id":"denied", "call_id":expected.call_id, "tool":expected.tool,
            "summary":expected.summary, "scope":expected.scope, "diff":expected.diff,
            "source":expected.source, "arguments_digest":expected.arguments_digest
        }))
        .unwrap();
        let mut tampered = payload.clone();
        tampered.diff.push_str("tampered");
        assert_eq!(
            retire_denied(&journal, &tampered, args),
            Err(CheckpointError::Invalid)
        );
        assert_eq!(
            checkpoint.load(&raw()),
            Ok(CheckpointState::Ready(formatted()))
        );
        assert_eq!(retire_denied(&journal, &payload, "changed"), Ok(false));
        assert_eq!(retire_denied(&journal, &payload, args), Ok(true));
        assert_eq!(checkpoint.load(&raw()), Ok(CheckpointState::Retired));
        assert_eq!(retire_denied(&journal, &payload, args), Ok(false));
    }

    #[test]
    fn legacy_checkpoint_paths_resume_and_conflicting_layouts_fail_closed() {
        let root = Temp::new();
        let journal = root.lock();
        let checkpoint = FormatCheckpoint::new(&journal, &key()).unwrap();
        checkpoint.start().unwrap();
        let direct = checkpoint.path();
        let legacy = journal.directory().join(format!("format-{}.json", key()));
        fs::rename(&direct, &legacy).unwrap();
        let checkpoint = FormatCheckpoint::new(&journal, &key()).unwrap();
        assert_eq!(checkpoint.load(&raw()), Ok(CheckpointState::Running));
        checkpoint
            .finish(&raw(), &formatted(), &"b".repeat(64))
            .unwrap();
        assert_eq!(
            checkpoint.load(&raw()),
            Ok(CheckpointState::Ready(formatted()))
        );
        fs::write(&direct, RETIRED).unwrap();
        assert!(matches!(
            FormatCheckpoint::new(&journal, &key()),
            Err(CheckpointError::Uncertain)
        ));
        fs::remove_file(&direct).unwrap();
        fs::remove_file(&legacy).unwrap();
        fs::write(
            journal.directory().join(format!(".format-{}.tmp", key())),
            b"partial",
        )
        .unwrap();
        let checkpoint = FormatCheckpoint::new(&journal, &key()).unwrap();
        assert_eq!(checkpoint.load(&raw()), Err(CheckpointError::Uncertain));
        assert_eq!(checkpoint.start(), Err(CheckpointError::Uncertain));
    }

    #[test]
    fn denial_cleanup_accepts_an_existing_legacy_checkpoint() {
        use super::super::multi_approval::{ApprovalPhase, PathApprovalChain};
        let root = Temp::new();
        let journal = root.lock();
        let binding = "b".repeat(64);
        let args = "{}";
        let chain = PathApprovalChain::new_before_format(&raw(), "call", args, &binding).unwrap();
        let key = chain.formatter_checkpoint_key().unwrap();
        let checkpoint = FormatCheckpoint::new(&journal, &key).unwrap();
        checkpoint.start().unwrap();
        checkpoint.finish(&raw(), &formatted(), &binding).unwrap();
        fs::rename(
            checkpoint.path(),
            journal.directory().join(format!("format-{key}.json")),
        )
        .unwrap();
        let candidate = formatted().record.unwrap();
        let request = chain
            .for_phase(&candidate, args, ApprovalPhase::AfterFormat)
            .unwrap()
            .request(&candidate, 0, args)
            .unwrap();
        let payload: kernel::ApprovalRequestedPayload = serde_json::from_value(serde_json::json!({
            "id":"denied", "call_id":request.call_id, "tool":request.tool, "summary":request.summary,
            "scope":request.scope, "diff":request.diff, "source":request.source, "arguments_digest":request.arguments_digest
        })).unwrap();
        assert_eq!(retire_denied(&journal, &payload, args), Ok(true));
        assert_eq!(
            FormatCheckpoint::new(&journal, &key).unwrap().load(&raw()),
            Ok(CheckpointState::Retired)
        );
    }

    #[test]
    fn direct_denial_lookup_does_not_depend_on_directory_entry_count() {
        use super::super::multi_approval::{ApprovalPhase, PathApprovalChain};
        let root = Temp::new();
        let journal = root.lock();
        let binding = "b".repeat(64);
        let args = "{}";
        let chain = PathApprovalChain::new_before_format(&raw(), "call", args, &binding).unwrap();
        let checkpoint =
            FormatCheckpoint::new(&journal, &chain.formatter_checkpoint_key().unwrap()).unwrap();
        checkpoint.start().unwrap();
        checkpoint.finish(&raw(), &formatted(), &binding).unwrap();
        let candidate = formatted().record.unwrap();
        let request = chain
            .for_phase(&candidate, args, ApprovalPhase::AfterFormat)
            .unwrap()
            .request(&candidate, 0, args)
            .unwrap();
        let payload: kernel::ApprovalRequestedPayload = serde_json::from_value(serde_json::json!({
            "id":"denied", "call_id":request.call_id, "tool":request.tool, "summary":request.summary,
            "scope":request.scope, "diff":request.diff, "source":request.source, "arguments_digest":request.arguments_digest
        })).unwrap();
        for index in 0..4100 {
            fs::write(journal.directory().join(format!("unrelated-{index}")), b"").unwrap();
        }
        assert_eq!(retire_denied(&journal, &payload, args), Ok(true));
        assert_eq!(checkpoint.load(&raw()), Ok(CheckpointState::Retired));
    }

    #[test]
    fn checkpoint_crash_child() {
        let Some(path) = std::env::var_os("RAPID_TEST_FORMAT_CHECKPOINT_ROOT") else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        let journal = JournalStore::lock(&path).unwrap();
        let checkpoint = FormatCheckpoint::new(&journal, &key()).unwrap();
        checkpoint.start().unwrap();
        fs::write(path.join("child-ready"), b"running marker synced").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
        panic!("checkpoint crash test child was not killed");
    }

    #[test]
    fn killed_process_leaves_an_uncertain_running_checkpoint() {
        let root = Temp::new();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("exec_tools::multi_format_checkpoint::tests::checkpoint_crash_child")
            .arg("--nocapture")
            .env("RAPID_TEST_FORMAT_CHECKPOINT_ROOT", &root.0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !root.0.join("child-ready").exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("checkpoint child exited before the marker: {status}");
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("checkpoint child did not create its marker");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let journal = root.lock();
        let checkpoint = FormatCheckpoint::new(&journal, &key()).unwrap();
        assert_eq!(checkpoint.load(&raw()), Ok(CheckpointState::Running));
        assert_eq!(checkpoint.start(), Err(CheckpointError::Exists));
    }
}
