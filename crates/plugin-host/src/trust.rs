//! Extension trust ledger: source, hash/signature, grants, scope, review.
//!
//! Trust is an explicit grant. Untrusted packages never enable executable
//! code. A package-hash change invalidates executable grants unless the
//! record carries an explicit version-range policy (T-006).

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Debug};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use capability_broker::{
    CancellationToken, Capability, CapabilityError, FilesystemRoot, ResourceDescriptor,
};
use protocol::{ApiError, ArtifactId, ArtifactIdParseError, ErrorCode, TraceId};
use serde_json::{Map, Value};

use crate::manifest::{
    CompatibilityRange, ManifestTrustBinding, PluginId, PluginVersion, PublisherId, SemVer,
    MAX_REQUESTED_CAPS,
};
use crate::skills::ProjectTrust;

/// Wire schema name for the persisted extension trust catalog.
pub const TRUST_SCHEMA: &str = "rapidlm.plugin_trust";

/// Schema version accepted by readers. Unknown versions fail closed.
pub const TRUST_SCHEMA_VERSION: u16 = 1;

/// Maximum persisted trust records.
pub const MAX_TRUST_RECORDS: usize = 4096;

/// Maximum trust-catalog file size in bytes.
pub const MAX_TRUST_CATALOG_BYTES: u64 = 256 * 1024;

/// Maximum granted capability/resource pairs on one record.
pub const MAX_GRANTED_CAPS: usize = MAX_REQUESTED_CAPS;

/// Maximum UTF-8 bytes accepted in an install-source locator.
pub const MAX_SOURCE_LOCATOR_BYTES: usize = 512;

const CANCEL_CHECK_EVERY: usize = 16;
const PART_SUFFIX: &str = ".part";
const QUARANTINE_SUFFIX: &str = ".quarantine";
const QUARANTINE_SCHEMA: &str = "rapidlm.plugin_trust_quarantine";
const QUARANTINE_SCHEMA_VERSION: u16 = 1;
const CATALOG_KEYS: &[&str] = &["schema", "schema_version", "records"];
const QUARANTINE_KEYS: &[&str] = &["schema", "schema_version", "observations"];
const OBSERVATION_KEYS: &[&str] = &[
    "plugin",
    "version",
    "publisher",
    "package_hash",
    "signature",
    "install_source",
    "scope",
    "last_review",
];
const RECORD_KEYS: &[&str] = &[
    "plugin",
    "version",
    "publisher",
    "package_hash",
    "signature",
    "install_source",
    "scope",
    "status",
    "trust_policy",
    "version_range",
    "granted_capabilities",
    "last_review",
];
const SOURCE_KEYS: &[&str] = &["kind", "locator"];
const RANGE_KEYS: &[&str] = &["min", "max"];
const GRANTED_KEYS: &[&str] = &["capability", "resource"];

/// Trusted only after an explicit grant for the exact package identity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum ExtensionTrustStatus {
    #[default]
    Untrusted,
    Trusted,
}

/// Project/user applicability of a stored grant. Not a privilege grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TrustScope {
    User,
    Project,
}

/// Where the package was obtained. Locator text is untrusted data.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum InstallSourceKind {
    Marketplace,
    User,
    Project,
}

/// Install origin persisted on the ledger. Never consulted as authority.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct InstallSource {
    kind: InstallSourceKind,
    locator: String,
}

/// How a later package-hash change is evaluated.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum HashTrustPolicy {
    ExactHash,
    VersionRange,
}

/// UTC review timestamp. Wire form is `YYYY-MM-DDTHH:MM:SSZ`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ReviewTimestamp(String);

/// Plugin identity bound into a trust record.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ExtensionIdentity {
    plugin: PluginId,
    version: PluginVersion,
    publisher: PublisherId,
    package_hash: ArtifactId,
    signature: Option<ArtifactId>,
}

/// Broker-taxonomy capability plus matching resource. Not a lease.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TrustedCapability {
    capability: Capability,
    resource: ResourceDescriptor,
}

/// First-seen package. Always stored untrusted; never enables executable code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionObservation {
    identity: ExtensionIdentity,
    source: InstallSource,
    scope: TrustScope,
    last_review: ReviewTimestamp,
}

/// Explicit trust grant. The only path that enables executable capabilities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustGrant {
    identity: ExtensionIdentity,
    source: InstallSource,
    scope: TrustScope,
    policy: HashTrustPolicy,
    version_range: Option<CompatibilityRange>,
    granted: Vec<TrustedCapability>,
    last_review: ReviewTimestamp,
}

/// Persisted trust record. Inspectable after revoke.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionTrustRecord {
    identity: ExtensionIdentity,
    source: InstallSource,
    scope: TrustScope,
    status: ExtensionTrustStatus,
    policy: HashTrustPolicy,
    version_range: Option<CompatibilityRange>,
    granted: Vec<TrustedCapability>,
    last_review: ReviewTimestamp,
}

/// Catalog/inspect projection. Safe for UI; locators stay untrusted data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustInspectView {
    plugin: PluginId,
    version: PluginVersion,
    publisher: PublisherId,
    package_hash: ArtifactId,
    has_signature: bool,
    source: InstallSource,
    scope: TrustScope,
    status: ExtensionTrustStatus,
    policy: HashTrustPolicy,
    version_range: Option<CompatibilityRange>,
    granted: Vec<TrustedCapability>,
    last_review: ReviewTimestamp,
    executable_enabled: bool,
}

/// Durable per-plugin extension trust store.
pub struct ExtensionTrustStore {
    catalog: PathBuf,
    max_records: usize,
    max_bytes: u64,
}

/// Typed trust-store failure. Display never echoes names, locators, or hashes.
#[derive(Debug)]
pub enum TrustError {
    Cancelled,
    InvalidPlugin,
    InvalidPublisher,
    InvalidVersion,
    InvalidHash,
    InvalidSignature,
    InvalidSource,
    InvalidReview,
    InvalidRange,
    InvalidCapability,
    AmbientHostFilesystem,
    AmbientNetwork,
    FamilyMismatch,
    Untrusted,
    ProjectUntrusted,
    CapabilityNotAllowed,
    ExecutableDenied,
    CatalogTooLarge { limit: u64, observed: u64 },
    TooManyRecords,
    TooManyCaps,
    CatalogCorrupt,
    UnsupportedSchema { found: u16 },
    Io(io::Error),
}

impl ExtensionTrustStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Trusted => "trusted",
        }
    }

    pub const fn is_trusted(self) -> bool {
        matches!(self, Self::Trusted)
    }

    /// Executable code is enabled only after an explicit Trusted grant.
    pub const fn is_enabled(self) -> bool {
        self.is_trusted()
    }

    fn parse(raw: &str) -> Result<Self, TrustError> {
        match raw {
            "untrusted" => Ok(Self::Untrusted),
            "trusted" => Ok(Self::Trusted),
            _ => Err(TrustError::CatalogCorrupt),
        }
    }
}

impl TrustScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }

    fn parse(raw: &str) -> Result<Self, TrustError> {
        match raw {
            "user" => Ok(Self::User),
            "project" => Ok(Self::Project),
            _ => Err(TrustError::CatalogCorrupt),
        }
    }
}

impl InstallSourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Marketplace => "marketplace",
            Self::User => "user",
            Self::Project => "project",
        }
    }

    fn parse(raw: &str) -> Result<Self, TrustError> {
        match raw {
            "marketplace" => Ok(Self::Marketplace),
            "user" => Ok(Self::User),
            "project" => Ok(Self::Project),
            _ => Err(TrustError::CatalogCorrupt),
        }
    }
}

impl InstallSource {
    pub fn new(kind: InstallSourceKind, locator: &str) -> Result<Self, TrustError> {
        Ok(Self {
            kind,
            locator: parse_locator(locator)?,
        })
    }

    pub fn kind(&self) -> InstallSourceKind {
        self.kind
    }

    pub fn locator(&self) -> &str {
        &self.locator
    }
}

impl HashTrustPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExactHash => "exact_hash",
            Self::VersionRange => "version_range",
        }
    }

    fn parse(raw: &str) -> Result<Self, TrustError> {
        match raw {
            "exact_hash" => Ok(Self::ExactHash),
            "version_range" => Ok(Self::VersionRange),
            _ => Err(TrustError::CatalogCorrupt),
        }
    }
}

