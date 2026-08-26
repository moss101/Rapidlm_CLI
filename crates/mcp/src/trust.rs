//! MCP server trust, credential-handle, and allowed-scope records.
//!
//! Project-configured servers start disabled. Trust is an explicit grant.
//! Records persist [`SecretRef`] handles only — plaintext is rejected at the
//! boundary and never enters a catalog or model-visible view (T-001, T-007).

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Debug};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use auth::SecretRef;
use capability_broker::CancellationToken;
use protocol::{ApiError, ErrorCode, TraceId};
use serde_json::{Map, Value};

use crate::catalog::{
    CapabilityHint, CatalogMeta, CatalogTrust, ExternalToolId, MAX_ITEMS_PER_KIND, MAX_SERVERS,
    McpServerId,
};

/// Wire schema name for the persisted trust catalog.
pub const TRUST_SCHEMA: &str = "rapidlm.mcp_trust";

/// Schema version accepted by readers.
pub const TRUST_SCHEMA_VERSION: u16 = 1;

/// Maximum persisted trust records (same bound as the catalog cache).
pub const MAX_TRUST_RECORDS: usize = MAX_SERVERS;

/// Maximum trust-catalog file size in bytes.
pub const MAX_TRUST_CATALOG_BYTES: u64 = 256 * 1024;

/// Maximum allowed-tool names stored on one record.
pub const MAX_ALLOWED_TOOLS: usize = MAX_ITEMS_PER_KIND;

/// Maximum allowed capability hints stored on one record.
pub const MAX_ALLOWED_CAPABILITIES: usize = 32;

/// Maximum UTF-8 bytes accepted in an endpoint fingerprint.
pub const MAX_FINGERPRINT_BYTES: usize = 80;

const CANCEL_CHECK_EVERY: usize = 16;
const PART_SUFFIX: &str = ".part";
const CATALOG_KEYS: &[&str] = &["schema", "schema_version", "records"];
const RECORD_KEYS: &[&str] = &[
    "server",
    "origin",
    "status",
    "endpoint_fingerprint",
    "credential",
    "allowed_tools",
    "allowed_capabilities",
];
const CREDENTIAL_KEYS: &[&str] = &["id", "alias"];

/// Trusted only after an explicit grant for the exact server identity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum McpTrustStatus {
    #[default]
    Untrusted,
    Trusted,
}

/// Where the server definition originated. Not a privilege grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ServerOrigin {
    User,
    Project,
}

/// Credential supplied by project/user config. Plaintext is always rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectCredential {
    /// Opaque handle. Never carries secret plaintext.
    Handle(SecretRef),
    /// Project file offered a token/password string. Fail closed.
    Plaintext,
}

/// Digest of the transport endpoint. Comparison uses this form only.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct EndpointFingerprint(String);

/// Canonical MCP server identity used as the trust key.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct McpServerIdentity {
    server: McpServerId,
    endpoint_fingerprint: Option<EndpointFingerprint>,
}

/// Project-config ingest. `enabled`/`trust` claims are not grants.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectMcpServerSpec {
    identity: McpServerIdentity,
    credential: Option<SecretRef>,
    requested_tools: BTreeSet<String>,
    requested_capabilities: Vec<CapabilityHint>,
}

/// Explicit trust grant. Allowed scopes are narrowing only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustGrant {
    identity: McpServerIdentity,
    origin: ServerOrigin,
    credential: Option<SecretRef>,
    allowed_tools: BTreeSet<String>,
    allowed_capabilities: Vec<CapabilityHint>,
}

/// Persisted trust record. Credential handle is not catalog/model-visible.
#[derive(Clone, Eq, PartialEq)]
pub struct McpTrustRecord {
    identity: McpServerIdentity,
    origin: ServerOrigin,
    status: McpTrustStatus,
    credential: Option<SecretRef>,
    allowed_tools: BTreeSet<String>,
    allowed_capabilities: Vec<CapabilityHint>,
}

/// Catalog/model projection. No credential handle or plaintext field exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustCatalogView {
    server: McpServerId,
    origin: ServerOrigin,
    status: McpTrustStatus,
    has_credential: bool,
    allowed_tools: BTreeSet<String>,
    allowed_capabilities: Vec<CapabilityHint>,
}

/// Durable per-server MCP trust store.
pub struct McpTrustStore {
    catalog: PathBuf,
    max_records: usize,
    max_bytes: u64,
}

/// Typed trust-store failure. Display never echoes names, tokens, or paths.
#[derive(Debug)]
pub enum TrustError {
    Cancelled,
    InvalidServer,
    InvalidTool,
    InvalidCapability,
    InvalidFingerprint,
    InvalidCredential,
    Untrusted,
    CredentialDenied,
    ToolNotAllowed,
    CapabilityNotAllowed,
    CatalogTooLarge { limit: u64, observed: u64 },
    TooManyRecords,
    TooManyScopes,
    CatalogCorrupt,
    UnsupportedSchema { found: u16 },
    Io(io::Error),
}

impl McpTrustStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Trusted => "trusted",
        }
    }

    pub const fn is_trusted(self) -> bool {
        matches!(self, Self::Trusted)
    }

    /// Project-configured servers start disabled; only Trusted is enabled.
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

