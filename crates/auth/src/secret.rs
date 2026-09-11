//! Secret-aware values, opaque handles, and redacted secret buffers.
//!
//! Plaintext non-secret data and credential handles are distinct types. Secret
//! bytes never appear in `Debug`, `Display`, or serde metadata. Revealing
//! plaintext requires a broker-issued, target-scoped, one-use token.

use std::fmt::{self, Debug, Display, Formatter};
use std::sync::atomic::{Ordering, compiler_fence};

use serde::de::{Error as DeError, IgnoredAny};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum accepted secret buffer / non-secret payload size.
pub const MAX_SECRET_BYTES: usize = 64 * 1024;

/// Maximum accepted `SecretRef` identifier length (hyphenated UUID).
pub const MAX_SECRET_REF_ID_BYTES: usize = 36;

/// Maximum accepted `SecretRef` alias length (`env:NAME`, keychain id, …).
pub const MAX_SECRET_REF_ALIAS_BYTES: usize = 256;

/// Maximum accepted secret-target domain length.
pub const MAX_SECRET_TARGET_BYTES: usize = 128;

const SECRET_REF_SCHEMA: &str = "rapidlm.auth.secret_ref";
const SECRET_VALUE_SCHEMA: &str = "rapidlm.auth.secret_value";
const SECRET_AWARE_SCHEMA: &str = "rapidlm.auth.secret_aware_value";
const SCHEMA_VERSION: u32 = 1;

/// Opaque credential handle. Never carries secret plaintext.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SecretRef {
    id: Option<String>,
    alias: Option<String>,
}

/// In-memory secret buffer. Not `Clone`; bytes are wiped on drop.
pub struct SecretValue {
    refer: SecretRef,
    bytes: Vec<u8>,
}

/// Distinguishes non-secret plaintext from a credential handle.
#[derive(Clone, Eq, PartialEq, Hash)]
pub enum SecretAwareValue {
    /// Non-secret text. Safe to show in Debug/Display/serde.
    Plaintext(String),
    /// Opaque handle. Never carries secret plaintext.
    Handle(SecretRef),
}

/// Destination that may receive materialized secret bytes.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SecretTarget {
    domain: String,
}

/// Broker-issued proof that plaintext may be revealed for one target-scoped use.
///
/// Construction is crate-private so only Auth/SecretBroker paths can mint tokens.
pub struct SecretBrokerToken {
    secret_ref: SecretRef,
    target: SecretTarget,
    cancelled: bool,
    spent: bool,
}

/// Borrow of secret bytes obtained through [`SecretValue::expose`].
///
/// Not `Clone`. `Debug` is redacted. There is no `Display` impl.
#[must_use]
pub struct ExposedSecret<'a> {
    bytes: &'a [u8],
}

/// Typed failures for secret-handle and expose operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretError {
    ExposeUnauthorized,
    TargetMismatch,
    Cancelled,
    BoundExceeded { limit: usize, requested: usize },
    Empty,
    InvalidRef,
    InvalidTarget,
    NotUtf8,
    DeserializeRejected,
}

impl SecretRef {
    pub const SCHEMA: &'static str = SECRET_REF_SCHEMA;
    pub const SCHEMA_VERSION: u32 = SCHEMA_VERSION;

    /// Parse a lowercase-normalized hyphenated UUID handle.
    pub fn from_id(id: &str) -> Result<Self, SecretError> {
        let id = parse_uuid_id(id)?;
        Ok(Self {
            id: Some(id),
            alias: None,
        })
    }

    /// Parse an opaque alias (`env:NAME`, keychain id, provider handle).
    pub fn from_alias(alias: &str) -> Result<Self, SecretError> {
        let alias = parse_alias(alias)?;
        Ok(Self {
            id: None,
            alias: Some(alias),
        })
    }

    /// Parse a UUID handle with an additional alias.
    pub fn from_id_and_alias(id: &str, alias: &str) -> Result<Self, SecretError> {
        Ok(Self {
            id: Some(parse_uuid_id(id)?),
            alias: Some(parse_alias(alias)?),
        })
    }

    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    pub fn alias(&self) -> Option<&str> {
        self.alias.as_deref()
    }

