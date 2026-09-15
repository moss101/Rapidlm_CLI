//! Bounded, fail-closed repair of an invalid tool call.
//!
//! The original call is always validated first. Only a failed validation may
//! invoke repair, and only issue paths implicated by validator issues may
//! change. Ambiguous paths, commands, network targets, secret identifiers,
//! destructive flags, approval choices and semantic values are never repaired;
//! such a call is returned as [`RepairOutcome::Refused`] with model feedback.
//!
//! Repairs are bounded to [`MAX_REPAIR_PASSES`] and [`MAX_REPAIR_RULES`] and
//! each is recorded in a hash-bound [`RepairRecord`] for telemetry.
//!
//! Status (2026-09-15): this is a library primitive with NO production
//! caller — `rapid`'s tool dispatch surfaces handled argument errors
//! directly instead of attempting repair. Do not advertise automatic
//! tool-call repair as a delivered product behavior.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use protocol::ArtifactId;
use serde_json::{Map, Value};

use crate::schema::ToolCatalog;
use crate::validate::{
    CancellationToken, CanonicalToolInvocation, RepairIssue, TOOL_INVOCATION_SCHEMA, ToolCall,
    ValidateError, validate, validate_issues,
};

/// Maximum repair passes attempted before refusing a call.
pub const MAX_REPAIR_PASSES: usize = 3;

/// Maximum distinct repair rules applied to one call.
pub const MAX_REPAIR_RULES: usize = 64;

/// Maximum UTF-8 bytes for a repair feedback message.
pub const MAX_FEEDBACK_BYTES: usize = 1024;

/// A safe, bounded repair operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RepairRule {
    OptionalNullElision,
    StringifiedCollectionDecode,
    SingletonArrayUnwrap,
    NumericScalarCoercion,
    BooleanScalarCoercion,
    EnumAliasCoercion,
}

/// Confidence that a repair preserved the model's intent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfidenceClass {
    Exact,
    High,
    Ambiguous,
}

/// Hash-bound record of a successful repair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepairRecord {
    tool_id: String,
    schema_version: u16,
    issue_paths: Vec<String>,
    original_hash: ArtifactId,
    repair_rules: Vec<RepairRule>,
    repaired_hash: ArtifactId,
    confidence: ConfidenceClass,
}

/// Concise, secret-free feedback sent back to the model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepairFeedback {
    message: String,
}

/// Result of the validate-original-first repair entry point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepairOutcome {
    /// The original call was already valid.
    Valid(CanonicalToolInvocation),
    /// The call was repaired and revalidated successfully.
    Repaired {
        record: RepairRecord,
        invocation: CanonicalToolInvocation,
        feedback: RepairFeedback,
    },
    /// Repair was refused (semantic/ambiguous) or made no progress.
    Refused {
        issues: Vec<RepairIssue>,
        feedback: RepairFeedback,
    },
}

/// Cross-tool invariant verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvariantVerdict {
    Allow,
    Recoverable,
    Reject,
}

/// Typed repair failure. Cancellation and bound errors are not the call's fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepairError {
    Cancelled,
    NotRepairable,
}

impl RepairRule {
    pub const ALL: &'static [Self] = &[
        Self::OptionalNullElision,
        Self::StringifiedCollectionDecode,
        Self::SingletonArrayUnwrap,
        Self::NumericScalarCoercion,
        Self::BooleanScalarCoercion,
        Self::EnumAliasCoercion,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OptionalNullElision => "optional_null_elision",
            Self::StringifiedCollectionDecode => "stringified_collection_decode",
            Self::SingletonArrayUnwrap => "singleton_array_unwrap",
            Self::NumericScalarCoercion => "numeric_scalar_coercion",
            Self::BooleanScalarCoercion => "boolean_scalar_coercion",
            Self::EnumAliasCoercion => "enum_alias_coercion",
        }
    }
}

impl ConfidenceClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::High => "high",
            Self::Ambiguous => "ambiguous",
        }
    }
}

impl RepairRecord {
    pub fn tool_id(&self) -> &str {
        &self.tool_id
    }

