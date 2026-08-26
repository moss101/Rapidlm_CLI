//! RapidLM MCP server mode.
//!
//! Exposes only explicitly published RapidLM tools/resources. The advertised
//! surface is the intersection of server config and effective policy. Client
//! identity is part of the policy principal. Host-only capabilities are never
//! implied and cannot be granted by client handshake fields (T-001, T-002,
//! T-007, T-008, T-012).

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use capability_broker::{
    ActionRequest, BrowserScope, CancellationToken, CanonicalAction, Capability, Decision,
    FilesystemScope, NetworkScheme, Origin, PolicyEvalError, PolicyStack, PrincipalRef,
    ProcessScope, ResourceDescriptor, evaluate,
};
use protocol::{ApiError, ErrorCode, RepoPath, SessionId, TraceId};
use serde_json::{Map, Value};

use crate::transport::{
    ImplementationInfo, MAX_FRAME_BYTES, McpTransport, ProtocolVersion, TransportError,
};

/// Wire schema name for bounded server-mode documents.
pub const SERVER_SCHEMA: &str = "rapidlm.mcp_server";

/// Schema version mixed into server-mode documents.
pub const SERVER_SCHEMA_VERSION: u16 = 1;

/// Maximum tools accepted in one server config.
pub const MAX_PUBLISHED_TOOLS: usize = 32;

/// Maximum resources accepted in one server config.
pub const MAX_PUBLISHED_RESOURCES: usize = 64;

/// Maximum capability tokens accepted in one server config.
pub const MAX_PUBLISHED_CAPABILITIES: usize = 16;

/// Maximum properties on inbound `tools/call` arguments.
pub const MAX_CALL_ARGUMENT_FIELDS: usize = 32;

/// Maximum UTF-8 bytes for one serialized `tools/call` arguments object.
pub const MAX_CALL_ARGUMENTS_BYTES: usize = 16 * 1024;

/// Maximum UTF-8 bytes retained from one executor result document.
pub const MAX_CALL_RESULT_BYTES: usize = 16 * 1024;

const JSONRPC_VERSION: &str = "2.0";
const INITIALIZE_METHOD: &str = "initialize";
const INITIALIZED_METHOD: &str = "notifications/initialized";
const TOOLS_LIST: &str = "tools/list";
const TOOLS_CALL: &str = "tools/call";
const RESOURCES_LIST: &str = "resources/list";
const RESOURCES_READ: &str = "resources/read";
const PING_METHOD: &str = "ping";
const PRINCIPAL_PREFIX: &str = "mcp-client/";
const CANCEL_STRIDE: usize = 8;
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const APPLICATION_ERROR: i64 = -32000;
const REQUEST_CANCELLED: i64 = -32800;

const PRIVILEGE_ARGUMENT_KEYS: &[&str] = &[
    "approval",
    "capability",
    "capability_lease",
    "elevate",
    "grant_capability",
    "host_path",
    "lease",
    "override_policy",
    "password",
    "plaintext",
    "privileged",
    "secret",
    "secret_plaintext",
    "sudo",
    "token",
];

const HOST_ONLY_TOOL_NAMES: &[&str] = &[
    "agent.spawn",
    "agent.result",
    "external.call",
    "goal.update",
    "mobile.act",
    "secret.use",
    "shell.sudo",
];

/// Closed RapidLM tools that server mode may publish.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum RapidLmTool {
    RepoSearch,
    RepoRead,
    WorkspaceStatus,
    EvidenceRecord,
    WorkspacePatch,
    ShellExec,
    BrowserAct,
}

/// Privilege class that is off unless the matching capability is configured.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PrivilegedClass {
    None,
    Write,
    Shell,
    Browser,
}

/// Repo-scoped resource URI. Host and `file:` URIs are rejected.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PublishedResource {
    path: RepoPath,
}

/// Explicit publish set. Default is empty: no write/shell/browser.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpServerConfig {
    tools: BTreeSet<RapidLmTool>,
    resources: BTreeSet<PublishedResource>,
    capabilities: Vec<Capability>,
}

/// Handshake client recorded into the policy principal. Not a grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerClient {
    info: ImplementationInfo,
    principal: PrincipalRef,
    protocol_version: ProtocolVersion,
    session_id: SessionId,
}

/// Effective advertised surface after config ∩ policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedSurface {
    tools: Vec<RapidLmTool>,
    resources: Vec<PublishedResource>,
    capabilities: Vec<Capability>,
}

/// Authorized tool invocation. Executor still cannot broaden policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedInvocation {
    tool: RapidLmTool,
    principal: PrincipalRef,
    session_id: SessionId,
    capability: Capability,
    resource: ResourceDescriptor,
    arguments: Value,
}

/// Authorized resource read. Host roots never appear.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedResourceRead {
    resource: PublishedResource,
    principal: PrincipalRef,
    session_id: SessionId,
    capability: Capability,
}

/// Bounded MCP tool result. Executor text is untrusted data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerToolResult {
    is_error: bool,
    truncated: bool,
    result: Value,
}

/// Bounded MCP resource contents. Path is repo-relative only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerResourceContents {
    uri: String,
    mime_type: String,
    text: String,
    truncated: bool,
}

/// RapidLM MCP server. Business execution is delegated; this type gates.
pub struct McpServer<'a> {
    config: McpServerConfig,
    policy: &'a PolicyStack,
    client: Option<ServerClient>,
    closed: bool,
}

/// Side-effecting published-tool backend. Invoked only after the gate.
pub trait PublishedExecutor {
    fn invoke_tool(
        &self,
        invocation: &AuthorizedInvocation,
        cancel: &CancellationToken,
    ) -> Result<ServerToolResult, ServerError>;

    fn read_resource(
        &self,
        request: &AuthorizedResourceRead,
        cancel: &CancellationToken,
    ) -> Result<ServerResourceContents, ServerError>;
}

/// Typed server-mode failure. Display never echoes frames, URIs, or args.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ServerError {
    Cancelled,
    InvalidIdent,
    InvalidArguments,
    ArgumentsTooLarge,
    InvalidFrame,
    FrameTooLarge,
    InvalidClient,
    UnsupportedProtocolVersion,
    NotInitialized,
    AlreadyInitialized,
    Closed,
    NotPublished,
    HostOnly,
    HostCapabilityNotConfigured,
    PolicyDenied,
    TooManyPublished,
    ResultTooLarge,
    ExecutorDenied,
    Transport(TransportError),
}

