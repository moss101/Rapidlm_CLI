//! Host-owned secret/credential brokerage (P4-033).
//!
//! [`SecretBroker`] composes the existing [`CredentialStore`], [`SecretBrokerToken`]
//! and [`EnvIdentity`] primitives into one path: the model receives only an
//! opaque [`ScopedSecret`] handle (never plaintext); resolution happens at the
//! executor/provider boundary via [`SecretBroker::open`]; the requested use is
//! bound to a secret target and the issuing environment; and the underlying
//! one-use token is spent on the first expose so an ephemeral credential cannot
//! be replayed. No second credential store is introduced.
//!
//! Secrets are never written to Event Ledger, traces, telemetry, logs or
//! model-visible tool output: [`ScopedSecret`] carries only handles and a
//! non-secret `EnvIdentity`, and its Debug/Display are redacted.

use std::fmt::{self, Debug, Formatter};

use crate::env_identity::EnvIdentity;
use crate::secret::{
    ExposedSecret, SecretBrokerToken, SecretError, SecretRef, SecretTarget, SecretValue,
};
use crate::store::{CancellationToken, CredentialStore, StoreError};

/// Host-owned broker. One instance = one environment identity.
pub struct SecretBroker<'a> {
    store: &'a dyn CredentialStore,
    environment: EnvIdentity,
}

/// Opaque, target-scoped, environment-bound secret handle. Never carries plaintext.
///
/// Deliberately not `Clone`: the one-use token must not be duplicated.
pub struct ScopedSecret {
    refer: SecretRef,
    target: SecretTarget,
    environment: EnvIdentity,
    token: SecretBrokerToken,
}

/// A resolved, still-unexposed credential owned at the executor boundary.
pub struct BoundSecret<'a> {
    scoped: &'a mut ScopedSecret,
    value: SecretValue,
}

impl SecretBroker<'_> {
    /// Bind a broker to a store and the environment it resolves credentials for.
    pub fn new(store: &dyn CredentialStore, environment: EnvIdentity) -> SecretBroker<'_> {
        SecretBroker { store, environment }
    }

    pub fn environment(&self) -> &EnvIdentity {
        &self.environment
    }

    /// Mint a target-scoped, environment-bound, one-use handle for `refer`.
    ///
    /// No store lookup and no plaintext occurs here; the model may hold the
    /// returned [`ScopedSecret`] without exposing a secret.
    pub fn issue(&self, refer: SecretRef, target: SecretTarget) -> ScopedSecret {
        let token = SecretBrokerToken::issue(refer.clone(), target.clone());
        ScopedSecret {
            refer,
            target,
            environment: self.environment.clone(),
            token,
        }
    }

    /// Resolve `scoped` at the executor/provider boundary.
    ///
    /// Fails closed if `scoped` was issued for a different environment, and
    /// fails if the store no longer holds the handle. The caller owns the
    /// returned [`BoundSecret`], which exposes plaintext exactly once.
    pub fn open<'a>(
        &self,
        scoped: &'a mut ScopedSecret,
        cancel: &CancellationToken,
    ) -> Result<BoundSecret<'a>, StoreError> {
        if scoped.environment != self.environment {
            return Err(StoreError::Secret(SecretError::ExposeUnauthorized));
        }
        cancel.check()?;
        let value = self.store.get(scoped.refer(), cancel)?;
        Ok(BoundSecret { scoped, value })
    }
}

impl ScopedSecret {
    pub fn refer(&self) -> &SecretRef {
        &self.refer
    }

    pub fn target(&self) -> &SecretTarget {
        &self.target
    }

    pub fn environment(&self) -> &EnvIdentity {
        &self.environment
    }
}

impl BoundSecret<'_> {
    /// Render the secret plaintext for this exact target, spending the token.
    ///
    /// A second call after a successful expose fails closed.
    pub fn expose(&mut self, target: &SecretTarget) -> Result<ExposedSecret<'_>, SecretError> {
        self.value.expose(&mut self.scoped.token, target)
    }

    pub fn secret_ref(&self) -> &SecretRef {
        self.value.secret_ref()
    }
}

impl Debug for ScopedSecret {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopedSecret")
            .field("redacted", &true)
            .field("target", &self.target.domain())
            .field("environment", &self.environment)
            .finish()
    }
}

