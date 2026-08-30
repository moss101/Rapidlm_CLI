//! Public error envelope and namespaced machine-readable codes.
//!
//! Wire form matches the domain-model public error object. Internal `source()`
//! chains are never stored or serialized.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::id::TraceId;

/// Public error envelope schema version (not a wire field).
pub const API_ERROR_SCHEMA: u16 = 1;

/// Safe public message used when the cause is unknown or internal.
pub const UNKNOWN_INTERNAL_MESSAGE: &str = "An unexpected internal error occurred";

/// Maximum UTF-8 bytes accepted in [`ApiError::message`].
pub const MAX_MESSAGE_BYTES: usize = 512;

/// Maximum number of entries in [`ErrorDetails`].
pub const MAX_DETAIL_FIELDS: usize = 16;

/// Maximum UTF-8 bytes accepted in a details key.
pub const MAX_DETAIL_KEY_BYTES: usize = 64;

/// Maximum UTF-8 bytes accepted in a details value.
pub const MAX_DETAIL_VALUE_BYTES: usize = 256;

const API_ERROR_FIELDS: &[&str] = &["code", "message", "retryable", "trace_id", "details"];

/// Stable v1 public error codes (`reference/error-codes.md`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    ConfigInvalid,
    AuthRequired,
    SessionConflict,
    SessionNotFound,
    GoalInvalidTransition,
    GoalEvidenceMissing,
    GoalBudgetExhausted,
    AgentConcurrencyLimit,
    ContextIndexUnavailable,
    WorkspacePreimageMismatch,
    WorkspaceMergeConflict,
    PolicyDenied,
    PolicyApprovalRequired,
    PolicyLeaseInvalid,
    SandboxTierUnavailable,
    ProcessTimeout,
    ProviderRateLimited,
    ProviderAuthFailed,
    ProviderContextTooLarge,
    ToolInvalidArguments,
    BrowserStaleObservation,
    MobileCapabilityUnavailable,
    McpServerUntrusted,
    PluginCapabilityDenied,
    StorageCorrupt,
    InternalUnexpected,
}

/// SDD error taxonomy class. Not serialized on the public envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RapidErrorClass {
    UserInput,
    PolicyDenied,
    ApprovalRequired,
    ProviderTransient,
    ProviderPermanent,
    ToolFailed,
    SandboxFailed,
    ResourceExhausted,
    Conflict,
    Corruption,
    Unsupported,
    Cancelled,
    Internal,
}

/// Parse failure for an unknown or malformed error code string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ErrorCodeParseError;

/// Failure when constructing bounded, sanitized [`ErrorDetails`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorDetailsError {
    EmptyKey,
    KeyTooLong,
    ValueTooLong,
    TooManyFields,
    ReservedKey,
}

/// Failure when constructing a known [`ApiError`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiErrorBuildError {
    EmptyMessage,
    MessageTooLong,
    Details(ErrorDetailsError),
}

/// Sanitized public details object. Source-chain keys are rejected.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ErrorDetails {
    fields: BTreeMap<String, String>,
}

/// Public API/SDK error envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiError {
    code: ErrorCode,
    message: String,
    retryable: bool,
    trace_id: TraceId,
    details: ErrorDetails,
}

