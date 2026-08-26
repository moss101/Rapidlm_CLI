//! Provider credential handle resolution.
//!
//! Config and model objects carry [`SecretRef`] handles only. Store lookup
//! yields an [`EphemeralCredential`] whose Debug/trace forms are redacted.
//! Auth failures map to [`ProviderError::AuthFailed`] (`provider.auth_failed`).

use std::fmt::{self, Debug, Display, Formatter};

use auth::{CancellationToken as StoreCancel, CredentialStore, MetadataQuery, StoreError};
use serde::de::{Deserializer, Error as DeError, IgnoredAny};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::provider::{CancellationToken, ProviderError, ProviderId};

/// Re-export so callers can build handle-only profiles without a second import.
pub use auth::{CredentialKind, SecretRef, SecretValue};

/// Wire schema name for [`ProviderProfile`].
pub const PROVIDER_PROFILE_SCHEMA: &str = "rapidlm.provider_profile";

/// Wire schema name for [`EphemeralCredential`] metadata (never plaintext).
pub const EPHEMERAL_CREDENTIAL_SCHEMA: &str = "rapidlm.ephemeral_credential";

/// v1 schema version for credential-resolver objects.
pub const CREDENTIAL_SCHEMA_VERSION: u16 = 1;

/// Maximum UTF-8 bytes for a provider profile identifier.
pub const MAX_PROFILE_ID_BYTES: usize = 64;

/// Bounded profile identifier (`default`, `work`, `eu-prod`).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ProfileId(String);

/// Handle-only provider profile. Never stores API-key plaintext.
#[derive(Clone, Eq, PartialEq)]
pub struct ProviderProfile {
    id: ProfileId,
    provider: ProviderId,
    credential: SecretRef,
    kind: CredentialKind,
}

/// Store-backed resolver. Transport asks for a handle; it does not read config plaintext.
pub struct CredentialResolver<'a> {
    store: &'a dyn CredentialStore,
}

/// Transport-scoped secret after store resolution. Debug/Display/serde omit bytes.
pub struct EphemeralCredential {
    provider: ProviderId,
    profile: ProfileId,
    kind: CredentialKind,
    value: SecretValue,
}

impl ProfileId {
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, ProviderError> {
        let raw = raw.as_ref();
        if raw.is_empty() {
            return Err(ProviderError::InvalidRequest);
        }
        if raw.len() > MAX_PROFILE_ID_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        let mut chars = raw.chars();
        let Some(first) = chars.next() else {
            return Err(ProviderError::InvalidRequest);
        };
        if !first.is_ascii_lowercase() {
            return Err(ProviderError::InvalidRequest);
        }
        let mut prev_sep = false;
        for ch in chars {
            let sep = ch == '-';
            if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || sep) || (sep && prev_sep) {
                return Err(ProviderError::InvalidRequest);
            }
            prev_sep = sep;
        }
        if raw.ends_with('-') {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ProviderProfile {
    /// Construct a handle-only profile. There is no plaintext constructor.
    pub fn new(
        id: impl AsRef<str>,
        provider: ProviderId,
        credential: SecretRef,
    ) -> Result<Self, ProviderError> {
        Self::with_kind(id, provider, credential, CredentialKind::ProviderApiKey)
    }

    pub fn with_kind(
        id: impl AsRef<str>,
        provider: ProviderId,
        credential: SecretRef,
        kind: CredentialKind,
    ) -> Result<Self, ProviderError> {
        if !is_provider_credential_kind(kind) {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self {
            id: ProfileId::parse(id)?,
            provider,
            credential,
            kind,
        })
    }

    pub fn id(&self) -> &ProfileId {
        &self.id
    }

    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }

    pub fn credential(&self) -> &SecretRef {
        &self.credential
    }

    pub fn kind(&self) -> CredentialKind {
        self.kind
    }
}

impl<'a> CredentialResolver<'a> {
    pub fn new(store: &'a dyn CredentialStore) -> Self {
        Self { store }
    }

