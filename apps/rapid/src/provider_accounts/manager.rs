//! Locked refresh/rotation and revocation. Never starts login or discovers accounts.
use super::{
    AccountError as Error, AccountSpec,
    net::{GrantEndpoint, GrantTransport},
    protocol::{parse_tokens, refresh_request, revoke_request},
    store::{AccountRecord, AccountStore},
};
use llm_router::CancellationToken;
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
#[derive(Clone)]
pub struct AccountManager {
    spec: AccountSpec,
    store: AccountStore,
    transport: Arc<dyn GrantTransport>,
    clock: Arc<dyn Fn() -> Result<u64, Error> + Send + Sync>,
}
impl std::fmt::Debug for AccountManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountManager")
            .field("account", &self.spec)
            .finish_non_exhaustive()
    }
}
impl AccountManager {
    pub fn new(spec: AccountSpec, store: AccountStore, transport: Arc<dyn GrantTransport>) -> Self {
        Self {
            spec,
            store,
            transport,
            clock: Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .map_err(|_| Error::Storage)
            }),
        }
    }
    #[cfg(test)]
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> Result<u64, Error> + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }
    pub fn spec(&self) -> &AccountSpec {
        &self.spec
    }
    pub fn access_token(&self, cancel: &CancellationToken) -> Result<String, Error> {
        self.resolve(false, cancel)
    }
    pub fn refresh(&self, cancel: &CancellationToken) -> Result<(), Error> {
        self.resolve(true, cancel).map(|_| ())
    }
    fn resolve(&self, force: bool, cancel: &CancellationToken) -> Result<String, Error> {
        let tx = self.store.transaction(&self.spec, cancel)?;
        let (record, tokens, interrupted) = match tx.load() {
            Ok((record, tokens)) => (record, tokens, false),
            Err(Error::Interrupted) => {
                let intent = tx.record()?.ok_or(Error::Missing)?;
                let tokens = tx.interrupted_tokens(&intent)?;
                (intent, tokens, true)
            }
            Err(error) => return Err(error),
        };
        let now = (self.clock)()?;
        if !interrupted
            && !force
            && (!tokens.refresh_due(now) || tokens.refresh.is_none())
            && tokens.usable(now)
        {
            cancel.check().map_err(|_| Error::Cancelled)?;
            return Ok(tokens.access.expose().to_owned());
        }
        let request = refresh_request(&self.spec, &tokens)?;
        let intent = if interrupted {
            record
        } else {
            tx.begin_refresh(record.generation)?
        };
        let reply = self
            .transport
            .post(&self.spec, GrantEndpoint::Token, &request, cancel)?;
        let result = parse_tokens(
            &self.spec,
            reply.status,
            &reply.body,
            (self.clock)()?,
            Some(&tokens),
        );
        let granted = match result {
            Ok(granted) => granted,
            Err(error @ (Error::Revoked | Error::Unauthorized)) => {
                tx.revoke_after_remote(intent.generation)?;
                return Err(error);
            }
            // Preserve the intent across transient/uncertain failures. A fresh
            // manager retries under the same lock, or adopts a staged winner.
            Err(error) => return Err(error),
        };
        tx.finish_refresh(&intent, &granted)?;
        cancel.check().map_err(|_| Error::Cancelled)?;
        if !granted.usable((self.clock)()?) {
            return Err(Error::Expired);
        }
        Ok(granted.access.expose().to_owned())
    }
    /// Explicit logout revokes local access even if the optional remote endpoint
    /// fails. The remote error is reported after the durable tombstone commits.
    pub fn revoke(&self, remote: bool, cancel: &CancellationToken) -> Result<AccountRecord, Error> {
        let tx = self.store.transaction(&self.spec, cancel)?;
        let current = tx.record()?.ok_or(Error::Missing)?;
        if matches!(
            current.state,
            super::store::AccountState::Revoked | super::store::AccountState::Revoking { .. }
        ) {
            let revoked = tx.revoke_after_remote(current.generation)?;
            // The local tombstone has retired the credential; it cannot be
            // presented to the issuer for a new remote revocation attempt.
            return if remote {
                Err(Error::Missing)
            } else {
                Ok(revoked)
            };
        }
        // Local logout must not depend on reading a working keychain. Commit
        // the tombstone first even when the saved bundle is missing or corrupt.
        if !remote {
            return tx.revoke_after_remote(current.generation);
        }
        let (record, tokens) = match tx.load() {
            Ok(pair) => pair,
            Err(Error::Interrupted) => {
                let record = tx.record()?.ok_or(Error::Missing)?;
                match tx.interrupted_tokens(&record) {
                    Ok(tokens) => (record, tokens),
                    Err(error) => {
                        tx.revoke_after_remote(record.generation)?;
                        return Err(error);
                    }
                }
            }
            Err(error) => {
                tx.revoke_after_remote(current.generation)?;
                return Err(error);
            }
        };
        let result = if remote && self.spec.definition().revoke_url.is_none() {
            Err(Error::InvalidConfig)
        } else if remote {
            revoke_request(&self.spec, &tokens)
                .and_then(|body| {
                    self.transport
                        .post(&self.spec, GrantEndpoint::Revoke, &body, cancel)
                })
                .and_then(|r| {
                    if (200..300).contains(&r.status) {
                        Ok(())
                    } else {
                        Err(if r.status >= 500 {
                            Error::Transient
                        } else if matches!(r.status, 401 | 403) {
                            Error::Unauthorized
                        } else {
                            Error::InvalidReply
                        })
                    }
                })
        } else {
            Ok(())
        };
        let revoked = tx.revoke_after_remote(record.generation)?;
        result?;
        Ok(revoked)
    }
}

