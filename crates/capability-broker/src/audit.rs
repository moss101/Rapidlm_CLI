//! Capability audit events.
//!
//! `audit_decision` appends every allow/ask/deny. Approval and lease outcomes
//! are separate appends. Summaries are redacted: reason text, argv, shell
//! scripts, lease tokens, and secret values never enter storage. Threats:
//! `T-012`, `T-004`, `T-017`.

use std::error::Error;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use event_ledger::event::{ActorRef, EventKind};
use event_ledger::ledger::{AppendOptions, EventLedger, LedgerError};
use protocol::{AgentId, LeaseId, RedactionClass, SessionId, TraceId};
use serde_json::{Value, json};

use crate::approval::{ActionFingerprint, ApprovalRequest, ApprovalResolution, ApprovalScopeId};
use crate::capability::{Capability, CapabilityFamily, ResourceDescriptor};
use crate::lease::{CapabilityLease, PolicyRevision};
use crate::normalize::command::{CancellationToken, CanonicalCommand, ShellMode};
use crate::normalize::fs::CanonicalFsAction;
use crate::normalize::network::CanonicalNetworkTarget;
use crate::policy::evaluator::{
    ActionRequest, CanonicalAction, Decision, DecisionWithTrace, DenyReason, PolicyStack,
    PrincipalRef, RiskClass,
};
use crate::validator::{ConsumedLeaseUse, PolicyError};

/// Maximum UTF-8 bytes in a redacted action summary.
pub const MAX_ACTION_SUMMARY_BYTES: usize = 256;

/// Maximum UTF-8 bytes in `resource_scope_json`.
pub const MAX_RESOURCE_SCOPE_BYTES: usize = 2048;

/// Maximum records retained by [`MemoryAuditStore`].
pub const MAX_AUDIT_RECORDS: usize = 4096;

/// Closed audit outcome. Wire form matches policy effect or lease/approval phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AuditOutcome {
    Allow,
    Ask,
    Deny,
    Requested,
    Resolved,
    Expired,
    Issued,
    Used,
}

/// Registered ledger kind used for a capability audit append.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AuditEventKind {
    ToolRequested,
    ToolAuthorized,
    ToolApprovalRequired,
    ToolDenied,
    ApprovalRequested,
    ApprovalResolved,
    ApprovalExpired,
}

/// Session/agent/trace attribution bound into every audit record.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct AuditContext {
    agent_id: AgentId,
    trace_id: TraceId,
    recorded_at: SystemTime,
}

/// Durable capability audit record. Secret values are never stored.
#[derive(Clone, Eq, PartialEq)]
pub struct CapabilityAuditRecord {
    session_id: SessionId,
    agent_id: AgentId,
    principal: PrincipalRef,
    action_hash: ActionFingerprint,
    capability: Capability,
    resource_scope_json: String,
    action_summary: String,
    decision: AuditOutcome,
    policy_revision: PolicyRevision,
    lease_id: Option<LeaseId>,
    approval_scope: Option<ApprovalScopeId>,
    deny_reason: Option<DenyReason>,
    risk: RiskClass,
    event_kind: AuditEventKind,
    redaction: RedactionClass,
    recorded_at: String,
    issued_at: Option<String>,
    expires_at: Option<String>,
    trace_id: TraceId,
}

/// Lease-centric projection matching `capability_audit` in the ledger schema.
#[derive(Clone, Eq, PartialEq)]
pub struct CapabilityAuditRow {
    lease_id: LeaseId,
    session_id: SessionId,
    agent_id: AgentId,
    action_hash: ActionFingerprint,
    capability: Capability,
    resource_scope_json: String,
    decision: AuditOutcome,
    policy_revision: PolicyRevision,
    issued_at: String,
    expires_at: Option<String>,
}

/// Typed audit failure. Display never echoes request or secret values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AuditError {
    Cancelled,
    Unavailable,
    StoreFull,
    Clock,
    PayloadBound,
    SessionMissing,
}

/// Append-only audit storage. Implementations fail closed.
pub trait CapabilityAuditStore {
    fn append(
        &self,
        record: CapabilityAuditRecord,
        cancel: &CancellationToken,
    ) -> Result<CapabilityAuditRecord, AuditError>;
}

/// In-process audit buffer used by tests and as a projection cache.
pub struct MemoryAuditStore {
    records: Mutex<Vec<CapabilityAuditRecord>>,
}

/// Event-ledger sink. Appends a typed tool/approval event before success.
pub struct LedgerAuditStore {
    ledger: EventLedger,
}

/// `AuditEmitter` from the capability-broker architecture.
pub struct AuditEmitter<S> {
    store: S,
}

impl AuditOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
            Self::Requested => "requested",
            Self::Resolved => "resolved",
            Self::Expired => "expired",
            Self::Issued => "issued",
            Self::Used => "used",
        }
    }
}

impl AuditEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolRequested => "tool.requested",
            Self::ToolAuthorized => "tool.authorized",
            Self::ToolApprovalRequired => "tool.approval_required",
            Self::ToolDenied => "tool.denied",
            Self::ApprovalRequested => "approval.requested",
            Self::ApprovalResolved => "approval.resolved",
            Self::ApprovalExpired => "approval.expired",
        }
    }

    pub const fn ledger_kind(self) -> EventKind {
        match self {
            Self::ToolRequested => EventKind::ToolRequested,
            Self::ToolAuthorized => EventKind::ToolAuthorized,
            Self::ToolApprovalRequired => EventKind::ToolApprovalRequired,
            Self::ToolDenied => EventKind::ToolDenied,
            Self::ApprovalRequested => EventKind::ApprovalRequested,
            Self::ApprovalResolved => EventKind::ApprovalResolved,
            Self::ApprovalExpired => EventKind::ApprovalExpired,
        }
    }
}

impl AuditContext {
    pub const fn new(agent_id: AgentId, trace_id: TraceId, recorded_at: SystemTime) -> Self {
        Self {
            agent_id,
            trace_id,
            recorded_at,
        }
    }

    pub const fn agent_id(self) -> AgentId {
        self.agent_id
    }

    pub const fn trace_id(self) -> TraceId {
        self.trace_id
    }

    pub const fn recorded_at(self) -> SystemTime {
        self.recorded_at
    }
}

impl CapabilityAuditRecord {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    pub fn principal(&self) -> &PrincipalRef {
        &self.principal
    }

