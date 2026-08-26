//! Atomic content-addressed artifact blobs on local disk.
//!
//! `put` stages into `tmp/`, fsyncs, names the blob from the SHA-256 digest,
//! then publishes with `rename`. Reads re-hash the blob and reject mismatch.
//! Path layout is derived only from the digest.

use std::fmt::{self, Debug};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::{
    ARTIFACT_DIGEST_LEN, ARTIFACT_ID_HEX_LEN, ARTIFACT_ID_PREFIX, ArtifactId, ArtifactRef,
    RedactionClass,
};
use sha2::{Digest, Sha256};

/// Default published-blob size ceiling (1 GiB).
pub const DEFAULT_MAX_BYTES: u64 = 1 << 30;

/// Maximum accepted `media_type` UTF-8 length.
pub const MAX_MEDIA_TYPE_BYTES: usize = 256;

/// Sidecar format marker. Not a wire/API field.
const META_MAGIC: &str = "rapidlm.artifact.meta.v1";

const BLOBS_DIR: &str = "blobs";
const TMP_DIR: &str = "tmp";
const META_SUFFIX: &str = ".meta";
const TEMP_SUFFIX: &str = ".part";
const COPY_BUF_BYTES: usize = 64 * 1024;
const HEX_TABLE: &[u8; 16] = b"0123456789abcdef";

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Cooperative cancellation for store operations.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Caller-supplied metadata for [`ArtifactStore::put`]. Size is measured.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactMetadata {
    pub media_type: String,
    pub redaction: RedactionClass,
}

/// Optional byte window for [`ArtifactStore::open`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    pub offset: u64,
    pub len: Option<u64>,
}

/// Verified, range-limited reader over a published blob.
pub struct ArtifactReader {
    file: File,
    remaining: u64,
    id: ArtifactId,
}

/// Local-disk content-addressed artifact store.
#[derive(Clone, Debug)]
pub struct ArtifactStore {
    root: PathBuf,
    blobs: PathBuf,
    tmp: PathBuf,
    max_bytes: u64,
}

