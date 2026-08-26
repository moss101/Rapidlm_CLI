//! Per-project trust catalog keyed by canonical project identity.
//!
//! Missing records are untrusted. A material identity change at the same
//! canonical root invalidates any stored grant. The catalog is a versioned
//! JSON file written with temp+rename; corrupt or oversized input fails closed.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};

use crate::config::loader::CancellationToken;

/// On-disk catalog schema. Readers accept only this version.
pub const PROJECT_TRUST_SCHEMA: u16 = 1;

/// Maximum UTF-8 bytes accepted in a canonical root.
pub const MAX_CANONICAL_ROOT_BYTES: usize = 4096;

/// Maximum UTF-8 bytes accepted in a fingerprint or manifest hash.
pub const MAX_FINGERPRINT_BYTES: usize = 80;

/// Maximum persisted trust records in one catalog.
pub const MAX_TRUST_RECORDS: usize = 4096;

/// Maximum catalog file size in bytes.
pub const MAX_CATALOG_BYTES: u64 = 1024 * 1024;

const CANCEL_CHECK_EVERY: usize = 32;
const PART_SUFFIX: &str = ".part";
const CATALOG_KEYS: &[&str] = &["schema", "records"];
const RECORD_KEYS: &[&str] = &[
    "canonical_root",
    "vcs_remote_fingerprint",
    "device",
    "inode",
    "manifest_hash",
    "status",
];

/// Trusted only after an explicit grant for the exact identity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum TrustStatus {
    #[default]
    Untrusted,
    Trusted,
}

/// Absolute, lexically normalized project root. Comparison uses this form only.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CanonicalRoot(String);

/// Digest-like identity component. Comparison is on the normalized form.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct IdentityFingerprint(String);

/// Optional inode/device pair observed when trust was recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct DeviceHint {
    device: u64,
    inode: u64,
}

/// Canonical project identity used as the trust key.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ProjectIdentity {
    canonical_root: CanonicalRoot,
    vcs_remote_fingerprint: Option<IdentityFingerprint>,
    device_hint: Option<DeviceHint>,
    manifest_hash: Option<IdentityFingerprint>,
}

/// Durable per-project trust store.
pub struct ProjectTrustStore {
    catalog: PathBuf,
    max_records: usize,
    max_bytes: u64,
}

/// Typed trust-store failure. Messages never include path or fingerprint values.
#[derive(Debug)]
pub enum ProjectTrustError {
    Cancelled,
    InvalidRoot,
    InvalidFingerprint,
    InvalidManifestHash,
    CatalogTooLarge { limit: u64, observed: u64 },
    TooManyRecords,
    CatalogCorrupt,
    UnsupportedSchema { found: u16 },
    Io(io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredRecord {
    identity: ProjectIdentity,
    status: TrustStatus,
}

impl TrustStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Trusted => "trusted",
        }
    }

    pub const fn is_trusted(self) -> bool {
        matches!(self, Self::Trusted)
    }

    fn parse(raw: &str) -> Result<Self, ProjectTrustError> {
        match raw {
            "untrusted" => Ok(Self::Untrusted),
            "trusted" => Ok(Self::Trusted),
            _ => Err(ProjectTrustError::CatalogCorrupt),
        }
    }
}

