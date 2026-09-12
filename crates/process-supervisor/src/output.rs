//! Bounded stdout/stderr spool: inline excerpt then immutable artifacts.
//!
//! Overflow is staged with `create_new` (mode 0600) under a private directory
//! owned by [`ArtifactStore`], then published by digest. Caller cancellation
//! is forwarded into `ArtifactStore::put`; a cancelled finish does not publish.

use std::error::Error;
use std::fmt::{self, Debug};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use capability_broker::CancellationToken;
use event_ledger::artifact_store::{
    ArtifactError, ArtifactMetadata, ArtifactStore, CancellationToken as StoreCancel,
};
use protocol::{ArtifactRef, ErrorCode, RedactionClass};

/// Hard ceiling for [`OutputLimits::inline_excerpt_bytes`]. Larger values fail closed.
pub const MAX_INLINE_EXCERPT_BYTES: usize = 4 * 1024;

/// Read/write window. Callers may pass larger slices; they are consumed in windows.
const WRITE_WINDOW: usize = 8 * 1024;

/// Store-owned spill directory name. Not the OS temp dir.
const SPOOL_DIR: &str = "spool";

const SPOOL_SUFFIX: &str = ".spool";

const STDOUT_MEDIA: &str = "application/octet-stream";
const STDERR_MEDIA: &str = "application/octet-stream";

static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Per-stream capture ceilings. Disk is also clamped to [`ArtifactStore::max_bytes`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputLimits {
    pub inline_excerpt_bytes: usize,
    pub disk_cap_bytes: u64,
}

/// Which child stream a write/absorb targets.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Live dual-stream spool. Memory stays bounded by the inline limit.
pub struct OutputSpool {
    store: ArtifactStore,
    stdout: StreamSpool,
    stderr: StreamSpool,
}

/// Finished stdout/stderr references. Payload lives only in excerpts/artifacts.
#[derive(Clone, Eq, PartialEq)]
pub struct FinishedSpool {
    pub stdout: OutputRef,
    pub stderr: OutputRef,
}

/// Bounded view of one stream. Debug/Display omit payload bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct OutputRef {
    pub inline_excerpt: Vec<u8>,
    pub artifact: Option<ArtifactRef>,
    pub truncated: bool,
    pub cursor: OutputCursor,
}

/// Byte offset of captured output (min(seen, disk cap) when an artifact exists).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct OutputCursor {
    pub offset: u64,
}

/// Counters only. Never holds output bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OutputMetrics {
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub stdout_captured: u64,
    pub stderr_captured: u64,
    pub truncated: bool,
}

/// Typed spool failure. Display never echoes child output.
#[derive(Debug)]
pub enum OutputError {
    Cancelled,
    LimitInvalid,
    Artifact(ArtifactError),
    Io(io::Error),
}

struct StreamSpool {
    kind: OutputStream,
    excerpt: Vec<u8>,
    tail: Vec<u8>,
    inline_limit: usize,
    disk_cap: u64,
    seen: u64,
    captured: u64,
    spilled: u64,
    truncated: bool,
    spool_dir: PathBuf,
    spill: Option<SpillFile>,
}

struct SpillFile {
    path: PathBuf,
    file: File,
}

/// Forwards the caller token into the store token on every read.
struct CancelLinkedReader<R> {
    inner: R,
    caller: CancellationToken,
    store: StoreCancel,
}

impl OutputLimits {
    pub const fn new(inline_excerpt_bytes: usize, disk_cap_bytes: u64) -> Self {
        Self {
            inline_excerpt_bytes,
            disk_cap_bytes,
        }
    }
}

