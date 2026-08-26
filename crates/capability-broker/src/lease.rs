//! Capability lease issuance.
//!
//! `issue` turns an [`ApprovedAction`] into a MAC-protected in-process lease
//! bound to principal, session, action hash, scope, expiry, uses, and policy
//! revision. The lease token is not a model/tool wire type.

use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::{ArtifactId, LeaseId, SessionId};

use crate::approval::{ActionFingerprint, ApprovalScopeId, ApprovedAction};
use crate::capability::{Capability, ResourceDescriptor};
use crate::normalize::command::CancellationToken;
use crate::policy::evaluator::{LeaseConstraints, PolicyStack, PrincipalRef};
use crate::policy::parser::{PolicyDocument, ResourcePattern};

/// Default uses copied from [`LeaseConstraints::standard`].
pub const DEFAULT_LEASE_MAX_USES: u32 = LeaseConstraints::DEFAULT_MAX_USES;

/// Default TTL copied from [`LeaseConstraints::standard`]. Always <= 60s.
pub const DEFAULT_LEASE_TTL_SECS: u32 = LeaseConstraints::DEFAULT_MAX_TTL_SECS;

const BINDING_TAG: &[u8] = b"rapidlm.capability_lease.v1";
const REVISION_TAG: &[u8] = b"rapidlm.policy_revision.v1";
const HMAC_BLOCK: usize = 64;

/// In-process issuer key. Never logged or copied into agent messages.
pub struct LeaseIssuer {
    key: [u8; 32],
}

/// MAC over the lease binding. Not a Serialize type.
#[derive(Clone, Eq, PartialEq)]
pub struct LeaseToken([u8; 32]);

/// Digest of the policy stack bound into every lease.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct PolicyRevision([u8; 32]);

/// Signed/MAC-protected in-process lease. Fields are private; the token
/// cannot be reconstructed from model-visible summaries.
#[derive(Clone, Eq, PartialEq)]
pub struct CapabilityLease {
    lease_id: LeaseId,
    principal: PrincipalRef,
    session_id: SessionId,
    action_hash: ActionFingerprint,
    capability: Capability,
    resource: ResourceDescriptor,
    scope: ApprovalScopeId,
    constraints: LeaseConstraints,
    remaining_uses: u32,
    policy_revision: PolicyRevision,
    issued_at: Instant,
    expires_at: Instant,
    nonce: [u8; 16],
    token: LeaseToken,
}

/// Typed issuance / binding failure. Display never echoes request values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LeaseError {
    Cancelled,
    Expired,
    WrongPrincipal,
    WrongSession,
    WrongAction,
    InvalidMac,
    InvalidConstraints,
    TokenNotExportable,
}

impl LeaseIssuer {
    /// Construct from an explicit 32-byte MAC key. All-zero keys fail closed.
    pub fn from_key(key: [u8; 32]) -> Result<Self, LeaseError> {
        if key.iter().all(|b| *b == 0) {
            return Err(LeaseError::InvalidMac);
        }
        Ok(Self { key })
    }

    /// Fresh in-process key. Not persisted and not a credential-store secret.
    pub fn ephemeral() -> Self {
        loop {
            let mut seed = Vec::with_capacity(64);
            for _ in 0..4 {
                seed.extend_from_slice(SessionId::new().as_uuid().as_bytes());
            }
            let key = *ArtifactId::from_bytes(&seed).as_digest();
            if let Ok(issuer) = Self::from_key(key) {
                return issuer;
            }
        }
    }

    /// Issue a one-shot short-TTL lease from an approved action.
    pub fn issue(
        &self,
        approved: &ApprovedAction,
        policies: &PolicyStack,
        now: Instant,
        cancel: &CancellationToken,
    ) -> Result<CapabilityLease, LeaseError> {
        issue(self, approved, policies, now, cancel)
    }

