//! Hook result v2 — the versioned JSON a hook may print on stdout.
//!
//! A hook that prints nothing structured keeps the v1 contract (its exit
//! code decides; its stderr is the detail). A hook that prints a JSON object
//! carrying either `"schema": "rapidlm.hook_result"` or a `decision` key is
//! read as a [`HookResult`]: `decision ∈ {allow, deny, ask, defer}` plus an
//! optional `reason`, `updated_input`, `additional_context` and
//! `hook_specific`. Unknown fields are ignored so the shape can grow
//! additively; an unknown `decision`, a wrong `schema` or a newer `version`
//! is a typed error the caller treats as a denial — a hook that speaks a
//! contract this binary does not know cannot be assumed to have allowed.
//!
//! This is the one parser for the contract (ADR 0022 §1): the exec loop's
//! project hooks and the plugin-host engine read the same shape. The
//! engine's historical `"block"` token is accepted as a synonym of `deny`.
//!
//! Hooks request; they never grant (invariant 13). A result carrying a
//! grant-shaped key is still a valid result, but [`HookResult::grant_attempted`]
//! is set so the caller can record the attempt and ignore the keys.

use std::error::Error;
use std::fmt;

use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Wire schema name.
pub const HOOK_RESULT_SCHEMA: &str = "rapidlm.hook_result";

/// Wire schema version this binary reads and writes.
pub const HOOK_RESULT_VERSION: u32 = 2;

/// Bound on `reason` (matches the bounded hook stderr the v1 contract
/// already delivers as detail).
pub const MAX_HOOK_REASON_BYTES: usize = 2048;

/// Bound on `additional_context` — the workspace-read ceiling, the smallest
/// per-tool output bound the model already sees whole.
pub const MAX_HOOK_CONTEXT_BYTES: usize = 4096;

/// Bound on a serialized `updated_input` object (the tool-argument bound the
/// exec loop enforces on the model's own arguments).
pub const MAX_HOOK_UPDATED_INPUT_BYTES: usize = 8 * 1024;

/// Bound on the whole stdout a parser will consider.
pub const MAX_HOOK_RESULT_BYTES: usize = 64 * 1024;

/// Keys a hook might use to try to widen its own authority. Their presence
/// is recorded, their content ignored.
const GRANT_KEYS: &[&str] = &[
    "grant",
    "grants",
    "capability",
    "capabilities",
    "permission",
    "permissions",
    "policy",
    "allow",
    "requested_caps",
    "lease",
];

/// What the hook decided about the call it was shown.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookDecision {
    /// Proceed (the normal permission flow still applies).
    Allow,
    /// Refuse the call; `reason` is what the model sees.
    Deny,
    /// A human decides; the turn waits on the existing approval surface.
    Ask,
    /// No opinion: the normal permission flow decides.
    Defer,
}

impl HookDecision {
    pub const ALL: [Self; 4] = [Self::Allow, Self::Deny, Self::Ask, Self::Defer];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
            Self::Defer => "defer",
        }
    }

    /// Wire token → decision. `block` is the plugin-host engine's v1 word
    /// for `deny` and stays readable.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "allow" => Some(Self::Allow),
            "deny" | "block" => Some(Self::Deny),
            "ask" => Some(Self::Ask),
            "defer" => Some(Self::Defer),
            _ => None,
        }
    }
}

impl fmt::Display for HookDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A parsed v2 hook result. Bounded on construction: `reason` and
/// `additional_context` are cut at their ceilings (a hook that talks too much
/// is still heard), `updated_input` over its bound is an error (a rewrite
/// cannot be half-applied).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookResult {
    pub decision: HookDecision,
    pub reason: Option<String>,
    pub updated_input: Option<Map<String, Value>>,
    pub additional_context: Option<String>,
    pub hook_specific: Option<Map<String, Value>>,
    /// The object carried a grant-shaped key. Recorded, never honoured.
    pub grant_attempted: bool,
}

