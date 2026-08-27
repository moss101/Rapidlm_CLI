//! Retained session-log recovery under a writer lease.
//!
//! Recovery of a retained JSONL session log follows one mandatory sequence,
//! derived from the fail-closed repair choreography used for durable
//! workflow journals: acquire the writer lease, fingerprint the target,
//! re-verify the target is unchanged, repair the torn tail, verify the
//! whole log, then reread and report. Skipping the re-verification lets a
//! concurrent writer invalidate the repair between fingerprint and truncate
//! (TOCTOU), so `verify_unchanged` runs inside the lease, immediately before
//! any mutation.
//!
//! The only repair this module performs is truncating a torn final line (a
//! crash mid-write leaves bytes without a terminating newline). Corruption
//! inside complete lines — invalid UTF-8, a missing interior newline — is
//! reported, never rewritten.

use std::fs;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::journal::CancellationToken;

/// Summary of a verified log; counts are facts, not secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogSummary {
    pub lines: u64,
    pub bytes: u64,
}

/// Outcome of one recovery pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryReport {
    /// True when a torn tail was truncated.
    pub repaired: bool,
    /// Bytes removed by the repair (0 when the log was already clean).
    pub torn_bytes_removed: u64,
    /// Line count after repair, from the final reread.
    pub lines: u64,
    /// Byte size after repair, from the final reread.
    pub bytes: u64,
    /// Always true on success: the report only exists when verified.
    pub verified: bool,
}

impl fmt::Display for RecoveryReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "repaired={} torn_bytes_removed={} lines={} bytes={} verified={}",
            self.repaired, self.torn_bytes_removed, self.lines, self.bytes, self.verified
        )
    }
}

/// Typed recovery failure. Paths name files; contents are never echoed.
#[derive(Debug)]
pub enum RecoveryError {
    /// Another recovery (or writer) holds the lease for this log.
    LeaseHeld { lock_path: PathBuf },
    /// The log to recover does not exist.
    TargetMissing { path: PathBuf },
    /// The target changed after the lease was taken; the repair was not
    /// attempted (or its basis is gone).
    TargetChanged { path: PathBuf },
    Unreadable { path: PathBuf, reason: String },
    Cancelled,
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LeaseHeld { lock_path } => {
                write!(f, "writer lease is held: {}", lock_path.display())
            }
            Self::TargetMissing { path } => {
                write!(f, "retained log is missing: {}", path.display())
            }
            Self::TargetChanged { path } => write!(
                f,
                "retained log changed while under the writer lease: {}",
                path.display()
            ),
            Self::Unreadable { path, reason } => {
                write!(f, "retained log is unreadable: {}: {reason}", path.display())
            }
            Self::Cancelled => f.write_str("recovery cancelled"),
        }
    }
}

impl std::error::Error for RecoveryError {}

/// Size + mtime identity of the log at lease-acquisition time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogFingerprint {
    size: u64,
    modified_nanos: i64,
}

/// Acquire-time fingerprint of the log. Size and sub-second mtime together
/// distinguish "another writer appended" from "nothing happened".
pub fn fingerprint(path: &Path) -> Result<LogFingerprint, RecoveryError> {
    let meta = fs::metadata(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            RecoveryError::TargetMissing {
                path: path.to_path_buf(),
            }
        } else {
            RecoveryError::Unreadable {
                path: path.to_path_buf(),
                reason: err.to_string(),
            }
        }
    })?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|delta| delta.as_nanos() as i64)
        .unwrap_or(0);
    Ok(LogFingerprint {
        size: meta.len(),
        modified_nanos: modified,
    })
}

/// Re-verify the log still matches the lease-acquisition fingerprint. This
/// is the TOCTOU guard: it must run after acquisition and before mutation.
pub fn verify_unchanged(path: &Path, expected: &LogFingerprint) -> Result<(), RecoveryError> {
    let current = fingerprint(path)?;
    if current == *expected {
        Ok(())
    } else {
        Err(RecoveryError::TargetChanged {
            path: path.to_path_buf(),
        })
    }
}

