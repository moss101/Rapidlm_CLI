//! Route validated invocations through the Capability Broker to executors.
//!
//! Privileged tools cannot be registered without a capability-descriptor
//! callback. Dispatch evaluates that descriptor, authorizes via the broker,
//! re-validates the lease, then invokes the executor. Large results become
//! artifact refs plus a bounded excerpt. Threats: `T-001`, `T-004`, `T-007`,
//! `T-012`, `T-017`, `T-020`.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Mutex;
use std::time::Instant;

use capability_broker::{
    ActionRequest, CanonicalAction, Capability, CapabilityError, CapabilityLease, DenyReason,
    FilesystemScope, LeaseUseGuard, McpScope, MobileScope, Origin, PluginScope, PolicyError,
    PrincipalRef, ProcessScope, ResourceDescriptor,
};
use protocol::{AgentId, ArtifactId, ArtifactRef, ErrorCode, RedactionClass, SessionId};
use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};
use serde_json::{Map, Value};

use crate::schema::{DENIED_ARGUMENT_NAMES, GatewayTool, V1_TOOL_COUNT, denied_argument_name};
use crate::validate::{CancellationToken, CanonicalToolInvocation, TOOL_INVOCATION_SCHEMA};

/// Wire schema version for [`ToolResultEnvelope`].
pub const TOOL_RESULT_SCHEMA: u16 = 1;

/// Maximum UTF-8 bytes kept in the model-visible summary.
pub const MAX_SUMMARY_BYTES: usize = 512;

/// Maximum serialized JSON bytes kept inline in `data`.
pub const MAX_INLINE_RESULT_BYTES: usize = 16 * 1024;

/// Maximum UTF-8 bytes of a truncated excerpt.
pub const MAX_EXCERPT_BYTES: usize = 2_048;

/// Maximum bytes an executor result may spill to an artifact.
pub const MAX_RESULT_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;

/// Privilege tokens that must never appear as fields or values in model-visible output.
const PRIVILEGE_VALUE_TOKENS: &[&str] = &["capability_lease", "secret", "token"];

/// Unambiguous privilege handles. Safer than generic `secret`/`token` in data values.
const UNAMBIGUOUS_PRIVILEGE_TOKENS: &[&str] =
    &["capability_lease", "capability_token", "secret_plaintext"];

/// Maps a validated invocation onto a broker action. Required for privileged tools.
pub type CapabilityDescriptor =
    fn(&CanonicalToolInvocation, &DispatchActor) -> Result<CapabilityNeed, DispatchError>;

/// Principal/session that owns a dispatch. Not a capability grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchActor {
    principal: PrincipalRef,
    session_id: SessionId,
    agent_id: AgentId,
}

/// Capability/resource/action the descriptor asks the broker to evaluate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityNeed {
    capability: Capability,
    resource: ResourceDescriptor,
    action: CanonicalAction,
    reason: String,
}

/// Broker outcome. Ask and deny never yield a lease.
#[derive(Debug)]
pub enum Authorization {
    Lease(Box<CapabilityLease>),
    ApprovalRequired,
    Denied { reason: DenyReason },
}

/// Bounded model-visible tool result. Matches `api-contracts/tool-gateway-api.md`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResultEnvelope {
    schema: u16,
    call_id: String,
    status: ToolResultStatus,
    summary: String,
    data: Value,
    artifacts: Vec<ArtifactRef>,
    truncated: bool,
    continuation: Option<ToolContinuation>,
}

/// Universal tool-outcome status (FR-HARNESS-008).
///
/// Maps the arch's six outcomes: `Ok` (success), `Recovered`, `Partial`,
/// `Retryable`, `Denied`, `Failed`. `ApprovalRequired` is an additional
/// terminal wait state. Unknown strings cannot be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ToolResultStatus {
    Ok,
    Recovered,
    Partial,
    Retryable,
    Denied,
    ApprovalRequired,
    Failed,
}

/// Artifact cursor for a truncated result. Not a capability token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolContinuation {
    artifact_id: ArtifactId,
}

/// Unbounded executor payload. Dispatch bounds it before model exposure.
#[derive(Clone, Debug)]
pub struct RawToolOutput {
    summary: String,
    data: Value,
    media_type: String,
    redaction: RedactionClass,
}

/// Typed dispatch failure. Display never echoes arguments, output, or leases.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DispatchError {
    Cancelled,
    UnregisteredTool,
    AlreadyRegistered,
    MissingCapabilityDescriptor,
    DescriptorFailed,
    BrokerUnavailable,
    PolicyDenied,
    ApprovalRequired,
    LeaseInvalid,
    ExecutorFailed,
    PrivilegeLeak,
    OversizedResult,
    InvalidActor,
    ArtifactUnavailable,
}

/// Registry of catalog tools. Privileged slots require a descriptor callback.
pub struct ToolRegistry {
    slots: [Option<RegisteredTool>; V1_TOOL_COUNT],
}

struct RegisteredTool {
    descriptor: CapabilityDescriptor,
    executor: Box<dyn ToolExecutor>,
}

/// Authorize, lease, and re-validate on the privileged path.
pub trait CapabilityBroker {
    fn authorize(
        &self,
        request: &ActionRequest,
        now: Instant,
        cancel: &CancellationToken,
    ) -> Result<Authorization, DispatchError>;

    fn validate_use(
        &self,
        lease: &CapabilityLease,
        actual: &CanonicalAction,
        now: Instant,
        cancel: &CancellationToken,
    ) -> Result<LeaseUseGuard, DispatchError>;
}

/// Registered executor. Receives a consumed-once [`LeaseUseGuard`].
pub trait ToolExecutor: Send + Sync {
    fn execute(
        &self,
        invocation: &CanonicalToolInvocation,
        actor: &DispatchActor,
        guard: LeaseUseGuard,
        cancel: &CancellationToken,
    ) -> Result<RawToolOutput, DispatchError>;
}

/// Content-addressed sink for oversized tool output.
pub trait ArtifactSink {
    fn put(
        &self,
        bytes: &[u8],
        media_type: &str,
        redaction: RedactionClass,
        cancel: &CancellationToken,
    ) -> Result<ArtifactRef, DispatchError>;
}

/// In-process artifact sink for tests and composition without a disk store.
#[derive(Debug, Default)]
pub struct MemoryArtifactSink {
    blobs: Mutex<HashMap<ArtifactId, Vec<u8>>>,
}

/// Dispatcher holding the registry, broker, and artifact sink.
pub struct ToolDispatcher<'a> {
    registry: &'a ToolRegistry,
    broker: &'a dyn CapabilityBroker,
    artifacts: &'a dyn ArtifactSink,
}

impl DispatchActor {
    pub fn new(principal: PrincipalRef, session_id: SessionId, agent_id: AgentId) -> Self {
        Self {
            principal,
            session_id,
            agent_id,
        }
    }

