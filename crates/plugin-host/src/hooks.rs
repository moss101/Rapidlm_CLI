//! Out-of-process lifecycle hook runner.
//!
//! Native hooks execute as supervised argv-first commands. They receive a
//! redacted event DTO on stdin, inherit no parent environment, and cannot
//! grant capabilities (FR-EXT-004, T-006, T-012). Timeout and nonzero exit
//! stay distinct. Failure policy is `ignore|warn|block` per spec.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::time::Duration;

use capability_broker::{
    CancellationToken, CanonicalHostPath, Capability, FilesystemRoot, LeaseUseGuard, PrincipalRef,
    ResourceDescriptor,
};
use process_supervisor::{
    DEFAULT_GRACE, ExecBinding, ExecSpec, MAX_STDIN_BYTES, SecretOrValue, StdinSpec, TerminalStatus,
    await_exit_draining, spawn,
};
use protocol::{ApiError, ArtifactId, ErrorCode, JobId, LeaseId, SandboxTier, SessionId, TraceId};
use sandbox::SandboxNetwork;
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::manifest::{MAX_IDENT_BYTES, MAX_REQUESTED_CAPS};

/// Wire schema name for [`HookSpec`].
pub const HOOK_SPEC_SCHEMA: &str = "rapidlm.hook_spec";

/// v1 schema version for hook specs.
pub const HOOK_SPEC_SCHEMA_VERSION: u16 = 1;

/// Wire schema name for the redacted stdin event DTO.
pub const HOOK_EVENT_SCHEMA: &str = "rapidlm.hook_event";

/// v1 schema version for hook event payloads.
pub const HOOK_EVENT_SCHEMA_VERSION: u16 = 1;

/// Process-scope command family bound into hook `proc.exec` leases.
pub const HOOK_COMMAND_FAMILY: &str = "hook";

/// Default wall-clock bound for one hook command.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard wall-clock ceiling. Larger requested timeouts fail closed.
pub const HARD_MAX_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Default captured stdout+stderr ceiling.
pub const DEFAULT_HOOK_OUTPUT_BYTES: u64 = 64 * 1024;

/// Hard captured output ceiling.
pub const HARD_MAX_HOOK_OUTPUT_BYTES: u64 = 1024 * 1024;

/// Maximum argv tokens on a command hook (including argv0).
pub const MAX_HOOK_ARGV: usize = 32;

/// Maximum UTF-8 bytes for one argv token.
pub const MAX_HOOK_ARG_BYTES: usize = 4096;

/// Maximum explicit env bindings accepted at run time.
pub const MAX_HOOK_ENV_NAMES: usize = 32;

/// Maximum UTF-8 bytes for one env name or allowlist entry.
pub const MAX_HOOK_ENV_NAME_BYTES: usize = 256;

/// Maximum UTF-8 bytes for the redacted stdin event DTO.
pub const MAX_HOOK_PAYLOAD_BYTES: usize = MAX_STDIN_BYTES;

/// Maximum extra event fields accepted before redaction.
pub const MAX_HOOK_EVENT_FIELDS: usize = 16;

/// Maximum UTF-8 bytes for one extra event field name or string value.
pub const MAX_HOOK_FIELD_BYTES: usize = 1024;

/// Maximum registered specs on one [`HookManager`].
pub const MAX_HOOK_SPECS: usize = 32;

/// Maximum secret canaries applied to one payload.
pub const MAX_HOOK_CANARIES: usize = 32;

/// Inline excerpt retained on [`HookCapture`].
pub const MAX_HOOK_EXCERPT_BYTES: usize = 1024;

const CANCEL_STRIDE: usize = 16;
const MAX_REDACT_DEPTH: usize = 4;
const REDACTED: &str = "[REDACTED]";

const SPEC_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "id",
    "event",
    "matcher",
    "kind",
    "command",
    "url",
    "plugin",
    "operation",
    "timeout_ms",
    "failure_policy",
    "requested_caps",
    "sandbox",
];
const SPEC_OPTIONAL: &[&str] = &[
    "matcher",
    "command",
    "url",
    "plugin",
    "operation",
    "timeout_ms",
    "failure_policy",
    "requested_caps",
];
const SANDBOX_FIELDS: &[&str] = &["tier", "network", "env_allowlist", "output_limit"];
const SANDBOX_OPTIONAL: &[&str] = &["network", "env_allowlist", "output_limit"];
const REQUESTED_CAP_FIELDS: &[&str] = &["capability", "resource"];
const SECRET_KEYS: &[&str] = &[
    "secret",
    "token",
    "password",
    "authorization",
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
    "private_key",
    "credential",
    "lease",
    "lease_token",
    "lease_id",
];
const GRANT_KEYS: &[&str] = &[
    "grant",
    "grants",
    "capability",
    "capabilities",
    "permission",
    "permissions",
    "policy",
    "allow",
    "requested_caps",
    "lease",
];

/// Bounded hook identity token.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct HookId(String);

/// Lifecycle event a hook may match.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    TurnStart,
    TurnEnd,
    GoalStart,
    GoalComplete,
    ContextPre,
    ContextPost,
    ModelPre,
    ModelPost,
    ToolPre,
    ToolPost,
    ToolFailure,
    PermissionRequest,
    FilePreWrite,
    FilePostWrite,
    PatchPre,
    PatchPost,
    CompactPre,
    CompactPost,
    CommitPre,
    AgentStart,
    AgentStop,
    VerifierStart,
    VerifierStop,
    ResourceAcquire,
    ResourceRelease,
}

/// Optional name matcher (`*`, prefix/suffix `*`, or exact).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct HookMatcher(String);

/// How a hook is invoked. Only [`HookKind::Command`] is executed here.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum HookKind {
    Command { argv: Vec<String> },
    Http { url: String },
    Plugin { plugin: String, operation: String },
}

/// Declared outcome when the hook times out or exits nonzero.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FailurePolicy {
    Ignore,
    Warn,
    Block,
}

/// Capability/resource pair declared on the spec. Not a grant.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct HookRequestedCap {
    capability: Capability,
    resource: ResourceDescriptor,
}

/// Explicit sandbox profile. Missing profile fails closed.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct HookSandboxProfile {
    tier: SandboxTier,
    network: SandboxNetwork,
    env_allowlist: Vec<String>,
    output_limit: u64,
}

/// Closed hook definition. Requested caps and output cannot widen policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookSpec {
    id: HookId,
    event: HookEvent,
    matcher: Option<HookMatcher>,
    kind: HookKind,
    timeout: Duration,
    failure_policy: FailurePolicy,
    requested_caps: Vec<HookRequestedCap>,
    sandbox: HookSandboxProfile,
}

/// Caller-supplied event. Extra fields are filtered before stdin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookEventInput {
    event: HookEvent,
    name: String,
    fields: BTreeMap<String, Value>,
}

/// Execution context. Parent environment is never read.
pub struct HookExecContext {
    cancel: CancellationToken,
    cwd: CanonicalHostPath,
    env: BTreeMap<String, String>,
    principal: PrincipalRef,
    session_id: SessionId,
    trace_id: TraceId,
    secret_canaries: Vec<Vec<u8>>,
}

/// Registered hooks executed in insertion order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HookManager {
    specs: Vec<HookSpec>,
}

/// Distinguishes success, nonzero exit, timeout, and cancel.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum HookRunStatus {
    Succeeded,
    NonZeroExit { code: i32 },
    Signaled { signal: i32 },
    TimedOut,
    Cancelled,
}

/// Parsed hook decision. Grant fields are never honored.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum HookDecision {
    Continue,
    Block,
}

/// Applied outcome after failure policy and blocking-event rules.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum HookDisposition {
    Continue,
    Warn,
    Block,
}

/// Bounded output view. Debug/Display omit payload bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct HookCapture {
    digest: ArtifactId,
    bytes: u64,
    excerpt: Vec<u8>,
    truncated: bool,
}

/// Artifact/event record for one hook invocation.
#[derive(Clone, Eq, PartialEq)]
pub struct HookRecord {
    hook_id: HookId,
    event: HookEvent,
    job_id: Option<JobId>,
    lease_id: Option<LeaseId>,
    status: HookRunStatus,
    failure_policy: FailurePolicy,
    decision: HookDecision,
    disposition: HookDisposition,
    grant_attempted: bool,
    stdout: HookCapture,
    stderr: HookCapture,
    sandbox_tier: SandboxTier,
}

/// Dispatch result. Later hooks are skipped after a block disposition.
#[derive(Clone, Eq, PartialEq)]
pub struct HookDispatchResult {
    records: Vec<HookRecord>,
    disposition: HookDisposition,
}

/// Typed hook failure. Display never echoes argv, env, payload, or secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum HookError {
    Cancelled,
    TooLarge,
    InvalidJson,
    UnsupportedSchema,
    UnknownField,
    MissingField,
    InvalidIdent,
    InvalidEvent,
    InvalidKind,
    InvalidMatcher,
    InvalidTimeout,
    InvalidOutputLimit,
    InvalidArgv,
    InvalidUrl,
    InvalidEnvName,
    TooManyArgs,
    TooManyEnvNames,
    TooManyCaps,
    TooManySpecs,
    DuplicateId,
    UnknownCapability,
    AmbientHostFilesystem,
    AmbientNetwork,
    FamilyMismatch,
    UnsupportedKind,
    SandboxTierUnavailable,
    NetworkNotIsolated,
    EnvNotAllowlisted,
    SecretNotMaterialized,
    LeaseRequired,
    LeaseInvalid,
    PayloadTooLarge,
    Spawn,
    Wait,
}