impl OutputSpool {
    pub fn new(store: ArtifactStore, limits: OutputLimits) -> Result<Self, OutputError> {
        if limits.inline_excerpt_bytes == 0
            || limits.inline_excerpt_bytes > MAX_INLINE_EXCERPT_BYTES
        {
            return Err(OutputError::LimitInvalid);
        }
        let disk_cap = limits.disk_cap_bytes.min(store.max_bytes());
        let spool_dir = store.root().join(SPOOL_DIR);
        ensure_private_dir(&spool_dir)?;
        Ok(Self {
            stdout: StreamSpool::new(
                OutputStream::Stdout,
                limits.inline_excerpt_bytes,
                disk_cap,
                spool_dir.clone(),
            ),
            stderr: StreamSpool::new(
                OutputStream::Stderr,
                limits.inline_excerpt_bytes,
                disk_cap,
                spool_dir,
            ),
            store,
        })
    }

    pub fn write(
        &mut self,
        stream: OutputStream,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<(), OutputError> {
        check_cancel(cancel)?;
        let dest = self.stream_mut(stream);
        dest.write(bytes, cancel)
    }

    pub fn write_stdout(
        &mut self,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.write(OutputStream::Stdout, bytes, cancel)
    }

    pub fn write_stderr(
        &mut self,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.write(OutputStream::Stderr, bytes, cancel)
    }

    pub fn absorb<R: Read>(
        &mut self,
        stream: OutputStream,
        mut reader: R,
        cancel: &CancellationToken,
    ) -> Result<(), OutputError> {
        check_cancel(cancel)?;
        let mut buf = [0u8; WRITE_WINDOW];
        loop {
            check_cancel(cancel)?;
            let n = reader.read(&mut buf).map_err(OutputError::Io)?;
            if n == 0 {
                return Ok(());
            }
            self.write(stream, &buf[..n], cancel)?;
        }
    }

    pub fn absorb_stdout<R: Read>(
        &mut self,
        reader: R,
        cancel: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.absorb(OutputStream::Stdout, reader, cancel)
    }

    pub fn absorb_stderr<R: Read>(
        &mut self,
        reader: R,
        cancel: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.absorb(OutputStream::Stderr, reader, cancel)
    }

    pub fn finish(self, cancel: &CancellationToken) -> Result<FinishedSpool, OutputError> {
        check_cancel(cancel)?;
        let store = self.store;
        let stdout = self.stdout.seal(&store, cancel)?;
        check_cancel(cancel)?;
        let stderr = self.stderr.seal(&store, cancel)?;
        Ok(FinishedSpool { stdout, stderr })
    }

    pub fn memory_bytes(&self) -> usize {
        self.stdout.memory_bytes() + self.stderr.memory_bytes()
    }

    pub fn metrics(&self) -> OutputMetrics {
        OutputMetrics {
            stdout_bytes: self.stdout.seen,
            stderr_bytes: self.stderr.seen,
            stdout_captured: self.stdout.captured,
            stderr_captured: self.stderr.captured,
            truncated: self.stdout.truncated || self.stderr.truncated,
        }
    }

    pub fn stdout_tail(&self) -> &[u8] {
        &self.stdout.tail
    }

    pub fn stderr_tail(&self) -> &[u8] {
        &self.stderr.tail
    }

    fn stream_mut(&mut self, stream: OutputStream) -> &mut StreamSpool {
        match stream {
            OutputStream::Stdout => &mut self.stdout,
            OutputStream::Stderr => &mut self.stderr,
        }
    }
}

impl StreamSpool {
    fn new(kind: OutputStream, inline_limit: usize, disk_cap: u64, spool_dir: PathBuf) -> Self {
        Self {
            kind,
            excerpt: Vec::new(),
            tail: Vec::new(),
            inline_limit,
            disk_cap,
            seen: 0,
            captured: 0,
            spilled: 0,
            truncated: false,
            spool_dir,
            spill: None,
        }
    }

    fn memory_bytes(&self) -> usize {
        self.excerpt.len() + self.tail.len()
    }

    fn write(&mut self, bytes: &[u8], cancel: &CancellationToken) -> Result<(), OutputError> {
        if bytes.is_empty() {
            return Ok(());
        }
        for window in bytes.chunks(WRITE_WINDOW) {
            check_cancel(cancel)?;
            self.write_window(window)?;
        }
        Ok(())
    }