impl ServerOrigin {
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

impl EndpointFingerprint {
    pub fn parse(raw: &str) -> Result<Self, TrustError> {
        if raw.is_empty() || raw.len() > MAX_FINGERPRINT_BYTES {
            return Err(TrustError::InvalidFingerprint);
        }
        if raw.contains('\0') || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(TrustError::InvalidFingerprint);
        }
        if !raw.bytes().all(is_fingerprint_byte) {
            return Err(TrustError::InvalidFingerprint);
        }
        Ok(Self(raw.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl McpServerIdentity {
    pub fn new(server: &str) -> Result<Self, TrustError> {
        Ok(Self {
            server: parse_server(server)?,
            endpoint_fingerprint: None,
        })
    }

    pub fn with_endpoint_fingerprint(mut self, fingerprint: &str) -> Result<Self, TrustError> {
        self.endpoint_fingerprint = Some(EndpointFingerprint::parse(fingerprint)?);
        Ok(self)
    }

    pub fn server(&self) -> &McpServerId {
        &self.server
    }

    pub fn endpoint_fingerprint(&self) -> Option<&EndpointFingerprint> {
        self.endpoint_fingerprint.as_ref()
    }

    fn material_eq(&self, other: &Self) -> bool {
        self.endpoint_fingerprint == other.endpoint_fingerprint
    }
}

impl ProjectMcpServerSpec {
    pub fn new(server: &str) -> Result<Self, TrustError> {
        Ok(Self {
            identity: McpServerIdentity::new(server)?,
            credential: None,
            requested_tools: BTreeSet::new(),
            requested_capabilities: Vec::new(),
        })
    }

    pub fn with_endpoint_fingerprint(mut self, fingerprint: &str) -> Result<Self, TrustError> {
        self.identity = self.identity.with_endpoint_fingerprint(fingerprint)?;
        Ok(self)
    }

    /// Bind a handle from project config. Plaintext is rejected.
    pub fn with_credential(mut self, credential: ProjectCredential) -> Result<Self, TrustError> {
        match credential {
            ProjectCredential::Handle(handle) => {
                self.credential = Some(handle);
                Ok(self)
            }
            ProjectCredential::Plaintext => Err(TrustError::InvalidCredential),
        }
    }

    pub fn with_requested_tools<I, S>(mut self, tools: I) -> Result<Self, TrustError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.requested_tools = parse_tools(&self.identity.server, tools)?;
        Ok(self)
    }

    pub fn with_requested_capabilities<I, S>(mut self, caps: I) -> Result<Self, TrustError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.requested_capabilities = parse_capabilities(caps)?;
        Ok(self)
    }

    pub fn identity(&self) -> &McpServerIdentity {
        &self.identity
    }
}

impl TrustGrant {
    pub fn new(identity: McpServerIdentity, origin: ServerOrigin) -> Self {
        Self {
            identity,
            origin,
            credential: None,
            allowed_tools: BTreeSet::new(),
            allowed_capabilities: Vec::new(),
        }
    }

    pub fn with_credential(mut self, handle: SecretRef) -> Self {
        self.credential = Some(handle);
        self
    }

    pub fn with_allowed_tools<I, S>(mut self, tools: I) -> Result<Self, TrustError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.allowed_tools = parse_tools(&self.identity.server, tools)?;
        Ok(self)
    }

    pub fn with_allowed_capabilities<I, S>(mut self, caps: I) -> Result<Self, TrustError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.allowed_capabilities = parse_capabilities(caps)?;
        Ok(self)
    }

    pub fn identity(&self) -> &McpServerIdentity {
        &self.identity
    }
}

impl McpTrustRecord {
    fn untrusted(identity: McpServerIdentity, origin: ServerOrigin) -> Self {
        Self {
            identity,
            origin,
            status: McpTrustStatus::Untrusted,
            credential: None,
            allowed_tools: BTreeSet::new(),
            allowed_capabilities: Vec::new(),
        }
    }

    pub fn identity(&self) -> &McpServerIdentity {
        &self.identity
    }

    pub fn origin(&self) -> ServerOrigin {
        self.origin
    }

    pub fn status(&self) -> McpTrustStatus {
        self.status
    }

    pub fn has_credential(&self) -> bool {
        self.credential.is_some()
    }

    pub fn allowed_tools(&self) -> impl Iterator<Item = &str> {
        self.allowed_tools.iter().map(String::as_str)
    }

    pub fn allowed_capabilities(&self) -> &[CapabilityHint] {
        &self.allowed_capabilities
    }

    pub fn catalog_trust(&self) -> CatalogTrust {
        if self.status.is_trusted() {
            CatalogTrust::Trusted
        } else {
            CatalogTrust::Untrusted
        }
    }

    /// Catalog/model projection. Credential handle is omitted.
    pub fn catalog_view(&self) -> TrustCatalogView {
        TrustCatalogView {
            server: self.identity.server.clone(),
            origin: self.origin,
            status: self.status,
            has_credential: self.credential.is_some(),
            allowed_tools: self.allowed_tools.clone(),
            allowed_capabilities: self.allowed_capabilities.clone(),
        }
    }

    pub fn catalog_meta(&self, fetched_at: SystemTime) -> CatalogMeta {
        CatalogMeta::new(fetched_at, self.catalog_trust())
            .with_hints(self.allowed_capabilities.clone())
    }
}

impl TrustCatalogView {
    pub fn server(&self) -> &McpServerId {
        &self.server
    }

    pub fn origin(&self) -> ServerOrigin {
        self.origin
    }

    pub fn status(&self) -> McpTrustStatus {
        self.status
    }

    pub fn catalog_trust(&self) -> CatalogTrust {
        if self.status.is_trusted() {
            CatalogTrust::Trusted
        } else {
            CatalogTrust::Untrusted
        }
    }

    pub fn has_credential(&self) -> bool {
        self.has_credential
    }

    pub fn allowed_tools(&self) -> impl Iterator<Item = &str> {
        self.allowed_tools.iter().map(String::as_str)
    }

    pub fn allowed_capabilities(&self) -> &[CapabilityHint] {
        &self.allowed_capabilities
    }