    /// Check MAC, expiry, and presented principal/session/action binding.
    ///
    /// Does not consume a use. Executor consumption is a later validator.
    pub fn verify(
        &self,
        lease: &CapabilityLease,
        principal: &PrincipalRef,
        session_id: SessionId,
        action_hash: ActionFingerprint,
        now: Instant,
    ) -> Result<(), LeaseError> {
        if !ct_eq(&lease.token.0, &self.mac(lease)) {
            return Err(LeaseError::InvalidMac);
        }
        if lease.is_expired(now) {
            return Err(LeaseError::Expired);
        }
        if principal != &lease.principal {
            return Err(LeaseError::WrongPrincipal);
        }
        if session_id != lease.session_id {
            return Err(LeaseError::WrongSession);
        }
        if action_hash != lease.action_hash {
            return Err(LeaseError::WrongAction);
        }
        Ok(())
    }

    fn mac(&self, lease: &CapabilityLease) -> [u8; 32] {
        hmac_sha256(&self.key, &binding_bytes(lease))
    }
}

impl PolicyRevision {
    /// Deterministic digest of the stacked documents and rules.
    pub fn of_stack(stack: &PolicyStack) -> Self {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(REVISION_TAG);
        for document in stack.documents() {
            append_document_bytes(&mut buf, document);
        }
        Self(*ArtifactId::from_bytes(&buf).as_digest())
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl CapabilityLease {
    pub fn lease_id(&self) -> LeaseId {
        self.lease_id
    }

    pub fn principal(&self) -> &PrincipalRef {
        &self.principal
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn action_hash(&self) -> ActionFingerprint {
        self.action_hash
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }

    pub fn resource(&self) -> &ResourceDescriptor {
        &self.resource
    }

    pub fn scope(&self) -> ApprovalScopeId {
        self.scope
    }

    pub fn constraints(&self) -> LeaseConstraints {
        self.constraints
    }

    pub fn max_uses(&self) -> u32 {
        self.constraints.max_uses()
    }

    pub fn remaining_uses(&self) -> u32 {
        self.remaining_uses
    }

    pub fn policy_revision(&self) -> PolicyRevision {
        self.policy_revision
    }

    pub fn issued_at(&self) -> Instant {
        self.issued_at
    }

    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }

    pub fn is_expired(&self, now: Instant) -> bool {
        now < self.issued_at || now >= self.expires_at
    }

    /// In-process token. Never encode this for model or tool output.
    pub fn token(&self) -> &LeaseToken {
        &self.token
    }

    /// Lease tokens are in-process only. Model/tool serialization always fails.
    pub fn token_for_tool_output(&self) -> Result<LeaseToken, LeaseError> {
        let _ = self;
        Err(LeaseError::TokenNotExportable)
    }

    /// True when this lease was issued under `current`.
    pub fn policy_revision_matches(&self, current: PolicyRevision) -> bool {
        self.policy_revision == current
    }
}

impl LeaseToken {
    /// Raw MAC bytes stay in-process. Callers cannot serialize this type.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl LeaseError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "lease issuance cancelled",
            Self::Expired => "lease expired",
            Self::WrongPrincipal => "lease principal does not match",
            Self::WrongSession => "lease session does not match",
            Self::WrongAction => "lease action hash does not match",
            Self::InvalidMac => "lease MAC is invalid",
            Self::InvalidConstraints => "lease constraints are invalid",
            Self::TokenNotExportable => "lease token cannot be serialized for model output",
        }
    }
}

/// Issue a MAC-protected lease. Defaults are one use and a short TTL.
pub fn issue(
    issuer: &LeaseIssuer,
    approved: &ApprovedAction,
    policies: &PolicyStack,
    now: Instant,
    cancel: &CancellationToken,
) -> Result<CapabilityLease, LeaseError> {
    if cancel.is_cancelled() {
        return Err(LeaseError::Cancelled);
    }
    let constraints = approved.scope().constraints();
    if constraints.max_uses() == 0 || constraints.max_ttl_secs() == 0 {
        return Err(LeaseError::InvalidConstraints);
    }
    let ttl = Duration::from_secs(u64::from(constraints.max_ttl_secs()));
    let expires_at = now.checked_add(ttl).ok_or(LeaseError::Expired)?;
    let mut nonce = [0u8; 16];
    nonce.copy_from_slice(SessionId::new().as_uuid().as_bytes());
    let mut lease = CapabilityLease {
        lease_id: LeaseId::new(),
        principal: approved.principal().clone(),
        session_id: approved.session_id(),
        action_hash: approved.action_hash(),
        capability: approved.capability(),
        resource: approved.resource().clone(),
        scope: approved.scope().id(),
        constraints,
        remaining_uses: constraints.max_uses(),
        policy_revision: PolicyRevision::of_stack(policies),
        issued_at: now,
        expires_at,
        nonce,
        token: LeaseToken([0u8; 32]),
    };
    lease.token = LeaseToken(issuer.mac(&lease));
    Ok(lease)
}

