//! The CLI error envelope (SEAM-10): what `rapid` writes on stderr when a
//! command fails in JSON mode (`--output json` / `RAPIDLM_OUTPUT=json`) —
//! `{"error": {"code", "message", "hint"}}` — so an agent driving the CLI
//! reads a stable code and the next command to try instead of parsing
//! prose. The exit code is unchanged; the envelope only describes it.

use serde::{Deserialize, Serialize};

/// One CLI failure.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliError {
    /// Stable, snake_case: what kind of failure (`usage`, `unknown_session`,
    /// `config`, …). Never a sentence.
    pub code: String,
    /// What went wrong, for a person.
    pub message: String,
    /// The next command to try, when there is one.
    pub hint: Option<String>,
}

/// The document a failure is written as: `{"error": {…}}`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliErrorEnvelope {
    pub error: CliError,
}

impl CliError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            hint,
        }
    }

    /// The envelope as one line of JSON.
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(&CliErrorEnvelope {
            error: self.clone(),
        })
        .unwrap_or_else(|_| {
            // Serializing three strings cannot fail; keep a well-formed
            // document even so.
            r#"{"error":{"code":"internal","message":"the error could not be encoded","hint":null}}"#
                .to_owned()
        })
    }
}
