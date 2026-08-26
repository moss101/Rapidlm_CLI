//! ACP v2 capability negotiation over the v1 adapter path.
//!
//! Handshake selects protocol 1 or 2 per official ACP version negotiation.
//! Capabilities are advertisements, not grants. Unknown or unsupported
//! features are omitted or ignored; they are never assumed enabled.

use std::error::Error;
use std::fmt;

use event_ledger::event::ActorRef;
use kernel::KernelClient;
use protocol::ProjectId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::stdio::{
    CancellationToken, INTERNAL_ERROR, INVALID_PARAMS, JsonRpcErrorObject, JsonRpcId,
    JsonRpcMessage, METHOD_NOT_FOUND,
};
use crate::v1::{
    HandleResult, InitializeResult, MAX_IMPLEMENTATION_NAME_BYTES,
    MAX_IMPLEMENTATION_VERSION_BYTES, METHOD_INITIALIZE, V1Adapter, V1Error,
};

/// Highest ACP major version this adapter will speak.
pub const PROTOCOL_VERSION: u32 = 2;
/// Oldest ACP major version this adapter will speak.
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// Maximum UTF-8 bytes accepted in `info.title`.
pub const MAX_IMPLEMENTATION_TITLE_BYTES: usize = 128;
/// Maximum keys accepted in one capability object.
pub const MAX_CAPABILITY_KEYS: usize = 32;
/// Maximum keys accepted in one `_meta` object.
pub const MAX_META_KEYS: usize = 16;
/// Maximum UTF-8 bytes accepted in one `_meta` key.
pub const MAX_META_KEY_BYTES: usize = 128;

const AGENT_NAME: &str = "rapidlm";
const AGENT_TITLE: &str = "RapidLM";
const AGENT_VERSION: &str = "0.1.0";

/// Negotiated ACP major version for one connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AcpVersion {
    V1,
    V2,
}

/// Frontend adapter that negotiates v1/v2 then delegates session work to v1.
pub struct V2Adapter<C> {
    inner: V1Adapter<C>,
    cancel: CancellationToken,
    negotiated: Option<NegotiatedHandshake>,
}

/// Recorded result of `initialize`. Client claims are not privilege grants.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegotiatedHandshake {
    protocol: AcpVersion,
    client_capabilities: ClientCapabilities,
    agent_capabilities: AgentCapabilities,
}

/// v2 client advertisements recorded at handshake.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClientCapabilities {
    elicitation_form: bool,
    elicitation_url: bool,
}

/// Agent advertisements for the selected protocol. Unsupported keys stay off.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AgentCapabilities {
    session: bool,
    prompt_image: bool,
    prompt_audio: bool,
    prompt_embedded_context: bool,
    mcp_stdio: bool,
    mcp_http: bool,
    session_delete: bool,
    additional_directories: bool,
    auth: bool,
}

/// Result of a successful handshake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InitializeOutcome {
    V1(InitializeResult),
    V2(V2InitializeResult),
}

/// Wire `initialize` result for a selected v2 connection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct V2InitializeResult {
    protocol_version: u32,
    capabilities: AgentCapabilitiesWire,
    info: ImplementationInfo,
    auth_methods: Vec<Value>,
}