fn binding_bytes(lease: &CapabilityLease) -> Vec<u8> {
    let mut buf = Vec::with_capacity(192);
    buf.extend_from_slice(BINDING_TAG);
    buf.push(0);
    buf.extend_from_slice(lease.lease_id.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.principal.as_str().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.session_id.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.action_hash.as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.capability.as_str().as_bytes());
    buf.push(0);
    append_resource_bytes(&mut buf, &lease.resource);
    buf.push(0);
    buf.extend_from_slice(lease.scope.as_str().as_bytes());
    buf.push(0);
    buf.extend_from_slice(&lease.constraints.max_uses().to_be_bytes());
    buf.extend_from_slice(&lease.constraints.max_ttl_secs().to_be_bytes());
    buf.extend_from_slice(lease.policy_revision.as_bytes());
    buf.extend_from_slice(&lease.nonce);
    buf
}

fn append_document_bytes(buf: &mut Vec<u8>, document: &PolicyDocument) {
    buf.push(0);
    buf.extend_from_slice(&document.schema().to_be_bytes());
    buf.push(0);
    buf.extend_from_slice(document.source().layer().as_str().as_bytes());
    buf.push(0);
    buf.extend_from_slice(document.source().origin().as_bytes());
    for rule in document.rules() {
        buf.push(0);
        buf.extend_from_slice(rule.id().as_str().as_bytes());
        buf.push(0);
        buf.extend_from_slice(rule.effect().as_str().as_bytes());
        buf.push(0);
        buf.extend_from_slice(rule.capability_pattern().as_str().as_bytes());
        for subject in rule.subjects() {
            buf.push(0);
            buf.extend_from_slice(subject.as_str().as_bytes());
        }
        append_resource_pattern(buf, rule.resource_pattern());
    }
}

