#![forbid(unsafe_code)]

pub mod approval;
pub mod audit;
pub mod capability;
pub mod lease;
pub mod validator;
pub mod normalize {
    pub mod command;
    pub mod fs;
    pub mod network;
}
pub mod policy {
    pub mod evaluator;
    pub mod parser;
}
pub mod projection;

pub use approval::{
    ActionFingerprint, ApprovalChoice, ApprovalError, ApprovalId, ApprovalRequest,
    ApprovalResolution, ApprovalScopeChoice, ApprovalScopeId, ApprovalScopeKind, ApprovalSpec,
    ApprovedAction, DEFAULT_APPROVAL_TTL_SECS, MAX_ACTION_DIFF_BYTES, NormalizedActionDiff,
    RiskSummary, request_approval,
};
pub use audit::{
    AuditContext, AuditEmitter, AuditError, AuditEventKind, AuditOutcome, CapabilityAuditRecord,
    CapabilityAuditRow, CapabilityAuditStore, LedgerAuditStore, MAX_ACTION_SUMMARY_BYTES,
    MAX_AUDIT_RECORDS, MAX_RESOURCE_SCOPE_BYTES, MemoryAuditStore, audit_approval_expired,
    audit_approval_requested, audit_approval_resolved, audit_decision, audit_lease_expired,
    audit_lease_issued, audit_lease_rejected, audit_lease_used,
};
pub use capability::{
    BrowserScope, CAPABILITY_SCHEMA, CAPABILITY_SCHEMA_VERSION, Capability, CapabilityError,
    CapabilityFamily, CommandFamily, DeviceId, FilesystemRoot, FilesystemScope, GitRefScope,
    GitScope, Hostname, McpScope, MobileScope, NetworkScheme, NetworkScope, Origin, PathGlob,
    PluginScope, ProcessScope, RESOURCE_SCHEMA, ResourceDescriptor, SecretHandle, SecretScope,
    SecretUseTarget,
};
pub use lease::{
    CapabilityLease, DEFAULT_LEASE_MAX_USES, DEFAULT_LEASE_TTL_SECS, LeaseError, LeaseIssuer,
    LeaseToken, PolicyRevision, issue,
};
pub use normalize::command::{
    CancellationToken, CanonicalCommand, CanonicalHostPath, CommandInvocation,
    CommandNormalizeError, ExecIntent, Resolver, ShellMode, normalize_exec,
};
pub use normalize::fs::{
    CanonicalFsAction, CanonicalFsIdentity, CanonicalFsTarget, FsIntent, FsNormalizeError,
    FsOpKind, FsOperation, FsResolver, MAX_FS_PATH_BYTES, MAX_SYMLINK_HOPS, normalize_fs,
};
pub use normalize::network::{
    CanonicalNetHost, CanonicalNetworkTarget, IpClass, MAX_REDIRECT_HOPS, MAX_RESOLVED_IPS,
    MAX_URL_BYTES, NetworkIntent, NetworkNormalizeError, NetworkResolver, RedirectHop,
    RedirectRevalidation, normalize_network,
};
pub use policy::evaluator::{
    ActionRequest, AskExplanation, CanonicalAction, Decision, DecisionWithTrace, DenyReason,
    LeaseConstraints, MAX_POLICY_STACK_DOCUMENTS, MAX_PRINCIPAL_BYTES, MAX_REASON_BYTES,
    MatchedRule, PolicyEvalError, PolicyStack, PrincipalRef, RiskClass, RuleConsideration,
    dont_ask, evaluate,
};
pub use policy::parser::{
    MAX_POLICY_DOCUMENT_BYTES, MAX_POLICY_RULES, POLICY_SCHEMA, PolicyDocument, PolicyEffect,
    PolicyField, PolicyLayer, PolicyParseError, PolicyRule, PolicySource, ResourcePattern,
    RuleConditions, RuleId, SubjectPattern,
};
pub use projection::{
    ALL_CAPABILITIES, CapabilityProjection, ProjectedCapability, ProjectionMode, ProjectionModel,
    ProjectionNode, ProjectionRole, ProjectionScope,
};
pub use validator::{
    ConsumedLeaseUse, LeaseUseGuard, LeaseValidator, PolicyError, PolicyRevisionMode, validate_use,
};
