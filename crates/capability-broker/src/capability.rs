//! Closed v1 capability, resource, and action taxonomy.
//!
//! Unknown families, actions, schema versions, and fields fail closed.
//! Secret scopes carry opaque handles only; there is no plaintext field.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use protocol::{RepoPath, RepoPathError};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Wire schema name for [`Capability`].
pub const CAPABILITY_SCHEMA: &str = "rapidlm.capability";

/// Wire schema name for [`ResourceDescriptor`].
pub const RESOURCE_SCHEMA: &str = "rapidlm.resource_descriptor";

/// v1 schema version for capability and resource objects.
pub const CAPABILITY_SCHEMA_VERSION: u16 = 1;

/// Maximum UTF-8 bytes for a filesystem glob.
pub const MAX_PATH_GLOB_BYTES: usize = 4096;

/// Maximum UTF-8 bytes for command-family, device, MCP, plugin, and secret ids.
pub const MAX_IDENT_BYTES: usize = 256;

/// Maximum UTF-8 bytes for a DNS hostname.
pub const MAX_HOST_BYTES: usize = 253;

/// Maximum UTF-8 bytes for a serialized origin.
pub const MAX_ORIGIN_BYTES: usize = 512;

const CAPABILITY_FIELDS: &[&str] = &["schema", "schema_version", "family", "action"];

/// v1 capability families. There is no catch-all / wildcard family.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum CapabilityFamily {
    Fs,
    Proc,
    Net,
    Git,
    Secret,
    Browser,
    Mobile,
    Mcp,
    Plugin,
}

/// Closed versioned capability. Variants are the v1 action set.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum Capability {
    FsRead,
    FsWrite,
    ProcExec,
    NetConnect,
    GitWrite,
    SecretUse,
    BrowserNavigate,
    BrowserDownload,
    MobileControl,
    McpInvoke,
    PluginInvoke,
}

/// Typed resource scopes. Kind must match the capability family.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum ResourceDescriptor {
    Filesystem(FilesystemScope),
    Process(ProcessScope),
    Network(NetworkScope),
    Git(GitScope),
    Secret(SecretScope),
    Browser(BrowserScope),
    Mobile(MobileScope),
    Mcp(McpScope),
    Plugin(PluginScope),
}

/// Filesystem root class. Host writes are a distinct scope from repo writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FilesystemRoot {
    Repo,
    Host,
}

/// Normalized filesystem glob. Traversal and repo-absolute forms are rejected.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct PathGlob(String);

/// Repo or host path glob for `fs.read` / `fs.write`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct FilesystemScope {
    root: FilesystemRoot,
    glob: PathGlob,
}

/// Command-family token for `proc.exec` (not a raw argv or shell string).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CommandFamily(String);

/// Process execution scope.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ProcessScope {
    command_family: CommandFamily,
}

/// Closed network schemes. Unknown schemes fail closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum NetworkScheme {
    Http,
    Https,
    Ws,
    Wss,
}

/// Lowercased concrete DNS/IP hostname. Wildcards, globs, and userinfo fail closed.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Hostname(String);

/// Scheme/host/port for `net.connect`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct NetworkScope {
    scheme: NetworkScheme,
    host: Hostname,
    port: u16,
}

/// Git ref pattern for `git.write`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct GitRefScope(String);

/// Dedicated git write scope. `.git` writes are not generic `fs.write`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct GitScope {
    ref_scope: GitRefScope,
}

/// Opaque secret handle. Never carries plaintext.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SecretHandle(String);

/// Injection destination for `secret.use` (not a secret value).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SecretUseTarget(String);

/// Secret use scope. There is no value/payload field.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SecretScope {
    secret_id: SecretHandle,
    target: SecretUseTarget,
}

/// Origin (scheme/host/port) for browser scopes.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Origin {
    scheme: NetworkScheme,
    host: Hostname,
    port: u16,
}

/// Browser origin, with a download path when the action is `browser.download`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct BrowserScope {
    origin: Origin,
    path: Option<PathGlob>,
}

/// Mobile simulator/device identifier.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct DeviceId(String);

/// Mobile control scope.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct MobileScope {
    device_id: DeviceId,
}

/// MCP server/tool invocation scope.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct McpScope {
    server: String,
    tool: String,
}

/// Plugin invocation scope. `capability` is plugin-local, not a RapidLM grant.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct PluginScope {
    plugin: String,
    capability: String,
}

/// Construction / decode failure. Unknown input is never mapped to a grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityError {
    UnknownFamily,
    UnknownAction,
    UnknownScheme,
    UnknownRoot,
    UnknownKind,
    UnsupportedSchema,
    UnknownField,
    MissingField,
    FamilyMismatch,
    MissingScope,
    Empty,
    TooLong,
    Nul,
    Control,
    Traversal,
    AbsolutePath,
    Unc,
    InvalidIdent,
    InvalidHost,
    InvalidOrigin,
    InvalidPort,
    InvalidJson,
}

impl CapabilityFamily {
    /// Every v1 family. Tests treat this as the serde fixture checklist.
    pub const ALL: &'static [Self] = &[
        Self::Fs,
        Self::Proc,
        Self::Net,
        Self::Git,
        Self::Secret,
        Self::Browser,
        Self::Mobile,
        Self::Mcp,
        Self::Plugin,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fs => "fs",
            Self::Proc => "proc",
            Self::Net => "net",
            Self::Git => "git",
            Self::Secret => "secret",
            Self::Browser => "browser",
            Self::Mobile => "mobile",
            Self::Mcp => "mcp",
            Self::Plugin => "plugin",
        }
    }
}

impl FromStr for CapabilityFamily {
    type Err = CapabilityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for family in Self::ALL {
            if family.as_str() == s {
                return Ok(*family);
            }
        }
        Err(CapabilityError::UnknownFamily)
    }
}