    fn write_window(&mut self, bytes: &[u8]) -> Result<(), OutputError> {
        self.seen = self.seen.saturating_add(bytes.len() as u64);
        self.push_tail(bytes);

        let excerpt_room = self.inline_limit.saturating_sub(self.excerpt.len());
        if excerpt_room > 0 {
            let take = excerpt_room.min(bytes.len());
            self.excerpt.extend_from_slice(&bytes[..take]);
            self.captured = self.captured.saturating_add(take as u64);
            if take == bytes.len() {
                return Ok(());
            }
            return self.spill_bytes(&bytes[take..]);
        }
        self.spill_bytes(bytes)
    }

    fn spill_bytes(&mut self, bytes: &[u8]) -> Result<(), OutputError> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.disk_cap == 0 {
            self.truncated = true;
            return Ok(());
        }
        self.ensure_spill()?;
        if self.spilled == 0 && !self.excerpt.is_empty() {
            // Artifact holds the full captured prefix, not only the overflow tail.
            let prefix = self.excerpt.clone();
            if self.write_spill_raw(&prefix)? < prefix.len() {
                return Ok(());
            }
        }
        let wrote = self.write_spill_raw(bytes)?;
        self.captured = self.captured.saturating_add(wrote as u64);
        Ok(())
    }

    fn ensure_spill(&mut self) -> Result<(), OutputError> {
        if self.spill.is_some() {
            ensure_private_dir(&self.spool_dir)?;
            return Ok(());
        }
        ensure_private_dir(&self.spool_dir)?;
        let path = next_spill_path(&self.spool_dir);
        let file = open_exclusive_spill(&path)?;
        self.spill = Some(SpillFile { path, file });
        Ok(())
    }

    /// Writes `bytes` up to the remaining disk cap. Returns the number of bytes stored.
    fn write_spill_raw(&mut self, bytes: &[u8]) -> Result<usize, OutputError> {
        let remaining = self.disk_cap.saturating_sub(self.spilled);
        if remaining == 0 {
            self.truncated = true;
            return Ok(0);
        }
        let take = (bytes.len() as u64).min(remaining) as usize;
        let spill = self
            .spill
            .as_mut()
            .ok_or_else(|| OutputError::Io(io::Error::other("spill file missing")))?;
        spill
            .file
            .write_all(&bytes[..take])
            .map_err(OutputError::Io)?;
        self.spilled = self.spilled.saturating_add(take as u64);
        if take < bytes.len() {
            self.truncated = true;
        }
        Ok(take)
    }

    fn push_tail(&mut self, bytes: &[u8]) {
        if self.inline_limit == 0 {
            self.tail.clear();
            return;
        }
        if bytes.len() >= self.inline_limit {
            self.tail.clear();
            self.tail
                .extend_from_slice(&bytes[bytes.len() - self.inline_limit..]);
            return;
        }
        let overflow = self.tail.len() + bytes.len();
        if overflow > self.inline_limit {
            let drop = overflow - self.inline_limit;
            self.tail.drain(..drop);
        }
        self.tail.extend_from_slice(bytes);
    }

    fn seal(
        mut self,
        store: &ArtifactStore,
        cancel: &CancellationToken,
    ) -> Result<OutputRef, OutputError> {
        check_cancel(cancel)?;
        let excerpt = std::mem::take(&mut self.excerpt);
        let truncated = self.truncated;
        let artifact = match self.spill.take() {
            Some(mut spill) => {
                let refer = publish_spill(store, &mut spill, self.kind, cancel);
                drop_spill(&mut spill);
                Some(refer?)
            }
            None => None,
        };
        let offset = artifact
            .as_ref()
            .map(|refer| refer.bytes)
            .unwrap_or(excerpt.len() as u64);
        Ok(OutputRef {
            inline_excerpt: excerpt,
            artifact,
            truncated,
            cursor: OutputCursor { offset },
        })
    }
}

impl Drop for StreamSpool {
    fn drop(&mut self) {
        if let Some(mut spill) = self.spill.take() {
            drop_spill(&mut spill);
        }
    }
}

