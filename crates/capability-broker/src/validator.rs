//! Executor-side lease validation.
//!
//! `validate_use` is the mandatory pre-effect guard. It re-checks the MAC,
//! expiry, bound action, and policy revision, then decrements remaining uses
//! atomically before returning a [`LeaseUseGuard`]. The executor consumes the
//! guard on its side-effect path. One-shot leases admit at most one guard.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Mutex;
use std::time::Instant;

use protocol::{ErrorCode, LeaseId};

use crate::approval::ActionFingerprint;
use crate::lease::{CapabilityLease, LeaseError, LeaseIssuer, PolicyRevision};
use crate::normalize::command::CancellationToken;
use crate::policy::evaluator::{ActionRequest, CanonicalAction};

/// How a bound policy-revision mismatch is treated.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PolicyRevisionMode {
    /// Current stack digest must match the lease binding. Fail closed.
    Invalidate,
}

/// Executor-verifiable use tracker. One instance owns remaining-use counts.
pub struct LeaseValidator {
    issuer: LeaseIssuer,
    current_revision: PolicyRevision,
    revision_mode: PolicyRevisionMode,
    uses: Mutex<HashMap<LeaseId, u32>>,
}

/// Permit acquired from [`validate_use`]. Not cloneable.
pub struct LeaseUseGuard {
    lease_id: LeaseId,
    action_hash: ActionFingerprint,
    remaining_uses: u32,
}

/// Proof the executor consumed a [`LeaseUseGuard`] immediately before a side effect.
pub struct ConsumedLeaseUse {
    lease_id: LeaseId,
    action_hash: ActionFingerprint,
}

/// Typed validate-use failure. Display never echoes request values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PolicyError {
    Cancelled,
    Expired,
    WrongAction,
    InvalidMac,
    InvalidLease,
    PolicyRevisionMismatch,
    UsesExhausted,
    Unavailable,
}

impl PolicyRevisionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invalidate => "invalidate",
        }
    }
}

impl LeaseValidator {
    /// Construct a validator that fails closed on policy-revision mismatch.
    pub fn new(issuer: LeaseIssuer, current_revision: PolicyRevision) -> Self {
        Self::new_with_mode(issuer, current_revision, PolicyRevisionMode::Invalidate)
    }

    /// Construct with an explicit revision mode. `Invalidate` fails closed.
    pub fn new_with_mode(
        issuer: LeaseIssuer,
        current_revision: PolicyRevision,
        revision_mode: PolicyRevisionMode,
    ) -> Self {
        Self {
            issuer,
            current_revision,
            revision_mode,
            uses: Mutex::new(HashMap::new()),
        }
    }

    pub fn current_revision(&self) -> PolicyRevision {
        self.current_revision
    }

    pub fn revision_mode(&self) -> PolicyRevisionMode {
        self.revision_mode
    }

    /// Remaining uses tracked after at least one validate attempt.
    ///
    /// `None` means this lease id has never entered the use table.
    pub fn remaining_uses(&self, lease_id: LeaseId) -> Result<Option<u32>, PolicyError> {
        let uses = self.uses.lock().map_err(|_| PolicyError::Unavailable)?;
        Ok(uses.get(&lease_id).copied())
    }

    /// Check the lease, decrement remaining uses, and issue a one-time guard.
    pub fn validate_use(
        &self,
        lease: &CapabilityLease,
        actual: &CanonicalAction,
        now: Instant,
        cancel: &CancellationToken,
    ) -> Result<LeaseUseGuard, PolicyError> {
        validate_use(self, lease, actual, now, cancel)
    }
}

impl LeaseUseGuard {
    pub fn lease_id(&self) -> LeaseId {
        self.lease_id
    }

    pub fn action_hash(&self) -> ActionFingerprint {
        self.action_hash
    }

    /// Uses remaining after this acquisition.
    pub fn remaining_uses(&self) -> u32 {
        self.remaining_uses
    }

    /// Consume on the executor path immediately before the side effect.
    pub fn consume(self) -> ConsumedLeaseUse {
        ConsumedLeaseUse {
            lease_id: self.lease_id,
            action_hash: self.action_hash,
        }
    }
}

impl ConsumedLeaseUse {
    pub fn lease_id(&self) -> LeaseId {
        self.lease_id
    }

    pub fn action_hash(&self) -> ActionFingerprint {
        self.action_hash
    }
}