fn append_resource_pattern(buf: &mut Vec<u8>, pattern: &ResourcePattern) {
    buf.push(0);
    match pattern {
        ResourcePattern::Any => buf.extend_from_slice(b"any"),
        ResourcePattern::Filesystem { root, glob } => {
            buf.extend_from_slice(b"fs");
            buf.push(0);
            if let Some(root) = root {
                buf.extend_from_slice(root.as_str().as_bytes());
            }
            buf.push(0);
            if let Some(glob) = glob {
                buf.extend_from_slice(glob.as_str().as_bytes());
            }
        }
        ResourcePattern::Process { command_family } => {
            buf.extend_from_slice(b"proc");
            buf.push(0);
            if let Some(family) = command_family {
                buf.extend_from_slice(family.as_str().as_bytes());
            }
        }
        ResourcePattern::Network { scheme, host, port } => {
            buf.extend_from_slice(b"net");
            buf.push(0);
            if let Some(scheme) = scheme {
                buf.extend_from_slice(scheme.as_str().as_bytes());
            }
            buf.push(0);
            if let Some(host) = host {
                buf.extend_from_slice(host.as_str().as_bytes());
            }
            buf.push(0);
            if let Some(port) = port {
                buf.extend_from_slice(&port.to_be_bytes());
            }
        }
        ResourcePattern::Git { ref_scope } => {
            buf.extend_from_slice(b"git");
            buf.push(0);
            if let Some(scope) = ref_scope {
                buf.extend_from_slice(scope.as_str().as_bytes());
            }
        }
        ResourcePattern::Secret { secret_id, target } => {
            buf.extend_from_slice(b"secret");
            buf.push(0);
            if let Some(id) = secret_id {
                buf.extend_from_slice(id.as_str().as_bytes());
            }
            buf.push(0);
            if let Some(target) = target {
                buf.extend_from_slice(target.as_str().as_bytes());
            }
        }
        ResourcePattern::Browser { origin, path } => {
            buf.extend_from_slice(b"browser");
            buf.push(0);
            if let Some(origin) = origin {
                buf.extend_from_slice(origin.to_string().as_bytes());
            }
            buf.push(0);
            if let Some(path) = path {
                buf.extend_from_slice(path.as_str().as_bytes());
            }
        }
        ResourcePattern::Mobile { device_id } => {
            buf.extend_from_slice(b"mobile");
            buf.push(0);
            if let Some(id) = device_id {
                buf.extend_from_slice(id.as_str().as_bytes());
            }
        }
        ResourcePattern::Mcp { server, tool } => {
            buf.extend_from_slice(b"mcp");
            buf.push(0);
            if let Some(server) = server {
                buf.extend_from_slice(server.as_bytes());
            }
            buf.push(0);
            if let Some(tool) = tool {
                buf.extend_from_slice(tool.as_bytes());
            }
        }
        ResourcePattern::Plugin { plugin, capability } => {
            buf.extend_from_slice(b"plugin");
            buf.push(0);
            if let Some(plugin) = plugin {
                buf.extend_from_slice(plugin.as_bytes());
            }
            buf.push(0);
            if let Some(capability) = capability {
                buf.extend_from_slice(capability.as_bytes());
            }
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
            buf.extend_from_slice(&scope.port().to_be_bytes());
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

fn hmac_sha256(key: &[u8; 32], msg: &[u8]) -> [u8; 32] {
    let mut ipad = [0x36u8; HMAC_BLOCK];
    let mut opad = [0x5cu8; HMAC_BLOCK];
    for (i, byte) in key.iter().copied().enumerate() {
        ipad[i] ^= byte;
        opad[i] ^= byte;
    }
    let mut inner = Vec::with_capacity(HMAC_BLOCK + msg.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner_hash = *ArtifactId::from_bytes(&inner).as_digest();
    let mut outer = Vec::with_capacity(HMAC_BLOCK + inner_hash.len());
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    *ArtifactId::from_bytes(&outer).as_digest()
}

fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut acc = 0u8;
    for i in 0..32 {
        acc |= a[i] ^ b[i];
    }
    acc == 0
}

impl fmt::Debug for LeaseIssuer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeaseIssuer")
            .field("key", &"<redacted>")
            .finish()
    }
}

impl fmt::Debug for LeaseToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("LeaseToken").field(&"<redacted>").finish()
    }
}

impl fmt::Display for PolicyRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for PolicyRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PolicyRevision")
            .field(&self.to_string())
            .finish()
    }
}

