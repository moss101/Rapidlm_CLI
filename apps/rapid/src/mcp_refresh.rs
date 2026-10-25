//! Keeping an authorized server's access token good (EXT-02 §4.4).
//!
//! The token a connection sends comes from a [`TokenSupply`], asked at the
//! moment a request is written, never held in the entry:
//!
//! - **Proactively**: a token that expires within 60 seconds is renewed before
//!   it is sent.
//! - **Reactively**: a request the server refuses with `401` is retried once
//!   with a renewed token (the io adapter does the retry; this module says what
//!   the new token is).
//! - **Single-flight**: any number of callers in a process, and any number of
//!   processes, that find the token stale cause one refresh. The process-wide
//!   lock is per credential; the cross-process lock is the operating system's
//!   advisory lock on a file in the home. After taking them a caller looks
//!   again: someone else may have done it.
//! - **Persisted before used**: a refresh reply's tokens go into the credential
//!   store first. If that fails the new token is not used and the server is
//!   `needs_auth`; a rotated refresh token that was not stored would be lost
//!   with the access token that came with it.
//! - **Only to the pinned endpoint**: the request goes to the token endpoint
//!   recorded when the person authorized, never to one found since.
//! - **`invalid_grant`** means the grant is gone: both tokens are deleted and
//!   the server is `needs_auth`.
//!
//! Nothing here prints a token; an error is a word.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use llm_router::providers::dial::ProxyConfig;

use crate::mcp_auth::{AuthStateStore, FileLock, NeedsAuth};
use crate::mcp_credentials::{Authorization, CredentialVault, OauthStore, now_ms};
use crate::mcp_oauth::{
    ProtocolError, Secret, Url, basic_authorization, parse_token_reply, refresh_body,
};
use crate::mcp_oauth_net::{GatedHttp, OauthHttp};

/// A token that expires within this long is renewed before it is sent.
pub const REFRESH_AHEAD_MS: i64 = 60_000;

/// How long a caller waits for another process that is refreshing.
const FILE_WAIT: Duration = Duration::from_secs(15);

/// The most tokens a supply remembers having sent (for redaction).
const MAX_SEEN: usize = 16;

/// Where a connection gets the token it sends.
pub trait TokenSupply: Send + Sync {
    /// The token to send now, renewed first when it is about to expire.
    fn bearer(&self) -> Option<Secret>;

    /// The server refused `used`: a token that may do better, if there is one.
    /// A given token is not renewed twice.
    fn after_rejection(&self, used: &Secret) -> Option<Secret>;

    /// Every access token this supply has handed out, newest last: what a
    /// transcript must not carry.
    fn tokens_seen(&self) -> Vec<String>;
}

/// How requests leave the machine.
#[derive(Clone, Default)]
pub struct Network {
    pub proxy: Option<ProxyConfig>,
    /// Where receipts go (`<root>/.rapidlm/egress-receipts.jsonl`).
    pub receipt_root: Option<PathBuf>,
}

