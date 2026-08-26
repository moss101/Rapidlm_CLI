//! Approval request model.
//!
//! Ask decisions become an [`ApprovalRequest`] with a risk summary, the exact
//! normalized action fingerprint, and a closed list of safe scopes. Free-form
//! lease bounds are rejected. Approvals expire. A mutated action after the
//! request cannot be resolved into [`ApprovedAction`].

use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::{ArtifactId, SessionId};

use crate::capability::{Capability, ResourceDescriptor};
use crate::normalize::command::{CancellationToken, CanonicalCommand, ShellMode};
use crate::normalize::fs::CanonicalFsAction;
use crate::normalize::network::CanonicalNetworkTarget;
use crate::policy::evaluator::{
    ActionRequest, AskExplanation, CanonicalAction, Decision, DecisionWithTrace, LeaseConstraints,
    PrincipalRef, RiskClass,
};
use crate::policy::parser::RuleId;

/// In-process approval lifetime. Matches the default lease TTL.
pub const DEFAULT_APPROVAL_TTL_SECS: u32 = LeaseConstraints::DEFAULT_MAX_TTL_SECS;

/// Maximum UTF-8 bytes in the human-readable action diff.
pub const MAX_ACTION_DIFF_BYTES: usize = 1024;

const BINDING_TAG: &[u8] = b"rapidlm.approval.binding.v1";

/// Opaque in-process approval identity. Not a lease and not transferable.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct ApprovalId([u8; 16]);

/// SHA-256 of the normalized action binding. Reason text is never hashed.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct ActionFingerprint([u8; 32]);

/// Closed scope identifiers. Unknown strings cannot construct a broader grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ApprovalScopeId {
    /// One use of the exact bound action. Default.
    Once,
    /// Remember this exact action fingerprint for the session. Still not blanket.
    SessionExact,
}

/// How a listed scope may be remembered. Does not broaden the resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ApprovalScopeKind {
    OneShot,
    SessionExact,
}

/// One predefined safe scope. Constraints are never caller-authored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApprovalScopeChoice {
    id: ApprovalScopeId,
    kind: ApprovalScopeKind,
    constraints: LeaseConstraints,
}

/// Listed one-shot / default-safe scopes. Free-form broad leases are absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalSpec {
    default: ApprovalScopeId,
    choices: Vec<ApprovalScopeChoice>,
}

/// Coarse risk plus policy explanation. Reason is untrusted display data.
#[derive(Clone, Eq, PartialEq)]
pub struct RiskSummary {
    class: RiskClass,
    capability: Capability,
    explanation: String,
    ask_rule_ids: Vec<RuleId>,
    reason: String,
}

/// Exact normalized action presented for review, bound by fingerprint.
#[derive(Clone, Eq, PartialEq)]
pub struct NormalizedActionDiff {
    capability: Capability,
    resource: ResourceDescriptor,
    action: CanonicalAction,
    fingerprint: ActionFingerprint,
    text: String,
}

/// Pending human decision. Expires on a monotonic deadline.
#[derive(Clone, Eq, PartialEq)]
pub struct ApprovalRequest {
    id: ApprovalId,
    principal: PrincipalRef,
    session_id: SessionId,
    spec: ApprovalSpec,
    risk: RiskSummary,
    action: NormalizedActionDiff,
    created_at: Instant,
    expires_at: Instant,
}

/// Human choice. Deny is explicit; approve must name a listed scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ApprovalChoice {
    Deny,
    Approve(ApprovalScopeId),
}

/// Outcome of a timely, unmutated resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalResolution {
    Denied,
    Approved(ApprovedAction),
}

/// Binding that later lease issuance may consume. Not an executor grant.
#[derive(Clone, Eq, PartialEq)]
pub struct ApprovedAction {
    request_id: ApprovalId,
    principal: PrincipalRef,
    session_id: SessionId,
    capability: Capability,
    resource: ResourceDescriptor,
    action_hash: ActionFingerprint,
    scope: ApprovalScopeChoice,
}

/// Typed approval failure. Display never echoes attacker-controlled input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ApprovalError {
    Cancelled,
    NotAsk,
    Denied,
    Expired,
    ActionMutated,
    UnknownScope,
    FreeFormScopeRejected,
}

