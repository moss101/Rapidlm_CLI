//! Independent verifier verdicts and append-only attestations.

use protocol::{EvidenceId, GoalId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::orchestration::evidence::WorkspaceIdentity;
use crate::orchestration::gaps::GapNode;

/// Panel-level verdict. Inconclusive cannot accept under Strict policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Verified,
    Refuted,
    Inconclusive,
    Blocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementVerificationStatus {
    Satisfied,
    Unsatisfied,
    InsufficientEvidence,
}

/// Per-requirement result produced by a skeptic (or the host gate).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementVerification {
    pub requirement_id: String,
    pub status: RequirementVerificationStatus,
    pub evidence_refs: Vec<EvidenceId>,
    pub gap_refs: Vec<String>,
    pub explanation: String,
}

/// Structured verifier output. Never "looks good to me".
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationVerdict {
    pub task_id: GoalId,
    pub verifier_id: String,
    pub verdict: Verdict,
    pub requirement_results: Vec<RequirementVerification>,
    pub evidence_used: Vec<EvidenceId>,
    pub gaps: Vec<GapNode>,
    pub confidence: u8,
    #[serde(default)]
    pub notes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<Attestation>,
}

/// Append-only attestation bound to a workspace identity. Never overwritten.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    pub id: String,
    pub task_id: GoalId,
    pub verifier_id: String,
    pub verifier_model: String,
    pub verification_round: u32,
    pub verdict: Verdict,
    pub requirement_coverage: u8,
    pub evidence_refs: Vec<EvidenceId>,
    pub gap_refs: Vec<String>,
    pub timestamp: u64,
    pub workspace_identity: WorkspaceIdentity,
    pub digest: String,
}

impl Attestation {
    #[allow(clippy::too_many_arguments)]
    pub fn issue(
        task_id: GoalId,
        verifier_id: &str,
        verifier_model: &str,
        round: u32,
        verdict: Verdict,
        requirement_coverage: u8,
        evidence_refs: Vec<EvidenceId>,
        gap_refs: Vec<String>,
        timestamp: u64,
        workspace_identity: WorkspaceIdentity,
    ) -> Self {
        let id = format!("att-{task_id}-{round}");
        let mut preimage = String::new();
        preimage.push_str(&id);
        preimage.push_str(verifier_id);
        preimage.push_str(verdict.as_str());
        preimage.push_str(&workspace_identity.hash);
        preimage.push_str(&round.to_string());
        let digest = hex_sha256(preimage.as_bytes());
        Self {
            id,
            task_id,
            verifier_id: verifier_id.to_owned(),
            verifier_model: verifier_model.to_owned(),
            verification_round: round,
            verdict,
            requirement_coverage,
            evidence_refs,
            gap_refs,
            timestamp,
            workspace_identity,
            digest,
        }
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in hash {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

impl Verdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Refuted => "refuted",
            Self::Inconclusive => "inconclusive",
            Self::Blocked => "blocked",
        }
    }
}
