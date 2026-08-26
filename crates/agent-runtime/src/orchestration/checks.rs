//! Deterministic verification checks with bounded summaries.

use std::time::Duration;

use protocol::EvidenceId;
use serde::{Deserialize, Serialize};

/// Maximum UTF-8 bytes of check output retained in the model-visible summary.
pub const MAX_CHECK_SUMMARY_BYTES: usize = 2 * 1024;

/// Kind of deterministic check. Unknown kinds fail at serde.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    UnitTest,
    IntegrationTest,
    TargetedTest,
    Build,
    Compiler,
    Typecheck,
    Lint,
    Formatting,
    StaticAnalysis,
    LspDiagnostics,
    WorkspaceDirty,
    Policy,
    ForbiddenFile,
    Dependency,
    Security,
}

/// Check specification. Command output is stored out-of-band.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationCheck {
    pub id: String,
    pub kind: CheckKind,
    pub action: String,
    pub timeout_ms: u64,
    pub required: bool,
    pub applicable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_result: Option<String>,
}

/// Structured result. Full stdout/stderr live as artifact refs, not in context.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckResult {
    pub check_id: String,
    pub status: CheckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_ref: Option<String>,
    pub evidence_id: EvidenceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_summary: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    Timeout,
    Skipped,
    Unavailable,
}

/// Injected runner. Production uses process-supervisor; tests use fakes.
pub trait CheckRunner {
    fn run(&self, check: &VerificationCheck) -> CheckResult;
}

impl CheckStatus {
    pub const fn is_passing(self) -> bool {
        matches!(self, Self::Passed | Self::Skipped)
    }
}

impl CheckResult {
    pub fn bounded_summary(&self) -> String {
        let raw = self.failure_summary.as_deref().unwrap_or("");
        if raw.len() <= MAX_CHECK_SUMMARY_BYTES {
            raw.to_owned()
        } else {
            raw[..MAX_CHECK_SUMMARY_BYTES].to_owned()
        }
    }
}

impl VerificationCheck {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}
