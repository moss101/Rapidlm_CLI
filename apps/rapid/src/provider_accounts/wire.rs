//! Request-time authorization for an explicitly bound provider account.
use super::{AccountError, AccountSpec, manager::AccountManager};
use auth::{CredentialKind, SecretRef};
use llm_router::{
    CancellationToken, EphemeralCredential, ProfileId, ProviderError, ProviderId, WireAuthorization,
};

/// A stable handle scoped to the registration, suitable for router replay and
/// credential ownership checks. It does not contain an access or refresh token.
pub fn credential_handle(spec: &AccountSpec) -> Result<SecretRef, AccountError> {
    SecretRef::from_alias(&format!(
        "provider-account:{}:{}",
        spec.id(),
        spec.binding()
    ))
    .map_err(|_| AccountError::InvalidConfig)
}

pub struct AccountWireAuth {
    manager: AccountManager,
    provider: ProviderId,
    profile: ProfileId,
    handle: SecretRef,
}
impl std::fmt::Debug for AccountWireAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountWireAuth")
            .field("provider", &self.provider)
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}
impl AccountWireAuth {
    /// The caller must also bind the inference resource and enforce managed
    /// policy before constructing this authorizer.
    pub fn new(
        manager: AccountManager,
        provider: ProviderId,
        profile: ProfileId,
    ) -> Result<Self, AccountError> {
        if manager.spec().definition().provider != provider.as_str() {
            return Err(AccountError::BindingMismatch);
        }
        let handle = credential_handle(manager.spec())?;
        Ok(Self {
            manager,
            provider,
            profile,
            handle,
        })
    }
}
impl WireAuthorization for AccountWireAuth {
    fn bearer_token(
        &self,
        credential: &EphemeralCredential,
        cancel: &CancellationToken,
    ) -> Result<String, ProviderError> {
        cancel.check()?;
        if credential.provider() != &self.provider
            || credential.profile() != &self.profile
            || credential.kind() != CredentialKind::OauthAccess
            || credential.secret_ref() != &self.handle
        {
            return Err(ProviderError::AuthFailed);
        }
        self.manager
            .access_token(cancel)
            .map_err(|error| match error {
                AccountError::Cancelled => ProviderError::Cancelled,
                AccountError::Transient | AccountError::Busy => ProviderError::Connection,
                _ => ProviderError::AuthFailed,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_oauth::Secret;
    use crate::provider_accounts::{
        net::{GrantEndpoint, GrantReply, GrantTransport},
        protocol::{GrantBody, Tokens},
        store::AccountStore,
        tests::spec,
    };
    use crate::provider_keychain::testing::MemoryKeychain;
    use auth::{CredentialPut, CredentialStore, InMemoryCredentialStore, SecretValue};
    use llm_router::{CredentialResolver, ProviderProfile};
    use std::sync::Arc;

    struct NoNetwork;
    impl GrantTransport for NoNetwork {
        fn post(
            &self,
            _: &AccountSpec,
            _: GrantEndpoint,
            _: &GrantBody,
            _: &CancellationToken,
        ) -> Result<GrantReply, AccountError> {
            panic!("an unexpired token must not contact the issuer")
        }
    }
    #[test]
    fn wire_authorizer_checks_profile_provider_kind_and_registration_handle_before_reading_account()
    {
        let root = std::env::temp_dir().join(format!(
            "rapid-account-wire-{}",
            ::protocol::SessionId::new()
        ));
        let spec = spec();
        let store = AccountStore::new(root.clone(), Arc::new(MemoryKeychain::default()));
        let cancel = CancellationToken::new();
        store
            .transaction(&spec, &cancel)
            .unwrap()
            .install(
                None,
                &Tokens {
                    access: Secret::new("wire-token-canary".into()),
                    refresh: None,
                    obtained_at: 1,
                    expires_at: None,
                    scopes: vec![],
                },
            )
            .unwrap();
        let manager = AccountManager::new(spec.clone(), store, Arc::new(NoNetwork));
        let provider = ProviderId::parse("openai-compatible").unwrap();
        let profile = ProfileId::parse("primary").unwrap();
        let auth = AccountWireAuth::new(manager, provider.clone(), profile).unwrap();
        let secrets = InMemoryCredentialStore::new();
        let resolve = |id: &str, handle: SecretRef, kind| {
            let value = SecretValue::from_str(handle.clone(), "placeholder").unwrap();
            secrets
                .put(
                    CredentialPut::ephemeral(value, kind)
                        .with_provider(provider.as_str())
                        .unwrap(),
                    &auth::CancellationToken::new(),
                )
                .unwrap();
            let profile = ProviderProfile::with_kind(id, provider.clone(), handle, kind).unwrap();
            CredentialResolver::new(&secrets)
                .resolve(&provider, &profile, &cancel)
                .unwrap()
        };
        let handle = credential_handle(&spec).unwrap();
        let right = resolve("primary", handle.clone(), CredentialKind::OauthAccess);
        assert_eq!(
            auth.bearer_token(&right, &cancel).unwrap(),
            "wire-token-canary"
        );
        let wrong_profile = resolve("other", handle, CredentialKind::OauthAccess);
        assert_eq!(
            auth.bearer_token(&wrong_profile, &cancel),
            Err(ProviderError::AuthFailed)
        );
        let other_handle = resolve(
            "primary",
            SecretRef::from_alias("provider-account:other").unwrap(),
            CredentialKind::OauthAccess,
        );
        assert_eq!(
            auth.bearer_token(&other_handle, &cancel),
            Err(ProviderError::AuthFailed)
        );
        let wrong_kind = resolve(
            "primary",
            SecretRef::from_alias("other-kind").unwrap(),
            CredentialKind::ProviderApiKey,
        );
        assert_eq!(
            auth.bearer_token(&wrong_kind, &cancel),
            Err(ProviderError::AuthFailed)
        );
        cancel.cancel();
        assert_eq!(
            auth.bearer_token(&right, &cancel),
            Err(ProviderError::Cancelled)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