impl CanonicalRoot {
    /// Parse an absolute root and lexically normalize `.` / `..` / separators.
    pub fn parse(path: impl AsRef<Path>) -> Result<Self, ProjectTrustError> {
        let raw = path
            .as_ref()
            .to_str()
            .ok_or(ProjectTrustError::InvalidRoot)?;
        if raw.is_empty() || raw.len() > MAX_CANONICAL_ROOT_BYTES {
            return Err(ProjectTrustError::InvalidRoot);
        }
        if raw.contains('\0') || raw.chars().any(char::is_control) {
            return Err(ProjectTrustError::InvalidRoot);
        }

        let input = Path::new(raw);
        if !input.is_absolute() {
            return Err(ProjectTrustError::InvalidRoot);
        }

        let mut out = PathBuf::new();
        for component in input.components() {
            match component {
                Component::Prefix(prefix) => {
                    let text = prefix
                        .as_os_str()
                        .to_str()
                        .ok_or(ProjectTrustError::InvalidRoot)?;
                    out.push(text);
                }
                Component::RootDir => out.push(component),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !out.pop() {
                        return Err(ProjectTrustError::InvalidRoot);
                    }
                }
                Component::Normal(part) => {
                    let text = part.to_str().ok_or(ProjectTrustError::InvalidRoot)?;
                    if text.contains('\0') {
                        return Err(ProjectTrustError::InvalidRoot);
                    }
                    out.push(text);
                }
            }
        }
        if !out.is_absolute() {
            return Err(ProjectTrustError::InvalidRoot);
        }
        let normalized = out.to_str().ok_or(ProjectTrustError::InvalidRoot)?;
        if normalized.is_empty() || normalized.len() > MAX_CANONICAL_ROOT_BYTES {
            return Err(ProjectTrustError::InvalidRoot);
        }
        Ok(Self(normalized.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl IdentityFingerprint {
    pub fn parse(raw: &str) -> Result<Self, ProjectTrustError> {
        parse_fingerprint(raw, ProjectTrustError::InvalidFingerprint)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl DeviceHint {
    pub fn new(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }

    pub fn device(self) -> u64 {
        self.device
    }

    pub fn inode(self) -> u64 {
        self.inode
    }
}

impl ProjectIdentity {
    /// Construct from the stable identity pair used as the store key.
    pub fn new(
        canonical_root: impl AsRef<Path>,
        vcs_remote_fingerprint: Option<&str>,
    ) -> Result<Self, ProjectTrustError> {
        Ok(Self {
            canonical_root: CanonicalRoot::parse(canonical_root)?,
            vcs_remote_fingerprint: vcs_remote_fingerprint
                .map(IdentityFingerprint::parse)
                .transpose()?,
            device_hint: None,
            manifest_hash: None,
        })
    }

    pub fn with_device_hint(mut self, hint: DeviceHint) -> Self {
        self.device_hint = Some(hint);
        self
    }

    pub fn with_manifest_hash(mut self, hash: &str) -> Result<Self, ProjectTrustError> {
        self.manifest_hash = Some(parse_fingerprint(
            hash,
            ProjectTrustError::InvalidManifestHash,
        )?);
        Ok(self)
    }

    pub fn canonical_root(&self) -> &CanonicalRoot {
        &self.canonical_root
    }

    pub fn vcs_remote_fingerprint(&self) -> Option<&IdentityFingerprint> {
        self.vcs_remote_fingerprint.as_ref()
    }

    pub fn device_hint(&self) -> Option<DeviceHint> {
        self.device_hint
    }

    pub fn manifest_hash(&self) -> Option<&IdentityFingerprint> {
        self.manifest_hash.as_ref()
    }

    fn material_eq(&self, other: &Self) -> bool {
        self.vcs_remote_fingerprint == other.vcs_remote_fingerprint
            && self.device_hint == other.device_hint
            && self.manifest_hash == other.manifest_hash
    }
}

impl ProjectTrustStore {
    /// Open a catalog at `catalog`. A missing file is an empty untrusted store.
    pub fn open(catalog: impl Into<PathBuf>) -> Self {
        Self {
            catalog: catalog.into(),
            max_records: MAX_TRUST_RECORDS,
            max_bytes: MAX_CATALOG_BYTES,
        }
    }

    pub fn catalog_path(&self) -> &Path {
        &self.catalog
    }

    /// Return stored status, or [`TrustStatus::Untrusted`] when absent.
    ///
    /// A record whose material identity no longer matches is invalidated
    /// before the untrusted status is returned.
    pub fn get(
        &self,
        identity: &ProjectIdentity,
        cancel: &CancellationToken,
    ) -> Result<TrustStatus, ProjectTrustError> {
        cancel.check().map_err(|_| ProjectTrustError::Cancelled)?;
        let mut records = self.load(cancel)?;
        cancel.check().map_err(|_| ProjectTrustError::Cancelled)?;
        match records.get(&identity.canonical_root) {
            None => Ok(TrustStatus::Untrusted),
            Some(stored) if stored.identity.material_eq(identity) => Ok(stored.status),
            Some(_) => {
                records.insert(
                    identity.canonical_root.clone(),
                    StoredRecord {
                        identity: identity.clone(),
                        status: TrustStatus::Untrusted,
                    },
                );
                self.persist(&records, cancel)?;
                Ok(TrustStatus::Untrusted)
            }
        }
    }

    /// Persist `status` for `identity`, replacing any record at the same root.
    pub fn set(
        &self,
        identity: &ProjectIdentity,
        status: TrustStatus,
        cancel: &CancellationToken,
    ) -> Result<(), ProjectTrustError> {
        cancel.check().map_err(|_| ProjectTrustError::Cancelled)?;
        let mut records = self.load(cancel)?;
        cancel.check().map_err(|_| ProjectTrustError::Cancelled)?;
        let exists = records.contains_key(&identity.canonical_root);
        if !exists && records.len() >= self.max_records {
            return Err(ProjectTrustError::TooManyRecords);
        }
        records.insert(
            identity.canonical_root.clone(),
            StoredRecord {
                identity: identity.clone(),
                status,
            },
        );
        self.persist(&records, cancel)
    }

    fn load(
        &self,
        cancel: &CancellationToken,
    ) -> Result<BTreeMap<CanonicalRoot, StoredRecord>, ProjectTrustError> {
        cancel.check().map_err(|_| ProjectTrustError::Cancelled)?;
        let bytes = match fs::read(&self.catalog) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(err) => return Err(ProjectTrustError::Io(err)),
        };
        let observed = bytes.len() as u64;
        if observed > self.max_bytes {
            return Err(ProjectTrustError::CatalogTooLarge {
                limit: self.max_bytes,
                observed,
            });
        }
        cancel.check().map_err(|_| ProjectTrustError::Cancelled)?;
        decode_catalog(&bytes, self.max_records, cancel)
    }

    fn persist(
        &self,
        records: &BTreeMap<CanonicalRoot, StoredRecord>,
        cancel: &CancellationToken,
    ) -> Result<(), ProjectTrustError> {
        cancel.check().map_err(|_| ProjectTrustError::Cancelled)?;
        if records.len() > self.max_records {
            return Err(ProjectTrustError::TooManyRecords);
        }
        let bytes = encode_catalog(records)?;
        if bytes.len() as u64 > self.max_bytes {
            return Err(ProjectTrustError::CatalogTooLarge {
                limit: self.max_bytes,
                observed: bytes.len() as u64,
            });
        }
        if let Some(parent) = self.catalog.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let tmp = part_path(&self.catalog);
        let write_result = (|| {
            cancel.check().map_err(|_| ProjectTrustError::Cancelled)?;
            let mut file = File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            cancel.check().map_err(|_| ProjectTrustError::Cancelled)?;
            fs::rename(&tmp, &self.catalog)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        write_result
    }
}

impl fmt::Debug for ProjectTrustStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectTrustStore")
            .field("catalog", &self.catalog)
            .finish()
    }
}

impl fmt::Display for ProjectTrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("operation cancelled"),
            Self::InvalidRoot => f.write_str("invalid canonical project root"),
            Self::InvalidFingerprint => f.write_str("invalid VCS remote fingerprint"),
            Self::InvalidManifestHash => f.write_str("invalid project manifest hash"),
            Self::CatalogTooLarge { .. } => f.write_str("trust catalog exceeds size bound"),
            Self::TooManyRecords => f.write_str("trust catalog exceeds record bound"),
            Self::CatalogCorrupt => f.write_str("trust catalog is corrupt"),
            Self::UnsupportedSchema { found } => {
                write!(f, "unsupported trust catalog schema {found}")
            }
            Self::Io(_) => f.write_str("trust catalog I/O failed"),
        }
    }
}

impl Error for ProjectTrustError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl PartialEq for ProjectTrustError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Cancelled, Self::Cancelled)
            | (Self::InvalidRoot, Self::InvalidRoot)
            | (Self::InvalidFingerprint, Self::InvalidFingerprint)
            | (Self::InvalidManifestHash, Self::InvalidManifestHash)
            | (Self::TooManyRecords, Self::TooManyRecords)
            | (Self::CatalogCorrupt, Self::CatalogCorrupt) => true,
            (
                Self::CatalogTooLarge {
                    limit: a_limit,
                    observed: a_obs,
                },
                Self::CatalogTooLarge {
                    limit: b_limit,
                    observed: b_obs,
                },
            ) => a_limit == b_limit && a_obs == b_obs,
            (Self::UnsupportedSchema { found: a }, Self::UnsupportedSchema { found: b }) => a == b,
            (Self::Io(a), Self::Io(b)) => a.kind() == b.kind(),
            _ => false,
        }
    }
}