    /// Whether `self` and `other` name the same handle: exact equality, or
    /// a shared id, or a shared alias. A store may hold a fuller ref
    /// (id+alias) than the one a caller originally queried or was issued a
    /// token for (id-only or alias-only) — this is the single definition of
    /// "same handle" both the store's own lookup and [`SecretValue::expose`]
    /// must agree on, so the two can never diverge again.
    pub(crate) fn matches(&self, other: &SecretRef) -> bool {
        if self == other {
            return true;
        }
        if let (Some(a), Some(b)) = (self.id(), other.id())
            && a == b
        {
            return true;
        }
        if let (Some(a), Some(b)) = (self.alias(), other.alias())
            && a == b
        {
            return true;
        }
        false
    }
}

impl SecretValue {
    pub const SCHEMA: &'static str = SECRET_VALUE_SCHEMA;
    pub const SCHEMA_VERSION: u32 = SCHEMA_VERSION;

    /// Ingest secret bytes. Callers outside Auth should keep only [`SecretRef`].
    pub fn from_bytes(refer: SecretRef, bytes: Vec<u8>) -> Result<Self, SecretError> {
        if bytes.is_empty() {
            return Err(SecretError::Empty);
        }
        if bytes.len() > MAX_SECRET_BYTES {
            return Err(SecretError::BoundExceeded {
                limit: MAX_SECRET_BYTES,
                requested: bytes.len(),
            });
        }
        Ok(Self { refer, bytes })
    }

    /// Ingest UTF-8 secret text.
    pub fn from_str(refer: SecretRef, value: &str) -> Result<Self, SecretError> {
        Self::from_bytes(refer, value.as_bytes().to_vec())
    }

    pub fn secret_ref(&self) -> &SecretRef {
        &self.refer
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Reveal plaintext. Requires a live broker token bound to `target`.
    ///
    /// The token is spent on success so a second expose is a deny.
    pub fn expose<'a>(
        &'a self,
        token: &mut SecretBrokerToken,
        target: &SecretTarget,
    ) -> Result<ExposedSecret<'a>, SecretError> {
        if token.cancelled {
            return Err(SecretError::Cancelled);
        }
        if token.spent {
            return Err(SecretError::ExposeUnauthorized);
        }
        if !token.secret_ref.matches(&self.refer) {
            return Err(SecretError::ExposeUnauthorized);
        }
        if token.target != *target {
            return Err(SecretError::TargetMismatch);
        }
        token.spent = true;
        Ok(ExposedSecret { bytes: &self.bytes })
    }
}

impl SecretAwareValue {
    pub const SCHEMA: &'static str = SECRET_AWARE_SCHEMA;
    pub const SCHEMA_VERSION: u32 = SCHEMA_VERSION;

    pub fn plaintext(value: impl Into<String>) -> Result<Self, SecretError> {
        let value = value.into();
        if value.len() > MAX_SECRET_BYTES {
            return Err(SecretError::BoundExceeded {
                limit: MAX_SECRET_BYTES,
                requested: value.len(),
            });
        }
        Ok(Self::Plaintext(value))
    }

    pub fn handle(refer: SecretRef) -> Self {
        Self::Handle(refer)
    }

    pub fn is_handle(&self) -> bool {
        matches!(self, Self::Handle(_))
    }

    pub fn as_plaintext(&self) -> Option<&str> {
        match self {
            Self::Plaintext(value) => Some(value.as_str()),
            Self::Handle(_) => None,
        }
    }

    pub fn as_handle(&self) -> Option<&SecretRef> {
        match self {
            Self::Plaintext(_) => None,
            Self::Handle(refer) => Some(refer),
        }
    }
}

impl SecretTarget {
    pub fn new(domain: &str) -> Result<Self, SecretError> {
        if domain.is_empty() {
            return Err(SecretError::InvalidTarget);
        }
        if domain.len() > MAX_SECRET_TARGET_BYTES {
            return Err(SecretError::BoundExceeded {
                limit: MAX_SECRET_TARGET_BYTES,
                requested: domain.len(),
            });
        }
        if !domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b':' | b'_' | b'-' | b'/'))
        {
            return Err(SecretError::InvalidTarget);
        }
        Ok(Self {
            domain: domain.to_ascii_lowercase(),
        })
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }
}