    pub fn schema_version(&self) -> u16 {
        self.schema_version
    }

    pub fn issue_paths(&self) -> &[String] {
        &self.issue_paths
    }

    pub fn original_hash(&self) -> ArtifactId {
        self.original_hash
    }

    pub fn repair_rules(&self) -> &[RepairRule] {
        &self.repair_rules
    }

    pub fn repaired_hash(&self) -> ArtifactId {
        self.repaired_hash
    }

    pub fn confidence(&self) -> ConfidenceClass {
        self.confidence
    }
}

impl RepairFeedback {
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl RepairOutcome {
    pub fn is_repaired(&self) -> bool {
        matches!(self, Self::Repaired { .. })
    }

    pub fn is_refused(&self) -> bool {
        matches!(self, Self::Refused { .. })
    }
}

impl fmt::Display for RepairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "tool repair cancelled",
            Self::NotRepairable => "tool call is not repairable",
        })
    }
}

impl Error for RepairError {}

/// Validate the original call first; only a failed validation may be repaired.
///
/// Returns the repaired invocation and its hash-bound record, or a refused
/// outcome with model feedback. Never echoes argument values or secrets.
pub fn repair(
    call: &ToolCall,
    model_family: &str,
    cancel: &CancellationToken,
) -> Result<RepairOutcome, RepairError> {
    if cancel.is_cancelled() {
        return Err(RepairError::Cancelled);
    }
    match validate(call, cancel) {
        Ok(invocation) => return Ok(RepairOutcome::Valid(invocation)),
        Err(ValidateError::Cancelled) => return Err(RepairError::Cancelled),
        Err(_) => {}
    }

    let issues = match validate_issues(call, cancel) {
        Ok(_) => Vec::new(),
        Err(issues) => issues,
    };
    if issues.is_empty() {
        return Ok(RepairOutcome::Refused {
            issues,
            feedback: feedback("call could not be validated and no issue path was located"),
        });
    }

    let catalog = ToolCatalog::v1();
    let schema = match catalog.get_by_name(call.tool()) {
        Ok(tool) => tool.parameters(),
        Err(_) => {
            return Ok(RepairOutcome::Refused {
                issues,
                feedback: feedback("unknown tool cannot be repaired"),
            });
        }
    };

    let original_hash = ArtifactId::from_bytes(
        serde_json::to_vec(&call.arguments())
            .unwrap_or_default()
            .as_slice(),
    );
    let mut current_arguments = call.arguments().clone();
    let mut applied_rules: Vec<RepairRule> = Vec::new();

    for _pass in 0..MAX_REPAIR_PASSES {
        cancel_check(cancel)?;
        let mut changed = false;
        for issue in &issues {
            let path = parse_pointer(issue.path());
            if should_refuse(&path) {
                continue;
            }
            if let Some(rule) = attempt_repair(&mut current_arguments, &path, schema) {
                if !applied_rules.contains(&rule) && applied_rules.len() < MAX_REPAIR_RULES {
                    applied_rules.push(rule);
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }

        let candidate = ToolCall::new(
            call.schema(),
            call.call_id(),
            call.tool(),
            current_arguments.clone(),
        );
        match validate(&candidate, cancel) {
            Ok(invocation) => {
                let repaired_hash = ArtifactId::from_bytes(
                    serde_json::to_vec(&current_arguments)
                        .unwrap_or_default()
                        .as_slice(),
                );
                let confidence = compute_confidence(&applied_rules);
                let record = RepairRecord {
                    tool_id: call.tool().to_owned(),
                    schema_version: TOOL_INVOCATION_SCHEMA,
                    issue_paths: issues.iter().map(|issue| issue.path().to_owned()).collect(),
                    original_hash,
                    repair_rules: applied_rules.clone(),
                    repaired_hash,
                    confidence,
                };
                return Ok(RepairOutcome::Repaired {
                    record,
                    invocation,
                    feedback: build_feedback(model_family, call.tool(), &applied_rules, confidence),
                });
            }
            Err(ValidateError::Cancelled) => return Err(RepairError::Cancelled),
            Err(_) => {}
        }
    }

    let remaining = match validate_issues(
        &ToolCall::new(
            call.schema(),
            call.call_id(),
            call.tool(),
            current_arguments.clone(),
        ),
        cancel,
    ) {
        Ok(_) => Vec::new(),
        Err(issues) => issues,
    };
    Ok(RepairOutcome::Refused {
        issues: remaining,
        feedback: feedback("call was not repairable without changing its meaning"),
    })
}

/// Evaluate one cross-tool invariant. `execution_state` optional for now.
pub fn check_invariant(
    tool: &str,
    arguments: &Value,
    _execution_state: Option<&Value>,
) -> InvariantVerdict {
    match tool {
        "workspace.patch" => {
            let has_author =
                arguments.get("author").is_some() || arguments.get("base_revision").is_some();
            if has_author {
                InvariantVerdict::Allow
            } else {
                // A patch carrying neither an author nor a base revision cannot
                // be attribution-safe; the host may recover with a revision.
                InvariantVerdict::Recoverable
            }
        }
        _ => InvariantVerdict::Allow,
    }
}

fn attempt_repair(arguments: &mut Value, path: &[String], schema: &Value) -> Option<RepairRule> {
    if path.is_empty() {
        return None;
    }
    let (parent_segments, last) = path.split_at(path.len() - 1);
    let last = last[0].as_str();
    let parent = navigate_mut(arguments, parent_segments)?;
    let value_schema = schema_at_path(schema, path)?;

    // Null elision must remove the key from its object parent.
    if let Value::Object(object) = parent {
        if let Some(Value::Null) = object.get(last) {
            let nullable = schema_type_accepts_null(value_schema);
            let required = is_required(schema_at_path(schema, parent_segments)?, last);
            if !nullable && !required {
                object.remove(last);
                return Some(RepairRule::OptionalNullElision);
            }
        }
        let value = object.get_mut(last)?;
        return apply_value_repair(value, value_schema);
    }
    if let Value::Array(items) = parent {
        let index = last.parse::<usize>().ok()?;
        let value = items.get_mut(index)?;
        return apply_value_repair(value, value_schema);
    }
    None
}

fn apply_value_repair(value: &mut Value, schema: &Value) -> Option<RepairRule> {
    let object = schema.as_object()?;
    let type_name = object.get("type").and_then(Value::as_str);

    // Stringified collection decode.
    if let Value::String(raw) = value
        && matches!(type_name, Some("array" | "object"))
        && let Ok(parsed) = serde_json::from_str::<Value>(raw.as_str())
        && ((type_name == Some("array") && parsed.is_array())
            || (type_name == Some("object") && parsed.is_object()))
    {
        *value = parsed;
        return Some(RepairRule::StringifiedCollectionDecode);
    }

    // Singleton-array unwrap for a scalar target.
    if let Some(items) = value.as_array()
        && items.len() == 1
        && matches!(type_name, Some("string" | "number" | "integer" | "boolean"))
        && type_matches_value(type_name.unwrap_or(""), &items[0])
    {
        *value = items[0].clone();
        return Some(RepairRule::SingletonArrayUnwrap);
    }

    if let Value::String(raw) = value {
        // Numeric / boolean scalar coercion.
        if matches!(type_name, Some("number" | "integer"))
            && let Some(number) = parse_json_number(raw.as_str(), type_name == Some("integer"))
            && numeric_in_bounds(object, &number)
        {
            *value = Value::Number(number);
            return Some(RepairRule::NumericScalarCoercion);
        }
        if type_name == Some("boolean")
            && let Some(boolean) = match raw.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            }
        {
            *value = Value::Bool(boolean);
            return Some(RepairRule::BooleanScalarCoercion);
        }
        // Enum alias coercion (only an exact, unique alias).
        if let Some(Value::Array(options)) = object.get("enum")
            && let Some(enum_value) = resolve_enum_alias(raw.as_str(), options)
        {
            *value = enum_value;
            return Some(RepairRule::EnumAliasCoercion);
        }
    }
    None
}

fn resolve_enum_alias(raw: &str, options: &[Value]) -> Option<Value> {
    let mut matches = options
        .iter()
        .filter(|option| {
            option.as_str().is_some_and(|candidate| {
                candidate.eq_ignore_ascii_case(raw) || candidate.trim() == raw.trim()
            })
        })
        .cloned();
    let first = matches.next()?;
    if matches.next().is_some() {
        // Ambiguous: two distinct enum candidates map to the same input.
        return None;
    }
    Some(first)
}

fn parse_json_number(raw: &str, require_int: bool) -> Option<serde_json::Number> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if require_int {
        if trimmed.parse::<i64>().is_ok() {
            return serde_json::Number::from_str(trimmed).ok();
        }
        return None;
    }
    if trimmed.parse::<f64>().is_ok() {
        return serde_json::Number::from_str(trimmed).ok();
    }
    None
}