impl ErrorCode {
    /// Registry order from `reference/error-codes.md`.
    pub const ALL: &'static [Self] = &[
        Self::ConfigInvalid,
        Self::AuthRequired,
        Self::SessionConflict,
        Self::SessionNotFound,
        Self::GoalInvalidTransition,
        Self::GoalEvidenceMissing,
        Self::GoalBudgetExhausted,
        Self::AgentConcurrencyLimit,
        Self::ContextIndexUnavailable,
        Self::WorkspacePreimageMismatch,
        Self::WorkspaceMergeConflict,
        Self::PolicyDenied,
        Self::PolicyApprovalRequired,
        Self::PolicyLeaseInvalid,
        Self::SandboxTierUnavailable,
        Self::ProcessTimeout,
        Self::ProviderRateLimited,
        Self::ProviderAuthFailed,
        Self::ProviderContextTooLarge,
        Self::ToolInvalidArguments,
        Self::BrowserStaleObservation,
        Self::MobileCapabilityUnavailable,
        Self::McpServerUntrusted,
        Self::PluginCapabilityDenied,
        Self::StorageCorrupt,
        Self::InternalUnexpected,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfigInvalid => "config.invalid",
            Self::AuthRequired => "auth.required",
            Self::SessionConflict => "session.conflict",
            Self::SessionNotFound => "session.not_found",
            Self::GoalInvalidTransition => "goal.invalid_transition",
            Self::GoalEvidenceMissing => "goal.evidence_missing",
            Self::GoalBudgetExhausted => "goal.budget_exhausted",
            Self::AgentConcurrencyLimit => "agent.concurrency_limit",
            Self::ContextIndexUnavailable => "context.index_unavailable",
            Self::WorkspacePreimageMismatch => "workspace.preimage_mismatch",
            Self::WorkspaceMergeConflict => "workspace.merge_conflict",
            Self::PolicyDenied => "policy.denied",
            Self::PolicyApprovalRequired => "policy.approval_required",
            Self::PolicyLeaseInvalid => "policy.lease_invalid",
            Self::SandboxTierUnavailable => "sandbox.tier_unavailable",
            Self::ProcessTimeout => "process.timeout",
            Self::ProviderRateLimited => "provider.rate_limited",
            Self::ProviderAuthFailed => "provider.auth_failed",
            Self::ProviderContextTooLarge => "provider.context_too_large",
            Self::ToolInvalidArguments => "tool.invalid_arguments",
            Self::BrowserStaleObservation => "browser.stale_observation",
            Self::MobileCapabilityUnavailable => "mobile.capability_unavailable",
            Self::McpServerUntrusted => "mcp.server_untrusted",
            Self::PluginCapabilityDenied => "plugin.capability_denied",
            Self::StorageCorrupt => "storage.corrupt",
            Self::InternalUnexpected => "internal.unexpected",
        }
    }

    /// Conservative default: only clearly transient codes are retryable.
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::AgentConcurrencyLimit
                | Self::ContextIndexUnavailable
                | Self::ProcessTimeout
                | Self::ProviderRateLimited
        )
    }

    pub const fn class(self) -> RapidErrorClass {
        match self {
            Self::ConfigInvalid
            | Self::AuthRequired
            | Self::SessionNotFound
            | Self::GoalEvidenceMissing
            | Self::ToolInvalidArguments => RapidErrorClass::UserInput,
            Self::SessionConflict
            | Self::GoalInvalidTransition
            | Self::WorkspacePreimageMismatch
            | Self::WorkspaceMergeConflict
            | Self::BrowserStaleObservation => RapidErrorClass::Conflict,
            Self::GoalBudgetExhausted | Self::AgentConcurrencyLimit | Self::ProcessTimeout => {
                RapidErrorClass::ResourceExhausted
            }
            Self::ContextIndexUnavailable | Self::MobileCapabilityUnavailable => {
                RapidErrorClass::Unsupported
            }
            Self::PolicyDenied
            | Self::PolicyLeaseInvalid
            | Self::McpServerUntrusted
            | Self::PluginCapabilityDenied => RapidErrorClass::PolicyDenied,
            Self::PolicyApprovalRequired => RapidErrorClass::ApprovalRequired,
            Self::SandboxTierUnavailable => RapidErrorClass::SandboxFailed,
            Self::ProviderRateLimited => RapidErrorClass::ProviderTransient,
            Self::ProviderAuthFailed | Self::ProviderContextTooLarge => {
                RapidErrorClass::ProviderPermanent
            }
            Self::StorageCorrupt => RapidErrorClass::Corruption,
            Self::InternalUnexpected => RapidErrorClass::Internal,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ErrorCode {
    type Err = ErrorCodeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for code in Self::ALL {
            if code.as_str() == s {
                return Ok(*code);
            }
        }
        Err(ErrorCodeParseError)
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(ErrorCodeVisitor)
    }
}

struct ErrorCodeVisitor;

impl Visitor<'_> for ErrorCodeVisitor {
    type Value = ErrorCode;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a namespaced RapidLM error code string")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        value.parse().map_err(|_| E::unknown_variant(value, &[]))
    }
}