impl Capability {
    /// Every v1 action. There is no `*` / `all` capability.
    pub const ALL: &'static [Self] = &[
        Self::FsRead,
        Self::FsWrite,
        Self::ProcExec,
        Self::NetConnect,
        Self::GitWrite,
        Self::SecretUse,
        Self::BrowserNavigate,
        Self::BrowserDownload,
        Self::MobileControl,
        Self::McpInvoke,
        Self::PluginInvoke,
    ];

    pub const fn family(self) -> CapabilityFamily {
        match self {
            Self::FsRead | Self::FsWrite => CapabilityFamily::Fs,
            Self::ProcExec => CapabilityFamily::Proc,
            Self::NetConnect => CapabilityFamily::Net,
            Self::GitWrite => CapabilityFamily::Git,
            Self::SecretUse => CapabilityFamily::Secret,
            Self::BrowserNavigate | Self::BrowserDownload => CapabilityFamily::Browser,
            Self::MobileControl => CapabilityFamily::Mobile,
            Self::McpInvoke => CapabilityFamily::Mcp,
            Self::PluginInvoke => CapabilityFamily::Plugin,
        }
    }

    /// Action name without family prefix (`read`, `exec`, `connect`, …).
    pub const fn action_name(self) -> &'static str {
        match self {
            Self::FsRead => "read",
            Self::FsWrite => "write",
            Self::ProcExec => "exec",
            Self::NetConnect => "connect",
            Self::GitWrite => "write",
            Self::SecretUse => "use",
            Self::BrowserNavigate => "navigate",
            Self::BrowserDownload => "download",
            Self::MobileControl => "control",
            Self::McpInvoke => "invoke",
            Self::PluginInvoke => "invoke",
        }
    }

    /// Canonical `family.action` form used by SDD examples (`fs.read`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::ProcExec => "proc.exec",
            Self::NetConnect => "net.connect",
            Self::GitWrite => "git.write",
            Self::SecretUse => "secret.use",
            Self::BrowserNavigate => "browser.navigate",
            Self::BrowserDownload => "browser.download",
            Self::MobileControl => "mobile.control",
            Self::McpInvoke => "mcp.invoke",
            Self::PluginInvoke => "plugin.invoke",
        }
    }

    /// Fail closed unless the resource family (and required scopes) match.
    pub fn compatible_with(self, resource: &ResourceDescriptor) -> Result<(), CapabilityError> {
        if self.family() != resource.family() {
            return Err(CapabilityError::FamilyMismatch);
        }
        if self == Self::BrowserDownload && resource.browser_path().is_none() {
            return Err(CapabilityError::MissingScope);
        }
        Ok(())
    }

    fn from_family_action(family: CapabilityFamily, action: &str) -> Result<Self, CapabilityError> {
        let capability = match (family, action) {
            (CapabilityFamily::Fs, "read") => Self::FsRead,
            (CapabilityFamily::Fs, "write") => Self::FsWrite,
            (CapabilityFamily::Proc, "exec") => Self::ProcExec,
            (CapabilityFamily::Net, "connect") => Self::NetConnect,
            (CapabilityFamily::Git, "write") => Self::GitWrite,
            (CapabilityFamily::Secret, "use") => Self::SecretUse,
            (CapabilityFamily::Browser, "navigate") => Self::BrowserNavigate,
            (CapabilityFamily::Browser, "download") => Self::BrowserDownload,
            (CapabilityFamily::Mobile, "control") => Self::MobileControl,
            (CapabilityFamily::Mcp, "invoke") => Self::McpInvoke,
            (CapabilityFamily::Plugin, "invoke") => Self::PluginInvoke,
            _ => return Err(CapabilityError::UnknownAction),
        };
        Ok(capability)
    }
}

impl FromStr for Capability {
    type Err = CapabilityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (family, action) = s.split_once('.').ok_or(CapabilityError::UnknownAction)?;
        if action.contains('.') {
            return Err(CapabilityError::UnknownAction);
        }
        Self::from_family_action(family.parse()?, action)
    }
}

impl ResourceDescriptor {
    pub const fn family(&self) -> CapabilityFamily {
        match self {
            Self::Filesystem(_) => CapabilityFamily::Fs,
            Self::Process(_) => CapabilityFamily::Proc,
            Self::Network(_) => CapabilityFamily::Net,
            Self::Git(_) => CapabilityFamily::Git,
            Self::Secret(_) => CapabilityFamily::Secret,
            Self::Browser(_) => CapabilityFamily::Browser,
            Self::Mobile(_) => CapabilityFamily::Mobile,
            Self::Mcp(_) => CapabilityFamily::Mcp,
            Self::Plugin(_) => CapabilityFamily::Plugin,
        }
    }

    fn browser_path(&self) -> Option<&PathGlob> {
        match self {
            Self::Browser(scope) => scope.path.as_ref(),
            _ => None,
        }
    }
}

impl FilesystemScope {
    /// Repository-relative glob. Absolute and traversal forms fail.
    pub fn repo(glob: &str) -> Result<Self, CapabilityError> {
        Ok(Self {
            root: FilesystemRoot::Repo,
            glob: PathGlob::repo(glob)?,
        })
    }

    /// Host glob. `..` and UNC/double-slash forms fail; local absolute paths are allowed.
    pub fn host(glob: &str) -> Result<Self, CapabilityError> {
        Ok(Self {
            root: FilesystemRoot::Host,
            glob: PathGlob::host(glob)?,
        })
    }

    pub fn root(&self) -> FilesystemRoot {
        self.root
    }

    pub fn glob(&self) -> &PathGlob {
        &self.glob
    }
}

impl PathGlob {
    pub fn repo(glob: &str) -> Result<Self, CapabilityError> {
        let path = RepoPath::parse(glob).map_err(map_repo_path)?;
        Ok(Self(path.as_str().to_owned()))
    }