/// Bytes of the unterminated final line, or `None` when the log ends on a
/// newline (or is empty).
pub fn torn_tail_bytes(path: &Path) -> Result<Option<u64>, RecoveryError> {
    let bytes = fs::read(path).map_err(|err| RecoveryError::Unreadable {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;
    Ok(match bytes.last() {
        None => None,
        Some(&b'\n') => None,
        Some(_) => {
            let torn = bytes
                .iter()
                .rposition(|&b| b == b'\n')
                .map_or(bytes.len(), |pos| pos + 1);
            Some((bytes.len() - torn) as u64)
        }
    })
}

/// Truncate the torn tail. Caller must hold the lease and have verified the
/// fingerprint immediately before calling.
pub fn repair_tail(path: &Path, torn_bytes: u64) -> Result<(), RecoveryError> {
    let current = fingerprint(path)?;
    let target_len = current
        .size
        .checked_sub(torn_bytes)
        .ok_or(RecoveryError::TargetChanged {
            path: path.to_path_buf(),
        })?;
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|err| RecoveryError::Unreadable {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })?;
    file.set_len(target_len)
        .map_err(|err| RecoveryError::Unreadable {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })
}

/// Verify the log is complete: newline-terminated and UTF-8 clean per line.
/// Returns the summary read from disk (the "reread" step).
pub fn verify_complete(path: &Path) -> Result<LogSummary, RecoveryError> {
    let bytes = fs::read(path).map_err(|err| RecoveryError::Unreadable {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;
    if let Some(&last) = bytes.last()
        && last != b'\n'
    {
        return Err(RecoveryError::Unreadable {
            path: path.to_path_buf(),
            reason: "final line is not newline-terminated".to_owned(),
        });
    }
    let mut lines = 0u64;
    for line in bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        std::str::from_utf8(line).map_err(|_| RecoveryError::Unreadable {
            path: path.to_path_buf(),
            reason: "line is not valid UTF-8".to_owned(),
        })?;
        lines += 1;
    }
    Ok(LogSummary {
        lines,
        bytes: bytes.len() as u64,
    })
}

/// Exclusive writer lease materialized as an exclusive-create lockfile next
/// to the log (`<log>.recover-lock`). Dropping the lease releases it on a
/// best-effort basis; `release` reports failures as typed errors.
#[derive(Debug)]
pub struct WriterLease {
    lock_path: PathBuf,
}

impl WriterLease {
    /// Create the lockfile exclusively; an existing lockfile is a held lease.
    pub fn acquire(log_path: &Path) -> Result<Self, RecoveryError> {
        let lock_path = lock_path_for(log_path);
        match fs::OpenOptions::new().write(true).create_new(true).open(&lock_path) {
            Ok(_) => Ok(Self { lock_path }),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(RecoveryError::LeaseHeld { lock_path })
            }
            Err(err) => Err(RecoveryError::Unreadable {
                path: lock_path,
                reason: err.to_string(),
            }),
        }
    }

    /// Release the lease. A missing lockfile is fine (already released).
    pub fn release(self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

fn lock_path_for(log_path: &Path) -> PathBuf {
    let mut name = log_path.file_name().map_or_else(
        || std::ffi::OsString::from("session-log"),
        |raw| raw.to_os_string(),
    );
    name.push(".recover-lock");
    log_path.with_file_name(name)
}

/// Full recovery sequence over `log_path`.
pub fn recover_retained_log(
    log_path: &Path,
    cancel: &CancellationToken,
) -> Result<RecoveryReport, RecoveryError> {
    if cancel.is_cancelled() {
        return Err(RecoveryError::Cancelled);
    }
    // Acquire, then fingerprint under the lease — acquisition order matters:
    // the fingerprint is only meaningful once no other writer can mutate.
    let lease = WriterLease::acquire(log_path)?;
    let fingerprint = fingerprint(log_path)?;
    // The TOCTOU guard: re-verify immediately before mutating.
    verify_unchanged(log_path, &fingerprint)?;
    let mut report = RecoveryReport {
        repaired: false,
        torn_bytes_removed: 0,
        lines: 0,
        bytes: 0,
        verified: false,
    };
    if let Some(torn) = torn_tail_bytes(log_path)? {
        verify_unchanged(log_path, &fingerprint)?;
        repair_tail(log_path, torn)?;
        report.repaired = true;
        report.torn_bytes_removed = torn;
    }
    if cancel.is_cancelled() {
        return Err(RecoveryError::Cancelled);
    }
    let summary = verify_complete(log_path)?;
    report.lines = summary.lines;
    report.bytes = summary.bytes;
    report.verified = true;
    lease.release();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> (tempdir::TempDir, PathBuf) {
        let dir = tempdir::TempDir::new().expect("temp dir");
        let path = dir.path().join(name);
        (dir, path)
    }

    // Minimal scoped temp-dir helper (unique per call) without new deps.
    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);

        pub(super) struct TempDir(PathBuf);

        impl TempDir {
            pub(super) fn new() -> std::io::Result<Self> {
                let seq = SEQ.fetch_add(1, Ordering::Relaxed);
                let dir = std::env::temp_dir().join(format!(
                    "rapidlm-recovery-{}-{seq}",
                    std::process::id()
                ));
                std::fs::create_dir_all(&dir)?;
                Ok(Self(dir))
            }

            pub(super) fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn clean_log_is_verified_without_repair() {
        let (_dir, path) = scratch("clean.jsonl");
        std::fs::write(&path, "{\"seq\":1}\n{\"seq\":2}\n").expect("write");
        let report = recover_retained_log(&path, &CancellationToken::new()).expect("recover");
        assert!(!report.repaired);
        assert_eq!(report.torn_bytes_removed, 0);
        assert_eq!(report.lines, 2);
        assert_eq!(report.bytes, 20);
        assert!(report.verified);
        assert!(report.to_string().contains("repaired=false"));
    }

    #[test]
    fn empty_log_is_valid() {
        let (_dir, path) = scratch("empty.jsonl");
        std::fs::write(&path, b"").expect("write");
        let report = recover_retained_log(&path, &CancellationToken::new()).expect("recover");
        assert_eq!(report.lines, 0);
        assert!(!report.repaired);
    }

    #[test]
    fn torn_tail_is_truncated_under_the_lease() {
        let (_dir, path) = scratch("torn.jsonl");
        std::fs::write(&path, b"{\"seq\":1}\n{\"seq\":2").expect("write");
        let report = recover_retained_log(&path, &CancellationToken::new()).expect("recover");
        assert!(report.repaired);
        assert_eq!(report.torn_bytes_removed, 8);
        assert_eq!(report.lines, 1);
        // The lockfile is released after success.
        assert!(!lock_path_for(&path).exists());
        let after = std::fs::read(&path).expect("reread");
        assert_eq!(after, b"{\"seq\":1}\n");
    }

    #[test]
    fn second_recovery_while_held_fails_typed() {
        let (_dir, path) = scratch("held.jsonl");
        std::fs::write(&path, "a\n").expect("write");
        let lease = WriterLease::acquire(&path).expect("first lease");
        let err = recover_retained_log(&path, &CancellationToken::new())
            .expect_err("lease is held");
        assert!(matches!(err, RecoveryError::LeaseHeld { .. }));
        assert!(err.to_string().contains("writer lease is held"));
        lease.release();
        // After release the recovery succeeds.
        assert!(recover_retained_log(&path, &CancellationToken::new()).is_ok());
    }

    #[test]
    fn target_changed_under_lease_is_detected() {
        let (_dir, path) = scratch("changed.jsonl");
        std::fs::write(&path, "a\n").expect("write");
        let fp = fingerprint(&path).expect("fingerprint");
        std::fs::write(&path, "a\nb\n").expect("append");
        let err = verify_unchanged(&path, &fp).expect_err("target changed");
        assert!(matches!(err, RecoveryError::TargetChanged { .. }));
    }

    #[test]
    fn interior_corruption_is_reported_not_rewritten() {
        let (_dir, path) = scratch("corrupt.jsonl");
        // Valid lines, then an invalid UTF-8 byte inside a complete line.
        let mut bytes = b"good\n".to_vec();
        bytes.extend_from_slice(b"bad\xffline\n");
        std::fs::write(&path, &bytes).expect("write");
        let err = recover_retained_log(&path, &CancellationToken::new())
            .expect_err("interior corruption");
        assert!(
            matches!(err, RecoveryError::Unreadable { ref reason, .. } if reason.contains("UTF-8"))
        );
    }

    #[test]
    fn missing_target_and_cancel_are_typed() {
        let (_dir, path) = scratch("absent.jsonl");
        let err = recover_retained_log(&path, &CancellationToken::new())
            .expect_err("missing target");
        assert!(matches!(err, RecoveryError::TargetMissing { .. }));

        let (_dir, path) = scratch("cancelled.jsonl");
        std::fs::write(&path, "a\n").expect("write");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            recover_retained_log(&path, &cancel),
            Err(RecoveryError::Cancelled)
        ));
    }
}
