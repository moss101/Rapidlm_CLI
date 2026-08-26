//! Verification policy, budgets, roles, verifier panel, and stagnation.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use protocol::ModelPolicyName;
use serde::{Deserialize, Serialize};

use crate::agent::model::AgentRole;

/// How expensive a verified-orchestration run is allowed to be.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskComplexity {
    Trivial,
    #[default]
    Standard,
    Substantial,
    Critical,
}

/// Host acceptance rule. Inconclusive never accepts under Strict.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptancePolicy {
    #[default]
    Strict,
    AllowInconclusive,
}

/// Multi-skeptic aggregation. Default fail-closed on any refutation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregationPolicy {
    #[default]
    AnyRefutationFails,
    Majority,
    Unanimous,
    Weighted,
    JudgeAggregation,
    RequirementPartitioned,
}

/// How many assigned verifiers must produce a verdict.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuorumPolicy {
    #[default]
    AllAssigned,
    AtLeastOne,
}

/// Orchestration role. Maps onto existing [`AgentRole`] where one exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationRole {
    Planner,
    Explorer,
    Retriever,
    Implementer,
    Verifier,
    Strategist,
}

/// Whether a role may mutate production files.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WriteClass {
    ReadOnly,
    ProductionWrite,
}

/// Caps that force Blocked rather than an infinite repair loop.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationBudget {
    pub max_verification_rounds: u32,
    pub max_repair_rounds: u32,
    pub max_strategist_calls: u32,
    pub max_agent_spawns: u32,
    pub max_tokens: u64,
    pub max_wall_clock_ms: u64,
    pub max_tool_executions: u32,
}

/// Policy object consumed by the supervisor, not by models.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationPolicy {
    pub deterministic_checks_required: bool,
    pub skeptic_count: u8,
    pub max_rounds: u32,
    pub strategist_after: u32,
    pub acceptance_policy: AcceptancePolicy,
    pub minimum_requirement_coverage: u8,
    pub require_workspace_identity: bool,
    pub allow_inconclusive_acceptance: bool,
    pub complexity: TaskComplexity,
}

/// 1..N verifier assignments. Default panel runs a single skeptic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerifierPanel {
    pub assignments: Vec<OrchestrationRole>,
    pub aggregation_policy: AggregationPolicy,
    pub quorum_policy: QuorumPolicy,
}

/// Role → existing model-policy name. Defaults to the selected policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleModelResolver {
    default: ModelPolicyName,
    overrides: BTreeMap<OrchestrationRole, ModelPolicyName>,
}

/// Signals that repair is not making progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum StagnationSignal {
    RepeatedGaps,
    OscillatingFiles,
    RepeatedFailedChecks,
    NoNewEvidence,
    TokenBurnWithoutProgress,
}

/// Reusable stagnation tracker. Not a bag of ad-hoc counters in the loop.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StagnationDetector {
    last_gap_fingerprint: Option<u64>,
    repeated_gap_rounds: u32,
    last_file_fingerprint: Option<u64>,
    oscillating_files: u32,
    last_failed_checks: Option<u64>,
    repeated_failed_checks: u32,
    last_evidence_count: usize,
    tokens_without_progress: u64,
    recent_file_fingerprints: VecDeque<u64>,
}

impl Default for OrchestrationBudget {
    fn default() -> Self {
        Self {
            max_verification_rounds: 8,
            max_repair_rounds: 6,
            max_strategist_calls: 2,
            max_agent_spawns: 16,
            max_tokens: 2_000_000,
            max_wall_clock_ms: 3_600_000,
            max_tool_executions: 256,
        }
    }
}

impl Default for VerificationPolicy {
    fn default() -> Self {
        Self::for_complexity(TaskComplexity::Substantial)
    }
}

impl VerificationPolicy {
    pub fn for_complexity(complexity: TaskComplexity) -> Self {
        match complexity {
            TaskComplexity::Trivial => Self {
                deterministic_checks_required: true,
                skeptic_count: 0,
                max_rounds: 1,
                strategist_after: 1,
                acceptance_policy: AcceptancePolicy::Strict,
                minimum_requirement_coverage: 100,
                require_workspace_identity: true,
                allow_inconclusive_acceptance: false,
                complexity,
            },
            TaskComplexity::Standard => Self {
                deterministic_checks_required: true,
                skeptic_count: 1,
                max_rounds: 4,
                strategist_after: 2,
                acceptance_policy: AcceptancePolicy::Strict,
                minimum_requirement_coverage: 100,
                require_workspace_identity: true,
                allow_inconclusive_acceptance: false,
                complexity,
            },
            TaskComplexity::Substantial => Self {
                deterministic_checks_required: true,
                skeptic_count: 1,
                max_rounds: 8,
                strategist_after: 3,
                acceptance_policy: AcceptancePolicy::Strict,
                minimum_requirement_coverage: 100,
                require_workspace_identity: true,
                allow_inconclusive_acceptance: false,
                complexity,
            },
            TaskComplexity::Critical => Self {
                deterministic_checks_required: true,
                skeptic_count: 2,
                max_rounds: 12,
                strategist_after: 2,
                acceptance_policy: AcceptancePolicy::Strict,
                minimum_requirement_coverage: 100,
                require_workspace_identity: true,
                allow_inconclusive_acceptance: false,
                complexity,
            },
        }
    }
}