    pub fn parse(
        principal: &str,
        session_id: SessionId,
        agent_id: AgentId,
    ) -> Result<Self, DispatchError> {
        let principal = PrincipalRef::parse(principal).map_err(|_| DispatchError::InvalidActor)?;
        Ok(Self::new(principal, session_id, agent_id))
    }

    pub fn principal(&self) -> &PrincipalRef {
        &self.principal
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }
}

impl CapabilityNeed {
    pub fn new(
        capability: Capability,
        resource: ResourceDescriptor,
        action: CanonicalAction,
        reason: impl Into<String>,
    ) -> Result<Self, DispatchError> {
        if capability.compatible_with(&resource).is_err() {
            return Err(DispatchError::DescriptorFailed);
        }
        match &action {
            CanonicalAction::Resource {
                capability: action_cap,
                resource: action_res,
            } if *action_cap == capability && action_res == &resource => {}
            CanonicalAction::Resource { .. } => return Err(DispatchError::DescriptorFailed),
            CanonicalAction::Command(_)
            | CanonicalAction::Filesystem(_)
            | CanonicalAction::Network(_) => {
                if capability.compatible_with(&resource).is_err() {
                    return Err(DispatchError::DescriptorFailed);
                }
            }
        }
        let reason = reason.into();
        if reason.is_empty() || reason.len() > capability_broker::MAX_REASON_BYTES {
            return Err(DispatchError::DescriptorFailed);
        }
        Ok(Self {
            capability,
            resource,
            action,
            reason,
        })
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }

    pub fn resource(&self) -> &ResourceDescriptor {
        &self.resource
    }

    pub fn action(&self) -> &CanonicalAction {
        &self.action
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    fn into_request(self, actor: &DispatchActor) -> Result<ActionRequest, DispatchError> {
        ActionRequest::new(
            actor.principal().clone(),
            actor.session_id(),
            self.capability,
            self.resource,
            self.action,
            self.reason,
        )
        .map_err(|_| DispatchError::DescriptorFailed)
    }
}

impl ToolResultStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Recovered => "recovered",
            Self::Partial => "partial",
            Self::Retryable => "retryable",
            Self::Denied => "denied",
            Self::ApprovalRequired => "approval_required",
            Self::Failed => "failed",
        }
    }

    /// Whether the outcome is the success-family and cannot be retried.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Ok | Self::Denied | Self::ApprovalRequired | Self::Failed
        )
    }

    /// Whether a machine-readable continuation/recovery hint is required.
    pub const fn needs_continuation(self) -> bool {
        matches!(self, Self::Partial | Self::Retryable)
    }
}

impl ToolResultEnvelope {
    pub const fn schema(&self) -> u16 {
        self.schema
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub const fn status(&self) -> ToolResultStatus {
        self.status
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn data(&self) -> &Value {
        &self.data
    }

    pub fn artifacts(&self) -> &[ArtifactRef] {
        &self.artifacts
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn continuation(&self) -> Option<&ToolContinuation> {
        self.continuation.as_ref()
    }

    /// Attach a machine-readable continuation/recovery cursor.
    pub fn with_continuation(mut self, continuation: ToolContinuation) -> Self {
        self.continuation = Some(continuation);
        self
    }

    /// Replace the status. `Partial`/`Retryable` require a continuation hint.
    pub fn with_status(mut self, status: ToolResultStatus) -> Self {
        self.status = status;
        self
    }
}

impl ToolContinuation {
    pub const fn artifact_id(&self) -> ArtifactId {
        self.artifact_id
    }
}

impl RawToolOutput {
    pub fn new(
        summary: impl Into<String>,
        data: Value,
        media_type: impl Into<String>,
        redaction: RedactionClass,
    ) -> Self {
        Self {
            summary: summary.into(),
            data,
            media_type: media_type.into(),
            redaction,
        }
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn data(&self) -> &Value {
        &self.data
    }
}

impl DispatchError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "tool dispatch cancelled",
            Self::UnregisteredTool => "tool is not registered",
            Self::AlreadyRegistered => "tool is already registered",
            Self::MissingCapabilityDescriptor => {
                "privileged executor requires a capability descriptor"
            }
            Self::DescriptorFailed => "capability descriptor failed",
            Self::BrokerUnavailable => "capability broker unavailable",
            Self::PolicyDenied => "policy denied",
            Self::ApprovalRequired => "policy approval required",
            Self::LeaseInvalid => "capability lease is invalid",
            Self::ExecutorFailed => "tool executor failed",
            Self::PrivilegeLeak => "executor output contained a privilege handle",
            Self::OversizedResult => "tool result exceeds the bound",
            Self::InvalidActor => "invalid dispatch actor",
            Self::ArtifactUnavailable => "artifact sink unavailable",
        }
    }

    pub const fn error_code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::UnregisteredTool
            | Self::AlreadyRegistered
            | Self::MissingCapabilityDescriptor
            | Self::DescriptorFailed
            | Self::InvalidActor => Some(ErrorCode::ToolInvalidArguments),
            Self::PolicyDenied | Self::BrokerUnavailable => Some(ErrorCode::PolicyDenied),
            Self::ApprovalRequired => Some(ErrorCode::PolicyApprovalRequired),
            Self::LeaseInvalid => Some(ErrorCode::PolicyLeaseInvalid),
            Self::ExecutorFailed | Self::PrivilegeLeak | Self::OversizedResult => {
                Some(ErrorCode::ToolInvalidArguments)
            }
            Self::ArtifactUnavailable => Some(ErrorCode::InternalUnexpected),
        }
    }
}

impl fmt::Display for ToolResultStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for DispatchError {}

impl From<PolicyError> for DispatchError {
    fn from(err: PolicyError) -> Self {
        match err {
            PolicyError::Cancelled => Self::Cancelled,
            PolicyError::Unavailable => Self::BrokerUnavailable,
            PolicyError::Expired
            | PolicyError::WrongAction
            | PolicyError::InvalidMac
            | PolicyError::InvalidLease
            | PolicyError::PolicyRevisionMismatch
            | PolicyError::UsesExhausted => Self::LeaseInvalid,
        }
    }
}

impl Serialize for ToolResultStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Serialize for ToolContinuation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ToolContinuation", 1)?;
        state.serialize_field("artifact_id", &self.artifact_id)?;
        state.end()
    }
}

impl Serialize for ToolResultEnvelope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ToolResultEnvelope", 8)?;
        state.serialize_field("schema", &self.schema)?;
        state.serialize_field("call_id", &self.call_id)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("summary", &self.summary)?;
        state.serialize_field("data", &self.data)?;
        state.serialize_field("artifacts", &self.artifacts)?;
        state.serialize_field("truncated", &self.truncated)?;
        state.serialize_field("continuation", &self.continuation)?;
        state.end()
    }
}

