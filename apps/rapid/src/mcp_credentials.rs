//! What is kept of an authorized MCP server (EXT-02 §4.3 step 9, §4.8).
//!
//! Two stores, split by what a leak would cost:
//!
//! - **The tokens** — the access token and the refresh token — are
//!   credentials. They live only in the platform's keychain, as `OauthAccess`
//!   and `OauthRefresh` items, under a name derived from the server's
//!   canonical URL, its issuer and the client id (decision D-5: per user, and
//!   a token cannot follow a URL, an issuer or a client change). Where there is
//!   no keychain the answer is a typed refusal; there is no file fallback.
//! - **The metadata** — issuer, client id, scopes, the token and revocation
//!   endpoints, when it was authorized and when the token expires — is in a
//!   small owner-only file in the RapidLM home (`mcp-oauth.json`), so `rapid
//!   mcp list`, `get` and `doctor` can say the state offline without asking the
//!   keychain for anything. It holds no secret. A client id is public; a client
//!   secret is read from the environment each time and never kept.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

use auth::{CancellationToken, CredentialKind, KeychainItemMeta, PlatformKeychain, SecretRef};
use sha2::{Digest, Sha256};

use crate::mcp_auth::{FileLock, LockError};
use crate::mcp_oauth::Secret;

/// The metadata file, beside the other per-user state in the RapidLM home.
pub const OAUTH_FILE: &str = "mcp-oauth.json";

const SCHEMA: &str = "rapidlm.mcp_oauth.v1";

/// A file larger than this is not read, and one is never written larger.
const MAX_FILE_BYTES: u64 = 128 * 1024;

/// The servers one file remembers; the oldest are forgotten past it.
const MAX_SERVERS: usize = 256;

/// How long an update waits for another process's write to the same file.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// Why the keychain could not be used. Never carries a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialError {
    /// No secure store: this platform has no keychain RapidLM can use, or it
    /// refused. There is no file to fall back on.
    NoSecureStore,
    /// The keychain answered and something else went wrong.
    Failed(&'static str),
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSecureStore => f.write_str(
                "no secure credential store is available on this system, and tokens are never \
written to a file (on Linux, a Secret Service such as GNOME Keyring or KWallet is needed)",
            ),
            Self::Failed(why) => write!(f, "the credential store failed: {why}"),
        }
    }
}

/// The name a binding's tokens are kept under: a hash of the canonical URL, the
/// issuer and the client id. A change to any of the three is a different name,
/// so a token never follows one.
pub fn credential_key(canonical_url: &str, issuer: &str, client_id: &str) -> String {
    let mut hash = Sha256::new();
    for part in [canonical_url, issuer, client_id] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn item(key: &str, kind: CredentialKind) -> Result<KeychainItemMeta, CredentialError> {
    let prefix = match kind {
        CredentialKind::OauthRefresh => "mcp-oauth-refresh",
        _ => "mcp-oauth-access",
    };
    let alias = format!("{prefix}:{key}");
    let refer =
        SecretRef::from_alias(&alias).map_err(|_| CredentialError::Failed("a bad item name"))?;
    KeychainItemMeta::new(refer, kind, None).map_err(|_| CredentialError::Failed("a bad item name"))
}

fn backend() -> Result<std::sync::Arc<dyn PlatformKeychain>, CredentialError> {
    let backend =
        crate::provider_keychain::backend().map_err(|_| CredentialError::NoSecureStore)?;
    if backend.probe().is_available() {
        Ok(backend)
    } else {
        Err(CredentialError::NoSecureStore)
    }
}

/// Whether tokens can be stored here at all.
pub fn secure_store_available() -> bool {
    backend().is_ok()
}

fn previous(
    backend: &dyn PlatformKeychain,
    item: &KeychainItemMeta,
) -> Result<Option<Vec<u8>>, CredentialError> {
    match backend.get(item, &CancellationToken::new()) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(auth::StoreError::NotFound) => Ok(None),
        Err(_) => Err(CredentialError::Failed("an item could not be read")),
    }
}