/// Why stdout that claimed to be a hook result could not be read as one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookResultError {
    /// More bytes than [`MAX_HOOK_RESULT_BYTES`].
    TooLarge { bytes: usize },
    /// `schema` present and not [`HOOK_RESULT_SCHEMA`].
    UnknownSchema(String),
    /// `version` present and not [`HOOK_RESULT_VERSION`].
    UnsupportedVersion(u64),
    /// No `decision`, or one this binary does not know.
    UnknownDecision(Option<String>),
    /// A field had the wrong JSON type.
    WrongType { field: &'static str },
    /// `updated_input` serializes past [`MAX_HOOK_UPDATED_INPUT_BYTES`].
    UpdatedInputTooLarge { bytes: usize },
    /// stdout began like a result (`{`) and was not exactly one JSON object:
    /// invalid JSON, invalid UTF-8, or text after the object. A hook that
    /// meant to decide and could not be understood is not allowed to have
    /// allowed.
    Malformed,
}

impl fmt::Display for HookResultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes } => write!(
                f,
                "hook result is {bytes} bytes; the ceiling is {MAX_HOOK_RESULT_BYTES}"
            ),
            Self::UnknownSchema(schema) => {
                write!(
                    f,
                    "hook result schema {schema:?} is not {HOOK_RESULT_SCHEMA:?}"
                )
            }
            Self::UnsupportedVersion(version) => write!(
                f,
                "hook result version {version} is not supported (this build reads {HOOK_RESULT_VERSION})"
            ),
            Self::UnknownDecision(None) => f.write_str("hook result has no decision"),
            Self::UnknownDecision(Some(word)) => write!(
                f,
                "hook result decision {word:?} is not one of allow, deny, ask, defer"
            ),
            Self::WrongType { field } => {
                write!(f, "hook result field {field:?} has the wrong type")
            }
            Self::UpdatedInputTooLarge { bytes } => write!(
                f,
                "hook result updated_input is {bytes} bytes; the ceiling is {MAX_HOOK_UPDATED_INPUT_BYTES}"
            ),
            Self::Malformed => {
                f.write_str("hook stdout begins like a result but is not exactly one JSON object")
            }
        }
    }
}

impl Error for HookResultError {}

impl HookResult {
    /// A result with just a decision.
    pub fn new(decision: HookDecision) -> Self {
        Self {
            decision,
            reason: None,
            updated_input: None,
            additional_context: None,
            hook_specific: None,
            grant_attempted: false,
        }
    }

    /// Read a hook's stdout.
    ///
    /// `Ok(None)` means "no structured result — v1 semantics": empty output,
    /// or text whose first non-blank byte is not `{` (whatever its size —
    /// a chatty v1 hook is still a v1 hook), or a JSON object that is some
    /// other program's (neither a `decision` key nor
    /// `"schema": "rapidlm.hook_result"`).
    ///
    /// Once stdout begins with `{` it is read as a result and must be
    /// exactly one JSON object within [`MAX_HOOK_RESULT_BYTES`]: invalid
    /// JSON, invalid UTF-8, trailing text or a second value is
    /// [`HookResultError::Malformed`]; an unknown decision, schema or
    /// version is its own error. `Err` is what the caller denies on — a
    /// hook that meant to decide and could not be understood cannot be
    /// assumed to have allowed (ADR 0022 §2).
    pub fn from_stdout(stdout: &[u8]) -> Result<Option<Self>, HookResultError> {
        let first = stdout
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .map(|index| stdout[index]);
        if first != Some(b'{') {
            return Ok(None);
        }
        if stdout.len() > MAX_HOOK_RESULT_BYTES {
            return Err(HookResultError::TooLarge {
                bytes: stdout.len(),
            });
        }
        let text = std::str::from_utf8(stdout)
            .map_err(|_| HookResultError::Malformed)?
            .trim();
        let value: Value = serde_json::from_str(text).map_err(|_| HookResultError::Malformed)?;
        let Value::Object(object) = value else {
            return Err(HookResultError::Malformed);
        };
        if !Self::is_marked(&object) {
            return Ok(None);
        }
        Self::from_object(&object).map(Some)
    }

    /// Whether an object declares itself a hook result: a `decision` key, or
    /// `schema` equal to [`HOOK_RESULT_SCHEMA`]. Some other program's JSON
    /// with an unrelated `schema` key is not a result.
    fn is_marked(object: &Map<String, Value>) -> bool {
        object.contains_key("decision")
            || object.get("schema").and_then(Value::as_str) == Some(HOOK_RESULT_SCHEMA)
    }

