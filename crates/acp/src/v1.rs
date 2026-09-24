//! ACP v1 adapter: session/prompt/tool-update mapping onto `KernelClient`.
//!
//! This module translates Agent Client Protocol v1 JSON-RPC methods into
//! kernel session/turn/approval calls and maps committed ledger events back
//! to `session/update` and `session/request_permission`. It stores only
//! session-id/cursor bindings. Permissions always resolve through
//! [`kernel::KernelClient::approve`].

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use event_ledger::event::{ActorRef, ErasedEventEnvelope, EventKind};
use kernel::{
    ApprovalDecision, CreateSession, EventStreamError, Interrupt, InterruptReason, KernelClient,
    ResolveApproval, SessionSnapshot, SessionStatus, SubmitTurn, SubscribeEvents, TurnHandle,
};
use protocol::{ApiError, ErrorCode, ProjectId, SessionId, TraceId, TurnId};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::stdio::{
    CancellationToken, INTERNAL_ERROR, INVALID_PARAMS, JsonRpcErrorObject, JsonRpcId,
    JsonRpcMessage, METHOD_NOT_FOUND,
};

/// ACP protocol version spoken by this adapter.
pub const PROTOCOL_VERSION: u32 = 1;

/// JSON-RPC method: handshake.
pub const METHOD_INITIALIZE: &str = "initialize";
/// JSON-RPC method: create a kernel session.
pub const METHOD_SESSION_NEW: &str = "session/new";
/// JSON-RPC method: attach to an existing kernel session.
pub const METHOD_SESSION_LOAD: &str = "session/load";
/// JSON-RPC method: start a kernel turn.
pub const METHOD_SESSION_PROMPT: &str = "session/prompt";
/// JSON-RPC notification: interrupt the active kernel turn.
pub const METHOD_SESSION_CANCEL: &str = "session/cancel";
/// JSON-RPC notification: mapped kernel event (agent → client).
pub const METHOD_SESSION_UPDATE: &str = "session/update";
/// JSON-RPC request: mapped approval (agent → client).
pub const METHOD_SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
/// JSON-RPC request: switch the session's mode for subsequent prompts.
/// Only served when a [`SessionModeControl`] is installed; otherwise the
/// method stays `METHOD_NOT_FOUND` — a capability is advertised only when
/// it is real.
pub const METHOD_SESSION_SET_MODE: &str = "session/set_mode";

/// What the composition root implements when the agent genuinely supports
/// switching modes between prompts. Advertised through `session/new` /
/// `session/load` (`modes` + `currentMode`) and acted on by
/// `session/set_mode`. The switch takes effect on the NEXT prompt: ACP
/// prompts on one session are sequential, so there is no mid-turn mode.
pub trait SessionModeControl: Send + Sync {
    /// Advertised modes as `(id, display name)`, in display order.
    fn modes(&self) -> Vec<(String, String)>;
    /// Id of the mode the next prompt will run under.
    fn current_mode(&self) -> String;
    /// Switch modes for subsequent prompts. `Err(reason)` when the id is
    /// unknown or the switch is refused.
    fn set_mode(&self, id: &str) -> Result<(), String>;
}

/// Maximum UTF-8 bytes accepted in `cwd`.
pub const MAX_CWD_BYTES: usize = 4096;
/// Maximum MCP server descriptions accepted (they are not connected).
pub const MAX_MCP_SERVERS: usize = 32;
/// Maximum content blocks in one `session/prompt`.
pub const MAX_PROMPT_BLOCKS: usize = 64;
/// Maximum UTF-8 bytes across all prompt text blocks.
pub const MAX_PROMPT_TEXT_BYTES: usize = 64 * 1024;
/// Maximum session-id/cursor bindings retained by one adapter.
pub const MAX_SESSION_BINDINGS: usize = 1024;
/// Maximum events drained in one subscribe poll.
pub const MAX_DRAIN_EVENTS: usize = 256;
/// Maximum UTF-8 bytes copied into a tool title.
pub const MAX_TOOL_TITLE_BYTES: usize = 256;
/// Maximum UTF-8 bytes accepted in a tool-call id.
pub const MAX_TOOL_CALL_ID_BYTES: usize = 128;
/// Maximum UTF-8 bytes copied into an agent message chunk.
pub const MAX_UPDATE_TEXT_BYTES: usize = 8 * 1024;

/// Upper bound for a unified diff body mapped onto an edit tool-call update.
pub const MAX_DIFF_BYTES: usize = 64 * 1024;
/// Upper bound for the repo path carried alongside a diff.
pub const MAX_DIFF_PATH_BYTES: usize = 1024;
/// Maximum UTF-8 bytes accepted in client/agent implementation name.
pub const MAX_IMPLEMENTATION_NAME_BYTES: usize = 128;
/// Maximum UTF-8 bytes accepted in client/agent implementation version.
pub const MAX_IMPLEMENTATION_VERSION_BYTES: usize = 64;

const AGENT_NAME: &str = "rapidlm";
const AGENT_VERSION: &str = "0.1.0";
const CANCEL_STRIDE: usize = 32;

const OPTION_ALLOW_ONCE: &str = "allow-once";
const OPTION_ALLOW_ALWAYS: &str = "allow-always";
const OPTION_REJECT_ONCE: &str = "reject-once";
const OPTION_REJECT_ALWAYS: &str = "reject-always";

/// Frontend adapter over [`KernelClient`]. Bindings are IDs and cursors only.
pub struct V1Adapter<C> {
    client: C,
    project_id: ProjectId,
    actor: ActorRef,
    cancel: CancellationToken,
    initialized: bool,
    cursors: HashMap<SessionId, u64>,
    /// Mode control installed by the composition root. `None` (default):
    /// `session/set_mode` stays METHOD_NOT_FOUND and no mode fields are
    /// advertised.
    session_modes: Option<Arc<dyn SessionModeControl>>,
}

/// Typed ACP v1 adapter failure. Display never echoes prompt, cwd, or payload.
#[derive(Debug)]
pub enum V1Error {
    Cancelled,
    NotInitialized,
    AlreadyInitialized,
    MethodNotFound,
    InvalidParams,
    UnsupportedContent,
    PromptTooLarge,
    TooManySessions,
    SessionClosed,
    UnknownSession,
    Kernel(ApiError),
    Stream(EventStreamError),
}

/// Result of dispatching one inbound JSON-RPC frame.
#[derive(Debug)]
pub enum HandleResult {
    Reply(JsonRpcMessage),
    AcceptedNotification,
    Prompt {
        request_id: JsonRpcId,
        turn: PromptTurn,
        events: Vec<MappedEvent>,
    },
}

/// Handshake result. Capabilities are advertisements, not grants.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    protocol_version: u32,
    agent_capabilities: AgentCapabilities,
    agent_info: ImplementationInfo,
    auth_methods: Vec<Value>,
}

/// Kernel session created (or attached) under the ACP session id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NewSessionResult {
    session_id: SessionId,
    seq: u64,
}

/// Accepted `session/prompt` after `turn.started` is committed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptTurn {
    session_id: SessionId,
    turn_id: TurnId,
    seq: u64,
}

/// One kernel event mapped onto ACP v1 session/prompt/tool-update semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MappedEvent {
    SessionUpdate(SessionUpdateNotification),
    PermissionRequired(PermissionRequest),
    PromptStopped(StopReason),
}

/// `session/update` notification params.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateNotification {
    session_id: SessionId,
    update: SessionUpdate,
}

impl SessionUpdateNotification {
    /// A text message from the agent to the client on `session_id`.
    pub fn agent_text(session_id: SessionId, text: impl Into<String>) -> Self {
        Self {
            session_id,
            update: SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text { text: text.into() },
            },
        }
    }
}

/// ACP v1 session update body.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    AgentMessageChunk {
        content: ContentBlock,
    },
    ToolCall {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        title: String,
        status: ToolCallStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        kind: Option<ToolKind>,
    },
    ToolCallUpdate {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<ToolCallStatus>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// File edit payload: bounded unified diff for patch/commit updates.
        #[serde(skip_serializing_if = "Option::is_none")]
        diff: Option<FileDiff>,
    },
}

/// Bounded unified file diff reported on an edit tool-call update. The body
/// is display context only; it never authorizes a write by itself.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    path: String,
    unified: String,
}

