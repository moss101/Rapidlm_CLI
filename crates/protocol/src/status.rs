//! The status-line payload (SEAM-07 AC-03): what a `[ui.status_line]`
//! command receives as JSON on stdin — the live session facts it may render.
//! Closed schema: unknown fields are refused. A fact the host does not have
//! is `null`, never a guess.

use serde::{Deserialize, Serialize};

/// Wire identity of a status payload.
pub const STATUS_PAYLOAD_SCHEMA: &str = "rapidlm.status_payload";
/// The version this crate reads and writes.
pub const STATUS_PAYLOAD_VERSION: u32 = 1;

/// A status payload, version 1.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusPayload {
    pub schema: String,
    pub version: u32,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context: StatusContext,
    pub cost: StatusCost,
    pub goal: Option<StatusGoal>,
    /// The worktree the session works in, when it is not the checkout.
    pub worktree: Option<String>,
    pub workspace: StatusWorkspace,
    /// Why the command ran now.
    pub trigger: StatusTrigger,
}

/// The model's window, as the last compiled context reported it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusContext {
    pub used_tokens: Option<u64>,
    pub limit_tokens: Option<u64>,
    /// `used_tokens` as a whole percentage of `limit_tokens`.
    pub used_percent: Option<u8>,
}

/// What the session has cost, as its ledger records say.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusCost {
    /// Provider-reported USD micros, when every step reported one.
    pub usd_micros: Option<u64>,
    /// `reported`, `estimated`, `unknown` or `none` (no steps yet).
    pub basis: String,
}

/// The project's active goal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusGoal {
    pub id: String,
    pub state: String,
}

/// Where the session runs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusWorkspace {
    pub cwd: String,
    /// The repository root, when the workspace is in one.
    pub repo: Option<String>,
}

/// Why a status command ran.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusTrigger {
    /// Something the payload carries changed.
    State,
    /// `refresh_interval` elapsed.
    RefreshInterval,
}

/// Why a payload is refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusPayloadError {
    Schema,
    Version(u32),
    Percent(u8),
    Basis(String),
}

impl std::fmt::Display for StatusPayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Schema => write!(f, "not a {STATUS_PAYLOAD_SCHEMA} document"),
            Self::Version(version) => write!(
                f,
                "status payload version {version}; this reads {STATUS_PAYLOAD_VERSION}"
            ),
            Self::Percent(percent) => write!(f, "context.used_percent {percent} is over 100"),
            Self::Basis(basis) => write!(
                f,
                "cost.basis '{basis}' is not reported, estimated, unknown or none"
            ),
        }
    }
}

impl std::error::Error for StatusPayloadError {}

impl StatusPayload {
    /// The checks the schema alone cannot make.
    pub fn validate(&self) -> Result<(), StatusPayloadError> {
        if self.schema != STATUS_PAYLOAD_SCHEMA {
            return Err(StatusPayloadError::Schema);
        }
        if self.version != STATUS_PAYLOAD_VERSION {
            return Err(StatusPayloadError::Version(self.version));
        }
        if let Some(percent) = self.context.used_percent
            && percent > 100
        {
            return Err(StatusPayloadError::Percent(percent));
        }
        if !matches!(
            self.cost.basis.as_str(),
            "reported" | "estimated" | "unknown" | "none"
        ) {
            return Err(StatusPayloadError::Basis(self.cost.basis.clone()));
        }
        Ok(())
    }
}