impl Eq for ProjectTrustError {}

impl From<io::Error> for ProjectTrustError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

fn parse_fingerprint(
    raw: &str,
    err: ProjectTrustError,
) -> Result<IdentityFingerprint, ProjectTrustError> {
    if raw.is_empty() || raw.len() > MAX_FINGERPRINT_BYTES {
        return Err(err);
    }
    if raw.contains('\0') || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(err);
    }
    if !raw.bytes().all(is_fingerprint_byte) {
        return Err(err);
    }
    Ok(IdentityFingerprint(raw.to_ascii_lowercase()))
}

fn is_fingerprint_byte(b: u8) -> bool {
    matches!(
        b,
        b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b':' | b'.' | b'_' | b'-'
    )
}

fn part_path(catalog: &Path) -> PathBuf {
    let mut out = catalog.as_os_str().to_os_string();
    out.push(PART_SUFFIX);
    PathBuf::from(out)
}

fn encode_catalog(
    records: &BTreeMap<CanonicalRoot, StoredRecord>,
) -> Result<Vec<u8>, ProjectTrustError> {
    let mut recs = Vec::with_capacity(records.len());
    for stored in records.values() {
        let mut rec = Map::new();
        rec.insert(
            "canonical_root".into(),
            Value::String(stored.identity.canonical_root.as_str().to_owned()),
        );
        if let Some(fp) = &stored.identity.vcs_remote_fingerprint {
            rec.insert(
                "vcs_remote_fingerprint".into(),
                Value::String(fp.as_str().to_owned()),
            );
        }
        if let Some(hint) = stored.identity.device_hint {
            rec.insert("device".into(), Value::from(hint.device));
            rec.insert("inode".into(), Value::from(hint.inode));
        }
        if let Some(hash) = &stored.identity.manifest_hash {
            rec.insert(
                "manifest_hash".into(),
                Value::String(hash.as_str().to_owned()),
            );
        }
        rec.insert(
            "status".into(),
            Value::String(stored.status.as_str().to_owned()),
        );
        recs.push(Value::Object(rec));
    }
    let mut root = Map::new();
    root.insert("schema".into(), Value::from(PROJECT_TRUST_SCHEMA));
    root.insert("records".into(), Value::Array(recs));
    serde_json::to_vec(&Value::Object(root)).map_err(|_| ProjectTrustError::CatalogCorrupt)
}