impl SecretBrokerToken {
    /// Mint a one-use expose token. Only Auth/SecretBroker may call this.
    #[allow(dead_code)] // minted by SecretBroker / store paths in later auth tasks
    pub(crate) fn issue(secret_ref: SecretRef, target: SecretTarget) -> Self {
        Self {
            secret_ref,
            target,
            cancelled: false,
            spent: false,
        }
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn is_spent(&self) -> bool {
        self.spent
    }

    pub fn secret_ref(&self) -> &SecretRef {
        &self.secret_ref
    }

    pub fn target(&self) -> &SecretTarget {
        &self.target
    }
}

impl ExposedSecret<'_> {
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes
    }

    pub fn as_str(&self) -> Result<&str, SecretError> {
        std::str::from_utf8(self.bytes).map_err(|_| SecretError::NotUtf8)
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl SecretError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ExposeUnauthorized => "auth.secret_expose_unauthorized",
            Self::TargetMismatch => "auth.secret_target_mismatch",
            Self::Cancelled => "auth.cancelled",
            Self::BoundExceeded { .. } => "auth.secret_bound_exceeded",
            Self::Empty => "auth.secret_empty",
            Self::InvalidRef => "auth.secret_ref_invalid",
            Self::InvalidTarget => "auth.secret_target_invalid",
            Self::NotUtf8 => "auth.secret_not_utf8",
            Self::DeserializeRejected => "auth.secret_deserialize_rejected",
        }
    }

    pub fn retryable(&self) -> bool {
        false
    }
}

impl Display for SecretError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExposeUnauthorized => f.write_str("secret expose was not authorized"),
            Self::TargetMismatch => {
                f.write_str("secret expose target did not match the broker token")
            }
            Self::Cancelled => f.write_str("secret operation was cancelled"),
            Self::BoundExceeded { limit, requested } => {
                write!(
                    f,
                    "secret payload exceeds bound ({requested} > {limit} bytes)"
                )
            }
            Self::Empty => f.write_str("secret value must not be empty"),
            Self::InvalidRef => f.write_str("secret reference is invalid"),
            Self::InvalidTarget => f.write_str("secret target is invalid"),
            Self::NotUtf8 => f.write_str("secret bytes are not valid UTF-8"),
            Self::DeserializeRejected => {
                f.write_str("secret values cannot be deserialized; use a SecretRef")
            }
        }
    }
}

impl std::error::Error for SecretError {}

impl Drop for SecretValue {
    fn drop(&mut self) {
        for byte in &mut self.bytes {
            *byte = 0;
        }
        compiler_fence(Ordering::SeqCst);
        self.bytes.clear();
    }
}

impl Debug for SecretRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretRef")
            .field("redacted", &true)
            .finish()
    }
}

impl Display for SecretRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("SecretRef(<redacted>)")
    }
}

impl Debug for SecretValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretValue")
            .field("redacted", &true)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

impl Display for SecretValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

impl Debug for SecretAwareValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plaintext(value) => f.debug_tuple("Plaintext").field(value).finish(),
            Self::Handle(refer) => f.debug_tuple("Handle").field(refer).finish(),
        }
    }
}

impl Display for SecretAwareValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plaintext(value) => f.write_str(value),
            Self::Handle(_) => f.write_str("SecretAwareValue::Handle(<redacted>)"),
        }
    }
}

impl Debug for SecretTarget {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretTarget")
            .field("domain", &self.domain)
            .finish()
    }
}

impl Display for SecretTarget {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.domain)
    }
}

impl Debug for SecretBrokerToken {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretBrokerToken")
            .field("secret_ref", &self.secret_ref)
            .field("target", &self.target)
            .field("cancelled", &self.cancelled)
            .field("spent", &self.spent)
            .finish()
    }
}

impl Debug for ExposedSecret<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExposedSecret")
            .field("redacted", &true)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

impl Serialize for SecretRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let field_count = 2 + usize::from(self.id.is_some()) + usize::from(self.alias.is_some());
        let mut state = serializer.serialize_struct("SecretRef", field_count)?;
        state.serialize_field("schema", SECRET_REF_SCHEMA)?;
        state.serialize_field("schema_version", &SCHEMA_VERSION)?;
        if let Some(id) = &self.id {
            state.serialize_field("id", id)?;
        }
        if let Some(alias) = &self.alias {
            state.serialize_field("alias", alias)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            schema: Option<String>,
            #[serde(default)]
            schema_version: Option<u32>,
            #[serde(default)]
            id: Option<String>,
            #[serde(default)]
            alias: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        if let Some(schema) = wire.schema.as_deref()
            && schema != SECRET_REF_SCHEMA
        {
            return Err(D::Error::custom("unsupported SecretRef schema"));
        }
        if let Some(version) = wire.schema_version
            && version != SCHEMA_VERSION
        {
            return Err(D::Error::custom("unsupported SecretRef schema_version"));
        }
        match (wire.id.as_deref(), wire.alias.as_deref()) {
            (Some(id), Some(alias)) => {
                SecretRef::from_id_and_alias(id, alias).map_err(D::Error::custom)
            }
            (Some(id), None) => SecretRef::from_id(id).map_err(D::Error::custom),
            (None, Some(alias)) => SecretRef::from_alias(alias).map_err(D::Error::custom),
            (None, None) => Err(D::Error::custom("SecretRef requires id or alias")),
        }
    }
}

