//! Trace/span correlation IDs and context-propagation baggage.
//!
//! Wire form is a JSON object `{ trace_id, parent_span_id, baggage }`. Baggage
//! is a bounded string map; keys that name raw prompt, code, or secret content
//! are rejected at construction and on the wire (threat `T-012`).

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::id::{Id, IdParseError, TraceId};

/// Trace-context schema version (not a wire field).
pub const TRACE_CONTEXT_SCHEMA: u16 = 1;

/// Maximum number of entries in [`Baggage`].
pub const MAX_BAGGAGE_FIELDS: usize = 16;

/// Maximum UTF-8 bytes accepted in a baggage key.
pub const MAX_BAGGAGE_KEY_BYTES: usize = 64;

/// Maximum UTF-8 bytes accepted in a baggage value.
pub const MAX_BAGGAGE_VALUE_BYTES: usize = 256;

const TRACE_CONTEXT_FIELDS: &[&str] = &["trace_id", "parent_span_id", "baggage"];

/// Marker for [`SpanId`].
pub enum SpanTag {}

/// UUIDv7 span identifier. Wire form is the lowercase hyphenated UUID string.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SpanId(Id<SpanTag>);

/// Failure when constructing bounded, redacted [`Baggage`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BaggageError {
    EmptyKey,
    KeyTooLong,
    ValueTooLong,
    TooManyFields,
    ProtectedKey,
}

/// Bounded correlation attributes. Protected content keys cannot be stored.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Baggage {
    fields: BTreeMap<String, String>,
}

/// Propagated trace parent for events, tools, and providers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraceContext {
    trace_id: TraceId,
    parent_span_id: Option<SpanId>,
    baggage: Baggage,
}

impl SpanId {
    /// Allocate a fresh UUIDv7 span identifier.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(Id::new())
    }

    pub const fn from_uuid(value: Uuid) -> Self {
        Self(Id::from_uuid(value))
    }

    pub const fn as_uuid(&self) -> Uuid {
        self.0.as_uuid()
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for SpanId {
    type Err = IdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Id::from_str(s).map(Self)
    }
}

impl Serialize for SpanId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SpanId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Id::deserialize(deserializer).map(Self)
    }
}

impl fmt::Display for BaggageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyKey => "baggage key must be non-empty",
            Self::KeyTooLong => "baggage key exceeds the byte bound",
            Self::ValueTooLong => "baggage value exceeds the byte bound",
            Self::TooManyFields => "baggage exceeds the field bound",
            Self::ProtectedKey => "baggage key is protected (prompt/code/secret)",
        })
    }
}

impl Error for BaggageError {}

impl Baggage {
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
    ) -> Result<(), BaggageError> {
        let key = key.into();
        let value = value.into();
        validate_baggage_entry(&key, &value)?;
        if !self.fields.contains_key(&key) && self.fields.len() >= MAX_BAGGAGE_FIELDS {
            return Err(BaggageError::TooManyFields);
        }
        self.fields.insert(key, value);
        Ok(())
    }
}

impl Serialize for Baggage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.fields.len()))?;
        for (key, value) in &self.fields {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Baggage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(BaggageVisitor)
    }
}

struct BaggageVisitor;

impl<'de> Visitor<'de> for BaggageVisitor {
    type Value = Baggage;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object of redacted baggage entries")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut baggage = Baggage::empty();
        while let Some((key, value)) = access.next_entry::<String, String>()? {
            baggage.insert(key, value).map_err(de::Error::custom)?;
        }
        Ok(baggage)
    }
}

impl TraceContext {
    /// Root context: fresh [`TraceId`], no parent span, empty baggage.
    pub fn root() -> Self {
        Self::new(TraceId::new(), None, Baggage::empty())
    }

    pub fn new(trace_id: TraceId, parent_span_id: Option<SpanId>, baggage: Baggage) -> Self {
        Self {
            trace_id,
            parent_span_id,
            baggage,
        }
    }