    /// JSON object safe for catalog/model context. No credential fields.
    pub fn to_json_value(&self) -> Value {
        let mut obj = Map::new();
        obj.insert(
            "server".into(),
            Value::String(self.server.as_str().to_owned()),
        );
        obj.insert(
            "origin".into(),
            Value::String(self.origin.as_str().to_owned()),
        );
        obj.insert(
            "status".into(),
            Value::String(self.status.as_str().to_owned()),
        );
        obj.insert("has_credential".into(), Value::Bool(self.has_credential));
        obj.insert(
            "allowed_tools".into(),
            Value::Array(
                self.allowed_tools
                    .iter()
                    .map(|tool| Value::String(tool.clone()))
                    .collect(),
            ),
        );
        obj.insert(
            "allowed_capabilities".into(),
            Value::Array(
                self.allowed_capabilities
                    .iter()
                    .map(|hint| Value::String(hint.as_str().to_owned()))
                    .collect(),
            ),
        );
        Value::Object(obj)
    }
}

impl McpTrustStore {
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
    /// A record whose endpoint identity no longer matches is invalidated
    /// before the untrusted status is returned.
    pub fn get(
        &self,
        identity: &McpServerIdentity,
        cancel: &CancellationToken,
    ) -> Result<McpTrustRecord, TrustError> {
        cancel_check(cancel)?;
        let mut records = self.load(cancel)?;
        cancel_check(cancel)?;
        match records.get(&identity.server) {
            None => Ok(McpTrustRecord::untrusted(
                identity.clone(),
                ServerOrigin::User,
            )),
            Some(stored) if stored.identity.material_eq(identity) => Ok(stored.clone()),
            Some(stored) => {
                let origin = stored.origin;
                records.insert(
                    identity.server.clone(),
                    McpTrustRecord::untrusted(identity.clone(), origin),
                );
                self.persist(&records, cancel)?;
                Ok(McpTrustRecord::untrusted(identity.clone(), origin))
            }
        }
    }

    /// Record a project-configured server. Always starts disabled/untrusted.
    ///
    /// Project `enabled`/`trust` claims are not consulted. A stored grant for
    /// the same endpoint identity is kept; a material identity change revokes it.
    pub fn register_project_server(
        &self,
        spec: &ProjectMcpServerSpec,
        cancel: &CancellationToken,
    ) -> Result<McpTrustRecord, TrustError> {
        cancel_check(cancel)?;
        let mut records = self.load(cancel)?;
        cancel_check(cancel)?;
        let existing = records.get(&spec.identity.server);
        let record = match existing {
            Some(stored) if stored.identity.material_eq(&spec.identity) => {
                let mut kept = stored.clone();
                kept.origin = ServerOrigin::Project;
                kept.identity = spec.identity.clone();
                if kept.status != McpTrustStatus::Trusted {
                    kept.credential = spec.credential.clone();
                    kept.allowed_tools = spec.requested_tools.clone();
                    kept.allowed_capabilities = spec.requested_capabilities.clone();
                }
                kept
            }
            _ => McpTrustRecord {
                identity: spec.identity.clone(),
                origin: ServerOrigin::Project,
                status: McpTrustStatus::Untrusted,
                credential: spec.credential.clone(),
                allowed_tools: spec.requested_tools.clone(),
                allowed_capabilities: spec.requested_capabilities.clone(),
            },
        };
        self.upsert(&mut records, record, cancel)
    }

    /// Persist an explicit trust grant. This is the only path that enables a server.
    pub fn grant(
        &self,
        grant: &TrustGrant,
        cancel: &CancellationToken,
    ) -> Result<McpTrustRecord, TrustError> {
        cancel_check(cancel)?;
        let mut records = self.load(cancel)?;
        cancel_check(cancel)?;
        let origin = records
            .get(&grant.identity.server)
            .map(|stored| stored.origin)
            .unwrap_or(grant.origin);
        let record = McpTrustRecord {
            identity: grant.identity.clone(),
            origin,
            status: McpTrustStatus::Trusted,
            credential: grant.credential.clone(),
            allowed_tools: grant.allowed_tools.clone(),
            allowed_capabilities: grant.allowed_capabilities.clone(),
        };
        self.upsert(&mut records, record, cancel)
    }

    /// Mark a server untrusted. Recorded handles stay unused until a new grant.
    pub fn revoke(
        &self,
        identity: &McpServerIdentity,
        cancel: &CancellationToken,
    ) -> Result<McpTrustRecord, TrustError> {
        cancel_check(cancel)?;
        let mut records = self.load(cancel)?;
        cancel_check(cancel)?;
        let mut record = match records.get(&identity.server) {
            Some(stored) => stored.clone(),
            None => McpTrustRecord::untrusted(identity.clone(), ServerOrigin::User),
        };
        record.identity = identity.clone();
        record.status = McpTrustStatus::Untrusted;
        self.upsert(&mut records, record, cancel)
    }

    /// Auto-connect is allowed only after an explicit trust grant.
    pub fn authorize_connect(
        &self,
        identity: &McpServerIdentity,
        cancel: &CancellationToken,
    ) -> Result<(), TrustError> {
        let record = self.get(identity, cancel)?;
        if record.status.is_enabled() {
            Ok(())
        } else {
            Err(TrustError::Untrusted)
        }
    }

    /// Project/user credential handles are usable only on a trusted server
    /// and only when they match the handle bound to that server.
    pub fn authorize_credential(
        &self,
        identity: &McpServerIdentity,
        requested: &SecretRef,
        cancel: &CancellationToken,
    ) -> Result<SecretRef, TrustError> {
        let record = self.get(identity, cancel)?;
        if !record.status.is_trusted() {
            return Err(TrustError::CredentialDenied);
        }
        match record.credential {
            Some(handle) if handle == *requested => Ok(handle),
            _ => Err(TrustError::CredentialDenied),
        }
    }