/// Every v1 catalog tool has a privilege boundary and requires a descriptor.
pub const fn is_privileged_tool(tool: GatewayTool) -> bool {
    match tool {
        GatewayTool::RepoSearch
        | GatewayTool::RepoRead
        | GatewayTool::WorkspacePatch
        | GatewayTool::WorkspaceStatus
        | GatewayTool::ShellExec
        | GatewayTool::AgentSpawn
        | GatewayTool::AgentResult
        | GatewayTool::GoalUpdate
        | GatewayTool::BrowserAct
        | GatewayTool::MobileAct
        | GatewayTool::ExternalCall
        | GatewayTool::EvidenceRecord => true,
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            slots: [const { None }; V1_TOOL_COUNT],
        }
    }

    /// Register a privileged executor. The descriptor callback is mandatory.
    pub fn register_privileged(
        &mut self,
        tool: GatewayTool,
        descriptor: CapabilityDescriptor,
        executor: Box<dyn ToolExecutor>,
    ) -> Result<(), DispatchError> {
        self.register(tool, Some(descriptor), executor)
    }

    /// Register an executor. Privileged tools fail closed without a descriptor.
    pub fn register(
        &mut self,
        tool: GatewayTool,
        descriptor: Option<CapabilityDescriptor>,
        executor: Box<dyn ToolExecutor>,
    ) -> Result<(), DispatchError> {
        if is_privileged_tool(tool) && descriptor.is_none() {
            return Err(DispatchError::MissingCapabilityDescriptor);
        }
        let Some(descriptor) = descriptor else {
            return Err(DispatchError::MissingCapabilityDescriptor);
        };
        let index = tool_index(tool).ok_or(DispatchError::UnregisteredTool)?;
        if self.slots[index].is_some() {
            return Err(DispatchError::AlreadyRegistered);
        }
        self.slots[index] = Some(RegisteredTool {
            descriptor,
            executor,
        });
        Ok(())
    }

    fn get(&self, tool: GatewayTool) -> Result<&RegisteredTool, DispatchError> {
        let index = tool_index(tool).ok_or(DispatchError::UnregisteredTool)?;
        self.slots[index]
            .as_ref()
            .ok_or(DispatchError::UnregisteredTool)
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryArtifactSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &ArtifactId) -> Result<Option<Vec<u8>>, DispatchError> {
        let blobs = self
            .blobs
            .lock()
            .map_err(|_| DispatchError::ArtifactUnavailable)?;
        Ok(blobs.get(id).cloned())
    }
}

impl ArtifactSink for MemoryArtifactSink {
    fn put(
        &self,
        bytes: &[u8],
        media_type: &str,
        redaction: RedactionClass,
        cancel: &CancellationToken,
    ) -> Result<ArtifactRef, DispatchError> {
        if cancel.is_cancelled() {
            return Err(DispatchError::Cancelled);
        }
        if bytes.len() > MAX_RESULT_ARTIFACT_BYTES {
            return Err(DispatchError::OversizedResult);
        }
        if media_type.is_empty() {
            return Err(DispatchError::ExecutorFailed);
        }
        let id = ArtifactId::from_bytes(bytes);
        let mut blobs = self
            .blobs
            .lock()
            .map_err(|_| DispatchError::ArtifactUnavailable)?;
        blobs.insert(id, bytes.to_vec());
        Ok(ArtifactRef::new(
            id,
            media_type,
            bytes.len() as u64,
            redaction,
        ))
    }
}

impl<'a> ToolDispatcher<'a> {
    pub fn new(
        registry: &'a ToolRegistry,
        broker: &'a dyn CapabilityBroker,
        artifacts: &'a dyn ArtifactSink,
    ) -> Self {
        Self {
            registry,
            broker,
            artifacts,
        }
    }

    /// `dispatch(invocation, actor) -> ToolResultEnvelope`. Privileged tools
    /// always take the capability path.
    pub fn dispatch(
        &self,
        invocation: &CanonicalToolInvocation,
        actor: &DispatchActor,
        now: Instant,
        cancel: &CancellationToken,
    ) -> Result<ToolResultEnvelope, DispatchError> {
        dispatch(
            self.registry,
            self.broker,
            self.artifacts,
            invocation,
            actor,
            now,
            cancel,
        )
    }
}

/// Route a validated invocation through the broker to a registered executor.
pub fn dispatch(
    registry: &ToolRegistry,
    broker: &dyn CapabilityBroker,
    artifacts: &dyn ArtifactSink,
    invocation: &CanonicalToolInvocation,
    actor: &DispatchActor,
    now: Instant,
    cancel: &CancellationToken,
) -> Result<ToolResultEnvelope, DispatchError> {
    if cancel.is_cancelled() {
        return Err(DispatchError::Cancelled);
    }
    if invocation.schema() != TOOL_INVOCATION_SCHEMA {
        return Err(DispatchError::UnregisteredTool);
    }

    let registered = registry.get(invocation.tool())?;
    if is_privileged_tool(invocation.tool()) {
        let need = (registered.descriptor)(invocation, actor)?;
        let action = need.action().clone();
        let request = need.into_request(actor)?;
        if cancel.is_cancelled() {
            return Err(DispatchError::Cancelled);
        }
        match broker.authorize(&request, now, cancel)? {
            Authorization::Denied { .. } => {
                return Ok(policy_envelope(
                    invocation.call_id(),
                    ToolResultStatus::Denied,
                    DispatchError::PolicyDenied.as_str(),
                ));
            }
            Authorization::ApprovalRequired => {
                return Ok(policy_envelope(
                    invocation.call_id(),
                    ToolResultStatus::ApprovalRequired,
                    DispatchError::ApprovalRequired.as_str(),
                ));
            }
            Authorization::Lease(lease) => {
                if cancel.is_cancelled() {
                    return Err(DispatchError::Cancelled);
                }
                if !lease_binds_invocation(lease.as_ref(), &action) {
                    return Err(DispatchError::LeaseInvalid);
                }
                let guard = broker.validate_use(lease.as_ref(), &action, now, cancel)?;
                if cancel.is_cancelled() {
                    return Err(DispatchError::Cancelled);
                }
                let output = registered
                    .executor
                    .execute(invocation, actor, guard, cancel)?;
                return bound_output(invocation.call_id(), output, artifacts, cancel);
            }
        }
    }

    Err(DispatchError::MissingCapabilityDescriptor)
}