impl Serialize for SecretValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("SecretValue", 5)?;
        state.serialize_field("schema", SECRET_VALUE_SCHEMA)?;
        state.serialize_field("schema_version", &SCHEMA_VERSION)?;
        state.serialize_field("redacted", &true)?;
        state.serialize_field("byte_len", &self.bytes.len())?;
        state.serialize_field("ref", &self.refer)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for SecretValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let _ = IgnoredAny::deserialize(deserializer)?;
        Err(D::Error::custom(SecretError::DeserializeRejected))
    }
}

impl Serialize for SecretAwareValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match self {
            Self::Plaintext(value) => {
                let mut state = serializer.serialize_struct("SecretAwareValue", 4)?;
                state.serialize_field("schema", SECRET_AWARE_SCHEMA)?;
                state.serialize_field("schema_version", &SCHEMA_VERSION)?;
                state.serialize_field("kind", "plaintext")?;
                state.serialize_field("value", value)?;
                state.end()
            }
            Self::Handle(refer) => {
                let mut state = serializer.serialize_struct("SecretAwareValue", 4)?;
                state.serialize_field("schema", SECRET_AWARE_SCHEMA)?;
                state.serialize_field("schema_version", &SCHEMA_VERSION)?;
                state.serialize_field("kind", "handle")?;
                state.serialize_field("ref", refer)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for SecretAwareValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            schema: Option<String>,
            #[serde(default)]
            schema_version: Option<u32>,
            kind: String,
            #[serde(default)]
            value: Option<String>,
            #[serde(default, rename = "ref")]
            refer: Option<SecretRef>,
        }

        let wire = Wire::deserialize(deserializer)?;
        if let Some(schema) = wire.schema.as_deref()
            && schema != SECRET_AWARE_SCHEMA
        {
            return Err(D::Error::custom("unsupported SecretAwareValue schema"));
        }
        if let Some(version) = wire.schema_version
            && version != SCHEMA_VERSION
        {
            return Err(D::Error::custom(
                "unsupported SecretAwareValue schema_version",
            ));
        }
        match wire.kind.as_str() {
            "plaintext" => {
                let value = wire
                    .value
                    .ok_or_else(|| D::Error::custom("plaintext SecretAwareValue missing value"))?;
                SecretAwareValue::plaintext(value).map_err(D::Error::custom)
            }
            "handle" => {
                let refer = wire
                    .refer
                    .ok_or_else(|| D::Error::custom("handle SecretAwareValue missing ref"))?;
                Ok(SecretAwareValue::handle(refer))
            }
            _ => Err(D::Error::custom("unknown SecretAwareValue kind")),
        }
    }
}

fn parse_uuid_id(id: &str) -> Result<String, SecretError> {
    if id.len() != MAX_SECRET_REF_ID_BYTES {
        return Err(if id.len() > MAX_SECRET_REF_ID_BYTES {
            SecretError::BoundExceeded {
                limit: MAX_SECRET_REF_ID_BYTES,
                requested: id.len(),
            }
        } else {
            SecretError::InvalidRef
        });
    }
    let bytes = id.as_bytes();
    const DASHES: [usize; 4] = [8, 13, 18, 23];
    for (index, byte) in bytes.iter().copied().enumerate() {
        if DASHES.contains(&index) {
            if byte != b'-' {
                return Err(SecretError::InvalidRef);
            }
        } else if !byte.is_ascii_hexdigit() {
            return Err(SecretError::InvalidRef);
        }
    }
    Ok(id.to_ascii_lowercase())
}