impl<R: Read> Read for CancelLinkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.caller.is_cancelled() {
            self.store.cancel();
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "output spool cancelled",
            ));
        }
        self.inner.read(buf)
    }
}

impl OutputError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "output spool cancelled",
            Self::LimitInvalid => "output spool limit is invalid",
            Self::Artifact(_) => "output spool artifact error",
            Self::Io(_) => "output spool io error",
        }
    }

    pub fn error_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::LimitInvalid => Some(ErrorCode::ToolInvalidArguments),
            Self::Artifact(ArtifactError::Cancelled) => None,
            Self::Artifact(ArtifactError::Integrity { .. } | ArtifactError::Metadata) => {
                Some(ErrorCode::StorageCorrupt)
            }
            Self::Artifact(ArtifactError::BoundExceeded { .. }) => {
                Some(ErrorCode::ToolInvalidArguments)
            }
            Self::Artifact(_) | Self::Io(_) => Some(ErrorCode::InternalUnexpected),
        }
    }
}

impl fmt::Display for OutputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for OutputError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Artifact(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::Cancelled | Self::LimitInvalid => None,
        }
    }
}

impl From<io::Error> for OutputError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl Debug for OutputSpool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputSpool")
            .field("stdout_bytes", &self.stdout.seen)
            .field("stderr_bytes", &self.stderr.seen)
            .field("stdout_captured", &self.stdout.captured)
            .field("stderr_captured", &self.stderr.captured)
            .field(
                "truncated",
                &(self.stdout.truncated || self.stderr.truncated),
            )
            .field("memory_bytes", &self.memory_bytes())
            .finish()
    }
}

impl Debug for FinishedSpool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FinishedSpool")
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .finish()
    }
}

impl Debug for OutputRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputRef")
            .field("excerpt_len", &self.inline_excerpt.len())
            .field("artifact", &self.artifact.as_ref().map(|refer| refer.id))
            .field("truncated", &self.truncated)
            .field("cursor", &self.cursor)
            .finish()
    }
}

impl fmt::Display for OutputRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OutputRef excerpt_len={} truncated={} cursor={}",
            self.inline_excerpt.len(),
            self.truncated,
            self.cursor.offset
        )
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), OutputError> {
    if cancel.is_cancelled() {
        Err(OutputError::Cancelled)
    } else {
        Ok(())
    }
}

fn ensure_private_dir(path: &Path) -> Result<(), OutputError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => validate_private_dir(&meta)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => create_private_dir(path)?,
        Err(err) => return Err(OutputError::Io(err)),
    }
    let meta = fs::symlink_metadata(path).map_err(OutputError::Io)?;
    validate_private_dir(&meta)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = meta.permissions();
        perms.set_mode(0o700);
        fs::set_permissions(path, perms).map_err(OutputError::Io)?;
        let again = fs::symlink_metadata(path).map_err(OutputError::Io)?;
        validate_private_dir(&again)?;
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<(), OutputError> {
    // Only the Unix mode bits mutate the builder; `mut` is scoped with them
    // so a Windows build does not fail `-D unused_mut`.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(path).map_err(OutputError::Io)?;
            validate_private_dir(&meta)
        }
        Err(err) => Err(OutputError::Io(err)),
    }
}

fn validate_private_dir(meta: &fs::Metadata) -> Result<(), OutputError> {
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Err(OutputError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "spool directory is not a private directory",
        )));
    }
    Ok(())
}

fn next_spill_path(dir: &Path) -> PathBuf {
    let seq = SPILL_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(
        "{}-{seq}-{nanos:x}{SPOOL_SUFFIX}",
        std::process::id()
    ))
}

fn open_exclusive_spill(path: &Path) -> Result<File, OutputError> {
    if let Some(parent) = path.parent() {
        let meta = fs::symlink_metadata(parent).map_err(OutputError::Io)?;
        validate_private_dir(&meta)?;
    }
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path).map_err(OutputError::Io)?;
    let meta = fs::symlink_metadata(path).map_err(OutputError::Io)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(OutputError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "spill path is not a regular file",
        )));
    }
    if let Some(parent) = path.parent() {
        let parent_meta = fs::symlink_metadata(parent).map_err(OutputError::Io)?;
        validate_private_dir(&parent_meta)?;
    }
    Ok(file)
}

