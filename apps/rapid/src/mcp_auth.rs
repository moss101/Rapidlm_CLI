//! The authorization state of a remote MCP server (EXT-02 §4.1).
//!
//! A remote server is in one of a few states, and every one is a projection of
//! what the entry says and what has been recorded about the server — never a
//! thing this module decides on its own:
//!
//! | state | meaning |
//! |---|---|
//! | `static` | a static credential is configured; the flow never engages |
//! | `disabled` | the entry says `auth: false`, or its URL is plain `http` off this machine (a token does not cross an unencrypted network) |
//! | `not_required` | the server has never answered "authorization required" |
//! | `needs_auth` | a `401`, or a `403` that names the scope, arrived and has not been resolved |
//!
//! What has been recorded is a small, non-secret file in the RapidLM home. It
//! holds no token, no code, no verifier and no client secret — and not the URL
//! either, since a URL can carry a secret in its path: a server is keyed by a
//! hash of its canonical URL, beside its origin. It says only that a server
//! asked, and for which scopes, so `rapid mcp list`, `rapid mcp get` and
//! `rapid doctor` can say it without starting anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::exec_tools::{McpAuthMode, McpHttpEndpoint};

/// The file, beside the other per-user state in the RapidLM home.
pub const AUTH_STATE_FILE: &str = "mcp-auth-state.json";

const SCHEMA: &str = "rapidlm.mcp_auth_state.v2";

/// A file larger than this is not read, and one is never written larger: the
/// oldest entries are forgotten first.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The servers one file remembers; the oldest are forgotten past it.
const MAX_SERVERS: usize = 256;

/// How long a record waits for another process's write to the same file.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// What a `needs_auth` server asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NeedsAuth {
    /// The scopes the server's challenge named, bounded and clean.
    pub scopes: Vec<String>,
    /// The server answered `403 insufficient_scope`: it had accepted a
    /// credential and wants a broader one.
    pub insufficient_scope: bool,
}

/// The state of one remote server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthState {
    Static,
    Disabled,
    NotRequired,
    NeedsAuth(NeedsAuth),
}

impl AuthState {
    /// The word the reports show.
    pub fn word(&self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Disabled => "disabled",
            Self::NotRequired => "not_required",
            Self::NeedsAuth(_) => "needs_auth",
        }
    }
}

/// Whether a header of this name carries a credential: `Authorization`, or a
/// name that says it is a key or a token (`X-Api-Key`, `X-Auth-Token`). The
/// one rule the loader (a credential over plain `http` is refused) and the
/// flow (a credential of its own ends it) share.
pub fn credential_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "authorization" || name.contains("key") || name.contains("token")
}

/// Whether the entry carries a credential of its own: a bearer token or a
/// header value read from the environment, or a header that carries one. The
/// authorization flow never engages for such a server, and a `401` from it is
/// a plain failure.
pub fn is_static(http: &McpHttpEndpoint) -> bool {
    http.bearer_token_env.is_some()
        || !http.headers_env.is_empty()
        || http.headers.iter().any(|(name, _)| credential_header(name))
}

/// Whether a token may be sent to this URL: `https`, or `http` on this
/// machine. A token does not cross an unencrypted network.
pub fn secure_enough(url: &str) -> bool {
    let starts = |prefix: &str| {
        url.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    };
    if starts("https://") {
        return true;
    }
    if !starts("http://") {
        return false;
    }
    // Read leniently, the way the connection does: whatever the flow later
    // finds wrong with the address, this only says whether a token may cross
    // the wire to it.
    let authority = url["http://".len()..]
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or_default();
    let host = if authority.starts_with('[') {
        authority.split_inclusive(']').next().unwrap_or_default()
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    }
    .to_ascii_lowercase();
    host == "localhost"
        || host == "[::1]"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
}

/// The state of an entry, given what has been recorded.
pub fn state_of(http: &McpHttpEndpoint, store: Option<&AuthStateStore>) -> AuthState {
    if is_static(http) {
        return AuthState::Static;
    }
    if matches!(http.auth, McpAuthMode::Never) || !secure_enough(&http.url) {
        return AuthState::Disabled;
    }
    match store.and_then(|store| store.needs_auth(&http.url)) {
        Some(need) => AuthState::NeedsAuth(need),
        None => AuthState::NotRequired,
    }
}