    /// Resolve `profile`'s credential handle for `provider` from the auth store.
    ///
    /// Cross-provider handle use, missing records, and wrong kinds fail closed
    /// as [`ProviderError::AuthFailed`]. Cancellation is not remapped.
    pub fn resolve(
        &self,
        provider: &ProviderId,
        profile: &ProviderProfile,
        cancel: &CancellationToken,
    ) -> Result<EphemeralCredential, ProviderError> {
        cancel.check()?;
        if profile.provider() != provider {
            return Err(ProviderError::AuthFailed);
        }
        if !is_provider_credential_kind(profile.kind()) {
            return Err(ProviderError::AuthFailed);
        }

        let store_cancel = StoreCancel::new();
        let query = metadata_query(provider, profile.kind())?;
        if !self.handle_authorized(profile.credential(), &query, &store_cancel, cancel)? {
            return Err(ProviderError::AuthFailed);
        }

        cancel.check()?;
        let value = self
            .store
            .get(profile.credential(), &store_cancel)
            .map_err(map_store_error)?;

        // Re-check ownership after fetch so a swapped record cannot be returned.
        cancel.check()?;
        if !self.handle_authorized(value.secret_ref(), &query, &store_cancel, cancel)? {
            return Err(ProviderError::AuthFailed);
        }

        Ok(EphemeralCredential {
            provider: provider.clone(),
            profile: profile.id().clone(),
            kind: profile.kind(),
            value,
        })
    }

    fn handle_authorized(
        &self,
        handle: &SecretRef,
        query: &MetadataQuery,
        store_cancel: &StoreCancel,
        cancel: &CancellationToken,
    ) -> Result<bool, ProviderError> {
        cancel.check()?;
        let listed = self
            .store
            .list_metadata(query, store_cancel)
            .map_err(map_store_error)?;
        Ok(listed.iter().any(|meta| {
            is_provider_credential_kind(meta.kind())
                && meta.kind() == query.kind().unwrap_or(meta.kind())
                && refs_match(meta.secret_ref(), handle)
        }))
    }
}

impl EphemeralCredential {
    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }

    pub fn profile(&self) -> &ProfileId {
        &self.profile
    }

    pub fn kind(&self) -> CredentialKind {
        self.kind
    }

    pub fn secret_ref(&self) -> &SecretRef {
        self.value.secret_ref()
    }

    pub fn secret(&self) -> &SecretValue {
        &self.value
    }

    pub fn into_secret(self) -> SecretValue {
        self.value
    }

    pub fn byte_len(&self) -> usize {
        self.value.len()
    }
}

impl Debug for ProviderProfile {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderProfile")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("credential", &self.credential)
            .field("kind", &self.kind)
            .finish()
    }
}

impl Display for ProviderProfile {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ProviderProfile(id={}, provider={}, credential={}, kind={:?})",
            self.id.as_str(),
            self.provider.as_str(),
            self.credential,
            self.kind
        )
    }
}

impl Debug for EphemeralCredential {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("EphemeralCredential")
            .field("provider", &self.provider)
            .field("profile", &self.profile)
            .field("kind", &self.kind)
            .field("redacted", &true)
            .field("byte_len", &self.value.len())
            .finish()
    }
}

impl Display for EphemeralCredential {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("EphemeralCredential(<redacted>)")
    }
}

impl Display for ProfileId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ProfileId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProfileId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        ProfileId::parse(raw).map_err(DeError::custom)
    }
}

impl Serialize for ProviderProfile {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ProviderProfile", 6)?;
        state.serialize_field("schema", PROVIDER_PROFILE_SCHEMA)?;
        state.serialize_field("schema_version", &CREDENTIAL_SCHEMA_VERSION)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("provider", self.provider.as_str())?;
        state.serialize_field("credential", &self.credential)?;
        state.serialize_field("kind", &self.kind)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ProviderProfile {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(default)]
            schema: Option<String>,
            #[serde(default)]
            schema_version: Option<u16>,
            id: String,
            provider: String,
            credential: SecretRef,
            kind: CredentialKind,
            #[serde(default)]
            value: Option<IgnoredAny>,
            #[serde(default)]
            secret: Option<IgnoredAny>,
            #[serde(default)]
            password: Option<IgnoredAny>,
            #[serde(default)]
            api_key: Option<IgnoredAny>,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.value.is_some()
            || wire.secret.is_some()
            || wire.password.is_some()
            || wire.api_key.is_some()
        {
            return Err(DeError::custom(
                "provider profile cannot carry credential plaintext",
            ));
        }
        if let Some(schema) = wire.schema.as_deref()
            && schema != PROVIDER_PROFILE_SCHEMA
        {
            return Err(DeError::custom("unsupported ProviderProfile schema"));
        }
        if let Some(version) = wire.schema_version
            && version != CREDENTIAL_SCHEMA_VERSION
        {
            return Err(DeError::custom(
                "unsupported ProviderProfile schema_version",
            ));
        }
        let provider = ProviderId::parse(wire.provider).map_err(DeError::custom)?;
        ProviderProfile::with_kind(wire.id, provider, wire.credential, wire.kind)
            .map_err(DeError::custom)
    }
}