fn publish_spill(
    store: &ArtifactStore,
    spill: &mut SpillFile,
    kind: OutputStream,
    cancel: &CancellationToken,
) -> Result<ArtifactRef, OutputError> {
    check_cancel(cancel)?;
    spill.file.flush().map_err(OutputError::Io)?;
    spill.file.sync_all().map_err(OutputError::Io)?;
    spill
        .file
        .seek(SeekFrom::Start(0))
        .map_err(OutputError::Io)?;
    let media = match kind {
        OutputStream::Stdout => STDOUT_MEDIA,
        OutputStream::Stderr => STDERR_MEDIA,
    };
    let meta = ArtifactMetadata::new(media, RedactionClass::Sensitive);
    publish_reader(store, &mut spill.file, meta, cancel)
}

fn publish_reader<R: Read>(
    store: &ArtifactStore,
    reader: R,
    meta: ArtifactMetadata,
    cancel: &CancellationToken,
) -> Result<ArtifactRef, OutputError> {
    check_cancel(cancel)?;
    let store_cancel = StoreCancel::new();
    let linked = CancelLinkedReader {
        inner: reader,
        caller: cancel.clone(),
        store: store_cancel.clone(),
    };
    match store.put(linked, meta, &store_cancel) {
        Ok(refer) => {
            if cancel.is_cancelled() {
                Err(OutputError::Cancelled)
            } else {
                Ok(refer)
            }
        }
        Err(ArtifactError::Cancelled) => Err(OutputError::Cancelled),
        Err(err) => {
            if cancel.is_cancelled() || store_cancel.is_cancelled() {
                Err(OutputError::Cancelled)
            } else {
                Err(OutputError::Artifact(err))
            }
        }
    }
}

