//! Durable storage for one validated multi-edit recovery record.
//!
//! The caller must provide an existing private directory whose ancestors and
//! contents cannot be replaced by untrusted processes. This type does not
//! confine workspace paths or apply recovery actions. Its lock coordinates
//! only participants that use this journal and is held until drop.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::multi_edit_recovery::{
    FileImage, MAX_RECOVERY_JSON_BYTES, RecoveryAction, RecoveryRecord, RecoveryState,
    StageIdentity,
};

const RECORD: &str = "active.json";
const LOCK: &str = "journal.lock";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalError {
    InvalidDirectory,
    Busy,
    Io,
    Corrupt,
    Exists,
    Conflict,
    Unsupported,
    /// Publication or removal happened, but its directory sync failed. The
    /// caller must reopen and inspect rather than assume either outcome.
    Uncertain,
}

pub struct JournalStore {
    directory: PathBuf,
    _lock: File,
}

/// Shared guard used by ordinary writes; concurrent guards can coexist, but
/// an active transaction or recovery requires the exclusive journal lock.
pub struct JournalReadGuard {
    directory: PathBuf,
    _lock: File,
}

impl JournalReadGuard {
    pub fn lock(directory: &Path) -> Result<Self, JournalError> {
        let (directory, file) = lock_directory(directory, true)?;
        Ok(Self {
            directory,
            _lock: file,
        })
    }

    pub fn load(&self) -> Result<Option<RecoveryRecord>, JournalError> {
        load_record(&self.directory)
    }
}

impl JournalStore {
    /// The canonical directory bound to this lock and record.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Lock an existing, trusted private directory. This is nonblocking.
    pub fn lock(directory: &Path) -> Result<Self, JournalError> {
        let (directory, file) = lock_directory(directory, false)?;
        Ok(Self {
            directory,
            _lock: file,
        })
    }

    pub fn load(&self) -> Result<Option<RecoveryRecord>, JournalError> {
        load_record(&self.directory)
    }

    /// Persist an empty staged file's identity before any candidate bytes are
    /// written. An uncertain return requires reopening and inspecting intent.
    pub fn register_stage(
        &self,
        expected: &RecoveryRecord,
        index: usize,
        identity: StageIdentity,
    ) -> Result<RecoveryRecord, JournalError> {
        if self.load()?.as_ref() != Some(expected) {
            return Err(JournalError::Conflict);
        }
        let next = expected
            .register_stage(index, identity)
            .map_err(|_| JournalError::Conflict)?;
        let temp = self.write_temp(&next)?;
        if fs::rename(&temp, self.directory.join(RECORD)).is_err() {
            let _ = fs::remove_file(&temp);
            return Err(JournalError::Io);
        }
        sync_dir(&self.directory).map_err(|_| JournalError::Uncertain)?;
        Ok(next)
    }

    /// Publish a new intent without replacing an existing record. Do not
    /// modify workspace files until this returns success.
    pub fn write_intent(&self, record: &RecoveryRecord) -> Result<(), JournalError> {
        if record.state() != RecoveryState::Intent {
            return Err(JournalError::Conflict);
        }
        let temp = self.write_temp(record)?;
        let result = fs::hard_link(&temp, self.directory.join(RECORD));
        if let Err(err) = result {
            let _ = fs::remove_file(&temp);
            return Err(if err.kind() == std::io::ErrorKind::AlreadyExists {
                JournalError::Exists
            } else {
                JournalError::Io
            });
        }
        let sync = sync_dir(&self.directory);
        let _ = fs::remove_file(&temp);
        sync.map_err(|_| JournalError::Uncertain)
    }

