//! `rapid mcp logout <server> [--forget-client]` (EXT-02 §4.6): the opposite of
//! authorizing.
//!
//! The stored tokens are deleted from the credential store, and the
//! authorization server is asked to revoke them if it advertised an endpoint
//! for that — through the egress gate, with a receipt, best effort: a server
//! that cannot be reached or refuses is reported, never a reason to keep the
//! tokens. The registered client is kept (a later authorization reuses it)
//! unless `--forget-client` says otherwise.

use crate::mcp_credentials::{CredentialError, CredentialVault, OauthStore};
use crate::mcp_oauth::{Secret, Url, basic_authorization, form_body};
use crate::mcp_oauth_net::OauthHttp;

/// What came of asking for the tokens to be revoked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Revocation {
    /// The authorization server advertised no revocation endpoint.
    NotOffered,
    /// There was no token to revoke.
    NothingToRevoke,
    /// Every token was revoked (the server said so).
    Revoked,
    /// A request failed or was refused; the tokens are deleted all the same.
    Failed(String),
}

impl Revocation {
    /// The word a report carries.
    pub fn word(&self) -> &'static str {
        match self {
            Self::NotOffered => "not_offered",
            Self::NothingToRevoke => "nothing_to_revoke",
            Self::Revoked => "revoked",
            Self::Failed(_) => "failed",
        }
    }
}

/// What a logout did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoggedOut {
    pub origin: String,
    pub issuer: String,
    pub client_id: String,
    pub revocation: Revocation,
    /// The registered client was dropped too.
    pub client_forgotten: bool,
}

/// Why a logout did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogoutError {
    /// Nothing is stored for this server.
    NotAuthorized,
    /// The credential store is not there, so nothing can be deleted from it.
    NoSecureStore,
    /// The credential store or the record failed.
    Failed(String),
}

impl std::fmt::Display for LogoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAuthorized => f.write_str("nothing is stored for this server"),
            Self::NoSecureStore => f.write_str(&CredentialError::NoSecureStore.to_string()),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

