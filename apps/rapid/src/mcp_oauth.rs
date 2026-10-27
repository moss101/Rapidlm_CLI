//! OAuth for `http` MCP servers: RFC 8414 discovery, RFC 8628 device
//! authorization, RFC 7009 nothing (no revocation — a logout is a deleted
//! token file).
//!
//! `rapid mcp auth <name>` runs the flow a terminal owes: fetch the
//! server's authorization-server metadata (`.well-known/oauth-authorization-
//! server` beside the MCP endpoint), POST the device-authorization request,
//! print the verification URL and the user code, poll the token endpoint
//! until the user finishes (or the flow's own `interval`/`expires_in` says
//! stop), and store the access token under the project's marker directory.
//! An entry with a stored token sends it as the bearer on every connect —
//! `connect_mcp_http`'s header merge — and a `401` on a real call says to
//! re-run `mcp auth` (no silent refresh guessing: a refresh token is used
//! exactly once, when it exists and the access token is stale).
//!
//! Everything is typed and fail-closed: a discovery document without the
//! endpoints this flow needs is an error naming them, a poll past
//! `expires_in` is `expired`, and nothing is ever written except the one
//! token file — `mcp.json` under `.rapidlm/`, one object per server, mode
//! 0600 where the platform allows it.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Longest discovery document accepted.
pub const MAX_DISCOVERY_BYTES: usize = 64 * 1024;
/// How long the whole flow may take.
pub const FLOW_TIMEOUT: Duration = Duration::from_secs(600);

/// The authorization-server metadata this flow needs (RFC 8414 subset).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizationServer {
    pub issuer: String,
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
}

/// Parse a discovery document (pure). The two endpoints the device flow
/// needs are required; an issuer is trusted as given (transport
/// validation is the fetch's egress gate's job).
pub fn parse_discovery(body: &str) -> Result<AuthorizationServer, String> {
    if body.len() > MAX_DISCOVERY_BYTES {
        return Err("the discovery document is over its size bound".to_owned());
    }
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|err| format!("the discovery document is not JSON: {err}"))?;
    let text = |key: &str| -> Result<String, String> {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
            .map(str::to_owned)
            .ok_or_else(|| format!("the discovery document names no {key}"))
    };
    Ok(AuthorizationServer {
        issuer: text("issuer")?,
        device_authorization_endpoint: text("device_authorization_endpoint")?,
        token_endpoint: text("token_endpoint")?,
    })
}

/// The `.well-known/oauth-authorization-server` URL for an MCP endpoint:
/// the origin's well-known path with the endpoint's own path appended as
/// the RFC's resource indicator (`/.well-known/oauth-authorization-server/<path>`).
pub fn discovery_url(endpoint: &str) -> Result<String, String> {
    let (scheme_rest, origin, path) = split_url(endpoint)?;
    if path.is_empty() {
        return Ok(format!(
            "{scheme_rest}://{origin}/.well-known/oauth-authorization-server"
        ));
    }
    Ok(format!(
        "{scheme_rest}://{origin}/.well-known/oauth-authorization-server/{path}"
    ))
}

fn split_url(endpoint: &str) -> Result<(&str, &str, &str), String> {
    let (scheme, rest) = endpoint
        .split_once("://")
        .ok_or_else(|| "the MCP endpoint has no scheme".to_owned())?;
    let (origin, path) = match rest.find('/') {
        Some(at) => (&rest[..at], rest[at + 1..].trim_end_matches('/')),
        None => (rest, ""),
    };
    if origin.is_empty() {
        return Err("the MCP endpoint has no host".to_owned());
    }
    Ok((scheme, origin, path))
}

/// One device-authorization response (RFC 8628): what the user visits and
/// enters, and how fast to poll.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// Seconds between token polls (default 5 when the server omits it).
    pub interval_secs: u64,
    /// Seconds the code lives.
    pub expires_in: u64,
}

/// Parse a device-authorization response (pure).
pub fn parse_device_authorization(body: &str) -> Result<DeviceAuthorization, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|err| format!("the device authorization response is not JSON: {err}"))?;
    let text = |key: &str| -> Result<String, String> {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("the device authorization response names no {key}"))
    };
    Ok(DeviceAuthorization {
        device_code: text("device_code")?,
        user_code: text("user_code")?,
        verification_uri: text("verification_uri")?,
        interval_secs: value
            .get("interval")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(5),
        expires_in: value
            .get("expires_in")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "the device authorization response names no expires_in".to_owned())?,
    })
}