/// One process-wide lock per credential: callers that find a token stale
/// queue here, and the second one finds the first one's work done.
static FLIGHTS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn flight(key: &str) -> Arc<Mutex<()>> {
    let mut flights = FLIGHTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if flights.len() > 512 {
        flights.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
    Arc::clone(
        flights
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

#[derive(Default)]
struct Cache {
    access: Option<Secret>,
    expires_at_ms: Option<i64>,
    seen: Vec<String>,
    /// Tokens already renewed after a refusal.
    renewed_after: Vec<String>,
}

/// The supply for a server authorized by `rapid mcp auth`.
pub struct OauthSupply {
    /// The server's URL as configured.
    url: String,
    mcp_url: Url,
    oauth: OauthStore,
    needs: Option<AuthStateStore>,
    vault: Arc<dyn CredentialVault>,
    home: PathBuf,
    network: Network,
    /// A pre-registered client's secret, read from the environment when the
    /// connection was made.
    client_secret: Option<Secret>,
    cache: Mutex<Cache>,
}

/// Why a refresh did not give a token.
enum RefreshFailed {
    /// The grant is gone (`invalid_grant`): authorize again.
    Gone,
    /// The server refused for another reason, or could not be asked.
    Failed,
}

impl OauthSupply {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        url: &str,
        oauth: OauthStore,
        needs: Option<AuthStateStore>,
        vault: Arc<dyn CredentialVault>,
        home: PathBuf,
        network: Network,
        client_secret: Option<Secret>,
    ) -> Option<Self> {
        Some(Self {
            url: url.to_owned(),
            mcp_url: Url::parse(url).ok()?,
            oauth,
            needs,
            vault,
            home,
            network,
            client_secret,
            cache: Mutex::new(Cache::default()),
        })
    }

    fn cached(&self) -> Option<Secret> {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let access = cache.access.as_ref()?;
        match cache.expires_at_ms {
            Some(at) if at - now_ms() <= REFRESH_AHEAD_MS => None,
            _ => Some(access.clone()),
        }
    }

    fn remember(&self, access: &Secret, expires_at_ms: Option<i64>) {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !cache.seen.iter().any(|seen| seen == access.expose()) {
            if cache.seen.len() >= MAX_SEEN {
                cache.seen.remove(0);
            }
            cache.seen.push(access.expose().to_owned());
        }
        cache.access = Some(access.clone());
        cache.expires_at_ms = expires_at_ms;
    }

    fn needs_auth(&self, record: &Authorization) {
        if let Some(store) = &self.needs {
            let _ = store.record_needs_auth(
                &self.url,
                &NeedsAuth {
                    scopes: record.scopes.clone(),
                    insufficient_scope: false,
                },
            );
        }
    }

    /// The token as it is now, renewing it when it is stale (or, `force`d,
    /// whatever its age) — once, whoever asks.
    fn current(&self, force: bool, rejected: Option<&Secret>) -> Option<Secret> {
        let record = self.oauth.get(&self.url).filter(|record| record.tokens)?;
        let key = record.credential_key.clone();
        let in_process = flight(&key);
        let _held = in_process
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lock_name = format!("mcp-refresh-{}.lock", key.get(..16).unwrap_or(&key));
        // Another process may be renewing. Waited for; if it takes too long
        // the token as it stands is all there is.
        let _file = FileLock::acquire(self.home.join(lock_name), FILE_WAIT).ok();

        // Look again, now that nobody else is at it.
        let record = self.oauth.get(&self.url).filter(|record| record.tokens)?;
        let now = now_ms();
        let stored = self.vault.access(&key).ok().flatten();
        let fresh_enough = record
            .expires_at_ms
            .is_none_or(|at| at - now > REFRESH_AHEAD_MS);
        if let Some(access) = &stored {
            let renewed_elsewhere = rejected.is_some_and(|used| used.expose() != access.expose());
            if (!force && fresh_enough) || renewed_elsewhere {
                self.remember(access, record.expires_at_ms);
                return Some(access.clone());
            }
        }
        let still_good = stored
            .clone()
            .filter(|_| record.expires_at_ms.is_none_or(|at| at > now));
        if !record.refreshable {
            if let Some(access) = &still_good {
                self.remember(access, record.expires_at_ms);
            }
            return still_good;
        }
        let Some(refresh) = self.vault.refresh(&key).ok().flatten() else {
            // The refresh token is not there: it cannot be renewed.
            let _ = self.oauth.renewed(&self.url, record.expires_at_ms, false);
            return still_good;
        };
        match self.refresh_grant(&record, &refresh) {
            Ok(granted) => {
                // Stored before it is used. A rotated refresh token that was
                // not stored would be lost with the token that came with it.
                let keep = granted.refresh.clone().unwrap_or(refresh);
                if self.vault.put(&key, &granted.access, Some(&keep)).is_err() {
                    self.needs_auth(&record);
                    return None;
                }
                let expires = granted.expires_in.map(|secs| {
                    now_ms().saturating_add(i64::try_from(secs).unwrap_or(0).saturating_mul(1000))
                });
                let _ = self.oauth.renewed(&self.url, expires, true);
                if let Some(store) = &self.needs {
                    let _ = store.clear(&self.url);
                }
                self.remember(&granted.access, expires);
                Some(granted.access)
            }
            Err(RefreshFailed::Gone) => {
                let _ = self.vault.delete(&key);
                let _ = self.oauth.tokens_gone(&self.url);
                self.needs_auth(&record);
                None
            }
            Err(RefreshFailed::Failed) => {
                // The server could not be asked or would not: what is stored
                // stays, and is used while it still is good.
                if let Some(access) = &still_good {
                    self.remember(access, record.expires_at_ms);
                }
                still_good
            }
        }
    }

    /// The refresh request, to the token endpoint recorded at authorization.
    fn refresh_grant(
        &self,
        record: &Authorization,
        refresh: &Secret,
    ) -> Result<crate::mcp_oauth::Granted, RefreshFailed> {
        let endpoint = Url::parse(&record.token_endpoint).map_err(|_| RefreshFailed::Failed)?;
        let issuer = Url::parse(&record.issuer).map_err(|_| RefreshFailed::Failed)?;
        let net = GatedHttp::for_server(
            &self.mcp_url,
            self.network.proxy.clone(),
            self.network.receipt_root.clone(),
            llm_router::provider::CancellationToken::new(),
        );
        let resource = crate::mcp_auth::canonical_url(&self.url);
        let secret = self.client_secret.as_ref();
        let body = refresh_body(
            refresh,
            &record.client_id,
            &resource,
            secret.filter(|_| record.secret_in_body),
        );
        let basic = secret
            .filter(|_| !record.secret_in_body)
            .map(|secret| basic_authorization(&record.client_id, secret));
        let reply = net
            .post(
                &endpoint,
                "application/x-www-form-urlencoded",
                basic.as_deref(),
                body.as_bytes(),
            )
            .map_err(|_| RefreshFailed::Failed)?;
        parse_token_reply(reply.status, &reply.body, &issuer).map_err(|err| match err {
            ProtocolError::Rejected { error } if error == "invalid_grant" => RefreshFailed::Gone,
            _ => RefreshFailed::Failed,
        })
    }
}

impl TokenSupply for OauthSupply {
    fn bearer(&self) -> Option<Secret> {
        self.cached().or_else(|| self.current(false, None))
    }

    fn after_rejection(&self, used: &Secret) -> Option<Secret> {
        {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cache.renewed_after.iter().any(|seen| seen == used.expose()) {
                return None;
            }
            if cache.renewed_after.len() >= MAX_SEEN {
                cache.renewed_after.remove(0);
            }
            cache.renewed_after.push(used.expose().to_owned());
        }
        self.current(true, Some(used))
            .filter(|renewed| renewed.expose() != used.expose())
    }

    fn tokens_seen(&self) -> Vec<String> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .seen
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_credentials::{MemoryVault, credential_key};
    use crate::mcp_oauth_stub::{World, WorldOptions};

    struct Rig {
        world: World,
        home: PathBuf,
        vault: Arc<MemoryVault>,
        oauth: OauthStore,
        needs: AuthStateStore,
        url: String,
        key: String,
    }

    fn rig(options: WorldOptions, expires_in_ms: i64, refreshable: bool) -> Rig {
        let world = World::start(options);
        let home = std::env::temp_dir().join(format!(
            "rapid-refresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&home).unwrap();
        let url = world.mcp_url();
        let key = credential_key(
            &crate::mcp_auth::canonical_url(&url),
            &world.issuer(),
            "pre-client",
        );
        let oauth = OauthStore::in_home(&home);
        oauth
            .put(
                &url,
                Authorization::new(
                    "http://127.0.0.1".to_owned(),
                    world.issuer(),
                    "pre-client".to_owned(),
                    false,
                    vec!["read".to_owned()],
                    format!("{}/token", world.issuer()),
                    Some(format!("{}/revoke", world.issuer())),
                    key.clone(),
                    Some(now_ms() + expires_in_ms),
                )
                .with_refresh(refreshable, false),
            )
            .unwrap();
        let vault = Arc::new(MemoryVault::default());
        vault.seed(
            &key,
            "stale-access",
            refreshable.then_some("world-refresh-canary-1"),
        );
        Rig {
            world,
            needs: AuthStateStore::in_home(&home),
            oauth,
            home,
            vault,
            url,
            key,
        }
    }

    impl Rig {
        fn supply(&self, secret: Option<&str>) -> OauthSupply {
            OauthSupply::new(
                &self.url,
                self.oauth.clone(),
                Some(self.needs.clone()),
                self.vault.clone(),
                self.home.clone(),
                Network {
                    proxy: None,
                    receipt_root: Some(self.home.clone()),
                },
                secret.map(|s| Secret::new(s.to_owned())),
            )
            .expect("supply")
        }

        fn refreshes(&self) -> usize {
            self.world.state.lock().unwrap().refresh_forms.len()
        }
    }

    fn expose(token: Option<Secret>) -> Option<String> {
        token.map(|token| token.expose().to_owned())
    }

    #[test]
    fn a_token_within_sixty_seconds_of_expiry_is_renewed_before_it_is_sent() {
        // 30 s left: renewed; the request carries the new token.
        let near = rig(WorldOptions::default(), 30_000, true);
        let supply = near.supply(None);
        let got = expose(supply.bearer());
        assert_eq!(got.as_deref(), Some("world-access-r1"));
        assert_eq!(near.refreshes(), 1);
        let form = near.world.state.lock().unwrap().refresh_forms[0].clone();
        let field = |name: &str| form.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        assert_eq!(field("grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(
            field("refresh_token").as_deref(),
            Some("world-refresh-canary-1")
        );
        assert_eq!(field("client_id").as_deref(), Some("pre-client"));
        assert_eq!(
            field("resource"),
            Some(crate::mcp_auth::canonical_url(&near.url))
        );
        // Stored first: the credential store and the record carry it.
        assert_eq!(
            near.vault.access_of(&near.key).as_deref(),
            Some("world-access-r1")
        );
        let record = near.oauth.get(&near.url).unwrap();
        let left = record.expires_at_ms.unwrap() - now_ms();
        assert!((3_500_000..=3_600_000).contains(&left), "{left}");
        assert!(record.refreshable);
        // And asked for again, it is the same token and no new request.
        assert_eq!(expose(supply.bearer()).as_deref(), Some("world-access-r1"));
        assert_eq!(near.refreshes(), 1);

        // 59 s left renews, 61 s does not: the line is sixty seconds.
        for (left_ms, renewed) in [(59_000, true), (61_000, false), (3_600_000, false)] {
            let rig = rig(WorldOptions::default(), left_ms, true);
            let got = expose(rig.supply(None).bearer());
            assert_eq!(rig.refreshes(), usize::from(renewed), "{left_ms}");
            assert_eq!(
                got.as_deref(),
                Some(if renewed {
                    "world-access-r1"
                } else {
                    "stale-access"
                }),
                "{left_ms}"
            );
        }
    }

    #[test]
    fn a_token_that_expires_never_is_not_renewed_ahead() {
        let rig = rig(WorldOptions::default(), 3_600_000, true);
        rig.oauth
            .renewed(&rig.url, None, true)
            .expect("no expiry given");
        assert_eq!(
            expose(rig.supply(None).bearer()).as_deref(),
            Some("stale-access")
        );
        assert_eq!(rig.refreshes(), 0);
    }

    #[test]
    fn a_rotated_refresh_token_is_stored_before_the_new_access_token_is_used() {
        let rig = rig(
            WorldOptions {
                refresh_rotates: true,
                ..WorldOptions::default()
            },
            1_000,
            true,
        );
        let got = expose(rig.supply(None).bearer());
        assert_eq!(got.as_deref(), Some("world-access-r1"));
        let puts = rig.vault.puts.lock().unwrap().clone();
        assert_eq!(
            puts,
            vec![(
                rig.key.clone(),
                "world-access-r1".to_owned(),
                Some("world-refresh-r1".to_owned())
            )],
            "both stored, together"
        );
        assert_eq!(
            rig.vault.refresh_of(&rig.key).as_deref(),
            Some("world-refresh-r1")
        );
        // Without rotation the refresh token that was there stays.
        let kept = rig_kept();
        assert_eq!(
            kept.vault.refresh_of(&kept.key).as_deref(),
            Some("world-refresh-canary-1")
        );
    }

    fn rig_kept() -> Rig {
        let rig = rig(WorldOptions::default(), 1_000, true);
        assert!(rig.supply(None).bearer().is_some());
        rig
    }

    #[test]
    fn a_token_that_could_not_be_stored_is_not_used_and_the_server_asks_again() {
        let rig = rig(
            WorldOptions {
                refresh_rotates: true,
                ..WorldOptions::default()
            },
            1_000,
            true,
        );
        rig.vault
            .fail_puts
            .store(true, std::sync::atomic::Ordering::Release);
        let supply = rig.supply(None);
        assert_eq!(supply.bearer().map(|t| t.expose().to_owned()), None);
        assert_eq!(rig.refreshes(), 1, "the server was asked, and answered");
        // The new token never reached the server in a request.
        assert!(
            rig.world
                .requests()
                .iter()
                .all(|r| r.header("authorization") != Some("Bearer world-access-r1")),
        );
        assert!(!supply.tokens_seen().iter().any(|t| t == "world-access-r1"));
        assert!(rig.needs.needs_auth(&rig.url).is_some(), "needs_auth");
        // What was stored is what was.
        assert_eq!(
            rig.vault.refresh_of(&rig.key).as_deref(),
            Some("world-refresh-canary-1")
        );
    }

    #[test]
    fn any_number_of_callers_in_any_number_of_processes_cause_one_refresh() {
        let rig = rig(
            WorldOptions {
                refresh_delay: Duration::from_millis(250),
                ..WorldOptions::default()
            },
            1_000,
            true,
        );
        // Two supplies stand for two processes: no cache, no lock in common
        // but the file's.
        let first = Arc::new(rig.supply(None));
        let second = Arc::new(rig.supply(None));
        let results = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let callers: Vec<_> = (0..8)
            .map(|n| {
                let supply = if n % 2 == 0 {
                    first.clone()
                } else {
                    second.clone()
                };
                let results = results.clone();
                std::thread::spawn(move || {
                    let got = supply.bearer().map(|t| t.expose().to_owned());
                    results.lock().unwrap().push(got);
                })
            })
            .collect();
        for caller in callers {
            caller.join().unwrap();
        }
        assert_eq!(rig.refreshes(), 1, "exactly one refresh at the server");
        let results = results.lock().unwrap().clone();
        assert_eq!(results.len(), 8);
        assert!(
            results
                .iter()
                .all(|got| got.as_deref() == Some("world-access-r1")),
            "{results:?}"
        );
    }

    #[test]
    fn invalid_grant_deletes_the_grant_and_the_server_asks_again() {
        let rig = rig(
            WorldOptions {
                refresh_error: Some("invalid_grant".to_owned()),
                ..WorldOptions::default()
            },
            1_000,
            true,
        );
        let supply = rig.supply(None);
        assert!(supply.bearer().is_none());
        assert_eq!(rig.vault.access_of(&rig.key), None);
        assert_eq!(
            rig.vault.refresh_of(&rig.key),
            None,
            "the refresh token is gone"
        );
        let record = rig.oauth.get(&rig.url).unwrap();
        assert!(!record.tokens && !record.refreshable);
        assert!(rig.needs.needs_auth(&rig.url).is_some());
        let http = crate::exec_tools::McpHttpEndpoint {
            url: rig.url.clone(),
            headers: Vec::new(),
            headers_env: Vec::new(),
            bearer_token_env: None,
            auth: crate::exec_tools::McpAuthMode::Engage,
        };
        let stores = crate::mcp_auth::Stores::in_home(&rig.needs, &rig.oauth);
        assert_eq!(
            crate::mcp_auth::state_with(&http, &stores).word(),
            "needs_auth"
        );
        // It is not asked again.
        assert!(supply.bearer().is_none());
        assert_eq!(rig.refreshes(), 1);
    }

    #[test]
    fn a_server_that_cannot_be_asked_leaves_the_tokens_where_they_are() {
        let rig = rig(WorldOptions::default(), 30_000, true);
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let mut record = rig.oauth.get(&rig.url).unwrap();
        record.token_endpoint = format!("http://127.0.0.1:{port}/token");
        rig.oauth.put(&rig.url, record).unwrap();
        let supply = rig.supply(None);
        // 30 s left and the server will not answer: the token that is good is used.
        assert_eq!(expose(supply.bearer()).as_deref(), Some("stale-access"));
        assert_eq!(
            rig.vault.refresh_of(&rig.key).as_deref(),
            Some("world-refresh-canary-1")
        );
        assert!(rig.oauth.get(&rig.url).unwrap().tokens);
        assert!(rig.needs.needs_auth(&rig.url).is_none(), "not a refusal");
        // Once it has expired there is nothing to send, and the grant is still there.
        rig.oauth
            .renewed(&rig.url, Some(now_ms() - 5_000), true)
            .unwrap();
        assert!(rig.supply(None).bearer().is_none());
        assert_eq!(
            rig.vault.refresh_of(&rig.key).as_deref(),
            Some("world-refresh-canary-1")
        );
        assert!(rig.oauth.get(&rig.url).unwrap().tokens);
    }

    #[test]
    fn the_refresh_goes_to_the_endpoint_recorded_at_authorization_and_to_no_other() {
        let other = crate::mcp_oauth_stub::HttpStub::start(|_| {
            crate::mcp_oauth_stub::Response::json(200, "{}")
        });
        let rig = rig(
            WorldOptions {
                // The metadata now says something else; a refresh does not read it.
                token_endpoint: Some(other.url("/token")),
                ..WorldOptions::default()
            },
            1_000,
            true,
        );
        assert_eq!(
            expose(rig.supply(None).bearer()).as_deref(),
            Some("world-access-r1")
        );
        assert!(other.requests().is_empty());
        assert!(
            rig.world
                .requests()
                .iter()
                .all(|r| !r.path.contains(".well-known")),
            "nothing was rediscovered"
        );
        // Every request left a receipt.
        let receipts =
            std::fs::read_to_string(rig.home.join(".rapidlm/egress-receipts.jsonl")).unwrap();
        assert!(
            receipts.contains("\"mcp_auth\"") && receipts.contains("\"allowed\":true"),
            "{receipts}"
        );
    }

    #[test]
    fn a_pre_registered_clients_secret_is_presented_as_the_authorization_server_asked() {
        let basic = rig(WorldOptions::default(), 1_000, true);
        assert!(basic.supply(Some("sekret-canary-9")).bearer().is_some());
        {
            let state = basic.world.state.lock().unwrap();
            assert!(
                state.refresh_authorizations[0]
                    .as_deref()
                    .unwrap()
                    .starts_with("Basic ")
            );
            assert!(
                state.refresh_forms[0]
                    .iter()
                    .all(|(k, _)| k != "client_secret")
            );
        }
        let in_body = rig(WorldOptions::default(), 1_000, true);
        let mut record = in_body.oauth.get(&in_body.url).unwrap();
        record.secret_in_body = true;
        in_body.oauth.put(&in_body.url, record).unwrap();
        assert!(in_body.supply(Some("sekret-canary-9")).bearer().is_some());
        let state = in_body.world.state.lock().unwrap();
        assert!(state.refresh_authorizations[0].is_none());
        assert!(
            state.refresh_forms[0]
                .iter()
                .any(|(k, v)| k == "client_secret" && v == "sekret-canary-9")
        );
        let receipts =
            std::fs::read_to_string(in_body.home.join(".rapidlm/egress-receipts.jsonl")).unwrap();
        assert!(!receipts.contains("sekret-canary-9"));
    }

    #[test]
    fn every_token_handed_out_is_a_secret_and_a_refused_one_is_renewed_once() {
        let rig = rig(WorldOptions::default(), 3_600_000, true);
        let supply = rig.supply(None);
        let first = supply.bearer().unwrap();
        assert_eq!(first.expose(), "stale-access");
        // The server refused it: renewed, and the refusal of the same token
        // again is not renewed a second time.
        let renewed = supply.after_rejection(&first).expect("renewed");
        assert_eq!(renewed.expose(), "world-access-r1");
        assert_eq!(rig.refreshes(), 1);
        assert!(supply.after_rejection(&first).is_none(), "once per refusal");
        assert_eq!(rig.refreshes(), 1);
        let seen = supply.tokens_seen();
        assert!(
            seen.contains(&"stale-access".to_owned())
                && seen.contains(&"world-access-r1".to_owned()),
            "{seen:?}"
        );
        // And asked for now, it is the renewed one.
        assert_eq!(expose(supply.bearer()).as_deref(), Some("world-access-r1"));
    }

    #[test]
    fn a_token_another_process_already_renewed_is_taken_not_renewed_again() {
        let rig = rig(WorldOptions::default(), 3_600_000, true);
        let supply = rig.supply(None);
        let used = supply.bearer().unwrap();
        // Another process renewed it meanwhile.
        rig.vault.seed(
            &rig.key,
            "renewed-elsewhere",
            Some("world-refresh-canary-1"),
        );
        let got = supply.after_rejection(&used).expect("taken");
        assert_eq!(got.expose(), "renewed-elsewhere");
        assert_eq!(rig.refreshes(), 0);
    }

    #[test]
    fn a_grant_without_a_refresh_token_cannot_be_renewed() {
        // Not refreshable: an expired token is no token.
        let gone = rig(WorldOptions::default(), -1_000, false);
        assert!(gone.supply(None).bearer().is_none());
        assert_eq!(gone.refreshes(), 0);
        // Said refreshable, but the refresh token is not in the store: it learns so.
        let missing = rig(WorldOptions::default(), -1_000, true);
        missing.vault.seed(&missing.key, "stale-access", None);
        assert!(missing.supply(None).bearer().is_none());
        assert!(!missing.oauth.get(&missing.url).unwrap().refreshable);
        assert_eq!(missing.refreshes(), 0);
    }
}