fn decode_catalog(
    bytes: &[u8],
    max_records: usize,
    cancel: &CancellationToken,
) -> Result<BTreeMap<CanonicalRoot, StoredRecord>, ProjectTrustError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| ProjectTrustError::CatalogCorrupt)?;
    let obj = value.as_object().ok_or(ProjectTrustError::CatalogCorrupt)?;
    reject_unknown_keys(obj, CATALOG_KEYS)?;
    let schema = obj
        .get("schema")
        .and_then(Value::as_u64)
        .ok_or(ProjectTrustError::CatalogCorrupt)?;
    if schema != u64::from(PROJECT_TRUST_SCHEMA) {
        let found = u16::try_from(schema).unwrap_or(u16::MAX);
        return Err(ProjectTrustError::UnsupportedSchema { found });
    }
    let records_value = obj
        .get("records")
        .ok_or(ProjectTrustError::CatalogCorrupt)?;
    let items = records_value
        .as_array()
        .ok_or(ProjectTrustError::CatalogCorrupt)?;
    if items.len() > max_records {
        return Err(ProjectTrustError::TooManyRecords);
    }

    let mut records = BTreeMap::new();
    for (i, item) in items.iter().enumerate() {
        if i % CANCEL_CHECK_EVERY == 0 {
            cancel.check().map_err(|_| ProjectTrustError::Cancelled)?;
        }
        let rec = item.as_object().ok_or(ProjectTrustError::CatalogCorrupt)?;
        reject_unknown_keys(rec, RECORD_KEYS)?;
        let root = rec
            .get("canonical_root")
            .and_then(Value::as_str)
            .ok_or(ProjectTrustError::CatalogCorrupt)?;
        let mut identity =
            ProjectIdentity::new(root, optional_str(rec, "vcs_remote_fingerprint")?)?;
        match (rec.get("device"), rec.get("inode")) {
            (None, None) => {}
            (Some(device), Some(inode)) => {
                let device = device.as_u64().ok_or(ProjectTrustError::CatalogCorrupt)?;
                let inode = inode.as_u64().ok_or(ProjectTrustError::CatalogCorrupt)?;
                identity = identity.with_device_hint(DeviceHint::new(device, inode));
            }
            _ => return Err(ProjectTrustError::CatalogCorrupt),
        }
        if let Some(hash) = optional_str(rec, "manifest_hash")? {
            identity = identity.with_manifest_hash(hash)?;
        }
        let status = rec
            .get("status")
            .and_then(Value::as_str)
            .ok_or(ProjectTrustError::CatalogCorrupt)?;
        let status = TrustStatus::parse(status)?;
        if records
            .insert(
                identity.canonical_root.clone(),
                StoredRecord { identity, status },
            )
            .is_some()
        {
            return Err(ProjectTrustError::CatalogCorrupt);
        }
    }
    Ok(records)
}