impl HookId {
    pub fn parse(value: &str) -> Result<Self, HookError> {
        Ok(Self(parse_ident(value)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl HookEvent {
    pub const ALL: &'static [Self] = &[
        Self::SessionStart,
        Self::SessionEnd,
        Self::TurnStart,
        Self::TurnEnd,
        Self::GoalStart,
        Self::GoalComplete,
        Self::ContextPre,
        Self::ContextPost,
        Self::ModelPre,
        Self::ModelPost,
        Self::ToolPre,
        Self::ToolPost,
        Self::ToolFailure,
        Self::PermissionRequest,
        Self::FilePreWrite,
        Self::FilePostWrite,
        Self::PatchPre,
        Self::PatchPost,
        Self::CompactPre,
        Self::CompactPost,
        Self::CommitPre,
        Self::AgentStart,
        Self::AgentStop,
        Self::VerifierStart,
        Self::VerifierStop,
        Self::ResourceAcquire,
        Self::ResourceRelease,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session.start",
            Self::SessionEnd => "session.end",
            Self::TurnStart => "turn.start",
            Self::TurnEnd => "turn.end",
            Self::GoalStart => "goal.start",
            Self::GoalComplete => "goal.complete",
            Self::ContextPre => "context.pre",
            Self::ContextPost => "context.post",
            Self::ModelPre => "model.pre",
            Self::ModelPost => "model.post",
            Self::ToolPre => "tool.pre",
            Self::ToolPost => "tool.post",
            Self::ToolFailure => "tool.failure",
            Self::PermissionRequest => "permission.request",
            Self::FilePreWrite => "file.pre_write",
            Self::FilePostWrite => "file.post_write",
            Self::PatchPre => "patch.pre",
            Self::PatchPost => "patch.post",
            Self::CompactPre => "compact.pre",
            Self::CompactPost => "compact.post",
            Self::CommitPre => "commit.pre",
            Self::AgentStart => "agent.start",
            Self::AgentStop => "agent.stop",
            Self::VerifierStart => "verifier.start",
            Self::VerifierStop => "verifier.stop",
            Self::ResourceAcquire => "resource.acquire",
            Self::ResourceRelease => "resource.release",
        }
    }

    /// Documented blocking events may return a block decision. Every `pre`
    /// gate plus permission and resource-acquisition gates are blocking;
    /// start/stop/post notifications are passive observers.
    pub const fn is_blocking(self) -> bool {
        matches!(
            self,
            Self::ToolPre
                | Self::FilePreWrite
                | Self::PatchPre
                | Self::CompactPre
                | Self::ContextPre
                | Self::ModelPre
                | Self::CommitPre
                | Self::PermissionRequest
                | Self::ResourceAcquire
        )
    }

    pub fn parse(value: &str) -> Result<Self, HookError> {
        for item in Self::ALL {
            if item.as_str() == value {
                return Ok(*item);
            }
        }
        Err(HookError::InvalidEvent)
    }

    fn default_failure_policy(self) -> FailurePolicy {
        if self.is_blocking() {
            FailurePolicy::Block
        } else {
            FailurePolicy::Ignore
        }
    }
}

impl HookMatcher {
    pub fn parse(value: &str) -> Result<Self, HookError> {
        if value.is_empty() || value.len() > MAX_IDENT_BYTES {
            return Err(HookError::InvalidMatcher);
        }
        if value.contains('\0') || value.chars().any(char::is_control) {
            return Err(HookError::InvalidMatcher);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn matches(&self, name: &str) -> bool {
        matcher_hits(&self.0, name)
    }
}

impl FailurePolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Warn => "warn",
            Self::Block => "block",
        }
    }

    pub fn parse(value: &str) -> Result<Self, HookError> {
        match value {
            "ignore" => Ok(Self::Ignore),
            "warn" => Ok(Self::Warn),
            "block" => Ok(Self::Block),
            _ => Err(HookError::InvalidJson),
        }
    }
}

impl HookRequestedCap {
    pub fn new(capability: Capability, resource: ResourceDescriptor) -> Result<Self, HookError> {
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
}

impl HookSandboxProfile {
    pub fn new(
        tier: SandboxTier,
        network: SandboxNetwork,
        env_allowlist: impl IntoIterator<Item = impl Into<String>>,
        output_limit: u64,
    ) -> Result<Self, HookError> {
        if output_limit == 0 || output_limit > HARD_MAX_HOOK_OUTPUT_BYTES {
            return Err(HookError::InvalidOutputLimit);
        }
        if tier != SandboxTier::HostRestricted {
            return Err(HookError::SandboxTierUnavailable);
        }
        if network != SandboxNetwork::None {
            return Err(HookError::NetworkNotIsolated);
        }
        let env_allowlist = collect_env_names(env_allowlist)?;
        Ok(Self {
            tier,
            network,
            env_allowlist,
            output_limit,
        })
    }

    pub fn isolated_host() -> Self {
        Self {
            tier: SandboxTier::HostRestricted,
            network: SandboxNetwork::None,
            env_allowlist: Vec::new(),
            output_limit: DEFAULT_HOOK_OUTPUT_BYTES,
        }
    }

    pub fn tier(&self) -> SandboxTier {
        self.tier
    }

    pub fn network(&self) -> SandboxNetwork {
        self.network
    }

    pub fn env_allowlist(&self) -> &[String] {
        &self.env_allowlist
    }

    pub fn output_limit(&self) -> u64 {
        self.output_limit
    }

    fn validate(&self) -> Result<(), HookError> {
        Self::new(
            self.tier,
            self.network,
            self.env_allowlist.iter().cloned(),
            self.output_limit,
        )
        .map(|_| ())
    }
}

impl HookKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Command { .. } => "command",
            Self::Http { .. } => "http",
            Self::Plugin { .. } => "plugin",
        }
    }
}