impl ApprovalId {
    fn allocate(fingerprint: ActionFingerprint, session_id: SessionId) -> Self {
        let mut bytes = [0u8; 16 + 32 + 16];
        bytes[..16].copy_from_slice(session_id.as_uuid().as_bytes());
        bytes[16..48].copy_from_slice(fingerprint.as_bytes());
        bytes[48..].copy_from_slice(SessionId::new().as_uuid().as_bytes());
        let digest = ArtifactId::from_bytes(&bytes);
        let mut id = [0u8; 16];
        id.copy_from_slice(&digest.as_digest()[..16]);
        Self(id)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl ActionFingerprint {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub(crate) fn of(request: &ActionRequest) -> Self {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(BINDING_TAG);
        buf.push(0);
        buf.extend_from_slice(request.principal().as_str().as_bytes());
        buf.push(0);
        buf.extend_from_slice(request.session_id().to_string().as_bytes());
        buf.push(0);
        buf.extend_from_slice(request.capability().as_str().as_bytes());
        buf.push(0);
        append_resource_bytes(&mut buf, request.resource());
        buf.push(0);
        append_action_bytes(&mut buf, request.normalized_action());
        Self(*ArtifactId::from_bytes(&buf).as_digest())
    }
}

impl ApprovalScopeId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::SessionExact => "session-exact",
        }
    }

    /// Parse a UI token. Only listed identifiers succeed.
    pub fn parse(raw: &str) -> Result<Self, ApprovalError> {
        match raw {
            "once" | "approve_once" => Ok(Self::Once),
            "session-exact" => Ok(Self::SessionExact),
            _ => Err(ApprovalError::FreeFormScopeRejected),
        }
    }
}

impl ApprovalScopeChoice {
    pub const fn id(self) -> ApprovalScopeId {
        self.id
    }

    pub const fn kind(self) -> ApprovalScopeKind {
        self.kind
    }

    pub const fn constraints(self) -> LeaseConstraints {
        self.constraints
    }
}

impl ApprovalSpec {
    fn default_safe() -> Self {
        Self {
            default: ApprovalScopeId::Once,
            choices: vec![
                ApprovalScopeChoice {
                    id: ApprovalScopeId::Once,
                    kind: ApprovalScopeKind::OneShot,
                    constraints: LeaseConstraints::standard(),
                },
                ApprovalScopeChoice {
                    id: ApprovalScopeId::SessionExact,
                    kind: ApprovalScopeKind::SessionExact,
                    constraints: LeaseConstraints::standard(),
                },
            ],
        }
    }

    pub fn default_scope(&self) -> ApprovalScopeId {
        self.default
    }

    pub fn choices(&self) -> &[ApprovalScopeChoice] {
        &self.choices
    }

    pub fn select(&self, id: ApprovalScopeId) -> Result<ApprovalScopeChoice, ApprovalError> {
        self.choices
            .iter()
            .copied()
            .find(|choice| choice.id == id)
            .ok_or(ApprovalError::UnknownScope)
    }

    /// Resolve a UI token to a listed choice. Free-form text fails closed.
    pub fn select_raw(&self, raw: &str) -> Result<ApprovalScopeChoice, ApprovalError> {
        self.select(ApprovalScopeId::parse(raw)?)
    }
}

impl RiskSummary {
    pub const fn class(&self) -> RiskClass {
        self.class
    }

    pub const fn capability(&self) -> Capability {
        self.capability
    }

    pub fn explanation(&self) -> &str {
        &self.explanation
    }

    pub fn ask_rule_ids(&self) -> impl Iterator<Item = &str> {
        self.ask_rule_ids.iter().map(RuleId::as_str)
    }

    /// Untrusted caller reason. Never used as authority.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl NormalizedActionDiff {
    pub const fn capability(&self) -> Capability {
        self.capability
    }

    pub fn resource(&self) -> &ResourceDescriptor {
        &self.resource
    }

    pub fn action(&self) -> &CanonicalAction {
        &self.action
    }

    pub fn fingerprint(&self) -> ActionFingerprint {
        self.fingerprint
    }

    pub fn as_text(&self) -> &str {
        &self.text
    }
}

impl ApprovalRequest {
    pub fn id(&self) -> ApprovalId {
        self.id
    }

    pub fn principal(&self) -> &PrincipalRef {
        &self.principal
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn spec(&self) -> &ApprovalSpec {
        &self.spec
    }

    pub fn risk(&self) -> &RiskSummary {
        &self.risk
    }

    pub fn action_diff(&self) -> &NormalizedActionDiff {
        &self.action
    }

    pub fn action_hash(&self) -> ActionFingerprint {
        self.action.fingerprint
    }

    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }

    pub fn is_expired(&self, now: Instant) -> bool {
        now < self.created_at || now >= self.expires_at
    }

    /// Detect a changed principal/session/capability/resource/action.
    pub fn detect_mutation(&self, current: &ActionRequest) -> Result<(), ApprovalError> {
        if ActionFingerprint::of(current) != self.action.fingerprint {
            return Err(ApprovalError::ActionMutated);
        }
        Ok(())
    }

    /// Resolve a listed choice after expiry and mutation checks.
    pub fn resolve(
        &self,
        choice: ApprovalChoice,
        current: &ActionRequest,
        now: Instant,
        cancel: &CancellationToken,
    ) -> Result<ApprovalResolution, ApprovalError> {
        if cancel.is_cancelled() {
            return Err(ApprovalError::Cancelled);
        }
        if self.is_expired(now) {
            return Err(ApprovalError::Expired);
        }
        self.detect_mutation(current)?;
        match choice {
            ApprovalChoice::Deny => Ok(ApprovalResolution::Denied),
            ApprovalChoice::Approve(id) => {
                let scope = self.spec.select(id)?;
                Ok(ApprovalResolution::Approved(ApprovedAction {
                    request_id: self.id,
                    principal: self.principal.clone(),
                    session_id: self.session_id,
                    capability: self.action.capability,
                    resource: self.action.resource.clone(),
                    action_hash: self.action.fingerprint,
                    scope,
                }))
            }
        }
    }
}

impl ApprovedAction {
    pub fn request_id(&self) -> ApprovalId {
        self.request_id
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

    pub fn action_hash(&self) -> ActionFingerprint {
        self.action_hash
    }

    pub fn scope(&self) -> ApprovalScopeChoice {
        self.scope
    }

    /// Fail closed if the action about to be leased is not the approved one.
    pub fn matches_request(&self, current: &ActionRequest) -> Result<(), ApprovalError> {
        if ActionFingerprint::of(current) != self.action_hash {
            return Err(ApprovalError::ActionMutated);
        }
        Ok(())
    }
}

impl ApprovalError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "approval cancelled",
            Self::NotAsk => "approval requires an ask decision",
            Self::Denied => "denied action cannot become an approval",
            Self::Expired => "approval expired",
            Self::ActionMutated => "normalized action changed after approval request",
            Self::UnknownScope => "approval scope is not listed",
            Self::FreeFormScopeRejected => "free-form approval scope is not accepted",
        }
    }
}

impl fmt::Display for ApprovalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ApprovalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ApprovalId")
            .field(&self.to_string())
            .finish()
    }
}

impl fmt::Display for ActionFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ActionFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ActionFingerprint")
            .field(&self.to_string())
            .finish()
    }
}

impl fmt::Display for ApprovalScopeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for RiskSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RiskSummary")
            .field("class", &self.class)
            .field("capability", &self.capability.as_str())
            .field("explanation", &self.explanation)
            .field("ask_rule_ids", &self.ask_rule_ids)
            .field("reason_len", &self.reason.len())
            .finish()
    }
}

impl fmt::Debug for NormalizedActionDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NormalizedActionDiff")
            .field("capability", &self.capability.as_str())
            .field("fingerprint", &self.fingerprint)
            .field("text", &self.text)
            .finish()
    }
}

impl fmt::Debug for ApprovalRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApprovalRequest")
            .field("id", &self.id)
            .field("principal", &self.principal)
            .field("session_id", &self.session_id)
            .field("spec", &self.spec)
            .field("risk", &self.risk)
            .field("action", &self.action)
            .finish()
    }
}

impl fmt::Debug for ApprovedAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApprovedAction")
            .field("request_id", &self.request_id)
            .field("principal", &self.principal)
            .field("session_id", &self.session_id)
            .field("capability", &self.capability.as_str())
            .field("action_hash", &self.action_hash)
            .field("scope", &self.scope)
            .finish()
    }
}

impl fmt::Display for ApprovalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ApprovalError {}

/// Build an approval request from an ask decision.
///
/// Allow and deny cannot be converted into an approval. The binding hash
/// excludes the untrusted reason string.
pub fn request_approval(
    request: &ActionRequest,
    decision: &DecisionWithTrace,
    now: Instant,
    cancel: &CancellationToken,
) -> Result<ApprovalRequest, ApprovalError> {
    if cancel.is_cancelled() {
        return Err(ApprovalError::Cancelled);
    }
    match decision.decision() {
        Decision::Ask(ask) => build_request(request, decision, ask, now),
        Decision::Deny(_) => Err(ApprovalError::Denied),
        Decision::Allow(_) => Err(ApprovalError::NotAsk),
    }
}