    /// Atomically replace exactly the expected intent with its committed
    /// record. Callers must hold workspace locks and validate postimages.
    pub fn write_committed(
        &self,
        expected: &RecoveryRecord,
        committed: &RecoveryRecord,
    ) -> Result<(), JournalError> {
        if !committed.is_commit_of(expected) {
            return Err(JournalError::Conflict);
        }
        if expected.has_staging_plan()
            && expected.entries().iter().enumerate().any(|(index, _)| {
                expected.staging_name(index).is_some() && expected.stage_identity(index).is_none()
            })
        {
            return Err(JournalError::Conflict);
        }
        if self.load()?.as_ref() != Some(expected) {
            return Err(JournalError::Conflict);
        }
        let temp = self.write_temp(committed)?;
        if fs::rename(&temp, self.directory.join(RECORD)).is_err() {
            let _ = fs::remove_file(&temp);
            return Err(JournalError::Io);
        }
        sync_dir(&self.directory).map_err(|_| JournalError::Uncertain)
    }

    /// Remove a record only when every observed image is settled in the
    /// corresponding state. Callers must hold locks through removal.
    pub fn remove_settled(
        &self,
        expected: &RecoveryRecord,
        current: &BTreeMap<String, Option<FileImage>>,
    ) -> Result<(), JournalError> {
        if self.load()?.as_ref() != Some(expected) {
            return Err(JournalError::Conflict);
        }
        let actions = expected.plan(current).map_err(|_| JournalError::Conflict)?;
        if actions.iter().any(|action| {
            !matches!(
                action,
                RecoveryAction::AlreadyOriginal { .. } | RecoveryAction::Committed { .. }
            )
        }) {
            return Err(JournalError::Conflict);
        }
        fs::remove_file(self.directory.join(RECORD)).map_err(|_| JournalError::Io)?;
        sync_dir(&self.directory).map_err(|_| JournalError::Uncertain)
    }