impl ReviewTimestamp {
    pub fn parse(raw: &str) -> Result<Self, TrustError> {
        if raw.len() != 20 {
            return Err(TrustError::InvalidReview);
        }
        let bytes = raw.as_bytes();
        if bytes[4] != b'-'
            || bytes[7] != b'-'
            || bytes[10] != b'T'
            || bytes[13] != b':'
            || bytes[16] != b':'
            || bytes[19] != b'Z'
        {
            return Err(TrustError::InvalidReview);
        }
        if !digits(&bytes[0..4])
            || !digits(&bytes[5..7])
            || !digits(&bytes[8..10])
            || !digits(&bytes[11..13])
            || !digits(&bytes[14..16])
            || !digits(&bytes[17..19])
        {
            return Err(TrustError::InvalidReview);
        }
        let month = two_digits(&bytes[5..7]);
        let day = two_digits(&bytes[8..10]);
        let hour = two_digits(&bytes[11..13]);
        let minute = two_digits(&bytes[14..16]);
        let second = two_digits(&bytes[17..19]);
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || second > 59
        {
            return Err(TrustError::InvalidReview);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ExtensionIdentity {
    pub fn new(
        plugin: &str,
        version: &str,
        publisher: &str,
        package_hash: &str,
    ) -> Result<Self, TrustError> {
        Ok(Self {
            plugin: PluginId::parse(plugin).map_err(|_| TrustError::InvalidPlugin)?,
            version: SemVer::parse(version).map_err(|_| TrustError::InvalidVersion)?,
            publisher: PublisherId::parse(publisher).map_err(|_| TrustError::InvalidPublisher)?,
            package_hash: parse_hash(package_hash)?,
            signature: None,
        })
    }

    pub fn from_binding(binding: &ManifestTrustBinding) -> Self {
        Self {
            plugin: binding.plugin_id().clone(),
            version: binding.version(),
            publisher: binding.publisher().clone(),
            package_hash: binding.package_digest(),
            signature: None,
        }
    }

    pub fn with_signature(mut self, signature: &str) -> Result<Self, TrustError> {
        self.signature = Some(parse_signature(signature)?);
        Ok(self)
    }

    pub fn plugin(&self) -> &PluginId {
        &self.plugin
    }

    pub fn version(&self) -> PluginVersion {
        self.version
    }

    pub fn publisher(&self) -> &PublisherId {
        &self.publisher
    }

    pub fn package_hash(&self) -> ArtifactId {
        self.package_hash
    }

    pub fn signature(&self) -> Option<ArtifactId> {
        self.signature
    }

    fn publisher_eq(&self, other: &Self) -> bool {
        self.publisher == other.publisher
    }

    fn package_eq(&self, other: &Self) -> bool {
        self.package_hash == other.package_hash && self.signature == other.signature
    }
}

impl TrustedCapability {
    pub fn new(capability: Capability, resource: ResourceDescriptor) -> Result<Self, TrustError> {
        capability
            .compatible_with(&resource)
            .map_err(map_capability_error)?;
        reject_ambient(capability, &resource)?;
        Ok(Self {
            capability,
            resource,
        })
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }

    pub fn resource(&self) -> &ResourceDescriptor {
        &self.resource
    }

    pub fn is_executable(&self) -> bool {
        is_executable_capability(self.capability)
    }
}

impl ExtensionObservation {
    pub fn new(
        identity: ExtensionIdentity,
        source: InstallSource,
        scope: TrustScope,
        last_review: ReviewTimestamp,
    ) -> Self {
        Self {
            identity,
            source,
            scope,
            last_review,
        }
    }

    pub fn identity(&self) -> &ExtensionIdentity {
        &self.identity
    }
}

impl TrustGrant {
    pub fn new(
        identity: ExtensionIdentity,
        source: InstallSource,
        scope: TrustScope,
        last_review: ReviewTimestamp,
    ) -> Self {
        Self {
            identity,
            source,
            scope,
            policy: HashTrustPolicy::ExactHash,
            version_range: None,
            granted: Vec::new(),
            last_review,
        }
    }

    /// Version-range trust. Hash changes inside `[min, max)` keep grants.
    pub fn with_version_range(mut self, min: SemVer, max: SemVer) -> Result<Self, TrustError> {
        let range = CompatibilityRange::new(min, max).map_err(|_| TrustError::InvalidRange)?;
        if !range.contains(self.identity.version) {
            return Err(TrustError::InvalidRange);
        }
        self.policy = HashTrustPolicy::VersionRange;
        self.version_range = Some(range);
        Ok(self)
    }

    pub fn with_granted_capabilities<I>(mut self, caps: I) -> Result<Self, TrustError>
    where
        I: IntoIterator<Item = TrustedCapability>,
    {
        self.granted = normalize_granted(caps)?;
        Ok(self)
    }

    pub fn identity(&self) -> &ExtensionIdentity {
        &self.identity
    }
}

impl ExtensionTrustRecord {
    fn untrusted(
        identity: ExtensionIdentity,
        source: InstallSource,
        scope: TrustScope,
        last_review: ReviewTimestamp,
    ) -> Self {
        Self {
            identity,
            source,
            scope,
            status: ExtensionTrustStatus::Untrusted,
            policy: HashTrustPolicy::ExactHash,
            version_range: None,
            granted: Vec::new(),
            last_review,
        }
    }

    pub fn identity(&self) -> &ExtensionIdentity {
        &self.identity
    }

    pub fn source(&self) -> &InstallSource {
        &self.source
    }

    pub fn scope(&self) -> TrustScope {
        self.scope
    }

    pub fn status(&self) -> ExtensionTrustStatus {
        self.status
    }

    pub fn policy(&self) -> HashTrustPolicy {
        self.policy
    }

    pub fn version_range(&self) -> Option<CompatibilityRange> {
        self.version_range
    }

    pub fn granted_capabilities(&self) -> &[TrustedCapability] {
        &self.granted
    }

    pub fn last_review(&self) -> &ReviewTimestamp {
        &self.last_review
    }

    pub fn executable_enabled(&self) -> bool {
        self.status.is_trusted() && self.granted.iter().any(TrustedCapability::is_executable)
    }

    pub fn inspect_view(&self) -> TrustInspectView {
        TrustInspectView {
            plugin: self.identity.plugin.clone(),
            version: self.identity.version,
            publisher: self.identity.publisher.clone(),
            package_hash: self.identity.package_hash,
            has_signature: self.identity.signature.is_some(),
            source: self.source.clone(),
            scope: self.scope,
            status: self.status,
            policy: self.policy,
            version_range: self.version_range,
            granted: self.granted.clone(),
            last_review: self.last_review.clone(),
            executable_enabled: self.executable_enabled(),
        }
    }

    fn strip_executable(&mut self) {
        self.status = ExtensionTrustStatus::Untrusted;
        self.granted.retain(|cap| !cap.is_executable());
        self.policy = HashTrustPolicy::ExactHash;
        self.version_range = None;
    }

    fn allows_version_range(&self, identity: &ExtensionIdentity) -> bool {
        self.policy == HashTrustPolicy::VersionRange
            && self.identity.publisher_eq(identity)
            && self
                .version_range
                .is_some_and(|range| range.contains(identity.version))
    }
}

impl TrustInspectView {
    pub fn plugin(&self) -> &PluginId {
        &self.plugin
    }

    pub fn version(&self) -> PluginVersion {
        self.version
    }

    pub fn publisher(&self) -> &PublisherId {
        &self.publisher
    }

    pub fn package_hash(&self) -> ArtifactId {
        self.package_hash
    }

    pub fn has_signature(&self) -> bool {
        self.has_signature
    }

    pub fn source(&self) -> &InstallSource {
        &self.source
    }

    pub fn scope(&self) -> TrustScope {
        self.scope
    }

    pub fn status(&self) -> ExtensionTrustStatus {
        self.status
    }

    pub fn policy(&self) -> HashTrustPolicy {
        self.policy
    }

    pub fn version_range(&self) -> Option<CompatibilityRange> {
        self.version_range
    }

    pub fn granted_capabilities(&self) -> &[TrustedCapability] {
        &self.granted
    }

    pub fn last_review(&self) -> &ReviewTimestamp {
        &self.last_review
    }

    pub fn executable_enabled(&self) -> bool {
        self.executable_enabled
    }

    /// JSON object for inspect/UI. Locators are data, never authority.
    pub fn to_json_value(&self) -> Value {
        let mut obj = Map::new();
        obj.insert(
            "plugin".into(),
            Value::String(self.plugin.as_str().to_owned()),
        );
        obj.insert("version".into(), Value::String(self.version.to_string()));
        obj.insert(
            "publisher".into(),
            Value::String(self.publisher.as_str().to_owned()),
        );
        obj.insert(
            "package_hash".into(),
            Value::String(self.package_hash.to_string()),
        );
        obj.insert("has_signature".into(), Value::Bool(self.has_signature));
        obj.insert("install_source".into(), encode_source(&self.source));
        obj.insert(
            "scope".into(),
            Value::String(self.scope.as_str().to_owned()),
        );
        obj.insert(
            "status".into(),
            Value::String(self.status.as_str().to_owned()),
        );
        obj.insert(
            "trust_policy".into(),
            Value::String(self.policy.as_str().to_owned()),
        );
        if let Some(range) = self.version_range {
            obj.insert("version_range".into(), encode_range(range));
        }
        obj.insert(
            "granted_capabilities".into(),
            encode_granted(&self.granted).unwrap_or(Value::Array(Vec::new())),
        );
        obj.insert(
            "last_review".into(),
            Value::String(self.last_review.as_str().to_owned()),
        );
        obj.insert(
            "executable_enabled".into(),
            Value::Bool(self.executable_enabled),
        );
        Value::Object(obj)
    }
}

impl ExtensionTrustStore {
    /// Open a catalog at `catalog`. A missing file is an empty untrusted store.
    pub fn open(catalog: impl Into<PathBuf>) -> Self {
        Self {
            catalog: catalog.into(),
            max_records: MAX_TRUST_RECORDS,
            max_bytes: MAX_TRUST_CATALOG_BYTES,
        }
    }

    pub fn catalog_path(&self) -> &Path {
        &self.catalog
    }

    /// Return the stored record, or an ephemeral untrusted default when absent.
    ///
    /// A material identity change invalidates executable grants unless the
    /// stored policy is version-range and the presented version is in range.
    pub fn get(
        &self,
        identity: &ExtensionIdentity,
        cancel: &CancellationToken,
    ) -> Result<ExtensionTrustRecord, TrustError> {
        cancel_check(cancel)?;
        let mut records = self.load(cancel)?;
        cancel_check(cancel)?;
        if self.quarantine_blocks(identity, records.get(&identity.plugin), cancel)? {
            return Ok(ExtensionTrustRecord::untrusted(
                identity.clone(),
                InstallSource::new(InstallSourceKind::User, "unregistered")?,
                TrustScope::User,
                ReviewTimestamp::parse("1970-01-01T00:00:00Z")?,
            ));
        }
        match records.get(&identity.plugin) {
            None => Ok(ExtensionTrustRecord::untrusted(
                identity.clone(),
                InstallSource::new(InstallSourceKind::User, "unregistered")?,
                TrustScope::User,
                ReviewTimestamp::parse("1970-01-01T00:00:00Z")?,
            )),
            Some(stored)
                if stored.identity.publisher_eq(identity)
                    && stored.identity.package_eq(identity) =>
            {
                Ok(stored.clone())
            }
            Some(stored) if stored.allows_version_range(identity) => {
                let mut kept = stored.clone();
                kept.identity = identity.clone();
                records.insert(identity.plugin.clone(), kept.clone());
                self.persist(&records, cancel)?;
                Ok(kept)
            }
            Some(stored) => {
                let mut revoked = stored.clone();
                revoked.identity = identity.clone();
                revoked.strip_executable();
                records.insert(identity.plugin.clone(), revoked.clone());
                self.persist(&records, cancel)?;
                Ok(revoked)
            }
        }
    }

    /// Inspect every persisted record. Missing catalogs yield an empty list.
    pub fn list(&self, cancel: &CancellationToken) -> Result<Vec<TrustInspectView>, TrustError> {
        cancel_check(cancel)?;
        let records = self.load(cancel)?;
        Ok(records
            .values()
            .map(ExtensionTrustRecord::inspect_view)
            .collect())
    }

    /// Inspect one plugin by id. Absent plugins are untrusted and disabled.
    pub fn inspect(
        &self,
        plugin: &str,
        cancel: &CancellationToken,
    ) -> Result<TrustInspectView, TrustError> {
        cancel_check(cancel)?;
        let plugin = PluginId::parse(plugin).map_err(|_| TrustError::InvalidPlugin)?;
        let records = self.load(cancel)?;
        match records.get(&plugin) {
            Some(stored) => Ok(stored.inspect_view()),
            None => Ok(ExtensionTrustRecord::untrusted(
                ExtensionIdentity {
                    plugin,
                    version: SemVer::new(0, 0, 0),
                    publisher: PublisherId::parse("unknown")
                        .map_err(|_| TrustError::InvalidPublisher)?,
                    package_hash: ArtifactId::from_bytes(&[]),
                    signature: None,
                },
                InstallSource::new(InstallSourceKind::User, "unregistered")?,
                TrustScope::User,
                ReviewTimestamp::parse("1970-01-01T00:00:00Z")?,
            )
            .inspect_view()),
        }
    }

    /// Record a discovered package. Always starts disabled/untrusted.
    ///
    /// Source `enabled`/`trust` claims are not consulted. A stored grant for
    /// the same package identity is kept; a material identity change revokes it.
    pub fn register(
        &self,
        observation: &ExtensionObservation,
        cancel: &CancellationToken,
    ) -> Result<ExtensionTrustRecord, TrustError> {
        cancel_check(cancel)?;
        let mut records = self.load(cancel)?;
        cancel_check(cancel)?;
        let existing = records.get(&observation.identity.plugin);
        let record = match existing {
            Some(stored)
                if stored.identity.publisher_eq(&observation.identity)
                    && stored.identity.package_eq(&observation.identity) =>
            {
                let mut kept = stored.clone();
                kept.source = observation.source.clone();
                kept.identity = observation.identity.clone();
                kept.scope = keep_or_narrow_scope(stored.scope, observation.scope);
                kept
            }
            Some(stored) if stored.allows_version_range(&observation.identity) => {
                let mut kept = stored.clone();
                kept.identity = observation.identity.clone();
                kept.source = observation.source.clone();
                kept.scope = keep_or_narrow_scope(stored.scope, observation.scope);
                kept
            }
            Some(stored) => {
                let mut revoked = stored.clone();
                revoked.identity = observation.identity.clone();
                revoked.source = observation.source.clone();
                revoked.scope = observation.scope;
                revoked.last_review = observation.last_review.clone();
                revoked.strip_executable();
                revoked
            }
            None => ExtensionTrustRecord {
                identity: observation.identity.clone(),
                source: observation.source.clone(),
                scope: observation.scope,
                status: ExtensionTrustStatus::Untrusted,
                policy: HashTrustPolicy::ExactHash,
                version_range: None,
                granted: Vec::new(),
                last_review: observation.last_review.clone(),
            },
        };
        self.upsert(&mut records, record, cancel)
    }

    /// Persist an untrusted observation only.
    ///
    /// Signature-failed installs must not call [`Self::register`]: that path
    /// can rebind or keep a VersionRange grant, or revoke a still-published
    /// plugin. This method never rebinds, keeps, or revokes an existing
    /// grant. A first-seen plugin is stored untrusted in the catalog; a
    /// colliding plugin id is recorded in the quarantine set so later
    /// `authorize_executable` of the rejected hash fails closed.
    pub fn observe_untrusted(
        &self,
        observation: &ExtensionObservation,
        cancel: &CancellationToken,
    ) -> Result<ExtensionTrustRecord, TrustError> {
        cancel_check(cancel)?;
        let mut records = self.load(cancel)?;
        cancel_check(cancel)?;
        let untrusted = ExtensionTrustRecord::untrusted(
            observation.identity.clone(),
            observation.source.clone(),
            observation.scope,
            observation.last_review.clone(),
        );
        if records.contains_key(&observation.identity.plugin) {
            self.record_quarantine_identity(observation, cancel)?;
            return Ok(untrusted);
        }
        self.upsert(&mut records, untrusted, cancel)
    }

    /// Persist an explicit trust grant. This is the only path that enables code.
    pub fn grant(
        &self,
        grant: &TrustGrant,
        cancel: &CancellationToken,
    ) -> Result<ExtensionTrustRecord, TrustError> {
        cancel_check(cancel)?;
        let mut records = self.load(cancel)?;
        cancel_check(cancel)?;
        let record = ExtensionTrustRecord {
            identity: grant.identity.clone(),
            source: grant.source.clone(),
            scope: grant.scope,
            status: ExtensionTrustStatus::Trusted,
            policy: grant.policy,
            version_range: grant.version_range,
            granted: grant.granted.clone(),
            last_review: grant.last_review.clone(),
        };
        self.upsert(&mut records, record, cancel)
    }

    /// Mark a plugin untrusted. The audit record remains inspectable.
    pub fn revoke(
        &self,
        identity: &ExtensionIdentity,
        last_review: ReviewTimestamp,
        cancel: &CancellationToken,
    ) -> Result<ExtensionTrustRecord, TrustError> {
        cancel_check(cancel)?;
        let mut records = self.load(cancel)?;
        cancel_check(cancel)?;
        let mut record = match records.get(&identity.plugin) {
            Some(stored) => stored.clone(),
            None => ExtensionTrustRecord::untrusted(
                identity.clone(),
                InstallSource::new(InstallSourceKind::User, "unregistered")?,
                TrustScope::User,
                last_review.clone(),
            ),
        };
        record.identity = identity.clone();
        record.last_review = last_review;
        record.strip_executable();
        self.upsert(&mut records, record, cancel)
    }

    /// Update last review without changing grants.
    pub fn mark_reviewed(
        &self,
        identity: &ExtensionIdentity,
        last_review: ReviewTimestamp,
        cancel: &CancellationToken,
    ) -> Result<ExtensionTrustRecord, TrustError> {
        let mut record = self.get(identity, cancel)?;
        record.last_review = last_review;
        let mut records = self.load(cancel)?;
        self.upsert(&mut records, record, cancel)
    }

    /// Executable code is allowed only after an explicit trusted grant.
    pub fn authorize_executable(
        &self,
        identity: &ExtensionIdentity,
        project_trust: ProjectTrust,
        cancel: &CancellationToken,
    ) -> Result<(), TrustError> {
        let record = self.get(identity, cancel)?;
        deny_if_project_untrusted(&record, project_trust)?;
        if !record.status.is_trusted() {
            return Err(TrustError::Untrusted);
        }
        if record.executable_enabled() {
            Ok(())
        } else {
            Err(TrustError::ExecutableDenied)
        }
    }

    /// Capability scope is narrowing only. Untrusted plugins have no usable scope.
    pub fn authorize_capability(
        &self,
        identity: &ExtensionIdentity,
        capability: Capability,
        resource: &ResourceDescriptor,
        project_trust: ProjectTrust,
        cancel: &CancellationToken,
    ) -> Result<(), TrustError> {
        let record = self.get(identity, cancel)?;
        deny_if_project_untrusted(&record, project_trust)?;
        if !record.status.is_trusted() {
            return Err(TrustError::Untrusted);
        }
        let requested = TrustedCapability::new(capability, resource.clone())?;
        if record.granted.iter().any(|granted| granted == &requested) {
            Ok(())
        } else {
            Err(TrustError::CapabilityNotAllowed)
        }
    }

    fn upsert(
        &self,
        records: &mut BTreeMap<PluginId, ExtensionTrustRecord>,
        record: ExtensionTrustRecord,
        cancel: &CancellationToken,
    ) -> Result<ExtensionTrustRecord, TrustError> {
        let exists = records.contains_key(&record.identity.plugin);
        if !exists && records.len() >= self.max_records {
            return Err(TrustError::TooManyRecords);
        }
        records.insert(record.identity.plugin.clone(), record.clone());
        self.persist(records, cancel)?;
        Ok(record)
    }

    fn load(
        &self,
        cancel: &CancellationToken,
    ) -> Result<BTreeMap<PluginId, ExtensionTrustRecord>, TrustError> {
        cancel_check(cancel)?;
        let bytes = match fs::read(&self.catalog) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(err) => return Err(TrustError::Io(err)),
        };
        let observed = bytes.len() as u64;
        if observed > self.max_bytes {
            return Err(TrustError::CatalogTooLarge {
                limit: self.max_bytes,
                observed,
            });
        }
        cancel_check(cancel)?;
        decode_catalog(&bytes, self.max_records, cancel)
    }

    fn persist(
        &self,
        records: &BTreeMap<PluginId, ExtensionTrustRecord>,
        cancel: &CancellationToken,
    ) -> Result<(), TrustError> {
        cancel_check(cancel)?;
        if records.len() > self.max_records {
            return Err(TrustError::TooManyRecords);
        }
        let bytes = encode_catalog(records)?;
        if bytes.len() as u64 > self.max_bytes {
            return Err(TrustError::CatalogTooLarge {
                limit: self.max_bytes,
                observed: bytes.len() as u64,
            });
        }
        if let Some(parent) = self.catalog.parent()
            && !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        let tmp = part_path(&self.catalog);
        let write_result = (|| {
            cancel_check(cancel)?;
            let mut file = File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            cancel_check(cancel)?;
            fs::rename(&tmp, &self.catalog)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        write_result
    }

    fn quarantine_path(&self) -> PathBuf {
        let mut out = self.catalog.as_os_str().to_os_string();
        out.push(QUARANTINE_SUFFIX);
        PathBuf::from(out)
    }

    fn quarantine_blocks(
        &self,
        identity: &ExtensionIdentity,
        stored: Option<&ExtensionTrustRecord>,
        cancel: &CancellationToken,
    ) -> Result<bool, TrustError> {
        if let Some(stored) = stored
            && stored.identity.publisher_eq(identity) && stored.identity.package_eq(identity) {
                return Ok(false);
            }
        let observations = self.load_quarantine(cancel)?;
        Ok(observations
            .iter()
            .any(|observed| pins_rejected_package(identity, observed.identity())))
    }

    fn record_quarantine_identity(
        &self,
        observation: &ExtensionObservation,
        cancel: &CancellationToken,
    ) -> Result<(), TrustError> {
        cancel_check(cancel)?;
        let mut observations = self.load_quarantine(cancel)?;
        if observations
            .iter()
            .any(|stored| pins_rejected_package(observation.identity(), stored.identity()))
        {
            return Ok(());
        }
        if observations.len() >= self.max_records {
            return Err(TrustError::TooManyRecords);
        }
        observations.push(observation.clone());
        self.persist_quarantine(&observations, cancel)
    }

    fn load_quarantine(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<ExtensionObservation>, TrustError> {
        cancel_check(cancel)?;
        let path = self.quarantine_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(TrustError::Io(err)),
        };
        let observed = bytes.len() as u64;
        if observed > self.max_bytes {
            return Err(TrustError::CatalogTooLarge {
                limit: self.max_bytes,
                observed,
            });
        }
        cancel_check(cancel)?;
        decode_quarantine(&bytes, self.max_records, cancel)
    }

    fn persist_quarantine(
        &self,
        observations: &[ExtensionObservation],
        cancel: &CancellationToken,
    ) -> Result<(), TrustError> {
        cancel_check(cancel)?;
        if observations.len() > self.max_records {
            return Err(TrustError::TooManyRecords);
        }
        let bytes = encode_quarantine(observations)?;
        if bytes.len() as u64 > self.max_bytes {
            return Err(TrustError::CatalogTooLarge {
                limit: self.max_bytes,
                observed: bytes.len() as u64,
            });
        }
        let path = self.quarantine_path();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        let tmp = part_path(&path);
        let write_result = (|| {
            cancel_check(cancel)?;
            let mut file = File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            cancel_check(cancel)?;
            fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        write_result
    }
}

impl Debug for ExtensionTrustStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtensionTrustStore")
            .field("catalog", &self.catalog)
            .finish()
    }
}

impl TrustError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::Untrusted
            | Self::ProjectUntrusted
            | Self::CapabilityNotAllowed
            | Self::ExecutableDenied
            | Self::AmbientHostFilesystem
            | Self::AmbientNetwork
            | Self::FamilyMismatch => Some(ErrorCode::PluginCapabilityDenied),
            Self::InvalidPlugin
            | Self::InvalidPublisher
            | Self::InvalidVersion
            | Self::InvalidHash
            | Self::InvalidSignature
            | Self::InvalidSource
            | Self::InvalidReview
            | Self::InvalidRange
            | Self::InvalidCapability
            | Self::TooManyCaps
            | Self::TooManyRecords => Some(ErrorCode::ConfigInvalid),
            Self::CatalogTooLarge { .. }
            | Self::CatalogCorrupt
            | Self::UnsupportedSchema { .. } => Some(ErrorCode::StorageCorrupt),
            Self::Io(_) => Some(ErrorCode::InternalUnexpected),
        }
    }

    pub fn into_api_error(self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = match &self {
            Self::Cancelled => return None,
            Self::UnsupportedSchema { found } => {
                return Some(
                    ApiError::new(
                        code,
                        format!("unsupported plugin trust catalog schema {found}"),
                        trace_id,
                    )
                    .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
                );
            }
            other => other.as_str(),
        };
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "plugin trust operation cancelled",
            Self::InvalidPlugin => "plugin identity is invalid",
            Self::InvalidPublisher => "plugin publisher is invalid",
            Self::InvalidVersion => "plugin version is invalid",
            Self::InvalidHash => "plugin package hash is invalid",
            Self::InvalidSignature => "plugin package signature is invalid",
            Self::InvalidSource => "plugin install source is invalid",
            Self::InvalidReview => "plugin review timestamp is invalid",
            Self::InvalidRange => "plugin version-range trust is invalid",
            Self::InvalidCapability => "plugin granted capability is invalid",
            Self::AmbientHostFilesystem => "plugin trust cannot grant ambient host filesystem",
            Self::AmbientNetwork => "plugin trust cannot grant ambient host network",
            Self::FamilyMismatch => "plugin capability family does not match resource",
            Self::Untrusted => "plugin is not trusted",
            Self::ProjectUntrusted => "project-scoped plugin requires project trust",
            Self::CapabilityNotAllowed => "plugin capability is outside the trusted scope",
            Self::ExecutableDenied => "plugin executable capability is not granted",
            Self::CatalogTooLarge { .. } => "plugin trust catalog exceeds size bound",
            Self::TooManyRecords => "plugin trust catalog exceeds record bound",
            Self::TooManyCaps => "plugin granted capability count exceeds the bound",
            Self::CatalogCorrupt => "plugin trust catalog is corrupt",
            Self::UnsupportedSchema { .. } => "unsupported plugin trust catalog schema",
            Self::Io(_) => "plugin trust catalog I/O failed",
        }
    }
}

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema { found } => {
                write!(f, "unsupported plugin trust catalog schema {found}")
            }
            other => f.write_str(other.as_str()),
        }
    }
}