/// Default descriptor: derive a concrete capability from validated arguments.
///
/// Missing or ambiguous scope fails closed. No catalog tool invents a broader grant.
pub fn describe_capability(
    invocation: &CanonicalToolInvocation,
    _actor: &DispatchActor,
) -> Result<CapabilityNeed, DispatchError> {
    let arguments = invocation.arguments();
    let (capability, resource) = match invocation.tool() {
        GatewayTool::RepoSearch | GatewayTool::RepoRead | GatewayTool::WorkspaceStatus => {
            let path = arg_str(arguments, "path").ok_or(DispatchError::DescriptorFailed)?;
            let resource = ResourceDescriptor::Filesystem(fs_repo(path)?);
            (Capability::FsRead, resource)
        }
        GatewayTool::WorkspacePatch => {
            let path = unique_patch_path(arguments)?;
            let resource = ResourceDescriptor::Filesystem(fs_repo(path)?);
            (Capability::FsWrite, resource)
        }
        GatewayTool::ShellExec => {
            let resource = describe_proc_from_argv(arguments)?;
            (Capability::ProcExec, resource)
        }
        GatewayTool::BrowserAct => {
            let action = arg_str(arguments, "action").ok_or(DispatchError::DescriptorFailed)?;
            if action != "navigate" {
                return Err(DispatchError::DescriptorFailed);
            }
            let url = arg_str(arguments, "url").ok_or(DispatchError::DescriptorFailed)?;
            let origin = Origin::parse(url).map_err(map_capability_err)?;
            let resource =
                ResourceDescriptor::Browser(capability_broker::BrowserScope::navigate(origin));
            (Capability::BrowserNavigate, resource)
        }
        GatewayTool::MobileAct => {
            let device = arg_str(arguments, "device_id").ok_or(DispatchError::DescriptorFailed)?;
            let resource =
                ResourceDescriptor::Mobile(MobileScope::new(device).map_err(map_capability_err)?);
            (Capability::MobileControl, resource)
        }
        GatewayTool::ExternalCall => match arg_str(arguments, "kind") {
            Some("mcp") => {
                let server = arg_str(arguments, "server").ok_or(DispatchError::DescriptorFailed)?;
                let tool = arg_str(arguments, "tool").ok_or(DispatchError::DescriptorFailed)?;
                let resource = ResourceDescriptor::Mcp(
                    McpScope::new(server, tool).map_err(map_capability_err)?,
                );
                (Capability::McpInvoke, resource)
            }
            Some("plugin") => {
                let plugin = arg_str(arguments, "plugin").ok_or(DispatchError::DescriptorFailed)?;
                let tool = arg_str(arguments, "tool").ok_or(DispatchError::DescriptorFailed)?;
                let resource = ResourceDescriptor::Plugin(
                    PluginScope::new(plugin, tool).map_err(map_capability_err)?,
                );
                (Capability::PluginInvoke, resource)
            }
            _ => return Err(DispatchError::DescriptorFailed),
        },
        GatewayTool::AgentSpawn
        | GatewayTool::AgentResult
        | GatewayTool::GoalUpdate
        | GatewayTool::EvidenceRecord => return Err(DispatchError::DescriptorFailed),
    };
    let action = CanonicalAction::Resource {
        capability,
        resource: resource.clone(),
    };
    CapabilityNeed::new(capability, resource, action, invocation.tool().as_str())
}

fn map_capability_err(_err: CapabilityError) -> DispatchError {
    DispatchError::DescriptorFailed
}

fn fs_repo(path: &str) -> Result<FilesystemScope, DispatchError> {
    FilesystemScope::repo(path).map_err(map_capability_err)
}

fn arg_str<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(Value::as_str)
}

/// Exactly one named repo path across all patch ops. Multiple paths are ambiguous.
fn unique_patch_path(arguments: &Value) -> Result<&str, DispatchError> {
    let ops = arguments
        .get("ops")
        .and_then(Value::as_array)
        .ok_or(DispatchError::DescriptorFailed)?;
    let mut unique: Option<&str> = None;
    for op in ops {
        for key in ["path", "from", "to"] {
            let Some(path) = op.get(key).and_then(Value::as_str) else {
                continue;
            };
            match unique {
                None => unique = Some(path),
                Some(seen) if seen == path => {}
                Some(_) => return Err(DispatchError::DescriptorFailed),
            }
        }
    }
    unique.ok_or(DispatchError::DescriptorFailed)
}

fn tool_index(tool: GatewayTool) -> Option<usize> {
    GatewayTool::ALL.iter().position(|item| *item == tool)
}

/// T-004: a lease bound to a different action than the invocation is invalid.
fn lease_binds_invocation(lease: &CapabilityLease, actual: &CanonicalAction) -> bool {
    match actual {
        CanonicalAction::Resource {
            capability,
            resource,
        } => lease.capability() == *capability && lease.resource() == resource,
        CanonicalAction::Command(_)
        | CanonicalAction::Filesystem(_)
        | CanonicalAction::Network(_) => true,
    }
}

fn policy_envelope(call_id: &str, status: ToolResultStatus, summary: &str) -> ToolResultEnvelope {
    ToolResultEnvelope {
        schema: TOOL_RESULT_SCHEMA,
        call_id: call_id.to_owned(),
        status,
        summary: bound_text(summary, MAX_SUMMARY_BYTES),
        data: Value::Object(Map::new()),
        artifacts: Vec::new(),
        truncated: false,
        continuation: None,
    }
}

fn bound_output(
    call_id: &str,
    output: RawToolOutput,
    artifacts: &dyn ArtifactSink,
    cancel: &CancellationToken,
) -> Result<ToolResultEnvelope, DispatchError> {
    if cancel.is_cancelled() {
        return Err(DispatchError::Cancelled);
    }
    reject_privilege_leak(&output.summary, &output.data)?;
    let summary = bound_text(&output.summary, MAX_SUMMARY_BYTES);
    reject_privilege_text(&summary)?;
    let encoded = serde_json::to_vec(&output.data).map_err(|_| DispatchError::ExecutorFailed)?;
    if encoded.len() > MAX_RESULT_ARTIFACT_BYTES {
        return Err(DispatchError::OversizedResult);
    }
    if encoded.len() <= MAX_INLINE_RESULT_BYTES {
        return Ok(ToolResultEnvelope {
            schema: TOOL_RESULT_SCHEMA,
            call_id: call_id.to_owned(),
            status: ToolResultStatus::Ok,
            summary,
            data: output.data,
            artifacts: Vec::new(),
            truncated: false,
            continuation: None,
        });
    }
    let refer = artifacts.put(&encoded, &output.media_type, output.redaction, cancel)?;
    let excerpt = bound_text(&String::from_utf8_lossy(&encoded), MAX_EXCERPT_BYTES);
    reject_privilege_text(&excerpt)?;
    let mut data = Map::new();
    data.insert("excerpt".to_owned(), Value::String(excerpt));
    let continuation = ToolContinuation {
        artifact_id: refer.id,
    };
    Ok(ToolResultEnvelope {
        schema: TOOL_RESULT_SCHEMA,
        call_id: call_id.to_owned(),
        status: ToolResultStatus::Ok,
        summary,
        data: Value::Object(data),
        artifacts: vec![refer],
        truncated: true,
        continuation: Some(continuation),
    })
}

fn bound_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn reject_privilege_leak(summary: &str, data: &Value) -> Result<(), DispatchError> {
    reject_privilege_text(summary)?;
    if let Ok(parsed) = serde_json::from_str::<Value>(summary) {
        reject_denied_output(&parsed)?;
        reject_privilege_values(&parsed)?;
    }
    reject_denied_output(data)?;
    reject_privilege_values(data)?;
    Ok(())
}