    fn write_temp(&self, record: &RecoveryRecord) -> Result<PathBuf, JournalError> {
        let bytes = record.encode().map_err(|_| JournalError::Corrupt)?;
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = self
            .directory
            .join(format!(".journal-{}-{id}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|_| JournalError::Io)?;
        let result = file.write_all(&bytes).and_then(|_| file.sync_all());
        if result.is_err() {
            let _ = fs::remove_file(&path);
            return Err(JournalError::Io);
        }
        Ok(path)
    }
}

fn lock_directory(directory: &Path, shared: bool) -> Result<(PathBuf, File), JournalError> {
    if !cfg!(unix) {
        return Err(JournalError::Unsupported);
    }
    let metadata = fs::symlink_metadata(directory).map_err(|_| JournalError::InvalidDirectory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(JournalError::InvalidDirectory);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(JournalError::InvalidDirectory);
        }
    }
    let directory =
        protocol::host_path::canonicalize(directory).map_err(|_| JournalError::InvalidDirectory)?;
    let lock_path = directory.join(LOCK);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(meta) = fs::symlink_metadata(&lock_path)
        && (meta.file_type().is_symlink() || !meta.is_file())
    {
        return Err(JournalError::InvalidDirectory);
    }
    let file = options.open(&lock_path).map_err(|_| JournalError::Io)?;
    if shared {
        file.try_lock_shared().map_err(|_| JournalError::Busy)?;
    } else {
        file.try_lock().map_err(|_| JournalError::Busy)?;
    }
    let path_meta = fs::symlink_metadata(&lock_path).map_err(|_| JournalError::InvalidDirectory)?;
    if path_meta.file_type().is_symlink() || !path_meta.is_file() {
        return Err(JournalError::InvalidDirectory);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let file_meta = file.metadata().map_err(|_| JournalError::Io)?;
        if path_meta.dev() != file_meta.dev() || path_meta.ino() != file_meta.ino() {
            return Err(JournalError::InvalidDirectory);
        }
    }
    Ok((directory, file))
}

fn load_record(directory: &Path) -> Result<Option<RecoveryRecord>, JournalError> {
    let path = directory.join(RECORD);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(JournalError::Io),
    };
    if meta.file_type().is_symlink()
        || !meta.is_file()
        || meta.len() > MAX_RECOVERY_JSON_BYTES as u64
    {
        return Err(JournalError::Corrupt);
    }
    let file = File::open(path).map_err(|_| JournalError::Io)?;
    let mut bytes = Vec::new();
    file.take(MAX_RECOVERY_JSON_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| JournalError::Io)?;
    RecoveryRecord::decode(&bytes)
        .map(Some)
        .map_err(|_| JournalError::Corrupt)
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}
#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "directory sync is unsupported",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::multi_edit_recovery::{RecoveryEntry, RecoveryError};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("rapid-journal-{}-{id}", std::process::id()));
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
    fn intent() -> RecoveryRecord {
        RecoveryRecord::new(
            RecoveryState::Intent,
            vec![RecoveryEntry {
                path: "a.txt".into(),
                before: image("before"),
                after: image("after"),
            }],
        )
        .unwrap()
    }

    #[test]
    fn child_journal_probe() {
        let Some(path) = std::env::var_os("RAPID_TEST_JOURNAL_PROBE") else {
            return;
        };
        match std::env::var("RAPID_TEST_JOURNAL_ACTION").unwrap().as_str() {
            "busy" => assert!(matches!(
                JournalStore::lock(Path::new(&path)),
                Err(JournalError::Busy)
            )),
            "write" => JournalStore::lock(Path::new(&path))
                .unwrap()
                .write_intent(&intent())
                .unwrap(),
            "write_wait" => {
                let journal = JournalStore::lock(Path::new(&path)).unwrap();
                journal.write_intent(&intent()).unwrap();
                println!("JOURNAL_DURABLE");
                std::io::stdout().flush().unwrap();
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
            _ => panic!("unknown probe"),
        }
    }

    #[test]
    fn lock_and_intent_are_shared_across_processes() {
        let temp = Temp::new();
        let run = |action: &str| {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "multi_edit_journal::tests::child_journal_probe",
                    "--nocapture",
                ])
                .env("RAPID_TEST_JOURNAL_PROBE", &temp.0)
                .env("RAPID_TEST_JOURNAL_ACTION", action)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        };
        let journal = JournalStore::lock(&temp.0).unwrap();
        run("busy");
        drop(journal);
        let read_guard = JournalReadGuard::lock(&temp.0).unwrap();
        run("busy");
        drop(read_guard);
        run("write");
        let journal = JournalStore::lock(&temp.0).unwrap();
        assert_eq!(journal.load().unwrap(), Some(intent()));
    }

    #[test]
    fn intent_survives_process_kill_and_lock_is_released() {
        use std::io::BufRead;
        let temp = Temp::new();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "multi_edit_journal::tests::child_journal_probe",
                "--nocapture",
            ])
            .env("RAPID_TEST_JOURNAL_PROBE", &temp.0)
            .env("RAPID_TEST_JOURNAL_ACTION", "write_wait")
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
                if line.contains("JOURNAL_DURABLE") {
                    let _ = tx.send(());
                    break;
                }
            }
        });
        let ready = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = child.kill();
        let status = child.wait().unwrap();
        assert!(ready.is_ok(), "child did not finish writing the intent");
        assert!(!status.success());
        let journal = JournalStore::lock(&temp.0).unwrap();
        assert_eq!(journal.load().unwrap(), Some(intent()));
    }

    #[test]
    fn shared_guards_can_coexist_but_exclude_a_transaction() {
        let temp = Temp::new();
        let first = JournalReadGuard::lock(&temp.0).unwrap();
        let second = JournalReadGuard::lock(&temp.0).unwrap();
        assert_eq!(first.load().unwrap(), None);
        assert_eq!(second.load().unwrap(), None);
        assert!(matches!(
            JournalStore::lock(&temp.0),
            Err(JournalError::Busy)
        ));
        drop(first);
        drop(second);
        let exclusive = JournalStore::lock(&temp.0).unwrap();
        assert!(matches!(
            JournalReadGuard::lock(&temp.0),
            Err(JournalError::Busy)
        ));
        drop(exclusive);
    }

    #[test]
    fn records_survive_reopen_and_are_removed_only_after_settlement() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        assert_eq!(journal.load().unwrap(), None);
        let record = intent();
        journal.write_intent(&record).unwrap();
        assert_eq!(journal.write_intent(&record), Err(JournalError::Exists));
        assert!(matches!(
            JournalStore::lock(&temp.0),
            Err(JournalError::Busy)
        ));
        drop(journal);
        let journal = JournalStore::lock(&temp.0).unwrap();
        assert_eq!(journal.load().unwrap(), Some(record.clone()));
        let external = BTreeMap::from([("a.txt".into(), image("external"))]);
        assert_eq!(
            journal.remove_settled(&record, &external),
            Err(JournalError::Conflict)
        );
        let original = BTreeMap::from([("a.txt".into(), image("before"))]);
        journal.remove_settled(&record, &original).unwrap();
        assert_eq!(journal.load().unwrap(), None);
    }

    #[test]
    fn stage_identity_is_durable_and_requires_the_exact_previous_intent() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let original = intent()
            .with_staging_plan("0123456789abcdef0123456789abcdef".into())
            .unwrap();
        journal.write_intent(&original).unwrap();
        let unregistered_commit = original
            .committed(&BTreeMap::from([("a.txt".into(), image("after"))]))
            .unwrap();
        assert_eq!(
            journal.write_committed(&original, &unregistered_commit),
            Err(JournalError::Conflict)
        );
        let identity = StageIdentity {
            device: 7,
            inode: 42,
        };
        let registered = journal.register_stage(&original, 0, identity).unwrap();
        assert_eq!(registered.stage_identity(0), Some(identity));
        assert!(matches!(
            journal.register_stage(&original, 0, identity),
            Err(JournalError::Conflict)
        ));
        assert!(matches!(
            journal.register_stage(&registered, 0, identity),
            Err(JournalError::Conflict)
        ));
        let old_commit = original
            .committed(&BTreeMap::from([("a.txt".into(), image("after"))]))
            .unwrap();
        assert_eq!(
            journal.write_committed(&registered, &old_commit),
            Err(JournalError::Conflict)
        );
        drop(journal);
        let journal = JournalStore::lock(&temp.0).unwrap();
        assert_eq!(journal.load().unwrap(), Some(registered.clone()));
        let commit = registered
            .committed(&BTreeMap::from([("a.txt".into(), image("after"))]))
            .unwrap();
        journal.write_committed(&registered, &commit).unwrap();
        assert_eq!(journal.load().unwrap(), Some(commit));
    }

    #[test]
    fn committed_record_survives_reopen_and_conflicts_are_not_cleared() {
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        let record = intent();
        journal.write_intent(&record).unwrap();
        let current = BTreeMap::from([("a.txt".into(), image("after"))]);
        let committed = record.committed(&current).unwrap();
        journal.write_committed(&record, &committed).unwrap();
        assert_eq!(
            journal.write_committed(&record, &committed),
            Err(JournalError::Conflict)
        );
        drop(journal);
        let journal = JournalStore::lock(&temp.0).unwrap();
        assert_eq!(journal.load().unwrap(), Some(committed.clone()));
        let changed = BTreeMap::from([("a.txt".into(), image("other"))]);
        assert_eq!(
            journal.remove_settled(&committed, &changed),
            Err(JournalError::Conflict)
        );
        journal.remove_settled(&committed, &current).unwrap();
        assert_eq!(journal.load().unwrap(), None);
        assert_eq!(
            record.committed(&BTreeMap::new()),
            Err(RecoveryError::MissingObservation)
        );
    }

    #[test]
    fn corrupt_and_symlink_records_are_never_treated_as_missing() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        let journal = JournalStore::lock(&temp.0).unwrap();
        fs::write(temp.0.join(RECORD), b"partial").unwrap();
        assert_eq!(journal.load(), Err(JournalError::Corrupt));
        fs::remove_file(temp.0.join(RECORD)).unwrap();
        symlink(temp.0.join("outside"), temp.0.join(RECORD)).unwrap();
        assert_eq!(journal.load(), Err(JournalError::Corrupt));
        assert_eq!(journal.write_intent(&intent()), Err(JournalError::Exists));
    }

    #[test]
    fn journal_rejects_a_directory_accessible_to_other_users() {
        use std::os::unix::fs::PermissionsExt;
        let temp = Temp::new();
        fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            JournalStore::lock(&temp.0),
            Err(JournalError::InvalidDirectory)
        ));
    }
}