impl VerifierPanel {
    pub fn single() -> Self {
        Self {
            assignments: vec![OrchestrationRole::Verifier],
            aggregation_policy: AggregationPolicy::AnyRefutationFails,
            quorum_policy: QuorumPolicy::AllAssigned,
        }
    }

    pub fn for_policy(policy: &VerificationPolicy) -> Self {
        let n = policy.skeptic_count.max(1);
        Self {
            assignments: vec![OrchestrationRole::Verifier; n as usize],
            aggregation_policy: if policy.complexity == TaskComplexity::Critical {
                AggregationPolicy::Unanimous
            } else {
                AggregationPolicy::AnyRefutationFails
            },
            quorum_policy: QuorumPolicy::AllAssigned,
        }
    }
}

impl OrchestrationRole {
    pub const ALL: &'static [Self] = &[
        Self::Planner,
        Self::Explorer,
        Self::Retriever,
        Self::Implementer,
        Self::Verifier,
        Self::Strategist,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::Explorer => "explorer",
            Self::Retriever => "retriever",
            Self::Implementer => "implementer",
            Self::Verifier => "verifier",
            Self::Strategist => "strategist",
        }
    }

    pub const fn agent_role(self) -> AgentRole {
        match self {
            Self::Planner => AgentRole::Planner,
            Self::Explorer => AgentRole::Explorer,
            Self::Retriever => AgentRole::ContextCurator,
            Self::Implementer => AgentRole::Coder,
            Self::Verifier => AgentRole::Verifier,
            Self::Strategist => AgentRole::Reviewer,
        }
    }

    pub const fn write_class(self) -> WriteClass {
        match self {
            Self::Implementer => WriteClass::ProductionWrite,
            Self::Planner
            | Self::Explorer
            | Self::Retriever
            | Self::Verifier
            | Self::Strategist => WriteClass::ReadOnly,
        }
    }
}

/// Verify ≠ repair: only implementer/repair may write production files.
pub fn profile_allows_writes(role: OrchestrationRole, _repairing: bool) -> bool {
    // Repair reuses the implementer role. Verifier/strategist never write.
    matches!(role, OrchestrationRole::Implementer)
}

impl RoleModelResolver {
    pub fn new(default: ModelPolicyName) -> Self {
        Self {
            default,
            overrides: BTreeMap::new(),
        }
    }

    pub fn assign(&mut self, role: OrchestrationRole, policy: ModelPolicyName) {
        self.overrides.insert(role, policy);
    }

    pub fn resolve(&self, role: OrchestrationRole) -> &ModelPolicyName {
        self.overrides.get(&role).unwrap_or(&self.default)
    }
}

impl StagnationDetector {
    pub fn observe_gaps(&mut self, fingerprint: u64) -> Option<StagnationSignal> {
        if self.last_gap_fingerprint == Some(fingerprint) {
            self.repeated_gap_rounds = self.repeated_gap_rounds.saturating_add(1);
            if self.repeated_gap_rounds >= 2 {
                return Some(StagnationSignal::RepeatedGaps);
            }
        } else {
            self.repeated_gap_rounds = 0;
            self.last_gap_fingerprint = Some(fingerprint);
        }
        None
    }

    pub fn observe_files(&mut self, fingerprint: u64) -> Option<StagnationSignal> {
        if self.recent_file_fingerprints.contains(&fingerprint) {
            self.oscillating_files = self.oscillating_files.saturating_add(1);
            if self.oscillating_files >= 2 {
                return Some(StagnationSignal::OscillatingFiles);
            }
        }
        if self.recent_file_fingerprints.len() >= 4 {
            self.recent_file_fingerprints.pop_front();
        }
        self.recent_file_fingerprints.push_back(fingerprint);
        self.last_file_fingerprint = Some(fingerprint);
        None
    }

    pub fn observe_failed_checks(&mut self, fingerprint: u64) -> Option<StagnationSignal> {
        if self.last_failed_checks == Some(fingerprint) {
            self.repeated_failed_checks = self.repeated_failed_checks.saturating_add(1);
            if self.repeated_failed_checks >= 2 {
                return Some(StagnationSignal::RepeatedFailedChecks);
            }
        } else {
            self.repeated_failed_checks = 0;
            self.last_failed_checks = Some(fingerprint);
        }
        None
    }

    pub fn observe_evidence(&mut self, count: usize, tokens: u64) -> Option<StagnationSignal> {
        if count <= self.last_evidence_count {
            self.tokens_without_progress = self.tokens_without_progress.saturating_add(tokens);
            if self.tokens_without_progress > 0 && count == self.last_evidence_count {
                return Some(StagnationSignal::NoNewEvidence);
            }
        } else {
            self.tokens_without_progress = 0;
            self.last_evidence_count = count;
        }
        None
    }

    pub fn gap_fingerprint(ids: &BTreeSet<String>) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for id in ids {
            for b in id.as_bytes() {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x100_0000_01b3);
            }
            h ^= 0xff;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }
}