impl fmt::Display for ErrorCodeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown error code")
    }
}

impl Error for ErrorCodeParseError {}

impl fmt::Display for ErrorDetailsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyKey => "error details key must be non-empty",
            Self::KeyTooLong => "error details key exceeds the byte bound",
            Self::ValueTooLong => "error details value exceeds the byte bound",
            Self::TooManyFields => "error details exceed the field bound",
            Self::ReservedKey => "error details key is reserved for internal source chains",
        })
    }
}

impl Error for ErrorDetailsError {}

impl fmt::Display for ApiErrorBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyMessage => f.write_str("error message must be non-empty"),
            Self::MessageTooLong => f.write_str("error message exceeds the byte bound"),
            Self::Details(inner) => fmt::Display::fmt(inner, f),
        }
    }
}

impl Error for ApiErrorBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Details(inner) => Some(inner),
            Self::EmptyMessage | Self::MessageTooLong => None,
        }
    }
}

impl ErrorDetails {
    pub const fn empty() -> Self {
        Self {
            fields: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.fields.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn insert(
        &mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), ErrorDetailsError> {
        let key = key.into();
        let value = value.into();
        validate_detail_entry(&key, &value)?;
        if !self.fields.contains_key(&key) && self.fields.len() >= MAX_DETAIL_FIELDS {
            return Err(ErrorDetailsError::TooManyFields);
        }
        self.fields.insert(key, value);
        Ok(())
    }
}

impl Serialize for ErrorDetails {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.fields.len()))?;
        for (key, value) in &self.fields {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for ErrorDetails {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ErrorDetailsVisitor)
    }
}

struct ErrorDetailsVisitor;

impl<'de> Visitor<'de> for ErrorDetailsVisitor {
    type Value = ErrorDetails;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object of safe string details")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut details = ErrorDetails::empty();
        while let Some((key, value)) = access.next_entry::<String, String>()? {
            details.insert(key, value).map_err(de::Error::custom)?;
        }
        Ok(details)
    }
}

impl ApiError {
    /// Construct a known public error. `retryable` follows [`ErrorCode::is_retryable`].
    pub fn new(
        code: ErrorCode,
        message: impl Into<String>,
        trace_id: TraceId,
    ) -> Result<Self, ApiErrorBuildError> {
        Self::new_with_details(code, message, trace_id, ErrorDetails::empty())
    }

    pub fn new_with_details(
        code: ErrorCode,
        message: impl Into<String>,
        trace_id: TraceId,
        details: ErrorDetails,
    ) -> Result<Self, ApiErrorBuildError> {
        let message = message.into();
        validate_message(&message)?;
        Ok(Self {
            code,
            message,
            retryable: code.is_retryable(),
            trace_id,
            details,
        })
    }

