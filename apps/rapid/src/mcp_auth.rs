//! The authorization state of a remote MCP server (EXT-02 §4.1).
//!
//! A remote server is in one of a few states, and every one is a projection of
//! what the entry says and what has been recorded about the server — never a
//! thing this module decides on its own:
//!
//! | state | meaning |
//! |---|---|
//! | `static` | a static credential is configured; the flow never engages |
//! | `disabled` | the entry says `auth: false` |
//! | `not_required` | the server has never answered "authorization required" |
//! | `needs_auth` | a `401`, or a `403` that names the scope, arrived and has not been resolved |
//!
//! What has been recorded is a small, non-secret file in the RapidLM home. It
//! holds no token, no code, no verifier and no client secret — only the fact
//! that a server asked, and for which scopes — so `rapid mcp list`, `rapid
//! mcp get` and `rapid doctor` can say it without starting anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::exec_tools::{McpAuthMode, McpHttpEndpoint};

/// The file, beside the other per-user state in the RapidLM home.
pub const AUTH_STATE_FILE: &str = "mcp-auth-state.json";

const SCHEMA: &str = "rapidlm.mcp_auth_state.v1";

/// A file larger than this is not read: it is not one this module wrote.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The servers one file remembers; the oldest are forgotten past it.
const MAX_SERVERS: usize = 256;

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

/// Whether the entry carries a credential of its own: a bearer token read from
/// the environment, a header value read from the environment, or an
/// `Authorization` header. The authorization flow never engages for such a
/// server, and a `401` from it is a plain failure.
pub fn is_static(http: &McpHttpEndpoint) -> bool {
    http.bearer_token_env.is_some()
        || !http.headers_env.is_empty()
        || http
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
}

/// The state of an entry, given what has been recorded.
pub fn state_of(http: &McpHttpEndpoint, store: Option<&AuthStateStore>) -> AuthState {
    if is_static(http) {
        return AuthState::Static;
    }
    if matches!(http.auth, McpAuthMode::Never) {
        return AuthState::Disabled;
    }
    match store.and_then(|store| store.needs_auth(&http.url)) {
        Some(need) => AuthState::NeedsAuth(need),
        None => AuthState::NotRequired,
    }
}

/// Whether a `401` from this server may start the flow: it has no static
/// credential and does not say `auth: false`.
pub fn engages(http: &McpHttpEndpoint) -> bool {
    !is_static(http) && !matches!(http.auth, McpAuthMode::Never)
}

/// The URL a server is known by: lowercase scheme and host, the default port
/// left out, the path as given (an empty one is `/`), and no query or fragment
/// — the part of a URL that can carry a token is never kept.
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

/// What is recorded about one server.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Recorded {
    need: NeedsAuth,
    since: String,
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
        for (url, entry) in servers.iter().take(MAX_SERVERS) {
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
                url.clone(),
                Recorded {
                    need: NeedsAuth {
                        scopes,
                        insufficient_scope: entry["insufficient_scope"].as_bool().unwrap_or(false),
                    },
                    since: entry["since"].as_str().unwrap_or_default().to_owned(),
                },
            );
        }
        out
    }

    fn write(&self, servers: &BTreeMap<String, Recorded>) -> Result<(), String> {
        let mut doc = serde_json::Map::new();
        for (url, recorded) in servers {
            doc.insert(
                url.clone(),
                serde_json::json!({
                    "state": "needs_auth",
                    "scopes": recorded.need.scopes,
                    "insufficient_scope": recorded.need.insufficient_scope,
                    "since": recorded.since,
                }),
            );
        }
        let text = serde_json::to_string_pretty(&serde_json::json!({
            "schema": SCHEMA,
            "servers": doc,
        }))
        .map_err(|err| err.to_string())?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        // A temporary file beside it and a rename: a reader sees the old file
        // or the new one, never half of either.
        let temp = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&temp, format!("{text}\n")).map_err(|err| err.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&temp, &self.path).map_err(|err| {
            let _ = std::fs::remove_file(&temp);
            err.to_string()
        })
    }

    /// What the server at `url` last asked for, if it asked and nothing has
    /// resolved it.
    pub fn needs_auth(&self, url: &str) -> Option<NeedsAuth> {
        self.read()
            .remove(&canonical_url(url))
            .map(|recorded| recorded.need)
    }

    /// Record that the server at `url` asked for authorization. Best effort: a
    /// state that cannot be written is said to the caller and costs the
    /// session nothing (the marker is in the tool list either way).
    pub fn record_needs_auth(&self, url: &str, need: &NeedsAuth) -> Result<(), String> {
        let mut servers = self.read();
        let key = canonical_url(url);
        if servers.get(&key).map(|recorded| &recorded.need) == Some(need) {
            return Ok(());
        }
        while servers.len() >= MAX_SERVERS && !servers.contains_key(&key) {
            let Some(oldest) = servers
                .iter()
                .min_by(|a, b| a.1.since.cmp(&b.1.since))
                .map(|(url, _)| url.clone())
            else {
                break;
            };
            servers.remove(&oldest);
        }
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
        servers.insert(
            key,
            Recorded {
                need,
                since: crate::headless::jsonl::now_rfc3339(),
            },
        );
        self.write(&servers)
    }

    /// The server at `url` no longer asks: forget that it did.
    pub fn clear(&self, url: &str) -> Result<(), String> {
        let mut servers = self.read();
        if servers.remove(&canonical_url(url)).is_none() {
            return Ok(());
        }
        self.write(&servers)
    }
}

/// A scope as the file keeps it: the bounds a project's `scopes` has.
fn valid_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope.len() <= crate::mcp_config::MAX_AUTH_SCOPE_BYTES
        && scope
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b'"' && byte != b'\\')
}

/// The scopes of a challenge's `scope` parameter, as the file keeps them.
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
        assert!(text.contains("\"https://example.com/mcp\""), "{text}");
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
            r#"{"schema":"rapidlm.mcp_auth_state.v1","servers":[]}"#.to_owned(),
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
        for n in 0..=MAX_SERVERS + 3 {
            store
                .record_needs_auth(
                    &format!("https://s{n:04}.example.com/mcp"),
                    &NeedsAuth::default(),
                )
                .expect("recorded");
        }
        let kept = store.read();
        assert_eq!(kept.len(), MAX_SERVERS);
        assert!(kept.contains_key(&format!("https://s{:04}.example.com/mcp", MAX_SERVERS + 3)));
    }
}