    /// Child context sharing this trace and baggage, with `parent` as parent span.
    pub fn child(&self, parent: SpanId) -> Self {
        Self {
            trace_id: self.trace_id,
            parent_span_id: Some(parent),
            baggage: self.baggage.clone(),
        }
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub fn parent_span_id(&self) -> Option<SpanId> {
        self.parent_span_id
    }

    pub fn baggage(&self) -> &Baggage {
        &self.baggage
    }
}

impl Serialize for TraceContext {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("TraceContext", 3)?;
        state.serialize_field("trace_id", &self.trace_id)?;
        state.serialize_field("parent_span_id", &self.parent_span_id)?;
        state.serialize_field("baggage", &self.baggage)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for TraceContext {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_struct("TraceContext", TRACE_CONTEXT_FIELDS, TraceContextVisitor)
    }
}

struct TraceContextVisitor;

#[derive(Clone, Copy)]
enum TraceContextField {
    TraceId,
    ParentSpanId,
    Baggage,
}

impl TraceContextField {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "trace_id" => Some(Self::TraceId),
            "parent_span_id" => Some(Self::ParentSpanId),
            "baggage" => Some(Self::Baggage),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for TraceContextField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_identifier(TraceContextFieldVisitor)
    }
}

struct TraceContextFieldVisitor;

impl Visitor<'_> for TraceContextFieldVisitor {
    type Value = TraceContextField;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a TraceContext field")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        TraceContextField::from_str(value)
            .ok_or_else(|| E::unknown_field(value, TRACE_CONTEXT_FIELDS))
    }
}

impl<'de> Visitor<'de> for TraceContextVisitor {
    type Value = TraceContext;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a RapidLM TraceContext object")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut trace_id = None;
        let mut parent_span_id = None;
        let mut baggage = None;

        while let Some(field) = access.next_key()? {
            match field {
                TraceContextField::TraceId => {
                    assign_once(&mut trace_id, access.next_value()?, "trace_id")?;
                }
                TraceContextField::ParentSpanId => {
                    assign_once(&mut parent_span_id, access.next_value()?, "parent_span_id")?;
                }
                TraceContextField::Baggage => {
                    assign_once(&mut baggage, access.next_value()?, "baggage")?;
                }
            }
        }