    /// Convert an unknown internal cause into a public envelope.
    ///
    /// The cause is accepted so callers can log it after redaction. It is never
    /// copied into `message` or `details`.
    pub fn from_unknown(trace_id: TraceId, _cause: &(dyn Error + '_)) -> Self {
        Self {
            code: ErrorCode::InternalUnexpected,
            message: UNKNOWN_INTERNAL_MESSAGE.to_owned(),
            retryable: ErrorCode::InternalUnexpected.is_retryable(),
            trace_id,
            details: ErrorDetails::empty(),
        }
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn retryable(&self) -> bool {
        self.retryable
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub fn details(&self) -> &ErrorDetails {
        &self.details
    }

    pub fn class(&self) -> RapidErrorClass {
        self.code.class()
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl Error for ApiError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

impl Serialize for ApiError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ApiError", 5)?;
        state.serialize_field("code", &self.code)?;
        state.serialize_field("message", &self.message)?;
        state.serialize_field("retryable", &self.retryable)?;
        state.serialize_field("trace_id", &self.trace_id)?;
        state.serialize_field("details", &self.details)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ApiError {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_struct("ApiError", API_ERROR_FIELDS, ApiErrorVisitor)
    }
}

struct ApiErrorVisitor;

#[derive(Clone, Copy)]
enum ApiErrorField {
    Code,
    Message,
    Retryable,
    TraceId,
    Details,
}

impl ApiErrorField {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "code" => Some(Self::Code),
            "message" => Some(Self::Message),
            "retryable" => Some(Self::Retryable),
            "trace_id" => Some(Self::TraceId),
            "details" => Some(Self::Details),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for ApiErrorField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_identifier(ApiErrorFieldVisitor)
    }
}

struct ApiErrorFieldVisitor;

impl Visitor<'_> for ApiErrorFieldVisitor {
    type Value = ApiErrorField;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an ApiError field")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        ApiErrorField::from_str(value).ok_or_else(|| E::unknown_field(value, API_ERROR_FIELDS))
    }
}

impl<'de> Visitor<'de> for ApiErrorVisitor {
    type Value = ApiError;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a public RapidLM ApiError object")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut code = None;
        let mut message = None;
        let mut retryable = None;
        let mut trace_id = None;
        let mut details = None;

        while let Some(field) = access.next_key()? {
            match field {
                ApiErrorField::Code => assign_once(&mut code, access.next_value()?, "code")?,
                ApiErrorField::Message => {
                    assign_once(&mut message, access.next_value()?, "message")?;
                }
                ApiErrorField::Retryable => {
                    assign_once(&mut retryable, access.next_value()?, "retryable")?;
                }
                ApiErrorField::TraceId => {
                    assign_once(&mut trace_id, access.next_value()?, "trace_id")?;
                }
                ApiErrorField::Details => {
                    assign_once(&mut details, access.next_value()?, "details")?;
                }
            }
        }

        let code: ErrorCode = code.ok_or_else(|| de::Error::missing_field("code"))?;
        let message: String = message.ok_or_else(|| de::Error::missing_field("message"))?;
        // Required on the wire (schema stability), but never trusted: retryable
        // is a derived property of `code`, not independent data, so a forged or
        // stale wire value must not override ErrorCode::is_retryable().
        // Required on the wire (schema stability), but never trusted: retryable
        // is a derived property of `code`, not independent data, so a forged or
        // stale wire value must not override ErrorCode::is_retryable().
        let _retryable: bool = retryable.ok_or_else(|| de::Error::missing_field("retryable"))?;
        let trace_id = trace_id.ok_or_else(|| de::Error::missing_field("trace_id"))?;
        let details = details.ok_or_else(|| de::Error::missing_field("details"))?;
        validate_message(&message).map_err(de::Error::custom)?;

        Ok(ApiError {
            code,
            message,
            retryable: code.is_retryable(),
            trace_id,
            details,
        })
    }
}

fn assign_once<T, E: de::Error>(
    slot: &mut Option<T>,
    value: T,
    field: &'static str,
) -> Result<(), E> {
    if slot.is_some() {
        Err(E::duplicate_field(field))
    } else {
        *slot = Some(value);
        Ok(())
    }
}

fn validate_message(message: &str) -> Result<(), ApiErrorBuildError> {
    if message.is_empty() {
        Err(ApiErrorBuildError::EmptyMessage)
    } else if message.len() > MAX_MESSAGE_BYTES {
        Err(ApiErrorBuildError::MessageTooLong)
    } else {
        Ok(())
    }
}