/// Typed v2 negotiation failure. Display never echoes handshake payloads.
#[derive(Debug)]
pub enum V2Error {
    Cancelled,
    NotInitialized,
    AlreadyInitialized,
    InvalidParams,
    MethodNotFound,
    Adapter(V1Error),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ImplementationInfo {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct AgentCapabilitiesWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<SessionCapabilitiesWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth: Option<SupportMarker>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionCapabilitiesWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<PromptCapabilitiesWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp: Option<McpCapabilitiesWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delete: Option<SupportMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_directories: Option<SupportMarker>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptCapabilitiesWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<SupportMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio: Option<SupportMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedded_context: Option<SupportMarker>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct McpCapabilitiesWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    stdio: Option<SupportMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http: Option<SupportMarker>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
struct SupportMarker {}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionOnly {
    protocol_version: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct V1InitializeParams {
    protocol_version: u32,
    #[serde(default)]
    client_info: Option<ImplementationInfo>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct V2InitializeParams {
    protocol_version: u32,
    info: ImplementationInfo,
    #[serde(default)]
    capabilities: Option<Value>,
}

impl AcpVersion {
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::V1 => MIN_PROTOCOL_VERSION,
            Self::V2 => PROTOCOL_VERSION,
        }
    }
}

impl<C: KernelClient> V2Adapter<C> {
    pub fn new(
        client: C,
        project_id: ProjectId,
        actor: ActorRef,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            inner: V1Adapter::new(client, project_id, actor, cancel.clone()),
            cancel,
            negotiated: None,
        }
    }

    pub fn inner(&self) -> &V1Adapter<C> {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut V1Adapter<C> {
        &mut self.inner
    }

    pub fn handshake(&self) -> Option<&NegotiatedHandshake> {
        self.negotiated.as_ref()
    }

    /// Dispatch one inbound frame. `initialize` is negotiated here; the rest
    /// stays on the v1 compatibility path.
    pub async fn handle(&mut self, message: &JsonRpcMessage) -> Result<HandleResult, V2Error> {
        self.check_cancel()?;
        match message {
            JsonRpcMessage::Request { id, method, params } if method == METHOD_INITIALIZE => {
                let params = params.clone().unwrap_or_else(|| Value::Object(Map::new()));
                match self.initialize(params).await {
                    Ok(outcome) => Ok(HandleResult::Reply(JsonRpcMessage::Result {
                        id: id.clone(),
                        result: outcome.to_value()?,
                    })),
                    Err(V2Error::Cancelled) => Err(V2Error::Cancelled),
                    Err(err) => Ok(HandleResult::Reply(jsonrpc_error(id.clone(), &err))),
                }
            }
            other => match self.inner.handle(other).await {
                Ok(result) => Ok(result),
                Err(V1Error::Cancelled) => Err(V2Error::Cancelled),
                Err(err) => Err(V2Error::from(err)),
            },
        }
    }

    pub async fn initialize(&mut self, params: Value) -> Result<InitializeOutcome, V2Error> {
        self.check_cancel()?;
        if self.negotiated.is_some() {
            return Err(V2Error::AlreadyInitialized);
        }
        let handshake = negotiate(&params)?;
        let v1_result = self.inner.initialize(params).await.map_err(V2Error::from)?;
        let outcome = match handshake.protocol {
            AcpVersion::V1 => InitializeOutcome::V1(v1_result),
            AcpVersion::V2 => InitializeOutcome::V2(v2_initialize_result(&handshake)),
        };
        self.negotiated = Some(handshake);
        Ok(outcome)
    }

    fn check_cancel(&self) -> Result<(), V2Error> {
        if self.cancel.is_cancelled() {
            Err(V2Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl NegotiatedHandshake {
    pub fn protocol(&self) -> AcpVersion {
        self.protocol
    }

    pub fn client_capabilities(&self) -> ClientCapabilities {
        self.client_capabilities
    }

    pub fn agent_capabilities(&self) -> AgentCapabilities {
        self.agent_capabilities
    }
}

impl ClientCapabilities {
    pub fn elicitation_form(&self) -> bool {
        self.elicitation_form
    }

    pub fn elicitation_url(&self) -> bool {
        self.elicitation_url
    }
}

impl AgentCapabilities {
    pub fn session(&self) -> bool {
        self.session
    }

    pub fn prompt_image(&self) -> bool {
        self.prompt_image
    }

    pub fn prompt_audio(&self) -> bool {
        self.prompt_audio
    }

    pub fn prompt_embedded_context(&self) -> bool {
        self.prompt_embedded_context
    }

    pub fn mcp_stdio(&self) -> bool {
        self.mcp_stdio
    }

    pub fn mcp_http(&self) -> bool {
        self.mcp_http
    }

    pub fn session_delete(&self) -> bool {
        self.session_delete
    }

    pub fn additional_directories(&self) -> bool {
        self.additional_directories
    }

    pub fn auth(&self) -> bool {
        self.auth
    }

    fn advertised_v2() -> Self {
        Self {
            session: true,
            ..Self::default()
        }
    }
}

impl InitializeOutcome {
    pub fn protocol_version(&self) -> u32 {
        match self {
            Self::V1(result) => result.protocol_version(),
            Self::V2(result) => result.protocol_version(),
        }
    }

    pub fn to_value(&self) -> Result<Value, V2Error> {
        match self {
            Self::V1(result) => serde_json::to_value(result).map_err(|_| V2Error::InvalidParams),
            Self::V2(result) => serde_json::to_value(result).map_err(|_| V2Error::InvalidParams),
        }
    }
}

impl V2InitializeResult {
    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    pub fn session(&self) -> bool {
        self.capabilities.session.is_some()
    }
}

impl V2Error {
    pub fn jsonrpc_code(&self) -> i64 {
        match self {
            Self::MethodNotFound => METHOD_NOT_FOUND,
            Self::InvalidParams | Self::NotInitialized | Self::AlreadyInitialized => INVALID_PARAMS,
            Self::Cancelled => INTERNAL_ERROR,
            Self::Adapter(err) => err.jsonrpc_code(),
        }
    }

    fn message(&self) -> &'static str {
        match self {
            Self::Cancelled => "acp adapter cancelled",
            Self::NotInitialized => "acp adapter is not initialized",
            Self::AlreadyInitialized => "acp adapter already initialized",
            Self::InvalidParams => "Invalid params",
            Self::MethodNotFound => "Method not found",
            Self::Adapter(err) => match err {
                V1Error::Cancelled => "acp adapter cancelled",
                V1Error::NotInitialized => "acp adapter is not initialized",
                V1Error::AlreadyInitialized => "acp adapter already initialized",
                V1Error::MethodNotFound => "Method not found",
                V1Error::InvalidParams => "Invalid params",
                V1Error::UnsupportedContent => "prompt content type is not advertised",
                V1Error::PromptTooLarge => "prompt exceeds the configured bound",
                V1Error::TooManySessions => "acp session binding limit reached",
                V1Error::SessionClosed => "session is closed",
                V1Error::UnknownSession => "session not found",
                V1Error::Kernel(_) => "kernel request failed",
                V1Error::Stream(_) => "session event stream failed",
            },
        }
    }
}

impl fmt::Display for V2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl Error for V2Error {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Adapter(err) => Some(err),
            _ => None,
        }
    }
}

impl From<V1Error> for V2Error {
    fn from(err: V1Error) -> Self {
        match err {
            V1Error::Cancelled => Self::Cancelled,
            V1Error::NotInitialized => Self::NotInitialized,
            V1Error::AlreadyInitialized => Self::AlreadyInitialized,
            V1Error::InvalidParams => Self::InvalidParams,
            V1Error::MethodNotFound => Self::MethodNotFound,
            other => Self::Adapter(other),
        }
    }
}

/// Select protocol version and record advertised capabilities.
///
/// Unknown optional capabilities are ignored. Known fields with the wrong
/// shape are rejected. Nothing omitted or unknown is treated as enabled.
pub fn negotiate(params: &Value) -> Result<NegotiatedHandshake, V2Error> {
    let parsed: VersionOnly = parse_params(params.clone())?;
    let protocol = select_version(parsed.protocol_version)?;
    match protocol {
        AcpVersion::V1 => negotiate_v1(params),
        AcpVersion::V2 => negotiate_v2(params),
    }
}

fn select_version(requested: u32) -> Result<AcpVersion, V2Error> {
    match requested {
        0 => Err(V2Error::InvalidParams),
        1 => Ok(AcpVersion::V1),
        2.. => Ok(AcpVersion::V2),
    }
}

fn negotiate_v1(params: &Value) -> Result<NegotiatedHandshake, V2Error> {
    let parsed: V1InitializeParams = parse_params(params.clone())?;
    if parsed.protocol_version != MIN_PROTOCOL_VERSION {
        return Err(V2Error::InvalidParams);
    }
    if let Some(info) = parsed.client_info.as_ref() {
        validate_implementation(info)?;
    }
    Ok(NegotiatedHandshake {
        protocol: AcpVersion::V1,
        client_capabilities: ClientCapabilities::default(),
        agent_capabilities: AgentCapabilities::default(),
    })
}

fn negotiate_v2(params: &Value) -> Result<NegotiatedHandshake, V2Error> {
    let parsed: V2InitializeParams = parse_params(params.clone())?;
    if parsed.protocol_version < PROTOCOL_VERSION {
        return Err(V2Error::InvalidParams);
    }
    validate_implementation(&parsed.info)?;
    if let Some(meta) = params.get("_meta") {
        validate_meta(meta)?;
    }
    let client_capabilities = parse_client_capabilities(parsed.capabilities.as_ref())?;
    Ok(NegotiatedHandshake {
        protocol: AcpVersion::V2,
        client_capabilities,
        agent_capabilities: AgentCapabilities::advertised_v2(),
    })
}

fn v2_initialize_result(handshake: &NegotiatedHandshake) -> V2InitializeResult {
    V2InitializeResult {
        protocol_version: handshake.protocol.as_u32(),
        capabilities: agent_capabilities_wire(handshake.agent_capabilities),
        info: ImplementationInfo {
            name: AGENT_NAME.to_owned(),
            title: Some(AGENT_TITLE.to_owned()),
            version: AGENT_VERSION.to_owned(),
        },
        auth_methods: Vec::new(),
    }
}

fn agent_capabilities_wire(caps: AgentCapabilities) -> AgentCapabilitiesWire {
    let session = if caps.session {
        Some(SessionCapabilitiesWire {
            prompt: prompt_wire(caps),
            mcp: mcp_wire(caps),
            delete: marker(caps.session_delete),
            additional_directories: marker(caps.additional_directories),
        })
    } else {
        None
    };
    AgentCapabilitiesWire {
        session,
        auth: marker(caps.auth),
    }
}

fn prompt_wire(caps: AgentCapabilities) -> Option<PromptCapabilitiesWire> {
    let prompt = PromptCapabilitiesWire {
        image: marker(caps.prompt_image),
        audio: marker(caps.prompt_audio),
        embedded_context: marker(caps.prompt_embedded_context),
    };
    if prompt == PromptCapabilitiesWire::default() {
        None
    } else {
        Some(prompt)
    }
}

fn mcp_wire(caps: AgentCapabilities) -> Option<McpCapabilitiesWire> {
    let mcp = McpCapabilitiesWire {
        stdio: marker(caps.mcp_stdio),
        http: marker(caps.mcp_http),
    };
    if mcp == McpCapabilitiesWire::default() {
        None
    } else {
        Some(mcp)
    }
}

fn marker(supported: bool) -> Option<SupportMarker> {
    if supported {
        Some(SupportMarker {})
    } else {
        None
    }
}

fn parse_client_capabilities(value: Option<&Value>) -> Result<ClientCapabilities, V2Error> {
    let Some(value) = value else {
        return Ok(ClientCapabilities::default());
    };
    if value.is_null() {
        return Ok(ClientCapabilities::default());
    }
    let object = value.as_object().ok_or(V2Error::InvalidParams)?;
    if object.len() > MAX_CAPABILITY_KEYS {
        return Err(V2Error::InvalidParams);
    }
    let mut caps = ClientCapabilities::default();
    for (key, entry) in object {
        match key.as_str() {
            "elicitation" => {
                let elicitation = parse_elicitation(entry)?;
                caps.elicitation_form = elicitation.0;
                caps.elicitation_url = elicitation.1;
            }
            "_meta" => validate_meta(entry)?,
            _ => {
                // Unknown optional capability: ignore, never assume enabled.
            }
        }
    }
    Ok(caps)
}

fn parse_elicitation(value: &Value) -> Result<(bool, bool), V2Error> {
    if value.is_null() {
        return Ok((false, false));
    }
    let object = value.as_object().ok_or(V2Error::InvalidParams)?;
    if object.len() > MAX_CAPABILITY_KEYS {
        return Err(V2Error::InvalidParams);
    }
    let mut form = false;
    let mut url = false;
    for (key, entry) in object {
        match key.as_str() {
            "form" => form = parse_support_marker(entry)?,
            "url" => url = parse_support_marker(entry)?,
            "_meta" => validate_meta(entry)?,
            _ => {}
        }
    }
    Ok((form, url))
}

fn parse_support_marker(value: &Value) -> Result<bool, V2Error> {
    match value {
        Value::Null => Ok(false),
        Value::Object(object) => {
            if object.len() > MAX_CAPABILITY_KEYS {
                return Err(V2Error::InvalidParams);
            }
            if let Some(meta) = object.get("_meta") {
                validate_meta(meta)?;
            }
            Ok(true)
        }
        _ => Err(V2Error::InvalidParams),
    }
}

fn validate_implementation(info: &ImplementationInfo) -> Result<(), V2Error> {
    if info.name.is_empty() || info.name.len() > MAX_IMPLEMENTATION_NAME_BYTES {
        return Err(V2Error::InvalidParams);
    }
    if info.version.is_empty() || info.version.len() > MAX_IMPLEMENTATION_VERSION_BYTES {
        return Err(V2Error::InvalidParams);
    }
    if let Some(title) = info.title.as_deref() {
        if title.is_empty() || title.len() > MAX_IMPLEMENTATION_TITLE_BYTES {
            return Err(V2Error::InvalidParams);
        }
    }
    Ok(())
}

fn validate_meta(value: &Value) -> Result<(), V2Error> {
    if value.is_null() {
        return Ok(());
    }
    let object = value.as_object().ok_or(V2Error::InvalidParams)?;
    if object.len() > MAX_META_KEYS {
        return Err(V2Error::InvalidParams);
    }
    for key in object.keys() {
        if key.is_empty() || key.len() > MAX_META_KEY_BYTES {
            return Err(V2Error::InvalidParams);
        }
    }
    Ok(())
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, V2Error> {
    serde_json::from_value(params).map_err(|_| V2Error::InvalidParams)
}

fn jsonrpc_error(id: JsonRpcId, err: &V2Error) -> JsonRpcMessage {
    JsonRpcMessage::Error {
        id,
        error: JsonRpcErrorObject::new(err.jsonrpc_code(), err.message()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_ledger::event::ActorKind;
    use kernel::InProcessKernelClient;
    use protocol::{EventId, ProjectId};
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    const GOLDEN_V1_INITIALIZE: &str = concat!(
        r#"{"protocolVersion":1,"agentCapabilities":{"#,
        r#""loadSession":true,"promptCapabilities":{"#,
        r#""image":false,"audio":false,"embeddedContext":false},"#,
        r#""mcpCapabilities":{"http":false,"sse":false}},"#,
        r#""agentInfo":{"name":"rapidlm","version":"0.1.0"},"authMethods":[]}"#
    );

    const GOLDEN_V2_INITIALIZE: &str = concat!(
        r#"{"protocolVersion":2,"capabilities":{"session":{}},"#,
        r#""info":{"name":"rapidlm","title":"RapidLM","version":"0.1.0"},"#,
        r#""authMethods":[]}"#
    );

    struct TempClient {
        path: PathBuf,
        client: InProcessKernelClient,
    }

    impl TempClient {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-acp-v2-{}-{seq}.sqlite",
                std::process::id()
            ));
            remove_db_files(&path);
            let client = InProcessKernelClient::open(&path).expect("open client");
            Self { path, client }
        }
    }

    impl Drop for TempClient {
        fn drop(&mut self) {
            remove_db_files(&self.path);
        }
    }

    fn remove_db_files(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(sidecar(path, "-wal"));
        let _ = std::fs::remove_file(sidecar(path, "-shm"));
        let _ = std::fs::remove_file(sidecar(path, "-journal"));
    }

    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut raw = path.as_os_str().to_os_string();
        raw.push(suffix);
        PathBuf::from(raw)
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(&waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("in-process kernel future stayed pending"),
        }
    }

    fn actor() -> ActorRef {
        ActorRef::new(ActorKind::ExternalHuman, &EventId::new().to_string()).expect("actor")
    }

    fn adapter(client: InProcessKernelClient) -> V2Adapter<InProcessKernelClient> {
        V2Adapter::new(client, ProjectId::new(), actor(), CancellationToken::new())
    }

    #[test]
    fn negotiate_selects_v1_and_v2() {
        let v1 = negotiate(&serde_json::json!({"protocolVersion": 1})).expect("v1");
        assert_eq!(v1.protocol(), AcpVersion::V1);
        assert!(!v1.agent_capabilities().session());

        let v2 = negotiate(&serde_json::json!({
            "protocolVersion": 2,
            "info": {"name": "editor", "version": "1.0.0"}
        }))
        .expect("v2");
        assert_eq!(v2.protocol(), AcpVersion::V2);
        assert!(v2.agent_capabilities().session());
        assert!(!v2.agent_capabilities().prompt_image());
        assert!(!v2.agent_capabilities().mcp_http());
        assert!(!v2.agent_capabilities().auth());
    }

    #[test]
    fn higher_client_version_negotiates_down_to_v2() {
        let handshake = negotiate(&serde_json::json!({
            "protocolVersion": 3,
            "info": {"name": "editor", "title": "Editor", "version": "2.0.0"},
            "capabilities": {}
        }))
        .expect("down");
        assert_eq!(handshake.protocol(), AcpVersion::V2);
        assert_eq!(handshake.protocol().as_u32(), PROTOCOL_VERSION);
    }

    #[test]
    fn version_zero_is_rejected() {
        let err = negotiate(&serde_json::json!({"protocolVersion": 0})).expect_err("zero");
        assert!(matches!(err, V2Error::InvalidParams));
    }

    #[test]
    fn v2_requires_info() {
        let err = negotiate(&serde_json::json!({"protocolVersion": 2})).expect_err("info");
        assert!(matches!(err, V2Error::InvalidParams));
    }

    #[test]
    fn unknown_v2_capability_is_ignored_not_assumed() {
        let handshake = negotiate(&serde_json::json!({
            "protocolVersion": 2,
            "info": {"name": "editor", "version": "1.0.0"},
            "capabilities": {
                "fs": {"readTextFile": true, "writeTextFile": true},
                "terminal": true,
                "sudo": {},
                "elicitation": {
                    "form": {},
                    "unknownMode": {},
                    "_custom": {}
                },
                "_zed.dev": {"workspace": true},
                "_meta": {"zed.dev": {"workspace": true}}
            }
        }))
        .expect("ignore unknown");
        assert!(handshake.client_capabilities().elicitation_form());
        assert!(!handshake.client_capabilities().elicitation_url());
        assert!(!handshake.agent_capabilities().prompt_image());
        assert!(!handshake.agent_capabilities().session_delete());
        assert!(!handshake.agent_capabilities().additional_directories());
        assert!(!handshake.agent_capabilities().auth());
    }

    #[test]
    fn known_capability_wrong_shape_is_rejected() {
        let err = negotiate(&serde_json::json!({
            "protocolVersion": 2,
            "info": {"name": "editor", "version": "1.0.0"},
            "capabilities": {"elicitation": true}
        }))
        .expect_err("boolean marker");
        assert!(matches!(err, V2Error::InvalidParams));

        let empty = negotiate(&serde_json::json!({
            "protocolVersion": 2,
            "info": {"name": "editor", "version": "1.0.0"},
            "capabilities": {"elicitation": {}}
        }))
        .expect("empty elicitation");
        assert!(!empty.client_capabilities().elicitation_form());
        assert!(!empty.client_capabilities().elicitation_url());
    }

    #[test]
    fn v1_initialize_fixture_unchanged() {
        let tmp = TempClient::create();
        let mut acp = adapter(tmp.client.clone());
        let outcome = block_on(acp.initialize(serde_json::json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": true}, "terminal": true},
            "clientInfo": {"name": "editor", "version": "1.0.0"}
        })))
        .expect("initialize");
        assert_eq!(outcome.protocol_version(), MIN_PROTOCOL_VERSION);
        let json = match &outcome {
            InitializeOutcome::V1(result) => serde_json::to_string(result).expect("ser"),
            other => panic!("expected v1, got {other:?}"),
        };
        assert_eq!(json, GOLDEN_V1_INITIALIZE);
        assert!(tmp.path.exists());
    }