/// Log the server at `url` out. `client_secret` is a pre-registered client's,
/// for the revocation request to present as the token endpoint would be asked.
pub fn logout(
    url: &str,
    forget_client: bool,
    oauth: &OauthStore,
    vault: &dyn CredentialVault,
    net: &dyn OauthHttp,
    client_secret: Option<&Secret>,
) -> Result<LoggedOut, LogoutError> {
    let record = oauth.get(url).ok_or(LogoutError::NotAuthorized)?;
    let key = record.credential_key.clone();
    let read = |result: Result<Option<Secret>, CredentialError>| match result {
        Ok(token) => Ok(token),
        Err(CredentialError::NoSecureStore) => Err(LogoutError::NoSecureStore),
        Err(other) => Err(LogoutError::Failed(other.to_string())),
    };
    // Read before anything is deleted: what is revoked is what was stored.
    let refresh = read(vault.refresh(&key))?;
    let access = read(vault.access(&key))?;

    let revocation = match record.revocation_endpoint.as_deref().map(Url::parse) {
        None => Revocation::NotOffered,
        Some(Err(_)) => {
            Revocation::Failed("the recorded revocation address is not usable".to_owned())
        }
        Some(Ok(endpoint)) => {
            let tokens: Vec<(&str, &Secret)> = refresh
                .iter()
                .map(|token| ("refresh_token", token))
                .chain(access.iter().map(|token| ("access_token", token)))
                .collect();
            if tokens.is_empty() {
                Revocation::NothingToRevoke
            } else {
                let mut failure = None;
                for (hint, token) in tokens {
                    let mut pairs = vec![
                        ("token", token.expose()),
                        ("token_type_hint", hint),
                        ("client_id", record.client_id.as_str()),
                    ];
                    if let Some(secret) = client_secret.filter(|_| record.secret_in_body) {
                        pairs.push(("client_secret", secret.expose()));
                    }
                    let basic = client_secret
                        .filter(|_| !record.secret_in_body)
                        .map(|secret| basic_authorization(&record.client_id, secret));
                    let body = form_body(&pairs);
                    match net.post(
                        &endpoint,
                        "application/x-www-form-urlencoded",
                        basic.as_deref(),
                        body.as_bytes(),
                    ) {
                        Ok(reply) if (200..300).contains(&reply.status) => {}
                        Ok(reply) => {
                            failure.get_or_insert(format!(
                                "{} answered HTTP {} to the {hint} revocation",
                                endpoint.authority(),
                                reply.status
                            ));
                        }
                        Err(err) => {
                            failure.get_or_insert(format!("the {hint} revocation: {err}"));
                        }
                    }
                }
                failure.map_or(Revocation::Revoked, Revocation::Failed)
            }
        }
    };

    // Deleted whatever the revocation said.
    vault.delete(&key).map_err(|err| match err {
        CredentialError::NoSecureStore => LogoutError::NoSecureStore,
        other => LogoutError::Failed(other.to_string()),
    })?;
    if forget_client {
        oauth.remove(url).map_err(LogoutError::Failed)?;
    } else {
        oauth.tokens_gone(url).map_err(LogoutError::Failed)?;
    }
    Ok(LoggedOut {
        origin: record.origin,
        issuer: record.issuer,
        client_id: record.client_id,
        revocation,
        client_forgotten: forget_client,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_credentials::{Authorization, MemoryVault, credential_key};
    use crate::mcp_oauth_stub::{HttpStub, Response};
    use std::sync::Arc;

    struct Rig {
        home: std::path::PathBuf,
        oauth: OauthStore,
        vault: Arc<MemoryVault>,
        key: String,
        url: String,
    }

    fn rig(revocation: Option<&str>, refresh: bool) -> Rig {
        let home = std::env::temp_dir().join(format!(
            "rapid-logout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&home).unwrap();
        let url = "http://127.0.0.1:7/mcp".to_owned();
        let key = credential_key(
            &crate::mcp_auth::canonical_url(&url),
            "http://127.0.0.1:9",
            "c1",
        );
        let oauth = OauthStore::in_home(&home);
        oauth
            .put(
                &url,
                Authorization::new(
                    "http://127.0.0.1:7".to_owned(),
                    "http://127.0.0.1:9".to_owned(),
                    "c1".to_owned(),
                    true,
                    vec!["read".to_owned()],
                    "http://127.0.0.1:9/token".to_owned(),
                    revocation.map(str::to_owned),
                    key.clone(),
                    None,
                )
                .with_refresh(refresh, false),
            )
            .unwrap();
        let vault = Arc::new(MemoryVault::default());
        vault.seed(&key, "access-canary", refresh.then_some("refresh-canary"));
        Rig {
            home,
            oauth,
            vault,
            key,
            url,
        }
    }

    fn client(url: &str) -> crate::mcp_oauth_net::GatedHttp {
        crate::mcp_oauth_net::GatedHttp::for_server(
            &Url::parse(url).unwrap(),
            None,
            None,
            llm_router::provider::CancellationToken::new(),
        )
    }

    #[test]
    fn every_token_is_revoked_then_deleted_and_the_client_is_kept() {
        let stub = HttpStub::start(|_| Response::json(200, "{}"));
        let rig = rig(Some(&stub.url("/revoke")), true);
        let net = client(&stub.url("/"));
        let out = logout(&rig.url, false, &rig.oauth, rig.vault.as_ref(), &net, None).unwrap();
        assert_eq!(out.revocation, Revocation::Revoked);
        assert!(!out.client_forgotten);
        // Both tokens went to the endpoint, the refresh token first, with the hint.
        let asked = stub.requests();
        assert_eq!(asked.len(), 2);
        let form = |n: usize| asked[n].form();
        let field =
            |n: usize, name: &str| form(n).into_iter().find(|(k, _)| k == name).map(|(_, v)| v);
        assert_eq!(field(0, "token").as_deref(), Some("refresh-canary"));
        assert_eq!(
            field(0, "token_type_hint").as_deref(),
            Some("refresh_token")
        );
        assert_eq!(field(1, "token").as_deref(), Some("access-canary"));
        assert_eq!(field(1, "token_type_hint").as_deref(), Some("access_token"));
        assert_eq!(field(0, "client_id").as_deref(), Some("c1"));
        assert_eq!(rig.vault.access_of(&rig.key), None);
        assert_eq!(rig.vault.refresh_of(&rig.key), None);
        let record = rig.oauth.get(&rig.url).expect("the client is kept");
        assert!(!record.tokens && !record.refreshable);
        assert_eq!(record.client_id, "c1");
    }

    #[test]
    fn forgetting_the_client_drops_the_record() {
        let rig = rig(None, true);
        let net = client("http://127.0.0.1:7/mcp");
        let out = logout(&rig.url, true, &rig.oauth, rig.vault.as_ref(), &net, None).unwrap();
        assert_eq!(out.revocation, Revocation::NotOffered);
        assert!(out.client_forgotten);
        assert!(rig.oauth.get(&rig.url).is_none());
        assert_eq!(rig.vault.access_of(&rig.key), None);
        let _ = std::fs::remove_dir_all(&rig.home);
    }

    #[test]
    fn a_revocation_that_fails_is_reported_and_the_tokens_are_deleted_anyway() {
        // Refused, and not reachable at all.
        let refusing = HttpStub::start(|_| Response::json(503, "{}"));
        let rig = rig(Some(&refusing.url("/revoke")), true);
        let net = client(&refusing.url("/"));
        let out = logout(&rig.url, false, &rig.oauth, rig.vault.as_ref(), &net, None).unwrap();
        match &out.revocation {
            Revocation::Failed(why) => assert!(why.contains("HTTP 503"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(rig.vault.access_of(&rig.key), None, "deleted all the same");
        assert!(!rig.oauth.get(&rig.url).unwrap().tokens);
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let down = rig_down(port);
        let net = client(&format!("http://127.0.0.1:{port}/"));
        let out = logout(
            &down.url,
            false,
            &down.oauth,
            down.vault.as_ref(),
            &net,
            None,
        )
        .unwrap();
        assert!(matches!(out.revocation, Revocation::Failed(_)), "{out:?}");
        assert_eq!(down.vault.access_of(&down.key), None);
    }

    fn rig_down(port: u16) -> Rig {
        rig(Some(&format!("http://127.0.0.1:{port}/revoke")), true)
    }

    #[test]
    fn a_secret_is_presented_as_the_server_asked_and_never_leaves_a_trace() {
        let stub = HttpStub::start(|_| Response::json(200, "{}"));
        let rig = rig(Some(&stub.url("/revoke")), false);
        let net = client(&stub.url("/"));
        let secret = Secret::new("sekret-canary-3".to_owned());
        let out = logout(
            &rig.url,
            false,
            &rig.oauth,
            rig.vault.as_ref(),
            &net,
            Some(&secret),
        )
        .unwrap();
        assert_eq!(out.revocation, Revocation::Revoked);
        let asked = stub.requests();
        assert_eq!(asked.len(), 1, "no refresh token was stored");
        assert!(
            asked[0]
                .header("authorization")
                .unwrap()
                .starts_with("Basic ")
        );
        assert!(!asked[0].body_text().contains("sekret-canary-3"));
    }

    #[test]
    fn nothing_stored_nothing_to_revoke_and_no_store_are_each_their_own_answer() {
        let stub = HttpStub::start(|_| Response::json(200, "{}"));
        let net = client(&stub.url("/"));
        // Never authorized.
        let rig = rig(Some(&stub.url("/revoke")), true);
        assert_eq!(
            logout(
                "http://127.0.0.1:99/other",
                false,
                &rig.oauth,
                rig.vault.as_ref(),
                &net,
                None
            )
            .unwrap_err(),
            LogoutError::NotAuthorized
        );
        // No tokens in the store (logged out before): nothing to revoke.
        rig.vault.delete(&rig.key).unwrap();
        let out = logout(&rig.url, false, &rig.oauth, rig.vault.as_ref(), &net, None).unwrap();
        assert_eq!(out.revocation, Revocation::NothingToRevoke);
        assert!(stub.requests().is_empty());
        // The store is not there: nothing is deleted, nothing is claimed.
        struct NoStore;
        impl CredentialVault for NoStore {
            fn access(&self, _: &str) -> Result<Option<Secret>, CredentialError> {
                Err(CredentialError::NoSecureStore)
            }
            fn refresh(&self, _: &str) -> Result<Option<Secret>, CredentialError> {
                Err(CredentialError::NoSecureStore)
            }
            fn put(&self, _: &str, _: &Secret, _: Option<&Secret>) -> Result<(), CredentialError> {
                Err(CredentialError::NoSecureStore)
            }
            fn delete(&self, _: &str) -> Result<(), CredentialError> {
                Err(CredentialError::NoSecureStore)
            }
        }
        let rig = self::rig(Some(&stub.url("/revoke")), true);
        assert_eq!(
            logout(&rig.url, true, &rig.oauth, &NoStore, &net, None).unwrap_err(),
            LogoutError::NoSecureStore
        );
        assert!(
            rig.oauth.get(&rig.url).unwrap().tokens,
            "the record still says what is stored"
        );
        assert!(
            stub.requests().is_empty(),
            "nothing was revoked without being able to delete"
        );
    }
}
