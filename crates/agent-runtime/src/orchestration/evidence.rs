//! Evidence nodes, verification edges, candidate completions, workspace identity.
//!
//! Extends [`crate::EvidenceKind`] rather than replacing [`crate::EvidenceStore`].

use protocol::{ArtifactRef, EvidenceId, GoalId};
use serde::{Deserialize, Serialize};

use crate::evidence::EvidenceKind;

/// Maximum evidence refs on one candidate.
pub const MAX_CANDIDATE_EVIDENCE: usize = 64;

/// SHA-256 hex of the workspace snapshot the attestation binds to.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct WorkspaceIdentity {
    pub hash: String,
}

/// Evidence kinds used by orchestration. Maps onto [`EvidenceKind`] when possible.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationEvidenceKind {
    Diff,
    Test,
    Lint,
    Typecheck,
    Build,
    Runtime,
    Diagnostic,
    File,
    Search,
    Tool,
    Policy,
    Agent,
    User,
    External,
}

/// Trust assigned by the producer, never inferred from prose.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceTrust {
    Low,
    Medium,
    High,
    Deterministic,
}

/// First-class evidence node. Natural-language summary is never sufficient alone.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceNode {
    pub id: EvidenceId,
    pub task_id: GoalId,
    pub requirement_ids: Vec<String>,
    pub producer: String,
    pub kind: OrchestrationEvidenceKind,
    pub source: String,
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_ref: Option<WorkspaceIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_payload: Option<String>,
    pub trust_level: EvidenceTrust,
}

/// Claim that a requirement is satisfied. Host still evaluates evidence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementClaim {
    pub requirement_id: String,
    pub claimed_status: RequirementClaimStatus,
    pub evidence_refs: Vec<EvidenceId>,
    pub explanation: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementClaimStatus {
    Satisfied,
    Unsatisfied,
    Partial,
}

/// Implementer completion package. `completion_claim` is not acceptance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateCompletion {
    pub task_id: GoalId,
    pub summary: String,
    #[serde(default)]
    pub changed_artifacts: Vec<String>,
    pub requirement_claims: Vec<RequirementClaim>,
    pub evidence_refs: Vec<EvidenceId>,
    #[serde(default)]
    pub checks_requested: Vec<String>,
    #[serde(default)]
    pub known_limitations: Vec<String>,
    #[serde(default)]
    pub unresolved_items: Vec<String>,
    pub completion_claim: CompletionClaim,
}

/// Model-side claim. Host ignores this when deciding Accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionClaim {
    Done,
    Partial,
    Blocked,
}

/// Edge in the requirement↔evidence DAG.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationEdge {
    pub id: String,
    pub from_node: String,
    pub to_node: String,
    pub relation: VerificationRelation,
    pub producer: String,
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationRelation {
    Supports,
    Contradicts,
    Verifies,
    Refutes,
    DerivedFrom,
    ProducedBy,
    Supersedes,
    Resolves,
}

impl WorkspaceIdentity {
    pub fn new(hash: impl Into<String>) -> Self {
        Self { hash: hash.into() }
    }
}

impl OrchestrationEvidenceKind {
    pub const fn as_evidence_kind(self) -> EvidenceKind {
        match self {
            Self::Diff => EvidenceKind::Diff,
            Self::Test => EvidenceKind::Test,
            Self::Lint => EvidenceKind::Lint,
            Self::Typecheck | Self::Build => EvidenceKind::Build,
            Self::Runtime => EvidenceKind::RuntimeObservation,
            Self::Diagnostic => EvidenceKind::Scan,
            Self::File | Self::Search => EvidenceKind::Source,
            Self::Tool => EvidenceKind::Command,
            Self::Policy => EvidenceKind::Scan,
            Self::Agent => EvidenceKind::ManualReview,
            Self::User => EvidenceKind::UserConfirmation,
            Self::External => EvidenceKind::ExternalAttestation,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Diff => "diff",
            Self::Test => "test",
            Self::Lint => "lint",
            Self::Typecheck => "typecheck",
            Self::Build => "build",
            Self::Runtime => "runtime",
            Self::Diagnostic => "diagnostic",
            Self::File => "file",
            Self::Search => "search",
            Self::Tool => "tool",
            Self::Policy => "policy",
            Self::Agent => "agent",
            Self::User => "user",
            Self::External => "external",
        }
    }
}