impl Error for TrustError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl PartialEq for TrustError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Cancelled, Self::Cancelled)
            | (Self::InvalidPlugin, Self::InvalidPlugin)
            | (Self::InvalidPublisher, Self::InvalidPublisher)
            | (Self::InvalidVersion, Self::InvalidVersion)
            | (Self::InvalidHash, Self::InvalidHash)
            | (Self::InvalidSignature, Self::InvalidSignature)
            | (Self::InvalidSource, Self::InvalidSource)
            | (Self::InvalidReview, Self::InvalidReview)
            | (Self::InvalidRange, Self::InvalidRange)
            | (Self::InvalidCapability, Self::InvalidCapability)
            | (Self::AmbientHostFilesystem, Self::AmbientHostFilesystem)
            | (Self::AmbientNetwork, Self::AmbientNetwork)
            | (Self::FamilyMismatch, Self::FamilyMismatch)
            | (Self::Untrusted, Self::Untrusted)
            | (Self::ProjectUntrusted, Self::ProjectUntrusted)
            | (Self::CapabilityNotAllowed, Self::CapabilityNotAllowed)
            | (Self::ExecutableDenied, Self::ExecutableDenied)
            | (Self::TooManyRecords, Self::TooManyRecords)
            | (Self::TooManyCaps, Self::TooManyCaps)
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