impl RapidLmTool {
    pub const ALL: &'static [Self] = &[
        Self::RepoSearch,
        Self::RepoRead,
        Self::WorkspaceStatus,
        Self::EvidenceRecord,
        Self::WorkspacePatch,
        Self::ShellExec,
        Self::BrowserAct,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepoSearch => "repo.search",
            Self::RepoRead => "repo.read",
            Self::WorkspaceStatus => "workspace.status",
            Self::EvidenceRecord => "evidence.record",
            Self::WorkspacePatch => "workspace.patch",
            Self::ShellExec => "shell.exec",
            Self::BrowserAct => "browser.act",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::RepoSearch => "Search repository files",
            Self::RepoRead => "Read a repository file excerpt",
            Self::WorkspaceStatus => "Read workspace status",
            Self::EvidenceRecord => "Record goal evidence",
            Self::WorkspacePatch => "Apply a workspace patch",
            Self::ShellExec => "Execute a shell command",
            Self::BrowserAct => "Perform a browser action",
        }
    }

    pub const fn capability(self) -> Capability {
        match self {
            Self::RepoSearch | Self::RepoRead | Self::WorkspaceStatus | Self::EvidenceRecord => {
                Capability::FsRead
            }
            Self::WorkspacePatch => Capability::FsWrite,
            Self::ShellExec => Capability::ProcExec,
            Self::BrowserAct => Capability::BrowserNavigate,
        }
    }

    pub const fn privileged_class(self) -> PrivilegedClass {
        match self {
            Self::RepoSearch | Self::RepoRead | Self::WorkspaceStatus | Self::EvidenceRecord => {
                PrivilegedClass::None
            }
            Self::WorkspacePatch => PrivilegedClass::Write,
            Self::ShellExec => PrivilegedClass::Shell,
            Self::BrowserAct => PrivilegedClass::Browser,
        }
    }

    pub const fn is_privileged(self) -> bool {
        !matches!(self.privileged_class(), PrivilegedClass::None)
    }

    pub fn parse(name: &str) -> Result<Self, ServerError> {
        for tool in Self::ALL {
            if tool.as_str() == name {
                return Ok(*tool);
            }
        }
        if HOST_ONLY_TOOL_NAMES.contains(&name) || looks_like_host_escape(name) {
            return Err(ServerError::HostOnly);
        }
        Err(ServerError::InvalidIdent)
    }
}

impl PrivilegedClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Write => "write",
            Self::Shell => "shell",
            Self::Browser => "browser",
        }
    }

    pub const fn required_capability(self) -> Option<Capability> {
        match self {
            Self::None => None,
            Self::Write => Some(Capability::FsWrite),
            Self::Shell => Some(Capability::ProcExec),
            Self::Browser => Some(Capability::BrowserNavigate),
        }
    }
}

impl PublishedResource {
    pub fn parse(uri: &str) -> Result<Self, ServerError> {
        let path = parse_repo_resource(uri)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn uri(&self) -> String {
        format!("repo:{}", self.path.as_str())
    }
}

impl McpServerConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tools(&self) -> impl Iterator<Item = RapidLmTool> + '_ {
        self.tools.iter().copied()
    }

    pub fn resources(&self) -> impl Iterator<Item = &PublishedResource> {
        self.resources.iter()
    }

    pub fn capabilities(&self) -> impl Iterator<Item = Capability> + '_ {
        self.capabilities.iter().copied()
    }

    pub fn with_tool(mut self, name: &str) -> Result<Self, ServerError> {
        let tool = RapidLmTool::parse(name)?;
        if self.tools.len() >= MAX_PUBLISHED_TOOLS {
            return Err(ServerError::TooManyPublished);
        }
        self.tools.insert(tool);
        Ok(self)
    }

    pub fn with_resource(mut self, uri: &str) -> Result<Self, ServerError> {
        let resource = PublishedResource::parse(uri)?;
        if self.resources.len() >= MAX_PUBLISHED_RESOURCES {
            return Err(ServerError::TooManyPublished);
        }
        self.resources.insert(resource);
        Ok(self)
    }

    pub fn with_capability(mut self, capability: Capability) -> Result<Self, ServerError> {
        if !is_publishable_capability(capability) {
            return Err(ServerError::HostOnly);
        }
        if self.capabilities.contains(&capability) {
            return Ok(self);
        }
        if self.capabilities.len() >= MAX_PUBLISHED_CAPABILITIES {
            return Err(ServerError::TooManyPublished);
        }
        self.capabilities.push(capability);
        Ok(self)
    }

    fn allows_privileged(&self, class: PrivilegedClass) -> bool {
        match class.required_capability() {
            None => true,
            Some(capability) => self.capabilities.contains(&capability),
        }
    }
}

impl ServerClient {
    pub fn info(&self) -> &ImplementationInfo {
        &self.info
    }

    pub fn principal(&self) -> &PrincipalRef {
        &self.principal
    }

    pub fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }
}

impl PublishedSurface {
    pub fn tools(&self) -> &[RapidLmTool] {
        &self.tools
    }

    pub fn resources(&self) -> &[PublishedResource] {
        &self.resources
    }

    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    pub fn has_tool(&self, tool: RapidLmTool) -> bool {
        self.tools.contains(&tool)
    }

    pub fn has_capability(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn exposes_write(&self) -> bool {
        self.has_capability(Capability::FsWrite) || self.has_tool(RapidLmTool::WorkspacePatch)
    }

    pub fn exposes_shell(&self) -> bool {
        self.has_capability(Capability::ProcExec) || self.has_tool(RapidLmTool::ShellExec)
    }

    pub fn exposes_browser(&self) -> bool {
        self.has_capability(Capability::BrowserNavigate)
            || self.has_capability(Capability::BrowserDownload)
            || self.has_tool(RapidLmTool::BrowserAct)
    }
}

impl AuthorizedInvocation {
    pub fn tool(&self) -> RapidLmTool {
        self.tool
    }

    pub fn principal(&self) -> &PrincipalRef {
        &self.principal
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }

    pub fn resource(&self) -> &ResourceDescriptor {
        &self.resource
    }

    pub fn arguments(&self) -> &Value {
        &self.arguments
    }
}

impl AuthorizedResourceRead {
    pub fn resource(&self) -> &PublishedResource {
        &self.resource
    }

    pub fn principal(&self) -> &PrincipalRef {
        &self.principal
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }
}

impl ServerToolResult {
    pub fn new(is_error: bool, text: &str) -> Result<Self, ServerError> {
        let (text, truncated) = bound_text(text, MAX_CALL_RESULT_BYTES.saturating_sub(256));
        let mut content = Map::new();
        content.insert("type".to_owned(), Value::String("text".to_owned()));
        content.insert("text".to_owned(), Value::String(text));
        let mut result = Map::new();
        result.insert("schema".to_owned(), Value::String(SERVER_SCHEMA.to_owned()));
        result.insert(
            "schema_version".to_owned(),
            Value::from(SERVER_SCHEMA_VERSION),
        );
        result.insert("isError".to_owned(), Value::Bool(is_error));
        result.insert(
            "content".to_owned(),
            Value::Array(vec![Value::Object(content)]),
        );
        let encoded = encode_json_value(&Value::Object(result.clone()))?;
        if encoded.len() > MAX_CALL_RESULT_BYTES {
            return Err(ServerError::ResultTooLarge);
        }
        Ok(Self {
            is_error,
            truncated,
            result: Value::Object(result),
        })
    }