fn numeric_in_bounds(schema: &Map<String, Value>, number: &serde_json::Number) -> bool {
    let Some(value) = number.as_f64() else {
        return false;
    };
    let within = |key: &str, greater: bool| -> bool {
        let Some(bound) = schema.get(key).and_then(Value::as_f64) else {
            return true;
        };
        if greater {
            value >= bound
        } else {
            value <= bound
        }
    };
    within("minimum", true) && within("maximum", false)
}

fn schema_type_accepts_null(schema: &Value) -> bool {
    let type_name = schema.get("type").and_then(Value::as_str);
    if type_name == Some("null") {
        return true;
    }
    if let Some(const_value) = schema.get("const") {
        return const_value.is_null();
    }
    if let Some(Value::Array(types)) = schema.get("type") {
        return types.iter().any(|t| t.as_str() == Some("null"));
    }
    // Defaults to non-null unless "nullable" is explicitly declared.
    schema
        .get("nullable")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn is_required(schema: &Value, name: &str) -> bool {
    schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| required.iter().any(|item| item.as_str() == Some(name)))
}

fn type_matches_value(type_name: &str, value: &Value) -> bool {
    match type_name {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some(),
        "boolean" => value.is_boolean(),
        _ => false,
    }
}

fn navigate_mut<'a>(mut root: &'a mut Value, segments: &[String]) -> Option<&'a mut Value> {
    for segment in segments {
        match root {
            Value::Object(map) => root = map.get_mut(segment)?,
            Value::Array(items) => root = items.get_mut(segment.parse::<usize>().ok()?)?,
            _ => return None,
        }
    }
    Some(root)
}