/// Typed failures for artifact-store operations.
#[derive(Debug)]
pub enum ArtifactError {
    Cancelled,
    NotFound {
        id: ArtifactId,
    },
    Integrity {
        id: ArtifactId,
    },
    BoundExceeded {
        limit: u64,
        observed: u64,
    },
    InvalidRange {
        offset: u64,
        len: Option<u64>,
        size: u64,
    },
    InvalidMetadata(&'static str),
    Metadata,
    Io(io::Error),
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<(), ArtifactError> {
        if self.is_cancelled() {
            Err(ArtifactError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl ArtifactMetadata {
    pub fn new(media_type: impl Into<String>, redaction: RedactionClass) -> Self {
        Self {
            media_type: media_type.into(),
            redaction,
        }
    }

    fn validate(&self) -> Result<(), ArtifactError> {
        if self.media_type.is_empty() {
            return Err(ArtifactError::InvalidMetadata("media_type is empty"));
        }
        if self.media_type.len() > MAX_MEDIA_TYPE_BYTES {
            return Err(ArtifactError::InvalidMetadata("media_type exceeds bound"));
        }
        if self.media_type.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(ArtifactError::InvalidMetadata(
                "media_type contains a control character",
            ));
        }
        Ok(())
    }
}

impl ArtifactStore {
    /// Create the store directory layout under `root`.
    pub fn create(root: impl AsRef<Path>) -> Result<Self, ArtifactError> {
        Self::create_with_limit(root, DEFAULT_MAX_BYTES)
    }

    /// Create the store with an explicit published-blob size ceiling.
    pub fn create_with_limit(
        root: impl AsRef<Path>,
        max_bytes: u64,
    ) -> Result<Self, ArtifactError> {
        let root = root.as_ref();
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        let blobs = root.join(BLOBS_DIR);
        let tmp = root.join(TMP_DIR);
        fs::create_dir_all(&blobs)?;
        fs::create_dir_all(&tmp)?;
        Ok(Self {
            root,
            blobs,
            tmp,
            max_bytes,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Stage `reader` to a unique temp file, fsync, then atomically publish.
    ///
    /// Concurrent puts of identical bytes share one digest path. A leftover
    /// temp is never treated as published.
    pub fn put<R: Read>(
        &self,
        reader: R,
        meta: ArtifactMetadata,
        cancel: &CancellationToken,
    ) -> Result<ArtifactRef, ArtifactError> {
        cancel.check()?;
        meta.validate()?;
        let staged = self.stage_and_hash(reader, cancel)?;
        let refer = ArtifactRef::new(staged.id, meta.media_type, staged.bytes, meta.redaction);
        self.publish_atomically(staged, &refer, cancel)?;
        Ok(refer)
    }

    /// Open a verified blob, optionally restricted to `range`.
    pub fn open(
        &self,
        id: &ArtifactId,
        range: Option<ByteRange>,
        cancel: &CancellationToken,
    ) -> Result<ArtifactReader, ArtifactError> {
        cancel.check()?;
        let path = self.blob_path(id);
        let mut file = File::open(&path).map_err(|err| map_io_not_found(err, id))?;
        let size = hash_and_verify(&mut file, id, self.max_bytes, cancel)?;
        self.check_sidecar(id, size)?;
        let (offset, remaining) = resolve_range(range, size)?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(ArtifactReader {
            file,
            remaining,
            id: *id,
        })
    }

    /// Read the entire verified blob into memory, bounded by `max_bytes`.
    pub fn get(
        &self,
        id: &ArtifactId,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, ArtifactError> {
        let mut reader = self.open(id, None, cancel)?;
        let mut out = Vec::new();
        let mut buf = [0u8; COPY_BUF_BYTES];
        loop {
            cancel.check()?;
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            let observed = out.len() as u64;
            if observed > self.max_bytes {
                return Err(ArtifactError::BoundExceeded {
                    limit: self.max_bytes,
                    observed,
                });
            }
        }
        Ok(out)
    }

    /// Load persisted put metadata. Missing sidecar is [`ArtifactError::NotFound`].
    pub fn metadata(&self, id: &ArtifactId) -> Result<ArtifactRef, ArtifactError> {
        let path = self.meta_path(id);
        let data = fs::read(&path).map_err(|err| map_io_not_found(err, id))?;
        decode_meta(&data, id)
    }

    pub fn exists(&self, id: &ArtifactId) -> bool {
        self.blob_path(id).is_file()
    }

    /// Published blob ids derived from digest-sharded filenames.
    pub fn list_published(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<ArtifactId>, ArtifactError> {
        cancel.check()?;
        let mut ids = Vec::new();
        let shards = match fs::read_dir(&self.blobs) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(ids),
            Err(err) => return Err(err.into()),
        };
        for shard in shards {
            cancel.check()?;
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            let entries = match fs::read_dir(shard.path()) {
                Ok(entries) => entries,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err.into()),
            };
            for entry in entries {
                cancel.check()?;
                let entry = entry?;
                let name = entry.file_name();
                let name = match name.to_str() {
                    Some(name) => name,
                    None => continue,
                };
                if name.ends_with(META_SUFFIX) || !entry.file_type()?.is_file() {
                    continue;
                }
                if name.len() != ARTIFACT_ID_HEX_LEN {
                    continue;
                }
                let mut wire =
                    String::with_capacity(ARTIFACT_ID_PREFIX.len() + ARTIFACT_ID_HEX_LEN);
                wire.push_str(ARTIFACT_ID_PREFIX);
                wire.push_str(name);
                if let Ok(id) = ArtifactId::from_str(&wire) {
                    ids.push(id);
                }
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Remove a published blob and sidecar. Missing files are success.
    pub fn unpublish(&self, id: &ArtifactId) -> Result<(), ArtifactError> {
        let blob = self.blob_path(id);
        match fs::remove_file(&blob) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        match fs::remove_file(self.meta_path(id)) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        Ok(())
    }

    /// Delete leftover `*.part` files in `tmp/` older than `older_than`.
    ///
    /// Live puts use unique temp names; this is crash recovery, not a GC of
    /// published blobs. Pass [`Duration::ZERO`] to remove every leftover.
    pub fn cleanup_temps(&self, older_than: Duration) -> Result<u64, ArtifactError> {
        let now = SystemTime::now();
        let mut removed = 0u64;
        let entries = match fs::read_dir(&self.tmp) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !is_temp_file(&path) {
                continue;
            }
            let file_type = entry.file_type()?;
            if !file_type.is_file() {
                continue;
            }
            if older_than > Duration::ZERO {
                let modified = match entry.metadata()?.modified() {
                    Ok(modified) => modified,
                    Err(_) => continue,
                };
                let age = match now.duration_since(modified) {
                    Ok(age) => age,
                    Err(_) => Duration::ZERO,
                };
                if age < older_than {
                    continue;
                }
            }
            match fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(removed)
    }

    fn stage_and_hash<R: Read>(
        &self,
        mut reader: R,
        cancel: &CancellationToken,
    ) -> Result<StagedBlob, ArtifactError> {
        let temp = self.new_temp_path();
        let staged = match stage_to_path(&temp, &mut reader, self.max_bytes, cancel) {
            Ok(staged) => staged,
            Err(err) => {
                let _ = fs::remove_file(&temp);
                return Err(err);
            }
        };
        Ok(StagedBlob {
            id: staged.id,
            bytes: staged.bytes,
            temp,
        })
    }

    fn publish_atomically(
        &self,
        staged: StagedBlob,
        refer: &ArtifactRef,
        cancel: &CancellationToken,
    ) -> Result<(), ArtifactError> {
        let dest = self.blob_path(&refer.id);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        let dest_state = if dest.exists() {
            match hash_path(&dest, &refer.id, self.max_bytes, cancel) {
                Ok(_) => DestState::Verified,
                Err(ArtifactError::Integrity { .. }) => DestState::Corrupt,
                Err(ArtifactError::NotFound { .. }) => DestState::Missing,
                Err(err) => {
                    let _ = fs::remove_file(&staged.temp);
                    return Err(err);
                }
            }
        } else {
            DestState::Missing
        };

        match dest_state {
            DestState::Verified => {
                let _ = fs::remove_file(&staged.temp);
                return self.write_meta_if_absent(refer);
            }
            DestState::Corrupt | DestState::Missing => {}
        }

        if let Err(err) = replace_blob(&staged.temp, &dest, dest_state) {
            if dest.exists() && hash_path(&dest, &refer.id, self.max_bytes, cancel).is_ok() {
                let _ = fs::remove_file(&staged.temp);
                return self.write_meta_if_absent(refer);
            }
            let _ = fs::remove_file(&staged.temp);
            return Err(err);
        }
        self.write_meta_if_absent(refer)
    }

    fn write_meta_if_absent(&self, refer: &ArtifactRef) -> Result<(), ArtifactError> {
        let dest = self.meta_path(&refer.id);
        if dest.is_file() {
            return Ok(());
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = self.new_temp_path();
        if let Err(err) = write_meta_file(&temp, refer) {
            let _ = fs::remove_file(&temp);
            return Err(err);
        }
        match fs::rename(&temp, &dest) {
            Ok(()) => {
                fsync_parent(&dest)?;
                Ok(())
            }
            Err(_err) if dest.is_file() => {
                let _ = fs::remove_file(&temp);
                Ok(())
            }
            Err(err) => {
                let _ = fs::remove_file(&temp);
                Err(err.into())
            }
        }
    }

    fn check_sidecar(&self, id: &ArtifactId, size: u64) -> Result<(), ArtifactError> {
        let path = self.meta_path(id);
        if !path.exists() {
            return Ok(());
        }
        let refer = self.metadata(id)?;
        if refer.bytes != size {
            return Err(ArtifactError::Integrity { id: *id });
        }
        Ok(())
    }

    fn blob_path(&self, id: &ArtifactId) -> PathBuf {
        let hex = digest_hex(id);
        self.blobs.join(&hex[..2]).join(hex)
    }

    fn meta_path(&self, id: &ArtifactId) -> PathBuf {
        let mut path = self.blob_path(id);
        path.as_mut_os_string().push(META_SUFFIX);
        path
    }

    fn new_temp_path(&self) -> PathBuf {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.tmp.join(format!(
            "{}-{seq}-{nanos:x}{TEMP_SUFFIX}",
            std::process::id()
        ))
    }
}

impl ArtifactReader {
    pub fn id(&self) -> ArtifactId {
        self.id
    }

    pub fn remaining(&self) -> u64 {
        self.remaining
    }
}

impl Read for ArtifactReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let take = usize::try_from(self.remaining).unwrap_or(usize::MAX);
        let n = buf.len().min(take);
        let n = self.file.read(&mut buf[..n])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

impl Debug for ArtifactReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArtifactReader")
            .field("id", &self.id)
            .field("remaining", &self.remaining)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("artifact operation cancelled"),
            Self::NotFound { id } => write!(f, "artifact {id} not found"),
            Self::Integrity { id } => write!(f, "artifact {id} failed integrity verification"),
            Self::BoundExceeded { limit, observed } => {
                write!(f, "artifact exceeds bound {limit}, observed {observed}")
            }
            Self::InvalidRange { offset, len, size } => {
                write!(
                    f,
                    "artifact range offset={offset} len={len:?} exceeds size {size}"
                )
            }
            Self::InvalidMetadata(reason) => write!(f, "invalid artifact metadata: {reason}"),
            Self::Metadata => f.write_str("artifact metadata sidecar is malformed"),
            Self::Io(err) => write!(f, "artifact store io error: {err}"),
        }
    }
}

impl std::error::Error for ArtifactError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Cancelled
            | Self::NotFound { .. }
            | Self::Integrity { .. }
            | Self::BoundExceeded { .. }
            | Self::InvalidRange { .. }
            | Self::InvalidMetadata(_)
            | Self::Metadata => None,
        }
    }
}

impl From<io::Error> for ArtifactError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

struct StagedBlob {
    id: ArtifactId,
    bytes: u64,
    temp: PathBuf,
}

struct HashedLen {
    id: ArtifactId,
    bytes: u64,
}

#[derive(Clone, Copy)]
enum DestState {
    Missing,
    Verified,
    Corrupt,
}

fn replace_blob(temp: &Path, dest: &Path, dest_state: DestState) -> Result<(), ArtifactError> {
    // Windows rename cannot replace an existing file; Unix rename can.
    if matches!(dest_state, DestState::Corrupt) && dest.exists() {
        fs::remove_file(dest)?;
    }
    fs::rename(temp, dest)?;
    fsync_parent(dest)?;
    Ok(())
}

fn stage_to_path<R: Read>(
    temp: &Path,
    reader: &mut R,
    max_bytes: u64,
    cancel: &CancellationToken,
) -> Result<HashedLen, ArtifactError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(temp)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; COPY_BUF_BYTES];
    let mut bytes = 0u64;
    loop {
        cancel.check()?;
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let observed = bytes
            .checked_add(n as u64)
            .ok_or(ArtifactError::BoundExceeded {
                limit: max_bytes,
                observed: u64::MAX,
            })?;
        if observed > max_bytes {
            return Err(ArtifactError::BoundExceeded {
                limit: max_bytes,
                observed,
            });
        }
        bytes = observed;
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
    }
    file.flush()?;
    file.sync_all()?;
    drop(file);
    fsync_parent(temp)?;
    let digest: [u8; ARTIFACT_DIGEST_LEN] = hasher.finalize().into();
    Ok(HashedLen {
        id: artifact_id_from_digest(digest),
        bytes,
    })
}

fn hash_path(
    path: &Path,
    expected: &ArtifactId,
    max_bytes: u64,
    cancel: &CancellationToken,
) -> Result<u64, ArtifactError> {
    let mut file = File::open(path).map_err(|err| map_io_not_found(err, expected))?;
    hash_and_verify(&mut file, expected, max_bytes, cancel)
}

fn hash_and_verify(
    file: &mut File,
    expected: &ArtifactId,
    max_bytes: u64,
    cancel: &CancellationToken,
) -> Result<u64, ArtifactError> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; COPY_BUF_BYTES];
    let mut bytes = 0u64;
    loop {
        cancel.check()?;
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let observed = bytes
            .checked_add(n as u64)
            .ok_or(ArtifactError::BoundExceeded {
                limit: max_bytes,
                observed: u64::MAX,
            })?;
        if observed > max_bytes {
            return Err(ArtifactError::BoundExceeded {
                limit: max_bytes,
                observed,
            });
        }
        bytes = observed;
        hasher.update(&buf[..n]);
    }
    let digest: [u8; ARTIFACT_DIGEST_LEN] = hasher.finalize().into();
    let actual = artifact_id_from_digest(digest);
    if actual != *expected {
        return Err(ArtifactError::Integrity { id: *expected });
    }
    Ok(bytes)
}

fn write_meta_file(path: &Path, refer: &ArtifactRef) -> Result<(), ArtifactError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let encoded = encode_meta(refer);
    file.write_all(encoded.as_bytes())?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    fsync_parent(path)?;
    Ok(())
}