impl Eq for TrustError {}

impl From<io::Error> for TrustError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

fn deny_if_project_untrusted(
    record: &ExtensionTrustRecord,
    project_trust: ProjectTrust,
) -> Result<(), TrustError> {
    if record.scope == TrustScope::Project && !project_trust.is_trusted() {
        Err(TrustError::ProjectUntrusted)
    } else {
        Ok(())
    }
}

/// Re-registration cannot widen a stored grant. User→Project is narrower.
fn keep_or_narrow_scope(stored: TrustScope, observed: TrustScope) -> TrustScope {
    match (stored, observed) {
        (TrustScope::User, TrustScope::Project) => TrustScope::Project,
        (stored, _) => stored,
    }
}

fn is_executable_capability(capability: Capability) -> bool {
    matches!(capability, Capability::ProcExec | Capability::PluginInvoke)
}

fn reject_ambient(
    _capability: Capability,
    resource: &ResourceDescriptor,
) -> Result<(), TrustError> {
    match resource {
        ResourceDescriptor::Filesystem(scope) if scope.root() == FilesystemRoot::Host => {
            Err(TrustError::AmbientHostFilesystem)
        }
        _ => Ok(()),
    }
}

fn map_capability_error(err: CapabilityError) -> TrustError {
    match err {
        CapabilityError::FamilyMismatch | CapabilityError::MissingScope => {
            TrustError::FamilyMismatch
        }
        CapabilityError::UnknownRoot => TrustError::AmbientHostFilesystem,
        CapabilityError::InvalidHost | CapabilityError::UnknownScheme => TrustError::AmbientNetwork,
        _ => TrustError::InvalidCapability,
    }
}