fn restore(backend: &dyn PlatformKeychain, item: &KeychainItemMeta, before: Option<Vec<u8>>) {
    let cancel = CancellationToken::new();
    let _ = match before {
        Some(bytes) => backend.put(item, &bytes, &cancel),
        None => match backend.delete(item, &cancel) {
            Err(auth::StoreError::NotFound) => Ok(()),
            other => other,
        },
    };
}

/// Store the tokens of a binding: both or neither. A refresh token of `None`
/// removes a previous one (a new grant that does not rotate is not the old
/// grant's refresh token). A failure puts back what was there.
pub fn put_tokens(
    key: &str,
    access: &Secret,
    refresh: Option<&Secret>,
) -> Result<(), CredentialError> {
    let backend = backend()?;
    let access_item = item(key, CredentialKind::OauthAccess)?;
    let refresh_item = item(key, CredentialKind::OauthRefresh)?;
    let before_access = previous(backend.as_ref(), &access_item)?;
    let before_refresh = previous(backend.as_ref(), &refresh_item)?;
    let cancel = CancellationToken::new();
    let undo = |backend: &dyn PlatformKeychain| {
        restore(backend, &access_item, before_access.clone());
        restore(backend, &refresh_item, before_refresh.clone());
    };
    if backend
        .put(&access_item, access.expose().as_bytes(), &cancel)
        .is_err()
    {
        undo(backend.as_ref());
        return Err(CredentialError::Failed("the access token was not stored"));
    }
    let wrote_refresh = match refresh {
        Some(token) => backend
            .put(&refresh_item, token.expose().as_bytes(), &cancel)
            .is_ok(),
        None => matches!(
            backend.delete(&refresh_item, &cancel),
            Ok(()) | Err(auth::StoreError::NotFound)
        ),
    };
    if !wrote_refresh {
        undo(backend.as_ref());
        return Err(CredentialError::Failed("the refresh token was not stored"));
    }
    Ok(())
}

fn read(key: &str, kind: CredentialKind) -> Result<Option<Secret>, CredentialError> {
    let backend = backend()?;
    match previous(backend.as_ref(), &item(key, kind)?)? {
        Some(bytes) => String::from_utf8(bytes)
            .map(|text| Some(Secret::new(text)))
            .map_err(|_| CredentialError::Failed("a stored token is not text")),
        None => Ok(None),
    }
}

/// The access token stored for a binding, if any.
pub fn read_access(key: &str) -> Result<Option<Secret>, CredentialError> {
    read(key, CredentialKind::OauthAccess)
}

/// The refresh token stored for a binding, if any.
pub fn read_refresh(key: &str) -> Result<Option<Secret>, CredentialError> {
    read(key, CredentialKind::OauthRefresh)
}

/// Delete both tokens of a binding. Nothing there is not an error.
pub fn delete_tokens(key: &str) -> Result<(), CredentialError> {
    let backend = backend()?;
    let cancel = CancellationToken::new();
    let mut failed = false;
    for kind in [CredentialKind::OauthAccess, CredentialKind::OauthRefresh] {
        match backend.delete(&item(key, kind)?, &cancel) {
            Ok(()) | Err(auth::StoreError::NotFound) => {}
            Err(_) => failed = true,
        }
    }
    if failed {
        Err(CredentialError::Failed("a token could not be deleted"))
    } else {
        Ok(())
    }
}

/// Where the tokens of a binding are kept. The keychain, in production; a
/// test gives one that threads share, since the test keychain is the thread's.
pub trait CredentialVault: Send + Sync {
    fn access(&self, key: &str) -> Result<Option<Secret>, CredentialError>;
    fn refresh(&self, key: &str) -> Result<Option<Secret>, CredentialError>;
    /// Both or neither (a `None` refresh token removes a stored one).
    fn put(
        &self,
        key: &str,
        access: &Secret,
        refresh: Option<&Secret>,
    ) -> Result<(), CredentialError>;
    /// Both tokens; nothing there is not an error.
    fn delete(&self, key: &str) -> Result<(), CredentialError>;
}

/// The platform's keychain.
pub struct KeychainVault;