fn schema_at_path<'a>(schema: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut current = schema;
    for segment in path {
        let object = current.as_object()?;
        let properties = object.get("properties").and_then(Value::as_object)?;
        if let Some(prop) = properties.get(segment) {
            current = prop;
            continue;
        }
        if let Some(items) = object.get("items") {
            current = items;
            continue;
        }
        return None;
    }
    Some(current)
}

fn parse_pointer(pointer: &str) -> Vec<String> {
    pointer
        .split('/')
        .skip(1)
        .map(|segment| segment.to_owned())
        .collect()
}

fn should_refuse(path: &[String]) -> bool {
    path.last()
        .is_some_and(|segment| NEVER_REPAIR_ARGUMENTS.contains(&segment.as_str()))
}

const NEVER_REPAIR_ARGUMENTS: &[&str] = &[
    "path",
    "host_path",
    "cwd",
    "script",
    "argv",
    "command",
    "host",
    "url",
    "server",
    "endpoint",
    "secret",
    "token",
    "password",
    "api_key",
    "private_key",
    "plaintext",
    "credential",
    "sudo",
    "elevate",
    "force_complete",
    "approval",
    "approve",
    "grant_capability",
    "capability_lease",
    "lease",
    "sh",
];

fn compute_confidence(rules: &[RepairRule]) -> ConfidenceClass {
    if rules.is_empty() {
        return ConfidenceClass::Ambiguous;
    }
    if rules.iter().all(|rule| {
        matches!(
            rule,
            RepairRule::OptionalNullElision | RepairRule::SingletonArrayUnwrap
        )
    }) {
        ConfidenceClass::Exact
    } else {
        ConfidenceClass::High
    }
}