    /// Read an object that is known to be a hook result.
    pub fn from_object(object: &Map<String, Value>) -> Result<Self, HookResultError> {
        if let Some(schema) = object.get("schema") {
            match schema.as_str() {
                Some(HOOK_RESULT_SCHEMA) => {}
                Some(other) => return Err(HookResultError::UnknownSchema(other.to_owned())),
                None => return Err(HookResultError::WrongType { field: "schema" }),
            }
        }
        if let Some(version) = object.get("version") {
            match version.as_u64() {
                Some(v) if v == u64::from(HOOK_RESULT_VERSION) => {}
                Some(v) => return Err(HookResultError::UnsupportedVersion(v)),
                None => return Err(HookResultError::WrongType { field: "version" }),
            }
        }
        let decision = match object.get("decision") {
            None | Some(Value::Null) => return Err(HookResultError::UnknownDecision(None)),
            Some(Value::String(word)) => HookDecision::parse(word)
                .ok_or_else(|| HookResultError::UnknownDecision(Some(word.clone())))?,
            Some(_) => return Err(HookResultError::WrongType { field: "decision" }),
        };
        let reason = optional_string(object, "reason", MAX_HOOK_REASON_BYTES)?;
        let additional_context =
            optional_string(object, "additional_context", MAX_HOOK_CONTEXT_BYTES)?;
        let updated_input = match object.get("updated_input") {
            None | Some(Value::Null) => None,
            Some(Value::Object(map)) => {
                let bytes = serde_json::to_vec(map)
                    .map(|v| v.len())
                    .unwrap_or(usize::MAX);
                if bytes > MAX_HOOK_UPDATED_INPUT_BYTES {
                    return Err(HookResultError::UpdatedInputTooLarge { bytes });
                }
                Some(map.clone())
            }
            Some(_) => {
                return Err(HookResultError::WrongType {
                    field: "updated_input",
                });
            }
        };
        let hook_specific = match object.get("hook_specific") {
            None | Some(Value::Null) => None,
            Some(Value::Object(map)) => Some(map.clone()),
            Some(_) => {
                return Err(HookResultError::WrongType {
                    field: "hook_specific",
                });
            }
        };
        let grant_attempted = object.keys().any(|key| is_grant_key(key));
        Ok(Self {
            decision,
            reason,
            updated_input,
            additional_context,
            hook_specific,
            grant_attempted,
        })
    }
}

fn optional_string(
    object: &Map<String, Value>,
    field: &'static str,
    cap: usize,
) -> Result<Option<String>, HookResultError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(truncate_utf8(text, cap))),
        Some(_) => Err(HookResultError::WrongType { field }),
    }
}

/// Cut `text` at `cap` bytes on a character boundary.
fn truncate_utf8(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn is_grant_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    GRANT_KEYS.iter().any(|item| lowered == *item)
}