fn parse_alias(alias: &str) -> Result<String, SecretError> {
    if alias.is_empty() {
        return Err(SecretError::InvalidRef);
    }
    if alias.len() > MAX_SECRET_REF_ALIAS_BYTES {
        return Err(SecretError::BoundExceeded {
            limit: MAX_SECRET_REF_ALIAS_BYTES,
            requested: alias.len(),
        });
    }
    if !alias
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'/' | b'-' | b'@'))
    {
        return Err(SecretError::InvalidRef);
    }
    Ok(alias.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-9f3c2a";
    const REF_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";
    const REF_ALIAS: &str = "env:CANARY";

    fn sample_ref() -> SecretRef {
        SecretRef::from_id_and_alias(REF_ID, REF_ALIAS).expect("valid test SecretRef")
    }

    fn sample_value() -> SecretValue {
        SecretValue::from_str(sample_ref(), CANARY).expect("valid test SecretValue")
    }

    fn sample_target() -> SecretTarget {
        SecretTarget::new("process.env").expect("valid test target")
    }

    fn assert_no_canary(label: &str, rendered: &str) {
        assert!(
            !rendered.contains(CANARY),
            "{label} leaked secret plaintext: {rendered}"
        );
        assert!(
            !rendered.contains("PLAINTEXT-do-not-leak"),
            "{label} leaked secret substring: {rendered}"
        );
    }

    #[test]
    fn secret_value_debug_display_serde_snapshot_redacts_plaintext() {
        let value = sample_value();

        let debug = format!("{value:?}");
        let display = format!("{value}");
        let json = serde_json::to_string(&value).expect("serialize SecretValue metadata");

        assert_eq!(debug, "SecretValue { redacted: true, byte_len: 42 }");
        assert_eq!(display, "SecretValue(<redacted>)");
        assert_eq!(
            json,
            "{\"schema\":\"rapidlm.auth.secret_value\",\"schema_version\":1,\"redacted\":true,\"byte_len\":42,\"ref\":{\"schema\":\"rapidlm.auth.secret_ref\",\"schema_version\":1,\"id\":\"01234567-89ab-cdef-0123-456789abcdef\",\"alias\":\"env:CANARY\"}}"
        );

        assert_no_canary("Debug", &debug);
        assert_no_canary("Display", &display);
        assert_no_canary("serde", &json);
    }

    #[test]
    fn secret_ref_debug_display_are_redacted() {
        let refer = sample_ref();
        let debug = format!("{refer:?}");
        let display = format!("{refer}");
        assert_eq!(debug, "SecretRef { redacted: true }");
        assert_eq!(display, "SecretRef(<redacted>)");
        assert!(!debug.contains(REF_ID));
        assert!(!display.contains(REF_ALIAS));
    }

    #[test]
    fn secret_aware_value_distinguishes_plaintext_from_handle() {
        let plain = SecretAwareValue::plaintext("hello-non-secret").expect("plaintext");
        let handle = SecretAwareValue::handle(sample_ref());

        assert_eq!(plain.as_plaintext(), Some("hello-non-secret"));
        assert!(plain.as_handle().is_none());
        assert!(!plain.is_handle());

        assert!(handle.as_plaintext().is_none());
        assert!(handle.as_handle().is_some());
        assert!(handle.is_handle());

        assert_eq!(format!("{plain}"), "hello-non-secret");
        assert_eq!(format!("{handle}"), "SecretAwareValue::Handle(<redacted>)");
        assert_eq!(
            serde_json::to_string(&plain).expect("serialize plaintext"),
            "{\"schema\":\"rapidlm.auth.secret_aware_value\",\"schema_version\":1,\"kind\":\"plaintext\",\"value\":\"hello-non-secret\"}"
        );
        assert_eq!(
            serde_json::to_string(&handle).expect("serialize handle"),
            "{\"schema\":\"rapidlm.auth.secret_aware_value\",\"schema_version\":1,\"kind\":\"handle\",\"ref\":{\"schema\":\"rapidlm.auth.secret_ref\",\"schema_version\":1,\"id\":\"01234567-89ab-cdef-0123-456789abcdef\",\"alias\":\"env:CANARY\"}}"
        );
    }

    #[test]
    fn expose_requires_matching_broker_token_and_target() {
        let value = sample_value();
        let target = sample_target();
        let mut token = SecretBrokerToken::issue(sample_ref(), target.clone());
        let exposed = value
            .expose(&mut token, &target)
            .expect("authorized expose");
        assert_eq!(exposed.as_str().expect("utf8"), CANARY);
        assert!(token.is_spent());
        assert_eq!(
            format!("{exposed:?}"),
            "ExposedSecret { redacted: true, byte_len: 42 }"
        );
        assert_no_canary("ExposedSecret Debug", &format!("{exposed:?}"));
    }

    #[test]
    fn expose_rejects_spent_token_reuse() {
        let value = sample_value();
        let target = sample_target();
        let mut token = SecretBrokerToken::issue(sample_ref(), target.clone());
        let _exposed = value.expose(&mut token, &target).expect("first expose");
        let err = value
            .expose(&mut token, &target)
            .expect_err("spent token must not expose again");
        assert_eq!(err, SecretError::ExposeUnauthorized);
        assert_eq!(err.code(), "auth.secret_expose_unauthorized");
        assert!(!err.retryable());
        assert_no_canary("error Display", &err.to_string());
    }

    #[test]
    fn expose_rejects_ref_mismatch_bypass() {
        let value = sample_value();
        let target = sample_target();
        let other = SecretRef::from_alias("env:OTHER").expect("other handle");
        let mut token = SecretBrokerToken::issue(other, target.clone());
        let err = value
            .expose(&mut token, &target)
            .expect_err("foreign token must not expose");
        assert_eq!(err, SecretError::ExposeUnauthorized);
        assert!(!token.is_spent());
    }

    #[test]
    fn expose_rejects_target_mismatch_bypass() {
        let value = sample_value();
        let expected = sample_target();
        let presented = SecretTarget::new("provider.transport").expect("other target");
        let mut token = SecretBrokerToken::issue(sample_ref(), expected);
        let err = value
            .expose(&mut token, &presented)
            .expect_err("target mismatch must deny");
        assert_eq!(err, SecretError::TargetMismatch);
        assert_eq!(err.code(), "auth.secret_target_mismatch");
        assert!(!token.is_spent());
    }

    #[test]
    fn expose_rejects_cancelled_token() {
        let value = sample_value();
        let target = sample_target();
        let mut token = SecretBrokerToken::issue(sample_ref(), target.clone());
        token.cancel();
        let err = value
            .expose(&mut token, &target)
            .expect_err("cancelled token must deny");
        assert_eq!(err, SecretError::Cancelled);
        assert_eq!(err.code(), "auth.cancelled");
        assert!(!token.is_spent());
    }

    #[test]
    fn construction_rejects_empty_and_oversized_secret() {
        assert_eq!(
            SecretValue::from_bytes(sample_ref(), Vec::new()).expect_err("empty"),
            SecretError::Empty
        );
        let requested = MAX_SECRET_BYTES + 1;
        let err = SecretValue::from_bytes(sample_ref(), vec![b'x'; requested])
            .expect_err("oversized secret");
        assert_eq!(
            err,
            SecretError::BoundExceeded {
                limit: MAX_SECRET_BYTES,
                requested,
            }
        );
    }

    #[test]
    fn secret_ref_rejects_invalid_alias_and_cleartext_dump() {
        assert_eq!(
            SecretRef::from_alias("").expect_err("empty alias"),
            SecretError::InvalidRef
        );
        assert_eq!(
            SecretRef::from_alias("env:API KEY").expect_err("whitespace"),
            SecretError::InvalidRef
        );
        assert_eq!(
            SecretRef::from_alias("-----BEGIN PRIVATE KEY-----").expect_err("pem-like alias"),
            SecretError::InvalidRef
        );
        let too_long = "a".repeat(MAX_SECRET_REF_ALIAS_BYTES + 1);
        assert_eq!(
            SecretRef::from_alias(&too_long).expect_err("oversize alias"),
            SecretError::BoundExceeded {
                limit: MAX_SECRET_REF_ALIAS_BYTES,
                requested: too_long.len(),
            }
        );
        assert!(SecretRef::from_id("01234567-89AB-CDEF-0123-456789ABCDEF").is_ok());
        assert!(SecretRef::from_id("not-a-uuid").is_err());
    }

    #[test]
    fn secret_value_deserialize_fails_closed() {
        let leaked = format!("{{\"value\":\"{CANARY}\"}}");
        let err = serde_json::from_str::<SecretValue>(&leaked).expect_err("must reject");
        assert!(err.to_string().contains("cannot be deserialized"));
        assert_no_canary("deserialize error", &err.to_string());
    }

    #[test]
    fn secret_broker_token_debug_omits_plaintext() {
        let mut token = SecretBrokerToken::issue(sample_ref(), sample_target());
        let rendered = format!("{token:?}");
        assert_no_canary("token Debug", &rendered);
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn error_messages_never_include_secret_material() {
        for err in [
            SecretError::ExposeUnauthorized,
            SecretError::TargetMismatch,
            SecretError::Cancelled,
            SecretError::Empty,
            SecretError::InvalidRef,
            SecretError::DeserializeRejected,
        ] {
            let rendered = format!("{err:?} {err} {}", err.code());
            assert_no_canary("SecretError", &rendered);
        }
    }
}