    /// Tool scope is narrowing only. Untrusted servers have no usable scope.
    pub fn authorize_tool(
        &self,
        identity: &McpServerIdentity,
        tool: &str,
        cancel: &CancellationToken,
    ) -> Result<(), TrustError> {
        let record = self.get(identity, cancel)?;
        if !record.status.is_trusted() {
            return Err(TrustError::Untrusted);
        }
        let tool = parse_tool(&record.identity.server, tool)?;
        if record.allowed_tools.contains(&tool) {
            Ok(())
        } else {
            Err(TrustError::ToolNotAllowed)
        }
    }

    /// Capability hints recorded on the grant. Never a broker lease.
    pub fn authorize_capability(
        &self,
        identity: &McpServerIdentity,
        capability: &str,
        cancel: &CancellationToken,
    ) -> Result<(), TrustError> {
        let record = self.get(identity, cancel)?;
        if !record.status.is_trusted() {
            return Err(TrustError::Untrusted);
        }
        let hint = parse_capability(capability)?;
        if record
            .allowed_capabilities
            .iter()
            .any(|allowed| allowed == &hint)
        {
            Ok(())
        } else {
            Err(TrustError::CapabilityNotAllowed)
        }
    }

    fn upsert(
        &self,
        records: &mut BTreeMap<McpServerId, McpTrustRecord>,
        record: McpTrustRecord,
        cancel: &CancellationToken,
    ) -> Result<McpTrustRecord, TrustError> {
        let exists = records.contains_key(&record.identity.server);
        if !exists && records.len() >= self.max_records {
            return Err(TrustError::TooManyRecords);
        }
        records.insert(record.identity.server.clone(), record.clone());
        self.persist(records, cancel)?;
        Ok(record)
    }

    fn load(
        &self,
        cancel: &CancellationToken,
    ) -> Result<BTreeMap<McpServerId, McpTrustRecord>, TrustError> {
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
        records: &BTreeMap<McpServerId, McpTrustRecord>,
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
            && !parent.as_os_str().is_empty()
        {
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
}

impl Debug for McpTrustStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpTrustStore")
            .field("catalog", &self.catalog)
            .finish()
    }
}

impl Debug for McpTrustRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpTrustRecord")
            .field("identity", &self.identity)
            .field("origin", &self.origin)
            .field("status", &self.status)
            .field("has_credential", &self.credential.is_some())
            .field("allowed_tools", &self.allowed_tools)
            .field("allowed_capabilities", &self.allowed_capabilities)
            .finish()
    }
}

impl TrustError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::Untrusted
            | Self::CredentialDenied
            | Self::ToolNotAllowed
            | Self::CapabilityNotAllowed => Some(ErrorCode::McpServerUntrusted),
            Self::InvalidServer
            | Self::InvalidTool
            | Self::InvalidCapability
            | Self::InvalidFingerprint
            | Self::InvalidCredential
            | Self::TooManyScopes
            | Self::TooManyRecords => Some(ErrorCode::ToolInvalidArguments),
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
            Self::InvalidServer => "MCP server identity is invalid",
            Self::InvalidTool => "MCP tool scope is invalid",
            Self::InvalidCapability => "MCP capability scope is invalid",
            Self::InvalidFingerprint => "MCP endpoint fingerprint is invalid",
            Self::InvalidCredential => "MCP credential must be an opaque handle",
            Self::Untrusted => "MCP server is not trusted",
            Self::CredentialDenied => "MCP credential handle is not authorized",
            Self::ToolNotAllowed => "MCP tool is outside the trusted scope",
            Self::CapabilityNotAllowed => "MCP capability is outside the trusted scope",
            Self::CatalogTooLarge { .. } => "MCP trust catalog exceeds size bound",
            Self::TooManyRecords => "MCP trust catalog exceeds record bound",
            Self::TooManyScopes => "MCP trust scope count exceeds the configured bound",
            Self::CatalogCorrupt => "MCP trust catalog is corrupt",
            Self::UnsupportedSchema { found } => {
                return Some(
                    ApiError::new(
                        code,
                        format!("unsupported MCP trust catalog schema {found}"),
                        trace_id,
                    )
                    .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
                );
            }
            Self::Io(_) => "MCP trust catalog I/O failed",
        };
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }
}

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("MCP trust operation cancelled"),
            Self::InvalidServer => f.write_str("MCP server identity is invalid"),
            Self::InvalidTool => f.write_str("MCP tool scope is invalid"),
            Self::InvalidCapability => f.write_str("MCP capability scope is invalid"),
            Self::InvalidFingerprint => f.write_str("MCP endpoint fingerprint is invalid"),
            Self::InvalidCredential => f.write_str("MCP credential must be an opaque handle"),
            Self::Untrusted => f.write_str("MCP server is not trusted"),
            Self::CredentialDenied => f.write_str("MCP credential handle is not authorized"),
            Self::ToolNotAllowed => f.write_str("MCP tool is outside the trusted scope"),
            Self::CapabilityNotAllowed => {
                f.write_str("MCP capability is outside the trusted scope")
            }
            Self::CatalogTooLarge { .. } => f.write_str("MCP trust catalog exceeds size bound"),
            Self::TooManyRecords => f.write_str("MCP trust catalog exceeds record bound"),
            Self::TooManyScopes => {
                f.write_str("MCP trust scope count exceeds the configured bound")
            }
            Self::CatalogCorrupt => f.write_str("MCP trust catalog is corrupt"),
            Self::UnsupportedSchema { found } => {
                write!(f, "unsupported MCP trust catalog schema {found}")
            }
            Self::Io(_) => f.write_str("MCP trust catalog I/O failed"),
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
            | (Self::InvalidServer, Self::InvalidServer)
            | (Self::InvalidTool, Self::InvalidTool)
            | (Self::InvalidCapability, Self::InvalidCapability)
            | (Self::InvalidFingerprint, Self::InvalidFingerprint)
            | (Self::InvalidCredential, Self::InvalidCredential)
            | (Self::Untrusted, Self::Untrusted)
            | (Self::CredentialDenied, Self::CredentialDenied)
            | (Self::ToolNotAllowed, Self::ToolNotAllowed)
            | (Self::CapabilityNotAllowed, Self::CapabilityNotAllowed)
            | (Self::TooManyRecords, Self::TooManyRecords)
            | (Self::TooManyScopes, Self::TooManyScopes)
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

