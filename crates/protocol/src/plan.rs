//! A plan proposal (ADR 0024 §2): what a turn in plan mode submits for
//! approval — steps in the workflow shape, the files it expects to change,
//! how it will be verified, its risks and open questions, and the workspace
//! it was planned against. Closed schema: unknown fields are refused.

use serde::{Deserialize, Serialize};

/// Wire identity of a plan proposal.
pub const PLAN_PROPOSAL_SCHEMA: &str = "rapidlm.plan_proposal";
/// The version this crate reads and writes.
pub const PLAN_PROPOSAL_VERSION: u32 = 1;
/// Most steps one proposal holds.
pub const MAX_PLAN_STEPS: usize = 64;
/// Most entries in each list field.
pub const MAX_PLAN_LIST_ENTRIES: usize = 64;
/// Most bytes of any one text field.
pub const MAX_PLAN_TEXT_BYTES: usize = 8 * 1024;

/// A plan proposal, version 1.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanProposal {
    pub schema: String,
    pub version: u32,
    pub title: String,
    pub summary: String,
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub files_expected_to_change: Vec<String>,
    #[serde(default)]
    pub verification: Vec<String>,
    #[serde(default)]
    pub risks: Vec<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    /// The workspace digest the plan was made against.
    pub base_revision: String,
}

/// One step, in the workflow's shape: a key, what kind of node it is, what
/// it waits on, and its payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub key: String,
    pub kind: PlanStepKind,
    pub label: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// An agent step's task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// A process or verification step's command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// A human step's question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// What a process step watches for, when it is a watch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<String>,
}

/// What a step is.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanStepKind {
    Agent,
    Process,
    Verification,
    Human,
}

/// Why a proposal is refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanProposalError {
    Schema,
    Version(u32),
    Empty(&'static str),
    TooMany(&'static str),
    TooLong(&'static str),
    DuplicateKey(String),
    UnknownDependency { step: String, depends_on: String },
    MissingPayload(String),
}

impl std::fmt::Display for PlanProposalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Schema => write!(f, "not a {PLAN_PROPOSAL_SCHEMA} document"),
            Self::Version(v) => write!(f, "unsupported plan proposal version {v}"),
            Self::Empty(field) => write!(f, "`{field}` is empty"),
            Self::TooMany(field) => write!(f, "`{field}` has too many entries"),
            Self::TooLong(field) => write!(f, "`{field}` is too long"),
            Self::DuplicateKey(key) => write!(f, "step key `{key}` is used twice"),
            Self::UnknownDependency { step, depends_on } => {
                write!(f, "step `{step}` depends on unknown step `{depends_on}`")
            }
            Self::MissingPayload(step) => write!(
                f,
                "step `{step}` lacks its payload (an agent's prompt, a process's or \
verification's command, a human's question)"
            ),
        }
    }
}

impl std::error::Error for PlanProposalError {}

impl PlanProposal {
    /// Check the schema marker, the bounds, unique keys, known
    /// dependencies, and each step's payload for its kind.
    pub fn validate(&self) -> Result<(), PlanProposalError> {
        if self.schema != PLAN_PROPOSAL_SCHEMA {
            return Err(PlanProposalError::Schema);
        }
        if self.version != PLAN_PROPOSAL_VERSION {
            return Err(PlanProposalError::Version(self.version));
        }
        let text = |field: &'static str, value: &str| {
            if value.len() > MAX_PLAN_TEXT_BYTES {
                Err(PlanProposalError::TooLong(field))
            } else {
                Ok(())
            }
        };
        if self.title.trim().is_empty() {
            return Err(PlanProposalError::Empty("title"));
        }
        text("title", &self.title)?;
        text("summary", &self.summary)?;
        text("base_revision", &self.base_revision)?;
        if self.steps.is_empty() {
            return Err(PlanProposalError::Empty("steps"));
        }
        if self.steps.len() > MAX_PLAN_STEPS {
            return Err(PlanProposalError::TooMany("steps"));
        }
        for (field, list) in [
            ("files_expected_to_change", &self.files_expected_to_change),
            ("verification", &self.verification),
            ("risks", &self.risks),
            ("open_questions", &self.open_questions),
        ] {
            if list.len() > MAX_PLAN_LIST_ENTRIES {
                return Err(PlanProposalError::TooMany(field));
            }
            for entry in list {
                text(field, entry)?;
            }
        }
        let mut keys: Vec<&str> = Vec::new();
        for step in &self.steps {
            if step.key.trim().is_empty() {
                return Err(PlanProposalError::Empty("steps[].key"));
            }
            if keys.contains(&step.key.as_str()) {
                return Err(PlanProposalError::DuplicateKey(step.key.clone()));
            }
            keys.push(&step.key);
        }
        for step in &self.steps {
            text("steps[].label", &step.label)?;
            for field in [&step.prompt, &step.command, &step.question, &step.watch]
                .into_iter()
                .flatten()
            {
                text("steps[]", field)?;
            }
            for dependency in &step.depends_on {
                if !keys.contains(&dependency.as_str()) {
                    return Err(PlanProposalError::UnknownDependency {
                        step: step.key.clone(),
                        depends_on: dependency.clone(),
                    });
                }
            }
            let payload = match step.kind {
                PlanStepKind::Agent => step.prompt.is_some(),
                PlanStepKind::Process | PlanStepKind::Verification => step.command.is_some(),
                PlanStepKind::Human => step.question.is_some(),
            };
            if !payload {
                return Err(PlanProposalError::MissingPayload(step.key.clone()));
            }
        }
        Ok(())
    }
}
