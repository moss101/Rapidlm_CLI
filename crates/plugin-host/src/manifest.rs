//! WASM plugin manifest schema and requested-capability validation.
//!
//! Manifests are untrusted package data. Unknown privileged capabilities and
//! ambient host filesystem/network declarations fail closed (T-006). The
//! computed manifest hash is part of the trust binding used by later install
//! records; a declaration is never a grant.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use capability_broker::{
    CancellationToken, Capability, CapabilityError, FilesystemRoot, ResourceDescriptor,
};
use protocol::{
    ApiError, ArtifactId, ArtifactIdParseError, ErrorCode, RepoPath, RepoPathError, TraceId,
};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Wire schema name for [`PluginManifest`].
pub const PLUGIN_MANIFEST_SCHEMA: &str = "rapidlm.plugin_manifest";

/// v1 schema version for plugin manifests.
pub const PLUGIN_MANIFEST_SCHEMA_VERSION: u16 = 1;

/// Host plugin API version this crate loads.
pub const HOST_PLUGIN_API_VERSION: SemVer = SemVer {
    major: 1,
    minor: 0,
    patch: 0,
};

/// Supported WIT package version (`rapidlm:plugin/tool@1.0.0`).
pub const SUPPORTED_WIT_VERSION: SemVer = SemVer {
    major: 1,
    minor: 0,
    patch: 0,
};

/// Maximum UTF-8 bytes accepted for one manifest document.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// Maximum UTF-8 bytes for plugin, publisher, skill, hook, and MCP ids.
pub const MAX_IDENT_BYTES: usize = 256;

/// Maximum UTF-8 bytes for the entrypoint path plus optional export.
pub const MAX_ENTRYPOINT_BYTES: usize = 512;

/// Maximum requested capability/resource pairs.
pub const MAX_REQUESTED_CAPS: usize = 32;

/// Maximum skill, hook, or MCP-server references on one manifest.
pub const MAX_EXTENSION_REFS: usize = 32;

const CANCEL_STRIDE: usize = 16;
const MANIFEST_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "id",
    "version",
    "publisher",
    "digest",
    "entrypoint",
    "wit_version",
    "compatibility",
    "requested_caps",
    "skills",
    "hooks",
    "mcp_servers",
];
const MANIFEST_OPTIONAL: &[&str] = &["skills", "hooks", "mcp_servers"];
const REQUESTED_CAP_FIELDS: &[&str] = &["capability", "resource"];
const COMPAT_FIELDS: &[&str] = &["min", "max"];

/// Bounded plugin identity token.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PluginId(String);

/// Bounded publisher identity token.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PublisherId(String);

/// Plugin package version (`major.minor.patch`).
pub type PluginVersion = SemVer;

/// Three-component version. Pre-release and build metadata fail closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SemVer {
    major: u32,
    minor: u32,
    patch: u32,
}

/// Inclusive-min, exclusive-max host API range.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct CompatibilityRange {
    min: SemVer,
    max: SemVer,
}

/// Repo-relative WASM component path and optional WIT export.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct PluginEntrypoint {
    path: RepoPath,
    export: Option<String>,
}

/// Skill, hook, or MCP-server name declared by the package. Not a grant.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ExtensionRef(String);

/// Broker-taxonomy capability plus matching resource scope.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RequestedCapability {
    capability: Capability,
    resource: ResourceDescriptor,
}

/// Validated plugin manifest. Requested caps are not issued leases.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginManifest {
    id: PluginId,
    version: PluginVersion,
    publisher: PublisherId,
    digest: ArtifactId,
    entrypoint: PluginEntrypoint,
    wit_version: SemVer,
    compatibility: CompatibilityRange,
    requested_caps: Vec<RequestedCapability>,
    skills: Vec<ExtensionRef>,
    hooks: Vec<ExtensionRef>,
    mcp_servers: Vec<ExtensionRef>,
    manifest_hash: ArtifactId,
}

/// Identity, package digest, and manifest hash bound into a trust record.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ManifestTrustBinding {
    plugin_id: PluginId,
    version: PluginVersion,
    publisher: PublisherId,
    package_digest: ArtifactId,
    manifest_hash: ArtifactId,
}

/// Typed parse/load failure. Display never echoes untrusted fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManifestError {
    Cancelled,
    TooLarge,
    InvalidJson,
    UnsupportedSchema,
    UnknownField,
    MissingField,
    InvalidIdent,
    InvalidVersion,
    InvalidRange,
    InvalidEntrypoint,
    InvalidDigest,
    UnknownCapability,
    AmbientHostFilesystem,
    AmbientNetwork,
    FamilyMismatch,
    IncompatibleHost,
    IncompatibleWit,
    TooManyCaps,
    TooManyRefs,
    DuplicateRef,
}