fn parse_server(value: &str) -> Result<McpServerId, TrustError> {
    McpServerId::parse(value).map_err(|_| TrustError::InvalidServer)
}

fn parse_tool(server: &McpServerId, tool: &str) -> Result<String, TrustError> {
    ExternalToolId::new(server.clone(), tool)
        .map(|id| id.tool().to_owned())
        .map_err(|_| TrustError::InvalidTool)
}

fn parse_capability(value: &str) -> Result<CapabilityHint, TrustError> {
    CapabilityHint::parse(value).map_err(|_| TrustError::InvalidCapability)
}

fn parse_tools<I, S>(server: &McpServerId, tools: I) -> Result<BTreeSet<String>, TrustError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = BTreeSet::new();
    for tool in tools {
        let tool = parse_tool(server, tool.as_ref())?;
        if !out.contains(&tool) && out.len() >= MAX_ALLOWED_TOOLS {
            return Err(TrustError::TooManyScopes);
        }
        out.insert(tool);
    }
    Ok(out)
}

fn parse_capabilities<I, S>(caps: I) -> Result<Vec<CapabilityHint>, TrustError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for cap in caps {
        if out.len() >= MAX_ALLOWED_CAPABILITIES {
            return Err(TrustError::TooManyScopes);
        }
        let hint = parse_capability(cap.as_ref())?;
        if seen.insert(hint.as_str().to_owned()) {
            out.push(hint);
        }
    }
    out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(out)
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

fn cancel_check(cancel: &CancellationToken) -> Result<(), TrustError> {
    if cancel.is_cancelled() {
        Err(TrustError::Cancelled)
    } else {
        Ok(())
    }
}

fn encode_catalog(records: &BTreeMap<McpServerId, McpTrustRecord>) -> Result<Vec<u8>, TrustError> {
    let mut recs = Vec::with_capacity(records.len());
    for stored in records.values() {
        let mut rec = Map::new();
        rec.insert(
            "server".into(),
            Value::String(stored.identity.server.as_str().to_owned()),
        );
        rec.insert(
            "origin".into(),
            Value::String(stored.origin.as_str().to_owned()),
        );
        rec.insert(
            "status".into(),
            Value::String(stored.status.as_str().to_owned()),
        );
        if let Some(fp) = &stored.identity.endpoint_fingerprint {
            rec.insert(
                "endpoint_fingerprint".into(),
                Value::String(fp.as_str().to_owned()),
            );
        }
        if let Some(handle) = &stored.credential {
            rec.insert("credential".into(), encode_secret_ref(handle));
        }
        rec.insert(
            "allowed_tools".into(),
            Value::Array(
                stored
                    .allowed_tools
                    .iter()
                    .map(|tool| Value::String(tool.clone()))
                    .collect(),
            ),
        );
        rec.insert(
            "allowed_capabilities".into(),
            Value::Array(
                stored
                    .allowed_capabilities
                    .iter()
                    .map(|hint| Value::String(hint.as_str().to_owned()))
                    .collect(),
            ),
        );
        recs.push(Value::Object(rec));
    }
    let mut root = Map::new();
    root.insert("schema".into(), Value::String(TRUST_SCHEMA.to_owned()));
    root.insert("schema_version".into(), Value::from(TRUST_SCHEMA_VERSION));
    root.insert("records".into(), Value::Array(recs));
    serde_json::to_vec(&Value::Object(root)).map_err(|_| TrustError::CatalogCorrupt)
}

fn encode_secret_ref(handle: &SecretRef) -> Value {
    let mut obj = Map::new();
    if let Some(id) = handle.id() {
        obj.insert("id".into(), Value::String(id.to_owned()));
    }
    if let Some(alias) = handle.alias() {
        obj.insert("alias".into(), Value::String(alias.to_owned()));
    }
    Value::Object(obj)
}

fn decode_catalog(
    bytes: &[u8],
    max_records: usize,
    cancel: &CancellationToken,
) -> Result<BTreeMap<McpServerId, McpTrustRecord>, TrustError> {
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
        let server = rec
            .get("server")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let mut identity = McpServerIdentity::new(server)?;
        if let Some(fp) = optional_str(rec, "endpoint_fingerprint")? {
            identity = identity.with_endpoint_fingerprint(fp)?;
        }
        let origin = rec
            .get("origin")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let origin = ServerOrigin::parse(origin)?;
        let status = rec
            .get("status")
            .and_then(Value::as_str)
            .ok_or(TrustError::CatalogCorrupt)?;
        let status = McpTrustStatus::parse(status)?;
        let credential = match rec.get("credential") {
            None => None,
            Some(value) => Some(decode_secret_ref(value)?),
        };
        let allowed_tools = decode_string_set(rec.get("allowed_tools"), &identity.server)?;
        let allowed_capabilities = decode_capabilities(rec.get("allowed_capabilities"))?;
        if records
            .insert(
                identity.server.clone(),
                McpTrustRecord {
                    identity,
                    origin,
                    status,
                    credential,
                    allowed_tools,
                    allowed_capabilities,
                },
            )
            .is_some()
        {
            return Err(TrustError::CatalogCorrupt);
        }
    }
    Ok(records)
}