    pub fn is_error(&self) -> bool {
        self.is_error
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn result(&self) -> &Value {
        &self.result
    }
}

impl ServerResourceContents {
    pub fn new(
        resource: &PublishedResource,
        mime_type: &str,
        text: &str,
    ) -> Result<Self, ServerError> {
        let mime = parse_ident(mime_type, 64)?;
        let (text, truncated) = bound_text(text, MAX_CALL_RESULT_BYTES);
        Ok(Self {
            uri: resource.uri(),
            mime_type: mime,
            text,
            truncated,
        })
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

impl<'a> McpServer<'a> {
    pub fn new(config: McpServerConfig, policy: &'a PolicyStack) -> Self {
        Self {
            config,
            policy,
            client: None,
            closed: false,
        }
    }

    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    pub fn client(&self) -> Option<&ServerClient> {
        self.client.as_ref()
    }

    /// Record client identity as the policy principal. Handshake caps are ignored.
    pub fn accept_client(
        &mut self,
        info: ImplementationInfo,
        protocol_version: ProtocolVersion,
        cancel: &CancellationToken,
    ) -> Result<&ServerClient, ServerError> {
        check_open(self.closed, cancel)?;
        if self.client.is_some() {
            return Err(ServerError::AlreadyInitialized);
        }
        let principal = principal_for(&info)?;
        self.client = Some(ServerClient {
            info,
            principal,
            protocol_version,
            session_id: SessionId::new(),
        });
        self.client.as_ref().ok_or(ServerError::NotInitialized)
    }

    /// Published capability list = server config ∩ effective policy.
    pub fn published_surface(
        &self,
        cancel: &CancellationToken,
    ) -> Result<PublishedSurface, ServerError> {
        check_open(self.closed, cancel)?;
        let client = self.client.as_ref().ok_or(ServerError::NotInitialized)?;
        let mut tools = Vec::new();
        for (idx, tool) in self.config.tools.iter().copied().enumerate() {
            if idx.is_multiple_of(CANCEL_STRIDE) {
                cancel_check(cancel)?;
            }
            if self.tool_is_effective(tool, client, cancel)? {
                tools.push(tool);
            }
        }
        let mut resources = Vec::new();
        for (idx, resource) in self.config.resources.iter().enumerate() {
            if idx.is_multiple_of(CANCEL_STRIDE) {
                cancel_check(cancel)?;
            }
            if self.resource_is_effective(resource, client, cancel)? {
                resources.push(resource.clone());
            }
        }
        let mut capabilities = Vec::new();
        for tool in &tools {
            push_unique_capability(&mut capabilities, tool.capability());
        }
        if !resources.is_empty() {
            push_unique_capability(&mut capabilities, Capability::FsRead);
        }
        for capability in &self.config.capabilities {
            if is_publishable_capability(*capability)
                && self.capability_allowed(*capability, client, cancel)?
            {
                push_unique_capability(&mut capabilities, *capability);
            }
        }
        capabilities.sort_by_key(|capability| capability.as_str());
        Ok(PublishedSurface {
            tools,
            resources,
            capabilities,
        })
    }

    pub fn authorize_tool(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<AuthorizedInvocation, ServerError> {
        check_open(self.closed, cancel)?;
        let client = self.client.as_ref().ok_or(ServerError::NotInitialized)?;
        let tool = RapidLmTool::parse(name)?;
        if !self.tool_is_effective(tool, client, cancel)? {
            return Err(unpublished_error(tool));
        }
        let arguments = validate_arguments(arguments)?;
        let resource = representative_resource(tool)?;
        Ok(AuthorizedInvocation {
            tool,
            principal: client.principal.clone(),
            session_id: client.session_id,
            capability: tool.capability(),
            resource,
            arguments,
        })
    }

    pub fn authorize_resource(
        &self,
        uri: &str,
        cancel: &CancellationToken,
    ) -> Result<AuthorizedResourceRead, ServerError> {
        check_open(self.closed, cancel)?;
        let client = self.client.as_ref().ok_or(ServerError::NotInitialized)?;
        let resource = PublishedResource::parse(uri)?;
        if !self.resource_is_effective(&resource, client, cancel)? {
            return Err(ServerError::NotPublished);
        }
        Ok(AuthorizedResourceRead {
            resource,
            principal: client.principal.clone(),
            session_id: client.session_id,
            capability: Capability::FsRead,
        })
    }

    pub fn call_tool<E: PublishedExecutor + ?Sized>(
        &self,
        name: &str,
        arguments: Value,
        executor: &E,
        cancel: &CancellationToken,
    ) -> Result<ServerToolResult, ServerError> {
        let invocation = self.authorize_tool(name, arguments, cancel)?;
        cancel_check(cancel)?;
        executor.invoke_tool(&invocation, cancel)
    }

    pub fn read_resource<E: PublishedExecutor + ?Sized>(
        &self,
        uri: &str,
        executor: &E,
        cancel: &CancellationToken,
    ) -> Result<ServerResourceContents, ServerError> {
        let request = self.authorize_resource(uri, cancel)?;
        cancel_check(cancel)?;
        executor.read_resource(&request, cancel)
    }

    /// One JSON-RPC request or notification. Notifications yield `None`.
    pub fn handle_frame<E: PublishedExecutor + ?Sized>(
        &mut self,
        bytes: &[u8],
        executor: &E,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<u8>>, ServerError> {
        check_open(self.closed, cancel)?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(ServerError::FrameTooLarge);
        }
        let object = match parse_json_object(bytes) {
            Ok(object) => object,
            Err(err) => {
                return encode_error_response(Value::Null, PARSE_ERROR, err).map(Some);
            }
        };
        if object.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
            return encode_error_response(
                id_or_null(&object),
                INVALID_REQUEST,
                ServerError::InvalidFrame,
            )
            .map(Some);
        }
        let method = match object.get("method").and_then(Value::as_str) {
            Some(method) => method,
            None => {
                return encode_error_response(
                    id_or_null(&object),
                    INVALID_REQUEST,
                    ServerError::InvalidFrame,
                )
                .map(Some);
            }
        };
        let id = object.get("id").cloned();
        if method == INITIALIZED_METHOD {
            return Ok(None);
        }
        let Some(id) = id else {
            return Ok(None);
        };
        let params = object.get("params");
        let outcome = self.dispatch(method, params, executor, cancel);
        match outcome {
            Ok(result) => encode_result_response(id, result).map(Some),
            Err(err) => encode_error_response(id, rpc_code(err), err).map(Some),
        }
    }

    pub fn serve_frame<T, E>(
        &mut self,
        transport: &mut T,
        executor: &E,
        cancel: &CancellationToken,
    ) -> Result<(), ServerError>
    where
        T: McpTransport,
        E: PublishedExecutor + ?Sized,
    {
        let inbound = transport
            .recv_frame(cancel)
            .map_err(ServerError::Transport)?;
        if let Some(outbound) = self.handle_frame(&inbound, executor, cancel)? {
            transport
                .send_frame(&outbound, cancel)
                .map_err(ServerError::Transport)?;
        }
        Ok(())
    }

    pub fn close(&mut self, cancel: &CancellationToken) -> Result<(), ServerError> {
        cancel_check(cancel)?;
        self.closed = true;
        Ok(())
    }

    fn dispatch<E: PublishedExecutor + ?Sized>(
        &mut self,
        method: &str,
        params: Option<&Value>,
        executor: &E,
        cancel: &CancellationToken,
    ) -> Result<Value, ServerError> {
        cancel_check(cancel)?;
        match method {
            INITIALIZE_METHOD => self.handle_initialize(params, cancel),
            TOOLS_LIST => self.handle_tools_list(cancel),
            TOOLS_CALL => self.handle_tools_call(params, executor, cancel),
            RESOURCES_LIST => self.handle_resources_list(cancel),
            RESOURCES_READ => self.handle_resources_read(params, executor, cancel),
            PING_METHOD => Ok(Value::Object(Map::new())),
            _ => Err(ServerError::InvalidFrame),
        }
    }

    fn handle_initialize(
        &mut self,
        params: Option<&Value>,
        cancel: &CancellationToken,
    ) -> Result<Value, ServerError> {
        let params = params
            .and_then(Value::as_object)
            .ok_or(ServerError::InvalidArguments)?;
        let version = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or(ServerError::InvalidArguments)?;
        let protocol = ProtocolVersion::parse(version).map_err(|err| match err {
            TransportError::UnsupportedProtocolVersion => ServerError::UnsupportedProtocolVersion,
            _ => ServerError::InvalidArguments,
        })?;
        let client_info = parse_client_info(params.get("clientInfo"))?;
        // Client-advertised capabilities are never RapidLM grants.
        let _ignored_caps = params.get("capabilities");
        self.accept_client(client_info, protocol, cancel)?;
        let surface = self.published_surface(cancel)?;
        encode_initialize_result(protocol, &surface)
    }

    fn handle_tools_list(&self, cancel: &CancellationToken) -> Result<Value, ServerError> {
        let surface = self.published_surface(cancel)?;
        let mut tools = Vec::with_capacity(surface.tools.len());
        for tool in &surface.tools {
            tools.push(encode_tool_descriptor(*tool));
        }
        let mut result = Map::new();
        result.insert("tools".to_owned(), Value::Array(tools));
        Ok(Value::Object(result))
    }

    fn handle_tools_call<E: PublishedExecutor + ?Sized>(
        &self,
        params: Option<&Value>,
        executor: &E,
        cancel: &CancellationToken,
    ) -> Result<Value, ServerError> {
        let params = params
            .and_then(Value::as_object)
            .ok_or(ServerError::InvalidArguments)?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or(ServerError::InvalidArguments)?;
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(value) => value.clone(),
        };
        let result = self.call_tool(name, arguments, executor, cancel)?;
        Ok(result.result)
    }

    fn handle_resources_list(&self, cancel: &CancellationToken) -> Result<Value, ServerError> {
        let surface = self.published_surface(cancel)?;
        let mut resources = Vec::with_capacity(surface.resources.len());
        for resource in &surface.resources {
            let mut item = Map::new();
            item.insert("uri".to_owned(), Value::String(resource.uri()));
            item.insert(
                "name".to_owned(),
                Value::String(resource.path.as_str().to_owned()),
            );
            resources.push(Value::Object(item));
        }
        let mut result = Map::new();
        result.insert("resources".to_owned(), Value::Array(resources));
        Ok(Value::Object(result))
    }

    fn handle_resources_read<E: PublishedExecutor + ?Sized>(
        &self,
        params: Option<&Value>,
        executor: &E,
        cancel: &CancellationToken,
    ) -> Result<Value, ServerError> {
        let params = params
            .and_then(Value::as_object)
            .ok_or(ServerError::InvalidArguments)?;
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or(ServerError::InvalidArguments)?;
        let contents = self.read_resource(uri, executor, cancel)?;
        let mut item = Map::new();
        item.insert("uri".to_owned(), Value::String(contents.uri));
        item.insert("mimeType".to_owned(), Value::String(contents.mime_type));
        item.insert("text".to_owned(), Value::String(contents.text));
        let mut result = Map::new();
        result.insert(
            "contents".to_owned(),
            Value::Array(vec![Value::Object(item)]),
        );
        Ok(Value::Object(result))
    }

    fn tool_is_effective(
        &self,
        tool: RapidLmTool,
        client: &ServerClient,
        cancel: &CancellationToken,
    ) -> Result<bool, ServerError> {
        if !self.config.tools.contains(&tool) {
            return Ok(false);
        }
        if !self.config.allows_privileged(tool.privileged_class()) {
            return Ok(false);
        }
        self.capability_allowed(tool.capability(), client, cancel)
    }

    fn resource_is_effective(
        &self,
        resource: &PublishedResource,
        client: &ServerClient,
        cancel: &CancellationToken,
    ) -> Result<bool, ServerError> {
        if !self.config.resources.contains(resource) {
            return Ok(false);
        }
        self.capability_allowed(Capability::FsRead, client, cancel)
    }

    fn capability_allowed(
        &self,
        capability: Capability,
        client: &ServerClient,
        cancel: &CancellationToken,
    ) -> Result<bool, ServerError> {
        cancel_check(cancel)?;
        if !is_publishable_capability(capability) {
            return Ok(false);
        }
        let resource = representative_capability_resource(capability)?;
        let action = CanonicalAction::Resource {
            capability,
            resource: resource.clone(),
        };
        let request = ActionRequest::new(
            client.principal.clone(),
            client.session_id,
            capability,
            resource,
            action,
            "mcp.server",
        )
        .map_err(map_policy_eval)?;
        match evaluate(self.policy, &request, cancel) {
            Err(PolicyEvalError::Cancelled) => Err(ServerError::Cancelled),
            Err(_) => Ok(false),
            Ok(trace) => match trace.decision() {
                Decision::Allow(_) => Ok(true),
                Decision::Ask(_) | Decision::Deny(_) => Ok(false),
            },
        }
    }
}

impl ServerError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "MCP server cancelled",
            Self::InvalidIdent => "MCP server identifier is invalid",
            Self::InvalidArguments => "MCP server arguments are invalid",
            Self::ArgumentsTooLarge => "MCP server arguments exceed the configured bound",
            Self::InvalidFrame => "MCP server frame is not valid JSON-RPC",
            Self::FrameTooLarge => "MCP server frame exceeds the configured bound",
            Self::InvalidClient => "MCP client identity is invalid",
            Self::UnsupportedProtocolVersion => "MCP protocol version is not supported",
            Self::NotInitialized => "MCP server is not initialized",
            Self::AlreadyInitialized => "MCP server is already initialized",
            Self::Closed => "MCP server is closed",
            Self::NotPublished => "MCP tool or resource is not published",
            Self::HostOnly => "MCP host-only capability is not published",
            Self::HostCapabilityNotConfigured => {
                "MCP write/shell/browser capability is not configured"
            }
            Self::PolicyDenied => "MCP server request was denied by policy",
            Self::TooManyPublished => "MCP published set exceeds the configured bound",
            Self::ResultTooLarge => "MCP server result exceeds the configured bound",
            Self::ExecutorDenied => "MCP published executor denied the call",
            Self::Transport(_) => "MCP server transport failed",
        }
    }