impl PolicyError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "lease validation cancelled",
            Self::Expired => "lease expired",
            Self::WrongAction => "lease action hash does not match",
            Self::InvalidMac => "lease MAC is invalid",
            Self::InvalidLease => "lease is invalid",
            Self::PolicyRevisionMismatch => "lease policy revision does not match",
            Self::UsesExhausted => "lease uses exhausted",
            Self::Unavailable => "lease validator unavailable",
        }
    }

    pub const fn error_code(self) -> ErrorCode {
        ErrorCode::PolicyLeaseInvalid
    }
}

impl From<LeaseError> for PolicyError {
    fn from(err: LeaseError) -> Self {
        match err {
            LeaseError::Cancelled => Self::Cancelled,
            LeaseError::Expired => Self::Expired,
            LeaseError::WrongAction => Self::WrongAction,
            LeaseError::InvalidMac => Self::InvalidMac,
            LeaseError::WrongPrincipal
            | LeaseError::WrongSession
            | LeaseError::InvalidConstraints
            | LeaseError::TokenNotExportable => Self::InvalidLease,
        }
    }
}

/// Acquire a [`LeaseUseGuard`] after atomic use-count decrement.
///
/// Fails closed if the broker/validator cannot lock its use table, the lease
/// MAC/expiry/binding is invalid, uses are exhausted, or (when configured
/// [`PolicyRevisionMode::Invalidate`]) the policy revision no longer matches.
pub fn validate_use(
    validator: &LeaseValidator,
    lease: &CapabilityLease,
    actual: &CanonicalAction,
    now: Instant,
    cancel: &CancellationToken,
) -> Result<LeaseUseGuard, PolicyError> {
    if cancel.is_cancelled() {
        return Err(PolicyError::Cancelled);
    }

    let actual_hash = fingerprint_actual(lease, actual)?;
    validator
        .issuer
        .verify(
            lease,
            lease.principal(),
            lease.session_id(),
            actual_hash,
            now,
        )
        .map_err(PolicyError::from)?;

    if validator.revision_mode == PolicyRevisionMode::Invalidate
        && !lease.policy_revision_matches(validator.current_revision)
    {
        return Err(PolicyError::PolicyRevisionMismatch);
    }

    let remaining = {
        let mut uses = validator
            .uses
            .lock()
            .map_err(|_| PolicyError::Unavailable)?;
        let remaining = uses
            .entry(lease.lease_id())
            .or_insert_with(|| lease.max_uses());
        let Some(next) = remaining.checked_sub(1) else {
            return Err(PolicyError::UsesExhausted);
        };
        *remaining = next;
        next
    };

    Ok(LeaseUseGuard {
        lease_id: lease.lease_id(),
        action_hash: actual_hash,
        remaining_uses: remaining,
    })
}

fn fingerprint_actual(
    lease: &CapabilityLease,
    actual: &CanonicalAction,
) -> Result<ActionFingerprint, PolicyError> {
    if !actual_family_matches(lease, actual) {
        return Err(PolicyError::WrongAction);
    }
    let request = ActionRequest::new(
        lease.principal().clone(),
        lease.session_id(),
        lease.capability(),
        lease.resource().clone(),
        actual.clone(),
        "",
    )
    .map_err(|_| PolicyError::InvalidLease)?;
    Ok(ActionFingerprint::of(&request))
}

fn actual_family_matches(lease: &CapabilityLease, actual: &CanonicalAction) -> bool {
    match actual {
        CanonicalAction::Command(_) => {
            lease.capability().family() == crate::capability::CapabilityFamily::Proc
        }
        CanonicalAction::Filesystem(_) => {
            lease.capability().family() == crate::capability::CapabilityFamily::Fs
        }
        CanonicalAction::Network(_) => {
            lease.capability().family() == crate::capability::CapabilityFamily::Net
        }
        CanonicalAction::Resource {
            capability,
            resource,
        } => *capability == lease.capability() && resource == lease.resource(),
    }
}

impl fmt::Debug for LeaseValidator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeaseValidator")
            .field("issuer", &self.issuer)
            .field("current_revision", &self.current_revision)
            .field("revision_mode", &self.revision_mode)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for LeaseUseGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeaseUseGuard")
            .field("lease_id", &self.lease_id)
            .field("action_hash", &self.action_hash)
            .field("remaining_uses", &self.remaining_uses)
            .finish()
    }
}