impl fmt::Display for ScopedSecret {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("ScopedSecret(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{CredentialKind, CredentialPut, InMemoryCredentialStore};

    fn refer() -> SecretRef {
        SecretRef::from_id("018f3c8a-7e2b-7a10-8c4d-0123456789ab").expect("refer")
    }

    fn env(uid: u32) -> EnvIdentity {
        EnvIdentity::from_parts(uid, [("HOME".to_owned(), "/home/u".to_owned())])
    }

    fn target(domain: &str) -> SecretTarget {
        SecretTarget::new(domain).expect("target")
    }

    fn store_with(value: &[u8]) -> InMemoryCredentialStore {
        let store = InMemoryCredentialStore::new();
        let put = CredentialPut::persist(
            SecretValue::from_bytes(refer(), value.to_vec()).expect("value"),
            CredentialKind::Generic,
        );
        store.put(put, &CancellationToken::new()).expect("put");
        store
    }

    #[test]
    fn model_gets_a_handle_and_plaintext_resolves_and_spends_once() {
        let store = store_with(b"tok-abc");
        let broker = SecretBroker::new(&store, env(1000));
        let mut scoped = broker.issue(refer(), target("sandbox"));

        // Debug/Display never leak the secret bytes or their raw environment.
        let dbg = format!("{scoped:?}");
        let disp = format!("{scoped}");
        assert!(!dbg.contains("tok-abc"));
        assert!(!dbg.contains("sk-"));
        assert!(disp.contains("redacted"));

        let mut bound = broker
            .open(&mut scoped, &CancellationToken::new())
            .expect("open");
        let exposed = bound.expose(&target("sandbox")).expect("expose");
        assert_eq!(exposed.as_bytes(), b"tok-abc");

        // Second expose of the same token fails closed.
        assert!(bound.expose(&target("sandbox")).is_err());
    }

    #[test]
    fn credential_cannot_cross_environment_identity() {
        let store = store_with(b"tok-xyz");
        let broker_a = SecretBroker::new(&store, env(1000));
        let broker_b = SecretBroker::new(&store, env(2000));

        let mut scoped = broker_a.issue(refer(), target("sandbox"));
        assert!(
            broker_b
                .open(&mut scoped, &CancellationToken::new())
                .is_err()
        );
        // Same-environment open still works.
        let mut bound = broker_a
            .open(&mut scoped, &CancellationToken::new())
            .expect("open a");
        assert_eq!(
            bound.expose(&target("sandbox")).expect("expose").as_bytes(),
            b"tok-xyz"
        );
    }

    #[test]
    fn target_mismatch_refuses_expose_and_keeps_token_usable_for_correct_target() {
        let store = store_with(b"tok");
        let broker = SecretBroker::new(&store, env(1000));
        let mut scoped = broker.issue(refer(), target("sandbox"));
        let mut bound = broker
            .open(&mut scoped, &CancellationToken::new())
            .expect("open");

        assert_eq!(
            bound.expose(&target("other")).expect_err("wrong target"),
            SecretError::TargetMismatch
        );
        // A failed expose does not spend the token; the correct target works.
        assert_eq!(
            bound.expose(&target("sandbox")).expect("right").as_bytes(),
            b"tok"
        );
    }

    #[test]
    fn broker_never_introduces_a_second_store_authority() {
        // The broker only reads from the single CredentialStore; a credential
        // must already exist there for open() to succeed.
        let store = InMemoryCredentialStore::new();
        let broker = SecretBroker::new(&store, env(1000));
        let mut scoped = broker.issue(refer(), target("sandbox"));
        assert!(broker.open(&mut scoped, &CancellationToken::new()).is_err());
        let _ = CredentialKind::Generic;
    }

    #[test]
    fn expose_succeeds_when_the_issued_ref_is_narrower_than_the_stored_one() {
        // The store deliberately does loose id-or-alias matching (see
        // `SecretRef::matches`) — a secret stored under id+alias must still
        // resolve for a token issued against alias-only, since `open()`
        // already proved the store holds that exact handle. `expose()` must
        // agree with that same definition of "same handle" or a caller can
        // see `open()` succeed and then be denied by `expose()` for a
        // handle the store just confirmed it holds.
        let store = InMemoryCredentialStore::new();
        let stored_ref = SecretRef::from_id_and_alias(
            "018f3c8a-7e2b-7a10-8c4d-0123456789ab",
            "env:ANTHROPIC_API_KEY",
        )
        .expect("stored ref");
        let put = CredentialPut::persist(
            SecretValue::from_bytes(stored_ref, b"sk-secret".to_vec()).expect("value"),
            CredentialKind::Generic,
        );
        store.put(put, &CancellationToken::new()).expect("put");

        let broker = SecretBroker::new(&store, env(1000));
        let alias_only = SecretRef::from_alias("env:ANTHROPIC_API_KEY").expect("alias ref");
        let mut scoped = broker.issue(alias_only, target("sandbox"));
        let mut bound = broker
            .open(&mut scoped, &CancellationToken::new())
            .expect("open");
        assert_eq!(
            bound.expose(&target("sandbox")).expect("expose").as_bytes(),
            b"sk-secret"
        );
    }
}