impl HookSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: HookId,
        event: HookEvent,
        matcher: Option<HookMatcher>,
        kind: HookKind,
        timeout: Duration,
        failure_policy: FailurePolicy,
        requested_caps: Vec<HookRequestedCap>,
        sandbox: HookSandboxProfile,
    ) -> Result<Self, HookError> {
        let spec = Self {
            id,
            event,
            matcher,
            kind,
            timeout,
            failure_policy,
            requested_caps,
            sandbox,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn id(&self) -> &HookId {
        &self.id
    }

    pub fn event(&self) -> HookEvent {
        self.event
    }

    pub fn matcher(&self) -> Option<&HookMatcher> {
        self.matcher.as_ref()
    }

    pub fn kind(&self) -> &HookKind {
        &self.kind
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn failure_policy(&self) -> FailurePolicy {
        self.failure_policy
    }

    pub fn requested_caps(&self) -> &[HookRequestedCap] {
        &self.requested_caps
    }

    pub fn sandbox(&self) -> &HookSandboxProfile {
        &self.sandbox
    }

    pub fn matches(&self, event: &HookEventInput) -> bool {
        if self.event != event.event {
            return false;
        }
        match &self.matcher {
            None => true,
            Some(matcher) => matcher.matches(&event.name),
        }
    }

    fn validate(&self) -> Result<(), HookError> {
        if self.timeout.is_zero() || self.timeout > HARD_MAX_HOOK_TIMEOUT {
            return Err(HookError::InvalidTimeout);
        }
        if self.requested_caps.len() > MAX_REQUESTED_CAPS {
            return Err(HookError::TooManyCaps);
        }
        for cap in &self.requested_caps {
            reject_ambient(cap.capability, &cap.resource)?;
            cap.capability
                .compatible_with(&cap.resource)
                .map_err(map_capability_error)?;
        }
        self.sandbox.validate()?;
        match &self.kind {
            HookKind::Command { argv } => validate_argv(argv)?,
            HookKind::Http { url } => validate_url(url)?,
            HookKind::Plugin { plugin, operation } => {
                parse_ident(plugin)?;
                parse_ident(operation)?;
            }
        }
        Ok(())
    }
}

impl HookEventInput {
    pub fn new(
        event: HookEvent,
        name: impl Into<String>,
        fields: impl IntoIterator<Item = (impl Into<String>, Value)>,
    ) -> Result<Self, HookError> {
        let name = name.into();
        if name.len() > MAX_IDENT_BYTES || !valid_text(&name) {
            return Err(HookError::InvalidIdent);
        }
        let mut map = BTreeMap::new();
        for (key, value) in fields {
            let key = key.into();
            if key.is_empty() || key.len() > MAX_HOOK_FIELD_BYTES || !valid_text(&key) {
                return Err(HookError::InvalidIdent);
            }
            map.insert(key, value);
            if map.len() > MAX_HOOK_EVENT_FIELDS {
                return Err(HookError::TooLarge);
            }
        }
        Ok(Self {
            event,
            name,
            fields: map,
        })
    }

    pub fn event(&self) -> HookEvent {
        self.event
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn fields(&self) -> &BTreeMap<String, Value> {
        &self.fields
    }
}

impl HookExecContext {
    pub fn new(
        cancel: CancellationToken,
        cwd: CanonicalHostPath,
        env: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
        principal: PrincipalRef,
        session_id: SessionId,
        trace_id: TraceId,
    ) -> Result<Self, HookError> {
        let mut map = BTreeMap::new();
        for (name, value) in env {
            let name = name.into();
            validate_env_name(&name)?;
            let value = value.into();
            if value.contains('\0') {
                return Err(HookError::InvalidJson);
            }
            map.insert(name, value);
            if map.len() > MAX_HOOK_ENV_NAMES {
                return Err(HookError::TooManyEnvNames);
            }
        }
        Ok(Self {
            cancel,
            cwd,
            env: map,
            principal,
            session_id,
            trace_id,
            secret_canaries: Vec::new(),
        })
    }

    /// Register exact-value canaries that must not appear on hook stdin.
    pub fn with_secret_canaries(
        mut self,
        canaries: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> Result<Self, HookError> {
        let mut out = Vec::new();
        for item in canaries {
            let bytes = item.as_ref();
            if bytes.is_empty() {
                continue;
            }
            if bytes.len() > MAX_HOOK_FIELD_BYTES {
                return Err(HookError::TooLarge);
            }
            out.push(bytes.to_vec());
            if out.len() > MAX_HOOK_CANARIES {
                return Err(HookError::TooLarge);
            }
        }
        self.secret_canaries = out;
        Ok(self)
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn cwd(&self) -> &CanonicalHostPath {
        &self.cwd
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    pub fn principal(&self) -> &PrincipalRef {
        &self.principal
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }
}

impl HookManager {
    pub fn new() -> Self {
        Self { specs: Vec::new() }
    }

    pub fn register(&mut self, spec: HookSpec) -> Result<(), HookError> {
        spec.validate()?;
        if self.specs.len() >= MAX_HOOK_SPECS {
            return Err(HookError::TooManySpecs);
        }
        if self.specs.iter().any(|existing| existing.id == spec.id) {
            return Err(HookError::DuplicateId);
        }
        self.specs.push(spec);
        Ok(())
    }

    pub fn specs(&self) -> &[HookSpec] {
        &self.specs
    }

    pub fn matching<'a>(&'a self, event: &'a HookEventInput) -> impl Iterator<Item = &'a HookSpec> {
        self.specs.iter().filter(move |spec| spec.matches(event))
    }
}

impl HookCapture {
    pub fn empty() -> Self {
        Self {
            digest: ArtifactId::from_bytes(&[]),
            bytes: 0,
            excerpt: Vec::new(),
            truncated: false,
        }
    }

    pub fn digest(&self) -> ArtifactId {
        self.digest
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn excerpt(&self) -> &[u8] {
        &self.excerpt
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    fn from_bytes(bytes: Vec<u8>, truncated: bool) -> Self {
        let excerpt_len = bytes.len().min(MAX_HOOK_EXCERPT_BYTES);
        Self {
            digest: ArtifactId::from_bytes(&bytes),
            bytes: bytes.len() as u64,
            excerpt: bytes[..excerpt_len].to_vec(),
            truncated,
        }
    }
}

impl HookRecord {
    pub fn hook_id(&self) -> &HookId {
        &self.hook_id
    }

    pub fn event(&self) -> HookEvent {
        self.event
    }

    pub fn job_id(&self) -> Option<JobId> {
        self.job_id
    }

    pub fn lease_id(&self) -> Option<LeaseId> {
        self.lease_id
    }

    pub fn status(&self) -> HookRunStatus {
        self.status
    }

    pub fn failure_policy(&self) -> FailurePolicy {
        self.failure_policy
    }

    pub fn decision(&self) -> HookDecision {
        self.decision
    }

    pub fn disposition(&self) -> HookDisposition {
        self.disposition
    }

    pub fn grant_attempted(&self) -> bool {
        self.grant_attempted
    }

    pub fn stdout(&self) -> &HookCapture {
        &self.stdout
    }

    pub fn stderr(&self) -> &HookCapture {
        &self.stderr
    }

    pub fn sandbox_tier(&self) -> SandboxTier {
        self.sandbox_tier
    }
}

impl HookDispatchResult {
    pub fn records(&self) -> &[HookRecord] {
        &self.records
    }

    pub fn disposition(&self) -> HookDisposition {
        self.disposition
    }
}

impl HookRunStatus {
    pub const fn is_timeout(self) -> bool {
        matches!(self, Self::TimedOut)
    }

    pub const fn is_nonzero_exit(self) -> bool {
        matches!(self, Self::NonZeroExit { .. } | Self::Signaled { .. })
    }

    pub const fn is_success(self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// Parse and validate a closed hook-spec document.
pub fn parse_hook_spec(bytes: &[u8], cancel: &CancellationToken) -> Result<HookSpec, HookError> {
    cancel.check().map_err(|_| HookError::Cancelled)?;
    if bytes.len() > 64 * 1024 {
        return Err(HookError::TooLarge);
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| HookError::InvalidJson)?;
    decode_spec(&value, cancel)
}

/// Redacted, minimum-field event DTO written to hook stdin.
pub fn redact_event_payload(
    spec: &HookSpec,
    event: &HookEventInput,
    ctx: &HookExecContext,
) -> Result<Value, HookError> {
    ctx.cancel.check().map_err(|_| HookError::Cancelled)?;
    let mut object = Map::new();
    object.insert("schema".into(), Value::String(HOOK_EVENT_SCHEMA.to_owned()));
    object.insert(
        "schema_version".into(),
        Value::Number(HOOK_EVENT_SCHEMA_VERSION.into()),
    );
    object.insert(
        "event".into(),
        Value::String(event.event.as_str().to_owned()),
    );
    object.insert("hook".into(), Value::String(spec.id.as_str().to_owned()));
    object.insert("name".into(), Value::String(event.name.clone()));
    object.insert(
        "session_id".into(),
        Value::String(ctx.session_id.to_string()),
    );
    object.insert("trace_id".into(), Value::String(ctx.trace_id.to_string()));
    let mut fields = Map::new();
    for (i, (key, value)) in event.fields.iter().enumerate() {
        if i % CANCEL_STRIDE == 0 {
            ctx.cancel.check().map_err(|_| HookError::Cancelled)?;
        }
        if is_secret_key(key) || is_grant_key(key) {
            continue;
        }
        fields.insert(
            key.clone(),
            redact_value(value, &ctx.secret_canaries, 0, &ctx.cancel)?,
        );
    }
    object.insert("fields".into(), Value::Object(fields));
    let encoded = Value::Object(object);
    let scan = serde_json::to_vec(&encoded).map_err(|_| HookError::InvalidJson)?;
    if scan.len() > MAX_HOOK_PAYLOAD_BYTES {
        return Err(HookError::PayloadTooLarge);
    }
    if contains_canary(&scan, &ctx.secret_canaries) {
        return Err(HookError::PayloadTooLarge);
    }
    Ok(encoded)
}

/// Build the supervised exec spec. Parent env is not copied.
pub fn prepare_command(
    spec: &HookSpec,
    ctx: &HookExecContext,
    payload: &[u8],
) -> Result<ExecSpec, HookError> {
    spec.validate()?;
    ctx.cancel.check().map_err(|_| HookError::Cancelled)?;
    if payload.len() > MAX_HOOK_PAYLOAD_BYTES {
        return Err(HookError::PayloadTooLarge);
    }
    let HookKind::Command { argv } = &spec.kind else {
        return Err(HookError::UnsupportedKind);
    };
    let env = bind_env(spec, ctx)?;
    let stdin = if payload.is_empty() {
        StdinSpec::Empty
    } else {
        StdinSpec::Bytes(payload.to_vec())
    };
    let exec = ExecSpec::argv(
        argv.iter().cloned(),
        ctx.cwd.clone(),
        env,
        stdin,
        Some(spec.timeout),
        spec.sandbox.output_limit,
        ctx.cancel.clone(),
    )
    .map_err(map_spawn)?;
    let binding =
        ExecBinding::proc_exec(ctx.principal.clone(), ctx.session_id, HOOK_COMMAND_FAMILY)
            .map_err(|_| HookError::LeaseRequired)?;
    exec.bind(binding).map_err(map_spawn)
}

/// Run one command hook. `lease` must match [`prepare_command`].
pub fn run_command_hook(
    spec: &HookSpec,
    event: &HookEventInput,
    ctx: &HookExecContext,
    lease: LeaseUseGuard,
) -> Result<HookRecord, HookError> {
    spec.validate()?;
    if spec.event != event.event {
        return Err(HookError::InvalidEvent);
    }
    ctx.cancel.check().map_err(|_| HookError::Cancelled)?;
    let payload = redact_event_payload(spec, event, ctx)?;
    let bytes = serde_json::to_vec(&payload).map_err(|_| HookError::InvalidJson)?;
    let exec = prepare_command(spec, ctx, &bytes)?;
    let mut handle = spawn(exec, lease).map_err(map_spawn)?;
    let job_id = handle.job_id();
    let lease_id = handle.lease_id();
    // Draining stdout/stderr concurrently with the wait (not after) avoids
    // deadlocking on a hook whose combined output exceeds the OS pipe buffer
    // before it exits.
    let cap = usize::try_from(spec.sandbox.output_limit).unwrap_or(usize::MAX);
    let (report, stdout, stderr) = await_exit_draining(&mut handle, &ctx.cancel, DEFAULT_GRACE, cap)
        .map_err(|_| HookError::Wait)?;
    let stdout = HookCapture::from_bytes(stdout.bytes, stdout.truncated);
    let stderr = HookCapture::from_bytes(stderr.bytes, stderr.truncated);
    drop(handle);
    let status = status_from_terminal(report.status());
    let (decision, grant_attempted) = parse_hook_output(event.event, status, stdout.excerpt());
    let disposition = apply_policy(spec, event.event, status, decision);
    Ok(HookRecord {
        hook_id: spec.id.clone(),
        event: spec.event,
        job_id: Some(job_id),
        lease_id: Some(lease_id),
        status,
        failure_policy: spec.failure_policy,
        decision,
        disposition,
        grant_attempted,
        stdout,
        stderr,
        sandbox_tier: spec.sandbox.tier,
    })
}

/// Run matching hooks. A block disposition stops later hooks.
pub fn dispatch(
    manager: &HookManager,
    event: &HookEventInput,
    ctx: &HookExecContext,
    mut lease_for: impl FnMut(&HookSpec, &ExecSpec) -> Result<LeaseUseGuard, HookError>,
) -> Result<HookDispatchResult, HookError> {
    ctx.cancel.check().map_err(|_| HookError::Cancelled)?;
    let mut records = Vec::new();
    let mut disposition = HookDisposition::Continue;
    for (i, spec) in manager.matching(event).enumerate() {
        if i % CANCEL_STRIDE == 0 {
            ctx.cancel.check().map_err(|_| HookError::Cancelled)?;
        }
        let payload = redact_event_payload(spec, event, ctx)?;
        let bytes = serde_json::to_vec(&payload).map_err(|_| HookError::InvalidJson)?;
        let exec = prepare_command(spec, ctx, &bytes)?;
        let lease = lease_for(spec, &exec)?;
        let record = run_prepared(spec, event, ctx, exec, lease, bytes)?;
        let blocked = record.disposition == HookDisposition::Block;
        if record.disposition == HookDisposition::Warn {
            disposition = HookDisposition::Warn;
        }
        if blocked {
            disposition = HookDisposition::Block;
        }
        records.push(record);
        if blocked {
            break;
        }
    }
    Ok(HookDispatchResult {
        records,
        disposition,
    })
}

fn run_prepared(
    spec: &HookSpec,
    event: &HookEventInput,
    ctx: &HookExecContext,
    exec: ExecSpec,
    lease: LeaseUseGuard,
    _payload: Vec<u8>,
) -> Result<HookRecord, HookError> {
    ctx.cancel.check().map_err(|_| HookError::Cancelled)?;
    let mut handle = spawn(exec, lease).map_err(map_spawn)?;
    let job_id = handle.job_id();
    let lease_id = handle.lease_id();
    // Draining stdout/stderr concurrently with the wait (not after) avoids
    // deadlocking on a hook whose combined output exceeds the OS pipe buffer
    // before it exits.
    let cap = usize::try_from(spec.sandbox.output_limit).unwrap_or(usize::MAX);
    let (report, stdout, stderr) = await_exit_draining(&mut handle, &ctx.cancel, DEFAULT_GRACE, cap)
        .map_err(|_| HookError::Wait)?;
    let stdout = HookCapture::from_bytes(stdout.bytes, stdout.truncated);
    let stderr = HookCapture::from_bytes(stderr.bytes, stderr.truncated);
    drop(handle);
    let status = status_from_terminal(report.status());
    let (decision, grant_attempted) = parse_hook_output(event.event, status, stdout.excerpt());
    let disposition = apply_policy(spec, event.event, status, decision);
    Ok(HookRecord {
        hook_id: spec.id.clone(),
        event: spec.event,
        job_id: Some(job_id),
        lease_id: Some(lease_id),
        status,
        failure_policy: spec.failure_policy,
        decision,
        disposition,
        grant_attempted,
        stdout,
        stderr,
        sandbox_tier: spec.sandbox.tier,
    })
}

fn apply_policy(
    spec: &HookSpec,
    event: HookEvent,
    status: HookRunStatus,
    decision: HookDecision,
) -> HookDisposition {
    if !status.is_success() {
        return match spec.failure_policy {
            FailurePolicy::Ignore => HookDisposition::Continue,
            FailurePolicy::Warn => HookDisposition::Warn,
            FailurePolicy::Block => HookDisposition::Block,
        };
    }
    if event.is_blocking() && decision == HookDecision::Block {
        HookDisposition::Block
    } else {
        HookDisposition::Continue
    }
}

fn parse_hook_output(
    event: HookEvent,
    status: HookRunStatus,
    stdout: &[u8],
) -> (HookDecision, bool) {
    if !status.is_success() {
        return (HookDecision::Continue, false);
    }
    let text = std::str::from_utf8(stdout).unwrap_or("").trim();
    if text.is_empty() {
        return (HookDecision::Continue, false);
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return (HookDecision::Continue, false);
    };
    let Some(object) = value.as_object() else {
        return (HookDecision::Continue, false);
    };
    let grant_attempted = object.keys().any(|key| is_grant_key(key));
    let decision = match object.get("decision").and_then(Value::as_str) {
        Some("block") if event.is_blocking() => HookDecision::Block,
        _ => HookDecision::Continue,
    };
    (decision, grant_attempted)
}

fn status_from_terminal(status: TerminalStatus) -> HookRunStatus {
    match status {
        TerminalStatus::TimedOut { .. } => HookRunStatus::TimedOut,
        TerminalStatus::Cancelled { .. } => HookRunStatus::Cancelled,
        TerminalStatus::Exited(exit) => {
            if let Some(code) = exit.code() {
                if code == 0 {
                    HookRunStatus::Succeeded
                } else {
                    HookRunStatus::NonZeroExit { code }
                }
            } else if let Some(signal) = exit.signal() {
                HookRunStatus::Signaled { signal }
            } else {
                HookRunStatus::NonZeroExit { code: -1 }
            }
        }
    }
}


fn bind_env(
    spec: &HookSpec,
    ctx: &HookExecContext,
) -> Result<Vec<(String, SecretOrValue)>, HookError> {
    let allow = &spec.sandbox.env_allowlist;
    for name in ctx.env.keys() {
        if !allow.iter().any(|item| item == name) {
            return Err(HookError::EnvNotAllowlisted);
        }
    }
    let mut env = Vec::new();
    for name in allow {
        if let Some(value) = ctx.env.get(name) {
            let bound = SecretOrValue::plaintext(value.clone()).map_err(|_| HookError::TooLarge)?;
            env.push((name.clone(), bound));
        }
    }
    Ok(env)
}

fn collect_env_names(
    names: impl IntoIterator<Item = impl Into<String>>,
) -> Result<Vec<String>, HookError> {
    let mut out = Vec::new();
    for name in names {
        let name = name.into();
        validate_env_name(&name)?;
        if out.iter().any(|existing: &String| existing == &name) {
            return Err(HookError::InvalidEnvName);
        }
        out.push(name);
        if out.len() > MAX_HOOK_ENV_NAMES {
            return Err(HookError::TooManyEnvNames);
        }
    }
    Ok(out)
}

fn validate_argv(argv: &[String]) -> Result<(), HookError> {
    if argv.is_empty() {
        return Err(HookError::InvalidArgv);
    }
    if argv.len() > MAX_HOOK_ARGV {
        return Err(HookError::TooManyArgs);
    }
    for (i, arg) in argv.iter().enumerate() {
        if arg.is_empty() || arg.len() > MAX_HOOK_ARG_BYTES {
            return Err(HookError::InvalidArgv);
        }
        if arg.contains('\0') || arg.chars().any(char::is_control) {
            return Err(HookError::InvalidArgv);
        }
        if i == 0 && CanonicalHostPath::from_resolved(arg).is_err() {
            return Err(HookError::InvalidArgv);
        }
    }
    Ok(())
}

fn validate_url(url: &str) -> Result<(), HookError> {
    if url.is_empty() || url.len() > MAX_HOOK_ARG_BYTES || !valid_text(url) {
        return Err(HookError::InvalidUrl);
    }
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err(HookError::InvalidUrl);
    }
    Ok(())
}

fn validate_env_name(name: &str) -> Result<(), HookError> {
    if name.is_empty() {
        return Err(HookError::InvalidEnvName);
    }
    if name.len() > MAX_HOOK_ENV_NAME_BYTES {
        return Err(HookError::TooLarge);
    }
    if name.contains('\0') || name.chars().any(char::is_control) {
        return Err(HookError::InvalidEnvName);
    }
    let bytes = name.as_bytes();
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(HookError::InvalidEnvName);
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        return Err(HookError::InvalidEnvName);
    }
    Ok(())
}

fn decode_spec(value: &Value, cancel: &CancellationToken) -> Result<HookSpec, HookError> {
    cancel.check().map_err(|_| HookError::Cancelled)?;
    let object = object_map(value)?;
    expect_keys(object, SPEC_FIELDS, SPEC_OPTIONAL)?;
    expect_schema(object)?;
    let id = HookId::parse(require_str(object, "id")?)?;
    let event = HookEvent::parse(require_str(object, "event")?)?;
    let matcher = match object.get("matcher") {
        Some(Value::Null) | None => None,
        Some(Value::String(raw)) => Some(HookMatcher::parse(raw)?),
        Some(_) => return Err(HookError::InvalidJson),
    };
    let kind = decode_kind(object)?;
    let timeout = match object.get("timeout_ms") {
        Some(Value::Number(number)) => {
            let ms = number.as_u64().ok_or(HookError::InvalidTimeout)?;
            Duration::from_millis(ms)
        }
        Some(_) => return Err(HookError::InvalidTimeout),
        None => DEFAULT_HOOK_TIMEOUT,
    };
    let failure_policy = match object.get("failure_policy") {
        Some(Value::String(raw)) => FailurePolicy::parse(raw)?,
        Some(Value::Null) | None => event.default_failure_policy(),
        Some(_) => return Err(HookError::InvalidJson),
    };
    let requested_caps = decode_requested_caps(optional_array(object, "requested_caps")?, cancel)?;
    let sandbox = decode_sandbox(require_object(object, "sandbox")?)?;
    HookSpec::new(
        id,
        event,
        matcher,
        kind,
        timeout,
        failure_policy,
        requested_caps,
        sandbox,
    )
}

fn decode_kind(object: &Map<String, Value>) -> Result<HookKind, HookError> {
    let kind = require_str(object, "kind")?;
    match kind {
        "command" => {
            let argv = require_array(object, "command")?;
            if argv.len() > MAX_HOOK_ARGV {
                return Err(HookError::TooManyArgs);
            }
            let mut out = Vec::with_capacity(argv.len());
            for item in argv {
                let raw = item.as_str().ok_or(HookError::InvalidArgv)?;
                out.push(raw.to_owned());
            }
            Ok(HookKind::Command { argv: out })
        }
        "http" => Ok(HookKind::Http {
            url: require_str(object, "url")?.to_owned(),
        }),
        "plugin" => Ok(HookKind::Plugin {
            plugin: require_str(object, "plugin")?.to_owned(),
            operation: require_str(object, "operation")?.to_owned(),
        }),
        _ => Err(HookError::InvalidKind),
    }
}

fn decode_sandbox(object: &Map<String, Value>) -> Result<HookSandboxProfile, HookError> {
    expect_keys(object, SANDBOX_FIELDS, SANDBOX_OPTIONAL)?;
    let tier: SandboxTier = require_str(object, "tier")?
        .parse()
        .map_err(|_| HookError::SandboxTierUnavailable)?;
    let network = match object.get("network") {
        Some(Value::String(raw)) => parse_network(raw)?,
        Some(Value::Null) | None => SandboxNetwork::None,
        Some(_) => return Err(HookError::InvalidJson),
    };
    let items = optional_array(object, "env_allowlist")?;
    let mut env_allowlist = Vec::new();
    for item in items {
        let raw = item.as_str().ok_or(HookError::InvalidEnvName)?;
        env_allowlist.push(raw.to_owned());
    }
    let output_limit = match object.get("output_limit") {
        Some(Value::Number(number)) => number.as_u64().ok_or(HookError::InvalidOutputLimit)?,
        Some(_) => return Err(HookError::InvalidOutputLimit),
        None => DEFAULT_HOOK_OUTPUT_BYTES,
    };
    HookSandboxProfile::new(tier, network, env_allowlist, output_limit)
}

fn parse_network(raw: &str) -> Result<SandboxNetwork, HookError> {
    match raw {
        "none" => Ok(SandboxNetwork::None),
        "allowlist" => Ok(SandboxNetwork::Allowlist),
        "proxy" => Ok(SandboxNetwork::Proxy),
        _ => Err(HookError::NetworkNotIsolated),
    }
}

fn decode_requested_caps(
    items: &[Value],
    cancel: &CancellationToken,
) -> Result<Vec<HookRequestedCap>, HookError> {
    if items.len() > MAX_REQUESTED_CAPS {
        return Err(HookError::TooManyCaps);
    }
    let mut caps = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if i % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| HookError::Cancelled)?;
        }
        caps.push(decode_requested_cap(item)?);
    }
    Ok(caps)
}

fn decode_requested_cap(value: &Value) -> Result<HookRequestedCap, HookError> {
    if value.as_str().is_some() {
        return Err(HookError::UnknownCapability);
    }
    let object = object_map(value)?;
    expect_keys(object, REQUESTED_CAP_FIELDS, &[])?;
    let capability = decode_capability(object.get("capability").ok_or(HookError::MissingField)?)?;
    let resource = decode_resource(object.get("resource").ok_or(HookError::MissingField)?)?;
    HookRequestedCap::new(capability, resource)
}

fn decode_capability(value: &Value) -> Result<Capability, HookError> {
    if let Some(raw) = value.as_str() {
        return raw.parse().map_err(map_capability_error);
    }
    serde_json::from_value(value.clone()).map_err(|_| HookError::UnknownCapability)
}

fn decode_resource(value: &Value) -> Result<ResourceDescriptor, HookError> {
    serde_json::from_value(value.clone()).map_err(|_| classify_resource_failure(value))
}

fn reject_ambient(_capability: Capability, resource: &ResourceDescriptor) -> Result<(), HookError> {
    match resource {
        ResourceDescriptor::Filesystem(scope) if scope.root() == FilesystemRoot::Host => {
            Err(HookError::AmbientHostFilesystem)
        }
        ResourceDescriptor::Network(_) => Err(HookError::AmbientNetwork),
        _ => Ok(()),
    }
}

fn classify_resource_failure(value: &Value) -> HookError {
    let Some(object) = value.as_object() else {
        return HookError::InvalidJson;
    };
    match object.get("kind").and_then(Value::as_str) {
        Some("filesystem") if object.get("root").and_then(Value::as_str) == Some("host") => {
            HookError::AmbientHostFilesystem
        }
        Some("network") => HookError::AmbientNetwork,
        _ => HookError::UnknownCapability,
    }
}

fn map_capability_error(err: capability_broker::CapabilityError) -> HookError {
    match err {
        capability_broker::CapabilityError::UnknownRoot => HookError::AmbientHostFilesystem,
        capability_broker::CapabilityError::InvalidHost
        | capability_broker::CapabilityError::UnknownScheme => HookError::AmbientNetwork,
        capability_broker::CapabilityError::FamilyMismatch
        | capability_broker::CapabilityError::MissingScope => HookError::FamilyMismatch,
        capability_broker::CapabilityError::UnsupportedSchema => HookError::UnsupportedSchema,
        capability_broker::CapabilityError::MissingField => HookError::MissingField,
        _ => HookError::UnknownCapability,
    }
}

fn map_spawn(err: process_supervisor::SpawnError) -> HookError {
    match err {
        process_supervisor::SpawnError::Cancelled => HookError::Cancelled,
        process_supervisor::SpawnError::LeaseNotBound => HookError::LeaseInvalid,
        process_supervisor::SpawnError::SecretNotMaterialized => HookError::SecretNotMaterialized,
        process_supervisor::SpawnError::StdinTooLarge => HookError::PayloadTooLarge,
        process_supervisor::SpawnError::TimeoutInvalid => HookError::InvalidTimeout,
        process_supervisor::SpawnError::TooManyArgs => HookError::TooManyArgs,
        process_supervisor::SpawnError::TooManyEnvNames => HookError::TooManyEnvNames,
        process_supervisor::SpawnError::InvalidEnvName
        | process_supervisor::SpawnError::EmptyEnvName => HookError::InvalidEnvName,
        process_supervisor::SpawnError::EmptyArgv
        | process_supervisor::SpawnError::EmptyExecutable
        | process_supervisor::SpawnError::RelativeExecutable
        | process_supervisor::SpawnError::UnresolvedExecutable => HookError::InvalidArgv,
        process_supervisor::SpawnError::Io => HookError::Spawn,
        _ => HookError::Spawn,
    }
}

fn redact_value(
    value: &Value,
    canaries: &[Vec<u8>],
    depth: usize,
    cancel: &CancellationToken,
) -> Result<Value, HookError> {
    cancel.check().map_err(|_| HookError::Cancelled)?;
    if depth > MAX_REDACT_DEPTH {
        return Ok(Value::Null);
    }
    match value {
        Value::String(text) => Ok(Value::String(redact_text(text, canaries))),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len().min(MAX_HOOK_EVENT_FIELDS));
            for (i, item) in items.iter().take(MAX_HOOK_EVENT_FIELDS).enumerate() {
                if i % CANCEL_STRIDE == 0 {
                    cancel.check().map_err(|_| HookError::Cancelled)?;
                }
                out.push(redact_value(item, canaries, depth + 1, cancel)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(object) => {
            let mut out = Map::new();
            for (i, (key, item)) in object.iter().enumerate() {
                if i % CANCEL_STRIDE == 0 {
                    cancel.check().map_err(|_| HookError::Cancelled)?;
                }
                if is_secret_key(key) || is_grant_key(key) {
                    continue;
                }
                if key.len() > MAX_HOOK_FIELD_BYTES || !valid_text(key) {
                    continue;
                }
                out.insert(
                    key.clone(),
                    redact_value(item, canaries, depth + 1, cancel)?,
                );
            }
            Ok(Value::Object(out))
        }
        Value::Number(number) => Ok(Value::Number(number.clone())),
        Value::Bool(flag) => Ok(Value::Bool(*flag)),
        Value::Null => Ok(Value::Null),
    }
}

fn redact_text(text: &str, canaries: &[Vec<u8>]) -> String {
    if text.len() > MAX_HOOK_FIELD_BYTES {
        return REDACTED.to_owned();
    }
    let mut bytes = text.as_bytes().to_vec();
    for canary in canaries {
        if canary.is_empty() {
            continue;
        }
        let mut i = 0;
        while let Some(at) = find_subslice(&bytes[i..], canary) {
            let start = i + at;
            bytes.splice(
                start..start + canary.len(),
                REDACTED.as_bytes().iter().copied(),
            );
            i = start + REDACTED.len();
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn contains_canary(haystack: &[u8], canaries: &[Vec<u8>]) -> bool {
    canaries
        .iter()
        .any(|canary| !canary.is_empty() && find_subslice(haystack, canary).is_some())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn matcher_hits(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let prefix = pattern.starts_with('*');
    let suffix = pattern.ends_with('*');
    match (prefix, suffix) {
        (true, true) => {
            let mid = &pattern[1..pattern.len() - 1];
            mid.is_empty() || name.contains(mid)
        }
        (true, false) => name.ends_with(&pattern[1..]),
        (false, true) => name.starts_with(&pattern[..pattern.len() - 1]),
        (false, false) => pattern == name,
    }
}

fn is_secret_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    SECRET_KEYS
        .iter()
        .any(|item| lowered == *item || lowered.ends_with(&format!("_{item}")))
}

fn is_grant_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    GRANT_KEYS.iter().any(|item| lowered == *item)
}

fn parse_ident(value: &str) -> Result<String, HookError> {
    if value.is_empty() || value.len() > MAX_IDENT_BYTES {
        return Err(HookError::InvalidIdent);
    }
    if value.contains('\0') || value.chars().any(char::is_control) {
        return Err(HookError::InvalidIdent);
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
    {
        return Err(HookError::InvalidIdent);
    }
    Ok(value.to_owned())
}

fn valid_text(value: &str) -> bool {
    !value.is_empty() && !value.contains('\0') && !value.chars().any(char::is_control)
}

fn object_map(value: &Value) -> Result<&Map<String, Value>, HookError> {
    value.as_object().ok_or(HookError::InvalidJson)
}

fn expect_schema(object: &Map<String, Value>) -> Result<(), HookError> {
    let got = require_str(object, "schema")?;
    if got != HOOK_SPEC_SCHEMA {
        return Err(HookError::UnsupportedSchema);
    }
    match object.get("schema_version") {
        Some(Value::Number(number)) => {
            let version = number
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .ok_or(HookError::UnsupportedSchema)?;
            if version != HOOK_SPEC_SCHEMA_VERSION {
                return Err(HookError::UnsupportedSchema);
            }
            Ok(())
        }
        Some(_) => Err(HookError::InvalidJson),
        None => Err(HookError::MissingField),
    }
}

fn expect_keys(
    object: &Map<String, Value>,
    required: &[&str],
    optional: &[&str],
) -> Result<(), HookError> {
    for key in object.keys() {
        if !required.contains(&key.as_str()) && !optional.contains(&key.as_str()) {
            return Err(HookError::UnknownField);
        }
    }
    for key in required {
        if optional.contains(key) {
            continue;
        }
        if !object.contains_key(*key) {
            return Err(HookError::MissingField);
        }
    }
    Ok(())
}

fn require_str<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, HookError> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(HookError::InvalidJson),
        None => Err(HookError::MissingField),
    }
}

fn require_object<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a Map<String, Value>, HookError> {
    match object.get(key) {
        Some(Value::Object(value)) => Ok(value),
        Some(_) => Err(HookError::InvalidJson),
        None => Err(HookError::MissingField),
    }
}

fn require_array<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a [Value], HookError> {
    match object.get(key) {
        Some(Value::Array(value)) => Ok(value.as_slice()),
        Some(_) => Err(HookError::InvalidJson),
        None => Err(HookError::MissingField),
    }
}

fn optional_array<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a [Value], HookError> {
    match object.get(key) {
        Some(Value::Array(value)) => Ok(value.as_slice()),
        Some(Value::Null) | None => Ok(&[]),
        Some(_) => Err(HookError::InvalidJson),
    }
}

fn network_as_str(network: SandboxNetwork) -> &'static str {
    match network {
        SandboxNetwork::None => "none",
        SandboxNetwork::Allowlist => "allowlist",
        SandboxNetwork::Proxy => "proxy",
    }
}

impl HookError {
    pub fn code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::SandboxTierUnavailable => Some(ErrorCode::SandboxTierUnavailable),
            Self::InvalidTimeout => Some(ErrorCode::ProcessTimeout),
            Self::AmbientHostFilesystem
            | Self::AmbientNetwork
            | Self::UnknownCapability
            | Self::FamilyMismatch
            | Self::NetworkNotIsolated
            | Self::EnvNotAllowlisted
            | Self::SecretNotMaterialized
            | Self::UnsupportedKind => Some(ErrorCode::PluginCapabilityDenied),
            Self::LeaseRequired | Self::LeaseInvalid => Some(ErrorCode::PolicyLeaseInvalid),
            Self::Spawn | Self::Wait => Some(ErrorCode::InternalUnexpected),
            _ => Some(ErrorCode::ConfigInvalid),
        }
    }

    pub fn into_api_error(self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        Some(
            ApiError::new(code, self.as_str(), trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "hook execution cancelled",
            Self::TooLarge => "hook document exceeds the configured bound",
            Self::InvalidJson => "hook document is not a closed JSON object",
            Self::UnsupportedSchema => "unsupported hook schema version",
            Self::UnknownField => "unknown hook field",
            Self::MissingField => "missing required hook field",
            Self::InvalidIdent => "hook identifier is invalid",
            Self::InvalidEvent => "hook event is invalid",
            Self::InvalidKind => "hook kind is invalid",
            Self::InvalidMatcher => "hook matcher is invalid",
            Self::InvalidTimeout => "hook timeout is invalid",
            Self::InvalidOutputLimit => "hook output limit is invalid",
            Self::InvalidArgv => "hook command argv is invalid",
            Self::InvalidUrl => "hook url is invalid",
            Self::InvalidEnvName => "hook environment name is invalid",
            Self::TooManyArgs => "hook argv exceeds the bound",
            Self::TooManyEnvNames => "hook environment allowlist exceeds the bound",
            Self::TooManyCaps => "hook requested capability count exceeds the bound",
            Self::TooManySpecs => "hook manager spec count exceeds the bound",
            Self::DuplicateId => "hook manager contains a duplicate hook id",
            Self::UnknownCapability => "unknown privileged hook capability",
            Self::AmbientHostFilesystem => "hook cannot declare ambient host filesystem",
            Self::AmbientNetwork => "hook cannot declare ambient host network",
            Self::FamilyMismatch => "hook capability family does not match resource",
            Self::UnsupportedKind => "hook kind is not an out-of-process command",
            Self::SandboxTierUnavailable => "hook sandbox tier is unavailable",
            Self::NetworkNotIsolated => "hook sandbox network must be none",
            Self::EnvNotAllowlisted => "hook environment name is not on the sandbox allowlist",
            Self::SecretNotMaterialized => "hook cannot receive an unresolved secret handle",
            Self::LeaseRequired => "hook command requires a proc.exec lease",
            Self::LeaseInvalid => "hook lease is not bound to this command",
            Self::PayloadTooLarge => "hook event payload exceeds the stdin bound",
            Self::Spawn => "hook process spawn failed",
            Self::Wait => "hook process wait failed",
        }
    }
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for HookError {}

impl fmt::Debug for HookExecContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookExecContext")
            .field("cwd", &self.cwd.as_str())
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("principal", &self.principal)
            .field("session_id", &self.session_id)
            .field("trace_id", &self.trace_id)
            .field("canaries", &self.secret_canaries.len())
            .finish()
    }
}

impl fmt::Debug for HookCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookCapture")
            .field("digest", &self.digest)
            .field("bytes", &self.bytes)
            .field("excerpt_len", &self.excerpt.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl fmt::Debug for HookRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookRecord")
            .field("hook_id", &self.hook_id)
            .field("event", &self.event)
            .field("job_id", &self.job_id)
            .field("lease_id", &self.lease_id)
            .field("status", &self.status)
            .field("failure_policy", &self.failure_policy)
            .field("decision", &self.decision)
            .field("disposition", &self.disposition)
            .field("grant_attempted", &self.grant_attempted)
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .field("sandbox_tier", &self.sandbox_tier)
            .finish()
    }
}

impl fmt::Debug for HookDispatchResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookDispatchResult")
            .field("records", &self.records.len())
            .field("disposition", &self.disposition)
            .finish()
    }
}

impl Serialize for HookSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("HookSpec", 11)?;
        state.serialize_field("schema", HOOK_SPEC_SCHEMA)?;
        state.serialize_field("schema_version", &HOOK_SPEC_SCHEMA_VERSION)?;
        state.serialize_field("id", self.id.as_str())?;
        state.serialize_field("event", self.event.as_str())?;
        match &self.matcher {
            Some(matcher) => state.serialize_field("matcher", matcher.as_str())?,
            None => state.serialize_field("matcher", &Value::Null)?,
        }
        state.serialize_field("kind", self.kind.as_str())?;
        match &self.kind {
            HookKind::Command { argv } => state.serialize_field("command", argv)?,
            HookKind::Http { url } => state.serialize_field("url", url)?,
            HookKind::Plugin { plugin, operation } => {
                state.serialize_field("plugin", plugin)?;
                state.serialize_field("operation", operation)?;
            }
        }
        state.serialize_field("timeout_ms", &(self.timeout.as_millis() as u64))?;
        state.serialize_field("failure_policy", self.failure_policy.as_str())?;
        state.serialize_field("requested_caps", &self.requested_caps)?;
        state.serialize_field("sandbox", &self.sandbox)?;
        state.end()
    }
}