/// Whether a `401` from this server may start the flow: it has no static
/// credential, does not say `auth: false`, and is reached over `https` (or
/// `http` on this machine).
pub fn engages(http: &McpHttpEndpoint) -> bool {
    !is_static(http) && !matches!(http.auth, McpAuthMode::Never) && secure_enough(&http.url)
}

/// The URL a server is known by: lowercase scheme and host, no name or
/// password, the default port left out, the path as given (an empty one is
/// `/`), and no query or fragment — the parts of a URL that can carry a token
/// are never kept.
pub fn canonical_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.split(['?', '#']).next().unwrap_or_default().to_owned();
    };
    let scheme = scheme.to_ascii_lowercase();
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    // Whatever ends in an `@` is a name and password, not a host.
    let authority = authority.rsplit('@').next().unwrap_or_default();
    let authority = authority.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => ":443",
        "http" => ":80",
        _ => "",
    };
    let authority = authority
        .strip_suffix(default_port)
        .filter(|_| !default_port.is_empty())
        .unwrap_or(&authority);
    format!("{scheme}://{authority}{path}")
}

/// `scheme://host[:port]` of a canonical URL: what the file says of a server
/// besides its hash.
pub(crate) fn origin_of(canonical: &str) -> String {
    match canonical.split_once("://") {
        Some((scheme, rest)) => {
            format!("{scheme}://{}", rest.split('/').next().unwrap_or_default())
        }
        None => String::new(),
    }
}

