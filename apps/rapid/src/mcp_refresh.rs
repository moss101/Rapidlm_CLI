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
//! - **Not a storm**: a refusal or an unreachable endpoint is not asked again
//!   by every caller in turn, and a token that lives less than the lead time is
//!   not renewed on every request (see [`hold`]). A caller that cannot get the
//!   cross-process lock within its wait does not ask the endpoint at all: what
//!   another process is spending is not its to spend again.
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
pub const FILE_WAIT: Duration = Duration::from_secs(15);

/// The most tokens a supply remembers having sent (for redaction).
const MAX_SEEN: usize = 16;

/// How long a caller that is only keeping a token fresh leaves a token
/// endpoint alone after it refused or could not be reached.
const HOLD_AFTER_FAILURE_MS: i64 = 30_000;

/// How long it leaves it alone after a renewal: a token that lives less than
/// the lead time is stale the moment it is stored, and must not be renewed on
/// every request.
const HOLD_AFTER_RENEWAL_MS: i64 = 10_000;

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
    /// The proxy configuration could not be read: nothing is sent.
    pub proxy_unusable: bool,
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

/// Until when (milliseconds since the epoch) a credential's token endpoint is
/// not asked again by a caller that is keeping the token fresh. A request the
/// server refused with `401` is not held back: it is renewed once, whenever.
static HOLD: LazyLock<Mutex<HashMap<String, i64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn hold(key: &str, for_ms: i64) {
    let now = now_ms();
    let mut held = HOLD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if held.len() > 512 {
        held.retain(|_, until| *until > now);
    }
    held.insert(key.to_owned(), now.saturating_add(for_ms));
}

fn held(key: &str) -> bool {
    HOLD.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)
        .is_some_and(|until| *until > now_ms())
}

/// The lock for a credential could not be taken within the wait.
#[derive(Debug)]
pub struct Busy;