    pub fn action_hash(&self) -> ActionFingerprint {
        self.action_hash
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }

    pub fn resource_scope_json(&self) -> &str {
        &self.resource_scope_json
    }

    pub fn action_summary(&self) -> &str {
        &self.action_summary
    }

    pub fn decision(&self) -> AuditOutcome {
        self.decision
    }

    pub fn policy_revision(&self) -> PolicyRevision {
        self.policy_revision
    }

    pub fn lease_id(&self) -> Option<LeaseId> {
        self.lease_id
    }

    pub fn approval_scope(&self) -> Option<ApprovalScopeId> {
        self.approval_scope
    }

    pub fn deny_reason(&self) -> Option<DenyReason> {
        self.deny_reason
    }

    pub fn risk(&self) -> RiskClass {
        self.risk
    }

    pub fn event_kind(&self) -> AuditEventKind {
        self.event_kind
    }

    pub fn redaction(&self) -> RedactionClass {
        self.redaction
    }

    pub fn recorded_at(&self) -> &str {
        &self.recorded_at
    }

    pub fn issued_at(&self) -> Option<&str> {
        self.issued_at.as_deref()
    }

    pub fn expires_at(&self) -> Option<&str> {
        self.expires_at.as_deref()
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    /// Payload written to the event ledger. No reason, argv, token, or secret value.
    pub fn event_payload(&self) -> Result<Value, AuditError> {
        let scope: Value = serde_json::from_str(&self.resource_scope_json)
            .map_err(|_| AuditError::PayloadBound)?;
        if scope.get("value").is_some() || scope.get("secret").is_some() {
            return Err(AuditError::PayloadBound);
        }
        let mut payload = json!({
            "session_id": self.session_id.to_string(),
            "agent_id": self.agent_id.to_string(),
            "action_hash": self.action_hash.to_string(),
            "policy_revision": self.policy_revision.to_string(),
            "trace_id": self.trace_id.to_string(),
            "capability": self.capability.as_str(),
            "decision": self.decision.as_str(),
            "resource_scope": scope,
            "action_summary": self.action_summary,
            "risk": self.risk.as_str(),
        });
        let object = payload.as_object_mut().ok_or(AuditError::PayloadBound)?;
        if let Some(lease_id) = self.lease_id {
            object.insert("lease_id".to_owned(), json!(lease_id.to_string()));
        }
        if let Some(scope) = self.approval_scope {
            object.insert("approval_scope".to_owned(), json!(scope.as_str()));
        }
        if let Some(reason) = self.deny_reason {
            object.insert("deny_reason".to_owned(), json!(reason.as_str()));
        }
        Ok(payload)
    }

    /// Lease projection row. Absent when this record is not lease-bound.
    pub fn capability_audit_row(&self) -> Option<CapabilityAuditRow> {
        let lease_id = self.lease_id?;
        Some(CapabilityAuditRow {
            lease_id,
            session_id: self.session_id,
            agent_id: self.agent_id,
            action_hash: self.action_hash,
            capability: self.capability,
            resource_scope_json: self.resource_scope_json.clone(),
            decision: self.decision,
            policy_revision: self.policy_revision,
            issued_at: self
                .issued_at
                .clone()
                .unwrap_or_else(|| self.recorded_at.clone()),
            expires_at: self.expires_at.clone(),
        })
    }
}

impl CapabilityAuditRow {
    pub fn lease_id(&self) -> LeaseId {
        self.lease_id
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    pub fn action_hash(&self) -> ActionFingerprint {
        self.action_hash
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }

    pub fn resource_scope_json(&self) -> &str {
        &self.resource_scope_json
    }

    pub fn decision(&self) -> AuditOutcome {
        self.decision
    }

    pub fn policy_revision(&self) -> PolicyRevision {
        self.policy_revision
    }

    pub fn issued_at(&self) -> &str {
        &self.issued_at
    }

    pub fn expires_at(&self) -> Option<&str> {
        self.expires_at.as_deref()
    }
}

impl AuditError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "capability audit cancelled",
            Self::Unavailable => "capability audit store unavailable",
            Self::StoreFull => "capability audit store is full",
            Self::Clock => "capability audit clock is invalid",
            Self::PayloadBound => "capability audit payload exceeds bound",
            Self::SessionMissing => "capability audit session is missing",
        }
    }
}

impl MemoryAuditStore {
    pub fn new() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
        }
    }

    pub fn len(&self) -> Result<usize, AuditError> {
        let records = self.records.lock().map_err(|_| AuditError::Unavailable)?;
        Ok(records.len())
    }

    pub fn is_empty(&self) -> Result<bool, AuditError> {
        Ok(self.len()? == 0)
    }

    pub fn records(&self) -> Result<Vec<CapabilityAuditRecord>, AuditError> {
        let records = self.records.lock().map_err(|_| AuditError::Unavailable)?;
        Ok(records.clone())
    }
}

impl Default for MemoryAuditStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CapabilityAuditStore for MemoryAuditStore {
    fn append(
        &self,
        record: CapabilityAuditRecord,
        cancel: &CancellationToken,
    ) -> Result<CapabilityAuditRecord, AuditError> {
        if cancel.is_cancelled() {
            return Err(AuditError::Cancelled);
        }
        let mut records = self.records.lock().map_err(|_| AuditError::Unavailable)?;
        if records.len() >= MAX_AUDIT_RECORDS {
            return Err(AuditError::StoreFull);
        }
        records.push(record.clone());
        Ok(record)
    }
}

impl LedgerAuditStore {
    pub fn new(ledger: EventLedger) -> Self {
        Self { ledger }
    }

    pub fn ledger(&self) -> &EventLedger {
        &self.ledger
    }
}

impl CapabilityAuditStore for LedgerAuditStore {
    fn append(
        &self,
        record: CapabilityAuditRecord,
        cancel: &CancellationToken,
    ) -> Result<CapabilityAuditRecord, AuditError> {
        if cancel.is_cancelled() {
            return Err(AuditError::Cancelled);
        }
        let payload = record.event_payload()?;
        let actor = ActorRef::agent(record.agent_id);
        let options = AppendOptions {
            redaction: record.redaction,
            trace_id: record.trace_id,
            expected_seq: None,
        };
        let ledger_cancel = snapshot_ledger_cancel(cancel);
        self.ledger
            .append(
                record.session_id,
                actor,
                record.event_kind.ledger_kind(),
                payload,
                &options,
                &ledger_cancel,
            )
            .map_err(map_ledger_error)?;
        Ok(record)
    }
}