impl fmt::Debug for ConsumedLeaseUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConsumedLeaseUse")
            .field("lease_id", &self.lease_id)
            .field("action_hash", &self.action_hash)
            .finish()
    }
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for PolicyError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use protocol::SessionId;

    use crate::approval::{ApprovalChoice, ApprovalResolution, ApprovalScopeId, request_approval};
    use crate::capability::{Capability, FilesystemScope, ResourceDescriptor};
    use crate::lease::{DEFAULT_LEASE_TTL_SECS, issue};
    use crate::policy::evaluator::{ActionRequest, DecisionWithTrace, PolicyStack, evaluate};
    use crate::policy::parser::{PolicyDocument, PolicySource};

    fn principal() -> crate::policy::evaluator::PrincipalRef {
        crate::policy::evaluator::PrincipalRef::parse("agent").expect("principal")
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

    fn eval(policies: &PolicyStack, request: &ActionRequest) -> DecisionWithTrace {
        evaluate(policies, request, &CancellationToken::new()).expect("evaluate")
    }

    fn approve(request: &ActionRequest, policies: &PolicyStack) -> crate::approval::ApprovedAction {
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
    ) -> (CapabilityLease, CanonicalAction) {
        let approved = approve(request, policies);
        let lease = issue(
            &issuer(),
            &approved,
            policies,
            now,
            &CancellationToken::new(),
        )
        .expect("issue");
        (lease, request.normalized_action().clone())
    }

    fn validator_for(policies: &PolicyStack) -> LeaseValidator {
        LeaseValidator::new_with_mode(
            issuer(),
            PolicyRevision::of_stack(policies),
            PolicyRevisionMode::Invalidate,
        )
    }

    #[test]
    fn validate_use_issues_guard_and_decrements_uses() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (lease, actual) = issued(&request, &policies, now);
        let validator = validator_for(&policies);
        let guard = validate_use(&validator, &lease, &actual, now, &CancellationToken::new())
            .expect("guard");
        assert_eq!(guard.lease_id(), lease.lease_id());
        assert_eq!(guard.action_hash(), lease.action_hash());
        assert_eq!(guard.remaining_uses(), 0);
        assert_eq!(
            validator.remaining_uses(lease.lease_id()).expect("tracked"),
            Some(0)
        );
        let consumed = guard.consume();
        assert_eq!(consumed.lease_id(), lease.lease_id());
        assert_eq!(consumed.action_hash(), lease.action_hash());
    }

    #[test]
    fn sequential_double_use_of_one_shot_lease_is_rejected() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (lease, actual) = issued(&request, &policies, now);
        let validator = validator_for(&policies);
        validate_use(&validator, &lease, &actual, now, &CancellationToken::new())
            .expect("first")
            .consume();
        let err = validate_use(&validator, &lease, &actual, now, &CancellationToken::new())
            .expect_err("second");
        assert_eq!(err, PolicyError::UsesExhausted);
        assert_eq!(err.error_code(), ErrorCode::PolicyLeaseInvalid);
    }

    #[test]
    fn concurrent_double_use_of_one_shot_lease_permits_one_side_effect() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (lease, actual) = issued(&request, &policies, now);
        let validator = Arc::new(validator_for(&policies));
        let lease = Arc::new(lease);
        let actual = Arc::new(actual);
        let start = Arc::new(Barrier::new(2));
        let results = thread::scope(|scope| {
            let mut joins = Vec::new();
            for _ in 0..2 {
                let validator = Arc::clone(&validator);
                let lease = Arc::clone(&lease);
                let actual = Arc::clone(&actual);
                let start = Arc::clone(&start);
                joins.push(scope.spawn(move || {
                    start.wait();
                    validate_use(&validator, &lease, &actual, now, &CancellationToken::new())
                }));
            }
            joins
                .into_iter()
                .map(|join| join.join().expect("thread"))
                .collect::<Vec<_>>()
        });
        let ok = results.iter().filter(|r| r.is_ok()).count();
        let exhausted = results
            .iter()
            .filter(|r| matches!(r, Err(PolicyError::UsesExhausted)))
            .count();
        assert_eq!(ok, 1, "exactly one side-effect guard");
        assert_eq!(exhausted, 1, "peer is rejected");
        assert_eq!(
            validator.remaining_uses(lease.lease_id()).expect("tracked"),
            Some(0)
        );
    }

    #[test]
    fn policy_revision_mismatch_fails_closed_when_configured_invalidating() {
        let policies = ask_stack();
        let other = stack([user(
            r#"
[[rules]]
id = "repo-read"
effect = "allow"
subjects = ["*"]
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }
"#,
        )]);
        let request = repo_read_request();
        let now = Instant::now();
        let (lease, actual) = issued(&request, &policies, now);
        let validator = LeaseValidator::new_with_mode(
            issuer(),
            PolicyRevision::of_stack(&other),
            PolicyRevisionMode::Invalidate,
        );
        let err = validate_use(&validator, &lease, &actual, now, &CancellationToken::new())
            .expect_err("revision");
        assert_eq!(err, PolicyError::PolicyRevisionMismatch);
        assert_eq!(
            validator.remaining_uses(lease.lease_id()).expect("unused"),
            None
        );
    }

    #[test]
    fn matching_policy_revision_is_accepted() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (lease, actual) = issued(&request, &policies, now);
        let validator = validator_for(&policies);
        assert!(lease.policy_revision_matches(validator.current_revision()));
        validate_use(&validator, &lease, &actual, now, &CancellationToken::new()).expect("match");
    }

    #[test]
    fn mutated_action_is_rejected_before_decrement() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (lease, _) = issued(&request, &policies, now);
        let other_resource =
            ResourceDescriptor::Filesystem(FilesystemScope::repo("src/other.rs").expect("fs"));
        let mutated = CanonicalAction::Resource {
            capability: Capability::FsRead,
            resource: other_resource,
        };
        let validator = validator_for(&policies);
        let err = validate_use(&validator, &lease, &mutated, now, &CancellationToken::new())
            .expect_err("mutated");
        assert_eq!(err, PolicyError::WrongAction);
        assert_eq!(
            validator.remaining_uses(lease.lease_id()).expect("unused"),
            None
        );
    }

    #[test]
    fn capability_family_mismatch_is_rejected() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (lease, _) = issued(&request, &policies, now);
        let validator = validator_for(&policies);
        let mutated = CanonicalAction::Resource {
            capability: Capability::FsWrite,
            resource: repo_read_resource(),
        };
        let err = validate_use(&validator, &lease, &mutated, now, &CancellationToken::new())
            .expect_err("family");
        assert_eq!(err, PolicyError::WrongAction);
    }

    #[test]
    fn expired_lease_is_rejected() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (lease, actual) = issued(&request, &policies, now);
        let validator = validator_for(&policies);
        let later = now + Duration::from_secs(u64::from(DEFAULT_LEASE_TTL_SECS));
        let err = validate_use(
            &validator,
            &lease,
            &actual,
            later,
            &CancellationToken::new(),
        )
        .expect_err("expired");
        assert_eq!(err, PolicyError::Expired);
    }

    #[test]
    fn foreign_issuer_mac_is_rejected() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (lease, actual) = issued(&request, &policies, now);
        let other = LeaseIssuer::from_key([0x22; 32]).expect("other");
        let validator = LeaseValidator::new_with_mode(
            other,
            PolicyRevision::of_stack(&policies),
            PolicyRevisionMode::Invalidate,
        );
        let err = validate_use(&validator, &lease, &actual, now, &CancellationToken::new())
            .expect_err("mac");
        assert_eq!(err, PolicyError::InvalidMac);
    }

    #[test]
    fn cancelled_validate_fails_closed() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (lease, actual) = issued(&request, &policies, now);
        let validator = validator_for(&policies);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = validate_use(&validator, &lease, &actual, now, &cancel).expect_err("cancelled");
        assert_eq!(err, PolicyError::Cancelled);
    }

    #[test]
    fn policy_error_display_does_not_echo_action_or_path() {
        let err = PolicyError::WrongAction;
        let text = err.to_string();
        assert_eq!(text, "lease action hash does not match");
        assert!(!text.contains("src/main.rs"));
        assert!(!text.contains("secret"));
        let debug = format!("{err:?}");
        assert!(!debug.contains("src/main.rs"));
    }

    #[test]
    fn lease_use_guard_is_not_reusable_after_consume() {
        let policies = ask_stack();
        let request = repo_read_request();
        let now = Instant::now();
        let (lease, actual) = issued(&request, &policies, now);
        let validator = validator_for(&policies);
        let guard = validate_use(&validator, &lease, &actual, now, &CancellationToken::new())
            .expect("guard");
        let _consumed = guard.consume();
        let err = validate_use(&validator, &lease, &actual, now, &CancellationToken::new())
            .expect_err("replay");
        assert_eq!(err, PolicyError::UsesExhausted);
    }
}