/// Run `body` while holding the credential's locks: the process-wide one, then
/// the operating system's advisory lock on a file in the home. Everything that
/// writes the tokens of a binding — a renewal, a new authorization, a logout —
/// takes them, so none of them spends or deletes what another has just stored.
pub fn exclusively<R>(
    home: &std::path::Path,
    key: &str,
    wait: Duration,
    body: impl FnOnce() -> R,
) -> Result<R, Busy> {
    let in_process = flight(key);
    let _held = in_process
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let lock_name = format!("mcp-refresh-{}.lock", key.get(..16).unwrap_or(key));
    let _file = FileLock::acquire(home.join(lock_name), wait).map_err(|_| Busy)?;
    Ok(body())
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
    /// How long a caller waits for another process that is refreshing.
    file_wait: Duration,
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
            file_wait: FILE_WAIT,
        })
    }

    #[cfg(test)]
    fn waiting(mut self, wait: Duration) -> Self {
        self.file_wait = wait;
        self
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
        match exclusively(&self.home, &key, self.file_wait, || {
            self.renew(force, rejected)
        }) {
            Ok(token) => token,
            // Another process has been renewing for longer than a caller
            // waits. Asking the endpoint now could spend a refresh token the
            // other has already spent, and its `invalid_grant` would delete
            // what the other stored: the token as it stands is all there is.
            Err(Busy) => self.as_it_stands(),
        }
    }

    /// The stored token without asking anybody, if it has not expired. (A
    /// caller whose token was refused is not offered the same one again: see
    /// [`TokenSupply::after_rejection`].)
    fn as_it_stands(&self) -> Option<Secret> {
        let record = self.oauth.get(&self.url).filter(|record| record.tokens)?;
        let stored = self.vault.access(&record.credential_key).ok().flatten()?;
        if record.expires_at_ms.is_some_and(|at| at <= now_ms()) {
            return None;
        }
        self.remember(&stored, record.expires_at_ms);
        Some(stored)
    }

    /// [`Self::current`] with the credential's locks held.
    fn renew(&self, force: bool, rejected: Option<&Secret>) -> Option<Secret> {
        // Look again, now that nobody else is at it.
        let record = self.oauth.get(&self.url).filter(|record| record.tokens)?;
        let key = record.credential_key.clone();
        let now = now_ms();
        // A store that cannot be read now says nothing about the grant.
        let stored = self.vault.access(&key).ok()?;
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
        let use_as_is = |this: &Self| {
            if let Some(access) = &still_good {
                this.remember(access, record.expires_at_ms);
            }
            still_good.clone()
        };
        if !record.refreshable {
            return use_as_is(self);
        }
        // Asked a moment ago, or refused a moment ago: not again, unless the
        // server has just refused the token this would replace.
        if !force && held(&key) {
            return use_as_is(self);
        }
        let refresh = match self.vault.refresh(&key) {
            Ok(Some(refresh)) => refresh,
            Ok(None) => {
                // The refresh token is not there: it cannot be renewed.
                let _ = self.oauth.renewed(&self.url, record.expires_at_ms, false);
                return use_as_is(self);
            }
            // A store that could not be read (a locked keychain, a prompt that
            // was dismissed) does not make the grant unrenewable.
            Err(_) => return use_as_is(self),
        };
        let reply = self.refresh_grant(&record, &refresh);
        // The binding may have been replaced while the endpoint was being asked
        // (a rebind, a new authorization, a logout): what came back belongs to
        // the old one, and writing it by URL would undo the new one.
        if self
            .oauth
            .get(&self.url)
            .is_none_or(|now| now.credential_key != key)
        {
            return self.as_it_stands();
        }
        match reply {
            Ok(granted) => {
                // The lifetime counts from when the reply arrived, not from
                // when the (possibly slow) store was done with it.
                let received = now_ms();
                // Stored before it is used. A rotated refresh token that was
                // not stored would be lost with the token that came with it.
                let keep = granted.refresh.clone().unwrap_or(refresh);
                if self.vault.put(&key, &granted.access, Some(&keep)).is_err() {
                    self.needs_auth(&record);
                    return None;
                }
                hold(&key, HOLD_AFTER_RENEWAL_MS);
                let expires = granted.expires_in.map(|secs| {
                    received.saturating_add(i64::try_from(secs).unwrap_or(0).saturating_mul(1000))
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
                // stays, and is used while it still is good. Nobody asks it
                // again for a while.
                hold(&key, HOLD_AFTER_FAILURE_MS);
                use_as_is(self)
            }
        }
    }

    /// The refresh request, to the token endpoint recorded at authorization.
    fn refresh_grant(
        &self,
        record: &Authorization,
        refresh: &Secret,
    ) -> Result<crate::mcp_oauth::Granted, RefreshFailed> {
        // Not sent around a proxy that was asked for.
        if self.network.proxy_unusable {
            return Err(RefreshFailed::Failed);
        }
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
                    proxy_unusable: false,
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

    /// Hold the cross-process lock of the rig's credential, as another process
    /// that is in the middle of a refresh would.
    fn another_process_refreshing(rig: &Rig) -> FileLock {
        FileLock::acquire(
            rig.home
                .join(format!("mcp-refresh-{}.lock", &rig.key[..16])),
            Duration::from_secs(5),
        )
        .expect("the lock was free")
    }

    #[test]
    fn a_caller_that_cannot_take_the_lock_in_time_uses_the_token_as_it_stands_and_asks_nobody() {
        let rig = rig(WorldOptions::default(), 30_000, true);
        let other = another_process_refreshing(&rig);
        let supply = rig.supply(None).waiting(Duration::from_millis(100));
        // 30 s left, the other process has been at it for longer than this
        // caller waits: the token that is good is used, and the endpoint is
        // not asked (what the other is spending is not ours to spend again).
        assert_eq!(expose(supply.bearer()).as_deref(), Some("stale-access"));
        assert_eq!(rig.refreshes(), 0);
        assert_eq!(
            rig.vault.refresh_of(&rig.key).as_deref(),
            Some("world-refresh-canary-1")
        );
        assert!(rig.oauth.get(&rig.url).unwrap().tokens);
        // The server refused that token: it is not offered again, but a token
        // the other process has stored since is taken.
        let used = Secret::new("stale-access".to_owned());
        assert!(supply.after_rejection(&used).is_none());
        assert_eq!(rig.refreshes(), 0);
        let again = rig.supply(None).waiting(Duration::from_millis(100));
        rig.vault.seed(
            &rig.key,
            "stored-by-the-other",
            Some("world-refresh-canary-1"),
        );
        assert_eq!(
            expose(again.after_rejection(&used)).as_deref(),
            Some("stored-by-the-other")
        );
        assert_eq!(rig.refreshes(), 0);
        // Expired, and nobody to ask: nothing to send.
        rig.oauth
            .renewed(&rig.url, Some(now_ms() - 5_000), true)
            .unwrap();
        assert!(
            rig.supply(None)
                .waiting(Duration::from_millis(100))
                .bearer()
                .is_none()
        );
        assert_eq!(rig.refreshes(), 0);
        assert!(
            rig.oauth.get(&rig.url).unwrap().tokens,
            "the grant is not touched"
        );
        // The other process is done: now it is renewed.
        drop(other);
        assert_eq!(
            expose(rig.supply(None).bearer()).as_deref(),
            Some("world-access-r1")
        );
        assert_eq!(rig.refreshes(), 1);
    }

    #[test]
    fn a_refresh_that_returns_after_the_binding_was_replaced_writes_nothing() {
        let rig = rig(
            WorldOptions {
                refresh_error: Some("invalid_grant".to_owned()),
                refresh_delay: Duration::from_millis(400),
                ..WorldOptions::default()
            },
            30_000,
            true,
        );
        let supply = Arc::new(rig.supply(None));
        let renewing = {
            let supply = supply.clone();
            std::thread::spawn(move || supply.bearer().map(|t| t.expose().to_owned()))
        };
        // While the endpoint is being asked, the server is authorized again
        // with another issuer: another binding, its own tokens.
        std::thread::sleep(Duration::from_millis(150));
        let new_key = credential_key(
            &crate::mcp_auth::canonical_url(&rig.url),
            "http://127.0.0.1:9",
            "another-client",
        );
        rig.vault.seed(&new_key, "new-access", Some("new-refresh"));
        rig.oauth
            .put(
                &rig.url,
                Authorization::new(
                    "http://127.0.0.1".to_owned(),
                    "http://127.0.0.1:9".to_owned(),
                    "another-client".to_owned(),
                    true,
                    vec![],
                    "http://127.0.0.1:9/token".to_owned(),
                    None,
                    new_key.clone(),
                    Some(now_ms() + 3_600_000),
                )
                .with_refresh(true, false),
            )
            .unwrap();
        let got = renewing.join().unwrap();
        // The old grant was refused; that says nothing about the new one.
        assert_eq!(got.as_deref(), Some("new-access"));
        let record = rig.oauth.get(&rig.url).unwrap();
        assert!(record.tokens && record.refreshable, "{record:?}");
        assert_eq!(record.credential_key, new_key);
        assert_eq!(rig.vault.access_of(&new_key).as_deref(), Some("new-access"));
        assert_eq!(
            rig.vault.refresh_of(&new_key).as_deref(),
            Some("new-refresh")
        );
        assert!(rig.needs.needs_auth(&rig.url).is_none());
    }

    #[test]
    fn a_proxy_that_cannot_be_used_is_not_gone_around() {
        let rig = rig(WorldOptions::default(), 30_000, true);
        let supply = OauthSupply::new(
            &rig.url,
            rig.oauth.clone(),
            Some(rig.needs.clone()),
            rig.vault.clone(),
            rig.home.clone(),
            Network {
                proxy: None,
                proxy_unusable: true,
                receipt_root: Some(rig.home.clone()),
            },
            None,
        )
        .expect("supply");
        // Due for renewal, and the endpoint is not asked: the token that is
        // still good is used, and the grant is as it was.
        assert_eq!(expose(supply.bearer()).as_deref(), Some("stale-access"));
        assert_eq!(rig.refreshes(), 0);
        assert!(rig.oauth.get(&rig.url).unwrap().tokens);
        assert_eq!(
            rig.vault.refresh_of(&rig.key).as_deref(),
            Some("world-refresh-canary-1")
        );
    }

    #[test]
    fn a_lifetime_counts_from_the_reply_not_from_a_slow_store() {
        let rig = rig(WorldOptions::default(), 1_000, true);
        rig.vault
            .put_delay_ms
            .store(700, std::sync::atomic::Ordering::Release);
        let asked = now_ms();
        assert!(rig.supply(None).bearer().is_some());
        let expires = rig.oauth.get(&rig.url).unwrap().expires_at_ms.unwrap();
        // 3600 s from the reply, which came well before the store was done.
        assert!(expires - asked < 3_600_000 + 500, "{}", expires - asked);
    }

    #[test]
    fn exclusively_runs_its_body_only_with_both_locks() {
        let rig = rig(WorldOptions::default(), 3_600_000, true);
        let ran = std::cell::Cell::new(false);
        let wait = Duration::from_millis(100);
        assert!(exclusively(&rig.home, &rig.key, wait, || ran.set(true)).is_ok());
        assert!(ran.replace(false));
        let other = another_process_refreshing(&rig);
        assert!(exclusively(&rig.home, &rig.key, wait, || ran.set(true)).is_err());
        assert!(!ran.get(), "the body did not run without the file lock");
        drop(other);
        assert!(exclusively(&rig.home, &rig.key, wait, || ran.set(true)).is_ok());
    }

    #[test]
    fn an_endpoint_that_refused_is_not_asked_again_by_every_caller_in_turn() {
        let rig = rig(
            WorldOptions {
                refresh_error: Some("temporarily_unavailable".to_owned()),
                ..WorldOptions::default()
            },
            30_000,
            true,
        );
        // Five requests over two connections: one question, and each of them
        // is sent the token that is still good.
        let first = rig.supply(None);
        let second = rig.supply(None);
        for supply in [&first, &second, &first, &second, &first] {
            assert_eq!(expose(supply.bearer()).as_deref(), Some("stale-access"));
        }
        assert_eq!(rig.refreshes(), 1);
        assert!(rig.oauth.get(&rig.url).unwrap().tokens, "the grant stays");
        // A request the server refused is renewed whenever, held or not.
        let used = Secret::new("stale-access".to_owned());
        assert!(first.after_rejection(&used).is_none());
        assert_eq!(rig.refreshes(), 2);
    }

    #[test]
    fn a_token_that_lives_less_than_the_lead_time_is_not_renewed_on_every_request() {
        let rig = rig(
            WorldOptions {
                refresh_expires_in: 30,
                ..WorldOptions::default()
            },
            1_000,
            true,
        );
        let first = rig.supply(None);
        let second = rig.supply(None);
        for supply in [&first, &second, &first, &second, &first, &second] {
            assert_eq!(expose(supply.bearer()).as_deref(), Some("world-access-r1"));
        }
        assert_eq!(rig.refreshes(), 1, "every token it gives is stale at once");
    }

    #[test]
    fn a_keychain_that_cannot_be_read_does_not_make_the_grant_unrenewable() {
        use std::sync::atomic::Ordering::Release;
        let rig = rig(WorldOptions::default(), 30_000, true);
        // The refresh token cannot be read now (a locked keychain): the token
        // that is good is used, and nothing is concluded about the grant.
        rig.vault.fail_refresh_reads.store(true, Release);
        assert_eq!(
            expose(rig.supply(None).bearer()).as_deref(),
            Some("stale-access")
        );
        assert!(rig.oauth.get(&rig.url).unwrap().refreshable);
        // The access token cannot be read (and the refresh token can): nothing
        // to send, nobody asked, and the grant is as it was.
        rig.vault.fail_refresh_reads.store(false, Release);
        rig.vault.fail_access_reads.store(true, Release);
        assert!(rig.supply(None).bearer().is_none());
        assert_eq!(rig.refreshes(), 0);
        let record = rig.oauth.get(&rig.url).unwrap();
        assert!(record.tokens && record.refreshable);
        // Readable again: renewed as if nothing had happened.
        rig.vault.fail_access_reads.store(false, Release);
        assert_eq!(
            expose(rig.supply(None).bearer()).as_deref(),
            Some("world-access-r1")
        );
        assert_eq!(rig.refreshes(), 1);
    }

    #[test]
    fn a_token_sent_for_want_of_a_refresh_token_is_still_a_secret() {
        let rig = rig(WorldOptions::default(), 30_000, true);
        // The record says it can be renewed; the store has no refresh token.
        rig.vault.seed(&rig.key, "stale-access", None);
        let supply = rig.supply(None);
        assert_eq!(expose(supply.bearer()).as_deref(), Some("stale-access"));
        assert!(!rig.oauth.get(&rig.url).unwrap().refreshable);
        assert_eq!(supply.tokens_seen(), ["stale-access"]);
    }
}