impl Serialize for HookRequestedCap {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("HookRequestedCap", 2)?;
        state.serialize_field("capability", &self.capability)?;
        state.serialize_field("resource", &self.resource)?;
        state.end()
    }
}

impl Serialize for HookSandboxProfile {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("HookSandboxProfile", 4)?;
        state.serialize_field("tier", &self.tier)?;
        state.serialize_field("network", network_as_str(self.network))?;
        state.serialize_field("env_allowlist", &self.env_allowlist)?;
        state.serialize_field("output_limit", &self.output_limit)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for HookSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        decode_spec(&value, &CancellationToken::new()).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Instant;

    use capability_broker::{
        ActionRequest, ApprovalChoice, ApprovalResolution, ApprovalScopeId, CanonicalAction,
        CanonicalHostPath, ExecIntent, LeaseIssuer, LeaseUseGuard, LeaseValidator, PolicyDocument,
        PolicyRevision, PolicySource, PolicyStack, Resolver, evaluate, issue, normalize_exec,
        request_approval, validate_use,
    };
    use process_supervisor::{Invocation, SpawnError};

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-7c1e9b";

    struct FrozenPathResolver;

    impl Resolver for FrozenPathResolver {
        fn resolve_cwd(
            &self,
            requested: &str,
        ) -> Result<CanonicalHostPath, capability_broker::CommandNormalizeError> {
            CanonicalHostPath::from_resolved(requested)
        }