fn encode_meta(refer: &ArtifactRef) -> String {
    format!(
        "{META_MAGIC}\n{}\n{}\n{}\n{}\n",
        refer.id, refer.bytes, refer.redaction, refer.media_type
    )
}

fn decode_meta(data: &[u8], expected: &ArtifactId) -> Result<ArtifactRef, ArtifactError> {
    let text = std::str::from_utf8(data).map_err(|_| ArtifactError::Metadata)?;
    let mut lines = text.lines();
    let magic = lines.next().ok_or(ArtifactError::Metadata)?;
    if magic != META_MAGIC {
        return Err(ArtifactError::Metadata);
    }
    let id = ArtifactId::from_str(lines.next().ok_or(ArtifactError::Metadata)?)
        .map_err(|_| ArtifactError::Metadata)?;
    let bytes = lines
        .next()
        .ok_or(ArtifactError::Metadata)?
        .parse::<u64>()
        .map_err(|_| ArtifactError::Metadata)?;
    let redaction = RedactionClass::from_str(lines.next().ok_or(ArtifactError::Metadata)?)
        .map_err(|_| ArtifactError::Metadata)?;
    let media_type = lines.next().ok_or(ArtifactError::Metadata)?.to_owned();
    if lines.next().is_some() {
        return Err(ArtifactError::Metadata);
    }
    if id != *expected {
        return Err(ArtifactError::Integrity { id: *expected });
    }
    if media_type.is_empty() {
        return Err(ArtifactError::Metadata);
    }
    Ok(ArtifactRef::new(id, media_type, bytes, redaction))
}