    pub fn code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::Transport(inner) => inner.code(),
            Self::PolicyDenied
            | Self::NotPublished
            | Self::HostOnly
            | Self::HostCapabilityNotConfigured => Some(ErrorCode::PolicyDenied),
            Self::UnsupportedProtocolVersion | Self::InvalidClient => Some(ErrorCode::AuthRequired),
            Self::InvalidIdent
            | Self::InvalidArguments
            | Self::ArgumentsTooLarge
            | Self::InvalidFrame
            | Self::FrameTooLarge
            | Self::NotInitialized
            | Self::AlreadyInitialized
            | Self::Closed
            | Self::TooManyPublished
            | Self::ResultTooLarge
            | Self::ExecutorDenied => Some(ErrorCode::ToolInvalidArguments),
        }
    }

    pub fn into_api_error(self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        if let Self::Transport(inner) = self {
            return inner.into_api_error(trace_id);
        }
        Some(
            ApiError::new(code, self.as_str(), trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }
}

impl fmt::Display for RapidLmTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for PrivilegedClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Self::Transport(inner) = self {
            return inner.fmt(f);
        }
        f.write_str(self.as_str())
    }
}

impl Error for ServerError {}

fn unpublished_error(_tool: RapidLmTool) -> ServerError {
    ServerError::NotPublished
}