        Ok(TraceContext {
            trace_id: trace_id.ok_or_else(|| de::Error::missing_field("trace_id"))?,
            parent_span_id: parent_span_id
                .ok_or_else(|| de::Error::missing_field("parent_span_id"))?,
            baggage: baggage.ok_or_else(|| de::Error::missing_field("baggage"))?,
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

fn validate_baggage_entry(key: &str, value: &str) -> Result<(), BaggageError> {
    if key.len() > MAX_BAGGAGE_KEY_BYTES {
        return Err(BaggageError::KeyTooLong);
    }
    if value.len() > MAX_BAGGAGE_VALUE_BYTES {
        return Err(BaggageError::ValueTooLong);
    }
    let normalized = normalize_baggage_key(key);
    if normalized.is_empty() {
        return Err(BaggageError::EmptyKey);
    }
    if is_protected_baggage_key(&normalized) {
        return Err(BaggageError::ProtectedKey);
    }
    Ok(())
}

/// Trim, ASCII-lowercase, and map `-` / `.` to `_` before the denylist check.
fn normalize_baggage_key(key: &str) -> String {
    key.trim().to_ascii_lowercase().replace(['-', '.'], "_")
}

fn is_protected_baggage_key(normalized_key: &str) -> bool {
    matches!(
        normalized_key,
        "prompt"
            | "prompts"
            | "raw_prompt"
            | "prompt_text"
            | "prompt_body"
            | "code"
            | "source_code"
            | "secret"
            | "secrets"
            | "password"
            | "passwd"
            | "token"
            | "api_key"
            | "apikey"
            | "credential"
            | "credentials"
            | "authorization"
            | "chain_of_thought"
            | "hidden_cot"
            | "cot"
            | "file_contents"
            | "file_content"
            | "shell_output"
            | "screenshot"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_TRACE: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const GOLDEN_SPAN: &str = "018f3c8a-7e2b-7a11-8c4d-0123456789ab";
    const GOLDEN_SESSION: &str = "018f3c8a-7e2b-7a12-8c4d-0123456789ab";

    const GOLDEN_ROOT: &str =
        r#"{"trace_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","parent_span_id":null,"baggage":{}}"#;
    const GOLDEN_CHILD: &str = r#"{"trace_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","parent_span_id":"018f3c8a-7e2b-7a11-8c4d-0123456789ab","baggage":{"session_id":"018f3c8a-7e2b-7a12-8c4d-0123456789ab"}}"#;

    fn golden_trace() -> TraceId {
        GOLDEN_TRACE.parse().expect("golden trace")
    }

    fn golden_span() -> SpanId {
        GOLDEN_SPAN.parse().expect("golden span")
    }

    fn assert_insert_rejects_protected(key: &str) {
        let mut baggage = Baggage::empty();
        assert_eq!(
            baggage.insert(key, "sk-hunter2"),
            Err(BaggageError::ProtectedKey),
            "accepted protected key {key:?}"
        );
        assert!(baggage.is_empty(), "stored protected key {key:?}");
    }

    fn assert_deserialize_rejects_protected(key: &str) {
        let json = format!(
            r#"{{"trace_id":"{GOLDEN_TRACE}","parent_span_id":null,"baggage":{{"{key}":"sk-hunter2"}}}}"#
        );
        assert!(
            serde_json::from_str::<TraceContext>(&json).is_err(),
            "accepted smuggled baggage key {key:?}"
        );
    }

    #[test]
    fn ids_round_trip_across_json() {
        let trace = golden_trace();
        let span = golden_span();

        let trace_json = serde_json::to_string(&trace).expect("serialize trace");
        assert_eq!(trace_json, format!("\"{GOLDEN_TRACE}\""));
        let decoded_trace: TraceId = serde_json::from_str(&trace_json).expect("deserialize trace");
        assert_eq!(decoded_trace, trace);
        assert_eq!(decoded_trace.to_string(), GOLDEN_TRACE);

        let span_json = serde_json::to_string(&span).expect("serialize span");
        assert_eq!(span_json, format!("\"{GOLDEN_SPAN}\""));
        let decoded_span: SpanId = serde_json::from_str(&span_json).expect("deserialize span");
        assert_eq!(decoded_span, span);
        assert_eq!(decoded_span.to_string(), GOLDEN_SPAN);

        let generated = SpanId::new();
        let generated_json = serde_json::to_string(&generated).expect("serialize generated");
        let generated_decoded: SpanId =
            serde_json::from_str(&generated_json).expect("deserialize generated");
        assert_eq!(generated_decoded, generated);
        assert_eq!(
            generated.to_string(),
            generated.to_string().to_ascii_lowercase()
        );
    }

    #[test]
    fn span_id_rejects_non_canonical_forms() {
        for sample in [
            "",
            "not-a-uuid",
            "018F3C8A-7E2B-7A11-8C4D-0123456789AB",
            "018f3c8a7e2b7a118c4d0123456789ab",
        ] {
            assert_eq!(sample.parse::<SpanId>(), Err(IdParseError));
            assert!(serde_json::from_str::<SpanId>(&format!("\"{sample}\"")).is_err());
        }
    }

    #[test]
    fn golden_trace_context_round_trips() {
        let root = TraceContext::new(golden_trace(), None, Baggage::empty());
        let json = serde_json::to_string(&root).expect("serialize root");
        assert_eq!(json, GOLDEN_ROOT);
        let decoded = serde_json::from_str::<TraceContext>(GOLDEN_ROOT).expect("deserialize root");
        assert_eq!(decoded, root);
        assert_eq!(decoded.parent_span_id(), None);

        let mut baggage = Baggage::empty();
        baggage
            .insert("session_id", GOLDEN_SESSION)
            .expect("correlation id");
        let child = TraceContext::new(golden_trace(), Some(golden_span()), baggage);
        let json = serde_json::to_string(&child).expect("serialize child");
        assert_eq!(json, GOLDEN_CHILD);
        let decoded =
            serde_json::from_str::<TraceContext>(GOLDEN_CHILD).expect("deserialize child");
        assert_eq!(decoded, child);
        assert_eq!(decoded.trace_id(), golden_trace());
        assert_eq!(decoded.parent_span_id(), Some(golden_span()));
        assert_eq!(decoded.baggage().get("session_id"), Some(GOLDEN_SESSION));
    }

    #[test]
    fn child_inherits_trace_and_baggage() {
        let mut baggage = Baggage::empty();
        baggage.insert("job_id", GOLDEN_SESSION).expect("job");
        let parent = TraceContext::new(golden_trace(), None, baggage);
        let child = parent.child(golden_span());
        assert_eq!(child.trace_id(), parent.trace_id());
        assert_eq!(child.parent_span_id(), Some(golden_span()));
        assert_eq!(child.baggage().get("job_id"), Some(GOLDEN_SESSION));
    }

    #[test]
    fn baggage_rejects_protected_keys() {
        for key in [
            "prompt",
            "PROMPT",
            "raw_prompt",
            "code",
            "source_code",
            "secret",
            "SECRET",
            "password",
            "token",
            "api_key",
            "chain_of_thought",
            "hidden_cot",
        ] {
            assert_insert_rejects_protected(key);
        }
        let mut baggage = Baggage::empty();
        baggage
            .insert("session_id", GOLDEN_SESSION)
            .expect("safe correlation key");
        assert_eq!(baggage.get("session_id"), Some(GOLDEN_SESSION));
        assert!(baggage.get("prompt").is_none());
        assert!(baggage.get("secret").is_none());
        assert!(baggage.get("code").is_none());
    }

    #[test]
    fn deserialize_rejects_protected_baggage_keys() {
        for key in ["prompt", "code", "secret", "raw_prompt"] {
            assert_deserialize_rejects_protected(key);
        }
    }

    #[test]
    fn baggage_rejects_hyphen_dot_and_whitespace_aliases() {
        for key in ["api-key", "raw-prompt", "source-code", " prompt "] {
            assert_insert_rejects_protected(key);
            assert_deserialize_rejects_protected(key);
        }
        assert_insert_rejects_protected("api.key");
        assert_deserialize_rejects_protected("api.key");
    }

    #[test]
    fn deserialize_rejects_unknown_and_content_fields() {
        for field in ["prompt", "code", "secret", "span_id"] {
            let json = format!(
                r#"{{"trace_id":"{GOLDEN_TRACE}","parent_span_id":null,"baggage":{{}},"{field}":"sk-hunter2"}}"#
            );
            assert!(
                serde_json::from_str::<TraceContext>(&json).is_err(),
                "accepted smuggled field {field}"
            );
        }
    }

    #[test]
    fn baggage_bounds_are_enforced() {
        let mut baggage = Baggage::empty();
        assert_eq!(baggage.insert("", "x"), Err(BaggageError::EmptyKey));
        assert_eq!(
            baggage.insert("k".repeat(MAX_BAGGAGE_KEY_BYTES + 1), "v"),
            Err(BaggageError::KeyTooLong)
        );
        assert_eq!(
            baggage.insert("k", "v".repeat(MAX_BAGGAGE_VALUE_BYTES + 1)),
            Err(BaggageError::ValueTooLong)
        );
        for i in 0..MAX_BAGGAGE_FIELDS {
            baggage.insert(format!("k{i}"), "v").expect("within bound");
        }
        assert_eq!(
            baggage.insert("overflow", "v"),
            Err(BaggageError::TooManyFields)
        );
    }

    #[test]
    fn serialized_context_does_not_emit_protected_content() {
        let ctx = TraceContext::new(golden_trace(), Some(golden_span()), Baggage::empty());
        let json = serde_json::to_string(&ctx).expect("serialize");
        let debug = format!("{ctx:?}");
        for leaked in ["sk-hunter2", "password", "prompt", "source_code"] {
            assert!(!json.contains(leaked), "json leaked {leaked:?}: {json}");
            assert!(!debug.contains(leaked), "debug leaked {leaked:?}: {debug}");
        }
        assert_eq!(TRACE_CONTEXT_SCHEMA, 1);
    }
}