fn reject_denied_output(value: &Value) -> Result<(), DispatchError> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if is_denied_key(key) {
                    return Err(DispatchError::PrivilegeLeak);
                }
                reject_denied_output(child)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                reject_denied_output(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn reject_privilege_values(value: &Value) -> Result<(), DispatchError> {
    match value {
        Value::Object(map) => {
            for child in map.values() {
                reject_privilege_values(child)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                reject_privilege_values(item)?;
            }
        }
        Value::String(text)
            if contains_named_privilege_token(text, UNAMBIGUOUS_PRIVILEGE_TOKENS) =>
        {
            return Err(DispatchError::PrivilegeLeak);
        }
        _ => {}
    }
    Ok(())
}

fn reject_privilege_text(text: &str) -> Result<(), DispatchError> {
    if contains_named_privilege_token(text, PRIVILEGE_VALUE_TOKENS)
        || contains_named_privilege_token(text, DENIED_ARGUMENT_NAMES)
    {
        return Err(DispatchError::PrivilegeLeak);
    }
    Ok(())
}

fn contains_named_privilege_token(text: &str, names: &[&str]) -> bool {
    let lowered = text.to_ascii_lowercase();
    if names.iter().any(|name| contains_ident(&lowered, name)) {
        return true;
    }
    let collapsed: String = lowered
        .chars()
        .map(|ch| {
            if matches!(ch, '-' | ' ' | ':') {
                '_'
            } else {
                ch
            }
        })
        .collect();
    collapsed != lowered && names.iter().any(|name| contains_ident(&collapsed, name))
}

fn contains_ident(text: &str, name: &str) -> bool {
    let bytes = text.as_bytes();
    let needle = name.as_bytes();
    if needle.is_empty() || bytes.len() < needle.len() {
        return false;
    }
    let mut index = 0;
    while index + needle.len() <= bytes.len() {
        if &bytes[index..index + needle.len()] == needle {
            let before_ok = index == 0 || !is_ident_byte(bytes[index - 1]);
            let after = index + needle.len();
            let after_ok = after == bytes.len() || !is_ident_byte(bytes[after]);
            if before_ok && after_ok {
                return true;
            }
        }
        index += 1;
    }
    false
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_denied_key(name: &str) -> bool {
    if denied_argument_name(name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    lower != name && denied_argument_name(&lower)
}

fn describe_proc_from_argv(arguments: &Value) -> Result<ResourceDescriptor, DispatchError> {
    if arguments.get("shell") == Some(&Value::Bool(true)) {
        return Ok(ResourceDescriptor::Process(
            ProcessScope::new("shell").map_err(map_capability_err)?,
        ));
    }
    let argv0 = arguments
        .get("argv")
        .and_then(Value::as_array)
        .and_then(|argv| argv.first())
        .and_then(Value::as_str)
        .ok_or(DispatchError::DescriptorFailed)?;
    let name = argv0
        .rsplit(['/', '\\'])
        .next()
        .filter(|part| !part.is_empty() && *part != "shell")
        .ok_or(DispatchError::DescriptorFailed)?;
    Ok(ResourceDescriptor::Process(
        ProcessScope::new(name).map_err(map_capability_err)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use capability_broker::{
        ApprovalChoice, ApprovalResolution, ApprovalScopeId, LeaseIssuer, LeaseValidator,
        PolicyDocument, PolicyRevision, PolicySource, PolicyStack, evaluate, issue,
        request_approval, validate_use,
    };
    use serde_json::json;

    use crate::validate::{TOOL_INVOCATION_SCHEMA, ToolCall, validate};

    const SECRET: &str = "hunter2-capability-lease";
    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-7c1e9b";
    const ARTIFACT: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum BrokerMode {
        Allow,
        Ask,
        Deny,
        Unavailable,
        MismatchedLease,
    }

    struct ScriptedBroker {
        mode: BrokerMode,
        issuer: LeaseIssuer,
        validator: LeaseValidator,
        policies: PolicyStack,
        authorize_calls: AtomicUsize,
        validate_calls: AtomicUsize,
    }

    struct RecordingExecutor {
        ran: AtomicBool,
        output: RawToolOutput,
        fail: bool,
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn broker_live() -> capability_broker::CancellationToken {
        capability_broker::CancellationToken::new()
    }

    fn actor() -> DispatchActor {
        DispatchActor::parse("agent", SessionId::new(), AgentId::new()).expect("actor")
    }

    fn invocation(tool: &str, arguments: Value) -> CanonicalToolInvocation {
        let call = ToolCall::new(TOOL_INVOCATION_SCHEMA, "call_7", tool, arguments);
        validate(&call, &live()).expect("valid invocation")
    }

    fn parse_doc(src: &str, source: PolicySource) -> PolicyDocument {
        PolicyDocument::parse_toml(src, source, &broker_live()).expect("parse")
    }

    fn ask_stack() -> PolicyStack {
        PolicyStack::new([
            parse_doc(
                r#"
[[rules]]
id = "fs-ask"
effect = "ask"
subjects = ["*"]
capability = "fs.read"
"#,
                PolicySource::user("user-policy.toml").expect("user"),
            ),
            parse_doc(
                r#"
[[rules]]
id = "proc-ask"
effect = "ask"
subjects = ["*"]
capability = "proc.exec"
"#,
                PolicySource::user("user-policy.toml").expect("user"),
            ),
        ])
        .expect("stack")
    }

    fn issuer() -> LeaseIssuer {
        LeaseIssuer::from_key([0x11; 32]).expect("issuer")
    }

    fn scripted(mode: BrokerMode) -> ScriptedBroker {
        let policies = ask_stack();
        let issuer = issuer();
        let validator = LeaseValidator::new(
            LeaseIssuer::from_key([0x11; 32]).expect("issuer"),
            PolicyRevision::of_stack(&policies),
        );
        ScriptedBroker {
            mode,
            issuer,
            validator,
            policies,
            authorize_calls: AtomicUsize::new(0),
            validate_calls: AtomicUsize::new(0),
        }
    }

    fn issue_lease(
        broker: &ScriptedBroker,
        request: &ActionRequest,
        now: Instant,
    ) -> CapabilityLease {
        let decision = evaluate(&broker.policies, request, &broker_live()).expect("evaluate");
        let approval = request_approval(request, &decision, now, &broker_live()).expect("approval");
        let approved = match approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                request,
                now,
                &broker_live(),
            )
            .expect("resolve")
        {
            ApprovalResolution::Approved(approved) => approved,
            ApprovalResolution::Denied => panic!("expected approved"),
        };
        issue(
            &broker.issuer,
            &approved,
            &broker.policies,
            now,
            &broker_live(),
        )
        .expect("issue")
    }

    fn mismatched_request(request: &ActionRequest) -> ActionRequest {
        let other_resource =
            ResourceDescriptor::Filesystem(fs_repo("src/other.rs").expect("other path"));
        ActionRequest::new(
            request.principal().clone(),
            request.session_id(),
            Capability::FsRead,
            other_resource.clone(),
            CanonicalAction::Resource {
                capability: Capability::FsRead,
                resource: other_resource,
            },
            request.reason(),
        )
        .expect("mismatched request")
    }

    impl CapabilityBroker for ScriptedBroker {
        fn authorize(
            &self,
            request: &ActionRequest,
            now: Instant,
            cancel: &CancellationToken,
        ) -> Result<Authorization, DispatchError> {
            self.authorize_calls.fetch_add(1, Ordering::SeqCst);
            if cancel.is_cancelled() {
                return Err(DispatchError::Cancelled);
            }
            match self.mode {
                BrokerMode::Unavailable => Err(DispatchError::BrokerUnavailable),
                BrokerMode::Deny => Ok(Authorization::Denied {
                    reason: DenyReason::DefaultDeny,
                }),
                BrokerMode::Ask => Ok(Authorization::ApprovalRequired),
                BrokerMode::Allow => Ok(Authorization::Lease(Box::new(issue_lease(
                    self, request, now,
                )))),
                BrokerMode::MismatchedLease => {
                    let other = mismatched_request(request);
                    Ok(Authorization::Lease(Box::new(issue_lease(
                        self, &other, now,
                    ))))
                }
            }
        }

        fn validate_use(
            &self,
            lease: &CapabilityLease,
            actual: &CanonicalAction,
            now: Instant,
            cancel: &CancellationToken,
        ) -> Result<LeaseUseGuard, DispatchError> {
            self.validate_calls.fetch_add(1, Ordering::SeqCst);
            if cancel.is_cancelled() {
                return Err(DispatchError::Cancelled);
            }
            let broker_cancel = broker_live();
            Ok(validate_use(
                &self.validator,
                lease,
                actual,
                now,
                &broker_cancel,
            )?)
        }
    }

    impl ToolExecutor for RecordingExecutor {
        fn execute(
            &self,
            _invocation: &CanonicalToolInvocation,
            _actor: &DispatchActor,
            guard: LeaseUseGuard,
            cancel: &CancellationToken,
        ) -> Result<RawToolOutput, DispatchError> {
            if cancel.is_cancelled() {
                return Err(DispatchError::Cancelled);
            }
            let _consumed = guard.consume();
            self.ran.store(true, Ordering::SeqCst);
            if self.fail {
                return Err(DispatchError::ExecutorFailed);
            }
            Ok(self.output.clone())
        }
    }

    fn recorder(data: Value) -> Arc<RecordingExecutor> {
        Arc::new(RecordingExecutor {
            ran: AtomicBool::new(false),
            output: RawToolOutput::new("ok", data, "application/json", RedactionClass::Project),
            fail: false,
        })
    }

    fn recorder_with(summary: &str, data: Value) -> Arc<RecordingExecutor> {
        Arc::new(RecordingExecutor {
            ran: AtomicBool::new(false),
            output: RawToolOutput::new(summary, data, "application/json", RedactionClass::Project),
            fail: false,
        })
    }

    fn fs_read_descriptor(
        invocation: &CanonicalToolInvocation,
        _actor: &DispatchActor,
    ) -> Result<CapabilityNeed, DispatchError> {
        let path = invocation
            .arguments()
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("crates/tool-gateway/src/lib.rs");
        let resource = ResourceDescriptor::Filesystem(fs_repo(path)?);
        let capability = Capability::FsRead;
        CapabilityNeed::new(
            capability,
            resource.clone(),
            CanonicalAction::Resource {
                capability,
                resource,
            },
            invocation.tool().as_str(),
        )
    }

    fn register_read(
        registry: &mut ToolRegistry,
        exec: Arc<RecordingExecutor>,
    ) -> Result<(), DispatchError> {
        struct ArcExec(Arc<RecordingExecutor>);
        impl ToolExecutor for ArcExec {
            fn execute(
                &self,
                invocation: &CanonicalToolInvocation,
                actor: &DispatchActor,
                guard: LeaseUseGuard,
                cancel: &CancellationToken,
            ) -> Result<RawToolOutput, DispatchError> {
                self.0.execute(invocation, actor, guard, cancel)
            }
        }
        registry.register_privileged(
            GatewayTool::RepoRead,
            fs_read_descriptor,
            Box::new(ArcExec(exec)),
        )
    }

    fn run(
        registry: &ToolRegistry,
        broker: &ScriptedBroker,
        artifacts: &MemoryArtifactSink,
        invocation: &CanonicalToolInvocation,
    ) -> Result<ToolResultEnvelope, DispatchError> {
        dispatch(
            registry,
            broker,
            artifacts,
            invocation,
            &actor(),
            Instant::now(),
            &live(),
        )
    }

    fn assert_no_secret(text: &str) {
        assert!(!text.contains(SECRET), "leaked payload: {text}");
        assert!(!text.contains(CANARY), "leaked canary: {text}");
        assert!(!text.contains("hunter2"), "leaked canary fragment: {text}");
        assert!(
            !text.contains("capability_lease"),
            "leaked privilege field: {text}"
        );
    }

    fn assert_privilege_leak(err: DispatchError) {
        assert_eq!(err, DispatchError::PrivilegeLeak);
        assert_eq!(err.error_code(), Some(ErrorCode::ToolInvalidArguments));
        assert_no_secret(&err.to_string());
        assert_no_secret(&format!("{err:?}"));
    }

    #[test]
    fn privileged_executor_cannot_register_without_descriptor() {
        let mut registry = ToolRegistry::new();
        let exec = recorder(json!({"hits": 0}));
        struct ArcExec(Arc<RecordingExecutor>);
        impl ToolExecutor for ArcExec {
            fn execute(
                &self,
                invocation: &CanonicalToolInvocation,
                actor: &DispatchActor,
                guard: LeaseUseGuard,
                cancel: &CancellationToken,
            ) -> Result<RawToolOutput, DispatchError> {
                self.0.execute(invocation, actor, guard, cancel)
            }
        }
        for tool in GatewayTool::ALL {
            assert!(is_privileged_tool(*tool), "{}", tool.as_str());
            let err = registry
                .register(*tool, None, Box::new(ArcExec(Arc::clone(&exec))))
                .expect_err("descriptor required");
            assert_eq!(err, DispatchError::MissingCapabilityDescriptor);
            assert_eq!(err.error_code(), Some(ErrorCode::ToolInvalidArguments));
            assert_no_secret(&err.to_string());
        }
    }

    #[test]
    fn universal_tool_outcome_statuses_serialize_and_need_continuation() {
        for (status, wire) in [
            (ToolResultStatus::Ok, "ok"),
            (ToolResultStatus::Recovered, "recovered"),
            (ToolResultStatus::Partial, "partial"),
            (ToolResultStatus::Retryable, "retryable"),
            (ToolResultStatus::Denied, "denied"),
            (ToolResultStatus::ApprovalRequired, "approval_required"),
            (ToolResultStatus::Failed, "failed"),
        ] {
            assert_eq!(
                serde_json::to_string(&status).expect("json"),
                format!("\"{wire}\"")
            );
        }
        assert!(ToolResultStatus::Ok.is_terminal());
        assert!(ToolResultStatus::Denied.is_terminal());
        assert!(ToolResultStatus::Partial.needs_continuation());
        assert!(ToolResultStatus::Retryable.needs_continuation());
        assert!(!ToolResultStatus::Recovered.needs_continuation());

        // A Partial/Retryable envelope carries a machine-readable continuation.
        let cursor = ToolContinuation {
            artifact_id: ArtifactId::from_bytes(b"continuation"),
        };
        let partial = policy_envelope("call_7", ToolResultStatus::Partial, "partial")
            .with_continuation(cursor.clone());
        assert_eq!(partial.status(), ToolResultStatus::Partial);
        assert_eq!(
            partial.continuation().expect("cursor").artifact_id(),
            cursor.artifact_id()
        );
        let retryable = policy_envelope("call_7", ToolResultStatus::Retryable, "retry")
            .with_continuation(cursor);
        assert_eq!(retryable.status(), ToolResultStatus::Retryable);
        assert!(retryable.continuation().is_some());
    }

    #[test]
    fn dispatch_requires_capability_path_for_privileged_tools() {
        let mut registry = ToolRegistry::new();
        let exec = recorder(json!({"ok": true}));
        register_read(&mut registry, Arc::clone(&exec)).expect("register");
        let inv = invocation(
            "repo.read",
            json!({"path": "crates/tool-gateway/src/lib.rs"}),
        );
        let artifacts = MemoryArtifactSink::new();

        let deny = scripted(BrokerMode::Deny);
        let denied = run(&registry, &deny, &artifacts, &inv).expect("deny envelope");
        assert_eq!(denied.status(), ToolResultStatus::Denied);
        assert_eq!(denied.call_id(), "call_7");
        assert!(!exec.ran.load(Ordering::SeqCst));
        assert_eq!(deny.authorize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(deny.validate_calls.load(Ordering::SeqCst), 0);
        assert_no_secret(&serde_json::to_string(&denied).expect("json"));

        let ask = scripted(BrokerMode::Ask);
        let waiting = run(&registry, &ask, &artifacts, &inv).expect("ask envelope");
        assert_eq!(waiting.status(), ToolResultStatus::ApprovalRequired);
        assert!(!exec.ran.load(Ordering::SeqCst));
        assert_eq!(ask.validate_calls.load(Ordering::SeqCst), 0);

        let down = scripted(BrokerMode::Unavailable);
        let err = run(&registry, &down, &artifacts, &inv).expect_err("unavailable");
        assert_eq!(err, DispatchError::BrokerUnavailable);
        assert!(!exec.ran.load(Ordering::SeqCst));
        assert_eq!(err.error_code(), Some(ErrorCode::PolicyDenied));
    }

    #[test]
    fn allow_lease_revalidated_then_executor_runs() {
        let mut registry = ToolRegistry::new();
        let exec = recorder(json!({"bytes": 12}));
        register_read(&mut registry, Arc::clone(&exec)).expect("register");
        let broker = scripted(BrokerMode::Allow);
        let artifacts = MemoryArtifactSink::new();
        let inv = invocation(
            "repo.read",
            json!({"path": "crates/tool-gateway/src/lib.rs"}),
        );
        let envelope = run(&registry, &broker, &artifacts, &inv).expect("ok");
        assert_eq!(envelope.status(), ToolResultStatus::Ok);
        assert_eq!(envelope.schema(), TOOL_RESULT_SCHEMA);
        assert_eq!(envelope.data()["bytes"], 12);
        assert!(!envelope.truncated());
        assert!(envelope.artifacts().is_empty());
        assert!(exec.ran.load(Ordering::SeqCst));
        assert_eq!(broker.authorize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(broker.validate_calls.load(Ordering::SeqCst), 1);
        let encoded = serde_json::to_string(&envelope).expect("json");
        assert!(encoded.contains("\"status\":\"ok\""));
        assert!(!encoded.contains("capability_lease"));
        assert!(!encoded.contains("lease_id"));
        assert_no_secret(&encoded);
    }

    #[test]
    fn unregistered_tool_never_reaches_broker() {
        let registry = ToolRegistry::new();
        let broker = scripted(BrokerMode::Allow);
        let artifacts = MemoryArtifactSink::new();
        let inv = invocation(
            "repo.read",
            json!({"path": "crates/tool-gateway/src/lib.rs"}),
        );
        let err = run(&registry, &broker, &artifacts, &inv).expect_err("unregistered");
        assert_eq!(err, DispatchError::UnregisteredTool);
        assert_eq!(broker.authorize_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn large_result_uses_artifact_ref_and_excerpt() {
        let mut registry = ToolRegistry::new();
        let payload = "x".repeat(MAX_INLINE_RESULT_BYTES + 32);
        let exec = recorder(json!({"blob": payload, "tail": CANARY}));
        register_read(&mut registry, Arc::clone(&exec)).expect("register");
        let broker = scripted(BrokerMode::Allow);
        let artifacts = MemoryArtifactSink::new();
        let inv = invocation(
            "repo.read",
            json!({"path": "crates/tool-gateway/src/lib.rs"}),
        );
        let envelope = run(&registry, &broker, &artifacts, &inv).expect("truncated");
        assert!(envelope.truncated());
        assert_eq!(envelope.artifacts().len(), 1);
        let refer = &envelope.artifacts()[0];
        assert_eq!(
            envelope.continuation().map(ToolContinuation::artifact_id),
            Some(refer.id)
        );
        let excerpt = envelope.data()["excerpt"].as_str().expect("excerpt");
        assert!(excerpt.len() <= MAX_EXCERPT_BYTES);
        assert!(!excerpt.contains(CANARY));
        let stored = artifacts.get(&refer.id).expect("sink").expect("blob");
        let stored_text = String::from_utf8(stored).expect("utf8");
        assert!(stored_text.contains(CANARY));
        let encoded = serde_json::to_string(&envelope).expect("json");
        assert!(!encoded.contains(CANARY));
    }

    #[test]
    fn executor_privilege_handle_is_not_returned_to_model() {
        let mut registry = ToolRegistry::new();
        let exec = recorder(json!({"capability_lease": SECRET, "ok": true}));
        register_read(&mut registry, Arc::clone(&exec)).expect("register");
        let broker = scripted(BrokerMode::Allow);
        let artifacts = MemoryArtifactSink::new();
        let inv = invocation(
            "repo.read",
            json!({"path": "crates/tool-gateway/src/lib.rs"}),
        );
        let err = run(&registry, &broker, &artifacts, &inv).expect_err("leak");
        assert_privilege_leak(err);
    }

    #[test]
    fn summary_privilege_fields_or_values_fail_closed() {
        let cases = [
            format!("issued capability_lease={SECRET}"),
            format!("secret={SECRET}"),
            format!("token={SECRET}"),
            format!(r#"{{"capability_lease":"{SECRET}"}}"#),
            format!(r#"{{"secret":"{SECRET}"}}"#),
            format!(r#"{{"token":"{SECRET}"}}"#),
            format!("Capability-Lease {SECRET}"),
            SECRET.to_owned(),
        ];
        for summary in cases {
            let mut registry = ToolRegistry::new();
            let exec = recorder_with(&summary, json!({"ok": true}));
            register_read(&mut registry, Arc::clone(&exec)).expect("register");
            let broker = scripted(BrokerMode::Allow);
            let artifacts = MemoryArtifactSink::new();
            let inv = invocation(
                "repo.read",
                json!({"path": "crates/tool-gateway/src/lib.rs"}),
            );
            let err = run(&registry, &broker, &artifacts, &inv).expect_err("summary leak");
            assert_privilege_leak(err);
            assert!(exec.ran.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn privilege_value_under_benign_key_fails_closed() {
        let mut registry = ToolRegistry::new();
        let exec = recorder(json!({"note": format!("capability_lease={SECRET}")}));
        register_read(&mut registry, Arc::clone(&exec)).expect("register");
        let broker = scripted(BrokerMode::Allow);
        let artifacts = MemoryArtifactSink::new();
        let inv = invocation(
            "repo.read",
            json!({"path": "crates/tool-gateway/src/lib.rs"}),
        );
        let err = run(&registry, &broker, &artifacts, &inv).expect_err("value leak");
        assert_privilege_leak(err);
    }

    #[test]
    fn cancellation_stops_before_executor() {
        let mut registry = ToolRegistry::new();
        let exec = recorder(json!({"ok": true}));
        register_read(&mut registry, Arc::clone(&exec)).expect("register");
        let broker = scripted(BrokerMode::Allow);
        let artifacts = MemoryArtifactSink::new();
        let inv = invocation(
            "repo.read",
            json!({"path": "crates/tool-gateway/src/lib.rs"}),
        );
        let cancel = live();
        cancel.cancel();
        let err = dispatch(
            &registry,
            &broker,
            &artifacts,
            &inv,
            &actor(),
            Instant::now(),
            &cancel,
        )
        .expect_err("cancelled");
        assert_eq!(err, DispatchError::Cancelled);
        assert!(!exec.ran.load(Ordering::SeqCst));
        assert_eq!(err.error_code(), None);
    }

    #[test]
    fn default_descriptor_maps_read_and_rejects_unscoped_tools() {
        let actor = actor();
        let read = invocation(
            "repo.read",
            json!({"path": "crates/tool-gateway/src/lib.rs"}),
        );
        let need = describe_capability(&read, &actor).expect("read");
        assert_eq!(need.capability(), Capability::FsRead);

        let search = invocation("repo.search", json!({"query": "lease"}));
        assert_eq!(
            describe_capability(&search, &actor).expect_err("no path"),
            DispatchError::DescriptorFailed
        );

        let spawn = invocation(
            "agent.spawn",
            json!({"role": "explorer", "task": "map the crate", "access": "read_only"}),
        );
        assert_eq!(
            describe_capability(&spawn, &actor).expect_err("no family"),
            DispatchError::DescriptorFailed
        );
        assert_no_secret(&DispatchError::DescriptorFailed.to_string());
    }

    #[test]
    fn default_descriptor_fails_closed_on_unmapped_or_ambiguous_privilege() {
        let actor = actor();

        for action in ["download", "upload_file", "click", "observe"] {
            let mut args = json!({"action": action});
            if action != "observe" {
                args["observation_id"] = json!("obs-1");
            }
            if action == "download" || action == "upload_file" || action == "click" {
                args["url"] = json!("https://example.com");
            }
            let inv = invocation("browser.act", args);
            assert_eq!(
                describe_capability(&inv, &actor).expect_err(action),
                DispatchError::DescriptorFailed,
                "browser.act {action} must not mint a lease"
            );
        }

        let navigate = invocation(
            "browser.act",
            json!({"action":"navigate","observation_id":"obs-1","url":"https://example.com"}),
        );
        let need = describe_capability(&navigate, &actor).expect("navigate");
        assert_eq!(need.capability(), Capability::BrowserNavigate);

        let two_paths = invocation(
            "workspace.patch",
            json!({"ops":[
                {"op":"create_file","path":"src/a.rs","content":""},
                {"op":"create_file","path":"src/b.rs","content":""}
            ]}),
        );
        assert_eq!(
            describe_capability(&two_paths, &actor).expect_err("two paths"),
            DispatchError::DescriptorFailed
        );

        let move_file = invocation(
            "workspace.patch",
            json!({"ops":[{
                "op":"move_file",
                "from":"src/a.rs",
                "to":"src/b.rs",
                "preimage": ARTIFACT
            }]}),
        );
        assert_eq!(
            describe_capability(&move_file, &actor).expect_err("move two paths"),
            DispatchError::DescriptorFailed
        );

        let one_path = invocation(
            "workspace.patch",
            json!({"ops":[
                {"op":"create_file","path":"src/a.rs","content":""},
                {"op":"replace_range","path":"src/a.rs","preimage":ARTIFACT,"start":0,"end":0,"content":"x"}
            ]}),
        );
        let patch = describe_capability(&one_path, &actor).expect("single path");
        assert_eq!(patch.capability(), Capability::FsWrite);
    }

    #[test]
    fn t004_mismatched_lease_is_rejected_and_executor_does_not_run() {
        let mut registry = ToolRegistry::new();
        let exec = recorder(json!({"ok": true}));
        register_read(&mut registry, Arc::clone(&exec)).expect("register");
        let broker = scripted(BrokerMode::MismatchedLease);
        let artifacts = MemoryArtifactSink::new();
        let inv = invocation(
            "repo.read",
            json!({"path": "crates/tool-gateway/src/lib.rs"}),
        );
        let err = run(&registry, &broker, &artifacts, &inv).expect_err("lease mismatch");
        assert_eq!(err, DispatchError::LeaseInvalid);
        assert_eq!(err.error_code(), Some(ErrorCode::PolicyLeaseInvalid));
        assert!(!exec.ran.load(Ordering::SeqCst));
        assert_eq!(broker.authorize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(broker.validate_calls.load(Ordering::SeqCst), 0);
        assert_no_secret(&err.to_string());
    }

    #[test]
    fn contract_status_fields_are_stable() {
        let envelope = policy_envelope("call_7", ToolResultStatus::Denied, "policy denied");
        let encoded = serde_json::to_value(&envelope).expect("json");
        assert_eq!(encoded["schema"], 1);
        assert_eq!(encoded["call_id"], "call_7");
        assert_eq!(encoded["status"], "denied");
        assert_eq!(encoded["truncated"], false);
        assert!(encoded["continuation"].is_null());
        assert_eq!(encoded["artifacts"], json!([]));
    }
}