fn optional_str<'a>(
    rec: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, ProjectTrustError> {
    match rec.get(key) {
        None => Ok(None),
        Some(Value::Null) => Err(ProjectTrustError::CatalogCorrupt),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or(ProjectTrustError::CatalogCorrupt),
    }
}

fn reject_unknown_keys(
    obj: &Map<String, Value>,
    allowed: &[&str],
) -> Result<(), ProjectTrustError> {
    if obj.keys().any(|key| !allowed.contains(&key.as_str())) {
        Err(ProjectTrustError::CatalogCorrupt)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    const SECRET: &str = "super-secret-password";
    const FP_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const FP_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const GOLDEN: &str = r#"{"records":[{"canonical_root":"/tmp/rapidlm-trust-golden","status":"trusted","vcs_remote_fingerprint":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}],"schema":1}"#;

    struct TempCatalog {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TempCatalog {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "rapidlm-project-trust-{}-{seq}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp trust dir");
            let path = dir.join("project-trust.json");
            Self { dir, path }
        }

        fn store(&self) -> ProjectTrustStore {
            ProjectTrustStore::open(&self.path)
        }

        fn bounded(&self, max_records: usize, max_bytes: u64) -> ProjectTrustStore {
            ProjectTrustStore {
                catalog: self.path.clone(),
                max_records,
                max_bytes,
            }
        }
    }

    impl Drop for TempCatalog {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn identity(root: &str, fingerprint: Option<&str>) -> ProjectIdentity {
        ProjectIdentity::new(root, fingerprint).unwrap_or_else(|err| {
            panic!("identity {root:?}: {err}");
        })
    }

    #[test]
    fn missing_catalog_is_untrusted() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let id = identity("/tmp/rapidlm-trust-missing", Some(FP_A));
        assert_eq!(
            store.get(&id, &live()).expect("get"),
            TrustStatus::Untrusted
        );
        assert!(!store.get(&id, &live()).expect("get").is_trusted());
        assert!(!tmp.path.exists());
    }

    #[test]
    fn set_then_get_round_trips_trusted() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let id = identity("/tmp/rapidlm-trust-roundtrip", Some(FP_A));
        store.set(&id, TrustStatus::Trusted, &live()).expect("set");
        assert_eq!(store.get(&id, &live()).expect("get"), TrustStatus::Trusted);
    }

    #[test]
    fn replay_after_reopen_preserves_trusted() {
        let tmp = TempCatalog::create();
        let id = identity("/tmp/rapidlm-trust-replay", Some(FP_A));
        tmp.store()
            .set(&id, TrustStatus::Trusted, &live())
            .expect("set");
        let reopened = tmp.store();
        assert_eq!(
            reopened.get(&id, &live()).expect("replay"),
            TrustStatus::Trusted
        );
    }

    #[test]
    fn explicit_untrusted_overwrites_grant() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let id = identity("/tmp/rapidlm-trust-revoke", Some(FP_A));
        store.set(&id, TrustStatus::Trusted, &live()).expect("set");
        store
            .set(&id, TrustStatus::Untrusted, &live())
            .expect("revoke");
        assert_eq!(
            store.get(&id, &live()).expect("get"),
            TrustStatus::Untrusted
        );
    }

    #[test]
    fn fingerprint_change_does_not_inherit_trust() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let trusted = identity("/tmp/rapidlm-trust-fp", Some(FP_A));
        store
            .set(&trusted, TrustStatus::Trusted, &live())
            .expect("set");
        let spoofed = identity("/tmp/rapidlm-trust-fp", Some(FP_B));
        assert_eq!(
            store.get(&spoofed, &live()).expect("spoof"),
            TrustStatus::Untrusted
        );
        let reopened = tmp.store();
        assert_eq!(
            reopened.get(&trusted, &live()).expect("stale identity"),
            TrustStatus::Untrusted
        );
        assert_eq!(
            reopened.get(&spoofed, &live()).expect("new identity"),
            TrustStatus::Untrusted
        );
    }

    #[test]
    fn missing_fingerprint_after_grant_is_material_change() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let trusted = identity("/tmp/rapidlm-trust-fp-optional", Some(FP_A));
        store
            .set(&trusted, TrustStatus::Trusted, &live())
            .expect("set");
        let bare = identity("/tmp/rapidlm-trust-fp-optional", None);
        assert_eq!(
            store.get(&bare, &live()).expect("bare"),
            TrustStatus::Untrusted
        );
    }

    #[test]
    fn moved_project_same_fingerprint_does_not_inherit_trust() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store
            .set(
                &identity("/tmp/rapidlm-trust-moved-a", Some(FP_A)),
                TrustStatus::Trusted,
                &live(),
            )
            .expect("set");
        assert_eq!(
            store
                .get(&identity("/tmp/rapidlm-trust-moved-b", Some(FP_A)), &live())
                .expect("moved"),
            TrustStatus::Untrusted
        );
    }

    #[test]
    fn sibling_and_parent_roots_do_not_inherit_trust() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store
            .set(
                &identity("/tmp/rapidlm-trust-root", Some(FP_A)),
                TrustStatus::Trusted,
                &live(),
            )
            .expect("set");
        assert_eq!(
            store
                .get(
                    &identity("/tmp/rapidlm-trust-root-evil", Some(FP_A)),
                    &live()
                )
                .expect("sibling"),
            TrustStatus::Untrusted
        );
        assert_eq!(
            store
                .get(&identity("/tmp", Some(FP_A)), &live())
                .expect("parent"),
            TrustStatus::Untrusted
        );
    }

    #[test]
    fn device_hint_change_invalidates_trust() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let trusted =
            identity("/tmp/rapidlm-trust-dev", Some(FP_A)).with_device_hint(DeviceHint::new(1, 10));
        store
            .set(&trusted, TrustStatus::Trusted, &live())
            .expect("set");
        let moved_inode =
            identity("/tmp/rapidlm-trust-dev", Some(FP_A)).with_device_hint(DeviceHint::new(1, 99));
        assert_eq!(
            store.get(&moved_inode, &live()).expect("inode"),
            TrustStatus::Untrusted
        );
    }

    #[test]
    fn manifest_hash_change_invalidates_trust() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let trusted = identity("/tmp/rapidlm-trust-manifest", Some(FP_A))
            .with_manifest_hash(FP_A)
            .expect("hash");
        store
            .set(&trusted, TrustStatus::Trusted, &live())
            .expect("set");
        let changed = identity("/tmp/rapidlm-trust-manifest", Some(FP_A))
            .with_manifest_hash(FP_B)
            .expect("hash");
        assert_eq!(
            store.get(&changed, &live()).expect("manifest"),
            TrustStatus::Untrusted
        );
    }

    #[test]
    fn lexical_normalization_is_the_comparison_key() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let granted = identity("/tmp/rapidlm-trust-norm/./proj", Some(FP_A));
        store
            .set(&granted, TrustStatus::Trusted, &live())
            .expect("set");
        let same = identity("/tmp/rapidlm-trust-norm/foo/../proj", Some(FP_A));
        assert_eq!(
            granted.canonical_root().as_str(),
            "/tmp/rapidlm-trust-norm/proj"
        );
        assert_eq!(same.canonical_root(), granted.canonical_root());
        assert_eq!(
            store.get(&same, &live()).expect("norm"),
            TrustStatus::Trusted
        );
    }

    #[test]
    fn relative_and_empty_roots_are_rejected() {
        assert_eq!(
            ProjectIdentity::new("relative/proj", Some(FP_A)).unwrap_err(),
            ProjectTrustError::InvalidRoot
        );
        assert_eq!(
            ProjectIdentity::new("", Some(FP_A)).unwrap_err(),
            ProjectTrustError::InvalidRoot
        );
        assert_eq!(
            ProjectIdentity::new("../etc", Some(FP_A)).unwrap_err(),
            ProjectTrustError::InvalidRoot
        );
    }

    #[test]
    fn catalog_serialization_matches_golden() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store
            .set(
                &identity("/tmp/rapidlm-trust-golden", Some(FP_A)),
                TrustStatus::Trusted,
                &live(),
            )
            .expect("set");
        let bytes = fs::read(&tmp.path).expect("read catalog");
        assert_eq!(String::from_utf8(bytes).expect("utf8"), GOLDEN);
    }

    #[test]
    fn leftover_part_file_is_not_consulted() {
        let tmp = TempCatalog::create();
        let part = part_path(&tmp.path);
        fs::write(&part, GOLDEN).expect("part");
        let store = tmp.store();
        assert_eq!(
            store
                .get(&identity("/tmp/rapidlm-trust-golden", Some(FP_A)), &live())
                .expect("part ignored"),
            TrustStatus::Untrusted
        );
    }

    #[test]
    fn corrupt_catalog_fails_closed() {
        let tmp = TempCatalog::create();
        fs::write(&tmp.path, "{not-json").expect("corrupt");
        let err = tmp
            .store()
            .get(&identity("/tmp/rapidlm-trust-corrupt", Some(FP_A)), &live())
            .expect_err("corrupt");
        assert_eq!(err, ProjectTrustError::CatalogCorrupt);
    }

    #[test]
    fn unknown_status_does_not_grant_trust() {
        let tmp = TempCatalog::create();
        fs::write(
            &tmp.path,
            r#"{"schema":1,"records":[{"canonical_root":"/tmp/rapidlm-trust-allow","status":"allow"}]}"#,
        )
        .expect("write");
        let err = tmp
            .store()
            .get(&identity("/tmp/rapidlm-trust-allow", None), &live())
            .expect_err("allow");
        assert_eq!(err, ProjectTrustError::CatalogCorrupt);
    }

    #[test]
    fn extra_catalog_key_fails_closed() {
        let tmp = TempCatalog::create();
        fs::write(&tmp.path, r#"{"schema":1,"records":[],"trusted":true}"#).expect("write");
        let err = tmp
            .store()
            .get(&identity("/tmp/rapidlm-trust-extra", None), &live())
            .expect_err("extra");
        assert_eq!(err, ProjectTrustError::CatalogCorrupt);
    }

    #[test]
    fn unsupported_schema_is_rejected() {
        let tmp = TempCatalog::create();
        fs::write(&tmp.path, r#"{"schema":2,"records":[]}"#).expect("write");
        let err = tmp
            .store()
            .get(&identity("/tmp/rapidlm-trust-schema", None), &live())
            .expect_err("schema");
        assert_eq!(err, ProjectTrustError::UnsupportedSchema { found: 2 });
    }

    #[test]
    fn oversized_catalog_is_rejected() {
        let tmp = TempCatalog::create();
        fs::write(&tmp.path, vec![b'x'; 128]).expect("write");
        let err = tmp
            .bounded(8, 64)
            .get(&identity("/tmp/rapidlm-trust-size", None), &live())
            .expect_err("size");
        assert_eq!(
            err,
            ProjectTrustError::CatalogTooLarge {
                limit: 64,
                observed: 128
            }
        );
    }

    #[test]
    fn record_bound_rejects_new_roots() {
        let tmp = TempCatalog::create();
        let store = tmp.bounded(1, MAX_CATALOG_BYTES);
        store
            .set(
                &identity("/tmp/rapidlm-trust-one", Some(FP_A)),
                TrustStatus::Trusted,
                &live(),
            )
            .expect("first");
        let err = store
            .set(
                &identity("/tmp/rapidlm-trust-two", Some(FP_A)),
                TrustStatus::Trusted,
                &live(),
            )
            .expect_err("second");
        assert_eq!(err, ProjectTrustError::TooManyRecords);
        store
            .set(
                &identity("/tmp/rapidlm-trust-one", Some(FP_B)),
                TrustStatus::Untrusted,
                &live(),
            )
            .expect("update existing");
    }

    #[test]
    fn cancelled_get_and_set_fail() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let id = identity("/tmp/rapidlm-trust-cancel", Some(FP_A));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            store.get(&id, &cancel).expect_err("get"),
            ProjectTrustError::Cancelled
        );
        assert_eq!(
            store
                .set(&id, TrustStatus::Trusted, &cancel)
                .expect_err("set"),
            ProjectTrustError::Cancelled
        );
    }

    #[test]
    fn errors_do_not_echo_rejected_fingerprint() {
        let err = ProjectIdentity::new("/tmp/proj", Some("super-secret-password@host"))
            .expect_err("secret fp");
        assert_eq!(err, ProjectTrustError::InvalidFingerprint);
        let rendered = format!("{err:?}{err}");
        assert!(!rendered.contains(SECRET));
    }

    #[test]
    fn store_debug_omits_catalog_body() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store
            .set(
                &identity("/tmp/rapidlm-trust-debug", Some(FP_A)),
                TrustStatus::Trusted,
                &live(),
            )
            .expect("set");
        let rendered = format!("{store:?}");
        assert!(rendered.contains("ProjectTrustStore"));
        assert!(!rendered.contains(FP_A));
        assert!(!rendered.contains("trusted"));
    }

    #[test]
    fn default_status_is_untrusted() {
        assert_eq!(TrustStatus::default(), TrustStatus::Untrusted);
        assert!(!TrustStatus::default().is_trusted());
        assert!(TrustStatus::Trusted.is_trusted());
    }
}