fn validate_detail_entry(key: &str, value: &str) -> Result<(), ErrorDetailsError> {
    if key.is_empty() {
        return Err(ErrorDetailsError::EmptyKey);
    }
    if key.len() > MAX_DETAIL_KEY_BYTES {
        return Err(ErrorDetailsError::KeyTooLong);
    }
    if value.len() > MAX_DETAIL_VALUE_BYTES {
        return Err(ErrorDetailsError::ValueTooLong);
    }
    if is_reserved_detail_key(key) {
        return Err(ErrorDetailsError::ReservedKey);
    }
    Ok(())
}

fn is_reserved_detail_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "source"
            | "cause"
            | "causes"
            | "inner"
            | "raw"
            | "exception"
            | "backtrace"
            | "back_trace"
            | "stack"
            | "stacktrace"
            | "stack_trace"
            | "error_chain"
            | "errorchain"
            | "source_chain"
            | "chained"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as StdError;

    const GOLDEN_TRACE: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";

    const GOLDEN_POLICY: &str = r#"{"code":"policy.denied","message":"Action denied by project policy","retryable":false,"trace_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","details":{}}"#;
    const GOLDEN_PROVIDER: &str = r#"{"code":"provider.rate_limited","message":"Provider rate limited","retryable":true,"trace_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","details":{}}"#;
    const GOLDEN_WORKSPACE: &str = r#"{"code":"workspace.preimage_mismatch","message":"Workspace preimage mismatch","retryable":false,"trace_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","details":{}}"#;

    #[derive(Debug)]
    struct InnerCause;

    impl fmt::Display for InnerCause {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("password=hunter2 stack: crates/secret.rs:12")
        }
    }

    impl StdError for InnerCause {}

    #[derive(Debug)]
    struct OuterCause;

    impl fmt::Display for OuterCause {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("provider dumped Authorization: Bearer leaked-token")
        }
    }

    impl StdError for OuterCause {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            Some(&InnerCause)
        }
    }

    fn golden_trace() -> TraceId {
        GOLDEN_TRACE.parse().expect("golden trace")
    }

    fn assert_golden(code: ErrorCode, message: &str, expected: &str) {
        let err = ApiError::new(code, message, golden_trace()).expect("build");
        let json = serde_json::to_string(&err).expect("serialize");
        assert_eq!(json, expected);
        let decoded: ApiError = serde_json::from_str(expected).expect("deserialize");
        assert_eq!(decoded, err);
        assert_eq!(decoded.code(), code);
        assert_eq!(decoded.retryable(), code.is_retryable());
        assert!(decoded.details().is_empty());
    }

    #[test]
    fn golden_json_covers_policy_provider_and_workspace() {
        assert_golden(
            ErrorCode::PolicyDenied,
            "Action denied by project policy",
            GOLDEN_POLICY,
        );
        assert_golden(
            ErrorCode::ProviderRateLimited,
            "Provider rate limited",
            GOLDEN_PROVIDER,
        );
        assert_golden(
            ErrorCode::WorkspacePreimageMismatch,
            "Workspace preimage mismatch",
            GOLDEN_WORKSPACE,
        );
    }

    #[test]
    fn unknown_internal_cause_does_not_leak_source_chain() {
        let err = ApiError::from_unknown(golden_trace(), &OuterCause);
        assert_eq!(err.code(), ErrorCode::InternalUnexpected);
        assert_eq!(err.message(), UNKNOWN_INTERNAL_MESSAGE);
        assert!(!err.retryable());
        assert!(err.details().is_empty());
        assert!(StdError::source(&err).is_none());

        let json = serde_json::to_string(&err).expect("serialize");
        for leaked in [
            "hunter2",
            "password=",
            "Authorization",
            "leaked-token",
            "crates/secret.rs",
            "Bearer",
            "stack:",
        ] {
            assert!(
                !json.contains(leaked),
                "serialized envelope leaked {leaked:?}: {json}"
            );
        }
        assert!(json.contains("\"details\":{}"));
        assert_eq!(err.class(), RapidErrorClass::Internal);
    }

    #[test]
    fn every_registry_code_round_trips() {
        for code in ErrorCode::ALL {
            assert_eq!(code.as_str().parse::<ErrorCode>(), Ok(*code));
            let json = serde_json::to_string(code).expect("serialize");
            assert_eq!(json, format!("\"{}\"", code.as_str()));
            let decoded: ErrorCode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, *code);
            let _ = code.class();
        }
    }

    #[test]
    fn unknown_code_is_rejected() {
        assert_eq!(
            "policy.allow".parse::<ErrorCode>(),
            Err(ErrorCodeParseError)
        );
        assert!(serde_json::from_str::<ErrorCode>("\"internal.panic\"").is_err());
        assert!(serde_json::from_str::<ErrorCode>("\"POLICY.DENIED\"").is_err());
    }

    #[test]
    fn reserved_detail_keys_are_rejected() {
        let mut details = ErrorDetails::empty();
        for key in ["cause", "source", "backtrace", "error_chain", "SOURCE"] {
            assert_eq!(
                details.insert(key, "password=hunter2"),
                Err(ErrorDetailsError::ReservedKey),
                "accepted reserved key {key}"
            );
        }
        details
            .insert("capability", "fs.write")
            .expect("safe detail");
        let err = ApiError::new_with_details(
            ErrorCode::PolicyDenied,
            "Action denied by project policy",
            golden_trace(),
            details,
        )
        .expect("build");
        let json = serde_json::to_string(&err).expect("serialize");
        assert!(json.contains("\"capability\":\"fs.write\""));
        assert!(!json.contains("password"));
    }

    #[test]
    fn details_bounds_are_enforced() {
        let mut details = ErrorDetails::empty();
        assert_eq!(details.insert("", "x"), Err(ErrorDetailsError::EmptyKey));
        assert_eq!(
            details.insert("k".repeat(MAX_DETAIL_KEY_BYTES + 1), "v"),
            Err(ErrorDetailsError::KeyTooLong)
        );
        assert_eq!(
            details.insert("k", "v".repeat(MAX_DETAIL_VALUE_BYTES + 1)),
            Err(ErrorDetailsError::ValueTooLong)
        );
        for i in 0..MAX_DETAIL_FIELDS {
            details.insert(format!("k{i}"), "v").expect("within bound");
        }
        assert_eq!(
            details.insert("overflow", "v"),
            Err(ErrorDetailsError::TooManyFields)
        );
    }

    #[test]
    fn deserialize_rejects_source_chain_details() {
        let json = r#"{"code":"internal.unexpected","message":"An unexpected internal error occurred","retryable":false,"trace_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","details":{"cause":"password=hunter2"}}"#;
        assert!(serde_json::from_str::<ApiError>(json).is_err());
    }

    #[test]
    fn deserialize_never_trusts_a_forged_retryable_bit() {
        // policy.denied is deliberately non-retryable (is_retryable's own
        // "conservative default" comment), but the wire value here forges
        // retryable:true. Deserialize must recompute from `code`, not trust
        // the wire bit, or a forged/stale peer could mark a fail-closed
        // error as safe to auto-retry.
        assert!(!ErrorCode::PolicyDenied.is_retryable());
        let json = r#"{"code":"policy.denied","message":"Action denied by project policy","retryable":true,"trace_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","details":{}}"#;
        let decoded: ApiError = serde_json::from_str(json).expect("decode");
        assert_eq!(decoded.code(), ErrorCode::PolicyDenied);
        assert!(
            !decoded.retryable(),
            "retryable must follow ErrorCode::is_retryable, never the wire value"
        );

        // The reverse forgery (retryable:false on a genuinely retryable code)
        // must also be corrected, not just the fail-closed direction.
        assert!(ErrorCode::ProcessTimeout.is_retryable());
        let json = r#"{"code":"process.timeout","message":"the operation timed out","retryable":false,"trace_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","details":{}}"#;
        let decoded: ApiError = serde_json::from_str(json).expect("decode");
        assert!(decoded.retryable());
    }
}