fn build_request(
    request: &ActionRequest,
    decision: &DecisionWithTrace,
    ask: &AskExplanation,
    now: Instant,
) -> Result<ApprovalRequest, ApprovalError> {
    let ttl = Duration::from_secs(u64::from(DEFAULT_APPROVAL_TTL_SECS));
    let expires_at = now.checked_add(ttl).ok_or(ApprovalError::Expired)?;
    let fingerprint = ActionFingerprint::of(request);
    let risk = RiskSummary {
        class: decision.risk(),
        capability: request.capability(),
        explanation: decision.explanation().to_owned(),
        ask_rule_ids: ask.rule_ids().to_vec(),
        reason: request.reason().to_owned(),
    };
    let action = NormalizedActionDiff {
        capability: request.capability(),
        resource: request.resource().clone(),
        action: request.normalized_action().clone(),
        fingerprint,
        text: action_diff_text(
            request.capability(),
            request.resource(),
            request.normalized_action(),
        ),
    };
    Ok(ApprovalRequest {
        id: ApprovalId::allocate(fingerprint, request.session_id()),
        principal: request.principal().clone(),
        session_id: request.session_id(),
        spec: ApprovalSpec::default_safe(),
        risk,
        action,
        created_at: now,
        expires_at,
    })
}

fn append_action_bytes(buf: &mut Vec<u8>, action: &CanonicalAction) {
    match action {
        CanonicalAction::Command(command) => {
            buf.extend_from_slice(b"command\0");
            buf.extend_from_slice(&command.policy_bytes());
        }
        CanonicalAction::Filesystem(fs) => {
            buf.extend_from_slice(b"fs\0");
            buf.extend_from_slice(&fs.policy_bytes());
        }
        CanonicalAction::Network(net) => {
            buf.extend_from_slice(b"net\0");
            buf.extend_from_slice(&net.policy_bytes());
        }
        CanonicalAction::Resource {
            capability,
            resource,
        } => {
            buf.extend_from_slice(b"resource\0");
            buf.extend_from_slice(capability.as_str().as_bytes());
            buf.push(0);
            append_resource_bytes(buf, resource);
        }
    }
}

fn append_resource_bytes(buf: &mut Vec<u8>, resource: &ResourceDescriptor) {
    match resource {
        ResourceDescriptor::Filesystem(scope) => {
            buf.extend_from_slice(b"fs\0");
            buf.extend_from_slice(scope.root().as_str().as_bytes());
            buf.push(0);
            buf.extend_from_slice(scope.glob().as_str().as_bytes());
        }
        ResourceDescriptor::Process(scope) => {
            buf.extend_from_slice(b"proc\0");
            buf.extend_from_slice(scope.command_family().as_str().as_bytes());
        }
        ResourceDescriptor::Network(scope) => {
            buf.extend_from_slice(b"net\0");
            buf.extend_from_slice(scope.scheme().as_str().as_bytes());
            buf.push(0);
            buf.extend_from_slice(scope.host().as_str().as_bytes());
            buf.push(0);
            buf.extend_from_slice(scope.port().to_string().as_bytes());
        }
        ResourceDescriptor::Git(scope) => {
            buf.extend_from_slice(b"git\0");
            buf.extend_from_slice(scope.ref_scope().as_str().as_bytes());
        }
        ResourceDescriptor::Secret(scope) => {
            buf.extend_from_slice(b"secret\0");
            buf.extend_from_slice(scope.secret_id().as_str().as_bytes());
            buf.push(0);
            buf.extend_from_slice(scope.target().as_str().as_bytes());
        }
        ResourceDescriptor::Browser(scope) => {
            buf.extend_from_slice(b"browser\0");
            buf.extend_from_slice(scope.origin().to_string().as_bytes());
            buf.push(0);
            if let Some(path) = scope.path() {
                buf.extend_from_slice(path.as_str().as_bytes());
            }
        }
        ResourceDescriptor::Mobile(scope) => {
            buf.extend_from_slice(b"mobile\0");
            buf.extend_from_slice(scope.device_id().as_str().as_bytes());
        }
        ResourceDescriptor::Mcp(scope) => {
            buf.extend_from_slice(b"mcp\0");
            buf.extend_from_slice(scope.server().as_bytes());
            buf.push(0);
            buf.extend_from_slice(scope.tool().as_bytes());
        }
        ResourceDescriptor::Plugin(scope) => {
            buf.extend_from_slice(b"plugin\0");
            buf.extend_from_slice(scope.plugin().as_bytes());
            buf.push(0);
            buf.extend_from_slice(scope.capability().as_bytes());
        }
    }
}