impl CredentialVault for KeychainVault {
    fn access(&self, key: &str) -> Result<Option<Secret>, CredentialError> {
        read_access(key)
    }

    fn refresh(&self, key: &str) -> Result<Option<Secret>, CredentialError> {
        read_refresh(key)
    }

    fn put(
        &self,
        key: &str,
        access: &Secret,
        refresh: Option<&Secret>,
    ) -> Result<(), CredentialError> {
        put_tokens(key, access, refresh)
    }

    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        delete_tokens(key)
    }
}

// --- the metadata ----------------------------------------------------------------

/// What is known of an authorization, none of it secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authorization {
    /// `scheme://host[:port]` of the server.
    pub origin: String,
    /// The pinned issuer (the identifier its metadata names).
    pub issuer: String,
    pub client_id: String,
    /// The client was registered by this command (dynamic registration), not
    /// named by the entry.
    pub client_dynamic: bool,
    pub scopes: Vec<String>,
    pub token_endpoint: String,
    pub revocation_endpoint: Option<String>,
    /// The name the tokens are kept under ([`credential_key`]).
    pub credential_key: String,
    /// Tokens are stored for this binding (false after a logout that kept the
    /// registered client).
    pub tokens: bool,
    pub authorized_at: String,
    /// When the access token stops being good, in milliseconds since the Unix
    /// epoch, from the instant the reply arrived; `None` is "until rejected".
    pub expires_at_ms: Option<i64>,
    /// A refresh token is stored for this binding: the access token may be
    /// renewed without the person.
    pub refreshable: bool,
    /// The client's secret (a pre-registered client's) is sent in the body of a
    /// token request, not as a Basic header: what the server's metadata said.
    pub secret_in_body: bool,
    /// Order of recording: the oldest is forgotten first.
    seq: u64,
}

impl Authorization {
    /// A new record; the store gives it its place in the order.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        origin: String,
        issuer: String,
        client_id: String,
        client_dynamic: bool,
        scopes: Vec<String>,
        token_endpoint: String,
        revocation_endpoint: Option<String>,
        credential_key: String,
        expires_at_ms: Option<i64>,
    ) -> Self {
        Self {
            origin,
            issuer,
            client_id,
            client_dynamic,
            scopes,
            token_endpoint,
            revocation_endpoint,
            credential_key,
            tokens: true,
            authorized_at: crate::headless::jsonl::now_rfc3339(),
            expires_at_ms,
            refreshable: false,
            secret_in_body: false,
            seq: 0,
        }
    }

    /// This record, for a grant that did (or did not) come with a refresh
    /// token and a client that presents its secret in the body (or not).
    pub fn with_refresh(mut self, refreshable: bool, secret_in_body: bool) -> Self {
        self.refreshable = refreshable;
        self.secret_in_body = secret_in_body;
        self
    }

    /// Seconds until the access token expires, from `now_ms`; `None` when it
    /// does not say. Negative when it already has.
    pub fn expires_in_secs(&self, now_ms: i64) -> Option<i64> {
        self.expires_at_ms
            .map(|at| at.saturating_sub(now_ms).div_euclid(1000))
    }
}

/// The milliseconds since the Unix epoch, now.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

/// How long until `at_ms`, as a person reads it: `41m`, `3h`, `2d`, or
/// `expired`.
pub fn humanize_expiry(secs: i64) -> String {
    if secs <= 0 {
        return "expired".to_owned();
    }
    match secs {
        s if s < 90 => format!("{s}s"),
        s if s < 90 * 60 => format!("{}m", (s + 30) / 60),
        s if s < 48 * 3600 => format!("{}h", (s + 1800) / 3600),
        s => format!("{}d", (s + 43200) / 86400),
    }
}

/// The metadata file.
#[derive(Clone, Debug)]
pub struct OauthStore {
    path: PathBuf,
}

impl OauthStore {
    pub fn in_home(home: &Path) -> Self {
        Self {
            path: home.join(OAUTH_FILE),
        }
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    fn lock_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_default();
        name.push(".lock");
        self.path.with_file_name(name)
    }