impl PluginId {
    pub fn parse(value: &str) -> Result<Self, ManifestError> {
        Ok(Self(parse_ident(value)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl PublisherId {
    pub fn parse(value: &str) -> Result<Self, ManifestError> {
        Ok(Self(parse_ident(value)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SemVer {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    pub const fn major(self) -> u32 {
        self.major
    }

    pub const fn minor(self) -> u32 {
        self.minor
    }

    pub const fn patch(self) -> u32 {
        self.patch
    }

    pub fn parse(value: &str) -> Result<Self, ManifestError> {
        if value.is_empty() || value.len() > MAX_IDENT_BYTES {
            return Err(ManifestError::InvalidVersion);
        }
        if value.contains('\0') || value.chars().any(char::is_control) {
            return Err(ManifestError::InvalidVersion);
        }
        let mut parts = value.split('.');
        let major = parse_ver_part(parts.next())?;
        let minor = parse_ver_part(parts.next())?;
        let patch = parse_ver_part(parts.next())?;
        if parts.next().is_some() {
            return Err(ManifestError::InvalidVersion);
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

impl FromStr for SemVer {
    type Err = ManifestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl CompatibilityRange {
    pub fn new(min: SemVer, max: SemVer) -> Result<Self, ManifestError> {
        if max <= min {
            return Err(ManifestError::InvalidRange);
        }
        Ok(Self { min, max })
    }

    pub fn min(self) -> SemVer {
        self.min
    }

    pub fn max(self) -> SemVer {
        self.max
    }

    pub fn contains(self, version: SemVer) -> bool {
        version >= self.min && version < self.max
    }
}

impl PluginEntrypoint {
    pub fn parse(value: &str) -> Result<Self, ManifestError> {
        if value.is_empty() || value.len() > MAX_ENTRYPOINT_BYTES {
            return Err(ManifestError::InvalidEntrypoint);
        }
        if value.contains('\0') || value.chars().any(char::is_control) {
            return Err(ManifestError::InvalidEntrypoint);
        }
        let (path_raw, export) = match value.split_once('#') {
            Some((path, export)) => (path, Some(parse_export(export)?)),
            None => (value, None),
        };
        if !path_raw.ends_with(".wasm") {
            return Err(ManifestError::InvalidEntrypoint);
        }
        let path = RepoPath::parse(path_raw).map_err(map_entrypoint_path)?;
        Ok(Self { path, export })
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn export(&self) -> Option<&str> {
        self.export.as_deref()
    }

    fn as_wire(&self) -> String {
        match &self.export {
            Some(export) => format!("{}#{export}", self.path.as_str()),
            None => self.path.as_str().to_owned(),
        }
    }
}

impl ExtensionRef {
    pub fn parse(value: &str) -> Result<Self, ManifestError> {
        Ok(Self(parse_ident(value)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl RequestedCapability {
    pub fn capability(&self) -> Capability {
        self.capability
    }

    pub fn resource(&self) -> &ResourceDescriptor {
        &self.resource
    }
}

impl PluginManifest {
    pub fn id(&self) -> &PluginId {
        &self.id
    }

    pub fn version(&self) -> PluginVersion {
        self.version
    }

    pub fn publisher(&self) -> &PublisherId {
        &self.publisher
    }

    pub fn digest(&self) -> ArtifactId {
        self.digest
    }

    pub fn entrypoint(&self) -> &PluginEntrypoint {
        &self.entrypoint
    }

    pub fn wit_version(&self) -> SemVer {
        self.wit_version
    }

    pub fn compatibility(&self) -> CompatibilityRange {
        self.compatibility
    }

    pub fn requested_caps(&self) -> &[RequestedCapability] {
        &self.requested_caps
    }

    pub fn skills(&self) -> &[ExtensionRef] {
        &self.skills
    }

    pub fn hooks(&self) -> &[ExtensionRef] {
        &self.hooks
    }

    pub fn mcp_servers(&self) -> &[ExtensionRef] {
        &self.mcp_servers
    }

    /// Canonical hash of the validated manifest. Bound into trust records.
    pub fn manifest_hash(&self) -> ArtifactId {
        self.manifest_hash
    }

    pub fn trust_binding(&self) -> ManifestTrustBinding {
        ManifestTrustBinding {
            plugin_id: self.id.clone(),
            version: self.version,
            publisher: self.publisher.clone(),
            package_digest: self.digest,
            manifest_hash: self.manifest_hash,
        }
    }
}

impl ManifestTrustBinding {
    pub fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    pub fn version(&self) -> PluginVersion {
        self.version
    }

    pub fn publisher(&self) -> &PublisherId {
        &self.publisher
    }

    pub fn package_digest(&self) -> ArtifactId {
        self.package_digest
    }

    pub fn manifest_hash(&self) -> ArtifactId {
        self.manifest_hash
    }
}

/// Parse and validate a manifest for install. Compatibility is not applied.
pub fn parse_manifest(
    bytes: &[u8],
    cancel: &CancellationToken,
) -> Result<PluginManifest, ManifestError> {
    cancel.check().map_err(|_| ManifestError::Cancelled)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestError::TooLarge);
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ManifestError::InvalidJson)?;
    let manifest = decode_manifest(&value, cancel)?;
    validate_for_install(&manifest)?;
    Ok(manifest)
}

/// Parse, then require host API / WIT compatibility for load.
pub fn load_manifest(
    bytes: &[u8],
    cancel: &CancellationToken,
) -> Result<PluginManifest, ManifestError> {
    let manifest = parse_manifest(bytes, cancel)?;
    validate_for_load(&manifest)?;
    Ok(manifest)
}

/// Privilege checks required before an install record may be written.
pub fn validate_for_install(manifest: &PluginManifest) -> Result<(), ManifestError> {
    for requested in &manifest.requested_caps {
        reject_ambient(requested.capability, &requested.resource)?;
        requested
            .capability
            .compatible_with(&requested.resource)
            .map_err(map_capability_error)?;
    }
    Ok(())
}

/// Install checks plus host/WIT compatibility. Unknown caps already failed.
pub fn validate_for_load(manifest: &PluginManifest) -> Result<(), ManifestError> {
    validate_for_install(manifest)?;
    if !manifest.compatibility.contains(HOST_PLUGIN_API_VERSION) {
        return Err(ManifestError::IncompatibleHost);
    }
    if manifest.wit_version != SUPPORTED_WIT_VERSION {
        return Err(ManifestError::IncompatibleWit);
    }
    Ok(())
}

impl ManifestError {
    pub fn code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::UnknownCapability
            | Self::AmbientHostFilesystem
            | Self::AmbientNetwork
            | Self::FamilyMismatch => Some(ErrorCode::PluginCapabilityDenied),
            Self::IncompatibleHost | Self::IncompatibleWit => {
                Some(ErrorCode::PluginCapabilityDenied)
            }
            Self::TooLarge
            | Self::InvalidJson
            | Self::UnsupportedSchema
            | Self::UnknownField
            | Self::MissingField
            | Self::InvalidIdent
            | Self::InvalidVersion
            | Self::InvalidRange
            | Self::InvalidEntrypoint
            | Self::InvalidDigest
            | Self::TooManyCaps
            | Self::TooManyRefs
            | Self::DuplicateRef => Some(ErrorCode::ConfigInvalid),
        }
    }

    pub fn into_api_error(self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = self.as_str();
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "plugin manifest parse cancelled",
            Self::TooLarge => "plugin manifest exceeds the configured bound",
            Self::InvalidJson => "plugin manifest is not a closed JSON object",
            Self::UnsupportedSchema => "unsupported plugin manifest schema version",
            Self::UnknownField => "unknown plugin manifest field",
            Self::MissingField => "missing required plugin manifest field",
            Self::InvalidIdent => "plugin manifest identifier is invalid",
            Self::InvalidVersion => "plugin manifest version is invalid",
            Self::InvalidRange => "plugin manifest compatibility range is invalid",
            Self::InvalidEntrypoint => "plugin manifest entrypoint is invalid",
            Self::InvalidDigest => "plugin manifest digest is invalid",
            Self::UnknownCapability => "unknown privileged plugin capability",
            Self::AmbientHostFilesystem => "plugin manifest cannot declare ambient host filesystem",
            Self::AmbientNetwork => "plugin manifest cannot declare ambient host network",
            Self::FamilyMismatch => "plugin capability family does not match resource",
            Self::IncompatibleHost => "plugin manifest is incompatible with the host API",
            Self::IncompatibleWit => "plugin manifest WIT version is not supported",
            Self::TooManyCaps => "plugin manifest requested capability count exceeds the bound",
            Self::TooManyRefs => "plugin manifest extension reference count exceeds the bound",
            Self::DuplicateRef => "plugin manifest contains a duplicate extension reference",
        }
    }
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ManifestError {}

impl Serialize for SemVer {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl Serialize for CompatibilityRange {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CompatibilityRange", 2)?;
        state.serialize_field("min", &self.min)?;
        state.serialize_field("max", &self.max)?;
        state.end()
    }
}

impl Serialize for RequestedCapability {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("RequestedCapability", 2)?;
        state.serialize_field("capability", &self.capability)?;
        state.serialize_field("resource", &self.resource)?;
        state.end()
    }
}

impl Serialize for PluginManifest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("PluginManifest", 13)?;
        state.serialize_field("schema", PLUGIN_MANIFEST_SCHEMA)?;
        state.serialize_field("schema_version", &PLUGIN_MANIFEST_SCHEMA_VERSION)?;
        state.serialize_field("id", self.id.as_str())?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("publisher", self.publisher.as_str())?;
        state.serialize_field("digest", &self.digest)?;
        state.serialize_field("entrypoint", &self.entrypoint.as_wire())?;
        state.serialize_field("wit_version", &self.wit_version)?;
        state.serialize_field("compatibility", &self.compatibility)?;
        state.serialize_field("requested_caps", &self.requested_caps)?;
        state.serialize_field("skills", &refs_as_strings(&self.skills))?;
        state.serialize_field("hooks", &refs_as_strings(&self.hooks))?;
        state.serialize_field("mcp_servers", &refs_as_strings(&self.mcp_servers))?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for PluginManifest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        decode_manifest(&value, &CancellationToken::new()).map_err(de::Error::custom)
    }
}

fn decode_manifest(
    value: &Value,
    cancel: &CancellationToken,
) -> Result<PluginManifest, ManifestError> {
    cancel.check().map_err(|_| ManifestError::Cancelled)?;
    let object = object_map(value)?;
    expect_keys(object, MANIFEST_FIELDS, MANIFEST_OPTIONAL)?;
    expect_schema(object)?;
    let id = PluginId::parse(require_str(object, "id")?)?;
    let version = SemVer::parse(require_str(object, "version")?)?;
    let publisher = PublisherId::parse(require_str(object, "publisher")?)?;
    let digest = parse_digest(require_str(object, "digest")?)?;
    let entrypoint = PluginEntrypoint::parse(require_str(object, "entrypoint")?)?;
    let wit_version = SemVer::parse(require_str(object, "wit_version")?)?;
    let compatibility = decode_compatibility(require_object(object, "compatibility")?)?;
    let requested_caps = decode_requested_caps(require_array(object, "requested_caps")?, cancel)?;
    let skills = decode_refs(optional_array(object, "skills")?, cancel)?;
    let hooks = decode_refs(optional_array(object, "hooks")?, cancel)?;
    let mcp_servers = decode_refs(optional_array(object, "mcp_servers")?, cancel)?;

    let mut manifest = PluginManifest {
        id,
        version,
        publisher,
        digest,
        entrypoint,
        wit_version,
        compatibility,
        requested_caps,
        skills,
        hooks,
        mcp_servers,
        manifest_hash: ArtifactId::from_bytes(&[]),
    };
    let canonical = serde_json::to_vec(&manifest).map_err(|_| ManifestError::InvalidJson)?;
    manifest.manifest_hash = ArtifactId::from_bytes(&canonical);
    Ok(manifest)
}

fn decode_compatibility(object: &Map<String, Value>) -> Result<CompatibilityRange, ManifestError> {
    expect_keys(object, COMPAT_FIELDS, &[])?;
    CompatibilityRange::new(
        SemVer::parse(require_str(object, "min")?)?,
        SemVer::parse(require_str(object, "max")?)?,
    )
}

fn decode_requested_caps(
    items: &[Value],
    cancel: &CancellationToken,
) -> Result<Vec<RequestedCapability>, ManifestError> {
    if items.len() > MAX_REQUESTED_CAPS {
        return Err(ManifestError::TooManyCaps);
    }
    let mut caps = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if i % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| ManifestError::Cancelled)?;
        }
        caps.push(decode_requested_cap(item)?);
    }
    caps.sort_by(|left, right| {
        left.capability
            .as_str()
            .cmp(right.capability.as_str())
            .then_with(|| canonical_json(&left.resource).cmp(&canonical_json(&right.resource)))
    });
    Ok(caps)
}

fn decode_requested_cap(value: &Value) -> Result<RequestedCapability, ManifestError> {
    if let Some(raw) = value.as_str() {
        return Err(classify_unknown_capability(raw));
    }
    let object = object_map(value)?;
    expect_keys(object, REQUESTED_CAP_FIELDS, &[])?;
    let capability = decode_capability(
        object
            .get("capability")
            .ok_or(ManifestError::MissingField)?,
    )?;
    let resource = decode_resource(object.get("resource").ok_or(ManifestError::MissingField)?)?;
    capability
        .compatible_with(&resource)
        .map_err(map_capability_error)?;
    reject_ambient(capability, &resource)?;
    Ok(RequestedCapability {
        capability,
        resource,
    })
}

fn decode_capability(value: &Value) -> Result<Capability, ManifestError> {
    if let Some(raw) = value.as_str() {
        return raw.parse().map_err(map_capability_error);
    }
    serde_json::from_value(value.clone()).map_err(|_| ManifestError::UnknownCapability)
}

fn decode_resource(value: &Value) -> Result<ResourceDescriptor, ManifestError> {
    match serde_json::from_value::<ResourceDescriptor>(value.clone()) {
        Ok(resource) => Ok(resource),
        Err(_) => Err(classify_resource_failure(value)),
    }
}

fn decode_refs(
    items: &[Value],
    cancel: &CancellationToken,
) -> Result<Vec<ExtensionRef>, ManifestError> {
    if items.len() > MAX_EXTENSION_REFS {
        return Err(ManifestError::TooManyRefs);
    }
    let mut refs = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if i % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| ManifestError::Cancelled)?;
        }
        let raw = item.as_str().ok_or(ManifestError::InvalidIdent)?;
        refs.push(ExtensionRef::parse(raw)?);
    }
    refs.sort();
    for window in refs.windows(2) {
        if window[0] == window[1] {
            return Err(ManifestError::DuplicateRef);
        }
    }
    Ok(refs)
}

fn reject_ambient(
    _capability: Capability,
    resource: &ResourceDescriptor,
) -> Result<(), ManifestError> {
    match resource {
        ResourceDescriptor::Filesystem(scope) if scope.root() == FilesystemRoot::Host => {
            Err(ManifestError::AmbientHostFilesystem)
        }
        _ => Ok(()),
    }
}

fn classify_unknown_capability(raw: &str) -> ManifestError {
    let lowered = raw.to_ascii_lowercase();
    if is_ambient_fs_token(&lowered) {
        ManifestError::AmbientHostFilesystem
    } else if is_ambient_net_token(&lowered) {
        ManifestError::AmbientNetwork
    } else {
        ManifestError::UnknownCapability
    }
}

fn classify_resource_failure(value: &Value) -> ManifestError {
    let Some(object) = value.as_object() else {
        return ManifestError::InvalidJson;
    };
    match object.get("kind").and_then(Value::as_str) {
        Some("filesystem") if object.get("root").and_then(Value::as_str) == Some("host") => {
            ManifestError::AmbientHostFilesystem
        }
        Some("network") => ManifestError::AmbientNetwork,
        Some("wasi") | Some("host") | Some("ambient") => ManifestError::UnknownCapability,
        _ => ManifestError::UnknownCapability,
    }
}

fn is_ambient_fs_token(raw: &str) -> bool {
    raw == "fs"
        || raw == "filesystem"
        || raw == "host.fs"
        || raw == "ambient.fs"
        || raw.contains("wasi:filesystem")
        || raw.contains("wasi:cli")
}

fn is_ambient_net_token(raw: &str) -> bool {
    raw == "net"
        || raw == "network"
        || raw == "host.net"
        || raw == "ambient.net"
        || raw.contains("wasi:sockets")
        || raw.contains("wasi:http")
}

fn map_capability_error(err: CapabilityError) -> ManifestError {
    match err {
        CapabilityError::UnknownFamily | CapabilityError::UnknownAction => {
            ManifestError::UnknownCapability
        }
        CapabilityError::UnknownKind | CapabilityError::UnknownField => {
            ManifestError::UnknownCapability
        }
        CapabilityError::UnknownRoot => ManifestError::AmbientHostFilesystem,
        CapabilityError::InvalidHost | CapabilityError::UnknownScheme => {
            ManifestError::AmbientNetwork
        }
        CapabilityError::FamilyMismatch | CapabilityError::MissingScope => {
            ManifestError::FamilyMismatch
        }
        CapabilityError::UnsupportedSchema => ManifestError::UnsupportedSchema,
        CapabilityError::MissingField => ManifestError::MissingField,
        _ => ManifestError::UnknownCapability,
    }
}

fn map_entrypoint_path(err: RepoPathError) -> ManifestError {
    match err {
        RepoPathError::Empty
        | RepoPathError::TooLong
        | RepoPathError::Nul
        | RepoPathError::Control
        | RepoPathError::Absolute
        | RepoPathError::WindowsDrive
        | RepoPathError::Unc
        | RepoPathError::Traversal => ManifestError::InvalidEntrypoint,
    }
}

fn parse_digest(value: &str) -> Result<ArtifactId, ManifestError> {
    value
        .parse()
        .map_err(|_: ArtifactIdParseError| ManifestError::InvalidDigest)
}

fn parse_ident(value: &str) -> Result<String, ManifestError> {
    if value.is_empty() || value.len() > MAX_IDENT_BYTES {
        return Err(ManifestError::InvalidIdent);
    }
    if value.contains('\0') || value.chars().any(char::is_control) {
        return Err(ManifestError::InvalidIdent);
    }
    if value.split(['/', '\\']).any(|part| part == "..") {
        return Err(ManifestError::InvalidIdent);
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
    {
        return Err(ManifestError::InvalidIdent);
    }
    Ok(value.to_owned())
}

fn parse_export(value: &str) -> Result<String, ManifestError> {
    if value.is_empty() || value.len() > MAX_IDENT_BYTES {
        return Err(ManifestError::InvalidEntrypoint);
    }
    if value.contains('\0') || value.chars().any(char::is_control) {
        return Err(ManifestError::InvalidEntrypoint);
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/' | b'@'))
    {
        return Err(ManifestError::InvalidEntrypoint);
    }
    Ok(value.to_owned())
}

fn parse_ver_part(part: Option<&str>) -> Result<u32, ManifestError> {
    let part = part.ok_or(ManifestError::InvalidVersion)?;
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ManifestError::InvalidVersion);
    }
    if part.len() > 1 && part.starts_with('0') {
        return Err(ManifestError::InvalidVersion);
    }
    part.parse().map_err(|_| ManifestError::InvalidVersion)
}

fn object_map(value: &Value) -> Result<&Map<String, Value>, ManifestError> {
    value.as_object().ok_or(ManifestError::InvalidJson)
}

fn expect_schema(object: &Map<String, Value>) -> Result<(), ManifestError> {
    let got = require_str(object, "schema")?;
    if got != PLUGIN_MANIFEST_SCHEMA {
        return Err(ManifestError::UnsupportedSchema);
    }
    match object.get("schema_version") {
        Some(Value::Number(number)) => {
            let version = number
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .ok_or(ManifestError::UnsupportedSchema)?;
            if version != PLUGIN_MANIFEST_SCHEMA_VERSION {
                return Err(ManifestError::UnsupportedSchema);
            }
            Ok(())
        }
        Some(_) => Err(ManifestError::InvalidJson),
        None => Err(ManifestError::MissingField),
    }
}

fn expect_keys(
    object: &Map<String, Value>,
    required: &[&str],
    optional: &[&str],
) -> Result<(), ManifestError> {
    for key in object.keys() {
        if !required.contains(&key.as_str()) && !optional.contains(&key.as_str()) {
            return Err(ManifestError::UnknownField);
        }
    }
    for key in required {
        if optional.contains(key) {
            continue;
        }
        if !object.contains_key(*key) {
            return Err(ManifestError::MissingField);
        }
    }
    Ok(())
}

fn require_str<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, ManifestError> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(ManifestError::InvalidJson),
        None => Err(ManifestError::MissingField),
    }
}

fn require_object<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a Map<String, Value>, ManifestError> {
    match object.get(key) {
        Some(Value::Object(value)) => Ok(value),
        Some(_) => Err(ManifestError::InvalidJson),
        None => Err(ManifestError::MissingField),
    }
}

fn require_array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a [Value], ManifestError> {
    match object.get(key) {
        Some(Value::Array(value)) => Ok(value.as_slice()),
        Some(_) => Err(ManifestError::InvalidJson),
        None => Err(ManifestError::MissingField),
    }
}

fn optional_array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a [Value], ManifestError> {
    match object.get(key) {
        Some(Value::Array(value)) => Ok(value.as_slice()),
        Some(Value::Null) | None => Ok(&[]),
        Some(_) => Err(ManifestError::InvalidJson),
    }
}

fn refs_as_strings(refs: &[ExtensionRef]) -> Vec<&str> {
    refs.iter().map(ExtensionRef::as_str).collect()
}

fn canonical_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_broker::{FilesystemScope, NetworkScheme, NetworkScope};

    const PACKAGE_FIXTURE: &[u8] = b"fixture-plugin-wasm";

    fn package_digest() -> ArtifactId {
        ArtifactId::from_bytes(PACKAGE_FIXTURE)
    }

    fn valid_manifest_json() -> String {
        format!(
            r#"{{"schema":"rapidlm.plugin_manifest","schema_version":1,"id":"acme.fmt","version":"1.2.3","publisher":"acme","digest":"{digest}","entrypoint":"plugin.wasm#rapidlm:plugin/tool@1.0.0","wit_version":"1.0.0","compatibility":{{"min":"1.0.0","max":"2.0.0"}},"requested_caps":[{{"capability":{{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"}},"resource":{{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"repo","glob":"src/**/*.rs"}}}}],"skills":["fmt"],"hooks":[],"mcp_servers":[]}}"#,
            digest = package_digest()
        )
    }

    fn parse_ok(json: &str) -> PluginManifest {
        parse_manifest(json.as_bytes(), &CancellationToken::new()).expect("valid manifest")
    }

    fn parse_err(json: &str) -> ManifestError {
        parse_manifest(json.as_bytes(), &CancellationToken::new()).expect_err("must fail closed")
    }

    #[test]
    fn golden_manifest_round_trip() {
        let json = valid_manifest_json();
        let manifest = parse_ok(&json);
        assert_eq!(manifest.id().as_str(), "acme.fmt");
        assert_eq!(manifest.version(), SemVer::new(1, 2, 3));
        assert_eq!(manifest.publisher().as_str(), "acme");
        assert_eq!(manifest.digest(), package_digest());
        assert_eq!(manifest.entrypoint().path().as_str(), "plugin.wasm");
        assert_eq!(
            manifest.entrypoint().export(),
            Some("rapidlm:plugin/tool@1.0.0")
        );
        assert_eq!(manifest.wit_version(), SUPPORTED_WIT_VERSION);
        assert!(manifest.compatibility().contains(HOST_PLUGIN_API_VERSION));
        assert_eq!(manifest.requested_caps().len(), 1);
        assert_eq!(
            manifest.requested_caps()[0].capability(),
            Capability::FsRead
        );
        assert_eq!(
            manifest.requested_caps()[0].resource(),
            &ResourceDescriptor::Filesystem(FilesystemScope::repo("src/**/*.rs").expect("fs"))
        );
        let encoded = serde_json::to_string(&manifest).expect("encode");
        assert_eq!(encoded, json);
        let decoded: PluginManifest = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn empty_requested_caps_is_valid() {
        let json = valid_manifest_json().replace(
            r#""requested_caps":[{"capability":{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"},"resource":{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"repo","glob":"src/**/*.rs"}}]"#,
            r#""requested_caps":[]"#,
        );
        let manifest = parse_ok(&json);
        assert!(manifest.requested_caps().is_empty());
        validate_for_install(&manifest).expect("install");
        validate_for_load(&manifest).expect("load");
    }

    #[test]
    fn unknown_privileged_capability_fails_install_and_load() {
        for (needle, replacement) in [
            (
                r#""family":"fs","action":"read""#,
                r#""family":"superuser","action":"all""#,
            ),
            (
                r#""family":"fs","action":"read""#,
                r#""family":"fs","action":"admin""#,
            ),
            (
                r#""family":"fs","action":"read""#,
                r#""family":"*","action":"read""#,
            ),
        ] {
            let json = valid_manifest_json().replace(needle, replacement);
            assert_eq!(parse_err(&json), ManifestError::UnknownCapability);
            assert_eq!(
                load_manifest(json.as_bytes(), &CancellationToken::new()).unwrap_err(),
                ManifestError::UnknownCapability
            );
        }

        let as_string = valid_manifest_json().replace(
            r#"[{"capability":{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"},"resource":{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"repo","glob":"src/**/*.rs"}}]"#,
            r#"["host.admin"]"#,
        );
        assert_eq!(parse_err(&as_string), ManifestError::UnknownCapability);
        assert!(
            !ManifestError::UnknownCapability
                .to_string()
                .contains("superuser")
        );
        assert!(
            !ManifestError::UnknownCapability
                .to_string()
                .contains("host.admin")
        );
    }

    #[test]
    fn ambient_host_fs_and_network_fail_closed() {
        let host_fs = valid_manifest_json()
            .replace(r#""root":"repo""#, r#""root":"host""#)
            .replace(r#""glob":"src/**/*.rs""#, r#""glob":"/etc/passwd""#);
        assert_eq!(parse_err(&host_fs), ManifestError::AmbientHostFilesystem);

        let wasi_fs = valid_manifest_json().replace(
            r#"[{"capability":{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"},"resource":{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"repo","glob":"src/**/*.rs"}}]"#,
            r#"["wasi:filesystem/preopens"]"#,
        );
        assert_eq!(parse_err(&wasi_fs), ManifestError::AmbientHostFilesystem);

        let wildcard_net = valid_manifest_json().replace(
            r#"{"capability":{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"},"resource":{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"repo","glob":"src/**/*.rs"}}"#,
            r#"{"capability":{"schema":"rapidlm.capability","schema_version":1,"family":"net","action":"connect"},"resource":{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"network","scheme":"https","host":"*","port":443}}"#,
        );
        assert_eq!(parse_err(&wildcard_net), ManifestError::AmbientNetwork);

        let wasi_net = valid_manifest_json().replace(
            r#"[{"capability":{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"},"resource":{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"repo","glob":"src/**/*.rs"}}]"#,
            r#"["wasi:sockets/tcp"]"#,
        );
        assert_eq!(parse_err(&wasi_net), ManifestError::AmbientNetwork);

        assert!(
            !ManifestError::AmbientHostFilesystem
                .to_string()
                .contains("/etc/passwd")
        );
        assert!(!ManifestError::AmbientNetwork.to_string().contains('*'));
    }

    #[test]
    fn scoped_broker_network_is_not_ambient() {
        let json = valid_manifest_json().replace(
            r#"{"capability":{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"},"resource":{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"repo","glob":"src/**/*.rs"}}"#,
            r#"{"capability":{"schema":"rapidlm.capability","schema_version":1,"family":"net","action":"connect"},"resource":{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"network","scheme":"https","host":"example.com","port":443}}"#,
        );
        let manifest = parse_ok(&json);
        assert_eq!(
            manifest.requested_caps()[0].capability(),
            Capability::NetConnect
        );
        assert_eq!(
            manifest.requested_caps()[0].resource(),
            &ResourceDescriptor::Network(
                NetworkScope::new(NetworkScheme::Https, "example.com", 443).expect("net")
            )
        );
    }

    #[test]
    fn manifest_hash_participates_in_trust_record() {
        let left = parse_ok(&valid_manifest_json());
        let right_json =
            valid_manifest_json().replace(r#""glob":"src/**/*.rs""#, r#""glob":"docs/**/*.md""#);
        let right = parse_ok(&right_json);
        assert_ne!(
            left.requested_caps()[0].resource(),
            right.requested_caps()[0].resource()
        );
        assert_ne!(left.manifest_hash(), right.manifest_hash());
        let binding = left.trust_binding();
        assert_eq!(binding.manifest_hash(), left.manifest_hash());
        assert_eq!(binding.package_digest(), left.digest());
        assert_eq!(binding.plugin_id(), left.id());
        assert_eq!(binding.version(), left.version());
        let again = parse_ok(&valid_manifest_json());
        assert_eq!(again.manifest_hash(), left.manifest_hash());
    }

    #[test]
    fn incompatible_range_fails_load_not_parse() {
        let json = valid_manifest_json().replace(
            r#""min":"1.0.0","max":"2.0.0""#,
            r#""min":"2.0.0","max":"3.0.0""#,
        );
        let manifest = parse_ok(&json);
        validate_for_install(&manifest).expect("install still allowed");
        assert_eq!(
            validate_for_load(&manifest),
            Err(ManifestError::IncompatibleHost)
        );
        assert_eq!(
            load_manifest(json.as_bytes(), &CancellationToken::new()).unwrap_err(),
            ManifestError::IncompatibleHost
        );
    }

    #[test]
    fn unsupported_wit_fails_load() {
        let json =
            valid_manifest_json().replace(r#""wit_version":"1.0.0""#, r#""wit_version":"9.0.0""#);
        let manifest = parse_ok(&json);
        assert_eq!(
            validate_for_load(&manifest),
            Err(ManifestError::IncompatibleWit)
        );
    }

    #[test]
    fn unknown_fields_and_schema_fail_closed() {
        let extra = valid_manifest_json().replace(
            r#""mcp_servers":[]"#,
            r#""mcp_servers":[],"wasi":{"filesystem":true}"#,
        );
        assert_eq!(parse_err(&extra), ManifestError::UnknownField);

        let schema =
            valid_manifest_json().replace(r#""schema_version":1"#, r#""schema_version":2"#);
        assert_eq!(parse_err(&schema), ManifestError::UnsupportedSchema);
    }

    #[test]
    fn entrypoint_rejects_host_and_traversal() {
        for entry in ["/tmp/plugin.wasm", "../plugin.wasm", r"..\plugin.wasm"] {
            let encoded = serde_json::to_string(entry).expect("json string");
            let json = valid_manifest_json()
                .replace(r#""plugin.wasm#rapidlm:plugin/tool@1.0.0""#, &encoded);
            assert_eq!(parse_err(&json), ManifestError::InvalidEntrypoint);
            assert!(!ManifestError::InvalidEntrypoint.to_string().contains(entry));
        }
    }

    #[test]
    fn oversized_and_cancelled_input_fail() {
        let too_large = vec![b'{'; MAX_MANIFEST_BYTES + 1];
        assert_eq!(
            parse_manifest(&too_large, &CancellationToken::new()).unwrap_err(),
            ManifestError::TooLarge
        );
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            parse_manifest(valid_manifest_json().as_bytes(), &token).unwrap_err(),
            ManifestError::Cancelled
        );
    }

    #[test]
    fn privilege_errors_map_to_plugin_capability_denied() {
        let trace = TraceId::new();
        let err = ManifestError::UnknownCapability
            .into_api_error(trace)
            .expect("api");
        assert_eq!(err.code(), ErrorCode::PluginCapabilityDenied);
        assert!(!err.message().contains("superuser"));
        let ambient = ManifestError::AmbientHostFilesystem
            .into_api_error(trace)
            .expect("api");
        assert_eq!(ambient.code(), ErrorCode::PluginCapabilityDenied);
    }
}