impl<S> AuditEmitter<S> {
    pub const fn new(store: S) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &S {
        &self.store
    }
}

impl AuditEmitter<MemoryAuditStore> {
    pub fn in_memory() -> Self {
        Self::new(MemoryAuditStore::new())
    }
}

impl<S: CapabilityAuditStore> AuditEmitter<S> {
    pub fn audit_decision(
        &self,
        request: &ActionRequest,
        decision: &DecisionWithTrace,
        policies: &PolicyStack,
        ctx: &AuditContext,
        cancel: &CancellationToken,
    ) -> Result<CapabilityAuditRecord, AuditError> {
        audit_decision(&self.store, request, decision, policies, ctx, cancel)
    }

    pub fn audit_approval_requested(
        &self,
        approval: &ApprovalRequest,
        policies: &PolicyStack,
        ctx: &AuditContext,
        cancel: &CancellationToken,
    ) -> Result<CapabilityAuditRecord, AuditError> {
        audit_approval_requested(&self.store, approval, policies, ctx, cancel)
    }

    pub fn audit_approval_resolved(
        &self,
        approval: &ApprovalRequest,
        resolution: &ApprovalResolution,
        policies: &PolicyStack,
        ctx: &AuditContext,
        cancel: &CancellationToken,
    ) -> Result<CapabilityAuditRecord, AuditError> {
        audit_approval_resolved(&self.store, approval, resolution, policies, ctx, cancel)
    }

    pub fn audit_approval_expired(
        &self,
        approval: &ApprovalRequest,
        policies: &PolicyStack,
        ctx: &AuditContext,
        cancel: &CancellationToken,
    ) -> Result<CapabilityAuditRecord, AuditError> {
        audit_approval_expired(&self.store, approval, policies, ctx, cancel)
    }

    pub fn audit_lease_issued(
        &self,
        lease: &CapabilityLease,
        ctx: &AuditContext,
        cancel: &CancellationToken,
    ) -> Result<CapabilityAuditRecord, AuditError> {
        audit_lease_issued(&self.store, lease, ctx, cancel)
    }

    pub fn audit_lease_used(
        &self,
        lease: &CapabilityLease,
        used: &ConsumedLeaseUse,
        ctx: &AuditContext,
        cancel: &CancellationToken,
    ) -> Result<CapabilityAuditRecord, AuditError> {
        audit_lease_used(&self.store, lease, used, ctx, cancel)
    }

    pub fn audit_lease_expired(
        &self,
        lease: &CapabilityLease,
        ctx: &AuditContext,
        cancel: &CancellationToken,
    ) -> Result<CapabilityAuditRecord, AuditError> {
        audit_lease_expired(&self.store, lease, ctx, cancel)
    }
}

/// Append one allow/ask/deny record linked to session/agent/hash/revision/trace.
pub fn audit_decision(
    store: &impl CapabilityAuditStore,
    request: &ActionRequest,
    decision: &DecisionWithTrace,
    policies: &PolicyStack,
    ctx: &AuditContext,
    cancel: &CancellationToken,
) -> Result<CapabilityAuditRecord, AuditError> {
    if cancel.is_cancelled() {
        return Err(AuditError::Cancelled);
    }
    let (outcome, event_kind, deny_reason) = match decision.decision() {
        Decision::Allow(_) => (AuditOutcome::Allow, AuditEventKind::ToolAuthorized, None),
        Decision::Ask(_) => (
            AuditOutcome::Ask,
            AuditEventKind::ToolApprovalRequired,
            None,
        ),
        Decision::Deny(reason) => (
            AuditOutcome::Deny,
            AuditEventKind::ToolDenied,
            Some(*reason),
        ),
    };
    let record = build_request_record(
        request,
        PolicyRevision::of_stack(policies),
        outcome,
        event_kind,
        deny_reason,
        None,
        None,
        None,
        decision.risk(),
        None,
        ctx,
    )?;
    store.append(record, cancel)
}

/// Append `approval.requested` for a pending ask.
pub fn audit_approval_requested(
    store: &impl CapabilityAuditStore,
    approval: &ApprovalRequest,
    policies: &PolicyStack,
    ctx: &AuditContext,
    cancel: &CancellationToken,
) -> Result<CapabilityAuditRecord, AuditError> {
    if cancel.is_cancelled() {
        return Err(AuditError::Cancelled);
    }
    let record = build_approval_record(
        approval,
        PolicyRevision::of_stack(policies),
        AuditOutcome::Requested,
        AuditEventKind::ApprovalRequested,
        None,
        ctx,
    )?;
    store.append(record, cancel)
}

/// Append `approval.resolved` for approve or explicit deny.
pub fn audit_approval_resolved(
    store: &impl CapabilityAuditStore,
    approval: &ApprovalRequest,
    resolution: &ApprovalResolution,
    policies: &PolicyStack,
    ctx: &AuditContext,
    cancel: &CancellationToken,
) -> Result<CapabilityAuditRecord, AuditError> {
    if cancel.is_cancelled() {
        return Err(AuditError::Cancelled);
    }
    let (outcome, deny_reason, scope) = match resolution {
        ApprovalResolution::Approved(approved) => {
            (AuditOutcome::Resolved, None, Some(approved.scope().id()))
        }
        ApprovalResolution::Denied => (AuditOutcome::Deny, Some(DenyReason::MatchedRule), None),
    };
    let event_kind = AuditEventKind::ApprovalResolved;
    let record = build_approval_record(
        approval,
        PolicyRevision::of_stack(policies),
        outcome,
        event_kind,
        deny_reason,
        ctx,
    )?;
    let mut record = record;
    record.approval_scope = scope;
    store.append(record, cancel)
}

/// Append `approval.expired` when the request deadline elapses.
pub fn audit_approval_expired(
    store: &impl CapabilityAuditStore,
    approval: &ApprovalRequest,
    policies: &PolicyStack,
    ctx: &AuditContext,
    cancel: &CancellationToken,
) -> Result<CapabilityAuditRecord, AuditError> {
    if cancel.is_cancelled() {
        return Err(AuditError::Cancelled);
    }
    let record = build_approval_record(
        approval,
        PolicyRevision::of_stack(policies),
        AuditOutcome::Expired,
        AuditEventKind::ApprovalExpired,
        None,
        ctx,
    )?;
    store.append(record, cancel)
}