        fn resolve_executable(
            &self,
            requested: &str,
            _cwd: &CanonicalHostPath,
        ) -> Result<CanonicalHostPath, capability_broker::CommandNormalizeError> {
            CanonicalHostPath::from_resolved(requested)
                .map_err(|_| capability_broker::CommandNormalizeError::UnresolvedExecutable)
        }
    }

    fn principal() -> PrincipalRef {
        PrincipalRef::parse("agent").expect("principal")
    }

    fn parse_doc(src: &str, source: PolicySource) -> PolicyDocument {
        PolicyDocument::parse_toml(src, source, &CancellationToken::new()).expect("parse")
    }

    fn hook_stack() -> PolicyStack {
        PolicyStack::new([
            parse_doc(
                r#"
[[rules]]
id = "hook-allow"
effect = "allow"
subjects = ["*"]
capability = "proc.exec"
resource = { command_family = "hook" }
"#,
                PolicySource::user("user-policy.toml").expect("user"),
            ),
            parse_doc(
                r#"
[[rules]]
id = "hook-ask"
effect = "ask"
subjects = ["*"]
capability = "proc.exec"
"#,
                PolicySource::trusted_project(".rapidlm/policy.toml").expect("project"),
            ),
        ])
        .expect("stack")
    }

    fn issuer() -> LeaseIssuer {
        LeaseIssuer::from_key([0x22; 32]).expect("issuer")
    }