fn action_diff_text(
    capability: Capability,
    resource: &ResourceDescriptor,
    action: &CanonicalAction,
) -> String {
    let raw = match action {
        CanonicalAction::Command(command) => command_diff_text(command),
        CanonicalAction::Filesystem(fs) => fs_diff_text(fs),
        CanonicalAction::Network(net) => net_diff_text(net),
        CanonicalAction::Resource { .. } => {
            format!("{} {}", capability.as_str(), resource_scope_text(resource))
        }
    };
    truncate_diff(&raw)
}

fn command_diff_text(command: &CanonicalCommand) -> String {
    match command.mode() {
        ShellMode::Argv => format!(
            "proc.exec argv cwd={} {}",
            command.cwd().as_str(),
            command.argv().join(" ")
        ),
        ShellMode::ShellString => format!(
            "proc.exec shell executable={} cwd={} script_bytes={}",
            command.executable().as_str(),
            command.cwd().as_str(),
            command.shell_script().map(str::len).unwrap_or(0)
        ),
    }
}

fn fs_diff_text(fs: &CanonicalFsAction) -> String {
    match fs.dest() {
        Some(dest) => format!(
            "fs.{} {} {} -> {}",
            fs.operation().as_str(),
            fs.root().as_str(),
            fs.path().as_str(),
            dest.as_str()
        ),
        None => format!(
            "fs.{} {} {}",
            fs.operation().as_str(),
            fs.root().as_str(),
            fs.path().as_str()
        ),
    }
}

fn net_diff_text(net: &CanonicalNetworkTarget) -> String {
    format!(
        "net.connect {}://{}:{}",
        net.scheme().as_str(),
        net.host().as_canonical_str(),
        net.port()
    )
}

fn resource_scope_text(resource: &ResourceDescriptor) -> String {
    match resource {
        ResourceDescriptor::Filesystem(scope) => {
            format!("{}:{}", scope.root().as_str(), scope.glob().as_str())
        }
        ResourceDescriptor::Process(scope) => scope.command_family().as_str().to_owned(),
        ResourceDescriptor::Network(scope) => format!(
            "{}://{}:{}",
            scope.scheme().as_str(),
            scope.host().as_str(),
            scope.port()
        ),
        ResourceDescriptor::Git(scope) => scope.ref_scope().as_str().to_owned(),
        ResourceDescriptor::Secret(scope) => format!("target={}", scope.target().as_str()),
        ResourceDescriptor::Browser(scope) => match scope.path() {
            Some(path) => format!("{} {}", scope.origin(), path.as_str()),
            None => scope.origin().to_string(),
        },
        ResourceDescriptor::Mobile(scope) => scope.device_id().as_str().to_owned(),
        ResourceDescriptor::Mcp(scope) => format!("{}/{}", scope.server(), scope.tool()),
        ResourceDescriptor::Plugin(scope) => format!("{}/{}", scope.plugin(), scope.capability()),
    }
}