impl Serialize for HookResult {
    /// The wire form carries `schema` and `version` so a reader can tell a
    /// v2 result from arbitrary JSON; `grant_attempted` is a reader-side
    /// observation and is not a wire field.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = 3;
        if self.reason.is_some() {
            fields += 1;
        }
        if self.updated_input.is_some() {
            fields += 1;
        }
        if self.additional_context.is_some() {
            fields += 1;
        }
        if self.hook_specific.is_some() {
            fields += 1;
        }
        let mut state = serializer.serialize_struct("HookResult", fields)?;
        state.serialize_field("schema", HOOK_RESULT_SCHEMA)?;
        state.serialize_field("version", &HOOK_RESULT_VERSION)?;
        state.serialize_field("decision", &self.decision)?;
        if let Some(reason) = &self.reason {
            state.serialize_field("reason", reason)?;
        }
        if let Some(input) = &self.updated_input {
            state.serialize_field("updated_input", input)?;
        }
        if let Some(context) = &self.additional_context {
            state.serialize_field("additional_context", context)?;
        }
        if let Some(specific) = &self.hook_specific {
            state.serialize_field("hook_specific", specific)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for HookResult {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let object = Map::<String, Value>::deserialize(deserializer)?;
        Self::from_object(&object).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_and_empty_stdout_are_v1() {
        assert_eq!(HookResult::from_stdout(b""), Ok(None));
        assert_eq!(HookResult::from_stdout(b"   \n"), Ok(None));
        assert_eq!(HookResult::from_stdout(b"checked ok"), Ok(None));
        assert_eq!(HookResult::from_stdout(b"[1,2]"), Ok(None));
        // Non-JSON text is v1 whatever it contains and however long it is:
        // a chatty hook that prints 70 KiB of lint output is still a v1 hook.
        assert_eq!(HookResult::from_stdout(&[0xff, b'x']), Ok(None));
        let chatty = vec![b'x'; MAX_HOOK_RESULT_BYTES + 1];
        assert_eq!(HookResult::from_stdout(&chatty), Ok(None));
        // An object with neither marker is some other program's JSON — a
        // `schema` key alone (another tool's `--json`) is not a marker.
        assert_eq!(HookResult::from_stdout(br#"{"ok":true}"#), Ok(None));
        assert_eq!(
            HookResult::from_stdout(br#"{"schema":"other.tool/v1","ok":true}"#),
            Ok(None)
        );
    }

    #[test]
    fn stdout_that_begins_like_a_result_and_is_not_one_object_is_malformed() {
        // A deny whose reason broke the JSON must not become an allow.
        assert_eq!(
            HookResult::from_stdout(br#"{"decision":"deny","reason":"a "quoted" word"}"#),
            Err(HookResultError::Malformed)
        );
        assert_eq!(
            HookResult::from_stdout(b"{not json"),
            Err(HookResultError::Malformed)
        );
        // A result followed by a diagnostic line, or by a second result.
        assert_eq!(
            HookResult::from_stdout(b"{\"decision\":\"deny\"}\nchecked 3 files"),
            Err(HookResultError::Malformed)
        );
        assert_eq!(
            HookResult::from_stdout(b"{\"decision\":\"allow\"}\n{\"decision\":\"deny\"}"),
            Err(HookResultError::Malformed)
        );
        // Invalid UTF-8 inside what began as a result.
        assert_eq!(
            HookResult::from_stdout(b"{\"decision\":\"deny\",\"reason\":\"\xff\"}"),
            Err(HookResultError::Malformed)
        );
        // Leading whitespace before the brace is still a result.
        let spaced = HookResult::from_stdout(b"  \n {\"decision\":\"defer\"}\n")
            .expect("parses")
            .expect("result");
        assert_eq!(spaced.decision, HookDecision::Defer);
        // The size ceiling applies to results, not to v1 text.
        let mut huge = br#"{"decision":"allow","pad":""#.to_vec();
        huge.extend(std::iter::repeat_n(b'p', MAX_HOOK_RESULT_BYTES));
        huge.extend_from_slice(b"\"}");
        assert!(matches!(
            HookResult::from_stdout(&huge),
            Err(HookResultError::TooLarge { .. })
        ));
    }

    #[test]
    fn the_four_decisions_and_the_legacy_block_token_parse() {
        for (word, expected) in [
            ("allow", HookDecision::Allow),
            ("deny", HookDecision::Deny),
            ("block", HookDecision::Deny),
            ("ask", HookDecision::Ask),
            ("defer", HookDecision::Defer),
        ] {
            let text = format!(r#"{{"decision":"{word}"}}"#);
            let result = HookResult::from_stdout(text.as_bytes())
                .expect("parses")
                .expect("is a result");
            assert_eq!(result.decision, expected, "{word}");
            assert!(!result.grant_attempted);
        }
        assert_eq!(
            HookResult::from_stdout(br#"{"decision":"maybe"}"#),
            Err(HookResultError::UnknownDecision(Some("maybe".to_owned())))
        );
        assert_eq!(
            HookResult::from_stdout(br#"{"schema":"rapidlm.hook_result"}"#),
            Err(HookResultError::UnknownDecision(None))
        );
        assert_eq!(
            HookResult::from_stdout(br#"{"decision":1}"#),
            Err(HookResultError::WrongType { field: "decision" })
        );
    }

    #[test]
    fn a_declared_schema_and_version_must_be_the_known_ones() {
        assert_eq!(
            HookResult::from_stdout(br#"{"schema":"rapidlm.other","decision":"allow"}"#),
            Err(HookResultError::UnknownSchema("rapidlm.other".to_owned()))
        );
        assert_eq!(
            HookResult::from_stdout(
                br#"{"schema":"rapidlm.hook_result","version":3,"decision":"allow"}"#
            ),
            Err(HookResultError::UnsupportedVersion(3))
        );
        let ok = HookResult::from_stdout(
            br#"{"schema":"rapidlm.hook_result","version":2,"decision":"allow","future_field":[1]}"#,
        )
        .expect("parses")
        .expect("result");
        assert_eq!(ok, HookResult::new(HookDecision::Allow));
    }

    #[test]
    fn optional_fields_are_typed_and_bounded() {
        let long = "r".repeat(MAX_HOOK_REASON_BYTES + 10);
        let ctx = "c".repeat(MAX_HOOK_CONTEXT_BYTES + 10);
        let text = serde_json::json!({
            "decision": "deny",
            "reason": long,
            "additional_context": ctx,
            "updated_input": {"path": "a.txt"},
            "hook_specific": {"x": 1}
        })
        .to_string();
        let result = HookResult::from_stdout(text.as_bytes())
            .expect("parses")
            .expect("result");
        assert_eq!(
            result.reason.as_deref().map(str::len),
            Some(MAX_HOOK_REASON_BYTES)
        );
        assert_eq!(
            result.additional_context.as_deref().map(str::len),
            Some(MAX_HOOK_CONTEXT_BYTES)
        );
        assert_eq!(
            result.updated_input.as_ref().and_then(|m| m.get("path")),
            Some(&Value::String("a.txt".to_owned()))
        );
        assert!(result.hook_specific.is_some());
        assert_eq!(
            HookResult::from_stdout(br#"{"decision":"allow","reason":7}"#),
            Err(HookResultError::WrongType { field: "reason" })
        );
        assert_eq!(
            HookResult::from_stdout(br#"{"decision":"allow","updated_input":"x"}"#),
            Err(HookResultError::WrongType {
                field: "updated_input"
            })
        );
        let huge = serde_json::json!({
            "decision": "allow",
            "updated_input": {"blob": "z".repeat(MAX_HOOK_UPDATED_INPUT_BYTES)}
        })
        .to_string();
        assert!(matches!(
            HookResult::from_stdout(huge.as_bytes()),
            Err(HookResultError::UpdatedInputTooLarge { .. })
        ));
    }

    #[test]
    fn a_truncated_reason_stays_on_a_character_boundary() {
        // Multi-byte characters straddling the cap are dropped whole rather
        // than leaving a broken sequence the model would see as garbage.
        let reason = "é".repeat(MAX_HOOK_REASON_BYTES);
        let text = serde_json::json!({"decision": "deny", "reason": reason}).to_string();
        let result = HookResult::from_stdout(text.as_bytes())
            .expect("parses")
            .expect("result");
        let got = result.reason.expect("reason");
        assert!(got.len() <= MAX_HOOK_REASON_BYTES);
        assert!(got.chars().all(|c| c == 'é'));
    }

    #[test]
    fn grant_shaped_keys_are_observed_not_honoured() {
        let result = HookResult::from_stdout(
            br#"{"decision":"allow","Capabilities":["fs.write"],"lease":"x"}"#,
        )
        .expect("parses")
        .expect("result");
        assert!(result.grant_attempted);
        assert_eq!(result.decision, HookDecision::Allow);
        assert!(result.hook_specific.is_none());
    }

    #[test]
    fn serialized_form_round_trips_and_carries_schema_and_version() {
        let mut input = Map::new();
        input.insert("path".to_owned(), Value::String("b.txt".to_owned()));
        let value = HookResult {
            decision: HookDecision::Ask,
            reason: Some("needs a human".to_owned()),
            updated_input: Some(input),
            additional_context: Some("context".to_owned()),
            hook_specific: None,
            grant_attempted: false,
        };
        let json = serde_json::to_value(&value).expect("serialize");
        assert_eq!(json["schema"], HOOK_RESULT_SCHEMA);
        assert_eq!(json["version"], HOOK_RESULT_VERSION);
        assert_eq!(json["decision"], "ask");
        assert!(json.get("hook_specific").is_none());
        let back: HookResult = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, value);
    }
}