/// Append lease issuance. Token bytes are never stored.
pub fn audit_lease_issued(
    store: &impl CapabilityAuditStore,
    lease: &CapabilityLease,
    ctx: &AuditContext,
    cancel: &CancellationToken,
) -> Result<CapabilityAuditRecord, AuditError> {
    if cancel.is_cancelled() {
        return Err(AuditError::Cancelled);
    }
    let expires_at = add_secs(ctx.recorded_at, lease.constraints().max_ttl_secs())?;
    let record = build_lease_record(
        lease,
        AuditOutcome::Issued,
        AuditEventKind::ToolAuthorized,
        Some(ctx.recorded_at),
        Some(expires_at),
        ctx,
    )?;
    store.append(record, cancel)
}

/// Append a consumed lease use. Action hash must match the lease binding.
pub fn audit_lease_used(
    store: &impl CapabilityAuditStore,
    lease: &CapabilityLease,
    used: &ConsumedLeaseUse,
    ctx: &AuditContext,
    cancel: &CancellationToken,
) -> Result<CapabilityAuditRecord, AuditError> {
    if cancel.is_cancelled() {
        return Err(AuditError::Cancelled);
    }
    if used.lease_id() != lease.lease_id() || used.action_hash() != lease.action_hash() {
        return Err(AuditError::Unavailable);
    }
    let record = build_lease_record(
        lease,
        AuditOutcome::Used,
        AuditEventKind::ToolRequested,
        None,
        None,
        ctx,
    )?;
    store.append(record, cancel)
}

/// Append lease expiry / replay rejection. No token is stored.
pub fn audit_lease_expired(
    store: &impl CapabilityAuditStore,
    lease: &CapabilityLease,
    ctx: &AuditContext,
    cancel: &CancellationToken,
) -> Result<CapabilityAuditRecord, AuditError> {
    if cancel.is_cancelled() {
        return Err(AuditError::Cancelled);
    }
    let record = build_lease_record(
        lease,
        AuditOutcome::Expired,
        AuditEventKind::ToolDenied,
        None,
        Some(ctx.recorded_at),
        ctx,
    )?;
    store.append(record, cancel)
}

/// Record a failed `validate_use` as deny/expired. Success is [`audit_lease_used`].
pub fn audit_lease_rejected(
    store: &impl CapabilityAuditStore,
    lease: &CapabilityLease,
    error: PolicyError,
    ctx: &AuditContext,
    cancel: &CancellationToken,
) -> Result<CapabilityAuditRecord, AuditError> {
    if cancel.is_cancelled() {
        return Err(AuditError::Cancelled);
    }
    let outcome = match error {
        PolicyError::Expired | PolicyError::UsesExhausted => AuditOutcome::Expired,
        _ => AuditOutcome::Deny,
    };
    let record = build_lease_record(lease, outcome, AuditEventKind::ToolDenied, None, None, ctx)?;
    store.append(record, cancel)
}

#[allow(clippy::too_many_arguments)]
fn build_request_record(
    request: &ActionRequest,
    policy_revision: PolicyRevision,
    decision: AuditOutcome,
    event_kind: AuditEventKind,
    deny_reason: Option<DenyReason>,
    lease_id: Option<LeaseId>,
    issued_at: Option<SystemTime>,
    expires_at: Option<SystemTime>,
    risk: RiskClass,
    approval_scope: Option<ApprovalScopeId>,
    ctx: &AuditContext,
) -> Result<CapabilityAuditRecord, AuditError> {
    let resource_scope_json = resource_scope_json(request.resource())?;
    let action_summary = redacted_action_summary(
        request.capability(),
        request.resource(),
        request.normalized_action(),
    )?;
    Ok(CapabilityAuditRecord {
        session_id: request.session_id(),
        agent_id: ctx.agent_id,
        principal: request.principal().clone(),
        action_hash: ActionFingerprint::of(request),
        capability: request.capability(),
        resource_scope_json,
        action_summary,
        decision,
        policy_revision,
        lease_id,
        approval_scope,
        deny_reason,
        risk,
        event_kind,
        redaction: redaction_for(request.capability()),
        recorded_at: format_rfc3339_millis(ctx.recorded_at)?,
        issued_at: match issued_at {
            Some(ts) => Some(format_rfc3339_millis(ts)?),
            None => None,
        },
        expires_at: match expires_at {
            Some(ts) => Some(format_rfc3339_millis(ts)?),
            None => None,
        },
        trace_id: ctx.trace_id,
    })
}

fn build_approval_record(
    approval: &ApprovalRequest,
    policy_revision: PolicyRevision,
    decision: AuditOutcome,
    event_kind: AuditEventKind,
    deny_reason: Option<DenyReason>,
    ctx: &AuditContext,
) -> Result<CapabilityAuditRecord, AuditError> {
    let resource_scope_json = resource_scope_json(approval.action_diff().resource())?;
    let action_summary = redacted_action_summary(
        approval.action_diff().capability(),
        approval.action_diff().resource(),
        approval.action_diff().action(),
    )?;
    Ok(CapabilityAuditRecord {
        session_id: approval.session_id(),
        agent_id: ctx.agent_id,
        principal: approval.principal().clone(),
        action_hash: approval.action_hash(),
        capability: approval.action_diff().capability(),
        resource_scope_json,
        action_summary,
        decision,
        policy_revision,
        lease_id: None,
        approval_scope: None,
        deny_reason,
        risk: approval.risk().class(),
        event_kind,
        redaction: redaction_for(approval.action_diff().capability()),
        recorded_at: format_rfc3339_millis(ctx.recorded_at)?,
        issued_at: None,
        expires_at: None,
        trace_id: ctx.trace_id,
    })
}