    pub fn host(glob: &str) -> Result<Self, CapabilityError> {
        let normalized = normalize_host_glob(glob)?;
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ProcessScope {
    pub fn new(command_family: &str) -> Result<Self, CapabilityError> {
        Ok(Self {
            command_family: CommandFamily::parse(command_family)?,
        })
    }

    pub fn command_family(&self) -> &CommandFamily {
        &self.command_family
    }
}

impl CommandFamily {
    pub fn parse(value: &str) -> Result<Self, CapabilityError> {
        let ident = parse_token(value, TokenKind::CommandFamily)?;
        Ok(Self(ident))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl NetworkScheme {
    pub const ALL: &'static [Self] = &[Self::Http, Self::Https, Self::Ws, Self::Wss];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Ws => "ws",
            Self::Wss => "wss",
        }
    }

    pub const fn default_port(self) -> u16 {
        match self {
            Self::Http | Self::Ws => 80,
            Self::Https | Self::Wss => 443,
        }
    }
}

impl FromStr for NetworkScheme {
    type Err = CapabilityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for scheme in Self::ALL {
            if scheme.as_str() == s {
                return Ok(*scheme);
            }
        }
        Err(CapabilityError::UnknownScheme)
    }
}

impl NetworkScope {
    pub fn new(scheme: NetworkScheme, host: &str, port: u16) -> Result<Self, CapabilityError> {
        if port == 0 {
            return Err(CapabilityError::InvalidPort);
        }
        Ok(Self {
            scheme,
            host: Hostname::parse(host)?,
            port,
        })
    }

    pub fn scheme(&self) -> NetworkScheme {
        self.scheme
    }