    #[test]
    fn v2_handshake_omits_unsupported_extensions() {
        let tmp = TempClient::create();
        let mut acp = adapter(tmp.client.clone());
        let outcome = block_on(acp.initialize(serde_json::json!({
            "protocolVersion": 2,
            "info": {"name": "editor", "version": "1.0.0"},
            "capabilities": {
                "fs": {"readTextFile": true},
                "elicitation": {"form": {}, "url": {}}
            }
        })))
        .expect("initialize");
        assert_eq!(outcome.protocol_version(), PROTOCOL_VERSION);
        let json = match &outcome {
            InitializeOutcome::V2(result) => serde_json::to_string(result).expect("ser"),
            other => panic!("expected v2, got {other:?}"),
        };
        assert_eq!(json, GOLDEN_V2_INITIALIZE);
        let handshake = acp.handshake().expect("recorded");
        assert!(handshake.client_capabilities().elicitation_form());
        assert!(handshake.client_capabilities().elicitation_url());
        match outcome {
            InitializeOutcome::V2(result) => assert!(result.session()),
            other => panic!("expected v2, got {other:?}"),
        }
        let again = block_on(acp.initialize(serde_json::json!({
            "protocolVersion": 2,
            "info": {"name": "editor", "version": "1.0.0"}
        })));
        assert!(matches!(again, Err(V2Error::AlreadyInitialized)));
    }

