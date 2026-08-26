//! Structured gap nodes and repair directives. IDs are stable across rounds.

use protocol::{EvidenceId, GoalId};
use serde::{Deserialize, Serialize};

use crate::orchestration::policy::StagnationDetector;

/// Gap category. Used for stable identity, not just reviewer prose.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapCategory {
    MissingImplementation,
    IncorrectImplementation,
    MissingTest,
    FailedTest,
    Regression,
    InsufficientEvidence,
    RequirementMismatch,
    SecurityFailure,
    PolicyFailure,
    BuildFailure,
    TypeFailure,
    RuntimeFailure,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapSeverity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapStatus {
    Open,
    Resolved,
    Superseded,
}

/// Structured verification gap. Not a free-form review comment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapNode {
    pub id: String,
    pub task_id: GoalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_criterion_id: Option<String>,
    pub category: GapCategory,
    pub severity: GapSeverity,
    pub description: String,
    #[serde(default)]
    pub evidence_refs: Vec<EvidenceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_validation: Option<String>,
    pub first_seen_round: u32,
    pub last_seen_round: u32,
    pub status: GapStatus,
    #[serde(default)]
    pub resolution_evidence: Vec<EvidenceId>,
}

/// Host directive for a repair implementer. No prior transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairDirective {
    pub task_id: GoalId,
    pub round: u32,
    pub unresolved_gap_ids: Vec<String>,
    pub relevant_requirement_ids: Vec<String>,
    pub selected_evidence: Vec<EvidenceId>,
    pub previous_attempt_summary: String,
    #[serde(default)]
    pub constraints: Vec<String>,
}

impl GapCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingImplementation => "missing_implementation",
            Self::IncorrectImplementation => "incorrect_implementation",
            Self::MissingTest => "missing_test",
            Self::FailedTest => "failed_test",
            Self::Regression => "regression",
            Self::InsufficientEvidence => "insufficient_evidence",
            Self::RequirementMismatch => "requirement_mismatch",
            Self::SecurityFailure => "security_failure",
            Self::PolicyFailure => "policy_failure",
            Self::BuildFailure => "build_failure",
            Self::TypeFailure => "type_failure",
            Self::RuntimeFailure => "runtime_failure",
            Self::Unknown => "unknown",
        }
    }
}

/// Stable gap id for the same issue across repair cycles.
pub fn stable_gap_id(
    task_id: GoalId,
    requirement_id: Option<&str>,
    category: GapCategory,
    description: &str,
) -> String {
    let mut desc = description.trim().to_ascii_lowercase();
    desc.retain(|c| c.is_ascii_alphanumeric() || c.is_ascii_whitespace());
    let fingerprint = StagnationDetector::gap_fingerprint(&{
        let mut set = std::collections::BTreeSet::new();
        set.insert(format!(
            "{task_id}|{}|{}|{desc}",
            requirement_id.unwrap_or("-"),
            category.as_str()
        ));
        set
    });
    format!("GAP-{fingerprint:016x}")
}

impl GapNode {
    pub fn open(
        task_id: GoalId,
        requirement_id: Option<String>,
        acceptance_criterion_id: Option<String>,
        category: GapCategory,
        severity: GapSeverity,
        description: String,
        round: u32,
    ) -> Self {
        let id = stable_gap_id(task_id, requirement_id.as_deref(), category, &description);
        Self {
            id,
            task_id,
            requirement_id,
            acceptance_criterion_id,
            category,
            severity,
            description,
            evidence_refs: Vec::new(),
            suggested_validation: None,
            first_seen_round: round,
            last_seen_round: round,
            status: GapStatus::Open,
            resolution_evidence: Vec::new(),
        }
    }
}