fn push_unique_capability(caps: &mut Vec<Capability>, capability: Capability) {
    if !caps.contains(&capability) {
        caps.push(capability);
    }
}

fn is_publishable_capability(capability: Capability) -> bool {
    matches!(
        capability,
        Capability::FsRead
            | Capability::FsWrite
            | Capability::ProcExec
            | Capability::BrowserNavigate
    )
}

fn looks_like_host_escape(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("secret")
        || lower.contains("host")
        || lower.contains("sudo")
        || lower.contains("lease")
        || lower.contains("plugin")
        || lower.contains("mobile")
}

fn parse_repo_resource(uri: &str) -> Result<RepoPath, ServerError> {
    if uri.is_empty() || uri.len() > crate::catalog::MAX_URI_BYTES {
        return Err(ServerError::InvalidIdent);
    }
    if uri.contains('\0') || uri.chars().any(char::is_control) {
        return Err(ServerError::InvalidIdent);
    }
    let lower = uri.to_ascii_lowercase();
    if lower.starts_with("file:")
        || lower.starts_with("host:")
        || lower.starts_with("unix:")
        || uri.starts_with('/')
        || uri.contains('\\')
    {
        return Err(ServerError::HostOnly);
    }
    let path = uri.strip_prefix("repo:").ok_or(ServerError::HostOnly)?;
    if path.starts_with('/') || path.contains('\\') {
        return Err(ServerError::HostOnly);
    }
    RepoPath::parse(path).map_err(|_| ServerError::InvalidIdent)
}

fn principal_for(info: &ImplementationInfo) -> Result<PrincipalRef, ServerError> {
    let raw = format!("{PRINCIPAL_PREFIX}{}", info.name());
    PrincipalRef::parse(&raw).map_err(|_| ServerError::InvalidClient)
}

fn representative_resource(tool: RapidLmTool) -> Result<ResourceDescriptor, ServerError> {
    representative_capability_resource(tool.capability())
}

fn representative_capability_resource(
    capability: Capability,
) -> Result<ResourceDescriptor, ServerError> {
    match capability {
        Capability::FsRead | Capability::FsWrite => Ok(ResourceDescriptor::Filesystem(
            FilesystemScope::repo("**").map_err(|_| ServerError::InvalidIdent)?,
        )),
        Capability::ProcExec => Ok(ResourceDescriptor::Process(
            ProcessScope::new("shell").map_err(|_| ServerError::InvalidIdent)?,
        )),
        Capability::BrowserNavigate => {
            let origin = Origin::new(NetworkScheme::Https, "invalid.invalid", 443)
                .map_err(|_| ServerError::InvalidIdent)?;
            Ok(ResourceDescriptor::Browser(BrowserScope::navigate(origin)))
        }
        _ => Err(ServerError::HostOnly),
    }
}

fn parse_client_info(value: Option<&Value>) -> Result<ImplementationInfo, ServerError> {
    let object = value
        .and_then(Value::as_object)
        .ok_or(ServerError::InvalidClient)?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or(ServerError::InvalidClient)?;
    let version = object.get("version").and_then(Value::as_str).unwrap_or("0");
    ImplementationInfo::new(name, version).map_err(|_| ServerError::InvalidClient)
}

fn validate_arguments(arguments: Value) -> Result<Value, ServerError> {
    let object = match arguments {
        Value::Null => return Ok(Value::Object(Map::new())),
        Value::Object(object) => object,
        _ => return Err(ServerError::InvalidArguments),
    };
    if object.len() > MAX_CALL_ARGUMENT_FIELDS {
        return Err(ServerError::ArgumentsTooLarge);
    }
    for key in object.keys() {
        if key.is_empty() || key.len() > crate::catalog::MAX_IDENT_BYTES || key.contains('\0') {
            return Err(ServerError::InvalidArguments);
        }
        if PRIVILEGE_ARGUMENT_KEYS.contains(&key.as_str()) {
            return Err(ServerError::HostOnly);
        }
    }
    let encoded = serde_json::to_vec(&Value::Object(object.clone()))
        .map_err(|_| ServerError::InvalidArguments)?;
    if encoded.len() > MAX_CALL_ARGUMENTS_BYTES {
        return Err(ServerError::ArgumentsTooLarge);
    }
    Ok(Value::Object(object))
}