fn normalize_granted<I>(caps: I) -> Result<Vec<TrustedCapability>, TrustError>
where
    I: IntoIterator<Item = TrustedCapability>,
{
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for cap in caps {
        cap.capability
            .compatible_with(&cap.resource)
            .map_err(map_capability_error)?;
        reject_ambient(cap.capability, &cap.resource)?;
        let key = grant_key(&cap)?;
        if !seen.insert(key) {
            continue;
        }
        if out.len() >= MAX_GRANTED_CAPS {
            return Err(TrustError::TooManyCaps);
        }
        out.push(cap);
    }
    out.sort_by(|left, right| {
        left.capability
            .as_str()
            .cmp(right.capability.as_str())
            .then_with(|| {
                grant_key(left)
                    .unwrap_or_default()
                    .cmp(&grant_key(right).unwrap_or_default())
            })
    });
    Ok(out)
}

fn grant_key(cap: &TrustedCapability) -> Result<String, TrustError> {
    let capability =
        serde_json::to_string(&cap.capability).map_err(|_| TrustError::InvalidCapability)?;
    let resource =
        serde_json::to_string(&cap.resource).map_err(|_| TrustError::InvalidCapability)?;
    Ok(format!("{capability}|{resource}"))
}

fn parse_hash(value: &str) -> Result<ArtifactId, TrustError> {
    value
        .parse()
        .map_err(|_: ArtifactIdParseError| TrustError::InvalidHash)
}

fn parse_signature(value: &str) -> Result<ArtifactId, TrustError> {
    value
        .parse()
        .map_err(|_: ArtifactIdParseError| TrustError::InvalidSignature)
}

fn parse_locator(value: &str) -> Result<String, TrustError> {
    if value.is_empty() || value.len() > MAX_SOURCE_LOCATOR_BYTES {
        return Err(TrustError::InvalidSource);
    }
    if value.contains('\0') || value.chars().any(char::is_control) {
        return Err(TrustError::InvalidSource);
    }
    Ok(value.to_owned())
}

fn digits(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| b.is_ascii_digit())
}

fn two_digits(bytes: &[u8]) -> u8 {
    (bytes[0] - b'0') * 10 + (bytes[1] - b'0')
}

fn part_path(catalog: &Path) -> PathBuf {
    let mut out = catalog.as_os_str().to_os_string();
    out.push(PART_SUFFIX);
    PathBuf::from(out)
}

fn cancel_check(cancel: &CancellationToken) -> Result<(), TrustError> {
    cancel.check().map_err(|_| TrustError::Cancelled)
}

fn encode_catalog(
    records: &BTreeMap<PluginId, ExtensionTrustRecord>,
) -> Result<Vec<u8>, TrustError> {
    let mut recs = Vec::with_capacity(records.len());
    for stored in records.values() {
        let mut rec = Map::new();
        rec.insert(
            "plugin".into(),
            Value::String(stored.identity.plugin.as_str().to_owned()),
        );
        rec.insert(
            "version".into(),
            Value::String(stored.identity.version.to_string()),
        );
        rec.insert(
            "publisher".into(),
            Value::String(stored.identity.publisher.as_str().to_owned()),
        );
        rec.insert(
            "package_hash".into(),
            Value::String(stored.identity.package_hash.to_string()),
        );
        if let Some(signature) = stored.identity.signature {
            rec.insert("signature".into(), Value::String(signature.to_string()));
        }
        rec.insert("install_source".into(), encode_source(&stored.source));
        rec.insert(
            "scope".into(),
            Value::String(stored.scope.as_str().to_owned()),
        );
        rec.insert(
            "status".into(),
            Value::String(stored.status.as_str().to_owned()),
        );
        rec.insert(
            "trust_policy".into(),
            Value::String(stored.policy.as_str().to_owned()),
        );
        if let Some(range) = stored.version_range {
            rec.insert("version_range".into(), encode_range(range));
        }
        rec.insert(
            "granted_capabilities".into(),
            encode_granted(&stored.granted)?,
        );
        rec.insert(
            "last_review".into(),
            Value::String(stored.last_review.as_str().to_owned()),
        );
        recs.push(Value::Object(rec));
    }
    let mut root = Map::new();
    root.insert("schema".into(), Value::String(TRUST_SCHEMA.to_owned()));
    root.insert("schema_version".into(), Value::from(TRUST_SCHEMA_VERSION));
    root.insert("records".into(), Value::Array(recs));
    serde_json::to_vec(&Value::Object(root)).map_err(|_| TrustError::CatalogCorrupt)
}

fn pins_rejected_package(identity: &ExtensionIdentity, observed: &ExtensionIdentity) -> bool {
    identity.plugin == observed.plugin
        && identity.package_hash == observed.package_hash
        && identity.signature == observed.signature
}

fn encode_quarantine(observations: &[ExtensionObservation]) -> Result<Vec<u8>, TrustError> {
    let mut items = Vec::with_capacity(observations.len());
    for observation in observations {
        let mut rec = Map::new();
        rec.insert(
            "plugin".into(),
            Value::String(observation.identity.plugin.as_str().to_owned()),
        );
        rec.insert(
            "version".into(),
            Value::String(observation.identity.version.to_string()),
        );
        rec.insert(
            "publisher".into(),
            Value::String(observation.identity.publisher.as_str().to_owned()),
        );
        rec.insert(
            "package_hash".into(),
            Value::String(observation.identity.package_hash.to_string()),
        );
        if let Some(signature) = observation.identity.signature {
            rec.insert("signature".into(), Value::String(signature.to_string()));
        }
        rec.insert("install_source".into(), encode_source(&observation.source));
        rec.insert(
            "scope".into(),
            Value::String(observation.scope.as_str().to_owned()),
        );
        rec.insert(
            "last_review".into(),
            Value::String(observation.last_review.as_str().to_owned()),
        );
        items.push(Value::Object(rec));
    }
    let mut root = Map::new();
    root.insert("schema".into(), Value::String(QUARANTINE_SCHEMA.to_owned()));
    root.insert(
        "schema_version".into(),
        Value::from(QUARANTINE_SCHEMA_VERSION),
    );
    root.insert("observations".into(), Value::Array(items));
    serde_json::to_vec(&Value::Object(root)).map_err(|_| TrustError::CatalogCorrupt)
}

fn decode_quarantine(
    bytes: &[u8],
    max_records: usize,
    cancel: &CancellationToken,
) -> Result<Vec<ExtensionObservation>, TrustError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| TrustError::CatalogCorrupt)?;
    let obj = value.as_object().ok_or(TrustError::CatalogCorrupt)?;
    reject_unknown_keys(obj, QUARANTINE_KEYS)?;
    let schema = obj
        .get("schema")
        .and_then(Value::as_str)
        .ok_or(TrustError::CatalogCorrupt)?;
    if schema != QUARANTINE_SCHEMA {
        return Err(TrustError::CatalogCorrupt);
    }
    let version = obj
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or(TrustError::CatalogCorrupt)?;
    if version != u64::from(QUARANTINE_SCHEMA_VERSION) {
        let found = u16::try_from(version).unwrap_or(u16::MAX);
        return Err(TrustError::UnsupportedSchema { found });
    }
    let items = obj
        .get("observations")
        .and_then(Value::as_array)
        .ok_or(TrustError::CatalogCorrupt)?;
    if items.len() > max_records {
        return Err(TrustError::TooManyRecords);
    }
    let mut observations = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if i % CANCEL_CHECK_EVERY == 0 {
            cancel_check(cancel)?;
        }
        let rec = item.as_object().ok_or(TrustError::CatalogCorrupt)?;
        reject_unknown_keys(rec, OBSERVATION_KEYS)?;
        let plugin = rec
            .get("plugin")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let version = rec
            .get("version")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let publisher = rec
            .get("publisher")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let package_hash = rec
            .get("package_hash")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let mut identity = ExtensionIdentity::new(plugin, version, publisher, package_hash)?;
        if let Some(signature) = optional_str(rec, "signature")? {
            identity = identity.with_signature(signature)?;
        }
        let source = decode_source(
            rec.get("install_source")
                .ok_or(TrustError::CatalogCorrupt)?,
        )?;
        let scope = TrustScope::parse(
            rec.get("scope")
                .and_then(Value::as_str)
                .ok_or(TrustError::CatalogCorrupt)?,
        )?;
        let last_review = ReviewTimestamp::parse(
            rec.get("last_review")
                .and_then(Value::as_str)
                .ok_or(TrustError::CatalogCorrupt)?,
        )?;
        observations.push(ExtensionObservation::new(
            identity,
            source,
            scope,
            last_review,
        ));
    }
    Ok(observations)
}

fn encode_source(source: &InstallSource) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "kind".into(),
        Value::String(source.kind.as_str().to_owned()),
    );
    obj.insert("locator".into(), Value::String(source.locator.clone()));
    Value::Object(obj)
}

fn encode_range(range: CompatibilityRange) -> Value {
    let mut obj = Map::new();
    obj.insert("min".into(), Value::String(range.min().to_string()));
    obj.insert("max".into(), Value::String(range.max().to_string()));
    Value::Object(obj)
}

fn encode_granted(caps: &[TrustedCapability]) -> Result<Value, TrustError> {
    let mut items = Vec::with_capacity(caps.len());
    for cap in caps {
        let mut obj = Map::new();
        obj.insert(
            "capability".into(),
            serde_json::to_value(cap.capability).map_err(|_| TrustError::InvalidCapability)?,
        );
        obj.insert(
            "resource".into(),
            serde_json::to_value(&cap.resource).map_err(|_| TrustError::InvalidCapability)?,
        );
        items.push(Value::Object(obj));
    }
    Ok(Value::Array(items))
}