    pub fn host(&self) -> &Hostname {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Hostname {
    /// Parse a concrete host. Wildcards, glob metacharacters, and whitespace fail closed.
    pub fn parse(value: &str) -> Result<Self, CapabilityError> {
        if value.is_empty() {
            return Err(CapabilityError::Empty);
        }
        if value.len() > MAX_HOST_BYTES {
            return Err(CapabilityError::TooLong);
        }
        if value.contains('\0') {
            return Err(CapabilityError::Nul);
        }
        if value.chars().any(char::is_control) {
            return Err(CapabilityError::Control);
        }
        if value.chars().any(char::is_whitespace) || value.chars().any(is_glob_metachar) {
            return Err(CapabilityError::InvalidHost);
        }
        if value.contains('/') || value.contains('\\') || value.contains('@') || value.contains(':')
        {
            return Err(CapabilityError::InvalidHost);
        }
        if value.contains("..") {
            return Err(CapabilityError::Traversal);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl GitScope {
    pub fn new(ref_scope: &str) -> Result<Self, CapabilityError> {
        Ok(Self {
            ref_scope: GitRefScope::parse(ref_scope)?,
        })
    }

    pub fn ref_scope(&self) -> &GitRefScope {
        &self.ref_scope
    }
}

impl GitRefScope {
    pub fn parse(value: &str) -> Result<Self, CapabilityError> {
        let ident = parse_token(value, TokenKind::GitRef)?;
        Ok(Self(ident))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SecretScope {
    pub fn new(secret_id: &str, target: &str) -> Result<Self, CapabilityError> {
        Ok(Self {
            secret_id: SecretHandle::parse(secret_id)?,
            target: SecretUseTarget::parse(target)?,
        })
    }

    pub fn secret_id(&self) -> &SecretHandle {
        &self.secret_id
    }

    pub fn target(&self) -> &SecretUseTarget {
        &self.target
    }
}

impl SecretHandle {
    pub fn parse(value: &str) -> Result<Self, CapabilityError> {
        Ok(Self(parse_token(value, TokenKind::Secret)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SecretUseTarget {
    pub fn parse(value: &str) -> Result<Self, CapabilityError> {
        Ok(Self(parse_token(value, TokenKind::Secret)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Origin {
    pub fn new(scheme: NetworkScheme, host: &str, port: u16) -> Result<Self, CapabilityError> {
        if port == 0 {
            return Err(CapabilityError::InvalidPort);
        }
        Ok(Self {
            scheme,
            host: Hostname::parse(host)?,
            port,
        })
    }

    /// Parse `scheme://host[:port]`. Userinfo, path, query, and fragment fail.
    pub fn parse(value: &str) -> Result<Self, CapabilityError> {
        if value.is_empty() {
            return Err(CapabilityError::Empty);
        }
        if value.len() > MAX_ORIGIN_BYTES {
            return Err(CapabilityError::TooLong);
        }
        if value.contains('\0') {
            return Err(CapabilityError::Nul);
        }
        if value.chars().any(char::is_control) {
            return Err(CapabilityError::Control);
        }
        if value.contains('@') {
            return Err(CapabilityError::InvalidOrigin);
        }
        let (scheme_raw, rest) = value
            .split_once("://")
            .ok_or(CapabilityError::InvalidOrigin)?;
        let scheme: NetworkScheme = scheme_raw.to_ascii_lowercase().parse()?;
        if rest.is_empty() || rest.contains('/') || rest.contains('?') || rest.contains('#') {
            return Err(CapabilityError::InvalidOrigin);
        }
        let (host_raw, port) = match rest.rsplit_once(':') {
            Some((host, port_raw)) => {
                let port: u16 = port_raw.parse().map_err(|_| CapabilityError::InvalidPort)?;
                (host, port)
            }
            None => (rest, scheme.default_port()),
        };
        Self::new(scheme, host_raw, port)
    }

    pub fn scheme(&self) -> NetworkScheme {
        self.scheme
    }

    pub fn host(&self) -> &Hostname {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.port == self.scheme.default_port() {
            write!(f, "{}://{}", self.scheme.as_str(), self.host.as_str())
        } else {
            write!(
                f,
                "{}://{}:{}",
                self.scheme.as_str(),
                self.host.as_str(),
                self.port
            )
        }
    }
}

impl BrowserScope {
    pub fn navigate(origin: Origin) -> Self {
        Self { origin, path: None }
    }

    pub fn download(origin: Origin, path: &str) -> Result<Self, CapabilityError> {
        Ok(Self {
            origin,
            path: Some(PathGlob::repo(path).or_else(|_| PathGlob::host(path))?),
        })
    }

    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    pub fn path(&self) -> Option<&PathGlob> {
        self.path.as_ref()
    }
}

impl MobileScope {
    pub fn new(device_id: &str) -> Result<Self, CapabilityError> {
        Ok(Self {
            device_id: DeviceId::parse(device_id)?,
        })
    }

    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }
}

impl DeviceId {
    pub fn parse(value: &str) -> Result<Self, CapabilityError> {
        Ok(Self(parse_token(value, TokenKind::Device)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl McpScope {
    pub fn new(server: &str, tool: &str) -> Result<Self, CapabilityError> {
        Ok(Self {
            server: parse_token(server, TokenKind::Mcp)?,
            tool: parse_token(tool, TokenKind::Mcp)?,
        })
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }
}

impl PluginScope {
    pub fn new(plugin: &str, capability: &str) -> Result<Self, CapabilityError> {
        Ok(Self {
            plugin: parse_token(plugin, TokenKind::Plugin)?,
            capability: parse_token(capability, TokenKind::Plugin)?,
        })
    }

    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    pub fn capability(&self) -> &str {
        &self.capability
    }
}

impl FilesystemRoot {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::Host => "host",
        }
    }
}

impl FromStr for FilesystemRoot {
    type Err = CapabilityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "repo" => Ok(Self::Repo),
            "host" => Ok(Self::Host),
            _ => Err(CapabilityError::UnknownRoot),
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for CapabilityFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::UnknownFamily => "unknown capability family",
            Self::UnknownAction => "unknown capability action",
            Self::UnknownScheme => "unknown network scheme",
            Self::UnknownRoot => "unknown filesystem root",
            Self::UnknownKind => "unknown resource kind",
            Self::UnsupportedSchema => "unsupported capability schema version",
            Self::UnknownField => "unknown capability field",
            Self::MissingField => "missing required capability field",
            Self::FamilyMismatch => "capability family does not match resource",
            Self::MissingScope => "required resource scope is missing",
            Self::Empty => "empty capability field",
            Self::TooLong => "capability field exceeds bound",
            Self::Nul => "capability field contains NUL",
            Self::Control => "capability field contains a control character",
            Self::Traversal => "capability path contains a traversal component",
            Self::AbsolutePath => "repository scope must be a relative path",
            Self::Unc => "UNC path is not a local filesystem scope",
            Self::InvalidIdent => "invalid capability identifier",
            Self::InvalidHost => "invalid hostname",
            Self::InvalidOrigin => "invalid origin",
            Self::InvalidPort => "invalid port",
            Self::InvalidJson => "capability JSON is not a closed object",
        };
        f.write_str(text)
    }
}

impl Error for CapabilityError {}

impl Serialize for Capability {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Capability", 4)?;
        state.serialize_field("schema", CAPABILITY_SCHEMA)?;
        state.serialize_field("schema_version", &CAPABILITY_SCHEMA_VERSION)?;
        state.serialize_field("family", self.family().as_str())?;
        state.serialize_field("action", self.action_name())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for Capability {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        decode_capability(&value).map_err(de::Error::custom)
    }
}

impl Serialize for ResourceDescriptor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Filesystem(scope) => {
                let mut state = serializer.serialize_struct("ResourceDescriptor", 5)?;
                serialize_resource_header(&mut state, "filesystem")?;
                state.serialize_field("root", scope.root.as_str())?;
                state.serialize_field("glob", scope.glob.as_str())?;
                state.end()
            }
            Self::Process(scope) => {
                let mut state = serializer.serialize_struct("ResourceDescriptor", 4)?;
                serialize_resource_header(&mut state, "process")?;
                state.serialize_field("command_family", scope.command_family.as_str())?;
                state.end()
            }
            Self::Network(scope) => {
                let mut state = serializer.serialize_struct("ResourceDescriptor", 6)?;
                serialize_resource_header(&mut state, "network")?;
                state.serialize_field("scheme", scope.scheme.as_str())?;
                state.serialize_field("host", scope.host.as_str())?;
                state.serialize_field("port", &scope.port)?;
                state.end()
            }
            Self::Git(scope) => {
                let mut state = serializer.serialize_struct("ResourceDescriptor", 4)?;
                serialize_resource_header(&mut state, "git")?;
                state.serialize_field("ref_scope", scope.ref_scope.as_str())?;
                state.end()
            }
            Self::Secret(scope) => {
                let mut state = serializer.serialize_struct("ResourceDescriptor", 5)?;
                serialize_resource_header(&mut state, "secret")?;
                state.serialize_field("secret_id", scope.secret_id.as_str())?;
                state.serialize_field("target", scope.target.as_str())?;
                state.end()
            }
            Self::Browser(scope) => {
                let fields = 4 + usize::from(scope.path.is_some());
                let mut state = serializer.serialize_struct("ResourceDescriptor", fields)?;
                serialize_resource_header(&mut state, "browser")?;
                state.serialize_field("origin", &scope.origin.to_string())?;
                if let Some(path) = &scope.path {
                    state.serialize_field("path", path.as_str())?;
                }
                state.end()
            }
            Self::Mobile(scope) => {
                let mut state = serializer.serialize_struct("ResourceDescriptor", 4)?;
                serialize_resource_header(&mut state, "mobile")?;
                state.serialize_field("device_id", scope.device_id.as_str())?;
                state.end()
            }
            Self::Mcp(scope) => {
                let mut state = serializer.serialize_struct("ResourceDescriptor", 5)?;
                serialize_resource_header(&mut state, "mcp")?;
                state.serialize_field("server", scope.server.as_str())?;
                state.serialize_field("tool", scope.tool.as_str())?;
                state.end()
            }
            Self::Plugin(scope) => {
                let mut state = serializer.serialize_struct("ResourceDescriptor", 5)?;
                serialize_resource_header(&mut state, "plugin")?;
                state.serialize_field("plugin", scope.plugin.as_str())?;
                state.serialize_field("capability", scope.capability.as_str())?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ResourceDescriptor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        decode_resource(&value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy)]
enum TokenKind {
    CommandFamily,
    GitRef,
    Secret,
    Device,
    Mcp,
    Plugin,
}

fn parse_token(value: &str, kind: TokenKind) -> Result<String, CapabilityError> {
    if value.is_empty() {
        return Err(CapabilityError::Empty);
    }
    if value.len() > MAX_IDENT_BYTES {
        return Err(CapabilityError::TooLong);
    }
    if value.contains('\0') {
        return Err(CapabilityError::Nul);
    }
    if value.chars().any(char::is_control) {
        return Err(CapabilityError::Control);
    }
    if value.split(['/', '\\']).any(|part| part == "..") {
        return Err(CapabilityError::Traversal);
    }
    let allowed = match kind {
        TokenKind::CommandFamily => value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+')),
        TokenKind::GitRef => value.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'*' | b'?')
        }),
        TokenKind::Secret => value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b':' | b'_' | b'-' | b'/')),
        TokenKind::Device | TokenKind::Mcp | TokenKind::Plugin => value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':')),
    };
    if !allowed {
        return Err(CapabilityError::InvalidIdent);
    }
    Ok(value.to_owned())
}

fn normalize_host_glob(glob: &str) -> Result<String, CapabilityError> {
    if glob.is_empty() {
        return Err(CapabilityError::Empty);
    }
    if glob.len() > MAX_PATH_GLOB_BYTES {
        return Err(CapabilityError::TooLong);
    }
    if glob.contains('\0') {
        return Err(CapabilityError::Nul);
    }
    if glob.chars().any(char::is_control) {
        return Err(CapabilityError::Control);
    }
    if is_unc(glob) {
        return Err(CapabilityError::Unc);
    }
    let mut parts = Vec::new();
    let absolute = glob.starts_with('/') || glob.starts_with('\\');
    for component in glob.split(['/', '\\']) {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err(CapabilityError::Traversal);
        }
        parts.push(component);
    }
    if parts.is_empty() {
        return Err(CapabilityError::Empty);
    }
    let mut normalized = parts.join("/");
    if absolute {
        normalized.insert(0, '/');
    }
    if normalized.len() > MAX_PATH_GLOB_BYTES {
        return Err(CapabilityError::TooLong);
    }
    Ok(normalized)
}

fn is_glob_metachar(c: char) -> bool {
    matches!(c, '*' | '?' | '[' | ']' | '{' | '}')
}

fn is_unc(input: &str) -> bool {
    let bytes = input.as_bytes();
    bytes.len() >= 2 && matches!(bytes[0], b'/' | b'\\') && matches!(bytes[1], b'/' | b'\\')
}

fn map_repo_path(err: RepoPathError) -> CapabilityError {
    match err {
        RepoPathError::Empty => CapabilityError::Empty,
        RepoPathError::TooLong => CapabilityError::TooLong,
        RepoPathError::Nul => CapabilityError::Nul,
        RepoPathError::Control => CapabilityError::Control,
        RepoPathError::Absolute | RepoPathError::WindowsDrive | RepoPathError::Unc => {
            CapabilityError::AbsolutePath
        }
        RepoPathError::Traversal => CapabilityError::Traversal,
    }
}

fn decode_capability(value: &Value) -> Result<Capability, CapabilityError> {
    let object = object_map(value)?;
    expect_keys(object, CAPABILITY_FIELDS, &[])?;
    expect_schema(object, CAPABILITY_SCHEMA)?;
    let family: CapabilityFamily = require_str(object, "family")?.parse()?;
    let action = require_str(object, "action")?;
    Capability::from_family_action(family, action)
}

fn decode_resource(value: &Value) -> Result<ResourceDescriptor, CapabilityError> {
    let object = object_map(value)?;
    expect_schema(object, RESOURCE_SCHEMA)?;
    let kind = require_str(object, "kind")?;
    match kind {
        "filesystem" => {
            expect_keys(
                object,
                &["schema", "schema_version", "kind", "root", "glob"],
                &[],
            )?;
            let root: FilesystemRoot = require_str(object, "root")?.parse()?;
            let glob = require_str(object, "glob")?;
            let scope = match root {
                FilesystemRoot::Repo => FilesystemScope::repo(glob)?,
                FilesystemRoot::Host => FilesystemScope::host(glob)?,
            };
            Ok(ResourceDescriptor::Filesystem(scope))
        }
        "process" => {
            expect_keys(
                object,
                &["schema", "schema_version", "kind", "command_family"],
                &[],
            )?;
            Ok(ResourceDescriptor::Process(ProcessScope::new(
                require_str(object, "command_family")?,
            )?))
        }
        "network" => {
            expect_keys(
                object,
                &["schema", "schema_version", "kind", "scheme", "host", "port"],
                &[],
            )?;
            let scheme: NetworkScheme = require_str(object, "scheme")?.parse()?;
            Ok(ResourceDescriptor::Network(NetworkScope::new(
                scheme,
                require_str(object, "host")?,
                require_u16(object, "port")?,
            )?))
        }
        "git" => {
            expect_keys(
                object,
                &["schema", "schema_version", "kind", "ref_scope"],
                &[],
            )?;
            Ok(ResourceDescriptor::Git(GitScope::new(require_str(
                object,
                "ref_scope",
            )?)?))
        }
        "secret" => {
            expect_keys(
                object,
                &["schema", "schema_version", "kind", "secret_id", "target"],
                &[],
            )?;
            Ok(ResourceDescriptor::Secret(SecretScope::new(
                require_str(object, "secret_id")?,
                require_str(object, "target")?,
            )?))
        }
        "browser" => {
            expect_keys(
                object,
                &["schema", "schema_version", "kind", "origin"],
                &["path"],
            )?;
            let origin = Origin::parse(require_str(object, "origin")?)?;
            let scope = match object.get("path") {
                Some(Value::Null) | None => BrowserScope::navigate(origin),
                Some(Value::String(path)) => BrowserScope::download(origin, path)?,
                Some(_) => return Err(CapabilityError::InvalidJson),
            };
            Ok(ResourceDescriptor::Browser(scope))
        }
        "mobile" => {
            expect_keys(
                object,
                &["schema", "schema_version", "kind", "device_id"],
                &[],
            )?;
            Ok(ResourceDescriptor::Mobile(MobileScope::new(require_str(
                object,
                "device_id",
            )?)?))
        }
        "mcp" => {
            expect_keys(
                object,
                &["schema", "schema_version", "kind", "server", "tool"],
                &[],
            )?;
            Ok(ResourceDescriptor::Mcp(McpScope::new(
                require_str(object, "server")?,
                require_str(object, "tool")?,
            )?))
        }
        "plugin" => {
            expect_keys(
                object,
                &["schema", "schema_version", "kind", "plugin", "capability"],
                &[],
            )?;
            Ok(ResourceDescriptor::Plugin(PluginScope::new(
                require_str(object, "plugin")?,
                require_str(object, "capability")?,
            )?))
        }
        _ => Err(CapabilityError::UnknownKind),
    }
}

fn serialize_resource_header<S: serde::ser::SerializeStruct>(
    state: &mut S,
    kind: &'static str,
) -> Result<(), S::Error> {
    state.serialize_field("schema", RESOURCE_SCHEMA)?;
    state.serialize_field("schema_version", &CAPABILITY_SCHEMA_VERSION)?;
    state.serialize_field("kind", kind)?;
    Ok(())
}

fn object_map(value: &Value) -> Result<&Map<String, Value>, CapabilityError> {
    value.as_object().ok_or(CapabilityError::InvalidJson)
}

fn expect_schema(object: &Map<String, Value>, schema: &str) -> Result<(), CapabilityError> {
    let got = require_str(object, "schema")?;
    if got != schema {
        return Err(CapabilityError::UnsupportedSchema);
    }
    let version = require_u16(object, "schema_version")?;
    if version != CAPABILITY_SCHEMA_VERSION {
        return Err(CapabilityError::UnsupportedSchema);
    }
    Ok(())
}

fn expect_keys(
    object: &Map<String, Value>,
    required: &[&str],
    optional: &[&str],
) -> Result<(), CapabilityError> {
    for key in object.keys() {
        if !required.contains(&key.as_str()) && !optional.contains(&key.as_str()) {
            return Err(CapabilityError::UnknownField);
        }
    }
    for key in required {
        if !object.contains_key(*key) {
            return Err(CapabilityError::MissingField);
        }
    }
    Ok(())
}

fn require_str<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, CapabilityError> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(CapabilityError::InvalidJson),
        None => Err(CapabilityError::MissingField),
    }
}

fn require_u16(object: &Map<String, Value>, key: &str) -> Result<u16, CapabilityError> {
    match object.get(key) {
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| *n != 0)
            .ok_or(CapabilityError::InvalidPort),
        Some(_) => Err(CapabilityError::InvalidJson),
        None => Err(CapabilityError::MissingField),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_FS: &str =
        r#"{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"}"#;
    const GOLDEN_PROC: &str =
        r#"{"schema":"rapidlm.capability","schema_version":1,"family":"proc","action":"exec"}"#;
    const GOLDEN_NET: &str =
        r#"{"schema":"rapidlm.capability","schema_version":1,"family":"net","action":"connect"}"#;
    const GOLDEN_GIT: &str =
        r#"{"schema":"rapidlm.capability","schema_version":1,"family":"git","action":"write"}"#;
    const GOLDEN_SECRET: &str =
        r#"{"schema":"rapidlm.capability","schema_version":1,"family":"secret","action":"use"}"#;
    const GOLDEN_BROWSER: &str = r#"{"schema":"rapidlm.capability","schema_version":1,"family":"browser","action":"navigate"}"#;
    const GOLDEN_MOBILE: &str = r#"{"schema":"rapidlm.capability","schema_version":1,"family":"mobile","action":"control"}"#;
    const GOLDEN_MCP: &str =
        r#"{"schema":"rapidlm.capability","schema_version":1,"family":"mcp","action":"invoke"}"#;
    const GOLDEN_PLUGIN: &str =
        r#"{"schema":"rapidlm.capability","schema_version":1,"family":"plugin","action":"invoke"}"#;

    const GOLDEN_RES_FS: &str = r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"repo","glob":"src/**/*.rs"}"#;
    const GOLDEN_RES_PROC: &str = r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"process","command_family":"cargo"}"#;
    const GOLDEN_RES_NET: &str = r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"network","scheme":"https","host":"example.com","port":443}"#;
    const GOLDEN_RES_GIT: &str = r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"git","ref_scope":"refs/heads/main"}"#;
    const GOLDEN_RES_SECRET: &str = r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"secret","secret_id":"env:NPM_TOKEN","target":"env"}"#;
    const GOLDEN_RES_BROWSER: &str = r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"browser","origin":"https://example.com"}"#;
    const GOLDEN_RES_MOBILE: &str = r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"mobile","device_id":"emulator-5554"}"#;
    const GOLDEN_RES_MCP: &str = r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"mcp","server":"github","tool":"create_issue"}"#;
    const GOLDEN_RES_PLUGIN: &str = r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"plugin","plugin":"formatter","capability":"format"}"#;

    fn family_fixtures() -> [(
        CapabilityFamily,
        Capability,
        &'static str,
        ResourceDescriptor,
        &'static str,
    ); 9] {
        [
            (
                CapabilityFamily::Fs,
                Capability::FsRead,
                GOLDEN_FS,
                ResourceDescriptor::Filesystem(FilesystemScope::repo("src/**/*.rs").expect("fs")),
                GOLDEN_RES_FS,
            ),
            (
                CapabilityFamily::Proc,
                Capability::ProcExec,
                GOLDEN_PROC,
                ResourceDescriptor::Process(ProcessScope::new("cargo").expect("proc")),
                GOLDEN_RES_PROC,
            ),
            (
                CapabilityFamily::Net,
                Capability::NetConnect,
                GOLDEN_NET,
                ResourceDescriptor::Network(
                    NetworkScope::new(NetworkScheme::Https, "example.com", 443).expect("net"),
                ),
                GOLDEN_RES_NET,
            ),
            (
                CapabilityFamily::Git,
                Capability::GitWrite,
                GOLDEN_GIT,
                ResourceDescriptor::Git(GitScope::new("refs/heads/main").expect("git")),
                GOLDEN_RES_GIT,
            ),
            (
                CapabilityFamily::Secret,
                Capability::SecretUse,
                GOLDEN_SECRET,
                ResourceDescriptor::Secret(
                    SecretScope::new("env:NPM_TOKEN", "env").expect("secret"),
                ),
                GOLDEN_RES_SECRET,
            ),
            (
                CapabilityFamily::Browser,
                Capability::BrowserNavigate,
                GOLDEN_BROWSER,
                ResourceDescriptor::Browser(BrowserScope::navigate(
                    Origin::parse("https://example.com").expect("origin"),
                )),
                GOLDEN_RES_BROWSER,
            ),
            (
                CapabilityFamily::Mobile,
                Capability::MobileControl,
                GOLDEN_MOBILE,
                ResourceDescriptor::Mobile(MobileScope::new("emulator-5554").expect("mobile")),
                GOLDEN_RES_MOBILE,
            ),
            (
                CapabilityFamily::Mcp,
                Capability::McpInvoke,
                GOLDEN_MCP,
                ResourceDescriptor::Mcp(McpScope::new("github", "create_issue").expect("mcp")),
                GOLDEN_RES_MCP,
            ),
            (
                CapabilityFamily::Plugin,
                Capability::PluginInvoke,
                GOLDEN_PLUGIN,
                ResourceDescriptor::Plugin(
                    PluginScope::new("formatter", "format").expect("plugin"),
                ),
                GOLDEN_RES_PLUGIN,
            ),
        ]
    }

    #[test]
    fn serde_fixtures_cover_every_v1_family() {
        let fixtures = family_fixtures();
        assert_eq!(fixtures.len(), CapabilityFamily::ALL.len());
        for (i, (family, capability, cap_json, resource, res_json)) in fixtures.iter().enumerate() {
            assert_eq!(*family, CapabilityFamily::ALL[i]);
            assert_eq!(capability.family(), *family);
            assert_eq!(resource.family(), *family);
            capability.compatible_with(resource).expect("compatible");

            let encoded = serde_json::to_string(capability).expect("encode capability");
            assert_eq!(encoded, *cap_json, "capability golden {}", family.as_str());
            let decoded: Capability = serde_json::from_str(cap_json).expect("decode capability");
            assert_eq!(decoded, *capability);

            let encoded_res = serde_json::to_string(resource).expect("encode resource");
            assert_eq!(
                encoded_res,
                *res_json,
                "resource golden {}",
                family.as_str()
            );
            let decoded_res: ResourceDescriptor =
                serde_json::from_str(res_json).expect("decode resource");
            assert_eq!(decoded_res, *resource);
        }
    }

    #[test]
    fn unknown_capabilities_fail_closed() {
        for json in [
            r#"{"schema":"rapidlm.capability","schema_version":1,"family":"superuser","action":"all"}"#,
            r#"{"schema":"rapidlm.capability","schema_version":1,"family":"*","action":"read"}"#,
            r#"{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"admin"}"#,
            r#"{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"*"}"#,
            r#"{"schema":"rapidlm.capability","schema_version":2,"family":"fs","action":"read"}"#,
            r#"{"schema":"rapidlm.capability.v2","schema_version":1,"family":"fs","action":"read"}"#,
            r#"{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read","allow":true}"#,
            r#"{"family":"fs","action":"read"}"#,
        ] {
            serde_json::from_str::<Capability>(json)
                .expect_err("unknown capability must fail closed");
        }
        assert_eq!(
            "*".parse::<Capability>(),
            Err(CapabilityError::UnknownAction)
        );
        assert_eq!(
            "all".parse::<Capability>(),
            Err(CapabilityError::UnknownAction)
        );
        assert_eq!(
            "superuser.all".parse::<Capability>(),
            Err(CapabilityError::UnknownFamily)
        );
        assert_eq!(
            "fs.delete".parse::<Capability>(),
            Err(CapabilityError::UnknownAction)
        );
    }

    #[test]
    fn unknown_resource_fields_and_kinds_fail_closed() {
        for json in [
            r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"kernel","glob":"src"}"#,
            r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"repo","glob":"src","allow":true}"#,
            r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"secret","secret_id":"env:X","target":"env","value":"sk-canary"}"#,
            r#"{"schema":"rapidlm.resource_descriptor","schema_version":2,"kind":"filesystem","root":"repo","glob":"src"}"#,
        ] {
            serde_json::from_str::<ResourceDescriptor>(json)
                .expect_err("unknown resource must fail closed");
        }
    }

    #[test]
    fn family_mismatch_fails_closed() {
        let fs = ResourceDescriptor::Filesystem(FilesystemScope::repo("src").expect("fs"));
        let err = Capability::NetConnect
            .compatible_with(&fs)
            .expect_err("mismatch");
        assert_eq!(err, CapabilityError::FamilyMismatch);
    }

    #[test]
    fn browser_download_requires_path() {
        let navigate = ResourceDescriptor::Browser(BrowserScope::navigate(
            Origin::parse("https://example.com").expect("origin"),
        ));
        let err = Capability::BrowserDownload
            .compatible_with(&navigate)
            .expect_err("missing path");
        assert_eq!(err, CapabilityError::MissingScope);
        let download = ResourceDescriptor::Browser(
            BrowserScope::download(
                Origin::parse("https://example.com").expect("origin"),
                "artifacts/a.bin",
            )
            .expect("download"),
        );
        Capability::BrowserDownload
            .compatible_with(&download)
            .expect("path present");
    }

    #[test]
    fn repo_globs_reject_traversal_and_absolute() {
        assert_eq!(
            FilesystemScope::repo("../etc/passwd").unwrap_err(),
            CapabilityError::Traversal
        );
        assert_eq!(
            FilesystemScope::repo("/etc/passwd").unwrap_err(),
            CapabilityError::AbsolutePath
        );
        assert_eq!(
            FilesystemScope::host("../etc/passwd").unwrap_err(),
            CapabilityError::Traversal
        );
        let host = FilesystemScope::host("/var/tmp/*.log").expect("host glob");
        assert_eq!(host.root(), FilesystemRoot::Host);
        assert_eq!(host.glob().as_str(), "/var/tmp/*.log");
    }

    #[test]
    fn origin_rejects_userinfo_and_unknown_scheme() {
        assert_eq!(
            Origin::parse("https://user:pass@example.com").unwrap_err(),
            CapabilityError::InvalidOrigin
        );
        assert_eq!(
            Origin::parse("file://example.com").unwrap_err(),
            CapabilityError::UnknownScheme
        );
        assert_eq!(
            Origin::parse("https://example.com/secret").unwrap_err(),
            CapabilityError::InvalidOrigin
        );
        let origin = Origin::parse("HTTPS://Example.COM").expect("case");
        assert_eq!(origin.to_string(), "https://example.com");
    }

    #[test]
    fn secret_scope_has_no_plaintext_field() {
        let scope = SecretScope::new("env:NPM_TOKEN", "env").expect("secret");
        let json = serde_json::to_string(&ResourceDescriptor::Secret(scope)).expect("json");
        assert!(!json.contains("value"));
        assert!(!json.contains("plaintext"));
        assert!(!json.contains("sk-"));
        assert!(json.contains("env:NPM_TOKEN"));
    }

    #[test]
    fn git_write_is_not_fs_write() {
        assert_ne!(Capability::GitWrite, Capability::FsWrite);
        assert_eq!(Capability::GitWrite.family(), CapabilityFamily::Git);
        assert_eq!(Capability::FsWrite.family(), CapabilityFamily::Fs);
        assert_eq!(Capability::GitWrite.as_str(), "git.write");
    }

    #[test]
    fn wildcard_hosts_cannot_construct_a_grant() {
        // T-005: DNS/SSRF grants require a concrete host. `*` and glob forms
        // must not construct NetworkScope / Origin.
        for host in [
            "*",
            "*.example.com",
            "example.*.com",
            "example.com*",
            "example?.com",
            "exam ple.com",
            "example.com ",
            " example.com",
            "[::1]",
            "{localhost}",
        ] {
            assert_eq!(
                Hostname::parse(host),
                Err(CapabilityError::InvalidHost),
                "hostname {host:?} must fail closed"
            );
            assert_eq!(
                NetworkScope::new(NetworkScheme::Https, host, 443).unwrap_err(),
                CapabilityError::InvalidHost,
                "network grant for {host:?} must fail closed"
            );
        }
        assert_eq!(
            Origin::parse("https://*").unwrap_err(),
            CapabilityError::InvalidHost
        );
        assert_eq!(
            Origin::parse("https://*.example.com").unwrap_err(),
            CapabilityError::InvalidHost
        );
        assert_eq!(
            Origin::new(NetworkScheme::Https, "*", 443).unwrap_err(),
            CapabilityError::InvalidHost
        );
        let wildcard_net = r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"network","scheme":"https","host":"*","port":443}"#;
        serde_json::from_str::<ResourceDescriptor>(wildcard_net)
            .expect_err("wildcard host resource must fail closed");
        let wildcard_origin = r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"browser","origin":"https://*"}"#;
        serde_json::from_str::<ResourceDescriptor>(wildcard_origin)
            .expect_err("wildcard origin resource must fail closed");
        let concrete = Hostname::parse("example.com").expect("concrete host");
        assert_eq!(concrete.as_str(), "example.com");
    }

    #[test]
    fn host_unc_forms_are_not_collapsed_to_local_paths() {
        // T-003: `\\server\share` / `//server/share` must not become `/server/share`.
        for sample in [
            r"\\server\share",
            "//server/share",
            r"\\server\share\file",
            "//server/share/**",
            r"/\server\share",
            r"\/server/share",
            r"\\?\C:\Windows",
            r"\\.\pipe\rapidlm",
        ] {
            assert_eq!(
                FilesystemScope::host(sample).unwrap_err(),
                CapabilityError::Unc,
                "UNC {sample:?} must not construct a host grant"
            );
            assert_eq!(
                PathGlob::host(sample).unwrap_err(),
                CapabilityError::Unc,
                "UNC {sample:?} must not normalize to a local path"
            );
        }
        let local = FilesystemScope::host("/var/tmp/*.log").expect("local host glob");
        assert_eq!(local.glob().as_str(), "/var/tmp/*.log");
        assert_ne!(local.glob().as_str(), "/server/share");
        let unc_json = r#"{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"host","glob":"//server/share"}"#;
        serde_json::from_str::<ResourceDescriptor>(unc_json)
            .expect_err("UNC host glob resource must fail closed");
    }
}