fn truncate_diff(text: &str) -> String {
    if text.len() <= MAX_ACTION_DIFF_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_ACTION_DIFF_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{FilesystemScope, ProcessScope, SecretScope};
    use crate::policy::evaluator::{PolicyStack, evaluate};
    use crate::policy::parser::{PolicyDocument, PolicySource};

    const SECRET: &str = "super-secret-password";

    fn principal() -> PrincipalRef {
        PrincipalRef::parse("agent").expect("principal")
    }

    fn parse_doc(src: &str, source: PolicySource) -> PolicyDocument {
        PolicyDocument::parse_toml(src, source, &CancellationToken::new()).expect("parse")
    }

    fn user(src: &str) -> PolicyDocument {
        parse_doc(src, PolicySource::user("user-policy.toml").expect("user"))
    }

    fn project(src: &str) -> PolicyDocument {
        parse_doc(
            src,
            PolicySource::trusted_project(".rapidlm/policy.toml").expect("project"),
        )
    }

    fn stack(docs: impl IntoIterator<Item = PolicyDocument>) -> PolicyStack {
        PolicyStack::new(docs).expect("stack")
    }

    fn repo_read_resource() -> ResourceDescriptor {
        ResourceDescriptor::Filesystem(FilesystemScope::repo("src/main.rs").expect("fs"))
    }

    fn repo_read_action() -> CanonicalAction {
        CanonicalAction::Resource {
            capability: Capability::FsRead,
            resource: repo_read_resource(),
        }
    }

    fn repo_read_request() -> ActionRequest {
        ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::FsRead,
            repo_read_resource(),
            repo_read_action(),
            "read source",
        )
        .expect("request")
    }

    fn ask_stack() -> PolicyStack {
        stack([
            user(
                r#"
[[rules]]
id = "repo-read"
effect = "allow"
subjects = ["*"]
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }
"#,
            ),
            project(
                r#"
[[rules]]
id = "repo-ask"
effect = "ask"
subjects = ["*"]
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }
"#,
            ),
        ])
    }

    fn deny_stack() -> PolicyStack {
        stack([user(
            r#"
[[rules]]
id = "repo-deny"
effect = "deny"
subjects = ["*"]
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }
"#,
        )])
    }

    fn allow_stack() -> PolicyStack {
        stack([user(
            r#"
[[rules]]
id = "repo-allow"
effect = "allow"
subjects = ["*"]
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }
"#,
        )])
    }

    fn eval(policies: &PolicyStack, request: &ActionRequest) -> DecisionWithTrace {
        evaluate(policies, request, &CancellationToken::new()).expect("evaluate")
    }

    fn request_now(request: &ActionRequest, decision: &DecisionWithTrace) -> ApprovalRequest {
        request_approval(request, decision, Instant::now(), &CancellationToken::new())
            .expect("approval")
    }

    #[test]
    fn ask_creates_risk_summary_action_diff_and_default_once_scope() {
        let request = repo_read_request();
        let decision = eval(&ask_stack(), &request);
        let approval = request_now(&request, &decision);
        assert_eq!(approval.risk().class(), RiskClass::Low);
        assert_eq!(approval.risk().capability(), Capability::FsRead);
        assert!(approval.risk().explanation().contains("repo-ask"));
        assert_eq!(
            approval.risk().ask_rule_ids().collect::<Vec<_>>(),
            ["repo-ask"]
        );
        assert_eq!(approval.risk().reason(), "read source");
        assert!(approval.action_diff().as_text().contains("fs.read"));
        assert!(approval.action_diff().as_text().contains("src/main.rs"));
        assert_eq!(approval.spec().default_scope(), ApprovalScopeId::Once);
        let ids: Vec<_> = approval.spec().choices().iter().map(|c| c.id()).collect();
        assert_eq!(ids, [ApprovalScopeId::Once, ApprovalScopeId::SessionExact]);
        let once = approval.spec().select(ApprovalScopeId::Once).expect("once");
        assert_eq!(once.kind(), ApprovalScopeKind::OneShot);
        assert_eq!(once.constraints().max_uses(), 1);
        assert_eq!(
            once.constraints().max_ttl_secs(),
            LeaseConstraints::DEFAULT_MAX_TTL_SECS
        );
        let remembered = approval
            .spec()
            .select(ApprovalScopeId::SessionExact)
            .expect("session");
        assert_eq!(remembered.kind(), ApprovalScopeKind::SessionExact);
        assert_eq!(remembered.constraints().max_uses(), 1);
    }

    #[test]
    fn deny_cannot_be_converted_into_approval() {
        let request = repo_read_request();
        let decision = eval(&deny_stack(), &request);
        let err = request_approval(
            &request,
            &decision,
            Instant::now(),
            &CancellationToken::new(),
        )
        .expect_err("deny");
        assert_eq!(err, ApprovalError::Denied);
    }

    #[test]
    fn allow_cannot_be_converted_into_approval() {
        let request = repo_read_request();
        let decision = eval(&allow_stack(), &request);
        let err = request_approval(
            &request,
            &decision,
            Instant::now(),
            &CancellationToken::new(),
        )
        .expect_err("allow");
        assert_eq!(err, ApprovalError::NotAsk);
    }

    #[test]
    fn approval_expires_before_resolution() {
        let request = repo_read_request();
        let decision = eval(&ask_stack(), &request);
        let now = Instant::now();
        let approval = request_approval(&request, &decision, now, &CancellationToken::new())
            .expect("approval");
        let later = now + Duration::from_secs(u64::from(DEFAULT_APPROVAL_TTL_SECS));
        assert!(approval.is_expired(later));
        let err = approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                &request,
                later,
                &CancellationToken::new(),
            )
            .expect_err("expired");
        assert_eq!(err, ApprovalError::Expired);
    }

    #[test]
    fn clock_regression_fails_closed_as_expired() {
        let request = repo_read_request();
        let decision = eval(&ask_stack(), &request);
        let now = Instant::now();
        let approval = request_approval(&request, &decision, now, &CancellationToken::new())
            .expect("approval");
        let earlier = now.checked_sub(Duration::from_secs(1)).expect("earlier");
        assert!(approval.is_expired(earlier));
    }

    #[test]
    fn action_mutation_is_detected_before_approved_action() {
        let request = repo_read_request();
        let decision = eval(&ask_stack(), &request);
        let approval = request_now(&request, &decision);
        let mutated_resource =
            ResourceDescriptor::Filesystem(FilesystemScope::repo("src/other.rs").expect("fs"));
        let mutated = ActionRequest::new(
            request.principal().clone(),
            request.session_id(),
            Capability::FsRead,
            mutated_resource.clone(),
            CanonicalAction::Resource {
                capability: Capability::FsRead,
                resource: mutated_resource,
            },
            request.reason(),
        )
        .expect("mutated");
        let err = approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                &mutated,
                Instant::now(),
                &CancellationToken::new(),
            )
            .expect_err("mutated");
        assert_eq!(err, ApprovalError::ActionMutated);
        assert_eq!(
            approval.detect_mutation(&mutated),
            Err(ApprovalError::ActionMutated)
        );
    }

    #[test]
    fn command_argv_mutation_changes_fingerprint() {
        let resource = ResourceDescriptor::Process(ProcessScope::new("git").expect("process"));
        let original_cmd = CanonicalCommand::try_from_parts_for_test();
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::ProcExec,
            resource.clone(),
            CanonicalAction::Command(original_cmd.0),
            "run git",
        )
        .expect("request");
        let mutated = ActionRequest::new(
            request.principal().clone(),
            request.session_id(),
            Capability::ProcExec,
            resource,
            CanonicalAction::Command(original_cmd.1),
            "run git",
        )
        .expect("mutated");
        assert_ne!(
            ActionFingerprint::of(&request),
            ActionFingerprint::of(&mutated)
        );
    }

    #[test]
    fn free_form_and_broad_scope_tokens_are_rejected() {
        let request = repo_read_request();
        let decision = eval(&ask_stack(), &request);
        let approval = request_now(&request, &decision);
        for raw in [
            "*",
            "all",
            "session",
            "session-all",
            "forever",
            "host/**",
            "repo/**",
            "max_uses=99",
            "",
            "ONCE",
        ] {
            let err = approval.spec().select_raw(raw).expect_err(raw);
            assert_eq!(err, ApprovalError::FreeFormScopeRejected, "{raw}");
        }
        assert_eq!(
            ApprovalScopeId::parse("approve_once").expect("alias"),
            ApprovalScopeId::Once
        );
        assert_eq!(
            approval.spec().select_raw("once").expect("once").id(),
            ApprovalScopeId::Once
        );
    }

    #[test]
    fn resolve_once_yields_approved_action_bound_to_fingerprint() {
        let request = repo_read_request();
        let decision = eval(&ask_stack(), &request);
        let approval = request_now(&request, &decision);
        let resolved = approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                &request,
                Instant::now(),
                &CancellationToken::new(),
            )
            .expect("resolve");
        let ApprovalResolution::Approved(approved) = resolved else {
            panic!("expected approved");
        };
        assert_eq!(approved.action_hash(), approval.action_hash());
        assert_eq!(approved.scope().id(), ApprovalScopeId::Once);
        assert_eq!(approved.capability(), Capability::FsRead);
        approved.matches_request(&request).expect("same action");
    }

    #[test]
    fn resolve_deny_does_not_issue_approved_action() {
        let request = repo_read_request();
        let decision = eval(&ask_stack(), &request);
        let approval = request_now(&request, &decision);
        let resolved = approval
            .resolve(
                ApprovalChoice::Deny,
                &request,
                Instant::now(),
                &CancellationToken::new(),
            )
            .expect("deny");
        assert_eq!(resolved, ApprovalResolution::Denied);
    }

    #[test]
    fn approved_action_rejects_later_mutation() {
        let request = repo_read_request();
        let decision = eval(&ask_stack(), &request);
        let approval = request_now(&request, &decision);
        let ApprovalResolution::Approved(approved) = approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::SessionExact),
                &request,
                Instant::now(),
                &CancellationToken::new(),
            )
            .expect("resolve")
        else {
            panic!("expected approved");
        };
        let mutated_resource =
            ResourceDescriptor::Filesystem(FilesystemScope::repo("src/leak.rs").expect("fs"));
        let mutated = ActionRequest::new(
            request.principal().clone(),
            request.session_id(),
            Capability::FsRead,
            mutated_resource.clone(),
            CanonicalAction::Resource {
                capability: Capability::FsRead,
                resource: mutated_resource,
            },
            request.reason(),
        )
        .expect("mutated");
        assert_eq!(
            approved.matches_request(&mutated),
            Err(ApprovalError::ActionMutated)
        );
    }

    #[test]
    fn cancelled_request_and_resolve_fail_closed() {
        let request = repo_read_request();
        let decision = eval(&ask_stack(), &request);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            request_approval(&request, &decision, Instant::now(), &cancel),
            Err(ApprovalError::Cancelled)
        );
        let live = CancellationToken::new();
        let approval =
            request_approval(&request, &decision, Instant::now(), &live).expect("created");
        assert_eq!(
            approval.resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                &request,
                Instant::now(),
                &cancel
            ),
            Err(ApprovalError::Cancelled)
        );
    }

    #[test]
    fn secret_reason_is_not_used_as_authority_or_debug_payload() {
        let policies = stack([
            user(
                r#"
[[rules]]
id = "secret-allow"
effect = "allow"
capability = "secret.use"
resource = { secret_id = "env:NPM_TOKEN", target = "env" }
"#,
            ),
            project(
                r#"
[[rules]]
id = "secret-ask"
effect = "ask"
capability = "secret.use"
"#,
            ),
        ]);
        let resource = ResourceDescriptor::Secret(
            SecretScope::new("env:NPM_TOKEN", "env").expect("secret scope"),
        );
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::SecretUse,
            resource.clone(),
            CanonicalAction::Resource {
                capability: Capability::SecretUse,
                resource,
            },
            SECRET,
        )
        .expect("request");
        let decision = eval(&policies, &request);
        let approval = request_now(&request, &decision);
        assert_eq!(approval.risk().reason(), SECRET);
        assert!(!approval.risk().explanation().contains(SECRET));
        let debug = format!("{approval:?}");
        assert!(!debug.contains(SECRET));
        assert!(debug.contains("reason_len"));
        assert!(!approval.action_diff().as_text().contains(SECRET));
        assert!(approval.action_diff().as_text().contains("target=env"));
    }

    #[test]
    fn fingerprint_is_stable_for_same_binding_and_ignores_reason() {
        let session = SessionId::new();
        let a = ActionRequest::new(
            principal(),
            session,
            Capability::FsRead,
            repo_read_resource(),
            repo_read_action(),
            "first",
        )
        .expect("a");
        let b = ActionRequest::new(
            principal(),
            session,
            Capability::FsRead,
            repo_read_resource(),
            repo_read_action(),
            "second",
        )
        .expect("b");
        assert_eq!(ActionFingerprint::of(&a), ActionFingerprint::of(&b));
    }
}