    fn read_all(&self) -> BTreeMap<String, Authorization> {
        use std::io::Read;
        let mut out = BTreeMap::new();
        let Ok(file) = std::fs::File::open(&self.path) else {
            return out;
        };
        if !file.metadata().is_ok_and(|meta| meta.is_file()) {
            return out;
        }
        let mut text = String::new();
        if file
            .take(MAX_FILE_BYTES + 1)
            .read_to_string(&mut text)
            .is_err()
            || text.len() as u64 > MAX_FILE_BYTES
        {
            return out;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return out;
        };
        if value["schema"] != SCHEMA {
            return out;
        }
        let Some(servers) = value["servers"].as_object() else {
            return out;
        };
        for (key, entry) in servers.iter().take(MAX_SERVERS) {
            let text_of = |name: &str| {
                entry[name]
                    .as_str()
                    .filter(|text| {
                        text.len() <= 2048 && text.bytes().all(|b| (0x21..=0x7e).contains(&b))
                    })
                    .map(str::to_owned)
            };
            let (Some(issuer), Some(client_id), Some(token_endpoint), Some(credential_key)) = (
                text_of("issuer"),
                text_of("client_id"),
                text_of("token_endpoint"),
                text_of("credential_key"),
            ) else {
                continue;
            };
            let scopes = entry["scopes"]
                .as_array()
                .map(|scopes| {
                    scopes
                        .iter()
                        .filter_map(|scope| scope.as_str())
                        .filter(|scope| {
                            !scope.is_empty()
                                && scope.len() <= crate::mcp_config::MAX_AUTH_SCOPE_BYTES
                                && scope.bytes().all(|b| (0x21..=0x7e).contains(&b))
                        })
                        .take(crate::mcp_config::MAX_AUTH_SCOPES)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            out.insert(
                key.clone(),
                Authorization {
                    origin: text_of("origin").unwrap_or_default(),
                    issuer,
                    client_id,
                    client_dynamic: entry["client_dynamic"].as_bool().unwrap_or(false),
                    scopes,
                    token_endpoint,
                    revocation_endpoint: text_of("revocation_endpoint"),
                    credential_key,
                    tokens: entry["tokens"].as_bool().unwrap_or(false),
                    refreshable: entry["refreshable"].as_bool().unwrap_or(false),
                    secret_in_body: entry["secret_in_body"].as_bool().unwrap_or(false),
                    authorized_at: entry["authorized_at"]
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .filter(|c| c.is_ascii_graphic())
                        .take(40)
                        .collect(),
                    expires_at_ms: entry["expires_at_ms"].as_i64(),
                    seq: entry["seq"].as_u64().unwrap_or(0),
                },
            );
        }
        out
    }

    fn render(servers: &mut BTreeMap<String, Authorization>) -> Result<String, String> {
        loop {
            let mut doc = serde_json::Map::new();
            for (key, a) in servers.iter() {
                doc.insert(
                    key.clone(),
                    serde_json::json!({
                        "origin": a.origin,
                        "issuer": a.issuer,
                        "client_id": a.client_id,
                        "client_dynamic": a.client_dynamic,
                        "scopes": a.scopes,
                        "token_endpoint": a.token_endpoint,
                        "revocation_endpoint": a.revocation_endpoint,
                        "credential_key": a.credential_key,
                        "tokens": a.tokens,
                        "refreshable": a.refreshable,
                        "secret_in_body": a.secret_in_body,
                        "authorized_at": a.authorized_at,
                        "expires_at_ms": a.expires_at_ms,
                        "seq": a.seq,
                    }),
                );
            }
            let text = serde_json::to_string(&serde_json::json!({
                "schema": SCHEMA,
                "servers": doc,
            }))
            .map_err(|err| err.to_string())?;
            if (text.len() as u64) < MAX_FILE_BYTES {
                return Ok(format!("{text}\n"));
            }
            let Some(oldest) = servers
                .iter()
                .min_by_key(|(_, a)| a.seq)
                .map(|(key, _)| key.clone())
            else {
                return Err("the record does not fit its file".to_owned());
            };
            servers.remove(&oldest);
        }
    }

    fn write_all(&self, servers: &mut BTreeMap<String, Authorization>) -> Result<(), String> {
        let text = Self::render(servers)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        crate::exec_tools::atomic_write_with_mode(&self.path, text.as_bytes(), Some(0o600))
            .map_err(|err| err.to_string())
    }

    /// What is recorded for the server at `url`.
    pub fn get(&self, url: &str) -> Option<Authorization> {
        self.read_all().remove(&crate::mcp_auth::key_of(url))
    }

    /// Record (or replace) the authorization of the server at `url`.
    pub fn put(&self, url: &str, mut authorization: Authorization) -> Result<(), String> {
        let _lock = self.lock()?;
        let mut servers = self.read_all();
        let key = crate::mcp_auth::key_of(url);
        while servers.len() >= MAX_SERVERS && !servers.contains_key(&key) {
            let Some(oldest) = servers
                .iter()
                .min_by_key(|(_, a)| a.seq)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            servers.remove(&oldest);
        }
        authorization.seq = servers
            .values()
            .map(|a| a.seq)
            .max()
            .map_or(1, |highest| highest.saturating_add(1));
        servers.insert(key, authorization);
        self.write_all(&mut servers)
    }

    /// Forget the server at `url`. Nothing there is not an error.
    pub fn remove(&self, url: &str) -> Result<(), String> {
        let _lock = self.lock()?;
        let mut servers = self.read_all();
        if servers.remove(&crate::mcp_auth::key_of(url)).is_none() {
            return Ok(());
        }
        self.write_all(&mut servers)
    }

    /// Mark the server's tokens gone, keeping the registered client.
    pub fn tokens_gone(&self, url: &str) -> Result<(), String> {
        let _lock = self.lock()?;
        let mut servers = self.read_all();
        let Some(record) = servers.get_mut(&crate::mcp_auth::key_of(url)) else {
            return Ok(());
        };
        record.tokens = false;
        record.refreshable = false;
        record.expires_at_ms = None;
        self.write_all(&mut servers)
    }

    /// A refresh renewed the access token: when it now expires, and whether a
    /// refresh token is still stored.
    pub fn renewed(
        &self,
        url: &str,
        expires_at_ms: Option<i64>,
        refreshable: bool,
    ) -> Result<(), String> {
        let _lock = self.lock()?;
        let mut servers = self.read_all();
        let Some(record) = servers.get_mut(&crate::mcp_auth::key_of(url)) else {
            return Ok(());
        };
        record.expires_at_ms = expires_at_ms;
        record.refreshable = refreshable;
        self.write_all(&mut servers)
    }

    fn lock(&self) -> Result<FileLock, String> {
        FileLock::acquire(self.lock_path(), LOCK_WAIT).map_err(|err| match err {
            LockError::Busy => "the authorization record is in use".to_owned(),
            LockError::Io(why) => why,
        })
    }
}

/// A vault in memory that threads share (tests only): the test keychain is the
/// thread's own, and a refresh is a thing several threads do at once.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemoryVault {
    access: Mutex<std::collections::HashMap<String, String>>,
    refresh: Mutex<std::collections::HashMap<String, String>>,
    /// Every write, in order: `(key, access, refresh)`.
    pub puts: Mutex<Vec<(String, String, Option<String>)>>,
    pub fail_puts: std::sync::atomic::AtomicBool,
    /// Reads of the access token fail (a locked keychain).
    pub fail_access_reads: std::sync::atomic::AtomicBool,
    /// Reads of the refresh token fail.
    pub fail_refresh_reads: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl MemoryVault {
    pub fn seed(&self, key: &str, access: &str, refresh: Option<&str>) {
        self.access
            .lock()
            .unwrap()
            .insert(key.to_owned(), access.to_owned());
        match refresh {
            Some(refresh) => self
                .refresh
                .lock()
                .unwrap()
                .insert(key.to_owned(), refresh.to_owned()),
            None => self.refresh.lock().unwrap().remove(key),
        };
    }

    pub fn access_of(&self, key: &str) -> Option<String> {
        self.access.lock().unwrap().get(key).cloned()
    }

    pub fn refresh_of(&self, key: &str) -> Option<String> {
        self.refresh.lock().unwrap().get(key).cloned()
    }
}

#[cfg(test)]
impl CredentialVault for MemoryVault {
    fn access(&self, key: &str) -> Result<Option<Secret>, CredentialError> {
        if self
            .fail_access_reads
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(CredentialError::Failed("an item could not be read"));
        }
        Ok(self.access_of(key).map(Secret::new))
    }

    fn refresh(&self, key: &str) -> Result<Option<Secret>, CredentialError> {
        if self
            .fail_refresh_reads
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(CredentialError::Failed("an item could not be read"));
        }
        Ok(self.refresh_of(key).map(Secret::new))
    }