fn decode_catalog(
    bytes: &[u8],
    max_records: usize,
    cancel: &CancellationToken,
) -> Result<BTreeMap<PluginId, ExtensionTrustRecord>, TrustError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| TrustError::CatalogCorrupt)?;
    let obj = value.as_object().ok_or(TrustError::CatalogCorrupt)?;
    reject_unknown_keys(obj, CATALOG_KEYS)?;
    let schema = obj
        .get("schema")
        .and_then(Value::as_str)
        .ok_or(TrustError::CatalogCorrupt)?;
    if schema != TRUST_SCHEMA {
        return Err(TrustError::CatalogCorrupt);
    }
    let version = obj
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or(TrustError::CatalogCorrupt)?;
    if version != u64::from(TRUST_SCHEMA_VERSION) {
        let found = u16::try_from(version).unwrap_or(u16::MAX);
        return Err(TrustError::UnsupportedSchema { found });
    }
    let items = obj
        .get("records")
        .and_then(Value::as_array)
        .ok_or(TrustError::CatalogCorrupt)?;
    if items.len() > max_records {
        return Err(TrustError::TooManyRecords);
    }

    let mut records = BTreeMap::new();
    for (i, item) in items.iter().enumerate() {
        if i % CANCEL_CHECK_EVERY == 0 {
            cancel_check(cancel)?;
        }
        let rec = item.as_object().ok_or(TrustError::CatalogCorrupt)?;
        reject_unknown_keys(rec, RECORD_KEYS)?;
        let plugin = rec
            .get("plugin")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let version = rec
            .get("version")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let publisher = rec
            .get("publisher")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let package_hash = rec
            .get("package_hash")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let mut identity = ExtensionIdentity::new(plugin, version, publisher, package_hash)?;
        if let Some(signature) = optional_str(rec, "signature")? {
            identity = identity.with_signature(signature)?;
        }
        let source = decode_source(
            rec.get("install_source")
                .ok_or(TrustError::CatalogCorrupt)?,
        )?;
        let scope = rec
            .get("scope")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let scope = TrustScope::parse(scope)?;
        let status = rec
            .get("status")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let status = ExtensionTrustStatus::parse(status)?;
        let policy = rec
            .get("trust_policy")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let policy = HashTrustPolicy::parse(policy)?;
        let version_range = match rec.get("version_range") {
            None => None,
            Some(value) => Some(decode_range(value)?),
        };
        match (policy, version_range) {
            (HashTrustPolicy::ExactHash, Some(_)) => return Err(TrustError::CatalogCorrupt),
            (HashTrustPolicy::VersionRange, None) => return Err(TrustError::CatalogCorrupt),
            (HashTrustPolicy::VersionRange, Some(range)) if !range.contains(identity.version) => {
                return Err(TrustError::CatalogCorrupt);
            }
            _ => {}
        }
        let granted = decode_granted(rec.get("granted_capabilities"))?;
        let last_review = rec
            .get("last_review")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let last_review = ReviewTimestamp::parse(last_review)?;
        if records
            .insert(
                identity.plugin.clone(),
                ExtensionTrustRecord {
                    identity,
                    source,
                    scope,
                    status,
                    policy,
                    version_range,
                    granted,
                    last_review,
                },
            )
            .is_some()
        {
            return Err(TrustError::CatalogCorrupt);
        }
    }
    Ok(records)
}

fn decode_source(value: &Value) -> Result<InstallSource, TrustError> {
    let obj = value.as_object().ok_or(TrustError::CatalogCorrupt)?;
    reject_unknown_keys(obj, SOURCE_KEYS)?;
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(TrustError::CatalogCorrupt)?;
    let locator = obj
        .get("locator")
        .and_then(Value::as_str)
        .ok_or(TrustError::CatalogCorrupt)?;
    InstallSource::new(InstallSourceKind::parse(kind)?, locator)
}

fn decode_range(value: &Value) -> Result<CompatibilityRange, TrustError> {
    let obj = value.as_object().ok_or(TrustError::CatalogCorrupt)?;
    reject_unknown_keys(obj, RANGE_KEYS)?;
    let min = obj
        .get("min")
        .and_then(Value::as_str)
        .ok_or(TrustError::CatalogCorrupt)?;
    let max = obj
        .get("max")
        .and_then(Value::as_str)
        .ok_or(TrustError::CatalogCorrupt)?;
    CompatibilityRange::new(
        SemVer::parse(min).map_err(|_| TrustError::InvalidRange)?,
        SemVer::parse(max).map_err(|_| TrustError::InvalidRange)?,
    )
    .map_err(|_| TrustError::InvalidRange)
}

fn decode_granted(value: Option<&Value>) -> Result<Vec<TrustedCapability>, TrustError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value.as_array().ok_or(TrustError::CatalogCorrupt)?;
    if items.len() > MAX_GRANTED_CAPS {
        return Err(TrustError::TooManyCaps);
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let obj = item.as_object().ok_or(TrustError::CatalogCorrupt)?;
        reject_unknown_keys(obj, GRANTED_KEYS)?;
        let capability = obj.get("capability").ok_or(TrustError::CatalogCorrupt)?;
        let resource = obj.get("resource").ok_or(TrustError::CatalogCorrupt)?;
        let capability: Capability = serde_json::from_value(capability.clone())
            .map_err(|_| TrustError::InvalidCapability)?;
        let resource: ResourceDescriptor = serde_json::from_value(resource.clone())
            .map_err(|_| classify_resource_failure(resource))?;
        out.push(TrustedCapability::new(capability, resource)?);
    }
    normalize_granted(out)
}

fn classify_resource_failure(value: &Value) -> TrustError {
    let Some(object) = value.as_object() else {
        return TrustError::InvalidCapability;
    };
    match object.get("kind").and_then(Value::as_str) {
        Some("filesystem") if object.get("root").and_then(Value::as_str) == Some("host") => {
            TrustError::AmbientHostFilesystem
        }
        Some("network") => TrustError::AmbientNetwork,
        _ => TrustError::InvalidCapability,
    }
}

fn optional_str<'a>(rec: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, TrustError> {
    match rec.get(key) {
        None => Ok(None),
        Some(Value::Null) => Err(TrustError::CatalogCorrupt),
        Some(value) => value.as_str().map(Some).ok_or(TrustError::CatalogCorrupt),
    }
}