impl Serialize for EphemeralCredential {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("EphemeralCredential", 8)?;
        state.serialize_field("schema", EPHEMERAL_CREDENTIAL_SCHEMA)?;
        state.serialize_field("schema_version", &CREDENTIAL_SCHEMA_VERSION)?;
        state.serialize_field("provider", self.provider.as_str())?;
        state.serialize_field("profile", &self.profile)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("redacted", &true)?;
        state.serialize_field("byte_len", &self.value.len())?;
        state.serialize_field("ref", self.value.secret_ref())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for EphemeralCredential {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let _ = IgnoredAny::deserialize(deserializer)?;
        Err(DeError::custom(
            "ephemeral credentials cannot be deserialized; resolve a SecretRef",
        ))
    }
}

fn is_provider_credential_kind(kind: CredentialKind) -> bool {
    matches!(
        kind,
        CredentialKind::ProviderApiKey | CredentialKind::OauthRefresh | CredentialKind::OauthAccess
    )
}

fn metadata_query(
    provider: &ProviderId,
    kind: CredentialKind,
) -> Result<MetadataQuery, ProviderError> {
    MetadataQuery::new()
        .with_provider(provider.as_str())
        .map(|query| query.with_kind(kind))
        .map_err(map_store_error)
}

fn refs_match(stored: &SecretRef, query: &SecretRef) -> bool {
    if stored == query {
        return true;
    }
    if let (Some(stored_id), Some(query_id)) = (stored.id(), query.id())
        && stored_id == query_id
    {
        return true;
    }
    if let (Some(stored_alias), Some(query_alias)) = (stored.alias(), query.alias())
        && stored_alias == query_alias
    {
        return true;
    }
    false
}