fn build_feedback(
    model_family: &str,
    tool: &str,
    rules: &[RepairRule],
    confidence: ConfidenceClass,
) -> RepairFeedback {
    let mut message = String::from("tool call was repaired");
    if !model_family.is_empty() {
        message.push_str(" for ");
        message.push_str(model_family);
    }
    message.push_str(" on ");
    message.push_str(tool);
    message.push(':');
    for rule in rules {
        message.push(' ');
        message.push_str(rule.as_str());
        message.push(',');
    }
    message.push_str(" confidence ");
    message.push_str(confidence.as_str());
    if message.len() > MAX_FEEDBACK_BYTES {
        truncate_to_char_boundary(&mut message, MAX_FEEDBACK_BYTES);
    }
    RepairFeedback { message }
}

fn feedback(message: &str) -> RepairFeedback {
    let mut truncated = message.to_owned();
    if truncated.len() > MAX_FEEDBACK_BYTES {
        truncate_to_char_boundary(&mut truncated, MAX_FEEDBACK_BYTES);
    }
    RepairFeedback { message: truncated }
}

/// Truncates `s` to at most `max` bytes without panicking on a multi-byte
/// character straddling the cut. `String::truncate`/slicing panics unless
/// `max` is a char boundary; a byte-length check alone (`s.len() > max`)
/// does not make a raw cut at `max` safe for arbitrary UTF-8 — `tool` and
/// `model_family` in `build_feedback` come from the model/caller, not a
/// fixed literal.
fn truncate_to_char_boundary(s: &mut String, max: usize) {
    let mut cut = max.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

fn cancel_check(cancel: &CancellationToken) -> Result<(), RepairError> {
    if cancel.is_cancelled() {
        Err(RepairError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn call(tool: &str, arguments: Value) -> ToolCall {
        ToolCall::new(TOOL_INVOCATION_SCHEMA, "call_9", tool, arguments)
    }

    #[test]
    fn valid_call_returns_original() {
        let outcome = repair(
            &call("repo.search", json!({"query": "lease"})),
            "",
            &cancel(),
        )
        .expect("ok");
        assert!(matches!(outcome, RepairOutcome::Valid(_)));
        assert!(!outcome.is_repaired());
    }

    #[test]
    fn optional_null_elision_repairs_and_revalidates() {
        // repo.search "limit" is optional and integer; a null value is elided.
        let outcome = repair(
            &call(
                "repo.search",
                json!({"query": "lease", "limit": Value::Null}),
            ),
            "test-model",
            &cancel(),
        )
        .expect("ok");
        match outcome {
            RepairOutcome::Repaired {
                record, invocation, ..
            } => {
                assert_eq!(invocation.tool().as_str(), "repo.search");
                assert!(
                    record
                        .repair_rules()
                        .contains(&RepairRule::OptionalNullElision)
                );
                assert_eq!(record.confidence(), ConfidenceClass::Exact);
                assert!(invocation.arguments().get("limit").is_none());
            }
            other => panic!("expected repaired, got {other:?}"),
        }
    }

    #[test]
    fn stringified_array_decodes() {
        let outcome = repair(
            &call(
                "repo.search",
                json!({"query": "lease", "repos": "[\"main\"]"}),
            ),
            "",
            &cancel(),
        )
        .expect("ok");
        match outcome {
            RepairOutcome::Repaired {
                invocation, record, ..
            } => {
                assert_eq!(invocation.arguments()["repos"], json!(["main"]));
                assert!(
                    record
                        .repair_rules()
                        .contains(&RepairRule::StringifiedCollectionDecode)
                );
            }
            other => panic!("expected repaired, got {other:?}"),
        }
    }

    #[test]
    fn singleton_array_unwraps_scalar() {
        let outcome = repair(
            &call("repo.search", json!({"query": ["lease"]})),
            "",
            &cancel(),
        )
        .expect("ok");
        match outcome {
            RepairOutcome::Repaired { invocation, .. } => {
                assert_eq!(invocation.arguments()["query"], "lease");
            }
            other => panic!("expected repaired, got {other:?}"),
        }
    }

    #[test]
    fn numeric_and_boolean_scalars_coerce() {
        let outcome = repair(
            &call("repo.search", json!({"query": "lease", "limit": "20"})),
            "",
            &cancel(),
        )
        .expect("ok");
        match outcome {
            RepairOutcome::Repaired { invocation, .. } => {
                assert_eq!(invocation.arguments()["limit"], 20);
            }
            other => panic!("expected repaired, got {other:?}"),
        }
    }

    #[test]
    fn enum_alias_coerces_uniquely() {
        let outcome = repair(
            &call("repo.search", json!({"query": "lease", "mode": "Hybrid"})),
            "",
            &cancel(),
        )
        .expect("ok");
        match outcome {
            RepairOutcome::Repaired {
                invocation, record, ..
            } => {
                assert_eq!(invocation.arguments()["mode"], "hybrid");
                assert!(
                    record
                        .repair_rules()
                        .contains(&RepairRule::EnumAliasCoercion)
                );
            }
            other => panic!("expected repaired, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_path_refuses_repair() {
        // A path traversal value is never repaired.
        let outcome = repair(
            &call("repo.read", json!({"path": "../etc/passwd"})),
            "",
            &cancel(),
        )
        .expect("ok");
        assert!(outcome.is_refused());
    }

    #[test]
    fn never_repairs_secret_named_arguments() {
        let outcome = repair(
            &call(
                "external.call",
                json!({"kind":"mcp","server":"docs","tool":"search","arguments":{"token":"x"}}),
            ),
            "",
            &cancel(),
        )
        .expect("ok");
        assert!(outcome.is_refused() || outcome.is_repaired());
    }

    #[test]
    fn cancelled_repair_fails_closed() {
        let token = cancel();
        token.cancel();
        assert_eq!(
            repair(&call("repo.search", json!({"query": 1})), "", &token),
            Err(RepairError::Cancelled)
        );
    }

    #[test]
    fn invariant_engine_verdicts() {
        assert_eq!(
            check_invariant("workspace.patch", &json!({"author":"x"}), None),
            InvariantVerdict::Allow
        );
        assert_eq!(
            check_invariant("workspace.patch", &json!({}), None),
            InvariantVerdict::Recoverable
        );
        assert_eq!(
            check_invariant("repo.search", &json!({"query":"x"}), None),
            InvariantVerdict::Allow
        );
    }

    #[test]
    fn feedback_never_echoes_secret() {
        let outcome = repair(
            &call("external.call", json!({"kind":"mcp","server":"docs","tool":"search","arguments":{"token":"hunter2"}})),
            "",
            &cancel(),
        )
        .expect("ok");
        let message = match outcome {
            RepairOutcome::Repaired { feedback, .. } => feedback.message().to_owned(),
            RepairOutcome::Refused { feedback, .. } => feedback.message().to_owned(),
            RepairOutcome::Valid(_) => String::new(),
        };
        assert!(!message.contains("hunter2"));
    }

    #[test]
    fn build_feedback_does_not_panic_when_model_family_straddles_the_cap() {
        // "tool call was repaired for " is 27 bytes; 996 more ASCII bytes
        // then a 4-byte char lands that char at bytes 1023..1027, so byte
        // offset 1024 (MAX_FEEDBACK_BYTES) falls inside it, not on a
        // boundary — model_family is caller/model-controlled, not a fixed
        // literal, so this is reachable with real input.
        let model_family = format!("{}{}", "a".repeat(996), '\u{1D518}');
        let feedback = build_feedback(
            &model_family,
            "repo.read",
            &[RepairRule::OptionalNullElision],
            ConfidenceClass::Exact,
        );
        assert!(feedback.message.len() <= MAX_FEEDBACK_BYTES);
    }
}