#[cfg(test)]
mod tests {
    use super::super::net::GrantReply;
    use super::*;
    use crate::mcp_oauth::Secret;
    use crate::{
        provider_accounts::{
            protocol::{Tokens, parse_tokens},
            tests::spec,
        },
        provider_keychain::testing::MemoryKeychain,
    };
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    struct Script {
        replies: Mutex<Vec<Result<GrantReply, Error>>>,
        calls: AtomicUsize,
    }
    impl GrantTransport for Script {
        fn post(
            &self,
            _: &AccountSpec,
            _: GrantEndpoint,
            _: &super::super::protocol::GrantBody,
            cancel: &CancellationToken,
        ) -> Result<GrantReply, Error> {
            cancel.check().map_err(|_| Error::Cancelled)?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.replies.lock().unwrap().remove(0)
        }
    }
    fn store() -> (std::path::PathBuf, AccountStore) {
        let root = std::env::temp_dir().join(format!(
            "rapid-account-manager-{}",
            ::protocol::SessionId::new()
        ));
        (
            root.clone(),
            AccountStore::new(root, Arc::new(MemoryKeychain::default())),
        )
    }
    fn token() -> Tokens {
        Tokens {
            access: Secret::new("old-access-canary".to_owned()),
            refresh: Some(Secret::new("old-refresh-canary".to_owned())),
            obtained_at: 1000,
            expires_at: Some(4600),
            scopes: vec!["inference".to_owned()],
        }
    }
    fn reply(status: u16, body: &str) -> Result<GrantReply, Error> {
        Ok(GrantReply {
            status,
            body: body.as_bytes().to_vec(),
        })
    }
    fn script(replies: Vec<Result<GrantReply, Error>>) -> Arc<Script> {
        Arc::new(Script {
            replies: Mutex::new(replies),
            calls: AtomicUsize::new(0),
        })
    }
    #[test]
    fn account_manager_reloads_durable_winners_and_performs_one_refresh_for_concurrent_consumers() {
        let (root, store) = store();
        let spec = spec();
        let cancel = CancellationToken::new();
        {
            store
                .transaction(&spec, &cancel)
                .unwrap()
                .install(None, &token())
                .unwrap();
        }
        let network = script(vec![reply(
            200,
            r#"{"access_token":"new-access-canary","refresh_token":"new-refresh-canary","expires_in":3600}"#,
        )]);
        let mut threads = Vec::new();
        for _ in 0..6 {
            let manager = AccountManager::new(spec.clone(), store.clone(), network.clone())
                .with_clock(Arc::new(|| Ok(4000)));
            threads.push(std::thread::spawn(move || {
                manager.access_token(&CancellationToken::new()).unwrap()
            }));
        }
        for thread in threads {
            assert_eq!(thread.join().unwrap(), "new-access-canary");
        }
        assert_eq!(network.calls.load(Ordering::SeqCst), 1);
        let restarted =
            AccountManager::new(spec, store, network.clone()).with_clock(Arc::new(|| Ok(4100)));
        assert_eq!(
            restarted.access_token(&cancel).unwrap(),
            "new-access-canary"
        );
        assert_eq!(network.calls.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn account_manager_recovers_transport_interruption_and_tombstones_rejected_refresh_after_restart()
     {
        let (root, store) = store();
        let spec = spec();
        let cancel = CancellationToken::new();
        {
            store
                .transaction(&spec, &cancel)
                .unwrap()
                .install(None, &token())
                .unwrap();
        }
        let failed = script(vec![Err(Error::Transient)]);
        let manager = AccountManager::new(spec.clone(), store.clone(), failed)
            .with_clock(Arc::new(|| Ok(4000)));
        assert_eq!(manager.access_token(&cancel).unwrap_err(), Error::Transient);
        let rejected = script(vec![reply(
            400,
            r#"{"error":"invalid_grant","error_description":"private-refresh-canary"}"#,
        )]);
        let manager = AccountManager::new(spec.clone(), store.clone(), rejected.clone())
            .with_clock(Arc::new(|| Ok(4001)));
        assert_eq!(manager.access_token(&cancel).unwrap_err(), Error::Revoked);
        assert_eq!(rejected.calls.load(Ordering::SeqCst), 1);
        let fresh =
            AccountManager::new(spec, store, rejected.clone()).with_clock(Arc::new(|| Ok(4002)));
        assert_eq!(fresh.access_token(&cancel).unwrap_err(), Error::Revoked);
        assert_eq!(rejected.calls.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn account_remote_revocation_failure_still_commits_a_local_tombstone() {
        let (root, store) = store();
        let spec = spec();
        let cancel = CancellationToken::new();
        {
            store
                .transaction(&spec, &cancel)
                .unwrap()
                .install(None, &token())
                .unwrap();
        }
        let network = script(vec![reply(503, "private-server-error")]);
        let manager = AccountManager::new(spec.clone(), store.clone(), network.clone());
        assert_eq!(manager.revoke(true, &cancel).unwrap_err(), Error::Transient);
        assert!(matches!(
            store.transaction(&spec, &cancel).unwrap().load(),
            Err(Error::Revoked)
        ));
        assert_eq!(manager.revoke(true, &cancel).unwrap_err(), Error::Missing);
        assert_eq!(network.calls.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn requested_remote_logout_without_endpoint_reports_failure_after_local_revocation() {
        let (root, store) = store();
        let mut definition = spec().definition().clone();
        definition.revoke_url = None;
        let spec = AccountSpec::new("work", definition).unwrap();
        let cancel = CancellationToken::new();
        store
            .transaction(&spec, &cancel)
            .unwrap()
            .install(None, &token())
            .unwrap();
        let network = script(vec![]);
        let manager = AccountManager::new(spec.clone(), store.clone(), network.clone());
        assert_eq!(
            manager.revoke(true, &cancel).unwrap_err(),
            Error::InvalidConfig
        );
        assert!(matches!(
            store.transaction(&spec, &cancel).unwrap().load(),
            Err(Error::Revoked)
        ));
        assert_eq!(network.calls.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn account_logout_tombstones_a_missing_keychain_bundle() {
        for remote in [false, true] {
            let root = std::env::temp_dir().join(format!(
                "rapid-missing-account-{}",
                ::protocol::SessionId::new()
            ));
            let backend = Arc::new(MemoryKeychain::default());
            let store = AccountStore::new(root.clone(), backend.clone());
            let spec = spec();
            let cancel = CancellationToken::new();
            store
                .transaction(&spec, &cancel)
                .unwrap()
                .install(None, &token())
                .unwrap();
            backend.items.lock().unwrap().clear();
            let network = script(vec![]);
            let manager = AccountManager::new(spec.clone(), store.clone(), network.clone());
            let result = manager.revoke(remote, &cancel);
            if remote {
                assert_eq!(result.unwrap_err(), Error::Missing);
            } else {
                assert!(result.is_ok());
            }
            assert!(matches!(
                store.transaction(&spec, &cancel).unwrap().load(),
                Err(Error::Revoked)
            ));
            assert_eq!(network.calls.load(Ordering::SeqCst), 0);
            let _ = std::fs::remove_dir_all(root);
        }
    }
    #[test]
    fn account_non_expiring_github_source_tokens_need_no_refresh_or_network() {
        let (root, store) = store();
        let spec = spec();
        let cancel = CancellationToken::new();
        let initial = parse_tokens(
            &spec,
            200,
            br#"{"access_token":"source-token"}"#,
            1000,
            None,
        )
        .unwrap();
        {
            store
                .transaction(&spec, &cancel)
                .unwrap()
                .install(None, &initial)
                .unwrap();
        }
        let network = script(vec![]);
        let manager =
            AccountManager::new(spec, store, network.clone()).with_clock(Arc::new(|| Ok(999999)));
        assert_eq!(manager.access_token(&cancel).unwrap(), "source-token");
        assert_eq!(network.calls.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(root);
    }
}