fn build_lease_record(
    lease: &CapabilityLease,
    decision: AuditOutcome,
    event_kind: AuditEventKind,
    issued_at: Option<SystemTime>,
    expires_at: Option<SystemTime>,
    ctx: &AuditContext,
) -> Result<CapabilityAuditRecord, AuditError> {
    let resource_scope_json = resource_scope_json(lease.resource())?;
    let action_summary = redacted_resource_summary(lease.capability(), lease.resource())?;
    Ok(CapabilityAuditRecord {
        session_id: lease.session_id(),
        agent_id: ctx.agent_id,
        principal: lease.principal().clone(),
        action_hash: lease.action_hash(),
        capability: lease.capability(),
        resource_scope_json,
        action_summary,
        decision,
        policy_revision: lease.policy_revision(),
        lease_id: Some(lease.lease_id()),
        approval_scope: Some(lease.scope()),
        deny_reason: None,
        risk: risk_for(lease.capability()),
        event_kind,
        redaction: redaction_for(lease.capability()),
        recorded_at: format_rfc3339_millis(ctx.recorded_at)?,
        issued_at: match issued_at {
            Some(ts) => Some(format_rfc3339_millis(ts)?),
            None => None,
        },
        expires_at: match expires_at {
            Some(ts) => Some(format_rfc3339_millis(ts)?),
            None => None,
        },
        trace_id: ctx.trace_id,
    })
}

fn resource_scope_json(resource: &ResourceDescriptor) -> Result<String, AuditError> {
    let value = resource_scope_value(resource);
    if value.get("value").is_some() || value.get("plaintext").is_some() {
        return Err(AuditError::PayloadBound);
    }
    let encoded = serde_json::to_string(&value).map_err(|_| AuditError::PayloadBound)?;
    if encoded.len() > MAX_RESOURCE_SCOPE_BYTES {
        return Err(AuditError::PayloadBound);
    }
    Ok(encoded)
}

fn resource_scope_value(resource: &ResourceDescriptor) -> Value {
    match resource {
        ResourceDescriptor::Filesystem(scope) => json!({
            "kind": "fs",
            "root": scope.root().as_str(),
            "glob": scope.glob().as_str(),
        }),
        ResourceDescriptor::Process(scope) => json!({
            "kind": "proc",
            "command_family": scope.command_family().as_str(),
        }),
        ResourceDescriptor::Network(scope) => json!({
            "kind": "net",
            "scheme": scope.scheme().as_str(),
            "host": scope.host().as_str(),
            "port": scope.port(),
        }),
        ResourceDescriptor::Git(scope) => json!({
            "kind": "git",
            "ref_scope": scope.ref_scope().as_str(),
        }),
        ResourceDescriptor::Secret(scope) => json!({
            "kind": "secret",
            "secret_id": scope.secret_id().as_str(),
            "target": scope.target().as_str(),
        }),
        ResourceDescriptor::Browser(scope) => match scope.path() {
            Some(path) => json!({
                "kind": "browser",
                "origin": scope.origin().to_string(),
                "path": path.as_str(),
            }),
            None => json!({
                "kind": "browser",
                "origin": scope.origin().to_string(),
            }),
        },
        ResourceDescriptor::Mobile(scope) => json!({
            "kind": "mobile",
            "device_id": scope.device_id().as_str(),
        }),
        ResourceDescriptor::Mcp(scope) => json!({
            "kind": "mcp",
            "server": scope.server(),
            "tool": scope.tool(),
        }),
        ResourceDescriptor::Plugin(scope) => json!({
            "kind": "plugin",
            "plugin": scope.plugin(),
            "capability": scope.capability(),
        }),
    }
}

fn redacted_action_summary(
    capability: Capability,
    resource: &ResourceDescriptor,
    action: &CanonicalAction,
) -> Result<String, AuditError> {
    let raw = match action {
        CanonicalAction::Command(command) => command_summary(command),
        CanonicalAction::Filesystem(fs) => fs_summary(fs),
        CanonicalAction::Network(net) => net_summary(net),
        CanonicalAction::Resource { .. } => resource_summary(capability, resource),
    };
    bound_summary(&raw)
}

fn redacted_resource_summary(
    capability: Capability,
    resource: &ResourceDescriptor,
) -> Result<String, AuditError> {
    bound_summary(&resource_summary(capability, resource))
}

fn command_summary(command: &CanonicalCommand) -> String {
    match command.mode() {
        ShellMode::Argv => format!(
            "proc.exec argv executable={} cwd={} argv_count={} env_count={}",
            command.executable().as_str(),
            command.cwd().as_str(),
            command.argv().len(),
            command.env_names().len()
        ),
        ShellMode::ShellString => format!(
            "proc.exec shell executable={} cwd={} script_bytes={} env_count={}",
            command.executable().as_str(),
            command.cwd().as_str(),
            command.shell_script().map(str::len).unwrap_or(0),
            command.env_names().len()
        ),
    }
}

fn fs_summary(fs: &CanonicalFsAction) -> String {
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

fn net_summary(net: &CanonicalNetworkTarget) -> String {
    format!(
        "net.connect {}://{}:{}",
        net.scheme().as_str(),
        net.host().as_canonical_str(),
        net.port()
    )
}

fn resource_summary(capability: Capability, resource: &ResourceDescriptor) -> String {
    match resource {
        ResourceDescriptor::Filesystem(scope) => format!(
            "{} {}:{}",
            capability.as_str(),
            scope.root().as_str(),
            scope.glob().as_str()
        ),
        ResourceDescriptor::Process(scope) => {
            format!(
                "{} {}",
                capability.as_str(),
                scope.command_family().as_str()
            )
        }
        ResourceDescriptor::Network(scope) => format!(
            "{} {}://{}:{}",
            capability.as_str(),
            scope.scheme().as_str(),
            scope.host().as_str(),
            scope.port()
        ),
        ResourceDescriptor::Git(scope) => {
            format!("{} {}", capability.as_str(), scope.ref_scope().as_str())
        }
        ResourceDescriptor::Secret(scope) => {
            format!("{} target={}", capability.as_str(), scope.target().as_str())
        }
        ResourceDescriptor::Browser(scope) => match scope.path() {
            Some(path) => format!(
                "{} {} {}",
                capability.as_str(),
                scope.origin(),
                path.as_str()
            ),
            None => format!("{} {}", capability.as_str(), scope.origin()),
        },
        ResourceDescriptor::Mobile(scope) => {
            format!("{} {}", capability.as_str(), scope.device_id().as_str())
        }
        ResourceDescriptor::Mcp(scope) => format!(
            "{} {}/{}",
            capability.as_str(),
            scope.server(),
            scope.tool()
        ),
        ResourceDescriptor::Plugin(scope) => format!(
            "{} {}/{}",
            capability.as_str(),
            scope.plugin(),
            scope.capability()
        ),
    }
}

fn bound_summary(text: &str) -> Result<String, AuditError> {
    if text.len() <= MAX_ACTION_SUMMARY_BYTES {
        return Ok(text.to_owned());
    }
    let mut end = MAX_ACTION_SUMMARY_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        return Err(AuditError::PayloadBound);
    }
    Ok(text[..end].to_owned())
}