    fn lease_guard(spec: &ExecSpec) -> LeaseUseGuard {
        let binding = spec.binding().expect("bound spec");
        let env_names = spec.env().keys().cloned();
        let intent = match spec.invocation() {
            Invocation::Argv { argv } => {
                ExecIntent::argv(argv.clone(), spec.cwd().as_str().to_owned(), env_names)
            }
            Invocation::Shell { shell, script } => ExecIntent::shell(
                shell.clone(),
                script.clone(),
                spec.cwd().as_str().to_owned(),
                env_names,
            ),
        };
        let command = normalize_exec(&intent, &FrozenPathResolver, spec.cancel()).expect("canon");
        let actual = CanonicalAction::Command(command);
        let request = ActionRequest::new(
            binding.principal().clone(),
            binding.session_id(),
            binding.capability(),
            binding.resource().clone(),
            actual.clone(),
            "hook",
        )
        .expect("request");
        let now = Instant::now();
        let decision = evaluate(&hook_stack(), &request, &CancellationToken::new()).expect("eval");
        let approval =
            request_approval(&request, &decision, now, &CancellationToken::new()).expect("ask");
        let approved = match approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                &request,
                now,
                &CancellationToken::new(),
            )
            .expect("resolve")
        {
            ApprovalResolution::Approved(approved) => approved,
            ApprovalResolution::Denied => panic!("expected approved"),
        };
        let lease = issue(
            &issuer(),
            &approved,
            &hook_stack(),
            now,
            &CancellationToken::new(),
        )
        .expect("issue");
        let validator = LeaseValidator::new(issuer(), PolicyRevision::of_stack(&hook_stack()));
        validate_use(&validator, &lease, &actual, now, &CancellationToken::new()).expect("guard")
    }

    fn temp_cwd() -> CanonicalHostPath {
        let tmp = std::env::temp_dir().canonicalize().expect("temp");
        let rendered = tmp.to_str().expect("utf8 temp").replace('\\', "/");
        CanonicalHostPath::from_resolved(&rendered).expect("cwd")
    }

    fn require_bin(path: &str) -> String {
        assert!(
            Path::new(path).is_file(),
            "missing test fixture binary {path}"
        );
        path.to_owned()
    }

    fn ctx() -> HookExecContext {
        HookExecContext::new(
            CancellationToken::new(),
            temp_cwd(),
            None::<(String, String)>,
            principal(),
            SessionId::new(),
            TraceId::new(),
        )
        .expect("ctx")
        .with_secret_canaries([CANARY.as_bytes()])
        .expect("canaries")
    }

    fn command_spec(id: &str, event: HookEvent, argv: &[&str], policy: FailurePolicy) -> HookSpec {
        HookSpec::new(
            HookId::parse(id).expect("id"),
            event,
            None,
            HookKind::Command {
                argv: argv.iter().map(|s| (*s).to_owned()).collect(),
            },
            Duration::from_secs(2),
            policy,
            Vec::new(),
            HookSandboxProfile::isolated_host(),
        )
        .expect("spec")
    }

    fn event(kind: HookEvent) -> HookEventInput {
        HookEventInput::new(kind, "fs.write", None::<(String, Value)>).expect("event")
    }

    fn valid_spec_json(command: &str) -> String {
        format!(
            r#"{{"schema":"rapidlm.hook_spec","schema_version":1,"id":"lint.pre","event":"tool.pre","matcher":null,"kind":"command","command":["{command}"],"timeout_ms":2000,"failure_policy":"block","requested_caps":[],"sandbox":{{"tier":"host-restricted","network":"none","env_allowlist":[],"output_limit":65536}}}}"#
        )
    }

    #[test]
    fn golden_spec_round_trip() {
        let json = valid_spec_json("/usr/bin/true");
        let spec = parse_hook_spec(json.as_bytes(), &CancellationToken::new()).expect("parse");
        assert_eq!(spec.id().as_str(), "lint.pre");
        assert_eq!(spec.event(), HookEvent::ToolPre);
        assert!(spec.event().is_blocking());
        assert_eq!(spec.failure_policy(), FailurePolicy::Block);
        assert_eq!(spec.sandbox().tier(), SandboxTier::HostRestricted);
        assert_eq!(spec.sandbox().network(), SandboxNetwork::None);
        let encoded = serde_json::to_string(&spec).expect("encode");
        let decoded: HookSpec = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, spec);
    }

    #[test]
    fn blocking_events_default_fail_closed_passive_fail_open() {
        let blocking = parse_hook_spec(
            br#"{"schema":"rapidlm.hook_spec","schema_version":1,"id":"sec.pre","event":"commit.pre","kind":"command","command":["/usr/bin/true"],"sandbox":{"tier":"host-restricted"}}"#,
            &CancellationToken::new(),
        )
        .expect("blocking");
        assert_eq!(blocking.failure_policy(), FailurePolicy::Block);
        let passive = parse_hook_spec(
            br#"{"schema":"rapidlm.hook_spec","schema_version":1,"id":"note.post","event":"tool.post","kind":"command","command":["/usr/bin/true"],"sandbox":{"tier":"host-restricted"}}"#,
            &CancellationToken::new(),
        )
        .expect("passive");
        assert_eq!(passive.failure_policy(), FailurePolicy::Ignore);
    }

    #[test]
    fn missing_sandbox_profile_fails_closed() {
        let err = parse_hook_spec(
            br#"{"schema":"rapidlm.hook_spec","schema_version":1,"id":"x","event":"tool.pre","kind":"command","command":["/usr/bin/true"]}"#,
            &CancellationToken::new(),
        )
        .expect_err("sandbox required");
        assert_eq!(err, HookError::MissingField);
    }

    #[test]
    fn stronger_tier_does_not_downgrade() {
        let err = HookSandboxProfile::new(
            SandboxTier::Gvisor,
            SandboxNetwork::None,
            None::<String>,
            DEFAULT_HOOK_OUTPUT_BYTES,
        )
        .expect_err("no downgrade");
        assert_eq!(err, HookError::SandboxTierUnavailable);
        assert_eq!(
            err.into_api_error(TraceId::new()).expect("api").code(),
            ErrorCode::SandboxTierUnavailable
        );
    }

    #[test]
    fn network_profile_cannot_be_widened() {
        let err = HookSandboxProfile::new(
            SandboxTier::HostRestricted,
            SandboxNetwork::Allowlist,
            None::<String>,
            DEFAULT_HOOK_OUTPUT_BYTES,
        )
        .expect_err("network");
        assert_eq!(err, HookError::NetworkNotIsolated);
    }

    #[test]
    fn ambient_host_fs_and_network_caps_fail() {
        let fs = parse_hook_spec(
            br#"{"schema":"rapidlm.hook_spec","schema_version":1,"id":"bad.fs","event":"tool.pre","kind":"command","command":["/usr/bin/true"],"requested_caps":[{"capability":{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"},"resource":{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"host","glob":"**"}}],"sandbox":{"tier":"host-restricted"}}"#,
            &CancellationToken::new(),
        )
        .expect_err("host fs");
        assert_eq!(fs, HookError::AmbientHostFilesystem);

        let net = parse_hook_spec(
            br#"{"schema":"rapidlm.hook_spec","schema_version":1,"id":"bad.net","event":"tool.pre","kind":"command","command":["/usr/bin/true"],"requested_caps":[{"capability":{"schema":"rapidlm.capability","schema_version":1,"family":"net","action":"connect"},"resource":{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"network","scheme":"https","host":"example.com","port":443}}],"sandbox":{"tier":"host-restricted"}}"#,
            &CancellationToken::new(),
        )
        .expect_err("net");
        assert_eq!(net, HookError::AmbientNetwork);
    }

    #[test]
    fn http_and_plugin_kinds_are_not_executed() {
        let spec = parse_hook_spec(
            br#"{"schema":"rapidlm.hook_spec","schema_version":1,"id":"web","event":"tool.pre","kind":"http","url":"https://example.invalid/hook","sandbox":{"tier":"host-restricted"}}"#,
            &CancellationToken::new(),
        )
        .expect("http spec");
        let ctx = ctx();
        let err = prepare_command(&spec, &ctx, b"{}").expect_err("http");
        assert_eq!(err, HookError::UnsupportedKind);
        assert!(!err.to_string().contains("example.invalid"));
    }

    #[test]
    fn secret_canary_is_redacted_from_payload_and_logs() {
        let spec = command_spec(
            "redact",
            HookEvent::ToolPre,
            &["/usr/bin/true"],
            FailurePolicy::Block,
        );
        let event = HookEventInput::new(
            HookEvent::ToolPre,
            "fs.write",
            [
                ("note".to_owned(), Value::String(format!("x{CANARY}y"))),
                ("token".to_owned(), Value::String(CANARY.to_owned())),
                ("grant".to_owned(), Value::String("fs.write".to_owned())),
            ],
        )
        .expect("event");
        let ctx = ctx();
        let payload = redact_event_payload(&spec, &event, &ctx).expect("redact");
        let encoded = serde_json::to_string(&payload).expect("json");
        assert!(!encoded.contains(CANARY));
        assert_eq!(
            payload["fields"]["note"],
            Value::String(format!("x{REDACTED}y"))
        );
        assert!(payload["fields"].get("token").is_none());
        assert!(payload["fields"].get("grant").is_none());
        assert!(!format!("{ctx:?}").contains(CANARY));
        assert!(!HookError::PayloadTooLarge.to_string().contains(CANARY));
    }

    #[test]
    fn env_not_on_allowlist_fails_closed() {
        let spec = command_spec(
            "env",
            HookEvent::ToolPost,
            &["/usr/bin/env"],
            FailurePolicy::Ignore,
        );
        let ctx = HookExecContext::new(
            CancellationToken::new(),
            temp_cwd(),
            [("PATH", "/bin")],
            principal(),
            SessionId::new(),
            TraceId::new(),
        )
        .expect("ctx");
        let err = prepare_command(&spec, &ctx, b"{}").expect_err("allowlist");
        assert_eq!(err, HookError::EnvNotAllowlisted);
    }

    #[cfg(unix)]
    #[test]
    fn hook_does_not_inherit_parent_environment() {
        assert!(
            std::env::var_os("PATH").is_some() || std::env::var_os("HOME").is_some(),
            "parent process must have PATH or HOME so inheritance is observable"
        );
        let env_bin = require_bin("/usr/bin/env");
        let spec = command_spec(
            "env.scan",
            HookEvent::ToolPost,
            &[&env_bin],
            FailurePolicy::Ignore,
        );
        let ctx = ctx();
        let input = event(HookEvent::ToolPost);
        let exec = prepare_command(&spec, &ctx, b"").expect("prep");
        assert!(exec.env().is_empty());
        let record = run_command_hook(&spec, &input, &ctx, lease_guard(&exec)).expect("run");
        assert_eq!(record.status(), HookRunStatus::Succeeded);
        let stdout = String::from_utf8_lossy(record.stdout().excerpt());
        assert_eq!(stdout.trim(), "");
        assert!(!stdout.contains("PATH="));
        assert!(!stdout.contains("HOME="));
        assert!(!stdout.contains("USER="));
        assert!(!stdout.contains(CANARY));
    }

    #[cfg(unix)]
    #[test]
    fn timeout_and_nonzero_exit_are_distinguishable() {
        let sleep = require_bin("/bin/sleep");
        let mut timeout_spec = command_spec(
            "slow",
            HookEvent::ToolPre,
            &[&sleep, "5"],
            FailurePolicy::Block,
        );
        timeout_spec.timeout = Duration::from_millis(150);
        let ctx = ctx();
        let event = event(HookEvent::ToolPre);
        let payload =
            serde_json::to_vec(&redact_event_payload(&timeout_spec, &event, &ctx).expect("p"))
                .expect("bytes");
        let exec = prepare_command(&timeout_spec, &ctx, &payload).expect("prep");
        let timed =
            run_command_hook(&timeout_spec, &event, &ctx, lease_guard(&exec)).expect("timeout");
        assert_eq!(timed.status(), HookRunStatus::TimedOut);
        assert!(timed.status().is_timeout());
        assert!(!timed.status().is_nonzero_exit());
        assert_eq!(timed.disposition(), HookDisposition::Block);

        let false_bin = require_bin("/usr/bin/false");
        let fail_spec = command_spec(
            "fail",
            HookEvent::ToolPre,
            &[&false_bin],
            FailurePolicy::Warn,
        );
        let exec = prepare_command(&fail_spec, &ctx, &payload).expect("prep fail");
        let failed = run_command_hook(&fail_spec, &event, &ctx, lease_guard(&exec)).expect("fail");
        assert!(failed.status().is_nonzero_exit());
        assert!(!failed.status().is_timeout());
        assert_eq!(failed.disposition(), HookDisposition::Warn);
        assert_ne!(timed.status(), failed.status());
    }

    #[cfg(unix)]
    #[test]
    fn ignore_policy_continues_on_nonzero_exit() {
        let false_bin = require_bin("/usr/bin/false");
        let spec = command_spec(
            "note",
            HookEvent::TurnEnd,
            &[&false_bin],
            FailurePolicy::Ignore,
        );
        let ctx = ctx();
        let event = event(HookEvent::TurnEnd);
        let exec = prepare_command(&spec, &ctx, b"").expect("prep");
        let record = run_command_hook(&spec, &event, &ctx, lease_guard(&exec)).expect("run");
        assert!(record.status().is_nonzero_exit());
        assert_eq!(record.disposition(), HookDisposition::Continue);
    }

    #[cfg(unix)]
    #[test]
    fn hook_output_cannot_grant_permissions() {
        let printf = require_bin("/usr/bin/printf");
        let spec = command_spec(
            "evil",
            HookEvent::ToolPre,
            &[
                &printf,
                r#"{"decision":"continue","grant":{"capability":"fs.write"},"permissions":["*"],"lease":"stolen"}"#,
            ],
            FailurePolicy::Block,
        );
        let ctx = ctx();
        let event = event(HookEvent::ToolPre);
        let payload = serde_json::to_vec(&redact_event_payload(&spec, &event, &ctx).expect("p"))
            .expect("bytes");
        let exec = prepare_command(&spec, &ctx, &payload).expect("prep");
        let record = run_command_hook(&spec, &event, &ctx, lease_guard(&exec)).expect("run");
        assert_eq!(record.status(), HookRunStatus::Succeeded);
        assert!(record.grant_attempted());
        assert_eq!(record.decision(), HookDecision::Continue);
        assert_eq!(record.disposition(), HookDisposition::Continue);
    }

    #[cfg(unix)]
    #[test]
    fn blocking_event_honors_block_decision_passive_cannot_escalate() {
        let printf = require_bin("/usr/bin/printf");
        let blocking = command_spec(
            "gate",
            HookEvent::FilePreWrite,
            &[&printf, r#"{"decision":"block"}"#],
            FailurePolicy::Ignore,
        );
        let ctx = ctx();
        let blocking_event = event(HookEvent::FilePreWrite);
        let payload =
            serde_json::to_vec(&redact_event_payload(&blocking, &blocking_event, &ctx).expect("p"))
                .expect("bytes");
        let exec = prepare_command(&blocking, &ctx, &payload).expect("prep");
        let record =
            run_command_hook(&blocking, &blocking_event, &ctx, lease_guard(&exec)).expect("run");
        assert_eq!(record.decision(), HookDecision::Block);
        assert_eq!(record.disposition(), HookDisposition::Block);

        let passive = command_spec(
            "noise",
            HookEvent::ToolPost,
            &[&printf, r#"{"decision":"block"}"#],
            FailurePolicy::Ignore,
        );
        let passive_event = event(HookEvent::ToolPost);
        let payload =
            serde_json::to_vec(&redact_event_payload(&passive, &passive_event, &ctx).expect("p"))
                .expect("bytes");
        let exec = prepare_command(&passive, &ctx, &payload).expect("prep");
        let record =
            run_command_hook(&passive, &passive_event, &ctx, lease_guard(&exec)).expect("run");
        assert_eq!(record.decision(), HookDecision::Continue);
        assert_eq!(record.disposition(), HookDisposition::Continue);
    }

    #[cfg(unix)]
    #[test]
    fn redacted_payload_is_delivered_on_stdin() {
        let cat = require_bin("/bin/cat");
        let spec = command_spec("cat", HookEvent::ToolPre, &[&cat], FailurePolicy::Block);
        let event = HookEventInput::new(
            HookEvent::ToolPre,
            "fs.write",
            [("path".to_owned(), Value::String("src/lib.rs".to_owned()))],
        )
        .expect("event");
        let ctx = ctx();
        let payload = serde_json::to_vec(&redact_event_payload(&spec, &event, &ctx).expect("p"))
            .expect("bytes");
        let exec = prepare_command(&spec, &ctx, &payload).expect("prep");
        let record = run_command_hook(&spec, &event, &ctx, lease_guard(&exec)).expect("run");
        let stdout = String::from_utf8_lossy(record.stdout().excerpt());
        assert!(stdout.contains("rapidlm.hook_event"));
        assert!(stdout.contains("src/lib.rs"));
        assert!(!stdout.contains(CANARY));
        assert_eq!(
            record.stdout().digest(),
            ArtifactId::from_bytes(record.stdout().excerpt())
        );
    }

    #[cfg(unix)]
    #[test]
    fn mismatched_lease_cannot_spawn() {
        let true_bin = require_bin("/usr/bin/true");
        let sleep = require_bin("/bin/sleep");
        let spec_a = command_spec(
            "a",
            HookEvent::ToolPost,
            &[&true_bin],
            FailurePolicy::Ignore,
        );
        let spec_b = command_spec(
            "b",
            HookEvent::ToolPost,
            &[&sleep, "1"],
            FailurePolicy::Ignore,
        );
        let ctx = ctx();
        let exec_a = prepare_command(&spec_a, &ctx, b"").expect("a");
        let exec_b = prepare_command(&spec_b, &ctx, b"").expect("b");
        let err = spawn(exec_b, lease_guard(&exec_a)).expect_err("mismatch");
        assert_eq!(err, SpawnError::LeaseNotBound);
        assert_eq!(map_spawn(err), HookError::LeaseInvalid);
    }

    #[cfg(unix)]
    #[test]
    fn cancel_is_distinct_from_timeout() {
        let sleep = require_bin("/bin/sleep");
        let spec = command_spec(
            "cancel",
            HookEvent::SessionStart,
            &[&sleep, "5"],
            FailurePolicy::Ignore,
        );
        let ctx = ctx();
        let exec = prepare_command(&spec, &ctx, b"").expect("prep");
        let lease = lease_guard(&exec);
        ctx.cancel.cancel();
        let result = run_command_hook(&spec, &event(HookEvent::SessionStart), &ctx, lease);
        match result {
            Ok(record) => {
                assert_eq!(record.status(), HookRunStatus::Cancelled);
                assert!(!record.status().is_timeout());
            }
            Err(HookError::Cancelled) => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn argv_metacharacters_are_not_a_shell() {
        let echo = require_bin("/bin/echo");
        let spec = command_spec(
            "echo",
            HookEvent::TurnStart,
            &[&echo, "hello; echo pwned"],
            FailurePolicy::Ignore,
        );
        let ctx = ctx();
        let event = event(HookEvent::TurnStart);
        let exec = prepare_command(&spec, &ctx, b"").expect("prep");
        let record = run_command_hook(&spec, &event, &ctx, lease_guard(&exec)).expect("run");
        let stdout = String::from_utf8_lossy(record.stdout().excerpt());
        assert_eq!(stdout.trim(), "hello; echo pwned");
        assert_eq!(stdout.lines().count(), 1);
    }

    #[test]
    fn hook_event_registry_covers_lifecycle_contract() {
        // The architecture contract names every lifecycle family; the
        // registry must round-trip all of them through the wire names.
        let expected_blocking = [
            "tool.pre",
            "file.pre_write",
            "patch.pre",
            "compact.pre",
            "context.pre",
            "model.pre",
            "commit.pre",
            "permission.request",
            "resource.acquire",
        ];
        for wire in expected_blocking {
            let parsed = HookEvent::parse(wire).expect("blocking event parses");
            assert!(parsed.is_blocking(), "{wire} must classify blocking");
        }
        let passive = [
            "session.start",
            "session.end",
            "turn.start",
            "turn.end",
            "goal.start",
            "goal.complete",
            "context.post",
            "model.post",
            "tool.post",
            "tool.failure",
            "file.post_write",
            "patch.post",
            "compact.post",
            "agent.start",
            "agent.stop",
            "verifier.start",
            "verifier.stop",
            "resource.release",
        ];
        for wire in passive {
            let parsed = HookEvent::parse(wire).expect("passive event parses");
            assert!(!parsed.is_blocking(), "{wire} must stay passive");
        }
        // Every declared variant round-trips and is listed exactly once.
        let mut seen = std::collections::BTreeSet::new();
        for item in HookEvent::ALL {
            assert_eq!(HookEvent::parse(item.as_str()).expect("round trip"), *item);
            assert!(seen.insert(item.as_str()), "duplicate wire name");
        }
        assert_eq!(seen.len(), HookEvent::ALL.len());
    }

    #[test]
    fn new_gate_events_default_fail_closed_like_other_blocking_events() {
        for wire in ["patch.pre", "permission.request", "resource.acquire"] {
            let spec_text = format!(
                r#"{{"schema":"rapidlm.hook_spec","schema_version":1,"id":"h-gate","event":"{wire}","kind":"command","command":["/usr/bin/true"],"sandbox":{{"tier":"host-restricted"}}}}"#
            );
            let spec =
                parse_hook_spec(spec_text.as_bytes(), &CancellationToken::new()).expect("gate");
            assert_eq!(
                spec.failure_policy(),
                FailurePolicy::Block,
                "{wire} default policy must be fail-closed"
            );
        }
    }
}