fn resolve_range(range: Option<ByteRange>, size: u64) -> Result<(u64, u64), ArtifactError> {
    let Some(range) = range else {
        return Ok((0, size));
    };
    if range.offset > size {
        return Err(ArtifactError::InvalidRange {
            offset: range.offset,
            len: range.len,
            size,
        });
    }
    let max_remaining = size - range.offset;
    let remaining = match range.len {
        Some(len) => {
            if len > max_remaining {
                return Err(ArtifactError::InvalidRange {
                    offset: range.offset,
                    len: range.len,
                    size,
                });
            }
            len
        }
        None => max_remaining,
    };
    Ok((range.offset, remaining))
}

fn map_io_not_found(err: io::Error, id: &ArtifactId) -> ArtifactError {
    if err.kind() == io::ErrorKind::NotFound {
        ArtifactError::NotFound { id: *id }
    } else {
        ArtifactError::Io(err)
    }
}

fn is_temp_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == TEMP_SUFFIX.trim_start_matches('.'))
}

fn digest_hex(id: &ArtifactId) -> String {
    let digest = id.as_digest();
    let mut hex = String::with_capacity(ARTIFACT_ID_HEX_LEN);
    for &byte in digest {
        hex.push(HEX_TABLE[(byte >> 4) as usize] as char);
        hex.push(HEX_TABLE[(byte & 0x0f) as usize] as char);
    }
    hex
}