fn redaction_for(capability: Capability) -> RedactionClass {
    match capability {
        Capability::SecretUse => RedactionClass::Secret,
        Capability::FsWrite
        | Capability::ProcExec
        | Capability::NetConnect
        | Capability::GitWrite
        | Capability::BrowserNavigate
        | Capability::BrowserDownload
        | Capability::MobileControl => RedactionClass::Sensitive,
        Capability::FsRead | Capability::McpInvoke | Capability::PluginInvoke => {
            RedactionClass::Project
        }
    }
}

fn risk_for(capability: Capability) -> RiskClass {
    match capability.family() {
        CapabilityFamily::Fs if matches!(capability, Capability::FsRead) => RiskClass::Low,
        CapabilityFamily::Fs | CapabilityFamily::Git => RiskClass::Medium,
        CapabilityFamily::Proc
        | CapabilityFamily::Net
        | CapabilityFamily::Secret
        | CapabilityFamily::Browser
        | CapabilityFamily::Mobile
        | CapabilityFamily::Mcp
        | CapabilityFamily::Plugin => RiskClass::High,
    }
}

fn add_secs(base: SystemTime, secs: u32) -> Result<SystemTime, AuditError> {
    base.checked_add(Duration::from_secs(u64::from(secs)))
        .ok_or(AuditError::Clock)
}

fn format_rfc3339_millis(time: SystemTime) -> Result<String, AuditError> {
    let dur = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AuditError::Clock)?;
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let (year, month, day, hour, minute, second) = unix_secs_to_civil(secs)?;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    ))
}