fn reject_unknown_keys(obj: &Map<String, Value>, allowed: &[&str]) -> Result<(), TrustError> {
    if obj.keys().any(|key| !allowed.contains(&key.as_str())) {
        Err(TrustError::CatalogCorrupt)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use capability_broker::{FilesystemScope, PluginScope, ProcessScope};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    const SECRET: &str = "super-secret-password";
    const HASH_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SIG_A: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const SIG_B: &str = "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const REVIEW: &str = "2026-01-15T12:00:00Z";
    const REVIEW_LATER: &str = "2026-02-01T00:00:00Z";
    const LOCATOR: &str = "marketplace:acme.fmt";
    const EVIL_LOCATOR: &str = "https://evil.example/plugin?token=super-secret-password";
    const GOLDEN: &str = concat!(
        r#"{"records":[{"granted_capabilities":[{"capability":{"action":"invoke","family":"plugin","schema":"rapidlm.capability","schema_version":1},"resource":{"capability":"format","kind":"plugin","plugin":"acme.fmt","schema":"rapidlm.resource_descriptor","schema_version":1}}],"install_source":{"kind":"marketplace","locator":"marketplace:acme.fmt"},"last_review":"2026-01-15T12:00:00Z","package_hash":""#,
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        r#"","plugin":"acme.fmt","publisher":"acme","scope":"user","signature":""#,
        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        r#"","status":"trusted","trust_policy":"exact_hash","version":"1.2.3"}],"#,
        r#""schema":"rapidlm.plugin_trust","schema_version":1}"#
    );

    struct TempCatalog {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TempCatalog {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("rapidlm-plugin-trust-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp trust dir");
            let path = dir.join("plugin-trust.json");
            Self { dir, path }
        }

        fn store(&self) -> ExtensionTrustStore {
            ExtensionTrustStore::open(&self.path)
        }

        fn bounded(&self, max_records: usize, max_bytes: u64) -> ExtensionTrustStore {
            ExtensionTrustStore {
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

    fn review() -> ReviewTimestamp {
        ReviewTimestamp::parse(REVIEW).expect("review")
    }

    fn later() -> ReviewTimestamp {
        ReviewTimestamp::parse(REVIEW_LATER).expect("later")
    }

    fn source() -> InstallSource {
        InstallSource::new(InstallSourceKind::Marketplace, LOCATOR).expect("source")
    }

    fn identity(plugin: &str, version: &str, hash: &str) -> ExtensionIdentity {
        ExtensionIdentity::new(plugin, version, "acme", hash)
            .unwrap_or_else(|err| panic!("identity {plugin:?}: {err}"))
            .with_signature(SIG_A)
            .expect("signature")
    }

    fn invoke_cap() -> TrustedCapability {
        TrustedCapability::new(
            Capability::PluginInvoke,
            ResourceDescriptor::Plugin(
                PluginScope::new("acme.fmt", "format").expect("plugin scope"),
            ),
        )
        .expect("invoke")
    }

    fn read_cap() -> TrustedCapability {
        TrustedCapability::new(
            Capability::FsRead,
            ResourceDescriptor::Filesystem(FilesystemScope::repo("src/**/*.rs").expect("fs")),
        )
        .expect("read")
    }

    fn grant_fmt() -> TrustGrant {
        TrustGrant::new(
            identity("acme.fmt", "1.2.3", HASH_A),
            source(),
            TrustScope::User,
            review(),
        )
        .with_granted_capabilities([invoke_cap()])
        .expect("caps")
    }

    #[test]
    fn missing_catalog_is_untrusted_and_disabled() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let id = identity("acme.fmt", "1.2.3", HASH_A);
        let record = store.get(&id, &live()).expect("get");
        assert_eq!(record.status(), ExtensionTrustStatus::Untrusted);
        assert!(!record.status().is_enabled());
        assert!(!record.executable_enabled());
        assert!(!tmp.path.exists());
        assert_eq!(
            store.authorize_executable(&id, ProjectTrust::Trusted, &live()),
            Err(TrustError::Untrusted)
        );
    }

    #[test]
    fn register_starts_disabled_and_does_not_enable_executable() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let id = identity("acme.fmt", "1.2.3", HASH_A);
        let observation =
            ExtensionObservation::new(id.clone(), source(), TrustScope::User, review());
        let record = store.register(&observation, &live()).expect("register");
        assert_eq!(record.status(), ExtensionTrustStatus::Untrusted);
        assert!(!record.executable_enabled());
        assert_eq!(
            store.authorize_executable(&id, ProjectTrust::Trusted, &live()),
            Err(TrustError::Untrusted)
        );
    }

    #[test]
    fn project_enabled_claim_is_not_a_grant() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let claimed_enabled = true;
        let claimed_trusted = true;
        assert!(claimed_enabled && claimed_trusted);
        let id = identity("acme.fmt", "1.2.3", HASH_A);
        let record = store
            .register(
                &ExtensionObservation::new(id.clone(), source(), TrustScope::Project, review()),
                &live(),
            )
            .expect("register");
        assert_eq!(record.status(), ExtensionTrustStatus::Untrusted);
        assert_eq!(
            store.authorize_executable(&id, ProjectTrust::Trusted, &live()),
            Err(TrustError::Untrusted)
        );
    }

    #[test]
    fn grant_persists_source_hash_signature_caps_scope_and_review() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let granted = store.grant(&grant_fmt(), &live()).expect("grant");
        assert_eq!(granted.status(), ExtensionTrustStatus::Trusted);
        assert_eq!(granted.source().kind(), InstallSourceKind::Marketplace);
        assert_eq!(granted.source().locator(), LOCATOR);
        assert_eq!(granted.identity().package_hash().to_string(), HASH_A);
        assert_eq!(
            granted
                .identity()
                .signature()
                .map(|s| s.to_string())
                .as_deref(),
            Some(SIG_A)
        );
        assert_eq!(granted.scope(), TrustScope::User);
        assert_eq!(granted.last_review().as_str(), REVIEW);
        assert_eq!(granted.granted_capabilities().len(), 1);
        assert!(granted.executable_enabled());
        store
            .authorize_executable(
                &identity("acme.fmt", "1.2.3", HASH_A),
                ProjectTrust::Untrusted,
                &live(),
            )
            .expect("user scope ignores project trust");
    }

    #[test]
    fn trust_is_inspectable_and_revocable() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store.grant(&grant_fmt(), &live()).expect("grant");
        let view = store.inspect("acme.fmt", &live()).expect("inspect");
        assert_eq!(view.status(), ExtensionTrustStatus::Trusted);
        assert!(view.executable_enabled());
        assert_eq!(view.source().locator(), LOCATOR);
        assert_eq!(view.last_review().as_str(), REVIEW);
        let listed = store.list(&live()).expect("list");
        assert_eq!(listed.len(), 1);
        let id = identity("acme.fmt", "1.2.3", HASH_A);
        store.revoke(&id, later(), &live()).expect("revoke");
        let revoked = store.inspect("acme.fmt", &live()).expect("inspect revoked");
        assert_eq!(revoked.status(), ExtensionTrustStatus::Untrusted);
        assert!(!revoked.executable_enabled());
        assert_eq!(revoked.last_review().as_str(), REVIEW_LATER);
        assert_eq!(revoked.source().locator(), LOCATOR);
        assert_eq!(
            store.authorize_executable(&id, ProjectTrust::Trusted, &live()),
            Err(TrustError::Untrusted)
        );
    }

    #[test]
    fn replay_after_reopen_preserves_trusted() {
        let tmp = TempCatalog::create();
        let id = identity("acme.fmt", "1.2.3", HASH_A);
        tmp.store().grant(&grant_fmt(), &live()).expect("grant");
        let reopened = tmp.store();
        assert_eq!(
            reopened.get(&id, &live()).expect("replay").status(),
            ExtensionTrustStatus::Trusted
        );
        reopened
            .authorize_executable(&id, ProjectTrust::Trusted, &live())
            .expect("executable");
    }

    #[test]
    fn leftover_part_file_is_not_consulted() {
        let tmp = TempCatalog::create();
        let part = part_path(&tmp.path);
        fs::write(&part, GOLDEN).expect("part");
        let store = tmp.store();
        let record = store
            .get(&identity("acme.fmt", "1.2.3", HASH_A), &live())
            .expect("part ignored");
        assert_eq!(record.status(), ExtensionTrustStatus::Untrusted);
        assert!(!record.executable_enabled());
    }

    #[test]
    fn hash_change_invalidates_executable_grant() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store.grant(&grant_fmt(), &live()).expect("grant");
        let spoofed = identity("acme.fmt", "1.2.3", HASH_B);
        let record = store.get(&spoofed, &live()).expect("spoof");
        assert_eq!(record.status(), ExtensionTrustStatus::Untrusted);
        assert!(!record.executable_enabled());
        assert_eq!(
            store.authorize_executable(&spoofed, ProjectTrust::Trusted, &live()),
            Err(TrustError::Untrusted)
        );
        let original = identity("acme.fmt", "1.2.3", HASH_A);
        assert_eq!(
            store.authorize_executable(&original, ProjectTrust::Trusted, &live()),
            Err(TrustError::Untrusted)
        );
    }

    #[test]
    fn version_range_trust_keeps_executable_grant_across_hash_change() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let grant = TrustGrant::new(
            identity("acme.fmt", "1.2.3", HASH_A),
            source(),
            TrustScope::User,
            review(),
        )
        .with_version_range(SemVer::new(1, 0, 0), SemVer::new(2, 0, 0))
        .expect("range")
        .with_granted_capabilities([invoke_cap()])
        .expect("caps");
        store.grant(&grant, &live()).expect("grant");
        let updated = identity("acme.fmt", "1.4.0", HASH_B);
        store
            .authorize_executable(&updated, ProjectTrust::Trusted, &live())
            .expect("range covers");
        let kept = store.get(&updated, &live()).expect("kept");
        assert_eq!(kept.status(), ExtensionTrustStatus::Trusted);
        assert!(kept.executable_enabled());
        assert_eq!(kept.identity().package_hash().to_string(), HASH_B);
    }

    #[test]
    fn version_range_outside_range_invalidates_executable_grant() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let grant = TrustGrant::new(
            identity("acme.fmt", "1.2.3", HASH_A),
            source(),
            TrustScope::User,
            review(),
        )
        .with_version_range(SemVer::new(1, 0, 0), SemVer::new(2, 0, 0))
        .expect("range")
        .with_granted_capabilities([invoke_cap()])
        .expect("caps");
        store.grant(&grant, &live()).expect("grant");
        let outside = identity("acme.fmt", "2.0.0", HASH_B);
        assert_eq!(
            store.authorize_executable(&outside, ProjectTrust::Trusted, &live()),
            Err(TrustError::Untrusted)
        );
    }

    #[test]
    fn signature_mismatch_invalidates_exact_hash_grant() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store.grant(&grant_fmt(), &live()).expect("grant");
        let spoofed = ExtensionIdentity::new("acme.fmt", "1.2.3", "acme", HASH_A)
            .expect("id")
            .with_signature(SIG_B)
            .expect("sig");
        assert_eq!(
            store.authorize_executable(&spoofed, ProjectTrust::Trusted, &live()),
            Err(TrustError::Untrusted)
        );
    }

    #[test]
    fn trusted_without_executable_cap_cannot_run_code() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let grant = TrustGrant::new(
            identity("acme.fmt", "1.2.3", HASH_A),
            source(),
            TrustScope::User,
            review(),
        )
        .with_granted_capabilities([read_cap()])
        .expect("caps");
        store.grant(&grant, &live()).expect("grant");
        let id = identity("acme.fmt", "1.2.3", HASH_A);
        store
            .authorize_capability(
                &id,
                Capability::FsRead,
                read_cap().resource(),
                ProjectTrust::Trusted,
                &live(),
            )
            .expect("read");
        assert_eq!(
            store.authorize_executable(&id, ProjectTrust::Trusted, &live()),
            Err(TrustError::ExecutableDenied)
        );
    }

    #[test]
    fn capability_outside_granted_scope_is_denied() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store.grant(&grant_fmt(), &live()).expect("grant");
        assert_eq!(
            store.authorize_capability(
                &identity("acme.fmt", "1.2.3", HASH_A),
                Capability::ProcExec,
                &ResourceDescriptor::Process(ProcessScope::new("cargo").expect("proc")),
                ProjectTrust::Trusted,
                &live(),
            ),
            Err(TrustError::CapabilityNotAllowed)
        );
    }

    #[test]
    fn project_scoped_plugin_requires_project_trust() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let grant = TrustGrant::new(
            identity("acme.fmt", "1.2.3", HASH_A),
            source(),
            TrustScope::Project,
            review(),
        )
        .with_granted_capabilities([invoke_cap()])
        .expect("caps");
        store.grant(&grant, &live()).expect("grant");
        let id = identity("acme.fmt", "1.2.3", HASH_A);
        assert_eq!(
            store.authorize_executable(&id, ProjectTrust::Untrusted, &live()),
            Err(TrustError::ProjectUntrusted)
        );
        store
            .authorize_executable(&id, ProjectTrust::Trusted, &live())
            .expect("project trusted");
    }

    #[test]
    fn sibling_plugin_does_not_inherit_trust() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store.grant(&grant_fmt(), &live()).expect("grant");
        assert_eq!(
            store
                .get(&identity("acme.fmt-evil", "1.2.3", HASH_A), &live())
                .expect("sibling")
                .status(),
            ExtensionTrustStatus::Untrusted
        );
    }

    #[test]
    fn register_cannot_widen_project_grant_to_user_scope() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let id = identity("acme.fmt", "1.2.3", HASH_A);
        let grant = TrustGrant::new(id.clone(), source(), TrustScope::Project, review())
            .with_granted_capabilities([invoke_cap()])
            .expect("caps");
        store.grant(&grant, &live()).expect("grant");
        let reregistered = store
            .register(
                &ExtensionObservation::new(id.clone(), source(), TrustScope::User, later()),
                &live(),
            )
            .expect("reregister");
        assert_eq!(reregistered.scope(), TrustScope::Project);
        assert_eq!(reregistered.status(), ExtensionTrustStatus::Trusted);
        assert!(reregistered.executable_enabled());
        assert_eq!(
            store.authorize_executable(&id, ProjectTrust::Untrusted, &live()),
            Err(TrustError::ProjectUntrusted)
        );
        store
            .authorize_executable(&id, ProjectTrust::Trusted, &live())
            .expect("project trusted still required");
    }

    #[test]
    fn register_cannot_widen_version_range_project_grant_to_user_scope() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let grant = TrustGrant::new(
            identity("acme.fmt", "1.2.3", HASH_A),
            source(),
            TrustScope::Project,
            review(),
        )
        .with_version_range(SemVer::new(1, 0, 0), SemVer::new(2, 0, 0))
        .expect("range")
        .with_granted_capabilities([invoke_cap()])
        .expect("caps");
        store.grant(&grant, &live()).expect("grant");
        let updated = identity("acme.fmt", "1.4.0", HASH_B);
        let reregistered = store
            .register(
                &ExtensionObservation::new(updated.clone(), source(), TrustScope::User, later()),
                &live(),
            )
            .expect("range reregister");
        assert_eq!(reregistered.scope(), TrustScope::Project);
        assert_eq!(reregistered.status(), ExtensionTrustStatus::Trusted);
        assert_eq!(
            store.authorize_executable(&updated, ProjectTrust::Untrusted, &live()),
            Err(TrustError::ProjectUntrusted)
        );
    }

    #[test]
    fn register_does_not_wipe_matching_grant() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let id = identity("acme.fmt", "1.2.3", HASH_A);
        store
            .register(
                &ExtensionObservation::new(id.clone(), source(), TrustScope::User, review()),
                &live(),
            )
            .expect("register");
        store.grant(&grant_fmt(), &live()).expect("grant");
        let again = store
            .register(
                &ExtensionObservation::new(id, source(), TrustScope::User, later()),
                &live(),
            )
            .expect("reregister");
        assert_eq!(again.status(), ExtensionTrustStatus::Trusted);
        assert!(again.executable_enabled());
    }

    #[test]
    fn ambient_host_filesystem_grant_is_rejected() {
        let err = TrustedCapability::new(
            Capability::FsRead,
            ResourceDescriptor::Filesystem(FilesystemScope::host("/etc/**").expect("host")),
        )
        .expect_err("ambient");
        assert_eq!(err, TrustError::AmbientHostFilesystem);
    }

    #[test]
    fn catalog_serialization_matches_golden() {
        let tmp = TempCatalog::create();
        tmp.store().grant(&grant_fmt(), &live()).expect("grant");
        let bytes = fs::read(&tmp.path).expect("read catalog");
        assert_eq!(String::from_utf8(bytes).expect("utf8"), GOLDEN);
    }

    #[test]
    fn inspect_view_does_not_treat_source_as_authority() {
        let tmp = TempCatalog::create();
        let record = tmp.store().grant(&grant_fmt(), &live()).expect("grant");
        let json = record.inspect_view().to_json_value();
        assert_eq!(json.get("status").and_then(Value::as_str), Some("trusted"));
        assert_eq!(
            json.get("executable_enabled").and_then(Value::as_bool),
            Some(true)
        );
        assert!(json.get("token").is_none());
        assert!(json.get("enabled").is_none());
    }

    #[test]
    fn corrupt_catalog_fails_closed() {
        let tmp = TempCatalog::create();
        fs::write(&tmp.path, "{not-json").expect("corrupt");
        let err = tmp
            .store()
            .get(&identity("acme.fmt", "1.2.3", HASH_A), &live())
            .expect_err("corrupt");
        assert_eq!(err, TrustError::CatalogCorrupt);
    }

    #[test]
    fn unknown_status_does_not_grant_trust() {
        let tmp = TempCatalog::create();
        fs::write(
            &tmp.path,
            r#"{"schema":"rapidlm.plugin_trust","schema_version":1,"records":[{"plugin":"acme.fmt","version":"1.0.0","publisher":"acme","package_hash":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","install_source":{"kind":"user","locator":"user:local"},"scope":"user","status":"allow","trust_policy":"exact_hash","granted_capabilities":[],"last_review":"2026-01-15T12:00:00Z"}]}"#,
        )
        .expect("write");
        let err = tmp.store().inspect("acme.fmt", &live()).expect_err("allow");
        assert_eq!(err, TrustError::CatalogCorrupt);
    }

    #[test]
    fn extra_catalog_key_fails_closed() {
        let tmp = TempCatalog::create();
        fs::write(
            &tmp.path,
            r#"{"schema":"rapidlm.plugin_trust","schema_version":1,"records":[],"trusted":true}"#,
        )
        .expect("write");
        let err = tmp.store().list(&live()).expect_err("extra");
        assert_eq!(err, TrustError::CatalogCorrupt);
    }

    #[test]
    fn unsupported_schema_is_rejected() {
        let tmp = TempCatalog::create();
        fs::write(
            &tmp.path,
            r#"{"schema":"rapidlm.plugin_trust","schema_version":2,"records":[]}"#,
        )
        .expect("write");
        let err = tmp.store().list(&live()).expect_err("schema");
        assert_eq!(err, TrustError::UnsupportedSchema { found: 2 });
    }

    #[test]
    fn oversized_catalog_is_rejected() {
        let tmp = TempCatalog::create();
        fs::write(&tmp.path, vec![b'x'; 128]).expect("write");
        let err = tmp
            .bounded(8, 64)
            .inspect("acme.fmt", &live())
            .expect_err("size");
        assert_eq!(
            err,
            TrustError::CatalogTooLarge {
                limit: 64,
                observed: 128
            }
        );
    }

    #[test]
    fn record_bound_rejects_new_plugins() {
        let tmp = TempCatalog::create();
        let store = tmp.bounded(1, MAX_TRUST_CATALOG_BYTES);
        store.grant(&grant_fmt(), &live()).expect("first");
        let err = store
            .grant(
                &TrustGrant::new(
                    identity("acme.other", "1.0.0", HASH_A),
                    source(),
                    TrustScope::User,
                    review(),
                ),
                &live(),
            )
            .expect_err("second");
        assert_eq!(err, TrustError::TooManyRecords);
    }

    #[test]
    fn cancelled_operations_fail_closed() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let cancel = live();
        cancel.cancel();
        assert_eq!(
            store
                .get(&identity("acme.fmt", "1.2.3", HASH_A), &cancel)
                .expect_err("cancel"),
            TrustError::Cancelled
        );
        assert!(store
            .get(&identity("acme.fmt", "1.2.3", HASH_A), &cancel)
            .expect_err("cancel")
            .code()
            .is_none());
    }

    #[test]
    fn invalid_plugin_and_hash_are_rejected() {
        assert_eq!(
            ExtensionIdentity::new("../etc", "1.0.0", "acme", HASH_A).unwrap_err(),
            TrustError::InvalidPlugin
        );
        assert_eq!(
            ExtensionIdentity::new("acme.fmt", "1.0.0", "acme", "not-a-hash").unwrap_err(),
            TrustError::InvalidHash
        );
        assert_eq!(
            InstallSource::new(InstallSourceKind::Marketplace, "").unwrap_err(),
            TrustError::InvalidSource
        );
    }

    #[test]
    fn display_does_not_echo_locator_or_secret() {
        let err = TrustError::Untrusted;
        let text = err.to_string();
        assert!(!text.contains(SECRET));
        assert!(!text.contains(EVIL_LOCATOR));
        assert!(!text.contains("acme.fmt"));
        assert_eq!(
            InstallSource::new(InstallSourceKind::Marketplace, EVIL_LOCATOR)
                .expect("locator is data")
                .locator(),
            EVIL_LOCATOR
        );
    }

    #[test]
    fn mark_reviewed_updates_timestamp_only() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store.grant(&grant_fmt(), &live()).expect("grant");
        let id = identity("acme.fmt", "1.2.3", HASH_A);
        let reviewed = store.mark_reviewed(&id, later(), &live()).expect("review");
        assert_eq!(reviewed.last_review().as_str(), REVIEW_LATER);
        assert_eq!(reviewed.status(), ExtensionTrustStatus::Trusted);
        assert!(reviewed.executable_enabled());
    }
}