fn encode_initialize_result(
    protocol: ProtocolVersion,
    surface: &PublishedSurface,
) -> Result<Value, ServerError> {
    let server = ImplementationInfo::rapidlm();
    let mut info = Map::new();
    info.insert("name".to_owned(), Value::String(server.name().to_owned()));
    info.insert(
        "version".to_owned(),
        Value::String(server.version().to_owned()),
    );
    let mut caps = Map::new();
    if !surface.tools.is_empty() {
        caps.insert("tools".to_owned(), Value::Object(Map::new()));
    }
    if !surface.resources.is_empty() {
        caps.insert("resources".to_owned(), Value::Object(Map::new()));
    }
    let mut result = Map::new();
    result.insert(
        "protocolVersion".to_owned(),
        Value::String(protocol.as_str().to_owned()),
    );
    result.insert("capabilities".to_owned(), Value::Object(caps));
    result.insert("serverInfo".to_owned(), Value::Object(info));
    Ok(Value::Object(result))
}

fn encode_tool_descriptor(tool: RapidLmTool) -> Value {
    let mut schema = Map::new();
    schema.insert("type".to_owned(), Value::String("object".to_owned()));
    let mut item = Map::new();
    item.insert("name".to_owned(), Value::String(tool.as_str().to_owned()));
    item.insert(
        "description".to_owned(),
        Value::String(tool.description().to_owned()),
    );
    item.insert("inputSchema".to_owned(), Value::Object(schema));
    Value::Object(item)
}

fn encode_result_response(id: Value, result: Value) -> Result<Vec<u8>, ServerError> {
    let mut body = Map::new();
    body.insert(
        "jsonrpc".to_owned(),
        Value::String(JSONRPC_VERSION.to_owned()),
    );
    body.insert("id".to_owned(), id);
    body.insert("result".to_owned(), result);
    encode_json_value(&Value::Object(body))
}

fn encode_error_response(id: Value, code: i64, err: ServerError) -> Result<Vec<u8>, ServerError> {
    let mut error = Map::new();
    error.insert("code".to_owned(), Value::from(code));
    error.insert("message".to_owned(), Value::String(err.as_str().to_owned()));
    let mut body = Map::new();
    body.insert(
        "jsonrpc".to_owned(),
        Value::String(JSONRPC_VERSION.to_owned()),
    );
    body.insert("id".to_owned(), id);
    body.insert("error".to_owned(), Value::Object(error));
    encode_json_value(&Value::Object(body))
}

fn encode_json_value(value: &Value) -> Result<Vec<u8>, ServerError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ServerError::InvalidFrame)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ServerError::FrameTooLarge);
    }
    Ok(bytes)
}

fn parse_json_object(bytes: &[u8]) -> Result<Map<String, Value>, ServerError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ServerError::FrameTooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ServerError::InvalidFrame)?;
    let value: Value = serde_json::from_str(text).map_err(|_| ServerError::InvalidFrame)?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(ServerError::InvalidFrame),
    }
}

fn id_or_null(object: &Map<String, Value>) -> Value {
    object.get("id").cloned().unwrap_or(Value::Null)
}

fn rpc_code(err: ServerError) -> i64 {
    match err {
        ServerError::Cancelled => REQUEST_CANCELLED,
        ServerError::InvalidFrame => METHOD_NOT_FOUND,
        ServerError::InvalidArguments
        | ServerError::InvalidIdent
        | ServerError::ArgumentsTooLarge
        | ServerError::InvalidClient
        | ServerError::UnsupportedProtocolVersion => INVALID_PARAMS,
        _ => APPLICATION_ERROR,
    }
}

fn parse_ident(value: &str, max: usize) -> Result<String, ServerError> {
    if value.is_empty() || value.len() > max {
        return Err(ServerError::InvalidIdent);
    }
    if value.contains('\0') || value.chars().any(char::is_control) {
        return Err(ServerError::InvalidIdent);
    }
    Ok(value.to_owned())
}