impl FileDiff {
    pub fn new(path: impl Into<String>, unified: impl Into<String>) -> Option<Self> {
        let path = path.into();
        let mut unified = unified.into();
        if path.is_empty() || path.len() > MAX_DIFF_PATH_BYTES {
            return None;
        }
        if unified.is_empty() {
            return None;
        }
        if unified.len() > MAX_DIFF_BYTES {
            truncate_to_char_boundary(&mut unified, MAX_DIFF_BYTES);
        }
        Some(Self { path, unified })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn unified(&self) -> &str {
        &self.unified
    }
}

/// ACP v1 content block. Prompt ingest accepts text and resource links only.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ResourceLink {
        uri: String,
        name: String,
    },
    #[serde(other)]
    Unsupported,
}

/// Tool-call lifecycle reported on `session/update`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Closed set of ACP tool kinds. Unknown payload kinds are omitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

/// Why a prompt turn stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

/// `session/request_permission` params. Decision is not applied here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    session_id: SessionId,
    tool_call: PermissionToolCall,
    options: Vec<PermissionOption>,
}

/// Client decision forwarded to [`kernel::KernelClient::approve`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PermissionOutcome {
    Approved,
    Denied,
}

/// The client's answer to a `session/request_permission` request, decoded
/// by [`decode_permission_response`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PermissionAnswer {
    /// The user selected one of the offered options.
    Selected(PermissionOutcome),
    /// The prompt turn was cancelled before the user chose.
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentCapabilities {
    load_session: bool,
    prompt_capabilities: PromptCapabilities,
    mcp_capabilities: McpCapabilities,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptCapabilities {
    image: bool,
    audio: bool,
    embedded_context: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct McpCapabilities {
    http: bool,
    sse: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ImplementationInfo {
    name: String,
    version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PermissionToolCall {
    tool_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PermissionOption {
    option_id: String,
    name: String,
    kind: PermissionOptionKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    protocol_version: u32,
    #[serde(default)]
    client_info: Option<ImplementationInfo>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewSessionParams {
    cwd: String,
    #[serde(default)]
    mcp_servers: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionIdParams {
    session_id: String,
}

/// `session/set_mode` parameters: the session to switch and the target
/// mode id (one of the advertised ids).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetModeParams {
    session_id: String,
    mode: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptParams {
    session_id: String,
    prompt: Vec<ContentBlock>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionOutcomeParams {
    session_id: String,
    outcome: PermissionOutcomeWire,
}

/// The result of a `session/request_permission` response.
#[derive(Clone, Debug, Deserialize)]
struct PermissionResponseWire {
    outcome: PermissionOutcomeWire,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum PermissionOutcomeWire {
    Cancelled,
    Selected {
        #[serde(rename = "optionId")]
        option_id: String,
    },
}

impl PermissionOutcomeWire {
    /// Read against the options [`permission_options`] offers; an option
    /// never offered is `InvalidParams`.
    fn answer(self) -> Result<PermissionAnswer, V1Error> {
        match self {
            Self::Cancelled => Ok(PermissionAnswer::Cancelled),
            Self::Selected { option_id } => match option_id.as_str() {
                OPTION_ALLOW_ONCE | OPTION_ALLOW_ALWAYS => {
                    Ok(PermissionAnswer::Selected(PermissionOutcome::Approved))
                }
                OPTION_REJECT_ONCE | OPTION_REJECT_ALWAYS => {
                    Ok(PermissionAnswer::Selected(PermissionOutcome::Denied))
                }
                _ => Err(V1Error::InvalidParams),
            },
        }
    }
}

impl<C: KernelClient> V1Adapter<C> {
    pub fn new(
        client: C,
        project_id: ProjectId,
        actor: ActorRef,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            client,
            project_id,
            actor,
            cancel,
            initialized: false,
            cursors: HashMap::new(),
            session_modes: None,
        }
    }

    /// Install mode control: `session/set_mode` becomes available and
    /// `session/new`/`session/load` advertise the modes. Install ONLY a
    /// control backed by real runtime behavior — advertisement is a
    /// contract, not a wish.
    pub fn with_session_modes(mut self, modes: Arc<dyn SessionModeControl>) -> Self {
        self.session_modes = Some(modes);
        self
    }

    /// The installed mode control, when any.
    pub fn session_modes(&self) -> Option<&Arc<dyn SessionModeControl>> {
        self.session_modes.as_ref()
    }

    /// The `modes` + `currentMode` advertisement block, present only when
    /// mode control is installed.
    fn mode_advertisement(&self) -> Option<Value> {
        let modes = self.session_modes.as_ref()?;
        let advertised: Vec<Value> = modes
            .modes()
            .into_iter()
            .map(|(id, name)| serde_json::json!({"id": id, "name": name}))
            .collect();
        Some(serde_json::json!({
            "modes": advertised,
            "currentMode": modes.current_mode(),
        }))
    }

    pub fn client(&self) -> &C {
        &self.client
    }

    pub fn cursor(&self, session_id: SessionId) -> Option<u64> {
        self.cursors.get(&session_id).copied()
    }

    /// Dispatch one inbound ACP frame. Unknown methods stay session-valid.
    pub async fn handle(&mut self, message: &JsonRpcMessage) -> Result<HandleResult, V1Error> {
        self.check_cancel()?;
        match message {
            JsonRpcMessage::Request { id, method, params } => {
                match self.dispatch_request(method, params.as_ref()).await {
                    Ok(Dispatch::Value(value)) => Ok(HandleResult::Reply(JsonRpcMessage::Result {
                        id: id.clone(),
                        result: value,
                    })),
                    Ok(Dispatch::Prompt { turn, events }) => Ok(HandleResult::Prompt {
                        request_id: id.clone(),
                        turn,
                        events,
                    }),
                    Err(err) => Ok(HandleResult::Reply(jsonrpc_error(id.clone(), &err))),
                }
            }
            JsonRpcMessage::Notification { method, params } => {
                if method != METHOD_SESSION_CANCEL {
                    return Err(V1Error::MethodNotFound);
                }
                match self.session_cancel_params(params.as_ref()).await {
                    Ok(()) => Ok(HandleResult::AcceptedNotification),
                    Err(V1Error::Cancelled) => Err(V1Error::Cancelled),
                    Err(err) => Err(err),
                }
            }
            JsonRpcMessage::Result { .. } | JsonRpcMessage::Error { .. } => {
                Err(V1Error::InvalidParams)
            }
        }
    }

    pub async fn initialize(&mut self, params: Value) -> Result<InitializeResult, V1Error> {
        self.check_cancel()?;
        if self.initialized {
            return Err(V1Error::AlreadyInitialized);
        }
        let parsed: InitializeParams = parse_params(params)?;
        if parsed.protocol_version == 0 {
            return Err(V1Error::InvalidParams);
        }
        if let Some(info) = parsed.client_info.as_ref() {
            validate_implementation(info)?;
        }
        self.initialized = true;
        Ok(InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                prompt_capabilities: PromptCapabilities {
                    image: false,
                    audio: false,
                    embedded_context: false,
                },
                mcp_capabilities: McpCapabilities {
                    http: false,
                    sse: false,
                },
            },
            agent_info: ImplementationInfo {
                name: AGENT_NAME.to_owned(),
                version: AGENT_VERSION.to_owned(),
            },
            auth_methods: Vec::new(),
        })
    }

    pub async fn session_new(&mut self, params: Value) -> Result<NewSessionResult, V1Error> {
        self.require_ready()?;
        let parsed: NewSessionParams = parse_params(params)?;
        if parsed.cwd.is_empty() || parsed.cwd.len() > MAX_CWD_BYTES {
            return Err(V1Error::InvalidParams);
        }
        if parsed.mcp_servers.len() > MAX_MCP_SERVERS {
            return Err(V1Error::InvalidParams);
        }
        let snapshot = self
            .client
            .create_session(CreateSession::new(
                self.project_id,
                self.actor.clone(),
                TraceId::new(),
            ))
            .await
            .map_err(map_kernel_err)?;
        self.bind(snapshot.id(), snapshot.seq())?;
        Ok(NewSessionResult {
            session_id: snapshot.id(),
            seq: snapshot.seq(),
        })
    }

    pub async fn session_load(&mut self, params: Value) -> Result<NewSessionResult, V1Error> {
        self.require_ready()?;
        let parsed: SessionIdParams = parse_params(params)?;
        let session_id = parse_session_id(&parsed.session_id)?;
        let snapshot = self.load_kernel_session(session_id).await?;
        self.bind(snapshot.id(), snapshot.seq())?;
        Ok(NewSessionResult {
            session_id: snapshot.id(),
            seq: snapshot.seq(),
        })
    }

    pub async fn session_prompt(
        &mut self,
        params: Value,
    ) -> Result<(PromptTurn, Vec<MappedEvent>), V1Error> {
        self.require_ready()?;
        let parsed: PromptParams = parse_params(params)?;
        validate_prompt(&parsed.prompt)?;
        let session_id = parse_session_id(&parsed.session_id)?;
        let snapshot = self.load_kernel_session(session_id).await?;
        if !self.cursors.contains_key(&session_id) {
            self.bind(session_id, snapshot.seq())?;
        }
        let handle = self
            .client
            .submit_turn(SubmitTurn::new(
                session_id,
                snapshot.seq(),
                self.actor.clone(),
                TraceId::new(),
                prompt_text(&parsed.prompt),
            ))
            .await
            .map_err(map_kernel_err)?;
        // A prompt's updates are its own turn's. The turn was submitted
        // against `snapshot.seq()` (its `turn.started` is the next event), so
        // the drain starts there: anything earlier is not this prompt's to
        // report — earlier prompts streamed their own turns, and a turn run
        // from another surface is that surface's — and is never replayed.
        self.cursors.insert(session_id, snapshot.seq());
        let events = self.drain_updates(session_id).await?;
        Ok((prompt_turn(handle), events))
    }

    pub async fn session_cancel(&mut self, session_id: SessionId) -> Result<(), V1Error> {
        self.require_ready()?;
        self.load_kernel_session(session_id).await?;
        self.client
            .interrupt(Interrupt::new(
                session_id,
                InterruptReason::ClientRequested,
                self.actor.clone(),
                TraceId::new(),
            ))
            .await
            .map_err(map_kernel_err)?;
        Ok(())
    }

    /// Forward a permission decision to the kernel approval API.
    pub async fn resolve_permission(
        &mut self,
        session_id: SessionId,
        outcome: PermissionOutcome,
    ) -> Result<(), V1Error> {
        self.require_ready()?;
        let snapshot = self.load_kernel_session(session_id).await?;
        let decision = match outcome {
            PermissionOutcome::Approved => ApprovalDecision::Approved,
            PermissionOutcome::Denied => ApprovalDecision::Denied,
        };
        self.client
            .approve(ResolveApproval::new(
                session_id,
                snapshot.seq(),
                decision,
                self.actor.clone(),
                TraceId::new(),
            ))
            .await
            .map_err(map_kernel_err)?;
        Ok(())
    }

    pub async fn resolve_permission_params(&mut self, params: Value) -> Result<(), V1Error> {
        self.require_ready()?;
        let parsed: PermissionOutcomeParams = parse_params(params)?;
        let session_id = parse_session_id(&parsed.session_id)?;
        let outcome = match parsed.outcome.answer()? {
            PermissionAnswer::Cancelled => PermissionOutcome::Denied,
            PermissionAnswer::Selected(outcome) => outcome,
        };
        self.resolve_permission(session_id, outcome).await
    }

    /// Consume committed events after `cursor` and advance the binding.
    pub async fn drain_updates(
        &mut self,
        session_id: SessionId,
    ) -> Result<Vec<MappedEvent>, V1Error> {
        self.check_cancel()?;
        let snapshot = self.load_kernel_session(session_id).await?;
        let from_seq = self.cursors.get(&session_id).copied().unwrap_or(0);
        if from_seq >= snapshot.seq() {
            return Ok(Vec::new());
        }
        let mut stream = self
            .client
            .subscribe(SubscribeEvents::new(session_id, from_seq))
            .await
            .map_err(map_kernel_err)?;
        let mut events = Vec::new();
        let target = snapshot.seq();
        let mut drained = 0usize;
        while drained < MAX_DRAIN_EVENTS {
            let consumed = self.cursors.get(&session_id).copied().unwrap_or(from_seq);
            if consumed >= target {
                break;
            }
            if drained.is_multiple_of(CANCEL_STRIDE) {
                self.check_cancel()?;
            }
            match stream.recv() {
                Ok(event) => {
                    drained = drained.saturating_add(1);
                    self.cursors.insert(session_id, event.seq());
                    if let Some(mapped) = map_kernel_event(&event) {
                        events.push(mapped);
                    }
                }
                Err(EventStreamError::Cancelled { resume_cursor }) => {
                    self.cursors.insert(session_id, resume_cursor);
                    return Err(V1Error::Cancelled);
                }
                Err(EventStreamError::Lagged { resume_cursor }) => {
                    self.cursors.insert(session_id, resume_cursor);
                    break;
                }
                Err(err) => return Err(V1Error::Stream(err)),
            }
        }
        Ok(events)
    }

    async fn dispatch_request(
        &mut self,
        method: &str,
        params: Option<&Value>,
    ) -> Result<Dispatch, V1Error> {
        let params = params.cloned().unwrap_or_else(|| Value::Object(Map::new()));
        match method {
            METHOD_INITIALIZE => {
                let result = self.initialize(params).await?;
                Ok(Dispatch::Value(
                    serde_json::to_value(result).map_err(|_| V1Error::InvalidParams)?,
                ))
            }
            METHOD_SESSION_NEW => {
                let result = self.session_new(params).await?;
                let mut reply = serde_json::json!({
                    "sessionId": result.session_id,
                });
                if let Some(advertised) = self.mode_advertisement() {
                    reply["modes"] = advertised["modes"].clone();
                    reply["currentMode"] = advertised["currentMode"].clone();
                }
                Ok(Dispatch::Value(reply))
            }
            METHOD_SESSION_LOAD => {
                let _ = self.session_load(params).await?;
                let mut reply = Value::Object(Map::new());
                if let Some(advertised) = self.mode_advertisement() {
                    reply["modes"] = advertised["modes"].clone();
                    reply["currentMode"] = advertised["currentMode"].clone();
                }
                Ok(Dispatch::Value(reply))
            }
            METHOD_SESSION_PROMPT => {
                let (turn, events) = self.session_prompt(params).await?;
                Ok(Dispatch::Prompt { turn, events })
            }
            METHOD_SESSION_SET_MODE => {
                let result = self.session_set_mode(params).await?;
                Ok(Dispatch::Value(result))
            }
            _ => Err(V1Error::MethodNotFound),
        }
    }

    /// `session/set_mode`: switch the session's mode for subsequent
    /// prompts. Requires installed mode control; unknown ids are typed
    /// parameter errors that leave the current mode untouched.
    async fn session_set_mode(&mut self, params: Value) -> Result<Value, V1Error> {
        self.require_ready()?;
        let Some(modes) = self.session_modes.as_ref() else {
            return Err(V1Error::MethodNotFound);
        };
        let parsed: SetModeParams = parse_params(params)?;
        let session_id = parse_session_id(&parsed.session_id)?;
        // The session must exist and be open; the mode itself is
        // composition-root state, so a switch on a closed session is a
        // protocol error, not a silent no-op.
        let snapshot = self.load_kernel_session(session_id).await?;
        if snapshot.status() == SessionStatus::Closed {
            return Err(V1Error::SessionClosed);
        }
        modes
            .set_mode(&parsed.mode)
            .map_err(|_| V1Error::InvalidParams)?;
        let advertisement = self.mode_advertisement().ok_or(V1Error::MethodNotFound)?;
        Ok(advertisement)
    }

    async fn session_cancel_params(&mut self, params: Option<&Value>) -> Result<(), V1Error> {
        let params = params.cloned().ok_or(V1Error::InvalidParams)?;
        let parsed: SessionIdParams = parse_params(params)?;
        let session_id = parse_session_id(&parsed.session_id)?;
        self.session_cancel(session_id).await
    }

    async fn load_kernel_session(&self, session_id: SessionId) -> Result<SessionSnapshot, V1Error> {
        self.check_cancel()?;
        let snapshot = self
            .client
            .get_session(session_id)
            .await
            .map_err(map_kernel_err)?;
        if snapshot.status() == SessionStatus::Closed {
            return Err(V1Error::SessionClosed);
        }
        Ok(snapshot)
    }

    fn bind(&mut self, session_id: SessionId, seq: u64) -> Result<(), V1Error> {
        if !self.cursors.contains_key(&session_id) && self.cursors.len() >= MAX_SESSION_BINDINGS {
            return Err(V1Error::TooManySessions);
        }
        self.cursors.insert(session_id, seq);
        Ok(())
    }

    /// Whether `initialize` has completed (and the adapter is not cancelled):
    /// a composition root that answers a request itself checks this first.
    pub fn is_ready(&self) -> bool {
        self.require_ready().is_ok()
    }

    fn require_ready(&self) -> Result<(), V1Error> {
        self.check_cancel()?;
        if self.initialized {
            Ok(())
        } else {
            Err(V1Error::NotInitialized)
        }
    }

    fn check_cancel(&self) -> Result<(), V1Error> {
        if self.cancel.is_cancelled() {
            Err(V1Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

enum Dispatch {
    Value(Value),
    Prompt {
        turn: PromptTurn,
        events: Vec<MappedEvent>,
    },
}

impl InitializeResult {
    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    pub fn load_session(&self) -> bool {
        self.agent_capabilities.load_session
    }
}

impl NewSessionResult {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }
}

impl PromptTurn {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }
}

impl PermissionRequest {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn tool_call_id(&self) -> &str {
        &self.tool_call.tool_call_id
    }
}

impl SessionUpdateNotification {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn update(&self) -> &SessionUpdate {
        &self.update
    }

    /// Diff body when this update is a file edit (`WorkspacePatchStaged` /
    /// `WorkspaceTransactionCommitted` mapping).
    pub fn diff(&self) -> Option<&FileDiff> {
        match &self.update {
            SessionUpdate::ToolCallUpdate { diff, .. } => diff.as_ref(),
            _ => None,
        }
    }
}

impl V1Error {
    pub fn jsonrpc_code(&self) -> i64 {
        match self {
            Self::MethodNotFound => METHOD_NOT_FOUND,
            Self::InvalidParams
            | Self::NotInitialized
            | Self::AlreadyInitialized
            | Self::UnsupportedContent
            | Self::PromptTooLarge
            | Self::TooManySessions
            | Self::SessionClosed
            | Self::UnknownSession => INVALID_PARAMS,
            Self::Cancelled | Self::Kernel(_) | Self::Stream(_) => INTERNAL_ERROR,
        }
    }

    fn message(&self) -> &'static str {
        match self {
            Self::Cancelled => "acp adapter cancelled",
            Self::NotInitialized => "acp adapter is not initialized",
            Self::AlreadyInitialized => "acp adapter already initialized",
            Self::MethodNotFound => "Method not found",
            Self::InvalidParams => "Invalid params",
            Self::UnsupportedContent => "prompt content type is not advertised",
            Self::PromptTooLarge => "prompt exceeds the configured bound",
            Self::TooManySessions => "acp session binding limit reached",
            Self::SessionClosed => "session is closed",
            Self::UnknownSession => "session not found",
            Self::Kernel(_) => "kernel request failed",
            Self::Stream(_) => "session event stream failed",
        }
    }
}

impl fmt::Display for V1Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl Error for V1Error {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Kernel(err) => Some(err),
            Self::Stream(err) => Some(err),
            _ => None,
        }
    }
}

impl StopReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTokens => "max_tokens",
            Self::MaxTurnRequests => "max_turn_requests",
            Self::Refusal => "refusal",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Map one committed kernel event onto ACP v1 update/permission/stop.
pub fn map_kernel_event(event: &ErasedEventEnvelope) -> Option<MappedEvent> {
    let session_id = event.session_id();
    let payload = event.payload();
    match event.kind() {
        EventKind::ModelStreamDelta | EventKind::ModelCompleted => {
            text_from_payload(payload).map(|text| {
                MappedEvent::SessionUpdate(SessionUpdateNotification {
                    session_id,
                    update: SessionUpdate::AgentMessageChunk {
                        content: ContentBlock::Text { text },
                    },
                })
            })
        }
        EventKind::ToolRequested | EventKind::ToolApprovalRequired => {
            tool_call(session_id, payload, ToolCallStatus::Pending).map(MappedEvent::SessionUpdate)
        }
        EventKind::ToolAuthorized | EventKind::ToolStarted => {
            tool_call_update(session_id, payload, ToolCallStatus::InProgress)
                .map(MappedEvent::SessionUpdate)
        }
        EventKind::ToolCompleted => {
            tool_call_update(session_id, payload, ToolCallStatus::Completed)
                .map(MappedEvent::SessionUpdate)
        }
        EventKind::ToolFailed | EventKind::ToolDenied => {
            tool_call_update(session_id, payload, ToolCallStatus::Failed)
                .map(MappedEvent::SessionUpdate)
        }
        EventKind::WorkspacePatchStaged | EventKind::WorkspaceTransactionCommitted => {
            file_edit_update(session_id, payload).map(MappedEvent::SessionUpdate)
        }
        EventKind::ApprovalRequested => Some(MappedEvent::PermissionRequired(PermissionRequest {
            session_id,
            tool_call: PermissionToolCall {
                tool_call_id: tool_call_id(payload).unwrap_or_else(|| "approval".to_owned()),
                title: tool_title(payload),
            },
            options: permission_options(),
        })),
        EventKind::TurnCompleted => Some(MappedEvent::PromptStopped(stop_reason(
            payload,
            StopReason::EndTurn,
        ))),
        EventKind::TurnInterrupted => Some(MappedEvent::PromptStopped(StopReason::Cancelled)),
        EventKind::TurnFailed => Some(MappedEvent::PromptStopped(stop_reason(
            payload,
            StopReason::Refusal,
        ))),
        _ => None,
    }
}

/// Encode a mapped `session/update` notification.
pub fn encode_session_update(
    update: &SessionUpdateNotification,
) -> Result<JsonRpcMessage, V1Error> {
    let params = serde_json::to_value(update).map_err(|_| V1Error::InvalidParams)?;
    Ok(JsonRpcMessage::Notification {
        method: METHOD_SESSION_UPDATE.to_owned(),
        params: Some(params),
    })
}

/// Encode a mapped `session/request_permission` request.
pub fn encode_permission_request(
    id: JsonRpcId,
    request: &PermissionRequest,
) -> Result<JsonRpcMessage, V1Error> {
    let params = serde_json::to_value(request).map_err(|_| V1Error::InvalidParams)?;
    Ok(JsonRpcMessage::Request {
        id,
        method: METHOD_SESSION_REQUEST_PERMISSION.to_owned(),
        params: Some(params),
    })
}

/// Decode the result of the client's response to a
/// `session/request_permission` request: the protocol nests the outcome,
/// `{"outcome":{"outcome":"selected","optionId":"allow-once"}}` or
/// `{"outcome":{"outcome":"cancelled"}}`. An option id the request never
/// offered, or any other shape, is `InvalidParams`.
pub fn decode_permission_response(result: Value) -> Result<PermissionAnswer, V1Error> {
    parse_params::<PermissionResponseWire>(result)?
        .outcome
        .answer()
}

/// Encode a completed `session/prompt` result.
pub fn encode_prompt_response(id: JsonRpcId, stop: StopReason) -> JsonRpcMessage {
    JsonRpcMessage::Result {
        id,
        result: serde_json::json!({ "stopReason": stop }),
    }
}

fn prompt_turn(handle: TurnHandle) -> PromptTurn {
    PromptTurn {
        session_id: handle.session_id(),
        turn_id: handle.turn_id(),
        seq: handle.seq(),
    }
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, V1Error> {
    serde_json::from_value(params).map_err(|_| V1Error::InvalidParams)
}

fn parse_session_id(raw: &str) -> Result<SessionId, V1Error> {
    SessionId::from_str(raw).map_err(|_| V1Error::InvalidParams)
}

fn validate_implementation(info: &ImplementationInfo) -> Result<(), V1Error> {
    if info.name.is_empty() || info.name.len() > MAX_IMPLEMENTATION_NAME_BYTES {
        return Err(V1Error::InvalidParams);
    }
    if info.version.is_empty() || info.version.len() > MAX_IMPLEMENTATION_VERSION_BYTES {
        return Err(V1Error::InvalidParams);
    }
    Ok(())
}

/// Flatten a prompt's text blocks for `SubmitTurn`'s display text. Non-text
/// blocks (image/resource) contribute nothing here — this is transcript
/// display text, not the model-facing content the turn actually runs on.
fn prompt_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn validate_prompt(blocks: &[ContentBlock]) -> Result<(), V1Error> {
    if blocks.is_empty() || blocks.len() > MAX_PROMPT_BLOCKS {
        return Err(V1Error::InvalidParams);
    }
    let mut text_bytes = 0usize;
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                if text.is_empty() {
                    return Err(V1Error::InvalidParams);
                }
                text_bytes = text_bytes.saturating_add(text.len());
                if text_bytes > MAX_PROMPT_TEXT_BYTES {
                    return Err(V1Error::PromptTooLarge);
                }
            }
            ContentBlock::ResourceLink { uri, name } => {
                if uri.is_empty() || name.is_empty() {
                    return Err(V1Error::InvalidParams);
                }
            }
            ContentBlock::Unsupported => return Err(V1Error::UnsupportedContent),
        }
    }
    Ok(())
}

fn map_kernel_err(err: ApiError) -> V1Error {
    if err.code() == ErrorCode::SessionNotFound {
        V1Error::UnknownSession
    } else {
        V1Error::Kernel(err)
    }
}

fn jsonrpc_error(id: JsonRpcId, err: &V1Error) -> JsonRpcMessage {
    JsonRpcMessage::Error {
        id,
        error: JsonRpcErrorObject::new(err.jsonrpc_code(), err.message()),
    }
}

fn tool_call(
    session_id: SessionId,
    payload: &Value,
    status: ToolCallStatus,
) -> Option<SessionUpdateNotification> {
    let tool_call_id = tool_call_id(payload)?;
    Some(SessionUpdateNotification {
        session_id,
        update: SessionUpdate::ToolCall {
            tool_call_id,
            title: tool_title(payload).unwrap_or_else(|| "tool".to_owned()),
            status,
            kind: tool_kind(payload),
        },
    })
}

fn tool_call_update(
    session_id: SessionId,
    payload: &Value,
    status: ToolCallStatus,
) -> Option<SessionUpdateNotification> {
    let tool_call_id = tool_call_id(payload)?;
    Some(SessionUpdateNotification {
        session_id,
        update: SessionUpdate::ToolCallUpdate {
            tool_call_id,
            status: Some(status),
            title: tool_title(payload),
            diff: None,
        },
    })
}

/// Map a staged/committed workspace patch onto an edit tool-call update that
/// carries the bounded unified diff. Payload without a usable path or patch
/// body maps to `None`.
fn file_edit_update(session_id: SessionId, payload: &Value) -> Option<SessionUpdateNotification> {
    let path = payload_string(payload, &["path", "file", "target"])
        .filter(|p| !p.is_empty() && p.len() <= MAX_DIFF_PATH_BYTES)?;
    let mut unified = payload_string(payload, &["diff", "patch", "unified"])?;
    if unified.is_empty() {
        return None;
    }
    if unified.len() > MAX_DIFF_BYTES {
        truncate_to_char_boundary(&mut unified, MAX_DIFF_BYTES);
    }
    let tool_call_id = tool_call_id(payload).unwrap_or_else(|| format!("edit:{path}"));
    Some(SessionUpdateNotification {
        session_id,
        update: SessionUpdate::ToolCallUpdate {
            tool_call_id,
            status: None,
            title: Some(path.clone()),
            diff: Some(FileDiff { path, unified }),
        },
    })
}

fn tool_call_id(payload: &Value) -> Option<String> {
    payload_string(payload, &["call_id", "tool_call_id", "approval_id", "id"])
        .filter(|id| !id.is_empty() && id.len() <= MAX_TOOL_CALL_ID_BYTES)
}

fn tool_title(payload: &Value) -> Option<String> {
    payload_string(payload, &["tool", "title", "name"]).map(|mut title| {
        if title.len() > MAX_TOOL_TITLE_BYTES {
            truncate_to_char_boundary(&mut title, MAX_TOOL_TITLE_BYTES);
        }
        title
    })
}

fn tool_kind(payload: &Value) -> Option<ToolKind> {
    match payload.get("kind").and_then(Value::as_str) {
        Some("read") => Some(ToolKind::Read),
        Some("edit") => Some(ToolKind::Edit),
        Some("delete") => Some(ToolKind::Delete),
        Some("move") => Some(ToolKind::Move),
        Some("search") => Some(ToolKind::Search),
        Some("execute") => Some(ToolKind::Execute),
        Some("think") => Some(ToolKind::Think),
        Some("fetch") => Some(ToolKind::Fetch),
        Some("other") => Some(ToolKind::Other),
        _ => None,
    }
}

fn text_from_payload(payload: &Value) -> Option<String> {
    let mut text = payload_string(payload, &["text", "delta", "content"])?;
    if text.is_empty() {
        return None;
    }
    if text.len() > MAX_UPDATE_TEXT_BYTES {
        truncate_to_char_boundary(&mut text, MAX_UPDATE_TEXT_BYTES);
    }
    Some(text)
}

/// Truncates `s` to at most `max` bytes without panicking on a multi-byte
/// character straddling the cut. `String::truncate` panics unless `max` is a
/// char boundary; a diff/title/chunk taken from a kernel event payload is
/// arbitrary UTF-8, so a length check alone (`s.len() > max`) does not make
/// a raw `truncate(max)` call safe.
fn truncate_to_char_boundary(s: &mut String, max: usize) {
    let mut cut = max.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

fn payload_string(payload: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = payload.get(*key).and_then(Value::as_str) {
            return Some(value.to_owned());
        }
    }
    None
}

fn stop_reason(payload: &Value, default: StopReason) -> StopReason {
    match payload.get("reason").and_then(Value::as_str) {
        Some("cancelled") | Some("client_requested") => StopReason::Cancelled,
        Some("budget_exhausted") | Some("max_tokens") => StopReason::MaxTokens,
        Some("max_turn_requests") => StopReason::MaxTurnRequests,
        Some("refusal") => StopReason::Refusal,
        Some("end_turn") | Some("completed") => StopReason::EndTurn,
        _ => default,
    }
}

fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            option_id: OPTION_ALLOW_ONCE.to_owned(),
            name: "Allow once".to_owned(),
            kind: PermissionOptionKind::AllowOnce,
        },
        PermissionOption {
            option_id: OPTION_ALLOW_ALWAYS.to_owned(),
            name: "Allow always".to_owned(),
            kind: PermissionOptionKind::AllowAlways,
        },
        PermissionOption {
            option_id: OPTION_REJECT_ONCE.to_owned(),
            name: "Reject once".to_owned(),
            kind: PermissionOptionKind::RejectOnce,
        },
        PermissionOption {
            option_id: OPTION_REJECT_ALWAYS.to_owned(),
            name: "Reject always".to_owned(),
            kind: PermissionOptionKind::RejectAlways,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_ledger::event::{ActorKind, EventEnvelope, RecordedAt};
    use kernel::InProcessKernelClient;
    use protocol::{EventId, RedactionClass};
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    const GOLDEN_INITIALIZE: &str = concat!(
        r#"{"protocolVersion":1,"agentCapabilities":{"#,
        r#""loadSession":true,"promptCapabilities":{"#,
        r#""image":false,"audio":false,"embeddedContext":false},"#,
        r#""mcpCapabilities":{"http":false,"sse":false}},"#,
        r#""agentInfo":{"name":"rapidlm","version":"0.1.0"},"authMethods":[]}"#
    );

    const GOLDEN_TOOL_UPDATE: &str = concat!(
        r#"{"sessionId":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","update":{"#,
        r#""sessionUpdate":"tool_call","toolCallId":"c1","title":"repo.read","status":"pending"}}"#
    );

    struct TempClient {
        path: PathBuf,
        client: InProcessKernelClient,
    }

    impl TempClient {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-acp-v1-{}-{seq}.sqlite",
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
        let mut cx = Context::from_waker(waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("in-process kernel future stayed pending"),
        }
    }

    fn actor() -> ActorRef {
        ActorRef::new(ActorKind::ExternalHuman, &EventId::new().to_string()).expect("actor")
    }

    fn adapter(client: InProcessKernelClient) -> V1Adapter<InProcessKernelClient> {
        V1Adapter::new(client, ProjectId::new(), actor(), CancellationToken::new())
    }

    async fn ready_adapter(client: InProcessKernelClient) -> V1Adapter<InProcessKernelClient> {
        let mut acp = adapter(client);
        acp.initialize(serde_json::json!({"protocolVersion": 1}))
            .await
            .expect("initialize");
        acp
    }

    fn envelope(kind: EventKind, session_id: SessionId, payload: Value) -> ErasedEventEnvelope {
        EventEnvelope::new(
            EventId::new(),
            session_id,
            2,
            RecordedAt::from_str("2026-08-14T15:20:04.123Z").expect("ts"),
            actor(),
            TraceId::new(),
            kind,
            RedactionClass::Project,
            payload,
        )
    }

    #[test]
    fn initialize_golden_and_higher_client_version_stays_v1() {
        let tmp = TempClient::create();
        let mut acp = adapter(tmp.client.clone());
        let result = block_on(acp.initialize(serde_json::json!({
            "protocolVersion": 2,
            "clientCapabilities": {"fs": {"readTextFile": true}, "terminal": true},
            "clientInfo": {"name": "editor", "version": "1.0.0"}
        })))
        .expect("initialize");
        assert_eq!(result.protocol_version(), PROTOCOL_VERSION);
        assert!(result.load_session());
        assert_eq!(
            serde_json::to_string(&result).expect("ser"),
            GOLDEN_INITIALIZE
        );
        let again = block_on(acp.initialize(serde_json::json!({"protocolVersion": 1})));
        assert!(matches!(again, Err(V1Error::AlreadyInitialized)));
    }

    #[test]
    fn session_new_is_visible_to_kernel_client() {
        let tmp = TempClient::create();
        let mut acp = block_on(ready_adapter(tmp.client.clone()));
        let created = block_on(acp.session_new(serde_json::json!({
            "cwd": "/tmp/project",
            "mcpServers": [{"name": "untrusted", "command": "echo"}]
        })))
        .expect("session/new");
        let loaded = block_on(tmp.client.get_session(created.session_id())).expect("kernel get");
        assert_eq!(loaded.id(), created.session_id());
        assert_eq!(loaded.seq(), created.seq());
        assert_eq!(acp.cursor(created.session_id()), Some(created.seq()));
        assert!(tmp.path.exists());
    }

    #[test]
    fn tui_created_session_can_be_loaded_and_prompted() {
        let tmp = TempClient::create();
        let kernel_session = block_on(tmp.client.create_session(CreateSession::new(
            ProjectId::new(),
            actor(),
            TraceId::new(),
        )))
        .expect("tui create");
        let mut acp = block_on(ready_adapter(tmp.client.clone()));
        let loaded = block_on(acp.session_load(serde_json::json!({
            "sessionId": kernel_session.id(),
            "cwd": "/tmp/project",
            "mcpServers": []
        })))
        .expect("session/load");
        assert_eq!(loaded.session_id(), kernel_session.id());

        let (turn, events) = block_on(acp.session_prompt(serde_json::json!({
            "sessionId": kernel_session.id(),
            "prompt": [{"type": "text", "text": "fix the test"}]
        })))
        .expect("session/prompt");
        assert_eq!(turn.session_id(), kernel_session.id());
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, MappedEvent::PromptStopped(_)))
        );
        let busy = block_on(tmp.client.get_session(kernel_session.id())).expect("busy");
        assert_eq!(busy.active_turn(), Some(turn.turn_id()));
        assert_eq!(busy.status(), SessionStatus::Busy);
    }

    #[test]
    fn a_prompt_drains_only_its_own_turn_never_the_previous_ones() {
        let tmp = TempClient::create();
        let mut acp = block_on(ready_adapter(tmp.client.clone()));
        let created = block_on(acp.session_new(serde_json::json!({
            "cwd": "/tmp/project",
            "mcpServers": []
        })))
        .expect("new");
        let session_id = created.session_id();
        let prompt = |text: &str| {
            serde_json::json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": text}]
            })
        };
        let (first, _) = block_on(acp.session_prompt(prompt("first"))).expect("first prompt");
        // The first turn runs to its end outside the adapter, the way a serve
        // executes a turn and streams it from its own `turn.started`.
        let author = actor();
        tmp.client
            .append_turn_progress(
                session_id,
                &author,
                TraceId::new(),
                EventKind::ModelCompleted,
                serde_json::json!({"text": "the first answer"}),
            )
            .expect("progress");
        tmp.client
            .finish_turn(kernel::FinishTurn::new(
                session_id,
                first.turn_id(),
                author,
                TraceId::new(),
                kernel::TurnOutcome::Completed { text: None },
            ))
            .expect("finish");

        let (second, events) =
            block_on(acp.session_prompt(prompt("second"))).expect("second prompt");
        let replayed: Vec<_> = events
            .iter()
            .filter(|event| match event {
                MappedEvent::PromptStopped(_) | MappedEvent::PermissionRequired(_) => true,
                MappedEvent::SessionUpdate(update) => {
                    matches!(update.update(), SessionUpdate::AgentMessageChunk { .. })
                }
            })
            .collect();
        assert!(replayed.is_empty(), "the first turn replayed: {replayed:?}");
        // Drained through its own `turn.started`, and no further.
        assert_eq!(acp.cursor(session_id), Some(second.seq()));
    }

    #[test]
    fn session_cancel_interrupts_kernel_turn() {
        let tmp = TempClient::create();
        let mut acp = block_on(ready_adapter(tmp.client.clone()));
        let created = block_on(acp.session_new(serde_json::json!({
            "cwd": "/tmp/project",
            "mcpServers": []
        })))
        .expect("new");
        let _ = block_on(acp.session_prompt(serde_json::json!({
            "sessionId": created.session_id(),
            "prompt": [{"type": "text", "text": "run"}]
        })))
        .expect("prompt");
        block_on(acp.session_cancel(created.session_id())).expect("cancel");
        let ready = block_on(tmp.client.get_session(created.session_id())).expect("ready");
        assert!(ready.active_turn().is_none());
        assert_eq!(ready.status(), SessionStatus::Ready);
        let updates = block_on(acp.drain_updates(created.session_id())).expect("drain");
        assert!(
            updates
                .iter()
                .any(|event| matches!(event, MappedEvent::PromptStopped(StopReason::Cancelled)))
        );
    }

    #[test]
    fn permission_decision_goes_through_kernel_approve() {
        let tmp = TempClient::create();
        let mut acp = block_on(ready_adapter(tmp.client.clone()));
        let created = block_on(acp.session_new(serde_json::json!({
            "cwd": "/tmp/project",
            "mcpServers": []
        })))
        .expect("new");
        block_on(acp.resolve_permission(created.session_id(), PermissionOutcome::Approved))
            .expect("approve");
        let snapshot = block_on(tmp.client.get_session(created.session_id())).expect("get");
        assert_eq!(snapshot.seq(), created.seq() + 1);
        let updates = block_on(acp.drain_updates(created.session_id())).expect("drain");
        assert!(updates.is_empty());
        let mut stream = block_on(
            tmp.client
                .subscribe(SubscribeEvents::new(created.session_id(), 1)),
        )
        .expect("subscribe");
        let resolved = stream.recv().expect("approval.resolved");
        assert_eq!(resolved.kind(), EventKind::ApprovalResolved);
        assert_eq!(
            resolved.payload().get("decision").and_then(Value::as_str),
            Some("approved")
        );
    }

    #[test]
    fn unknown_permission_option_is_not_approved() {
        let tmp = TempClient::create();
        let mut acp = block_on(ready_adapter(tmp.client.clone()));
        let created = block_on(acp.session_new(serde_json::json!({
            "cwd": "/tmp/project",
            "mcpServers": []
        })))
        .expect("new");
        let err = block_on(acp.resolve_permission_params(serde_json::json!({
            "sessionId": created.session_id(),
            "outcome": {"outcome": "selected", "optionId": "allow-everything"}
        })))
        .expect_err("unknown option");
        assert!(matches!(err, V1Error::InvalidParams));
        let snapshot = block_on(tmp.client.get_session(created.session_id())).expect("unchanged");
        assert_eq!(snapshot.seq(), created.seq());
    }

    #[test]
    fn a_permission_response_is_read_in_the_protocol_s_nested_shape() {
        let selected = |option: &str| {
            decode_permission_response(serde_json::json!({
                "outcome": {"outcome": "selected", "optionId": option}
            }))
        };
        for option in permission_options() {
            let expected = match option.kind {
                PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways => {
                    PermissionOutcome::Approved
                }
                PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => {
                    PermissionOutcome::Denied
                }
            };
            assert_eq!(
                selected(&option.option_id).expect("an offered option"),
                PermissionAnswer::Selected(expected),
                "{}",
                option.option_id
            );
        }
        assert_eq!(
            decode_permission_response(serde_json::json!({"outcome": {"outcome": "cancelled"}}))
                .expect("cancelled"),
            PermissionAnswer::Cancelled
        );
        assert!(matches!(
            selected("allow-everything"),
            Err(V1Error::InvalidParams)
        ));
        // The outcome not nested under `outcome` is no answer at all.
        for flat in [
            serde_json::json!({"outcome": "selected", "optionId": "allow-once"}),
            serde_json::json!({"outcome": "cancelled"}),
            serde_json::json!({}),
        ] {
            assert!(
                matches!(
                    decode_permission_response(flat.clone()),
                    Err(V1Error::InvalidParams)
                ),
                "{flat}"
            );
        }
    }

    #[test]
    fn tool_and_approval_events_map_to_session_updates() {
        let session_id = SessionId::from_str("018f3c8a-7e2b-7a10-8c4d-0123456789ab").expect("id");
        let requested = map_kernel_event(&envelope(
            EventKind::ToolRequested,
            session_id,
            serde_json::json!({"call_id": "c1", "tool": "repo.read"}),
        ))
        .expect("tool_call");
        match &requested {
            MappedEvent::SessionUpdate(update) => {
                assert_eq!(
                    serde_json::to_string(update).expect("ser"),
                    GOLDEN_TOOL_UPDATE
                );
            }
            other => panic!("expected tool_call, got {other:?}"),
        }

        let started = map_kernel_event(&envelope(
            EventKind::ToolStarted,
            session_id,
            serde_json::json!({"call_id": "c1", "tool": "repo.read"}),
        ));
        assert!(matches!(
            started,
            Some(MappedEvent::SessionUpdate(SessionUpdateNotification {
                update: SessionUpdate::ToolCallUpdate {
                    status: Some(ToolCallStatus::InProgress),
                    ..
                },
                ..
            }))
        ));

        let completed = map_kernel_event(&envelope(
            EventKind::ToolCompleted,
            session_id,
            serde_json::json!({"call_id": "c1", "tool": "repo.read"}),
        ));
        assert!(matches!(
            completed,
            Some(MappedEvent::SessionUpdate(SessionUpdateNotification {
                update: SessionUpdate::ToolCallUpdate {
                    status: Some(ToolCallStatus::Completed),
                    ..
                },
                ..
            }))
        ));

        let permission = map_kernel_event(&envelope(
            EventKind::ApprovalRequested,
            session_id,
            serde_json::json!({"approval_id": "a1", "tool": "shell.exec"}),
        ));
        match permission {
            Some(MappedEvent::PermissionRequired(req)) => {
                assert_eq!(req.session_id(), session_id);
                assert_eq!(req.tool_call_id(), "a1");
            }
            other => panic!("expected permission, got {other:?}"),
        }
    }

    #[test]
    fn handle_unknown_method_keeps_session_valid() {
        let tmp = TempClient::create();
        let mut acp = block_on(ready_adapter(tmp.client.clone()));
        let created = block_on(acp.session_new(serde_json::json!({
            "cwd": "/tmp/project",
            "mcpServers": []
        })))
        .expect("new");
        let result = block_on(acp.handle(&JsonRpcMessage::Request {
            id: JsonRpcId::Number(9),
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
    }

    /// A scripted mode control: three known modes; unknown ids refused.
    struct ScriptedModes {
        current: std::sync::Mutex<String>,
    }
    impl SessionModeControl for ScriptedModes {
        fn modes(&self) -> Vec<(String, String)> {
            vec![
                ("default".to_owned(), "Default".to_owned()),
                ("acceptEdits".to_owned(), "Accept edits".to_owned()),
                ("plan".to_owned(), "Plan".to_owned()),
            ]
        }
        fn current_mode(&self) -> String {
            self.current
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }
        fn set_mode(&self, id: &str) -> Result<(), String> {
            let mut current = self.current.lock().unwrap_or_else(|p| p.into_inner());
            if self.modes().iter().any(|(known, _)| known == id) {
                *current = id.to_owned();
                Ok(())
            } else {
                Err(format!("unknown mode {id}"))
            }
        }
    }

    fn adapter_with_modes(client: InProcessKernelClient) -> V1Adapter<InProcessKernelClient> {
        adapter(client).with_session_modes(Arc::new(ScriptedModes {
            current: std::sync::Mutex::new("default".to_owned()),
        }))
    }

    #[test]
    fn modes_are_advertised_and_set_mode_switches_for_subsequent_prompts() {
        let tmp = TempClient::create();
        let mut acp = {
            let mut acp = adapter_with_modes(tmp.client.clone());
            block_on(acp.initialize(serde_json::json!({"protocolVersion": 1})))
                .expect("initialize");
            acp
        };
        // session/new advertises exactly the modes the control names, with
        // the current mode id.
        let reply = block_on(acp.handle(&JsonRpcMessage::Request {
            id: JsonRpcId::Number(2),
            method: METHOD_SESSION_NEW.into(),
            params: Some(serde_json::json!({
                "cwd": "/tmp/project",
                "mcpServers": []
            })),
        }))
        .expect("handle");
        let session_id = match &reply {
            HandleResult::Reply(JsonRpcMessage::Result { result, .. }) => {
                assert_eq!(result["modes"].as_array().expect("modes").len(), 3);
                assert_eq!(result["modes"][0]["id"], "default");
                assert_eq!(result["modes"][1]["name"], "Accept edits");
                assert_eq!(result["currentMode"], "default");
                result["sessionId"].as_str().expect("session id").to_owned()
            }
            other => panic!("expected session/new result, got {other:?}"),
        };
        // A known id switches; the reply carries the refreshed advertisement.
        let reply = block_on(acp.handle(&JsonRpcMessage::Request {
            id: JsonRpcId::Number(3),
            method: METHOD_SESSION_SET_MODE.into(),
            params: Some(serde_json::json!({
                "sessionId": session_id,
                "mode": "acceptEdits"
            })),
        }))
        .expect("handle");
        match &reply {
            HandleResult::Reply(JsonRpcMessage::Result { result, .. }) => {
                assert_eq!(result["currentMode"], "acceptEdits");
            }
            other => panic!("expected set_mode result, got {other:?}"),
        }
        let control = acp.session_modes().expect("control installed").clone();
        assert_eq!(
            control.current_mode(),
            "acceptEdits",
            "state actually switched"
        );
        // An unknown id is a typed parameter error and changes nothing.
        let reply = block_on(acp.handle(&JsonRpcMessage::Request {
            id: JsonRpcId::Number(4),
            method: METHOD_SESSION_SET_MODE.into(),
            params: Some(serde_json::json!({
                "sessionId": session_id,
                "mode": "no-such-mode"
            })),
        }))
        .expect("handle");
        match reply {
            HandleResult::Reply(JsonRpcMessage::Error { error, .. }) => {
                assert_eq!(error.code(), INVALID_PARAMS);
            }
            other => panic!("expected invalid-params, got {other:?}"),
        }
        assert_eq!(
            control.current_mode(),
            "acceptEdits",
            "failed switch is a no-op"
        );
        block_on(
            tmp.client
                .get_session(parse_session_id(&session_id).expect("id")),
        )
        .expect("session still valid");
    }

    #[test]
    fn session_new_omits_mode_fields_without_mode_control() {
        let tmp = TempClient::create();
        let mut acp = block_on(ready_adapter(tmp.client.clone()));
        let reply = block_on(acp.handle(&JsonRpcMessage::Request {
            id: JsonRpcId::Number(5),
            method: METHOD_SESSION_NEW.into(),
            params: Some(serde_json::json!({
                "cwd": "/tmp/project",
                "mcpServers": []
            })),
        }))
        .expect("handle");
        match &reply {
            HandleResult::Reply(JsonRpcMessage::Result { result, .. }) => {
                assert!(
                    result.get("modes").is_none(),
                    "no control, no advertisement"
                );
                assert!(result.get("currentMode").is_none());
            }
            other => panic!("expected session/new result, got {other:?}"),
        }
    }

    #[test]
    fn untrusted_prompt_variants_and_bounds_fail_closed() {
        let tmp = TempClient::create();
        let mut acp = block_on(ready_adapter(tmp.client.clone()));
        let created = block_on(acp.session_new(serde_json::json!({
            "cwd": "/tmp/project",
            "mcpServers": []
        })))
        .expect("new");
        let image = block_on(acp.session_prompt(serde_json::json!({
            "sessionId": created.session_id(),
            "prompt": [{"type": "image", "mimeType": "image/png", "data": "aaaa"}]
        })));
        assert!(matches!(image, Err(V1Error::UnsupportedContent)));

        let huge = "x".repeat(MAX_PROMPT_TEXT_BYTES + 1);
        let oversized = block_on(acp.session_prompt(serde_json::json!({
            "sessionId": created.session_id(),
            "prompt": [{"type": "text", "text": huge}]
        })));
        assert!(matches!(oversized, Err(V1Error::PromptTooLarge)));
        let snapshot = block_on(tmp.client.get_session(created.session_id())).expect("no turn");
        assert!(snapshot.active_turn().is_none());
    }

    #[test]
    fn cancelled_adapter_does_not_create_session() {
        let tmp = TempClient::create();
        let cancel = CancellationToken::new();
        let mut acp = V1Adapter::new(
            tmp.client.clone(),
            ProjectId::new(),
            actor(),
            cancel.clone(),
        );
        block_on(acp.initialize(serde_json::json!({"protocolVersion": 1}))).expect("init");
        cancel.cancel();
        let err = block_on(acp.session_new(serde_json::json!({
            "cwd": "/tmp/project",
            "mcpServers": []
        })));
        assert!(matches!(err, Err(V1Error::Cancelled)));
    }

    #[test]
    fn encode_prompt_response_uses_stop_reason() {
        let message = encode_prompt_response(JsonRpcId::Number(1), StopReason::EndTurn);
        assert_eq!(
            serde_json::to_string(&message).expect("ser"),
            r#"{"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}"#
        );
    }

    #[test]
    fn workspace_patch_staged_maps_to_diff_session_update() {
        let session_id = SessionId::new();
        let mapped = map_kernel_event(&envelope(
            EventKind::WorkspacePatchStaged,
            session_id,
            serde_json::json!({
                "call_id": "c9",
                "path": "src/lib.rs",
                "diff": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n"
            }),
        ))
        .expect("mapped");
        match mapped {
            MappedEvent::SessionUpdate(notification) => {
                assert_eq!(notification.session_id(), session_id);
                match notification.update() {
                    SessionUpdate::ToolCallUpdate {
                        tool_call_id,
                        status,
                        title,
                        diff,
                    } => {
                        assert_eq!(tool_call_id, "c9");
                        assert_eq!(*status, None);
                        assert_eq!(title.as_deref(), Some("src/lib.rs"));
                        let diff = diff.as_ref().expect("diff");
                        assert_eq!(diff.path(), "src/lib.rs");
                        assert!(diff.unified().starts_with("--- a/"));
                    }
                    other => panic!("expected tool-call update, got {other:?}"),
                }
                assert_eq!(
                    notification.diff().expect("diff accessor").path(),
                    "src/lib.rs"
                );
            }
            other => panic!("expected session update, got {other:?}"),
        }
    }

    #[test]
    fn patch_diff_derives_stable_call_id_from_path() {
        let mapped = map_kernel_event(&envelope(
            EventKind::WorkspaceTransactionCommitted,
            SessionId::new(),
            serde_json::json!({"file": "crates/tui/src/lib.rs", "patch": "@@ -2 +2 @@"}),
        ))
        .expect("mapped");
        match mapped {
            MappedEvent::SessionUpdate(notification) => match notification.update() {
                SessionUpdate::ToolCallUpdate {
                    tool_call_id,
                    status: None,
                    ..
                } => assert_eq!(tool_call_id, "edit:crates/tui/src/lib.rs"),
                other => panic!("expected edit update, got {other:?}"),
            },
            other => panic!("expected session update, got {other:?}"),
        }
    }

    #[test]
    fn oversized_patch_diff_is_truncated_bounded() {
        let huge = "x".repeat(MAX_DIFF_BYTES * 4);
        let mapped = map_kernel_event(&envelope(
            EventKind::WorkspacePatchStaged,
            SessionId::new(),
            serde_json::json!({"path": "big.txt", "diff": huge}),
        ))
        .expect("mapped");
        match mapped {
            MappedEvent::SessionUpdate(notification) => {
                let diff = notification.diff().expect("diff");
                assert_eq!(diff.unified().len(), MAX_DIFF_BYTES);
            }
            other => panic!("expected session update, got {other:?}"),
        }
    }

    #[test]
    fn malformed_patch_payload_maps_to_none() {
        for payload in [
            serde_json::json!({"path": "a.rs"}),
            serde_json::json!({"diff": "@@ -1 +1 @@"}),
            serde_json::json!({"path": "", "diff": "@@"}),
            serde_json::json!({"path": "a.rs", "diff": ""}),
        ] {
            assert!(
                map_kernel_event(&envelope(
                    EventKind::WorkspacePatchStaged,
                    SessionId::new(),
                    payload
                ))
                .is_none(),
                "payload without path+diff must not map"
            );
        }
    }

    /// Builds a string one byte longer than `max`, with a 2-byte UTF-8
    /// character's encoding straddling byte offset `max` exactly — the
    /// specific shape a length check (`len() > max`) does not protect
    /// against, since `String::truncate(max)` panics unless `max` is itself
    /// a char boundary.
    fn straddling_boundary(max: usize) -> String {
        let mut s = "a".repeat(max - 1);
        s.push('é');
        assert_eq!(s.len(), max + 1);
        assert!(
            !s.is_char_boundary(max),
            "test fixture must actually straddle the cut"
        );
        s
    }

    #[test]
    fn truncate_to_char_boundary_never_panics_on_a_straddling_cut() {
        let mut s = straddling_boundary(MAX_TOOL_TITLE_BYTES);
        truncate_to_char_boundary(&mut s, MAX_TOOL_TITLE_BYTES);
        assert!(s.len() <= MAX_TOOL_TITLE_BYTES);
        assert_eq!(s, "a".repeat(MAX_TOOL_TITLE_BYTES - 1));
    }

    #[test]
    fn tool_title_does_not_panic_on_a_straddling_multibyte_cut() {
        let payload = serde_json::json!({"tool": straddling_boundary(MAX_TOOL_TITLE_BYTES)});
        let title = tool_title(&payload).expect("title");
        assert!(title.len() <= MAX_TOOL_TITLE_BYTES);
    }

    #[test]
    fn text_from_payload_does_not_panic_on_a_straddling_multibyte_cut() {
        let payload = serde_json::json!({"text": straddling_boundary(MAX_UPDATE_TEXT_BYTES)});
        let text = text_from_payload(&payload).expect("text");
        assert!(text.len() <= MAX_UPDATE_TEXT_BYTES);
    }

    #[test]
    fn file_edit_update_does_not_panic_on_a_straddling_multibyte_diff() {
        let payload = serde_json::json!({
            "path": "a.rs",
            "diff": straddling_boundary(MAX_DIFF_BYTES),
        });
        let mapped = map_kernel_event(&envelope(
            EventKind::WorkspacePatchStaged,
            SessionId::new(),
            payload,
        ))
        .expect("mapped");
        let MappedEvent::SessionUpdate(SessionUpdateNotification {
            update:
                SessionUpdate::ToolCallUpdate {
                    diff: Some(diff), ..
                },
            ..
        }) = mapped
        else {
            panic!("expected a tool-call diff update");
        };
        assert!(diff.unified().len() <= MAX_DIFF_BYTES);
    }

    #[test]
    fn file_diff_new_does_not_panic_on_a_straddling_multibyte_unified_body() {
        let diff = FileDiff::new("a.rs", straddling_boundary(MAX_DIFF_BYTES)).expect("diff");
        assert!(diff.unified().len() <= MAX_DIFF_BYTES);
    }
}