    #[test]
    fn unknown_method_after_v2_handshake_keeps_session_valid() {
        let tmp = TempClient::create();
        let mut acp = adapter(tmp.client.clone());
        block_on(acp.initialize(serde_json::json!({
            "protocolVersion": 2,
            "info": {"name": "editor", "version": "1.0.0"}
        })))
        .expect("init");
        let created = block_on(acp.inner_mut().session_new(serde_json::json!({
            "cwd": "/tmp/project",
            "mcpServers": []
        })))
        .expect("session/new");
        let result = block_on(acp.handle(&JsonRpcMessage::Request {
            id: JsonRpcId::Number(4),
            method: "session/set_mode".into(),
            params: Some(serde_json::json!({"sessionId": created.session_id()})),
        }))
        .expect("handle");
        match result {
            HandleResult::Reply(JsonRpcMessage::Error { error, .. }) => {
                assert_eq!(error.code(), METHOD_NOT_FOUND);
            }
            other => panic!("expected method-not-found, got {other:?}"),
        }
        block_on(tmp.client.get_session(created.session_id())).expect("still valid");
        assert_eq!(
            acp.inner().cursor(created.session_id()).expect("bound"),
            created.seq()
        );
    }

    #[test]
    fn cancelled_adapter_does_not_negotiate() {
        let tmp = TempClient::create();
        let cancel = CancellationToken::new();
        let mut acp = V2Adapter::new(
            tmp.client.clone(),
            ProjectId::new(),
            actor(),
            cancel.clone(),
        );
        cancel.cancel();
        let err = block_on(acp.initialize(serde_json::json!({
            "protocolVersion": 2,
            "info": {"name": "editor", "version": "1.0.0"}
        })));
        assert!(matches!(err, Err(V2Error::Cancelled)));
        assert!(acp.handshake().is_none());
    }

    #[test]
    fn handle_initialize_returns_v2_result() {
        let tmp = TempClient::create();
        let mut acp = adapter(tmp.client.clone());
        let result = block_on(acp.handle(&JsonRpcMessage::Request {
            id: JsonRpcId::Number(1),
            method: METHOD_INITIALIZE.into(),
            params: Some(serde_json::json!({
                "protocolVersion": 2,
                "info": {"name": "editor", "version": "1.0.0"}
            })),
        }))
        .expect("handle");
        match result {
            HandleResult::Reply(JsonRpcMessage::Result { result, .. }) => {
                assert_eq!(
                    result,
                    serde_json::from_str::<Value>(GOLDEN_V2_INITIALIZE).expect("golden")
                );
            }
            other => panic!("expected result, got {other:?}"),
        }
    }
}