fn drop_spill(spill: &mut SpillFile) {
    let _ = fs::remove_file(&spill.path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::AtomicU64;

    const CANARY: &str = "super-secret-password-CANARY";
    const CSI: &[u8] = b"\x1b[31mRED\x1b]0;injected-title\x07";

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempStore {
        store: ArtifactStore,
        root: PathBuf,
    }

    impl TempStore {
        fn create() -> Self {
            Self::create_with_limit(1 << 20)
        }

        fn create_with_limit(max_bytes: u64) -> Self {
            let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("rapidlm-output-spool-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let store = ArtifactStore::create_with_limit(&root, max_bytes).expect("create store");
            Self { store, root }
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn limits(inline: usize, disk: u64) -> OutputLimits {
        OutputLimits::new(inline, disk)
    }

    fn count_blobs(store: &ArtifactStore) -> usize {
        let blobs = store.root().join("blobs");
        let Ok(shards) = fs::read_dir(&blobs) else {
            return 0;
        };
        let mut count = 0;
        for shard in shards {
            let shard = shard.expect("shard");
            if !shard.file_type().expect("type").is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path()).expect("read shard") {
                let path = entry.expect("entry").path();
                if path.extension().is_some_and(|ext| ext == "meta") {
                    continue;
                }
                if path.is_file() {
                    count += 1;
                }
            }
        }
        count
    }

    fn leak_text(value: &impl fmt::Debug, display: &impl fmt::Display) -> String {
        format!("{value:?} | {display}")
    }

    #[test]
    fn small_output_is_excerpt_only() {
        let tmp = TempStore::create();
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(64, 1024)).expect("spool");
        spool.write_stdout(b"hello", &live()).expect("write");
        let done = spool.finish(&live()).expect("finish");
        assert_eq!(done.stdout.inline_excerpt, b"hello");
        assert!(done.stdout.artifact.is_none());
        assert!(!done.stdout.truncated);
        assert_eq!(done.stdout.cursor.offset, 5);
        assert!(done.stderr.inline_excerpt.is_empty());
        assert_eq!(count_blobs(&tmp.store), 0);
    }

    #[test]
    fn overflow_spills_full_output_to_artifact() {
        let tmp = TempStore::create();
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(4, 64)).expect("spool");
        spool.write_stdout(b"abcdefghij", &live()).expect("write");
        let done = spool.finish(&live()).expect("finish");
        assert_eq!(done.stdout.inline_excerpt, b"abcd");
        let refer = done.stdout.artifact.expect("artifact");
        assert_eq!(refer.bytes, 10);
        assert_eq!(refer.redaction, RedactionClass::Sensitive);
        let got = tmp.store.get(&refer.id, &StoreCancel::new()).expect("get");
        assert_eq!(got, b"abcdefghij");
        assert!(!done.stdout.truncated);
        assert_eq!(done.stdout.cursor.offset, 10);
    }

    #[test]
    fn streams_are_independent() {
        let tmp = TempStore::create();
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(4, 64)).expect("spool");
        spool.write_stdout(b"STDOUT-DATA", &live()).expect("out");
        spool.write_stderr(b"STDERR-DATA", &live()).expect("err");
        let done = spool.finish(&live()).expect("finish");
        assert_eq!(done.stdout.inline_excerpt, b"STDO");
        assert_eq!(done.stderr.inline_excerpt, b"STDE");
        let out = tmp
            .store
            .get(&done.stdout.artifact.expect("out").id, &StoreCancel::new())
            .expect("get out");
        let err = tmp
            .store
            .get(&done.stderr.artifact.expect("err").id, &StoreCancel::new())
            .expect("get err");
        assert_eq!(out, b"STDOUT-DATA");
        assert_eq!(err, b"STDERR-DATA");
        assert_ne!(out, err);
    }

    #[test]
    fn tail_ring_keeps_only_recent_bytes() {
        let tmp = TempStore::create();
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(8, 64)).expect("spool");
        spool
            .write_stdout(b"abcdefghijklmnopqrst", &live())
            .expect("write");
        assert_eq!(spool.stdout_tail(), b"mnopqrst");
        assert!(spool.stdout_tail().len() <= 8);
        let done = spool.finish(&live()).expect("finish");
        assert_eq!(done.stdout.inline_excerpt, b"abcdefgh");
    }

    #[test]
    fn cancelled_write_does_not_publish() {
        let tmp = TempStore::create();
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(4, 64)).expect("spool");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = spool
            .write_stdout(b"abcdefghij", &cancel)
            .expect_err("cancelled");
        assert!(matches!(err, OutputError::Cancelled));
        assert_eq!(count_blobs(&tmp.store), 0);
        drop(spool);
        assert_eq!(count_blobs(&tmp.store), 0);
    }

    #[test]
    fn cancelled_finish_does_not_publish() {
        let tmp = TempStore::create();
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(4, 64)).expect("spool");
        spool.write_stdout(b"abcdefghij", &live()).expect("write");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = spool.finish(&cancel).expect_err("cancelled");
        assert!(matches!(err, OutputError::Cancelled));
        assert_eq!(count_blobs(&tmp.store), 0);
    }

    #[test]
    fn cancelled_during_finish_does_not_publish() {
        let tmp = TempStore::create();
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(8, 256 * 1024)).expect("spool");
        let payload = vec![b'x'; 32 * 1024];
        spool.write_stdout(&payload, &live()).expect("write");
        let cancel = CancellationToken::new();
        let err = publish_reader(
            &tmp.store,
            CancelAfterFirst {
                data: payload,
                pos: 0,
                cancel: cancel.clone(),
            },
            ArtifactMetadata::new(STDOUT_MEDIA, RedactionClass::Sensitive),
            &cancel,
        )
        .expect_err("mid-put cancel");
        assert!(matches!(err, OutputError::Cancelled));
        assert_eq!(count_blobs(&tmp.store), 0);
        let cancel_finish = CancellationToken::new();
        cancel_finish.cancel();
        let err = spool.finish(&cancel_finish).expect_err("finish cancel");
        assert!(matches!(err, OutputError::Cancelled));
        assert_eq!(count_blobs(&tmp.store), 0);
    }

    #[test]
    fn oversized_inline_limit_fails_closed() {
        let tmp = TempStore::create();
        let err = OutputSpool::new(
            tmp.store.clone(),
            limits(MAX_INLINE_EXCERPT_BYTES + 1, 1024),
        )
        .expect_err("limit");
        assert!(matches!(err, OutputError::LimitInvalid));
        assert_eq!(err.error_code(), Some(ErrorCode::ToolInvalidArguments));
        assert_eq!(count_blobs(&tmp.store), 0);
    }

    #[test]
    fn store_max_clamps_disk_cap() {
        let tmp = TempStore::create_with_limit(16);
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(4, 1_000_000)).expect("spool");
        spool.write_stdout(&[b'a'; 64], &live()).expect("write");
        let done = spool.finish(&live()).expect("finish");
        let refer = done.stdout.artifact.expect("artifact");
        assert_eq!(refer.bytes, 16);
        assert!(done.stdout.truncated);
    }

    #[test]
    fn zero_disk_cap_stores_nothing_and_truncates() {
        let tmp = TempStore::create();
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(4, 0)).expect("spool");
        spool.write_stdout(b"abcdefghij", &live()).expect("write");
        let done = spool.finish(&live()).expect("finish");
        assert_eq!(done.stdout.inline_excerpt, b"abcd");
        assert!(done.stdout.artifact.is_none());
        assert!(done.stdout.truncated);
        assert_eq!(count_blobs(&tmp.store), 0);
    }

    #[test]
    fn flood_does_not_grow_memory_and_sets_truncated() {
        let tmp = TempStore::create();
        let inline = 256;
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(inline, 1024)).expect("spool");
        let chunk = vec![b'f'; 4 * 1024];
        for _ in 0..32 {
            spool.write_stdout(&chunk, &live()).expect("write");
        }
        assert!(spool.memory_bytes() <= 4 * inline);
        let metrics = spool.metrics();
        assert_eq!(metrics.stdout_bytes, 32 * 4 * 1024);
        assert!(metrics.truncated);
        let debug = format!("{spool:?}");
        assert!(!debug.contains("fffff"));
        let done = spool.finish(&live()).expect("finish");
        assert!(done.stdout.truncated);
        assert_eq!(done.stdout.artifact.expect("artifact").bytes, 1024);
        assert!(spool_dir_has_no_leftover(&tmp.store));
    }

    #[test]
    fn single_huge_chunk_cannot_bypass_disk_cap() {
        let tmp = TempStore::create();
        let inline = 64;
        let disk = 512;
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(inline, disk)).expect("spool");
        let chunk = vec![b'z'; 1_000_000];
        spool.write_stdout(&chunk, &live()).expect("write");
        assert!(spool.memory_bytes() <= 4 * inline);
        let done = spool.finish(&live()).expect("finish");
        let refer = done.stdout.artifact.expect("artifact");
        assert_eq!(refer.bytes, disk);
        assert!(done.stdout.truncated);
        let got = tmp.store.get(&refer.id, &StoreCancel::new()).expect("get");
        assert_eq!(got.len() as u64, disk);
        assert!(got.iter().all(|b| *b == b'z'));
    }

    #[test]
    fn canary_and_csi_are_absent_from_debug_and_errors() {
        let tmp = TempStore::create();
        let mut payload = CANARY.as_bytes().to_vec();
        payload.extend_from_slice(CSI);
        payload.extend_from_slice(&[b'x'; 32]);
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(8, 256)).expect("spool");
        spool.write_stdout(&payload, &live()).expect("write");
        let metrics = spool.metrics();
        let spool_dbg = format!("{spool:?} {metrics:?}");
        assert!(!spool_dbg.contains(CANARY), "{spool_dbg}");
        assert!(!spool_dbg.as_bytes().windows(CSI.len()).any(|w| w == CSI));
        let done = spool.finish(&live()).expect("finish");
        let refer = done.stdout.artifact.as_ref().expect("artifact");
        let stored = tmp.store.get(&refer.id, &StoreCancel::new()).expect("get");
        assert!(stored.windows(CANARY.len()).any(|w| w == CANARY.as_bytes()));
        assert!(stored.windows(CSI.len()).any(|w| w == CSI));
        let shown = leak_text(&done, &done.stdout);
        assert!(!shown.contains(CANARY), "{shown}");
        assert!(!shown.as_bytes().windows(CSI.len()).any(|w| w == CSI));
        let err = OutputError::LimitInvalid;
        let err_text = leak_text(&err, &err);
        assert!(!err_text.contains(CANARY));
        assert_eq!(refer.redaction, RedactionClass::Sensitive);
    }

    #[test]
    fn spill_create_new_refuses_symlink_and_preserves_target() {
        let tmp = TempStore::create();
        let spool_dir = tmp.store.root().join(SPOOL_DIR);
        ensure_private_dir(&spool_dir).expect("dir");
        let target = tmp.root.join("canary-target");
        fs::write(&target, CANARY).expect("canary");
        let planted = spool_dir.join("planted.spool");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &planted).expect("symlink");
        }
        #[cfg(not(unix))]
        {
            fs::write(&planted, CANARY).expect("plant existing");
        }
        let err = open_exclusive_spill(&planted).expect_err("create_new");
        assert!(matches!(err, OutputError::Io(_)));
        assert_eq!(fs::read(&target).expect("target"), CANARY.as_bytes());
        let shown = leak_text(&err, &err);
        assert!(!shown.contains(CANARY), "{shown}");
        assert_eq!(count_blobs(&tmp.store), 0);
    }

    #[cfg(unix)]
    #[test]
    fn spill_rejects_symlinked_spool_directory() {
        let tmp = TempStore::create();
        let outside = tmp.root.join("outside");
        fs::create_dir_all(&outside).expect("outside");
        let canary = outside.join("canary");
        fs::write(&canary, CANARY).expect("canary");
        std::os::unix::fs::symlink(&outside, tmp.store.root().join(SPOOL_DIR)).expect("dir link");
        let err = OutputSpool::new(tmp.store.clone(), limits(4, 64)).expect_err("symlink dir");
        assert!(matches!(err, OutputError::Io(_)));
        assert_eq!(fs::read(&canary).expect("intact"), CANARY.as_bytes());
        let shown = leak_text(&err, &err);
        assert!(!shown.contains(CANARY), "{shown}");
        assert_eq!(count_blobs(&tmp.store), 0);
    }

    #[test]
    fn absorb_uses_bounded_windows() {
        let tmp = TempStore::create();
        let mut spool = OutputSpool::new(tmp.store.clone(), limits(8, 64)).expect("spool");
        let data = vec![b'q'; 40];
        spool
            .absorb_stdout(Cursor::new(data.clone()), &live())
            .expect("absorb");
        let done = spool.finish(&live()).expect("finish");
        assert_eq!(done.stdout.inline_excerpt, b"qqqqqqqq");
        let got = tmp
            .store
            .get(&done.stdout.artifact.expect("art").id, &StoreCancel::new())
            .expect("get");
        assert_eq!(got, data);
    }

    fn spool_dir_has_no_leftover(store: &ArtifactStore) -> bool {
        let dir = store.root().join(SPOOL_DIR);
        let Ok(entries) = fs::read_dir(dir) else {
            return true;
        };
        entries
            .filter_map(Result::ok)
            .all(|entry| !entry.path().extension().is_some_and(|ext| ext == "spool"))
    }

    struct CancelAfterFirst {
        data: Vec<u8>,
        pos: usize,
        cancel: CancellationToken,
    }

    impl Read for CancelAfterFirst {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos > 0 {
                self.cancel.cancel();
            }
            if self.pos >= self.data.len() || buf.is_empty() {
                return Ok(0);
            }
            let n = buf.len().min(self.data.len() - self.pos).min(64);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }
}