fn decode_secret_ref(value: &Value) -> Result<SecretRef, TrustError> {
    let obj = value.as_object().ok_or(TrustError::InvalidCredential)?;
    reject_unknown_keys(obj, CREDENTIAL_KEYS)?;
    if obj.is_empty() {
        return Err(TrustError::InvalidCredential);
    }
    let id = optional_str(obj, "id")?;
    let alias = optional_str(obj, "alias")?;
    match (id, alias) {
        (Some(id), Some(alias)) => {
            SecretRef::from_id_and_alias(id, alias).map_err(|_| TrustError::InvalidCredential)
        }
        (Some(id), None) => SecretRef::from_id(id).map_err(|_| TrustError::InvalidCredential),
        (None, Some(alias)) => {
            SecretRef::from_alias(alias).map_err(|_| TrustError::InvalidCredential)
        }
        (None, None) => Err(TrustError::InvalidCredential),
    }
}

fn decode_string_set(
    value: Option<&Value>,
    server: &McpServerId,
) -> Result<BTreeSet<String>, TrustError> {
    let Some(value) = value else {
        return Ok(BTreeSet::new());
    };
    let items = value.as_array().ok_or(TrustError::CatalogCorrupt)?;
    if items.len() > MAX_ALLOWED_TOOLS {
        return Err(TrustError::TooManyScopes);
    }
    let mut out = BTreeSet::new();
    for item in items {
        let tool = item.as_str().ok_or(TrustError::CatalogCorrupt)?;
        let tool = parse_tool(server, tool)?;
        if !out.insert(tool) {
            return Err(TrustError::CatalogCorrupt);
        }
    }
    Ok(out)
}