fn bound_text(value: &str, max: usize) -> (String, bool) {
    if value.len() <= max {
        return (value.to_owned(), false);
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

fn map_policy_eval(err: PolicyEvalError) -> ServerError {
    match err {
        PolicyEvalError::Cancelled => ServerError::Cancelled,
        _ => ServerError::PolicyDenied,
    }
}

fn cancel_check(cancel: &CancellationToken) -> Result<(), ServerError> {
    if cancel.is_cancelled() {
        Err(ServerError::Cancelled)
    } else {
        Ok(())
    }
}

fn check_open(closed: bool, cancel: &CancellationToken) -> Result<(), ServerError> {
    cancel_check(cancel)?;
    if closed {
        Err(ServerError::Closed)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_broker::{PolicyDocument, PolicySource};
    use serde_json::json;

    use crate::transport::{IoBounds, LoopbackTransport, TransportKind};

    const SECRET: &str = "password=super-secret-token";

    struct RecordingExecutor {
        calls: std::cell::RefCell<Vec<String>>,
        reads: std::cell::RefCell<Vec<String>>,
    }

    impl RecordingExecutor {
        fn new() -> Self {
            Self {
                calls: std::cell::RefCell::new(Vec::new()),
                reads: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl PublishedExecutor for RecordingExecutor {
        fn invoke_tool(
            &self,
            invocation: &AuthorizedInvocation,
            cancel: &CancellationToken,
        ) -> Result<ServerToolResult, ServerError> {
            cancel_check(cancel)?;
            self.calls
                .borrow_mut()
                .push(invocation.tool().as_str().to_owned());
            ServerToolResult::new(false, "ok")
        }

        fn read_resource(
            &self,
            request: &AuthorizedResourceRead,
            cancel: &CancellationToken,
        ) -> Result<ServerResourceContents, ServerError> {
            cancel_check(cancel)?;
            self.reads.borrow_mut().push(request.resource().uri());
            ServerResourceContents::new(request.resource(), "text/plain", "ok")
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn client_info(name: &str) -> ImplementationInfo {
        ImplementationInfo::new(name, "1.0.0").expect("client")
    }

    fn accept(server: &mut McpServer<'_>, name: &str) {
        server
            .accept_client(client_info(name), ProtocolVersion::TARGET, &live())
            .expect("accept");
    }

    fn allow_stack(subject: &str, capability: &str) -> PolicyStack {
        let src = format!(
            r#"
[[rules]]
id = "mcp-server-allow"
effect = "allow"
subjects = ["{subject}"]
capability = "{capability}"
"#
        );
        PolicyStack::new([PolicyDocument::parse_toml(
            &src,
            PolicySource::user("user-policy.toml").expect("source"),
            &live(),
        )
        .expect("parse")])
        .expect("stack")
    }

    fn allow_many(subject: &str, capabilities: &[&str]) -> PolicyStack {
        let mut src = String::new();
        for (idx, capability) in capabilities.iter().enumerate() {
            src.push_str(&format!(
                r#"
[[rules]]
id = "mcp-server-allow-{idx}"
effect = "allow"
subjects = ["{subject}"]
capability = "{capability}"
"#
            ));
        }
        PolicyStack::new([PolicyDocument::parse_toml(
            &src,
            PolicySource::user("user-policy.toml").expect("source"),
            &live(),
        )
        .expect("parse")])
        .expect("stack")
    }

    fn empty_stack() -> PolicyStack {
        PolicyStack::empty()
    }

    fn initialize_frame(name: &str, extra_caps: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": ProtocolVersion::TARGET.as_str(),
                "capabilities": extra_caps,
                "clientInfo": { "name": name, "version": "1.0.0" }
            }
        }))
        .expect("json")
    }

    fn request_frame(id: u64, method: &str, params: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .expect("json")
    }

    #[test]
    fn default_server_mode_exposes_no_write_shell_browser() {
        let policy = empty_stack();
        let mut server = McpServer::new(McpServerConfig::new(), &policy);
        accept(&mut server, "cursor");
        let surface = server.published_surface(&live()).expect("surface");
        assert!(surface.tools().is_empty());
        assert!(surface.resources().is_empty());
        assert!(surface.capabilities().is_empty());
        assert!(!surface.exposes_write());
        assert!(!surface.exposes_shell());
        assert!(!surface.exposes_browser());

        let executor = RecordingExecutor::new();
        for name in ["workspace.patch", "shell.exec", "browser.act"] {
            let err = server
                .call_tool(name, json!({}), &executor, &live())
                .expect_err("privileged");
            assert!(
                matches!(
                    err,
                    ServerError::NotPublished | ServerError::HostCapabilityNotConfigured
                ),
                "{name}: {err:?}"
            );
        }
        assert!(executor.calls().is_empty());
    }

    #[test]
    fn published_surface_is_config_and_policy_intersection() {
        let policy = allow_many("mcp-client/cursor", &["fs.read", "fs.write"]);
        let config = McpServerConfig::new()
            .with_tool("repo.read")
            .expect("read")
            .with_tool("workspace.patch")
            .expect("patch")
            .with_tool("shell.exec")
            .expect("shell")
            .with_capability(Capability::FsWrite)
            .expect("write cap");
        let mut server = McpServer::new(config, &policy);
        accept(&mut server, "cursor");
        let surface = server.published_surface(&live()).expect("surface");
        assert!(surface.has_tool(RapidLmTool::RepoRead));
        assert!(surface.has_tool(RapidLmTool::WorkspacePatch));
        assert!(!surface.has_tool(RapidLmTool::ShellExec));
        assert!(surface.exposes_write());
        assert!(!surface.exposes_shell());
        assert!(!surface.exposes_browser());
        assert!(surface.has_capability(Capability::FsRead));
        assert!(surface.has_capability(Capability::FsWrite));
        assert!(!surface.has_capability(Capability::ProcExec));
    }

    #[test]
    fn client_identity_is_policy_principal() {
        let policy = allow_stack("mcp-client/allowed-ide", "fs.read");
        let config = McpServerConfig::new()
            .with_tool("repo.read")
            .expect("read")
            .with_resource("repo:README.md")
            .expect("resource");
        let mut allowed = McpServer::new(config.clone(), &policy);
        accept(&mut allowed, "allowed-ide");
        let surface = allowed.published_surface(&live()).expect("allowed");
        assert!(surface.has_tool(RapidLmTool::RepoRead));
        assert_eq!(surface.resources().len(), 1);
        assert_eq!(
            allowed.client().expect("client").principal().as_str(),
            "mcp-client/allowed-ide"
        );

        let mut denied = McpServer::new(config, &policy);
        accept(&mut denied, "other-ide");
        let surface = denied.published_surface(&live()).expect("denied");
        assert!(surface.tools().is_empty());
        assert!(surface.resources().is_empty());
        let executor = RecordingExecutor::new();
        let err = denied
            .call_tool("repo.read", json!({}), &executor, &live())
            .expect_err("other client");
        assert_eq!(err, ServerError::NotPublished);
        assert!(executor.calls().is_empty());
    }

    #[test]
    fn client_handshake_cannot_grant_host_capabilities() {
        let policy = allow_many(
            "mcp-client/attacker",
            &["fs.write", "proc.exec", "browser.navigate"],
        );
        let mut server = McpServer::new(McpServerConfig::new(), &policy);
        let executor = RecordingExecutor::new();
        let frame = initialize_frame(
            "attacker",
            json!({
                "tools": { "write": true, "shell": true, "browser": true },
                "experimental": { "host": true, "grant_capability": "proc.exec" }
            }),
        );
        let response = server
            .handle_frame(&frame, &executor, &live())
            .expect("handle")
            .expect("response");
        let parsed: Value = serde_json::from_slice(&response).expect("json");
        assert!(parsed.get("error").is_none());
        let caps = parsed
            .pointer("/result/capabilities")
            .and_then(Value::as_object)
            .expect("caps");
        assert!(caps.get("tools").is_none());
        let surface = server.published_surface(&live()).expect("surface");
        assert!(!surface.exposes_write());
        assert!(!surface.exposes_shell());
        assert!(!surface.exposes_browser());
        for name in ["workspace.patch", "shell.exec", "browser.act"] {
            let err = server
                .call_tool(name, json!({SECRET: true}), &executor, &live())
                .expect_err("no grant");
            assert!(matches!(
                err,
                ServerError::NotPublished
                    | ServerError::HostCapabilityNotConfigured
                    | ServerError::HostOnly
                    | ServerError::InvalidIdent
            ));
        }
        assert!(executor.calls().is_empty());
    }

    #[test]
    fn host_only_names_and_uris_fail_closed() {
        assert_eq!(RapidLmTool::parse("secret.use"), Err(ServerError::HostOnly));
        assert_eq!(
            RapidLmTool::parse("agent.spawn"),
            Err(ServerError::HostOnly)
        );
        assert_eq!(
            McpServerConfig::new().with_capability(Capability::SecretUse),
            Err(ServerError::HostOnly)
        );
        assert_eq!(
            McpServerConfig::new().with_capability(Capability::PluginInvoke),
            Err(ServerError::HostOnly)
        );
        assert_eq!(
            PublishedResource::parse("file:///etc/passwd"),
            Err(ServerError::HostOnly)
        );
        assert_eq!(
            PublishedResource::parse("host:/tmp/secret"),
            Err(ServerError::HostOnly)
        );
        assert_eq!(
            PublishedResource::parse("/etc/passwd"),
            Err(ServerError::HostOnly)
        );
        let err = McpServerConfig::new()
            .with_resource(&format!("file://{SECRET}"))
            .expect_err("host uri");
        assert_eq!(err, ServerError::HostOnly);
        assert!(!format!("{err}").contains(SECRET));
    }

    #[test]
    fn privilege_argument_keys_do_not_reach_executor() {
        let policy = allow_stack("mcp-client/cursor", "fs.read");
        let config = McpServerConfig::new().with_tool("repo.read").expect("read");
        let mut server = McpServer::new(config, &policy);
        accept(&mut server, "cursor");
        let executor = RecordingExecutor::new();
        let err = server
            .call_tool(
                "repo.read",
                json!({"grant_capability": "proc.exec", "path": "src/lib.rs"}),
                &executor,
                &live(),
            )
            .expect_err("privilege key");
        assert_eq!(err, ServerError::HostOnly);
        assert!(executor.calls().is_empty());
    }

    #[test]
    fn authorized_published_tool_reaches_executor() {
        let policy = allow_stack("mcp-client/cursor", "fs.read");
        let config = McpServerConfig::new().with_tool("repo.read").expect("read");
        let mut server = McpServer::new(config, &policy);
        accept(&mut server, "cursor");
        let executor = RecordingExecutor::new();
        let result = server
            .call_tool(
                "repo.read",
                json!({"path": "src/lib.rs"}),
                &executor,
                &live(),
            )
            .expect("call");
        assert!(!result.is_error());
        assert_eq!(executor.calls(), vec!["repo.read".to_owned()]);
        let invocation = server
            .authorize_tool("repo.read", json!({"path": "src/lib.rs"}), &live())
            .expect("authz");
        assert_eq!(invocation.principal().as_str(), "mcp-client/cursor");
        assert_eq!(invocation.capability(), Capability::FsRead);
    }

    #[test]
    fn jsonrpc_lists_only_effective_tools() {
        let policy = allow_stack("mcp-client/cursor", "fs.read");
        let config = McpServerConfig::new()
            .with_tool("repo.search")
            .expect("search")
            .with_tool("shell.exec")
            .expect("shell");
        let mut server = McpServer::new(config, &policy);
        let executor = RecordingExecutor::new();
        let init = server
            .handle_frame(&initialize_frame("cursor", json!({})), &executor, &live())
            .expect("init")
            .expect("body");
        let init_val: Value = serde_json::from_slice(&init).expect("json");
        assert!(init_val.pointer("/result/capabilities/tools").is_some());

        let listed = server
            .handle_frame(&request_frame(2, TOOLS_LIST, json!({})), &executor, &live())
            .expect("list")
            .expect("body");
        let listed_val: Value = serde_json::from_slice(&listed).expect("json");
        let tools = listed_val
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].get("name").and_then(Value::as_str),
            Some("repo.search")
        );

        let denied = server
            .handle_frame(
                &request_frame(
                    3,
                    TOOLS_CALL,
                    json!({"name": "shell.exec", "arguments": {}}),
                ),
                &executor,
                &live(),
            )
            .expect("call")
            .expect("body");
        let denied_val: Value = serde_json::from_slice(&denied).expect("json");
        assert!(denied_val.get("error").is_some());
        assert!(executor.calls().is_empty());
    }

    #[test]
    fn cancelled_and_oversized_frames_fail_closed() {
        let policy = empty_stack();
        let mut server = McpServer::new(McpServerConfig::new(), &policy);
        accept(&mut server, "cursor");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            server.published_surface(&cancel).expect_err("cancel"),
            ServerError::Cancelled
        );

        let live = live();
        let mut server = McpServer::new(McpServerConfig::new(), &policy);
        let executor = RecordingExecutor::new();
        let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        assert_eq!(
            server
                .handle_frame(&oversized, &executor, &live)
                .expect_err("oversize"),
            ServerError::FrameTooLarge
        );
    }

    #[test]
    fn serve_frame_round_trip_does_not_leak_host_tools() {
        let policy = allow_stack("mcp-client/cursor", "fs.read");
        let config = McpServerConfig::new()
            .with_tool("workspace.status")
            .expect("status");
        let mut server = McpServer::new(config, &policy);
        let executor = RecordingExecutor::new();
        let mut transport = LoopbackTransport::new(TransportKind::Stdio, IoBounds::standard());
        transport
            .push_inbound(initialize_frame(
                "cursor",
                json!({"experimental":{"host":true}}),
            ))
            .expect("inbound");
        server
            .serve_frame(&mut transport, &executor, &live())
            .expect("serve");
        assert_eq!(transport.outbound().len(), 1);
        transport
            .push_inbound(request_frame(2, TOOLS_LIST, json!({})))
            .expect("list inbound");
        server
            .serve_frame(&mut transport, &executor, &live())
            .expect("list");
        let listed: Value = serde_json::from_slice(&transport.outbound()[1]).expect("json");
        let names: Vec<&str> = listed
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .expect("tools")
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(names, vec!["workspace.status"]);
        assert!(!names.contains(&"shell.exec"));
        assert!(!names.contains(&"browser.act"));
        assert!(!names.contains(&"workspace.patch"));
    }

    #[test]
    fn configured_write_still_requires_policy_allow() {
        let policy = empty_stack();
        let config = McpServerConfig::new()
            .with_tool("workspace.patch")
            .expect("patch")
            .with_capability(Capability::FsWrite)
            .expect("write");
        let mut server = McpServer::new(config, &policy);
        accept(&mut server, "cursor");
        let surface = server.published_surface(&live()).expect("surface");
        assert!(!surface.exposes_write());
        let executor = RecordingExecutor::new();
        let err = server
            .call_tool("workspace.patch", json!({}), &executor, &live())
            .expect_err("deny");
        assert_eq!(err, ServerError::NotPublished);
        assert!(executor.calls().is_empty());
    }

    #[test]
    fn error_display_does_not_echo_secrets() {
        let err = ServerError::HostOnly;
        let shown = err.to_string();
        assert!(!shown.contains(SECRET));
        assert!(!shown.contains("password"));
        assert!(shown.contains("host-only"));
    }
}