    fn put(
        &self,
        key: &str,
        access: &Secret,
        refresh: Option<&Secret>,
    ) -> Result<(), CredentialError> {
        if self.fail_puts.load(std::sync::atomic::Ordering::Acquire) {
            return Err(CredentialError::Failed("the store refused"));
        }
        self.puts.lock().unwrap().push((
            key.to_owned(),
            access.expose().to_owned(),
            refresh.map(|token| token.expose().to_owned()),
        ));
        self.seed(key, access.expose(), refresh.map(Secret::expose));
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        self.access.lock().unwrap().remove(key);
        self.refresh.lock().unwrap().remove(key);
        Ok(())
    }
}

/// An in-memory keychain for tests.
#[cfg(test)]
pub(crate) fn with_memory_keychain<R>(
    body: impl FnOnce(&std::sync::Arc<MemoryKeychain>) -> R,
) -> R {
    let keychain = std::sync::Arc::new(MemoryKeychain::default());
    crate::provider_keychain::with_backend(keychain.clone(), || body(&keychain))
}

#[cfg(test)]
pub(crate) use crate::provider_keychain::testing::MemoryKeychain;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rapid-mcp-cred-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn secret(text: &str) -> Secret {
        Secret::new(text.to_owned())
    }

    fn sample(key: &str) -> Authorization {
        Authorization::new(
            "https://mcp.example".to_owned(),
            "https://as.example".to_owned(),
            "client-1".to_owned(),
            true,
            vec!["read".to_owned(), "write".to_owned()],
            "https://as.example/token".to_owned(),
            Some("https://as.example/revoke".to_owned()),
            key.to_owned(),
            Some(1_000_000),
        )
    }

    #[test]
    fn a_binding_is_named_by_the_url_the_issuer_and_the_client_together() {
        let base = credential_key("https://m.example/mcp", "https://as.example", "c1");
        assert_eq!(base.len(), 64);
        assert_eq!(
            base,
            credential_key("https://m.example/mcp", "https://as.example", "c1")
        );
        // A change to any one is a different name: a token never follows one.
        for other in [
            credential_key("https://m.example/other", "https://as.example", "c1"),
            credential_key("https://m.example/mcp", "https://as2.example", "c1"),
            credential_key("https://m.example/mcp", "https://as.example", "c2"),
            // The parts are length-prefixed: moving a boundary is a change.
            credential_key("https://m.example/mcp", "https://as.examplec", "1"),
            credential_key("https://m.example/mcpx", "https://as.example", "1"),
        ] {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn tokens_go_to_the_keychain_as_two_kinds_and_come_back() {
        with_memory_keychain(|keychain| {
            put_tokens(
                "k1",
                &secret("access-canary"),
                Some(&secret("refresh-canary")),
            )
            .expect("stored");
            assert_eq!(
                read_access("k1")
                    .expect("read")
                    .map(|s| s.expose().to_owned()),
                Some("access-canary".to_owned())
            );
            assert_eq!(
                read_refresh("k1")
                    .expect("read")
                    .map(|s| s.expose().to_owned()),
                Some("refresh-canary".to_owned())
            );
            let names: Vec<String> = keychain.items.lock().unwrap().keys().cloned().collect();
            assert_eq!(names.len(), 2, "{names:?}");
            assert!(names.iter().any(|n| n.starts_with("mcp-oauth-access:")));
            assert!(names.iter().any(|n| n.starts_with("mcp-oauth-refresh:")));
            // Another binding's tokens are not these.
            assert_eq!(read_access("k2").expect("read"), None);
            // A grant with no refresh token drops the old one.
            put_tokens("k1", &secret("access-2"), None).expect("stored");
            assert_eq!(read_refresh("k1").expect("read"), None);
            assert_eq!(
                read_access("k1")
                    .expect("read")
                    .map(|s| s.expose().to_owned()),
                Some("access-2".to_owned())
            );
            delete_tokens("k1").expect("deleted");
            assert_eq!(read_access("k1").expect("read"), None);
            delete_tokens("k1").expect("nothing to delete is fine");
        });
    }

    #[test]
    fn no_keychain_is_a_typed_refusal_and_nothing_is_written_anywhere() {
        let dir = temp("nostore");
        let unavailable = std::sync::Arc::new(MemoryKeychain::unavailable());
        crate::provider_keychain::with_backend(unavailable, || {
            assert!(!secure_store_available());
            assert_eq!(
                put_tokens("k", &secret("a"), None).unwrap_err(),
                CredentialError::NoSecureStore
            );
            assert_eq!(
                read_access("k").unwrap_err(),
                CredentialError::NoSecureStore
            );
            assert!(
                CredentialError::NoSecureStore
                    .to_string()
                    .contains("never written to a file")
            );
        });
        // And with no backend installed at all (a test must not reach the real one).
        assert!(!secure_store_available());
        assert_eq!(
            std::fs::read_dir(&dir).expect("dir").count(),
            0,
            "no file fallback"
        );
    }

    #[test]
    fn a_failed_second_write_puts_the_first_back() {
        with_memory_keychain(|keychain| {
            put_tokens("k", &secret("old-access"), Some(&secret("old-refresh"))).expect("stored");
            let items_before = keychain.items.lock().unwrap().clone();
            // The access token's write is the first put and succeeds; the
            // refresh token's is the second and fails.
            struct FailSecond(std::sync::Arc<MemoryKeychain>, std::sync::Mutex<usize>);
            impl PlatformKeychain for FailSecond {
                fn probe(&self) -> auth::KeychainProbe {
                    self.0.probe()
                }
                fn put(
                    &self,
                    item: &KeychainItemMeta,
                    secret: &[u8],
                    cancel: &CancellationToken,
                ) -> Result<(), auth::StoreError> {
                    let mut seen = self.1.lock().unwrap();
                    *seen += 1;
                    if *seen == 2 {
                        return Err(auth::StoreError::PersistenceBlocked {
                            reason: auth::PersistenceBlockReason::KeychainUnavailable,
                        });
                    }
                    drop(seen);
                    self.0.put(item, secret, cancel)
                }
                fn get(
                    &self,
                    item: &KeychainItemMeta,
                    cancel: &CancellationToken,
                ) -> Result<Vec<u8>, auth::StoreError> {
                    self.0.get(item, cancel)
                }
                fn delete(
                    &self,
                    item: &KeychainItemMeta,
                    cancel: &CancellationToken,
                ) -> Result<(), auth::StoreError> {
                    self.0.delete(item, cancel)
                }
                fn list(
                    &self,
                    cancel: &CancellationToken,
                ) -> Result<Vec<KeychainItemMeta>, auth::StoreError> {
                    self.0.list(cancel)
                }
            }
            let failing =
                std::sync::Arc::new(FailSecond(keychain.clone(), std::sync::Mutex::new(0)));
            let result = crate::provider_keychain::with_backend(failing, || {
                put_tokens("k", &secret("new-access"), Some(&secret("new-refresh")))
            });
            assert!(
                matches!(result, Err(CredentialError::Failed(_))),
                "{result:?}"
            );
            assert_eq!(
                *keychain.items.lock().unwrap(),
                items_before,
                "both tokens are as they were"
            );
        });
    }

    #[test]
    fn the_metadata_holds_no_secret_survives_a_restart_and_is_owner_only() {
        let dir = temp("meta");
        let store = OauthStore::in_home(&dir);
        let url = "https://user:pw@Mcp.example/s/PATH-CANARY/mcp?token=Q#f";
        store.put(url, sample("ck")).expect("put");
        let text = std::fs::read_to_string(dir.join(OAUTH_FILE)).expect("file");
        for canary in ["PATH-CANARY", "pw@", "token=Q"] {
            assert!(!text.contains(canary), "{canary}: {text}");
        }
        let again = OauthStore::in_home(&dir);
        let got = again.get(url).expect("recorded");
        assert_eq!(got.issuer, "https://as.example");
        assert_eq!(got.client_id, "client-1");
        assert!(got.client_dynamic && got.tokens);
        assert_eq!(got.scopes, ["read", "write"]);
        assert_eq!(got.expires_at_ms, Some(1_000_000));
        assert_eq!(got.credential_key, "ck");
        // The same server by another spelling of its URL.
        assert!(again.get("https://mcp.example/s/PATH-CANARY/mcp").is_some());
        assert!(again.get("https://mcp.example/s/other/mcp").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(OAUTH_FILE))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
        again.tokens_gone(url).expect("gone");
        let after = again.get(url).expect("client kept");
        assert!(!after.tokens);
        assert_eq!(after.expires_at_ms, None);
        assert_eq!(after.client_id, "client-1");
        again.remove(url).expect("removed");
        assert!(again.get(url).is_none());
        again.remove(url).expect("nothing to remove is fine");
    }

    #[test]
    fn a_file_that_is_not_ours_or_is_too_big_says_nothing_and_the_oldest_go_first() {
        let dir = temp("meta-bad");
        let path = dir.join(OAUTH_FILE);
        let store = OauthStore::at(path.clone());
        for content in [
            "not json".to_owned(),
            r#"{"schema":"other","servers":{}}"#.to_owned(),
            "x".repeat(MAX_FILE_BYTES as usize + 1),
        ] {
            std::fs::write(&path, &content).expect("write");
            assert!(store.get("https://a.example/").is_none());
        }
        // An entry missing what a refresh needs is not taken.
        std::fs::write(
            &path,
            format!(
                r#"{{"schema":"{SCHEMA}","servers":{{"{}":{{"issuer":"https://as"}}}}}}"#,
                crate::mcp_auth::key_of("https://a.example/")
            ),
        )
        .expect("write");
        assert!(store.get("https://a.example/").is_none());
        // Past the count, the first recorded goes.
        std::fs::remove_file(&path).expect("remove");
        let url = |n: usize| format!("https://s{:04}.example/mcp", 9999 - n);
        for n in 0..=MAX_SERVERS + 2 {
            store.put(&url(n), sample("k")).expect("put");
        }
        assert!(store.get(&url(0)).is_none());
        assert!(store.get(&url(2)).is_none());
        assert!(store.get(&url(3)).is_some());
        assert!(store.get(&url(MAX_SERVERS + 2)).is_some());
    }

    #[test]
    fn expiry_is_computed_from_the_instant_and_read_as_a_person_reads_it() {
        let a = Authorization {
            expires_at_ms: Some(10_000_000),
            ..sample("k")
        };
        assert_eq!(
            a.expires_in_secs(10_000_000 - 41 * 60 * 1000),
            Some(41 * 60)
        );
        assert_eq!(a.expires_in_secs(10_000_000 + 5000), Some(-5));
        let none = Authorization {
            expires_at_ms: None,
            ..sample("k")
        };
        assert_eq!(none.expires_in_secs(0), None);
        for (secs, said) in [
            (-1, "expired"),
            (0, "expired"),
            (45, "45s"),
            (41 * 60, "41m"),
            (3 * 3600, "3h"),
            (3 * 86400, "3d"),
        ] {
            assert_eq!(humanize_expiry(secs), said, "{secs}");
        }
    }
}
