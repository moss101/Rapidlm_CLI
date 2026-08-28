//! Host-owned verified orchestration substrate.
//!
//! Dormant unless [`protocol::OrchestrationMode::Verified`] is selected.
//! Models return typed results; only [`Supervisor::accept`] can reach
//! [`OrchestrationState::Accepted`]. Completes via the existing
//! [`crate::EvidenceStore`] rather than a second evidence authority.

pub mod checks;
pub mod contract;
pub mod events;
pub mod evidence;
pub mod gaps;
pub mod policy;
pub mod state;
pub mod supervisor;
pub mod verification;

pub use checks::{
    CheckKind, CheckResult, CheckRunner, CheckStatus, MAX_CHECK_SUMMARY_BYTES, VerificationCheck,
};
pub use contract::{
    AcceptanceCriterion, MAX_ID_BYTES, MAX_OBJECTIVE_BYTES, MAX_REQUIREMENTS, RequirementNode,
    RequirementPriority, TaskContract, TaskContractError, WorkspacePolicy,
};
pub use events::{
    MemoryEventSink, OrchestrationEvent, OrchestrationEventKind, OrchestrationEventSink,
};
pub use evidence::{
    CandidateCompletion, CompletionClaim, EvidenceNode, EvidenceTrust, MAX_CANDIDATE_EVIDENCE,
    OrchestrationEvidenceKind, RequirementClaim, RequirementClaimStatus, VerificationEdge,
    VerificationRelation, WorkspaceIdentity,
};
pub use gaps::{GapCategory, GapNode, GapSeverity, GapStatus, RepairDirective, stable_gap_id};
pub use policy::{
    AcceptancePolicy, AggregationPolicy, OrchestrationBudget, OrchestrationRole, QuorumPolicy,
    RoleModelResolver, StagnationDetector, StagnationSignal, TaskComplexity, VerificationPolicy,
    VerifierPanel, WriteClass, profile_allows_writes,
};
pub use state::{
    OrchestrationState, OrchestrationTransition, TransitionError, validate_transition,
};
pub use supervisor::{
    AgentContextPacket, DiscoveryResult, Explorer, Implementer, PlanResult, Planner, Retriever,
    Strategist, StrategyRevision, Supervisor, SupervisorDrivers, SupervisorError, Verifier,
};
pub use verification::{
    Attestation, RequirementVerification, RequirementVerificationStatus, Verdict,
    VerificationVerdict,
};