/// Test-only canonical command pair with distinct argv.
#[cfg(test)]
impl CanonicalCommand {
    fn try_from_parts_for_test() -> (Self, Self) {
        use crate::normalize::command::{
            CommandNormalizeError, ExecIntent, Resolver, normalize_exec,
        };

        struct Fixed;

        impl Resolver for Fixed {
            fn resolve_cwd(
                &self,
                requested: &str,
            ) -> Result<crate::normalize::command::CanonicalHostPath, CommandNormalizeError>
            {
                crate::normalize::command::CanonicalHostPath::from_resolved(requested)
            }

            fn resolve_executable(
                &self,
                requested: &str,
                _cwd: &crate::normalize::command::CanonicalHostPath,
            ) -> Result<crate::normalize::command::CanonicalHostPath, CommandNormalizeError>
            {
                crate::normalize::command::CanonicalHostPath::from_resolved(requested)
            }
        }

        let a = normalize_exec(
            &ExecIntent::argv(["/usr/bin/git", "status"], "/repo", None::<String>),
            &Fixed,
            &CancellationToken::new(),
        )
        .expect("git status");
        let b = normalize_exec(
            &ExecIntent::argv(["/usr/bin/git", "push"], "/repo", None::<String>),
            &Fixed,
            &CancellationToken::new(),
        )
        .expect("git push");
        (a, b)
    }
}