fn decode_capabilities(value: Option<&Value>) -> Result<Vec<CapabilityHint>, TrustError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value.as_array().ok_or(TrustError::CatalogCorrupt)?;
    if items.len() > MAX_ALLOWED_CAPABILITIES {
        return Err(TrustError::TooManyScopes);
    }
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        let name = item.as_str().ok_or(TrustError::CatalogCorrupt)?;
        names.push(name);
    }
    parse_capabilities(names)
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

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    const SECRET: &str = "super-secret-password";
    const FP_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const FP_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const HANDLE_ALIAS: &str = "env:MCP_TOKEN";
    const GOLDEN: &str = concat!(
        r#"{"records":[{"allowed_capabilities":["mcp.invoke"],"allowed_tools":["search"],"#,
        r#""credential":{"alias":"env:MCP_TOKEN"},"endpoint_fingerprint":""#,
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        r#"","origin":"project","server":"github","status":"trusted"}],"#,
        r#""schema":"rapidlm.mcp_trust","schema_version":1}"#
    );

    struct TempCatalog {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TempCatalog {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("rapidlm-mcp-trust-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp trust dir");
            let path = dir.join("mcp-trust.json");
            Self { dir, path }
        }

        fn store(&self) -> McpTrustStore {
            McpTrustStore::open(&self.path)
        }

        fn bounded(&self, max_records: usize, max_bytes: u64) -> McpTrustStore {
            McpTrustStore {
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

    fn identity(server: &str, fingerprint: Option<&str>) -> McpServerIdentity {
        let mut id = McpServerIdentity::new(server).unwrap_or_else(|err| {
            panic!("identity {server:?}: {err}");
        });
        if let Some(fp) = fingerprint {
            id = id.with_endpoint_fingerprint(fp).expect("fingerprint");
        }
        id
    }

    fn handle() -> SecretRef {
        SecretRef::from_alias(HANDLE_ALIAS).expect("handle")
    }

    fn other_handle() -> SecretRef {
        SecretRef::from_alias("env:OTHER_TOKEN").expect("handle")
    }

    fn project_spec(server: &str) -> ProjectMcpServerSpec {
        ProjectMcpServerSpec::new(server)
            .expect("spec")
            .with_endpoint_fingerprint(FP_A)
            .expect("fp")
            .with_credential(ProjectCredential::Handle(handle()))
            .expect("cred")
            .with_requested_tools(["search"])
            .expect("tools")
            .with_requested_capabilities(["mcp.invoke"])
            .expect("caps")
    }

    fn grant_github() -> TrustGrant {
        TrustGrant::new(identity("github", Some(FP_A)), ServerOrigin::Project)
            .with_credential(handle())
            .with_allowed_tools(["search"])
            .expect("tools")
            .with_allowed_capabilities(["mcp.invoke"])
            .expect("caps")
    }

    #[test]
    fn missing_catalog_is_untrusted_and_disabled() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let id = identity("github", Some(FP_A));
        let record = store.get(&id, &live()).expect("get");
        assert_eq!(record.status(), McpTrustStatus::Untrusted);
        assert!(!record.status().is_enabled());
        assert_eq!(record.catalog_trust(), CatalogTrust::Untrusted);
        assert!(!tmp.path.exists());
    }

    #[test]
    fn project_configured_server_starts_disabled() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let record = store
            .register_project_server(&project_spec("github"), &live())
            .expect("register");
        assert_eq!(record.origin(), ServerOrigin::Project);
        assert_eq!(record.status(), McpTrustStatus::Untrusted);
        assert!(!record.status().is_enabled());
        assert_eq!(
            store.authorize_connect(&identity("github", Some(FP_A)), &live()),
            Err(TrustError::Untrusted)
        );
    }

    #[test]
    fn project_enabled_or_trust_claim_is_not_a_grant() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let claimed_enabled = true;
        let claimed_trusted = true;
        assert!(claimed_enabled && claimed_trusted);
        let spec = project_spec("github");
        let record = store
            .register_project_server(&spec, &live())
            .expect("register");
        assert_eq!(record.status(), McpTrustStatus::Untrusted);
        assert_eq!(
            store.authorize_connect(&spec.identity, &live()),
            Err(TrustError::Untrusted)
        );
    }

    #[test]
    fn untrusted_server_cannot_use_project_credential_handle() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let spec = project_spec("github");
        store
            .register_project_server(&spec, &live())
            .expect("register");
        assert_eq!(
            store.authorize_credential(&spec.identity, &handle(), &live()),
            Err(TrustError::CredentialDenied)
        );
        assert!(
            store
                .get(&spec.identity, &live())
                .expect("get")
                .has_credential()
        );
    }

    #[test]
    fn plaintext_project_credential_is_rejected() {
        let err = ProjectMcpServerSpec::new("github")
            .expect("spec")
            .with_credential(ProjectCredential::Plaintext)
            .expect_err("plaintext");
        assert_eq!(err, TrustError::InvalidCredential);
    }

    #[test]
    fn grant_enables_connect_and_bound_credential() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store
            .register_project_server(&project_spec("github"), &live())
            .expect("register");
        let granted = store.grant(&grant_github(), &live()).expect("grant");
        assert_eq!(granted.status(), McpTrustStatus::Trusted);
        assert_eq!(granted.origin(), ServerOrigin::Project);
        store
            .authorize_connect(&identity("github", Some(FP_A)), &live())
            .expect("connect");
        assert_eq!(
            store
                .authorize_credential(&identity("github", Some(FP_A)), &handle(), &live())
                .expect("cred"),
            handle()
        );
        store
            .authorize_tool(&identity("github", Some(FP_A)), "search", &live())
            .expect("tool");
        store
            .authorize_capability(&identity("github", Some(FP_A)), "mcp.invoke", &live())
            .expect("cap");
    }

    #[test]
    fn trusted_server_cannot_use_another_servers_handle() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store.grant(&grant_github(), &live()).expect("grant");
        assert_eq!(
            store.authorize_credential(&identity("github", Some(FP_A)), &other_handle(), &live()),
            Err(TrustError::CredentialDenied)
        );
    }

    #[test]
    fn tool_outside_granted_scope_is_denied() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store.grant(&grant_github(), &live()).expect("grant");
        assert_eq!(
            store.authorize_tool(&identity("github", Some(FP_A)), "delete_repo", &live()),
            Err(TrustError::ToolNotAllowed)
        );
        assert_eq!(
            store.authorize_capability(&identity("github", Some(FP_A)), "mcp.admin", &live()),
            Err(TrustError::CapabilityNotAllowed)
        );
    }

    #[test]
    fn empty_grant_scope_denies_tools() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store
            .grant(
                &TrustGrant::new(identity("github", Some(FP_A)), ServerOrigin::User),
                &live(),
            )
            .expect("grant");
        store
            .authorize_connect(&identity("github", Some(FP_A)), &live())
            .expect("connect");
        assert_eq!(
            store.authorize_tool(&identity("github", Some(FP_A)), "search", &live()),
            Err(TrustError::ToolNotAllowed)
        );
    }

    #[test]
    fn replay_after_reopen_preserves_trusted() {
        let tmp = TempCatalog::create();
        let id = identity("github", Some(FP_A));
        tmp.store().grant(&grant_github(), &live()).expect("grant");
        let reopened = tmp.store();
        assert_eq!(
            reopened.get(&id, &live()).expect("replay").status(),
            McpTrustStatus::Trusted
        );
        reopened.authorize_connect(&id, &live()).expect("connect");
    }

    #[test]
    fn revoke_disables_connect_and_credential() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        let id = identity("github", Some(FP_A));
        store.grant(&grant_github(), &live()).expect("grant");
        store.revoke(&id, &live()).expect("revoke");
        assert_eq!(
            store.authorize_connect(&id, &live()),
            Err(TrustError::Untrusted)
        );
        assert_eq!(
            store.authorize_credential(&id, &handle(), &live()),
            Err(TrustError::CredentialDenied)
        );
    }

    #[test]
    fn fingerprint_change_does_not_inherit_trust_or_credential() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store.grant(&grant_github(), &live()).expect("grant");
        let spoofed = identity("github", Some(FP_B));
        let record = store.get(&spoofed, &live()).expect("spoof");
        assert_eq!(record.status(), McpTrustStatus::Untrusted);
        assert!(!record.has_credential());
        assert_eq!(
            store.authorize_connect(&spoofed, &live()),
            Err(TrustError::Untrusted)
        );
        assert_eq!(
            store.authorize_credential(&spoofed, &handle(), &live()),
            Err(TrustError::CredentialDenied)
        );
    }

    #[test]
    fn sibling_server_does_not_inherit_trust() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store.grant(&grant_github(), &live()).expect("grant");
        assert_eq!(
            store
                .get(&identity("github-evil", Some(FP_A)), &live())
                .expect("sibling")
                .status(),
            McpTrustStatus::Untrusted
        );
    }

    #[test]
    fn register_does_not_wipe_matching_grant() {
        let tmp = TempCatalog::create();
        let store = tmp.store();
        store
            .register_project_server(&project_spec("github"), &live())
            .expect("register");
        store.grant(&grant_github(), &live()).expect("grant");
        let again = store
            .register_project_server(&project_spec("github"), &live())
            .expect("reregister");
        assert_eq!(again.status(), McpTrustStatus::Trusted);
    }

    #[test]
    fn catalog_and_model_view_omit_credential_and_plaintext() {
        let tmp = TempCatalog::create();
        let record = tmp.store().grant(&grant_github(), &live()).expect("grant");
        let view = record.catalog_view();
        let json = view.to_json_value();
        let text = json.to_string();
        assert!(json.get("credential").is_none());
        assert!(json.get("token").is_none());
        assert!(!text.contains(SECRET));
        assert!(!text.contains(HANDLE_ALIAS));
        assert!(!text.contains("env:"));
        assert_eq!(view.catalog_trust(), CatalogTrust::Trusted);
        assert!(view.has_credential());
        let debug = format!("{record:?}");
        assert!(!debug.contains(SECRET));
        assert!(!debug.contains(HANDLE_ALIAS));
        assert!(debug.contains("has_credential: true"));
    }

    #[test]
    fn catalog_serialization_matches_golden() {
        let tmp = TempCatalog::create();
        tmp.store().grant(&grant_github(), &live()).expect("grant");
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
                .get(&identity("github", Some(FP_A)), &live())
                .expect("part ignored")
                .status(),
            McpTrustStatus::Untrusted
        );
    }

    #[test]
    fn corrupt_catalog_fails_closed() {
        let tmp = TempCatalog::create();
        fs::write(&tmp.path, "{not-json").expect("corrupt");
        let err = tmp
            .store()
            .get(&identity("github", Some(FP_A)), &live())
            .expect_err("corrupt");
        assert_eq!(err, TrustError::CatalogCorrupt);
    }

    #[test]
    fn unknown_status_does_not_grant_trust() {
        let tmp = TempCatalog::create();
        fs::write(
            &tmp.path,
            r#"{"schema":"rapidlm.mcp_trust","schema_version":1,"records":[{"server":"github","origin":"project","status":"allow","allowed_tools":[],"allowed_capabilities":[]}]}"#,
        )
        .expect("write");
        let err = tmp
            .store()
            .get(&identity("github", None), &live())
            .expect_err("allow");
        assert_eq!(err, TrustError::CatalogCorrupt);
    }

    #[test]
    fn plaintext_credential_field_fails_closed() {
        let tmp = TempCatalog::create();
        fs::write(
            &tmp.path,
            r#"{"schema":"rapidlm.mcp_trust","schema_version":1,"records":[{"server":"github","origin":"project","status":"trusted","credential":"super-secret-password","allowed_tools":[],"allowed_capabilities":[]}]}"#,
        )
        .expect("write");
        let err = tmp
            .store()
            .get(&identity("github", None), &live())
            .expect_err("plaintext");
        assert_eq!(err, TrustError::InvalidCredential);
        assert_eq!(err.code(), Some(ErrorCode::ToolInvalidArguments));
    }

    #[test]
    fn extra_token_key_fails_closed() {
        let tmp = TempCatalog::create();
        fs::write(
            &tmp.path,
            r#"{"schema":"rapidlm.mcp_trust","schema_version":1,"records":[{"server":"github","origin":"project","status":"trusted","token":"super-secret-password","allowed_tools":[],"allowed_capabilities":[]}]}"#,
        )
        .expect("write");
        let err = tmp
            .store()
            .get(&identity("github", None), &live())
            .expect_err("token");
        assert_eq!(err, TrustError::CatalogCorrupt);
    }

    #[test]
    fn credential_object_rejects_value_key() {
        let tmp = TempCatalog::create();
        fs::write(
            &tmp.path,
            r#"{"schema":"rapidlm.mcp_trust","schema_version":1,"records":[{"server":"github","origin":"project","status":"trusted","credential":{"alias":"env:MCP_TOKEN","value":"super-secret-password"},"allowed_tools":[],"allowed_capabilities":[]}]}"#,
        )
        .expect("write");
        let err = tmp
            .store()
            .get(&identity("github", None), &live())
            .expect_err("value");
        assert_eq!(err, TrustError::CatalogCorrupt);
    }

    #[test]
    fn unsupported_schema_is_rejected() {
        let tmp = TempCatalog::create();
        fs::write(
            &tmp.path,
            r#"{"schema":"rapidlm.mcp_trust","schema_version":2,"records":[]}"#,
        )
        .expect("write");
        let err = tmp
            .store()
            .get(&identity("github", None), &live())
            .expect_err("schema");
        assert_eq!(err, TrustError::UnsupportedSchema { found: 2 });
    }

    #[test]
    fn oversized_catalog_is_rejected() {
        let tmp = TempCatalog::create();
        fs::write(&tmp.path, vec![b'x'; 128]).expect("write");
        let err = tmp
            .bounded(8, 64)
            .get(&identity("github", None), &live())
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
    fn record_bound_rejects_new_servers() {
        let tmp = TempCatalog::create();
        let store = tmp.bounded(1, MAX_TRUST_CATALOG_BYTES);
        store
            .grant(
                &TrustGrant::new(identity("alpha", None), ServerOrigin::User),
                &live(),
            )
            .expect("first");
        let err = store
            .grant(
                &TrustGrant::new(identity("beta", None), ServerOrigin::User),
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
                .get(&identity("github", None), &cancel)
                .expect_err("cancel"),
            TrustError::Cancelled
        );
        assert!(
            store
                .get(&identity("github", None), &cancel)
                .expect_err("cancel")
                .code()
                .is_none()
        );
    }

    #[test]
    fn invalid_server_and_tool_names_are_rejected() {
        assert_eq!(
            McpServerIdentity::new("../etc").unwrap_err(),
            TrustError::InvalidServer
        );
        assert_eq!(
            TrustGrant::new(identity("github", None), ServerOrigin::User)
                .with_allowed_tools(["../secret"])
                .unwrap_err(),
            TrustError::InvalidTool
        );
    }

    #[test]
    fn display_does_not_echo_secret_or_server_payload() {
        let err = TrustError::CredentialDenied;
        let text = err.to_string();
        assert!(!text.contains(SECRET));
        assert!(!text.contains(HANDLE_ALIAS));
        assert!(!text.contains("github"));
    }
}
