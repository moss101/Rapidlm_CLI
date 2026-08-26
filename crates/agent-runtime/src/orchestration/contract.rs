//! First-class task contract with stable requirement and AC identifiers.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use protocol::GoalId;
use serde::{Deserialize, Serialize};

use crate::orchestration::policy::{OrchestrationBudget, TaskComplexity, VerificationPolicy};

/// Maximum UTF-8 bytes accepted in the objective.
pub const MAX_OBJECTIVE_BYTES: usize = 16 * 1024;

/// Maximum requirement nodes on one contract.
pub const MAX_REQUIREMENTS: usize = 64;

/// Maximum acceptance criteria on one contract or requirement.
pub const MAX_CRITERIA: usize = 64;

/// Maximum UTF-8 bytes in a stable requirement or AC identifier.
pub const MAX_ID_BYTES: usize = 64;

/// Maximum UTF-8 bytes in a description field.
pub const MAX_TEXT_BYTES: usize = 4 * 1024;

/// Maximum entries in a string list (scope, constraints, capabilities).
pub const MAX_LIST: usize = 32;

/// Host-owned statement of work. Models cannot mutate this after start.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskContract {
    pub id: GoalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<GoalId>,
    pub objective: String,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    pub requirements: Vec<RequirementNode>,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    #[serde(default)]
    pub permitted_capabilities: Vec<String>,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    #[serde(default)]
    pub forbidden_actions: Vec<String>,
    #[serde(default)]
    pub expected_outputs: Vec<String>,
    pub verification_policy: VerificationPolicy,
    pub resource_budget: OrchestrationBudget,
    #[serde(default)]
    pub context_budget: u32,
    #[serde(default)]
    pub workspace_policy: WorkspacePolicy,
    #[serde(default)]
    pub complexity: TaskComplexity,
    #[serde(default)]
    pub metadata: BTreeSet<String>,
}

/// Independent requirement node. Identifiers are stable across repair rounds.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementNode {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub priority: RequirementPriority,
    #[serde(default = "default_true")]
    pub mandatory: bool,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
}

/// Acceptance criterion with a stable identifier (e.g. `AC-001`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub text: String,
}

/// Workspace mutation class allowed for this task.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePolicy {
    #[default]
    Isolated,
    Direct,
    ReadOnly,
}

/// Requirement urgency. Unknown wire values fail at serde deny.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementPriority {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

/// Contract validation failure. Display never echoes objective text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TaskContractError {
    EmptyObjective,
    ObjectiveTooLong,
    NoRequirements,
    TooManyRequirements,
    DuplicateRequirement,
    DuplicateCriterion,
    InvalidRequirementId,
    InvalidCriterionId,
    EmptyDescription,
    DanglingCriterion,
    DanglingDependency,
    ListTooLong,
    IdTooLong,
}

fn default_true() -> bool {
    true
}

impl TaskContract {
    /// Fail closed on empty/duplicate/dangling identifiers.
    pub fn validate(&self) -> Result<(), TaskContractError> {
        if self.objective.is_empty() {
            return Err(TaskContractError::EmptyObjective);
        }
        if self.objective.len() > MAX_OBJECTIVE_BYTES {
            return Err(TaskContractError::ObjectiveTooLong);
        }
        if self.requirements.is_empty() {
            return Err(TaskContractError::NoRequirements);
        }
        if self.requirements.len() > MAX_REQUIREMENTS {
            return Err(TaskContractError::TooManyRequirements);
        }
        check_list(&self.scope)?;
        check_list(&self.constraints)?;
        check_list(&self.permitted_capabilities)?;
        check_list(&self.required_capabilities)?;
        check_list(&self.forbidden_actions)?;
        check_list(&self.expected_outputs)?;
        if self.acceptance_criteria.len() > MAX_CRITERIA {
            return Err(TaskContractError::TooManyRequirements);
        }

        let mut req_ids = BTreeSet::new();
        for req in &self.requirements {
            validate_id(&req.id, true)?;
            if req.description.is_empty() {
                return Err(TaskContractError::EmptyDescription);
            }
            if req.description.len() > MAX_TEXT_BYTES {
                return Err(TaskContractError::EmptyDescription);
            }
            if !req_ids.insert(req.id.clone()) {
                return Err(TaskContractError::DuplicateRequirement);
            }
        }
        for req in &self.requirements {
            for dep in &req.depends_on {
                if !req_ids.contains(dep) {
                    return Err(TaskContractError::DanglingDependency);
                }
            }
        }

        let mut ac_ids = BTreeSet::new();
        for ac in &self.acceptance_criteria {
            validate_id(&ac.id, false)?;
            if ac.text.is_empty() {
                return Err(TaskContractError::EmptyDescription);
            }
            if !ac_ids.insert(ac.id.clone()) {
                return Err(TaskContractError::DuplicateCriterion);
            }
        }
        for req in &self.requirements {
            for ac in &req.acceptance_criteria {
                if !ac_ids.contains(ac) {
                    return Err(TaskContractError::DanglingCriterion);
                }
            }
        }
        Ok(())
    }

    pub fn mandatory_requirement_ids(&self) -> Vec<&str> {
        self.requirements
            .iter()
            .filter(|r| r.mandatory)
            .map(|r| r.id.as_str())
            .collect()
    }
}

fn validate_id(id: &str, requirement: bool) -> Result<(), TaskContractError> {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err(if requirement {
            TaskContractError::InvalidRequirementId
        } else {
            TaskContractError::InvalidCriterionId
        });
    }
    let prefix_ok = if requirement {
        id.starts_with("REQ-")
    } else {
        id.starts_with("AC-")
    };
    let rest_ok = id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !prefix_ok || !rest_ok {
        return Err(if requirement {
            TaskContractError::InvalidRequirementId
        } else {
            TaskContractError::InvalidCriterionId
        });
    }
    Ok(())
}

fn check_list(list: &[String]) -> Result<(), TaskContractError> {
    if list.len() > MAX_LIST {
        return Err(TaskContractError::ListTooLong);
    }
    for item in list {
        if item.len() > MAX_TEXT_BYTES {
            return Err(TaskContractError::IdTooLong);
        }
    }
    Ok(())
}

impl fmt::Display for TaskContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyObjective => "task contract objective is empty",
            Self::ObjectiveTooLong => "task contract objective exceeds byte limit",
            Self::NoRequirements => "task contract has no requirements",
            Self::TooManyRequirements => "task contract exceeds requirement limit",
            Self::DuplicateRequirement => "duplicate requirement id",
            Self::DuplicateCriterion => "duplicate acceptance-criterion id",
            Self::InvalidRequirementId => "requirement id must be REQ-* alphanumeric",
            Self::InvalidCriterionId => "acceptance criterion id must be AC-* alphanumeric",
            Self::EmptyDescription => "requirement or criterion text is empty or too long",
            Self::DanglingCriterion => "requirement references unknown acceptance criterion",
            Self::DanglingDependency => "requirement depends on unknown requirement",
            Self::ListTooLong => "contract list exceeds bound",
            Self::IdTooLong => "contract list entry exceeds byte limit",
        })
    }
}

impl Error for TaskContractError {}