/// The hash a server's URL is recorded under.
pub(crate) fn key_of(url: &str) -> String {
    Sha256::digest(canonical_url(url).as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Why a lock on a file in the home was not taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockError {
    /// Another holder kept it past the wait.
    Busy,
    Io(String),
}

/// A lock on a file in the home, held while a read-modify-write of it runs
/// (or, for the flow, while one authorization runs). It is the operating
/// system's advisory lock on a file that stays where it is: a process that
/// dies, however it dies, releases it, so there is no stale lock to take over
/// and no race in taking one over.
pub struct FileLock {
    file: std::fs::File,
}

impl FileLock {
    pub fn acquire(path: PathBuf, wait: Duration) -> Result<Self, LockError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| LockError::Io(err.to_string()))?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| LockError::Io(err.to_string()))?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if started.elapsed() >= wait {
                        return Err(LockError::Busy);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(std::fs::TryLockError::Error(err)) => {
                    return Err(LockError::Io(err.to_string()));
                }
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// What is recorded about one server.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Recorded {
    need: NeedsAuth,
    origin: String,
    since: String,
    /// Order of recording: the oldest is forgotten first.
    seq: u64,
}

/// The record of which servers asked for authorization.
#[derive(Clone, Debug)]
pub struct AuthStateStore {
    path: PathBuf,
}

impl AuthStateStore {
    /// The store in a RapidLM home.
    pub fn in_home(home: &Path) -> Self {
        Self {
            path: home.join(AUTH_STATE_FILE),
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

    /// Everything recorded. A file that is missing, too large or not what this
    /// module wrote says nothing: the state it would have given is
    /// `not_required`, and the next `401` records itself again.
    fn read(&self) -> BTreeMap<String, Recorded> {
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
            if entry["state"] != "needs_auth" {
                continue;
            }
            let scopes = entry["scopes"]
                .as_array()
                .map(|scopes| {
                    scopes
                        .iter()
                        .filter_map(|scope| scope.as_str())
                        .filter(|scope| valid_scope(scope))
                        .take(crate::mcp_config::MAX_AUTH_SCOPES)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            out.insert(
                key.clone(),
                Recorded {
                    need: NeedsAuth {
                        scopes,
                        insufficient_scope: entry["insufficient_scope"].as_bool().unwrap_or(false),
                    },
                    origin: entry["origin"]
                        .as_str()
                        .filter(|origin| {
                            origin.len() <= 300
                                && origin.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
                        })
                        .unwrap_or_default()
                        .to_owned(),
                    since: entry["since"].as_str().unwrap_or_default().to_owned(),
                    seq: entry["seq"].as_u64().unwrap_or(0),
                },
            );
        }
        out
    }

    /// The file's text for `servers`, the oldest entries dropped until it is
    /// within [`MAX_FILE_BYTES`] (a reader refuses a larger one, and would
    /// otherwise lose every entry at once).
    fn render(servers: &mut BTreeMap<String, Recorded>) -> Result<String, String> {
        loop {
            let mut doc = serde_json::Map::new();
            for (key, recorded) in servers.iter() {
                doc.insert(
                    key.clone(),
                    serde_json::json!({
                        "state": "needs_auth",
                        "origin": recorded.origin,
                        "scopes": recorded.need.scopes,
                        "insufficient_scope": recorded.need.insufficient_scope,
                        "since": recorded.since,
                        "seq": recorded.seq,
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
                .min_by_key(|(_, recorded)| recorded.seq)
                .map(|(key, _)| key.clone())
            else {
                return Err("the record does not fit its file".to_owned());
            };
            servers.remove(&oldest);
        }
    }

    fn write(&self, servers: &mut BTreeMap<String, Recorded>) -> Result<(), String> {
        let text = Self::render(servers)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        // A unique temporary file at the final mode, and a rename: a reader
        // sees the old file or the new one, never half of either, and the
        // content is never there at a wider mode.
        crate::exec_tools::atomic_write_with_mode(&self.path, text.as_bytes(), Some(0o600))
            .map_err(|err| err.to_string())
    }

    /// What the server at `url` last asked for, if it asked and nothing has
    /// resolved it.
    pub fn needs_auth(&self, url: &str) -> Option<NeedsAuth> {
        self.read()
            .remove(&key_of(url))
            .map(|recorded| recorded.need)
    }

    /// Record that the server at `url` asked for authorization. Best effort: a
    /// state that cannot be written is said to the caller and costs the
    /// session nothing (the marker is in the tool list either way).
    pub fn record_needs_auth(&self, url: &str, need: &NeedsAuth) -> Result<(), String> {
        let _lock = FileLock::acquire(self.lock_path(), LOCK_WAIT)
            .map_err(|err| format!("the record is in use: {err:?}"))?;
        let mut servers = self.read();
        let key = key_of(url);
        let need = NeedsAuth {
            scopes: need
                .scopes
                .iter()
                .filter(|scope| valid_scope(scope))
                .take(crate::mcp_config::MAX_AUTH_SCOPES)
                .cloned()
                .collect(),
            insufficient_scope: need.insufficient_scope,
        };
        if servers.get(&key).map(|recorded| &recorded.need) == Some(&need) {
            return Ok(());
        }
        while servers.len() >= MAX_SERVERS && !servers.contains_key(&key) {
            let Some(oldest) = servers
                .iter()
                .min_by_key(|(_, recorded)| recorded.seq)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            servers.remove(&oldest);
        }
        let seq = servers
            .values()
            .map(|recorded| recorded.seq)
            .max()
            .map_or(1, |highest| highest.saturating_add(1));
        servers.insert(
            key,
            Recorded {
                need,
                origin: origin_of(&canonical_url(url)),
                since: crate::headless::jsonl::now_rfc3339(),
                seq,
            },
        );
        self.write(&mut servers)
    }

    /// The server at `url` no longer asks: forget that it did.
    pub fn clear(&self, url: &str) -> Result<(), String> {
        let _lock = FileLock::acquire(self.lock_path(), LOCK_WAIT)
            .map_err(|err| format!("the record is in use: {err:?}"))?;
        let mut servers = self.read();
        if servers.remove(&key_of(url)).is_none() {
            return Ok(());
        }
        self.write(&mut servers)
    }
}

/// A scope as the record keeps it, and as a person or a model is shown it:
/// letters, digits and the few marks scope names use (`repo:read`,
/// `https://api.example/auth/drive`, `user@org`). A server chooses these, so
/// nothing that could read as markup or a command passes.
fn valid_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope.len() <= crate::mcp_config::MAX_AUTH_SCOPE_BYTES
        && scope
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:/@-".contains(&byte))
}

/// The scopes of a challenge's `scope` parameter, as the record keeps them.
pub fn scopes_of(challenge_scope: Option<&str>) -> Vec<String> {
    challenge_scope
        .unwrap_or_default()
        .split_ascii_whitespace()
        .filter(|scope| valid_scope(scope))
        .take(crate::mcp_config::MAX_AUTH_SCOPES)
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rapid-mcp-auth-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn remote(url: &str) -> McpHttpEndpoint {
        McpHttpEndpoint {
            url: url.to_owned(),
            headers: Vec::new(),
            headers_env: Vec::new(),
            bearer_token_env: None,
            auth: McpAuthMode::Engage,
        }
    }

    #[test]
    fn a_server_is_known_by_its_url_without_the_part_that_can_carry_a_token() {
        for (written, canonical) in [
            ("https://Example.COM/mcp", "https://example.com/mcp"),
            ("https://example.com:443/mcp", "https://example.com/mcp"),
            ("http://example.com:80", "http://example.com/"),
            ("https://example.com", "https://example.com/"),
            (
                "https://example.com/mcp?token=abc#frag",
                "https://example.com/mcp",
            ),
            ("http://127.0.0.1:8123/m", "http://127.0.0.1:8123/m"),
            ("https://example.com:8443/m", "https://example.com:8443/m"),
            // A name and password are not part of which server it is.
            ("https://user:pw@Example.com/mcp", "https://example.com/mcp"),
            ("https://tok@example.com:443/mcp", "https://example.com/mcp"),
            // An `@` after the host belongs to the path, not to a login.
            ("https://example.com/a@b/mcp", "https://example.com/a@b/mcp"),
            ("HTTPS://EXAMPLE.com:443", "https://example.com/"),
            ("http://[::1]:80/mcp", "http://[::1]/mcp"),
            ("http://[::1]:8080/mcp", "http://[::1]:8080/mcp"),
            (
                "https://example.com:1443/mcp",
                "https://example.com:1443/mcp",
            ),
        ] {
            assert_eq!(canonical_url(written), canonical, "{written}");
        }
        // A different path or port is a different server: tokens never follow
        // a URL change.
        assert_ne!(
            canonical_url("https://example.com/a"),
            canonical_url("https://example.com/b")
        );
    }

    #[test]
    fn what_an_entry_carries_decides_whether_the_flow_can_start_and_what_it_reads_as() {
        let store = AuthStateStore::at(temp("states").join(AUTH_STATE_FILE));
        let plain = remote("https://example.com/mcp");
        assert!(engages(&plain));
        assert_eq!(state_of(&plain, Some(&store)), AuthState::NotRequired);
        store
            .record_needs_auth(
                &plain.url,
                &NeedsAuth {
                    scopes: vec!["read".to_owned()],
                    insufficient_scope: false,
                },
            )
            .expect("recorded");
        assert_eq!(
            state_of(&plain, Some(&store)),
            AuthState::NeedsAuth(NeedsAuth {
                scopes: vec!["read".to_owned()],
                insufficient_scope: false
            })
        );
        // No store: nothing is known, which reads as the server never asking.
        assert_eq!(state_of(&plain, None), AuthState::NotRequired);
        // A static credential, of any of the three kinds, ends the flow.
        for http in [
            McpHttpEndpoint {
                bearer_token_env: Some("MCP_TOKEN".to_owned()),
                ..remote(&plain.url)
            },
            McpHttpEndpoint {
                headers_env: vec![("X-Key".to_owned(), "MCP_KEY".to_owned())],
                ..remote(&plain.url)
            },
            McpHttpEndpoint {
                headers: vec![("authorization".to_owned(), "Bearer x".to_owned())],
                ..remote(&plain.url)
            },
        ] {
            assert!(!engages(&http));
            assert_eq!(state_of(&http, Some(&store)), AuthState::Static);
        }
        // A credential beside `auth: false` is still the credential in use.
        let both = McpHttpEndpoint {
            bearer_token_env: Some("MCP_TOKEN".to_owned()),
            auth: McpAuthMode::Never,
            ..remote(&plain.url)
        };
        assert_eq!(state_of(&both, Some(&store)), AuthState::Static);
        // `auth: false` never engages, and says so.
        let never = McpHttpEndpoint {
            auth: McpAuthMode::Never,
            ..remote(&plain.url)
        };
        assert!(!engages(&never));
        assert_eq!(state_of(&never, Some(&store)), AuthState::Disabled);
        // A header that is not a credential is not one.
        let tenant = McpHttpEndpoint {
            headers: vec![("X-Tenant".to_owned(), "acme".to_owned())],
            ..remote(&plain.url)
        };
        assert!(engages(&tenant));
        // A key or token header is one, whichever way it is spelled: the same
        // rule the loader applies to a credential over plain `http`.
        for name in ["X-Api-Key", "x-auth-token", "Authorization", "apikey"] {
            let http = McpHttpEndpoint {
                headers: vec![(name.to_owned(), "k".to_owned())],
                ..remote(&plain.url)
            };
            assert!(is_static(&http), "{name}");
            assert!(!engages(&http), "{name}");
            assert_eq!(state_of(&http, Some(&store)), AuthState::Static, "{name}");
        }
    }

    #[test]
    fn a_token_is_not_sent_over_plain_http_off_this_machine() {
        let store = AuthStateStore::at(temp("plain").join(AUTH_STATE_FILE));
        for url in [
            "http://mcp.corp/mcp",
            "http://10.0.0.5:8080/mcp",
            "http://192.168.1.2/mcp",
        ] {
            let http = remote(url);
            assert!(!secure_enough(url), "{url}");
            assert!(!engages(&http), "{url}");
            assert_eq!(state_of(&http, Some(&store)), AuthState::Disabled, "{url}");
        }
        for url in [
            "https://mcp.example/mcp",
            "http://127.0.0.1:8123/mcp",
            "http://localhost/mcp",
        ] {
            assert!(secure_enough(url), "{url}");
            assert!(engages(&remote(url)), "{url}");
        }
        assert!(!secure_enough("not a url"));
    }

    #[test]
    fn the_record_holds_no_secret_and_survives_a_restart_and_a_url_spelled_two_ways() {
        let dir = temp("record");
        let store = AuthStateStore::in_home(&dir);
        let need = NeedsAuth {
            scopes: vec!["repo:read".to_owned(), "issues".to_owned()],
            insufficient_scope: true,
        };
        store
            .record_needs_auth("https://Example.com:443/mcp?token=SECRET#x", &need)
            .expect("recorded");
        let text = std::fs::read_to_string(dir.join(AUTH_STATE_FILE)).expect("file");
        assert!(!text.contains("SECRET"), "{text}");
        // Keyed by a hash, beside the origin: the URL itself is not kept.
        assert!(
            text.contains("\"origin\":\"https://example.com\""),
            "{text}"
        );
        assert!(!text.contains("example.com/mcp"), "{text}");
        // Another process reads it.
        let again = AuthStateStore::in_home(&dir);
        assert_eq!(
            again.needs_auth("https://example.com/mcp"),
            Some(need.clone())
        );
        // Recording the same thing again leaves the file as it is.
        let before = std::fs::read(dir.join(AUTH_STATE_FILE)).expect("file");
        again
            .record_needs_auth("https://example.com/mcp", &need)
            .expect("recorded");
        assert_eq!(
            before,
            std::fs::read(dir.join(AUTH_STATE_FILE)).expect("file")
        );
        // Cleared once the server stops asking; clearing nothing is fine.
        again.clear("https://example.com/mcp").expect("cleared");
        assert_eq!(again.needs_auth("https://example.com/mcp"), None);
        again
            .clear("https://example.com/mcp")
            .expect("nothing to clear");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            store
                .record_needs_auth("https://example.com/mcp", &need)
                .expect("recorded");
            let mode = std::fs::metadata(dir.join(AUTH_STATE_FILE))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "the file is the user's alone: {mode:o}");
        }
    }

    #[test]
    fn a_file_that_is_not_ours_or_is_too_big_or_damaged_says_nothing() {
        let dir = temp("damaged");
        let path = dir.join(AUTH_STATE_FILE);
        let store = AuthStateStore::at(path.clone());
        for content in [
            "not json".to_owned(),
            r#"{"schema":"something.else","servers":{"https://a/":{"state":"needs_auth"}}}"#
                .to_owned(),
            r#"{"schema":"rapidlm.mcp_auth_state.v2","servers":[]}"#.to_owned(),
            // The first shape of this file, which keyed servers by URL.
            r#"{"schema":"rapidlm.mcp_auth_state.v1","servers":{"https://a/":{"state":"needs_auth"}}}"#
                .to_owned(),
            "x".repeat(MAX_FILE_BYTES as usize + 1),
        ] {
            std::fs::write(&path, &content).expect("write");
            assert_eq!(
                store.needs_auth("https://a/"),
                None,
                "{}",
                &content[..20.min(content.len())]
            );
        }
        // A damaged file is replaced by the next record, not extended.
        std::fs::write(&path, "garbage").expect("write");
        store
            .record_needs_auth("https://a/", &NeedsAuth::default())
            .expect("recorded");
        assert_eq!(store.needs_auth("https://a/"), Some(NeedsAuth::default()));
        // A directory where the file belongs is no file.
        let blocked = AuthStateStore::at(dir.clone());
        assert_eq!(blocked.needs_auth("https://a/"), None);
    }

    #[test]
    fn what_a_server_names_as_a_scope_is_kept_only_as_far_as_it_is_clean_and_bounded() {
        assert_eq!(
            scopes_of(Some("read  write\tadmin")),
            ["read", "write", "admin"]
        );
        assert_eq!(scopes_of(None), Vec::<String>::new());
        assert_eq!(scopes_of(Some("a\"b ok")), ["ok"]);
        // A server chooses these, and a model reads them: nothing that could
        // read as markup, a link or a command passes.
        assert_eq!(
            scopes_of(Some(
                "repo:read https://api.example/auth/drive user@org a-b_c.d"
            )),
            [
                "repo:read",
                "https://api.example/auth/drive",
                "user@org",
                "a-b_c.d"
            ]
        );
        for hostile in [
            "`curl", "a|b", "<b>", "[x](u)", "a;b", "$(x)", "a&b", "a=b", "{x}", "a*b",
        ] {
            assert!(scopes_of(Some(hostile)).is_empty(), "{hostile}");
        }
        assert_eq!(scopes_of(Some(&"s ".repeat(40))).len(), 16);
        assert!(scopes_of(Some(&"x".repeat(65))).is_empty());
        // And the file keeps no more than that, whatever it is handed.
        let dir = temp("scopes");
        let store = AuthStateStore::in_home(&dir);
        store
            .record_needs_auth(
                "https://example.com/mcp",
                &NeedsAuth {
                    scopes: vec!["ok".to_owned(), "has space".to_owned(), "x".repeat(65)],
                    insufficient_scope: false,
                },
            )
            .expect("recorded");
        assert_eq!(
            store
                .needs_auth("https://example.com/mcp")
                .map(|need| need.scopes),
            Some(vec!["ok".to_owned()])
        );
    }

    #[test]
    fn the_file_remembers_a_bounded_number_of_servers_and_forgets_the_oldest() {
        let dir = temp("bounded");
        let store = AuthStateStore::in_home(&dir);
        // Named so that the order of the names is the reverse of the order of
        // recording: it is recording, not the name, that decides who is oldest.
        let url = |n: usize| format!("https://s{:04}.example.com/mcp", 9999 - n);
        for n in 0..=MAX_SERVERS + 3 {
            store
                .record_needs_auth(&url(n), &NeedsAuth::default())
                .expect("recorded");
        }
        let kept = store.read();
        assert_eq!(kept.len(), MAX_SERVERS);
        for forgotten in 0..4 {
            assert_eq!(store.needs_auth(&url(forgotten)), None, "{forgotten}");
        }
        for kept in [4, 5, MAX_SERVERS + 2, MAX_SERVERS + 3] {
            assert!(store.needs_auth(&url(kept)).is_some(), "{kept}");
        }
    }

    #[test]
    fn the_file_never_grows_past_what_a_reader_takes_and_loses_only_the_oldest() {
        let dir = temp("size");
        let store = AuthStateStore::in_home(&dir);
        let long = NeedsAuth {
            scopes: (0..16)
                .map(|n| format!("{n:02}{}", "s".repeat(60)))
                .collect(),
            insufficient_scope: true,
        };
        let url = |n: usize| format!("https://s{n:04}.example.com/{}", "p".repeat(900));
        for n in 0..200 {
            store.record_needs_auth(&url(n), &long).expect("recorded");
            let size = std::fs::metadata(dir.join(AUTH_STATE_FILE))
                .expect("meta")
                .len();
            assert!(size <= MAX_FILE_BYTES, "{n}: {size}");
        }
        // Still readable: the newest are there, the oldest went first.
        assert_eq!(store.needs_auth(&url(199)), Some(long.clone()));
        assert_eq!(store.needs_auth(&url(0)), None);
        let kept = store.read().len();
        assert!(kept > 10 && kept < 200, "{kept}");
    }

    #[test]
    fn writers_in_one_process_neither_lose_each_other_nor_leave_a_torn_file() {
        let dir = temp("writers");
        let writers: Vec<_> = (0..8)
            .map(|n| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let store = AuthStateStore::in_home(&dir);
                    for round in 0..5 {
                        store
                            .record_needs_auth(
                                &format!("https://w{n}.example.com/{round}"),
                                &NeedsAuth::default(),
                            )
                            .expect("recorded");
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer");
        }
        let store = AuthStateStore::in_home(&dir);
        for n in 0..8 {
            for round in 0..5 {
                assert!(
                    store
                        .needs_auth(&format!("https://w{n}.example.com/{round}"))
                        .is_some(),
                    "{n}/{round}"
                );
            }
        }
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != AUTH_STATE_FILE && name != "mcp-auth-state.json.lock")
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temporary file is left: {leftovers:?}"
        );
    }

    #[test]
    fn a_path_that_carries_a_secret_is_not_kept() {
        let dir = temp("pathsecret");
        let store = AuthStateStore::in_home(&dir);
        store
            .record_needs_auth(
                "https://user:PASSWORD-CANARY@example.com/s/PATH-CANARY/mcp;p=PARAM-CANARY?q=Q#f",
                &NeedsAuth::default(),
            )
            .expect("recorded");
        let text = std::fs::read_to_string(dir.join(AUTH_STATE_FILE)).expect("file");
        for canary in ["PASSWORD-CANARY", "PATH-CANARY", "PARAM-CANARY", "Q#f"] {
            assert!(!text.contains(canary), "{canary}: {text}");
        }
        assert!(text.contains("https://example.com"), "{text}");
        // And it is still found by the URL as written.
        assert!(store
            .needs_auth("https://user:PASSWORD-CANARY@example.com/s/PATH-CANARY/mcp;p=PARAM-CANARY?q=Q#f")
            .is_some());
    }

    #[test]
    fn a_lock_is_exclusive_waits_and_is_released_when_its_holder_lets_go() {
        let dir = temp("lock");
        let path = dir.join("x.lock");
        let first = FileLock::acquire(path.clone(), Duration::from_millis(100)).expect("first");
        let started = Instant::now();
        assert_eq!(
            FileLock::acquire(path.clone(), Duration::from_millis(150)).err(),
            Some(LockError::Busy)
        );
        assert!(started.elapsed() >= Duration::from_millis(150));
        drop(first);
        let second = FileLock::acquire(path.clone(), Duration::from_millis(100)).expect("free");
        // A waiter gets it as soon as the holder lets go.
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || FileLock::acquire(path, Duration::from_secs(10)).is_ok())
        };
        std::thread::sleep(Duration::from_millis(80));
        drop(second);
        assert!(waiter.join().expect("waiter"), "taken once released");
        // The file is left where it is: nothing to go stale, nothing to race over.
        assert!(path.exists());
    }

    #[test]
    fn the_reader_refuses_a_file_over_its_bound_even_when_it_is_valid() {
        let dir = temp("oversize");
        let path = dir.join(AUTH_STATE_FILE);
        let store = AuthStateStore::at(path.clone());
        store
            .record_needs_auth("https://a.example/", &NeedsAuth::default())
            .expect("recorded");
        let valid = std::fs::read_to_string(&path).expect("file");
        assert!(store.needs_auth("https://a.example/").is_some());
        // The same document, padded past the bound: not read, though it parses.
        let padding = " ".repeat(MAX_FILE_BYTES as usize);
        std::fs::write(&path, format!("{valid}{padding}")).expect("write");
        assert_eq!(store.needs_auth("https://a.example/"), None);
        // And padded to just under it: read.
        let room = MAX_FILE_BYTES as usize - valid.len() - 1;
        std::fs::write(&path, format!("{valid}{}", " ".repeat(room))).expect("write");
        assert!(store.needs_auth("https://a.example/").is_some());
    }

    #[test]
    fn a_url_is_read_the_way_the_connection_reads_it() {
        for (url, secure) in [
            ("HTTPS://m.example/mcp", true),
            ("https://under_score.example/mcp", true),
            ("https://m.example./mcp#frag", true),
            ("http://127.0.0.1:8080/mcp", true),
            ("http://localhost/mcp", true),
            ("HTTP://LOCALHOST:3000/mcp", true),
            ("http://[::1]:8080/mcp", true),
            ("http://user:pw@127.0.0.1/mcp", true),
            ("http://127.0.0.1.evil.example/mcp", false),
            ("http://localhost.evil.example/mcp", false),
            ("http://evil.example/127.0.0.1", false),
            ("http://evil.example?x=localhost", false),
            ("http://user@evil.example:80@127.0.0.1/", true),
            ("http://127.0.0.1@evil.example/mcp", false),
            ("ftp://127.0.0.1/", false),
            ("", false),
        ] {
            assert_eq!(secure_enough(url), secure, "{url}");
        }
    }
}