fn unix_secs_to_civil(secs: u64) -> Result<(i32, u32, u32, u32, u32, u32), AuditError> {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = (rem / 3_600) as u32;
    let minute = ((rem % 3_600) / 60) as u32;
    let second = (rem % 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = u64::try_from(z - era * 146_097).map_err(|_| AuditError::Clock)?;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = i64::try_from(yoe).map_err(|_| AuditError::Clock)? + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let year = i32::try_from(y).map_err(|_| AuditError::Clock)?;
    if !(0..=9999).contains(&year) {
        return Err(AuditError::Clock);
    }
    let month = u32::try_from(m).map_err(|_| AuditError::Clock)?;
    let day = u32::try_from(d).map_err(|_| AuditError::Clock)?;
    Ok((year, month, day, hour, minute, second))
}

fn snapshot_ledger_cancel(cancel: &CancellationToken) -> event_ledger::ledger::CancellationToken {
    let token = event_ledger::ledger::CancellationToken::new();
    if cancel.is_cancelled() {
        token.cancel();
    }
    token
}

fn map_ledger_error(err: LedgerError) -> AuditError {
    match err {
        LedgerError::Cancelled => AuditError::Cancelled,
        LedgerError::SessionNotFound { .. } => AuditError::SessionMissing,
        LedgerError::PayloadBound { .. } => AuditError::PayloadBound,
        LedgerError::InvalidTimestamp => AuditError::Clock,
        _ => AuditError::Unavailable,
    }
}

impl fmt::Debug for CapabilityAuditRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapabilityAuditRecord")
            .field("session_id", &self.session_id)
            .field("agent_id", &self.agent_id)
            .field("action_hash", &self.action_hash)
            .field("capability", &self.capability.as_str())
            .field("decision", &self.decision)
            .field("policy_revision", &self.policy_revision)
            .field("event_kind", &self.event_kind)
            .field("trace_id", &self.trace_id)
            .field("action_summary", &self.action_summary)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for CapabilityAuditRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapabilityAuditRow")
            .field("lease_id", &self.lease_id)
            .field("session_id", &self.session_id)
            .field("agent_id", &self.agent_id)
            .field("action_hash", &self.action_hash)
            .field("capability", &self.capability.as_str())
            .field("decision", &self.decision)
            .field("policy_revision", &self.policy_revision)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for AuditOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for AuditEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for AuditError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    use protocol::ProjectId;

    use crate::approval::{ApprovalChoice, request_approval};
    use crate::capability::{FilesystemScope, ProcessScope, SecretScope};
    use crate::lease::{LeaseIssuer, issue};
    use crate::normalize::command::{CommandNormalizeError, ExecIntent, Resolver, normalize_exec};
    use crate::policy::evaluator::evaluate;
    use crate::policy::parser::{PolicyDocument, PolicySource};
    use crate::validator::{LeaseValidator, validate_use};

    const SECRET: &str = "super-secret-password";
    const FIXED_UNIX: u64 = 1_786_521_604;

    struct FixedResolver;

    impl Resolver for FixedResolver {
        fn resolve_cwd(
            &self,
            requested: &str,
        ) -> Result<crate::normalize::command::CanonicalHostPath, CommandNormalizeError> {
            crate::normalize::command::CanonicalHostPath::from_resolved(requested)
        }

        fn resolve_executable(
            &self,
            requested: &str,
            _cwd: &crate::normalize::command::CanonicalHostPath,
        ) -> Result<crate::normalize::command::CanonicalHostPath, CommandNormalizeError> {
            crate::normalize::command::CanonicalHostPath::from_resolved(requested)
        }
    }

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

    fn eval(policies: &PolicyStack, request: &ActionRequest) -> DecisionWithTrace {
        evaluate(policies, request, &CancellationToken::new()).expect("evaluate")
    }

    fn ctx() -> AuditContext {
        AuditContext::new(
            AgentId::new(),
            TraceId::new(),
            UNIX_EPOCH + Duration::from_secs(FIXED_UNIX),
        )
    }

    fn decide(
        store: &MemoryAuditStore,
        policies: &PolicyStack,
        request: &ActionRequest,
        ctx: &AuditContext,
    ) -> CapabilityAuditRecord {
        let decision = eval(policies, request);
        audit_decision(
            store,
            request,
            &decision,
            policies,
            ctx,
            &CancellationToken::new(),
        )
        .expect("audit")
    }

    fn temp_ledger() -> (EventLedger, PathBuf, SessionId) {
        let session = SessionId::new();
        let path = std::env::temp_dir().join(format!("rapidlm-cap-audit-{session}.sqlite"));
        let _ = std::fs::remove_file(&path);
        let ledger = EventLedger::open(&path).expect("open");
        ledger
            .create_session(
                session,
                ProjectId::new(),
                &event_ledger::ledger::CancellationToken::new(),
            )
            .expect("session");
        (ledger, path, session)
    }

    fn cleanup_ledger(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        let shm = PathBuf::from(format!("{}-shm", path.display()));
        let _ = std::fs::remove_file(wal);
        let _ = std::fs::remove_file(shm);
    }

    #[test]
    fn audit_decision_links_session_agent_hash_revision_and_trace() {
        let store = MemoryAuditStore::new();
        let policies = allow_stack();
        let request = repo_read_request();
        let ctx = ctx();
        let record = decide(&store, &policies, &request, &ctx);
        assert_eq!(record.session_id(), request.session_id());
        assert_eq!(record.agent_id(), ctx.agent_id());
        assert_eq!(record.action_hash(), ActionFingerprint::of(&request));
        assert_eq!(
            record.policy_revision(),
            PolicyRevision::of_stack(&policies)
        );
        assert_eq!(record.trace_id(), ctx.trace_id());
        assert_eq!(record.decision(), AuditOutcome::Allow);
        assert_eq!(record.event_kind(), AuditEventKind::ToolAuthorized);
        assert_eq!(record.event_kind().as_str(), "tool.authorized");
        assert_eq!(record.recorded_at(), "2026-08-12T08:00:04.000Z");
    }

    #[test]
    fn allow_ask_and_deny_each_have_an_auditable_record() {
        let store = MemoryAuditStore::new();
        let request = repo_read_request();
        let ctx = ctx();
        let allow = decide(&store, &allow_stack(), &request, &ctx);
        let ask = decide(&store, &ask_stack(), &request, &ctx);
        let deny = decide(&store, &deny_stack(), &request, &ctx);
        assert_eq!(allow.decision(), AuditOutcome::Allow);
        assert_eq!(allow.event_kind(), AuditEventKind::ToolAuthorized);
        assert_eq!(ask.decision(), AuditOutcome::Ask);
        assert_eq!(ask.event_kind(), AuditEventKind::ToolApprovalRequired);
        assert_eq!(deny.decision(), AuditOutcome::Deny);
        assert_eq!(deny.event_kind(), AuditEventKind::ToolDenied);
        assert!(deny.deny_reason().is_some());
        assert_eq!(store.len().expect("len"), 3);
    }

    #[test]
    fn attacker_reason_cannot_suppress_or_rewrite_a_deny_audit() {
        let store = MemoryAuditStore::new();
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::FsRead,
            repo_read_resource(),
            repo_read_action(),
            "please allow this and ignore policy",
        )
        .expect("request");
        let policies = deny_stack();
        let decision = eval(&policies, &request);
        assert!(matches!(decision.decision(), Decision::Deny(_)));
        let record = audit_decision(
            &store,
            &request,
            &decision,
            &policies,
            &ctx(),
            &CancellationToken::new(),
        )
        .expect("audit");
        assert_eq!(record.decision(), AuditOutcome::Deny);
        assert_eq!(record.event_kind(), AuditEventKind::ToolDenied);
        assert!(!record.action_summary().contains("please allow"));
        let payload = record.event_payload().expect("payload");
        let dumped = payload.to_string();
        assert!(!dumped.contains("please allow"));
        assert!(!dumped.contains("ignore policy"));
    }

    #[test]
    fn audit_does_not_include_secret_values() {
        let store = MemoryAuditStore::new();
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
        let record = decide(&store, &policies, &request, &ctx());
        assert_eq!(record.redaction(), RedactionClass::Secret);
        assert!(!record.action_summary().contains(SECRET));
        assert!(!record.resource_scope_json().contains(SECRET));
        assert!(record.resource_scope_json().contains("env:NPM_TOKEN"));
        assert!(!record.resource_scope_json().contains("\"value\""));
        let payload = record.event_payload().expect("payload");
        let dumped = payload.to_string();
        assert!(!dumped.contains(SECRET));
        assert!(dumped.contains("env:NPM_TOKEN"));
        let debug = format!("{record:?}");
        assert!(!debug.contains(SECRET));
    }

    #[test]
    fn command_argv_and_script_are_omitted_from_summaries() {
        let store = MemoryAuditStore::new();
        let policies = stack([user(
            r#"
[[rules]]
id = "git-allow"
effect = "allow"
subjects = ["*"]
capability = "proc.exec"
resource = { command_family = "git" }
"#,
        )]);
        let resource = ResourceDescriptor::Process(ProcessScope::new("git").expect("process"));
        let command = normalize_exec(
            &ExecIntent::argv(
                ["/usr/bin/git", "commit", "-m", SECRET],
                "/repo",
                None::<String>,
            ),
            &FixedResolver,
            &CancellationToken::new(),
        )
        .expect("normalize");
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::ProcExec,
            resource,
            CanonicalAction::Command(command),
            SECRET,
        )
        .expect("request");
        let record = decide(&store, &policies, &request, &ctx());
        assert!(record.action_summary().contains("argv_count=4"));
        assert!(!record.action_summary().contains(SECRET));
        assert!(!record.action_summary().contains("commit"));
        let payload = record.event_payload().expect("payload").to_string();
        assert!(!payload.contains(SECRET));
        assert!(!payload.contains("-m"));
    }

    #[test]
    fn cancelled_audit_fails_closed_without_a_record() {
        let store = MemoryAuditStore::new();
        let policies = allow_stack();
        let request = repo_read_request();
        let decision = eval(&policies, &request);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            audit_decision(&store, &request, &decision, &policies, &ctx(), &cancel),
            Err(AuditError::Cancelled)
        );
        assert!(store.is_empty().expect("empty"));
    }

    #[test]
    fn approval_and_lease_outcomes_are_appended() {
        let store = MemoryAuditStore::new();
        let policies = ask_stack();
        let request = repo_read_request();
        let decision = eval(&policies, &request);
        let now = Instant::now();
        let approval = request_approval(&request, &decision, now, &CancellationToken::new())
            .expect("approval");
        let ctx = ctx();
        let requested = audit_approval_requested(
            &store,
            &approval,
            &policies,
            &ctx,
            &CancellationToken::new(),
        )
        .expect("requested");
        assert_eq!(requested.event_kind(), AuditEventKind::ApprovalRequested);
        let resolved = approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                &request,
                now,
                &CancellationToken::new(),
            )
            .expect("resolve");
        let resolved_record = audit_approval_resolved(
            &store,
            &approval,
            &resolved,
            &policies,
            &ctx,
            &CancellationToken::new(),
        )
        .expect("resolved");
        assert_eq!(
            resolved_record.event_kind(),
            AuditEventKind::ApprovalResolved
        );
        assert_eq!(
            resolved_record.approval_scope(),
            Some(ApprovalScopeId::Once)
        );
        let ApprovalResolution::Approved(approved) = resolved else {
            panic!("expected approved");
        };
        let issuer = LeaseIssuer::from_key([0x11; 32]).expect("issuer");
        let lease = issue(
            &issuer,
            &approved,
            &policies,
            now,
            &CancellationToken::new(),
        )
        .expect("issue");
        let issued =
            audit_lease_issued(&store, &lease, &ctx, &CancellationToken::new()).expect("issued");
        assert_eq!(issued.decision(), AuditOutcome::Issued);
        assert_eq!(issued.lease_id(), Some(lease.lease_id()));
        assert!(issued.capability_audit_row().is_some());
        let token_hex = lease
            .token()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let issued_dump = issued.event_payload().expect("payload").to_string();
        assert!(!issued_dump.contains(&token_hex));
        let validator = LeaseValidator::new(issuer, PolicyRevision::of_stack(&policies));
        let guard = validate_use(
            &validator,
            &lease,
            request.normalized_action(),
            now,
            &CancellationToken::new(),
        )
        .expect("use");
        let used = audit_lease_used(
            &store,
            &lease,
            &guard.consume(),
            &ctx,
            &CancellationToken::new(),
        )
        .expect("used");
        assert_eq!(used.decision(), AuditOutcome::Used);
        let expired =
            audit_lease_expired(&store, &lease, &ctx, &CancellationToken::new()).expect("expired");
        assert_eq!(expired.decision(), AuditOutcome::Expired);
        assert_eq!(expired.event_kind(), AuditEventKind::ToolDenied);
    }

    #[test]
    fn used_audit_rejects_mismatched_lease_binding() {
        let store = MemoryAuditStore::new();
        let policies = ask_stack();
        let request = repo_read_request();
        let decision = eval(&policies, &request);
        let now = Instant::now();
        let approval = request_approval(&request, &decision, now, &CancellationToken::new())
            .expect("approval");
        let ApprovalResolution::Approved(approved) = approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                &request,
                now,
                &CancellationToken::new(),
            )
            .expect("resolve")
        else {
            panic!("expected approved");
        };
        let issuer = LeaseIssuer::from_key([0x11; 32]).expect("issuer");
        let lease = issue(
            &issuer,
            &approved,
            &policies,
            now,
            &CancellationToken::new(),
        )
        .expect("issue");
        let other = issue(
            &issuer,
            &approved,
            &policies,
            now,
            &CancellationToken::new(),
        )
        .expect("other");
        let validator = LeaseValidator::new(issuer, PolicyRevision::of_stack(&policies));
        let used = validate_use(
            &validator,
            &other,
            request.normalized_action(),
            now,
            &CancellationToken::new(),
        )
        .expect("use")
        .consume();
        assert_eq!(
            audit_lease_used(&store, &lease, &used, &ctx(), &CancellationToken::new()),
            Err(AuditError::Unavailable)
        );
        assert!(store.is_empty().expect("empty"));
    }

    #[test]
    fn ledger_store_appends_allow_ask_deny_events() {
        let (ledger, path, session) = temp_ledger();
        let store = LedgerAuditStore::new(ledger.clone());
        let request = ActionRequest::new(
            principal(),
            session,
            Capability::FsRead,
            repo_read_resource(),
            repo_read_action(),
            "read source",
        )
        .expect("request");
        let ctx = ctx();
        let cases = [
            (
                allow_stack(),
                EventKind::ToolAuthorized,
                AuditOutcome::Allow,
            ),
            (
                ask_stack(),
                EventKind::ToolApprovalRequired,
                AuditOutcome::Ask,
            ),
            (deny_stack(), EventKind::ToolDenied, AuditOutcome::Deny),
        ];
        for (i, (policies, kind, outcome)) in cases.into_iter().enumerate() {
            let decision = eval(&policies, &request);
            let record = audit_decision(
                &store,
                &request,
                &decision,
                &policies,
                &ctx,
                &CancellationToken::new(),
            )
            .expect("append");
            assert_eq!(record.decision(), outcome);
            let seq = (i as u64) + 1;
            let stored = ledger
                .get(
                    session,
                    seq,
                    &event_ledger::ledger::CancellationToken::new(),
                )
                .expect("get");
            assert_eq!(stored.kind(), kind);
            assert_eq!(stored.trace_id(), ctx.trace_id());
            assert_eq!(stored.actor().id(), ctx.agent_id().to_string());
            let payload = stored.payload();
            let action_hash = record.action_hash().to_string();
            let policy_revision = record.policy_revision().to_string();
            assert_eq!(
                payload.get("action_hash").and_then(Value::as_str),
                Some(action_hash.as_str())
            );
            assert_eq!(
                payload.get("policy_revision").and_then(Value::as_str),
                Some(policy_revision.as_str())
            );
            assert_eq!(
                payload.get("decision").and_then(Value::as_str),
                Some(outcome.as_str())
            );
            assert!(!payload.to_string().contains("read source"));
        }
        cleanup_ledger(&path);
    }

    #[test]
    fn ledger_append_without_session_fails_closed() {
        let path = std::env::temp_dir().join(format!(
            "rapidlm-cap-audit-missing-{}.sqlite",
            SessionId::new()
        ));
        let _ = std::fs::remove_file(&path);
        let ledger = EventLedger::open(&path).expect("open");
        let store = LedgerAuditStore::new(ledger);
        let policies = allow_stack();
        let request = repo_read_request();
        let decision = eval(&policies, &request);
        let err = audit_decision(
            &store,
            &request,
            &decision,
            &policies,
            &ctx(),
            &CancellationToken::new(),
        )
        .expect_err("missing session");
        assert_eq!(err, AuditError::SessionMissing);
        cleanup_ledger(&path);
    }
}