fn map_store_error(err: StoreError) -> ProviderError {
    match err {
        StoreError::Cancelled => ProviderError::Cancelled,
        StoreError::BoundExceeded { .. } => ProviderError::BoundExceeded,
        StoreError::LockPoisoned => ProviderError::Permanent,
        StoreError::NotFound
        | StoreError::PersistenceBlocked { .. }
        | StoreError::InvalidHandle
        | StoreError::InvalidMetadata
        | StoreError::InvalidProvider
        | StoreError::Secret(_) => ProviderError::AuthFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::{CredentialPut, InMemoryCredentialStore};
    use protocol::{ErrorCode, TraceId};

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-9f3c2a";
    const REF_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";
    const REF_ALIAS: &str = "env:OPENAI_API_KEY";
    const OTHER_ID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    const TRACE: &str = "0193e0f6-6c3a-7d1e-8b2c-4d5e6f708192";

    fn sample_ref() -> SecretRef {
        SecretRef::from_id_and_alias(REF_ID, REF_ALIAS).expect("valid test SecretRef")
    }

    fn openai() -> ProviderId {
        ProviderId::parse("openai").expect("provider")
    }

    fn anthropic() -> ProviderId {
        ProviderId::parse("anthropic").expect("provider")
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn store_live() -> StoreCancel {
        StoreCancel::new()
    }

    fn profile_for(provider: ProviderId, refer: SecretRef) -> ProviderProfile {
        ProviderProfile::new("default", provider, refer).expect("profile")
    }

    fn put_provider(
        store: &InMemoryCredentialStore,
        provider: &str,
        refer: SecretRef,
        secret: &str,
        kind: CredentialKind,
    ) {
        let value = SecretValue::from_str(refer, secret).expect("secret");
        let put = CredentialPut::ephemeral(value, kind)
            .with_provider(provider)
            .expect("provider");
        store.put(put, &store_live()).expect("put");
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

    fn assert_auth_failed(err: ProviderError) {
        assert_eq!(err, ProviderError::AuthFailed);
        assert_eq!(err.code(), Some(ErrorCode::ProviderAuthFailed));
        assert_eq!(
            ErrorCode::ProviderAuthFailed.as_str(),
            "provider.auth_failed"
        );
        assert!(!err.is_retryable());
        let trace = TRACE.parse::<TraceId>().expect("trace");
        let api = err.into_api_error(trace).expect("api");
        assert_eq!(api.code(), ErrorCode::ProviderAuthFailed);
        assert!(!api.retryable());
        assert_no_canary("auth error", &format!("{err:?} {err}"));
    }

    #[test]
    fn resolve_returns_ephemeral_credential_without_config_plaintext() {
        let store = InMemoryCredentialStore::new();
        put_provider(
            &store,
            "openai",
            sample_ref(),
            CANARY,
            CredentialKind::ProviderApiKey,
        );
        let profile = profile_for(openai(), sample_ref());
        let resolved = CredentialResolver::new(&store)
            .resolve(&openai(), &profile, &live())
            .expect("resolve");

        assert_eq!(resolved.provider(), &openai());
        assert_eq!(resolved.profile().as_str(), "default");
        assert_eq!(resolved.kind(), CredentialKind::ProviderApiKey);
        assert_eq!(resolved.byte_len(), CANARY.len());
        assert!(refs_match(resolved.secret_ref(), &sample_ref()));

        let profile_json = serde_json::to_string(&profile).expect("profile json");
        assert!(profile_json.contains("\"credential\""));
        assert!(!profile_json.contains(CANARY));
        assert!(!profile_json.contains("\"api_key\""));
        assert!(!profile_json.contains("\"value\""));
        assert_no_canary("profile serde", &profile_json);
        assert_no_canary("profile Debug", &format!("{profile:?}"));
        assert_no_canary("profile Display", &format!("{profile}"));
    }

    #[test]
    fn ephemeral_credential_debug_display_serde_redact_plaintext() {
        let store = InMemoryCredentialStore::new();
        put_provider(
            &store,
            "openai",
            sample_ref(),
            CANARY,
            CredentialKind::ProviderApiKey,
        );
        let profile = profile_for(openai(), sample_ref());
        let resolved = CredentialResolver::new(&store)
            .resolve(&openai(), &profile, &live())
            .expect("resolve");

        let debug = format!("{resolved:?}");
        let display = format!("{resolved}");
        let json = serde_json::to_string(&resolved).expect("serialize");

        assert!(debug.contains("redacted: true"));
        assert!(debug.contains(&format!("byte_len: {}", CANARY.len())));
        assert_eq!(display, "EphemeralCredential(<redacted>)");
        assert!(json.contains("\"redacted\":true"));
        assert!(!json.contains(CANARY));
        assert_no_canary("Debug", &debug);
        assert_no_canary("Display", &display);
        assert_no_canary("serde", &json);
        assert_no_canary("secret Debug", &format!("{:?}", resolved.secret()));
    }

    #[test]
    fn missing_credential_maps_to_provider_auth_failed() {
        let store = InMemoryCredentialStore::new();
        let profile = profile_for(openai(), sample_ref());
        let err = CredentialResolver::new(&store)
            .resolve(&openai(), &profile, &live())
            .expect_err("missing");
        assert_auth_failed(err);
    }

    #[test]
    fn cross_provider_handle_is_denied() {
        let store = InMemoryCredentialStore::new();
        put_provider(
            &store,
            "anthropic",
            sample_ref(),
            CANARY,
            CredentialKind::ProviderApiKey,
        );
        let profile = profile_for(openai(), sample_ref());
        let err = CredentialResolver::new(&store)
            .resolve(&openai(), &profile, &live())
            .expect_err("cross-provider");
        assert_auth_failed(err);
    }

    #[test]
    fn resolve_provider_argument_must_match_profile() {
        let store = InMemoryCredentialStore::new();
        put_provider(
            &store,
            "openai",
            sample_ref(),
            CANARY,
            CredentialKind::ProviderApiKey,
        );
        let profile = profile_for(openai(), sample_ref());
        let err = CredentialResolver::new(&store)
            .resolve(&anthropic(), &profile, &live())
            .expect_err("provider mismatch");
        assert_auth_failed(err);
    }

    #[test]
    fn non_provider_kind_cannot_be_resolved() {
        let store = InMemoryCredentialStore::new();
        put_provider(
            &store,
            "openai",
            sample_ref(),
            CANARY,
            CredentialKind::DaemonToken,
        );
        let profile = profile_for(openai(), sample_ref());
        let err = CredentialResolver::new(&store)
            .resolve(&openai(), &profile, &live())
            .expect_err("daemon token");
        assert_auth_failed(err);
    }

    #[test]
    fn profile_rejects_non_provider_kind_at_construction() {
        let err = ProviderProfile::with_kind(
            "default",
            openai(),
            sample_ref(),
            CredentialKind::WorkerCert,
        )
        .expect_err("worker cert");
        assert_eq!(err, ProviderError::InvalidRequest);
    }

    #[test]
    fn sibling_provider_secret_is_not_selected() {
        let store = InMemoryCredentialStore::new();
        put_provider(
            &store,
            "anthropic",
            sample_ref(),
            CANARY,
            CredentialKind::ProviderApiKey,
        );
        let other = SecretRef::from_id(OTHER_ID).expect("other");
        put_provider(
            &store,
            "openai",
            other.clone(),
            "other-provider-secret-NOT-THE-CANARY",
            CredentialKind::ProviderApiKey,
        );
        let profile = profile_for(openai(), sample_ref());
        let err = CredentialResolver::new(&store)
            .resolve(&openai(), &profile, &live())
            .expect_err("wrong handle");
        assert_auth_failed(err);

        let ok_profile = profile_for(openai(), other.clone());
        let resolved = CredentialResolver::new(&store)
            .resolve(&openai(), &ok_profile, &live())
            .expect("openai handle");
        assert!(refs_match(resolved.secret_ref(), &other));
        assert_ne!(resolved.byte_len(), CANARY.len());
        assert_no_canary("sibling Debug", &format!("{resolved:?}"));
    }

    #[test]
    fn cancellation_is_not_remapped_to_auth_failed() {
        let store = InMemoryCredentialStore::new();
        put_provider(
            &store,
            "openai",
            sample_ref(),
            CANARY,
            CredentialKind::ProviderApiKey,
        );
        let profile = profile_for(openai(), sample_ref());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = CredentialResolver::new(&store)
            .resolve(&openai(), &profile, &cancel)
            .expect_err("cancelled");
        assert_eq!(err, ProviderError::Cancelled);
        assert!(err.code().is_none());
        assert_no_canary("cancelled", &format!("{err}"));
    }

    #[test]
    fn profile_deserialize_rejects_plaintext_fields() {
        let leaked = format!(
            r#"{{"schema":"{PROVIDER_PROFILE_SCHEMA}","schema_version":1,"id":"default","provider":"openai","credential":{{"schema":"rapidlm.auth.secret_ref","schema_version":1,"id":"{REF_ID}","alias":"{REF_ALIAS}"}},"kind":"provider_api_key","api_key":"{CANARY}"}}"#
        );
        let err = serde_json::from_str::<ProviderProfile>(&leaked).expect_err("must reject");
        assert!(
            err.to_string().contains("plaintext") || err.to_string().contains("unknown field"),
            "unexpected reject: {err}"
        );
        assert_no_canary("deserialize error", &err.to_string());
    }

    #[test]
    fn ephemeral_credential_deserialize_fails_closed() {
        let leaked = format!(r#"{{"value":"{CANARY}"}}"#);
        let err = serde_json::from_str::<EphemeralCredential>(&leaked).expect_err("must reject");
        assert!(err.to_string().contains("cannot be deserialized"));
        assert_no_canary("ephemeral deserialize", &err.to_string());
    }

    #[test]
    fn profile_id_bounds() {
        assert!(ProfileId::parse("default").is_ok());
        assert_eq!(
            ProfileId::parse("").expect_err("empty"),
            ProviderError::InvalidRequest
        );
        assert_eq!(
            ProfileId::parse("Default").expect_err("case"),
            ProviderError::InvalidRequest
        );
        let too_long = "a".repeat(MAX_PROFILE_ID_BYTES + 1);
        assert_eq!(
            ProfileId::parse(&too_long).expect_err("bound"),
            ProviderError::BoundExceeded
        );
    }

    #[test]
    fn alias_only_handle_matches_stored_id_and_alias() {
        let store = InMemoryCredentialStore::new();
        put_provider(
            &store,
            "openai",
            sample_ref(),
            CANARY,
            CredentialKind::ProviderApiKey,
        );
        let alias_only = SecretRef::from_alias(REF_ALIAS).expect("alias");
        let profile = profile_for(openai(), alias_only);
        let resolved = CredentialResolver::new(&store)
            .resolve(&openai(), &profile, &live())
            .expect("alias resolve");
        assert_eq!(resolved.byte_len(), CANARY.len());
        assert_no_canary("alias resolve", &format!("{resolved:?}"));
    }
}