fn artifact_id_from_digest(digest: [u8; ARTIFACT_DIGEST_LEN]) -> ArtifactId {
    let mut wire = String::with_capacity(ARTIFACT_ID_PREFIX.len() + ARTIFACT_ID_HEX_LEN);
    wire.push_str(ARTIFACT_ID_PREFIX);
    for &byte in &digest {
        wire.push(HEX_TABLE[(byte >> 4) as usize] as char);
        wire.push(HEX_TABLE[(byte & 0x0f) as usize] as char);
    }
    // Hex alphabet + prefix is the only accepted ArtifactId wire form.
    ArtifactId::from_str(&wire).expect("sha256 hex digest is a canonical ArtifactId")
}

fn fsync_parent(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fsync_dir(parent)
}

fn fsync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Barrier};
    use std::thread;

    const ABC: &[u8] = b"abc";
    const ABC_ID: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    struct TempStore {
        store: ArtifactStore,
        root: PathBuf,
    }

    impl TempStore {
        fn create() -> Self {
            Self::create_with_limit(DEFAULT_MAX_BYTES)
        }

        fn create_with_limit(max_bytes: u64) -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "rapidlm-artifact-store-{}-{seq}",
                std::process::id()
            ));
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

    fn text_meta() -> ArtifactMetadata {
        ArtifactMetadata::new("text/plain", RedactionClass::Public)
    }

    fn count_blobs(store: &ArtifactStore) -> usize {
        let mut count = 0;
        let shards = fs::read_dir(&store.blobs).expect("read blobs");
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

    fn count_temps(store: &ArtifactStore) -> usize {
        fs::read_dir(&store.tmp)
            .expect("read tmp")
            .filter(|entry| entry.as_ref().ok().is_some_and(|e| is_temp_file(&e.path())))
            .count()
    }

    #[test]
    fn put_then_get_round_trips_and_matches_known_digest() {
        let tmp = TempStore::create();
        let refer = tmp
            .store
            .put(Cursor::new(ABC), text_meta(), &live())
            .expect("put");
        assert_eq!(refer.id.to_string(), ABC_ID);
        assert_eq!(refer.bytes, 3);
        assert_eq!(refer.media_type, "text/plain");
        assert_eq!(refer.redaction, RedactionClass::Public);
        let got = tmp.store.get(&refer.id, &live()).expect("get");
        assert_eq!(got, ABC);
        assert_eq!(tmp.store.metadata(&refer.id).expect("meta"), refer);
    }

    #[test]
    fn path_layout_is_digest_sharded_not_filename() {
        let tmp = TempStore::create();
        let meta = ArtifactMetadata::new("../../../etc/passwd", RedactionClass::Public);
        let refer = tmp.store.put(Cursor::new(ABC), meta, &live()).expect("put");
        let hex = digest_hex(&refer.id);
        let blob = tmp.store.blobs.join(&hex[..2]).join(&hex);
        assert!(blob.is_file(), "missing {blob:?}");
        assert!(!tmp.root.join("etc").exists());
        assert!(!tmp.root.join("passwd").exists());
        assert_eq!(count_blobs(&tmp.store), 1);
    }

    #[test]
    fn concurrent_identical_puts_deduplicate() {
        let tmp = TempStore::create();
        let store = Arc::new(tmp.store.clone());
        let n = 8;
        let barrier = Arc::new(Barrier::new(n));
        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                store.put(Cursor::new(ABC), text_meta(), &live())
            }));
        }
        let mut ids = Vec::with_capacity(n);
        for handle in handles {
            let refer = handle.join().expect("thread").expect("put");
            assert_eq!(refer.id.to_string(), ABC_ID);
            ids.push(refer.id);
        }
        assert!(ids.iter().all(|id| *id == ids[0]));
        assert_eq!(count_blobs(&tmp.store), 1);
        assert_eq!(tmp.store.get(&ids[0], &live()).expect("get"), ABC);
        assert_eq!(count_temps(&tmp.store), 0);
    }

    #[test]
    fn corrupted_blob_read_returns_integrity_error() {
        let tmp = TempStore::create();
        let refer = tmp
            .store
            .put(Cursor::new(ABC), text_meta(), &live())
            .expect("put");
        fs::write(tmp.store.blob_path(&refer.id), b"xxx").expect("corrupt");
        let err = tmp.store.get(&refer.id, &live()).expect_err("integrity");
        assert!(
            matches!(err, ArtifactError::Integrity { id } if id == refer.id),
            "got {err:?}"
        );
        let err = tmp.store.open(&refer.id, None, &live()).expect_err("open");
        assert!(matches!(err, ArtifactError::Integrity { id } if id == refer.id));
    }

    #[test]
    fn leftover_temp_is_not_published_and_is_cleanable() {
        let tmp = TempStore::create();
        let id = ArtifactId::from_bytes(ABC);
        let orphan = tmp.store.tmp.join("crash-orphan.part");
        fs::write(&orphan, ABC).expect("plant leftover");
        assert!(!tmp.store.exists(&id));
        let err = tmp.store.get(&id, &live()).expect_err("unpublished");
        assert!(matches!(err, ArtifactError::NotFound { id: found } if found == id));
        assert!(orphan.is_file());
        let removed = tmp.store.cleanup_temps(Duration::ZERO).expect("cleanup");
        assert_eq!(removed, 1);
        assert!(!orphan.exists());
        assert_eq!(count_blobs(&tmp.store), 0);
    }

    #[test]
    fn failed_put_does_not_leave_temp_or_blob() {
        let tmp = TempStore::create_with_limit(4);
        let err = tmp
            .store
            .put(Cursor::new(b"too-big"), text_meta(), &live())
            .expect_err("bound");
        assert!(
            matches!(
                err,
                ArtifactError::BoundExceeded {
                    limit: 4,
                    observed: 7
                }
            ),
            "got {err:?}"
        );
        assert_eq!(count_blobs(&tmp.store), 0);
        assert_eq!(count_temps(&tmp.store), 0);
    }

    #[test]
    fn cancelled_put_is_not_published() {
        let tmp = TempStore::create();
        let cancel = CancellationToken::new();
        let reader = CancelAfterFirstChunk {
            data: b"abcdefghijklmnop",
            pos: 0,
            cancel: cancel.clone(),
        };
        let err = tmp
            .store
            .put(reader, text_meta(), &cancel)
            .expect_err("cancelled");
        assert!(matches!(err, ArtifactError::Cancelled));
        assert_eq!(count_blobs(&tmp.store), 0);
        assert_eq!(count_temps(&tmp.store), 0);
    }

    #[test]
    fn put_replaces_corrupt_dest_with_verified_stage() {
        let tmp = TempStore::create();
        let refer = tmp
            .store
            .put(Cursor::new(ABC), text_meta(), &live())
            .expect("put");
        fs::write(tmp.store.blob_path(&refer.id), b"tampered").expect("corrupt");
        let again = tmp
            .store
            .put(Cursor::new(ABC), text_meta(), &live())
            .expect("repair put");
        assert_eq!(again.id, refer.id);
        assert_eq!(tmp.store.get(&refer.id, &live()).expect("get"), ABC);
    }

    #[test]
    fn range_read_is_verified_then_sliced() {
        let tmp = TempStore::create();
        let refer = tmp
            .store
            .put(Cursor::new(b"abcdef"), text_meta(), &live())
            .expect("put");
        let mut reader = tmp
            .store
            .open(
                &refer.id,
                Some(ByteRange {
                    offset: 2,
                    len: Some(3),
                }),
                &live(),
            )
            .expect("open range");
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).expect("read");
        assert_eq!(buf, b"cde");
        assert_eq!(reader.remaining(), 0);
        let err = tmp
            .store
            .open(
                &refer.id,
                Some(ByteRange {
                    offset: 10,
                    len: None,
                }),
                &live(),
            )
            .expect_err("bad range");
        assert!(matches!(
            err,
            ArtifactError::InvalidRange { offset: 10, .. }
        ));
    }

    #[test]
    fn empty_blob_is_addressable() {
        let tmp = TempStore::create();
        let refer = tmp
            .store
            .put(Cursor::new(b""), text_meta(), &live())
            .expect("put empty");
        assert_eq!(refer.bytes, 0);
        assert_eq!(
            refer.id,
            ArtifactId::from_bytes(b""),
            "empty digest mismatch"
        );
        assert_eq!(tmp.store.get(&refer.id, &live()).expect("get"), b"");
    }

    #[test]
    fn missing_artifact_is_not_found() {
        let tmp = TempStore::create();
        let id = ArtifactId::from_bytes(b"missing");
        let err = tmp.store.get(&id, &live()).expect_err("missing");
        assert!(matches!(err, ArtifactError::NotFound { id: found } if found == id));
    }

    struct CancelAfterFirstChunk {
        data: &'static [u8],
        pos: usize,
        cancel: CancellationToken,
    }

    impl Read for CancelAfterFirstChunk {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos > 0 {
                self.cancel.cancel();
            }
            if self.pos >= self.data.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.data[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }
}