impl fmt::Debug for CapabilityLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapabilityLease")
            .field("lease_id", &self.lease_id)
            .field("principal", &self.principal)
            .field("session_id", &self.session_id)
            .field("action_hash", &self.action_hash)
            .field("capability", &self.capability.as_str())
            .field("scope", &self.scope)
            .field("max_uses", &self.max_uses())
            .field("remaining_uses", &self.remaining_uses)
            .field("policy_revision", &self.policy_revision)
            .field("token", &self.token)
            .finish()
    }
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for LeaseError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{ApprovalChoice, ApprovalResolution, ApprovalScopeId, request_approval};
    use crate::capability::{FilesystemScope, ProcessScope};
    use crate::policy::evaluator::{ActionRequest, CanonicalAction, DecisionWithTrace, evaluate};
    use crate::policy::parser::{PolicyDocument, PolicySource};

    fn principal() -> PrincipalRef {
        PrincipalRef::parse("agent").expect("principal")
    }

    fn other_principal() -> PrincipalRef {
        PrincipalRef::parse("other-agent").expect("principal")
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

    fn repo_read_request_as(who: PrincipalRef, session: SessionId) -> ActionRequest {
        ActionRequest::new(
            who,
            session,
            Capability::FsRead,
            repo_read_resource(),
            repo_read_action(),
            "read source",
        )
        .expect("request")
    }

    fn repo_read_request() -> ActionRequest {
        repo_read_request_as(principal(), SessionId::new())
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

    fn eval(policies: &PolicyStack, request: &ActionRequest) -> DecisionWithTrace {
        evaluate(policies, request, &CancellationToken::new()).expect("evaluate")
    }

    fn approve(request: &ActionRequest, policies: &PolicyStack) -> ApprovedAction {
        let decision = eval(policies, request);
        let approval = request_approval(
            request,
            &decision,
            Instant::now(),
            &CancellationToken::new(),
        )
        .expect("approval");
        match approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                request,
                Instant::now(),
                &CancellationToken::new(),
            )
            .expect("resolve")
        {
            ApprovalResolution::Approved(approved) => approved,
            ApprovalResolution::Denied => panic!("expected approved"),
        }
    }

    fn issuer() -> LeaseIssuer {
        LeaseIssuer::from_key([0x11; 32]).expect("issuer")
    }

    fn issued(
        request: &ActionRequest,
        policies: &PolicyStack,
        now: Instant,
    ) -> (ApprovedAction, CapabilityLease) {
        let approved = approve(request, policies);
        let lease = issue(
            &issuer(),
            &approved,
            policies,
            now,
            &CancellationToken::new(),
        )
        .expect("issue");
        (approved, lease)
    }

    #[test]
    fn issue_defaults_to_one_use_and_short_expiry() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (approved, lease) = issued(&request, &policies, now);
        assert_eq!(lease.max_uses(), DEFAULT_LEASE_MAX_USES);
        assert_eq!(lease.max_uses(), 1);
        assert_eq!(lease.remaining_uses(), 1);
        assert_eq!(lease.constraints().max_ttl_secs(), DEFAULT_LEASE_TTL_SECS);
        assert_eq!(lease.expires_at(), now + Duration::from_secs(60));
        assert_eq!(lease.principal(), approved.principal());
        assert_eq!(lease.session_id(), approved.session_id());
        assert_eq!(lease.action_hash(), approved.action_hash());
        assert_eq!(lease.capability(), Capability::FsRead);
        assert_eq!(lease.scope(), ApprovalScopeId::Once);
        assert!(lease.policy_revision_matches(PolicyRevision::of_stack(&policies)));
        issuer()
            .verify(
                &lease,
                lease.principal(),
                lease.session_id(),
                lease.action_hash(),
                now,
            )
            .expect("valid");
    }

    #[test]
    fn expired_lease_is_rejected() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (_, lease) = issued(&request, &policies, now);
        let later = now + Duration::from_secs(u64::from(DEFAULT_LEASE_TTL_SECS));
        assert!(lease.is_expired(later));
        let err = issuer()
            .verify(
                &lease,
                lease.principal(),
                lease.session_id(),
                lease.action_hash(),
                later,
            )
            .expect_err("expired");
        assert_eq!(err, LeaseError::Expired);
    }

    #[test]
    fn clock_regression_fails_closed_as_expired() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (_, lease) = issued(&request, &policies, now);
        let earlier = now.checked_sub(Duration::from_secs(1)).expect("earlier");
        assert!(lease.is_expired(earlier));
        assert_eq!(
            issuer().verify(
                &lease,
                lease.principal(),
                lease.session_id(),
                lease.action_hash(),
                earlier
            ),
            Err(LeaseError::Expired)
        );
    }

    #[test]
    fn wrong_agent_lease_is_rejected() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (_, lease) = issued(&request, &policies, now);
        let err = issuer()
            .verify(
                &lease,
                &other_principal(),
                lease.session_id(),
                lease.action_hash(),
                now,
            )
            .expect_err("wrong agent");
        assert_eq!(err, LeaseError::WrongPrincipal);
    }

    #[test]
    fn wrong_session_lease_is_rejected() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (_, lease) = issued(&request, &policies, now);
        let err = issuer()
            .verify(
                &lease,
                lease.principal(),
                SessionId::new(),
                lease.action_hash(),
                now,
            )
            .expect_err("wrong session");
        assert_eq!(err, LeaseError::WrongSession);
    }

    #[test]
    fn wrong_action_lease_is_rejected() {
        let policies = ask_stack();
        let request = repo_read_request();
        let other_resource =
            ResourceDescriptor::Filesystem(FilesystemScope::repo("src/other.rs").expect("fs"));
        let other = ActionRequest::new(
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
        .expect("other");
        let now = Instant::now();
        let (_, lease) = issued(&request, &policies, now);
        let other_approved = approve(&other, &policies);
        let err = issuer()
            .verify(
                &lease,
                lease.principal(),
                lease.session_id(),
                other_approved.action_hash(),
                now,
            )
            .expect_err("wrong action");
        assert_eq!(err, LeaseError::WrongAction);
    }

    #[test]
    fn foreign_issuer_mac_is_rejected() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (_, lease) = issued(&request, &policies, now);
        let other = LeaseIssuer::from_key([0x22; 32]).expect("other");
        let err = other
            .verify(
                &lease,
                lease.principal(),
                lease.session_id(),
                lease.action_hash(),
                now,
            )
            .expect_err("mac");
        assert_eq!(err, LeaseError::InvalidMac);
    }

    #[test]
    fn zero_issuer_key_is_rejected() {
        assert_eq!(
            LeaseIssuer::from_key([0u8; 32]).expect_err("zero"),
            LeaseError::InvalidMac
        );
    }

    #[test]
    fn lease_token_cannot_enter_model_tool_output() {
        let policies = ask_stack();
        let request = repo_read_request();
        let (_, lease) = issued(&request, &policies, Instant::now());
        assert_eq!(
            lease.token_for_tool_output(),
            Err(LeaseError::TokenNotExportable)
        );
        let debug = format!("{lease:?}");
        let token_hex = lease
            .token()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert!(!debug.contains(&token_hex));
        assert!(debug.contains("<redacted>"));
        let issuer_debug = format!("{:?}", issuer());
        assert!(issuer_debug.contains("<redacted>"));
        assert!(
            !issuer_debug
                .contains("1111111111111111111111111111111111111111111111111111111111111111")
        );
    }

    #[test]
    fn policy_revision_changes_when_stack_changes() {
        let a = ask_stack();
        let b = stack([user(
            r#"
[[rules]]
id = "repo-read"
effect = "allow"
subjects = ["*"]
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }
"#,
        )]);
        assert_ne!(PolicyRevision::of_stack(&a), PolicyRevision::of_stack(&b));
        let request = repo_read_request();
        let (_, lease) = issued(&request, &a, Instant::now());
        assert!(lease.policy_revision_matches(PolicyRevision::of_stack(&a)));
        assert!(!lease.policy_revision_matches(PolicyRevision::of_stack(&b)));
    }

    #[test]
    fn cancelled_issue_fails_closed() {
        let policies = ask_stack();
        let request = repo_read_request();
        let approved = approve(&request, &policies);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            issue(&issuer(), &approved, &policies, Instant::now(), &cancel),
            Err(LeaseError::Cancelled)
        );
    }

    #[test]
    fn two_issues_are_not_the_same_token() {
        let policies = ask_stack();
        let request = repo_read_request();
        let approved = approve(&request, &policies);
        let now = Instant::now();
        let a = issue(
            &issuer(),
            &approved,
            &policies,
            now,
            &CancellationToken::new(),
        )
        .expect("a");
        let b = issue(
            &issuer(),
            &approved,
            &policies,
            now,
            &CancellationToken::new(),
        )
        .expect("b");
        assert_ne!(a.lease_id(), b.lease_id());
        assert_ne!(a.token(), b.token());
    }

    #[test]
    fn process_scope_lease_binds_command_family_not_argv() {
        let policies = stack([
            user(
                r#"
[[rules]]
id = "git-allow"
effect = "allow"
subjects = ["*"]
capability = "proc.exec"
resource = { command_family = "git" }
"#,
            ),
            project(
                r#"
[[rules]]
id = "git-ask"
effect = "ask"
subjects = ["*"]
capability = "proc.exec"
"#,
            ),
        ]);
        let resource = ResourceDescriptor::Process(ProcessScope::new("git").expect("process"));
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::ProcExec,
            resource.clone(),
            CanonicalAction::Resource {
                capability: Capability::ProcExec,
                resource,
            },
            "status",
        )
        .expect("request");
        let (_, lease) = issued(&request, &policies, Instant::now());
        assert_eq!(lease.capability(), Capability::ProcExec);
        assert_eq!(lease.max_uses(), 1);
    }
}