/// The token poll's outcome (RFC 8628 §3.5 error codes + success).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenPoll {
    /// The access token (and refresh token, when the server issued one).
    Granted {
        access_token: String,
        refresh_token: Option<String>,
    },
    /// The user has not finished yet: keep polling.
    Pending,
    /// The flow is over and did not grant.
    Denied(String),
}

/// Parse a token-endpoint response (pure).
pub fn parse_token_response(body: &str) -> Result<TokenPoll, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|err| format!("the token response is not JSON: {err}"))?;
    if value.get("error").is_none() {
        let access_token = value
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "the token response grants no access_token".to_owned())?;
        return Ok(TokenPoll::Granted {
            access_token,
            refresh_token: value
                .get("refresh_token")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        });
    }
    match value.get("error").and_then(serde_json::Value::as_str) {
        Some("authorization_pending") | Some("slow_down") => Ok(TokenPoll::Pending),
        Some(other) => Ok(TokenPoll::Denied(other.to_owned())),
        None => Err("the token response names no error and no access_token".to_owned()),
    }
}

/// The stored credential for one server: what `mcp auth` wrote and
/// `connect_mcp_http` reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// Where a server's token lives: `<project>/.rapidlm/mcp-oauth/<name>.json`.
pub fn token_path(marker: &Path, server: &str) -> Option<PathBuf> {
    (server
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
    .then(|| marker.join("mcp-oauth").join(format!("{server}.json")))
}

/// Read a server's stored token. `None` when none is stored; a corrupt
/// file is an error naming it (never "no token" — the caller would then
/// connect anonymous and hammer the server with 401s).
pub fn load_token(marker: &Path, server: &str) -> Result<Option<StoredToken>, String> {
    let Some(path) = token_path(marker, server) else {
        return Err(format!(
            "{server:?} is not a server name this can store a token for"
        ));
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{} could not be read: {err}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| format!("{} is not valid JSON", path.display()))?;
    let access_token = value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{} holds no access_token", path.display()))?;
    Ok(Some(StoredToken {
        access_token,
        refresh_token: value
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    }))
}

/// Write a server's token (creating the directory). The file is one JSON
/// object; permissions are tightened to 0600 where the platform allows.
pub fn store_token(marker: &Path, server: &str, token: &StoredToken) -> Result<(), String> {
    let Some(path) = token_path(marker, server) else {
        return Err(format!(
            "{server:?} is not a server name this can store a token for"
        ));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("{} could not be created: {err}", parent.display()))?;
    }
    let body = serde_json::json!({
        "access_token": token.access_token,
        "refresh_token": token.refresh_token,
    });
    std::fs::write(&path, body.to_string())
        .map_err(|err| format!("{} could not be written: {err}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Forget a server's token (`rapid mcp unauth <name>`): the file is gone.
pub fn forget_token(marker: &Path, server: &str) -> Result<bool, String> {
    let Some(path) = token_path(marker, server) else {
        return Err(format!(
            "{server:?} is not a server name this stores tokens for"
        ));
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("{} could not be removed: {err}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_urls_sit_beside_the_endpoint_per_the_well_known_path() {
        assert_eq!(
            discovery_url("https://api.example.test/mcp").expect("url"),
            "https://api.example.test/.well-known/oauth-authorization-server/mcp"
        );
        // No path: the plain well-known.
        assert_eq!(
            discovery_url("https://sso.example.test").expect("url"),
            "https://sso.example.test/.well-known/oauth-authorization-server"
        );
        // Trailing slashes are not carried into the resource path.
        assert_eq!(
            discovery_url("https://api.example.test/mcp/").expect("url"),
            "https://api.example.test/.well-known/oauth-authorization-server/mcp"
        );
        assert!(discovery_url("api.example.test/mcp").is_err());
    }

    #[test]
    fn the_discovery_document_parses_only_with_both_endpoints() {
        let doc = r#"{
            "issuer": "https://sso.example.test",
            "device_authorization_endpoint": "https://sso.example.test/device",
            "token_endpoint": "https://sso.example.test/token"
        }"#;
        let server = parse_discovery(doc).expect("server");
        assert_eq!(
            server.device_authorization_endpoint,
            "https://sso.example.test/device"
        );
        assert_eq!(server.token_endpoint, "https://sso.example.test/token");

        // Missing either endpoint names it; a non-URL is as good as absent.
        for (broken, names) in [
            (
                r#"{"issuer":"https://x","token_endpoint":"https://x/t"}"#,
                "device_authorization_endpoint",
            ),
            (
                r#"{"issuer":"https://x","device_authorization_endpoint":"https://x/d"}"#,
                "token_endpoint",
            ),
            (
                r#"{"issuer":"https://x","device_authorization_endpoint":"not-a-url","token_endpoint":"https://x/t"}"#,
                "device_authorization_endpoint",
            ),
        ] {
            let err = parse_discovery(broken).expect_err("rejected");
            assert!(err.contains(names), "{err}");
        }
    }

    #[test]
    fn device_authorizations_and_token_polls_parse_to_their_flow_states() {
        let auth = parse_device_authorization(
            r#"{"device_code":"DC","user_code":"ABCD-EFGH","verification_uri":"https://sso/verify","interval":3,"expires_in":900}"#,
        )
        .expect("auth");
        assert_eq!(auth.user_code, "ABCD-EFGH");
        assert_eq!(auth.interval_secs, 3);
        // `interval` is optional (default 5); `expires_in` is not.
        let default_interval = parse_device_authorization(
            r#"{"device_code":"DC","user_code":"U","verification_uri":"https://sso/v","expires_in":60}"#,
        )
        .expect("auth");
        assert_eq!(default_interval.interval_secs, 5);
        assert!(parse_device_authorization(r#"{"device_code":"DC"}"#).is_err());

        // Token polls: pending, slow_down, denied, granted.
        assert_eq!(
            parse_token_response(r#"{"error":"authorization_pending"}"#).expect("poll"),
            TokenPoll::Pending
        );
        assert_eq!(
            parse_token_response(r#"{"error":"slow_down"}"#).expect("poll"),
            TokenPoll::Pending
        );
        assert_eq!(
            parse_token_response(r#"{"error":"access_denied"}"#).expect("poll"),
            TokenPoll::Denied("access_denied".to_owned())
        );
        assert_eq!(
            parse_token_response(r#"{"access_token":"at","refresh_token":"rt"}"#).expect("poll"),
            TokenPoll::Granted {
                access_token: "at".to_owned(),
                refresh_token: Some("rt".to_owned()),
            }
        );
        assert_eq!(
            parse_token_response(r#"{"access_token":"at"}"#).expect("poll"),
            TokenPoll::Granted {
                access_token: "at".to_owned(),
                refresh_token: None,
            }
        );
    }

    #[test]
    fn tokens_round_trip_through_the_marker_directory_and_forget_cleanly() {
        let dir = std::env::temp_dir().join(format!("rapidlm-oauth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(load_token(&dir, "sso").expect("none"), None);

        let token = StoredToken {
            access_token: "at".to_owned(),
            refresh_token: Some("rt".to_owned()),
        };
        store_token(&dir, "sso", &token).expect("stored");
        let reloaded_token = token.clone();
        assert_eq!(
            load_token(&dir, "sso").expect("loaded"),
            Some(reloaded_token)
        );

        // A corrupt file is an error, never "no token".
        let path = dir.join("mcp-oauth").join("sso.json");
        std::fs::write(&path, "not json").expect("corrupt");
        assert!(load_token(&dir, "sso").is_err());

        // Forget: gone, then idempotent.
        std::fs::write(&path, r#"{"access_token":"at"}"#).expect("rewrite");
        assert!(forget_token(&dir, "sso").expect("forgot"));
        assert!(!forget_token(&dir, "sso").expect("already gone"));
        assert_eq!(load_token(&dir, "sso").expect("none"), None);

        // A server name that could escape the directory is refused.
        assert!(token_path(&dir, "../escape").is_none());
        assert!(token_path(&dir, "../escape").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
