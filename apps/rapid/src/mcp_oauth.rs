//! Authorization code with PKCE for a remote MCP server (EXT-02-2): the part
//! that is only protocol — URLs, the metadata documents, PKCE, the
//! authorization URL, the registration and token bodies, the callback's query.
//!
//! Nothing here touches a socket, a file or the keychain. The flow
//! (`mcp_authorize`) drives it, and every limit the specification names is a
//! constant here: a reply of at most 64 KiB, an access token of at most 8 KiB,
//! a lifetime of at most 30 days, three redirects, ten seconds a request.
//!
//! Everything a server sends is data. A string that is shown to the person
//! passes through [`shown`], which keeps printable ASCII and a bounded length,
//! so a hostile metadata document cannot write escape sequences to a terminal.

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The most bytes of one discovery, registration or token reply.
pub const MAX_DOCUMENT_BYTES: usize = 64 * 1024;

/// The most redirects one discovery request follows; each is re-authorized.
pub const MAX_REDIRECTS: usize = 3;

/// How long one request of the flow may take.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest access token taken.
pub const MAX_ACCESS_TOKEN_BYTES: usize = 8 * 1024;

/// The longest refresh token taken.
pub const MAX_REFRESH_TOKEN_BYTES: usize = 8 * 1024;

/// The longest lifetime an `expires_in` may name.
pub const MAX_LIFETIME_SECS: u64 = 30 * 24 * 60 * 60;

/// The longest URL taken from a document or the command line.
const MAX_URL_BYTES: usize = 2048;

/// The longest string of a document shown to the person.
const MAX_SHOWN_BYTES: usize = 120;

/// The most authorization servers a resource document is read for.
const MAX_AUTHORIZATION_SERVERS: usize = 8;

/// A string out of a document, as it may be shown: printable ASCII only,
/// shortened. Never the means to carry an escape sequence to a terminal.
pub fn shown(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
        .take(MAX_SHOWN_BYTES)
        .collect();
    if text.chars().count() > MAX_SHOWN_BYTES {
        out.push_str("...");
    }
    out
}

// --- secrets -----------------------------------------------------------------

/// A value that is a credential: a token, a code, a verifier, a client secret.
/// It prints as nothing and is read only through [`Secret::expose`].
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

// --- URLs --------------------------------------------------------------------

/// Why text is not a URL this flow will use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlError {
    Empty,
    TooLong,
    /// Anything but `http` and `https`.
    Scheme,
    /// A name and password ahead of the host.
    Userinfo,
    Host,
    Port,
    Fragment,
    /// A byte that is not printable ASCII (an address is already escaped), or
    /// a backslash.
    Characters,
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "it is empty",
            Self::TooLong => "it is longer than 2048 bytes",
            Self::Scheme => "its scheme is not http or https",
            Self::Userinfo => "it carries a name or password",
            Self::Host => "its host is not a name or an address",
            Self::Port => "its port is not 1 to 65535",
            Self::Fragment => "it carries a fragment",
            Self::Characters => "it has a byte outside printable ASCII or a backslash",
        })
    }
}

/// An `http` or `https` URL taken apart, strictly: no name or password, no
/// fragment, printable ASCII only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    https: bool,
    host: String,
    port: u16,
    /// The path and query; always starts with `/`.
    target: String,
}

impl Url {
    pub fn parse(text: &str) -> Result<Self, UrlError> {
        if text.is_empty() {
            return Err(UrlError::Empty);
        }
        if text.len() > MAX_URL_BYTES {
            return Err(UrlError::TooLong);
        }
        if !text
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b'\\')
        {
            return Err(UrlError::Characters);
        }
        let (https, rest) = if let Some(rest) = strip_prefix_ignoring_case(text, "https://") {
            (true, rest)
        } else if let Some(rest) = strip_prefix_ignoring_case(text, "http://") {
            (false, rest)
        } else {
            return Err(UrlError::Scheme);
        };
        if rest.contains('#') {
            return Err(UrlError::Fragment);
        }
        let (authority, target) = match rest.find(['/', '?']) {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, ""),
        };
        if authority.contains('@') {
            return Err(UrlError::Userinfo);
        }
        let (host, port) = split_authority(authority, https)?;
        let target = if target.is_empty() {
            "/".to_owned()
        } else if target.starts_with('?') {
            format!("/{target}")
        } else {
            target.to_owned()
        };
        Ok(Self {
            https,
            host,
            port,
            target,
        })
    }

    pub fn is_https(&self) -> bool {
        self.https
    }

    /// The host, lowercase; an IPv6 literal keeps its brackets.
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The path and query.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The path, without the query.
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("/")
    }

    /// `scheme://host[:port]`, the port left out when it is the scheme's own.
    pub fn origin(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        let default = if self.https { 443 } else { 80 };
        if self.port == default {
            format!("{scheme}://{}", self.host)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }

    /// `host[:port]`, as a person reads which machine it is.
    pub fn authority(&self) -> String {
        let default = if self.https { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Whether the host is this machine: `localhost`, an IPv4 address in
    /// `127.0.0.0/8`, or `[::1]`.
    pub fn is_loopback(&self) -> bool {
        self.host == "localhost"
            || self.host == "[::1]"
            || self
                .host
                .parse::<std::net::Ipv4Addr>()
                .is_ok_and(|address| address.is_loopback())
    }

    /// This URL with `path` as its path and no query.
    pub fn with_path(&self, path: &str) -> Self {
        Self {
            target: if path.starts_with('/') {
                path.to_owned()
            } else {
                format!("/{path}")
            },
            ..self.clone()
        }
    }

    /// The well-known location of `suffix` for this URL, the way the metadata
    /// specifications insert it: between the host and the path, the query
    /// dropped (`https://h/mcp` → `https://h/.well-known/<suffix>/mcp`).
    pub fn well_known(&self, suffix: &str) -> Self {
        let path = self.path().trim_end_matches('/');
        self.with_path(&format!("/.well-known/{suffix}{path}"))
    }

    /// The URL a `Location` header names, relative to this one. A scheme-less
    /// `//host` form is refused rather than guessed at.
    pub fn join(&self, location: &str) -> Result<Self, UrlError> {
        let location = location.split('#').next().unwrap_or_default();
        if location.is_empty() {
            return Err(UrlError::Empty);
        }
        if strip_prefix_ignoring_case(location, "https://").is_some()
            || strip_prefix_ignoring_case(location, "http://").is_some()
        {
            return Self::parse(location);
        }
        if location.starts_with("//") {
            return Err(UrlError::Scheme);
        }
        // `ftp://…`, `javascript:…`: a scheme that is not ours is not a path.
        if location
            .split(['/', '?'])
            .next()
            .is_some_and(|first| first.contains(':'))
        {
            return Err(UrlError::Scheme);
        }
        let rebuilt = if location.starts_with('/') {
            format!("{}{location}", self.origin())
        } else {
            let path = self.path();
            let directory = &path[..path.rfind('/').map_or(0, |at| at + 1)];
            format!("{}{directory}{location}", self.origin())
        };
        Self::parse(&rebuilt)
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.origin(), self.target)
    }
}

fn strip_prefix_ignoring_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

fn split_authority(authority: &str, https: bool) -> Result<(String, u16), UrlError> {
    let default = if https { 443 } else { 80 };
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let end = inner.find(']').ok_or(UrlError::Host)?;
        let literal = &inner[..end];
        if literal.is_empty()
            || !literal
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b':' || byte == b'.')
        {
            return Err(UrlError::Host);
        }
        let after = &inner[end + 1..];
        let port = match after.strip_prefix(':') {
            Some(port) => Some(port),
            None if after.is_empty() => None,
            None => return Err(UrlError::Host),
        };
        (format!("[{}]", literal.to_ascii_lowercase()), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_ascii_lowercase(), Some(port)),
            None => (authority.to_ascii_lowercase(), None),
        }
    };
    if host.is_empty() {
        return Err(UrlError::Host);
    }
    if !host.starts_with('[')
        && (host.starts_with(['.', '-'])
            || host.ends_with(['.', '-'])
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-'))
    {
        return Err(UrlError::Host);
    }
    let port = match port {
        None => default,
        Some(port) => {
            if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(UrlError::Port);
            }
            match port.parse::<u16>() {
                Ok(0) | Err(_) => return Err(UrlError::Port),
                Ok(port) => port,
            }
        }
    };
    Ok((host, port))
}

// --- percent-encoding ----------------------------------------------------------

/// `text` as one URL query or form component: everything but the unreserved
/// characters is escaped.
pub fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A query or form component back to text; `None` for a bad escape or bytes
/// that are not UTF-8.
pub fn decode_component(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'%' => {
                let pair = bytes.get(at + 1..at + 3)?;
                let value = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
                out.push(value);
                at += 3;
            }
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            byte => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// The `name=value` pairs of a query or form body, decoded. A pair that does
/// not decode is dropped; a name seen twice keeps both, in order.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((decode_component(name)?, decode_component(value)?))
        })
        .collect()
}

/// `pairs` as a form body.
pub fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("{}={}", encode_component(name), encode_component(value)))
        .collect::<Vec<_>>()
        .join("&")
}

// --- randomness and PKCE ---------------------------------------------------------

/// The system's source of randomness did not answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntropyError;

impl fmt::Display for EntropyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the system's random source did not answer")
    }
}

/// `bytes` random bytes, as base64url without padding.
pub fn random_token(bytes: usize) -> Result<String, EntropyError> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|_| EntropyError)?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

/// The `state` value: 256 random bits.
pub fn new_state() -> Result<String, EntropyError> {
    random_token(32)
}

/// A PKCE pair (RFC 7636, method `S256`).
pub struct Pkce {
    verifier: Secret,
    challenge: String,
}

impl Pkce {
    /// A fresh pair: a 43-character verifier from 256 random bits.
    pub fn generate() -> Result<Self, EntropyError> {
        Ok(Self::from_verifier(random_token(32)?))
    }

    pub fn from_verifier(verifier: String) -> Self {
        let challenge = challenge_of(&verifier);
        Self {
            verifier: Secret::new(verifier),
            challenge,
        }
    }

    pub fn verifier(&self) -> &Secret {
        &self.verifier
    }

    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pkce(..)")
    }
}

/// The `S256` challenge of a verifier.
pub fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Whether two strings are equal, in time that does not depend on where they
/// first differ.
pub fn same_secret(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

// --- what a document may not be ------------------------------------------------------

/// Why a document of the flow was not taken. Every text is safe to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    /// A URL the flow would contact is neither `https` nor the loopback
    /// exception.
    NotHttps { what: &'static str, url: String },
    /// A URL that is not one.
    BadUrl {
        what: &'static str,
        reason: UrlError,
    },
    /// Not JSON, not an object, or a field of the wrong type.
    Malformed { what: &'static str, why: String },
    /// Larger than a document may be.
    TooLarge { what: &'static str },
    /// The resource document names another resource.
    ResourceMismatch { expected: String, got: String },
    /// The authorization server's document names another issuer.
    IssuerMismatch { expected: String, got: String },
    /// No authorization server a resource names is usable.
    NoAuthorizationServer,
    /// The server does not say it supports PKCE `S256`.
    PkceUnsupported,
    /// The registration reply is not a public client.
    NotAPublicClient,
    /// `token_type` is not Bearer.
    TokenType { got: String },
    /// A token that is empty, too long or has bytes a header cannot carry.
    Token {
        what: &'static str,
        why: &'static str,
    },
    /// An `expires_in` that is not a sane lifetime.
    Lifetime,
    /// An `iss` that is not the discovered issuer.
    IssMismatch { expected: String, got: String },
    /// The token endpoint refused the exchange with this error code.
    Rejected { error: String },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotHttps { what, url } => write!(
                f,
                "{what} {} is not https (plain http is accepted only on this machine, for a \
server that is itself on this machine)",
                shown(url)
            ),
            Self::BadUrl { what, reason } => write!(f, "{what} is not a usable URL: {reason}"),
            Self::Malformed { what, why } => write!(f, "{what} is malformed: {}", shown(why)),
            Self::TooLarge { what } => write!(f, "{what} is larger than 64 KiB"),
            Self::ResourceMismatch { expected, got } => write!(
                f,
                "the server's metadata describes {} rather than {}",
                shown(got),
                shown(expected)
            ),
            Self::IssuerMismatch { expected, got } => write!(
                f,
                "the authorization server's metadata names the issuer {} rather than {}",
                shown(got),
                shown(expected)
            ),
            Self::NoAuthorizationServer => {
                f.write_str("the server names no authorization server this command can use")
            }
            Self::PkceUnsupported => f.write_str(
                "the authorization server does not say it supports PKCE with S256, which this \
command requires",
            ),
            Self::NotAPublicClient => f.write_str(
                "the registration did not produce a public client (token_endpoint_auth_method \
is not none)",
            ),
            Self::TokenType { got } => {
                write!(f, "the token type is {} rather than Bearer", shown(got))
            }
            Self::Token { what, why } => write!(f, "the {what} is not usable: {why}"),
            Self::Lifetime => {
                f.write_str("the token's lifetime is not between 1 second and 30 days")
            }
            Self::IssMismatch { expected, got } => write!(
                f,
                "the response names the issuer {} rather than {}",
                shown(got),
                shown(expected)
            ),
            Self::Rejected { error } => {
                write!(f, "the token endpoint answered {}", shown(error))
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Whether `url` may be contacted by the flow: `https`, or `http` on this
/// machine when the server being authorized is itself `http` on this machine.
pub fn check_endpoint(
    what: &'static str,
    url: &Url,
    loopback_http_ok: bool,
) -> Result<(), ProtocolError> {
    if url.is_https() || (loopback_http_ok && url.is_loopback()) {
        Ok(())
    } else {
        Err(ProtocolError::NotHttps {
            what,
            url: url.to_string(),
        })
    }
}

fn endpoint(
    document: &Value,
    key: &'static str,
    what: &'static str,
    loopback_http_ok: bool,
) -> Result<Option<Url>, ProtocolError> {
    let Some(value) = document.get(key) else {
        return Ok(None);
    };
    let text = value.as_str().ok_or_else(|| ProtocolError::Malformed {
        what,
        why: format!("{key} is not a string"),
    })?;
    let url = Url::parse(text).map_err(|reason| match reason {
        UrlError::Scheme => ProtocolError::NotHttps {
            what,
            url: text.to_owned(),
        },
        reason => ProtocolError::BadUrl { what, reason },
    })?;
    check_endpoint(what, &url, loopback_http_ok)?;
    Ok(Some(url))
}

fn object(body: &[u8], what: &'static str) -> Result<Value, ProtocolError> {
    if body.len() > MAX_DOCUMENT_BYTES {
        return Err(ProtocolError::TooLarge { what });
    }
    let value: Value = serde_json::from_slice(body).map_err(|err| ProtocolError::Malformed {
        what,
        why: err.to_string(),
    })?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(ProtocolError::Malformed {
            what,
            why: "it is not an object".to_owned(),
        })
    }
}

fn strings(document: &Value, key: &str) -> Vec<String> {
    document
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .take(64)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

// --- the resource's metadata ---------------------------------------------------------

/// What a protected resource says about itself (RFC 9728), as far as this flow
/// reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedResource {
    /// The authorization servers it names, in its order, each usable.
    pub authorization_servers: Vec<Url>,
    /// The scopes it says it understands.
    pub scopes_supported: Vec<String>,
}

/// Read a resource's metadata. `mcp_url` is the server being authorized: the
/// document's `resource` must be that server, or the document is another
/// resource's.
pub fn parse_protected_resource(
    body: &[u8],
    mcp_url: &str,
    loopback_http_ok: bool,
) -> Result<ProtectedResource, ProtocolError> {
    const WHAT: &str = "the server's protected-resource metadata";
    let document = object(body, WHAT)?;
    let resource = document
        .get("resource")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::Malformed {
            what: WHAT,
            why: "it has no resource".to_owned(),
        })?;
    let expected = crate::mcp_auth::canonical_url(mcp_url);
    if crate::mcp_auth::canonical_url(resource).trim_end_matches('/')
        != expected.trim_end_matches('/')
    {
        return Err(ProtocolError::ResourceMismatch {
            expected,
            got: resource.to_owned(),
        });
    }
    let mut authorization_servers = Vec::new();
    let mut first_refusal = None;
    for text in strings(&document, "authorization_servers")
        .into_iter()
        .take(MAX_AUTHORIZATION_SERVERS)
    {
        let parsed = Url::parse(&text)
            .map_err(|reason| match reason {
                UrlError::Scheme => ProtocolError::NotHttps {
                    what: "an authorization server",
                    url: text.clone(),
                },
                reason => ProtocolError::BadUrl {
                    what: "an authorization server",
                    reason,
                },
            })
            .and_then(|url| {
                check_endpoint("an authorization server", &url, loopback_http_ok)?;
                Ok(url)
            });
        match parsed {
            Ok(url) => authorization_servers.push(url),
            Err(err) => {
                first_refusal.get_or_insert(err);
            }
        }
    }
    if authorization_servers.is_empty() {
        // A document whose every server was refused says why the first was.
        return Err(first_refusal.unwrap_or(ProtocolError::NoAuthorizationServer));
    }
    Ok(ProtectedResource {
        authorization_servers,
        scopes_supported: strings(&document, "scopes_supported"),
    })
}

// --- the authorization server's metadata -----------------------------------------------

/// What an authorization server says about itself (RFC 8414), as far as this
/// flow reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizationServer {
    pub issuer: Url,
    pub authorization_endpoint: Url,
    pub token_endpoint: Url,
    pub registration_endpoint: Option<Url>,
    pub revocation_endpoint: Option<Url>,
    /// `token_endpoint_auth_methods_supported`; empty when the document does
    /// not say.
    pub token_auth_methods: Vec<String>,
    pub scopes_supported: Vec<String>,
}

/// Read an authorization server's metadata. `issuer` is the identifier it was
/// looked up by: the document must name exactly that, or it is another
/// server's (the mix-up RFC 8414 §3.3 guards against).
pub fn parse_authorization_server(
    body: &[u8],
    issuer: &Url,
    loopback_http_ok: bool,
) -> Result<AuthorizationServer, ProtocolError> {
    const WHAT: &str = "the authorization server's metadata";
    let document = object(body, WHAT)?;
    let named = document
        .get("issuer")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::Malformed {
            what: WHAT,
            why: "it has no issuer".to_owned(),
        })?;
    if named.trim_end_matches('/') != issuer.to_string().trim_end_matches('/') {
        return Err(ProtocolError::IssuerMismatch {
            expected: issuer.to_string(),
            got: named.to_owned(),
        });
    }
    let required = |key: &'static str, what: &'static str| {
        endpoint(&document, key, what, loopback_http_ok)?.ok_or_else(|| ProtocolError::Malformed {
            what: WHAT,
            why: format!("it has no {key}"),
        })
    };
    let authorization_endpoint = required("authorization_endpoint", "the authorization endpoint")?;
    let token_endpoint = required("token_endpoint", "the token endpoint")?;
    let registration_endpoint = endpoint(
        &document,
        "registration_endpoint",
        "the registration endpoint",
        loopback_http_ok,
    )?;
    let revocation_endpoint = endpoint(
        &document,
        "revocation_endpoint",
        "the revocation endpoint",
        loopback_http_ok,
    )?;
    // The profile requires a server that supports PKCE to say so; one that
    // says nothing is treated as not supporting it.
    let methods = strings(&document, "code_challenge_methods_supported");
    if !methods.iter().any(|method| method == "S256") {
        return Err(ProtocolError::PkceUnsupported);
    }
    Ok(AuthorizationServer {
        issuer: issuer.clone(),
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        revocation_endpoint,
        token_auth_methods: strings(&document, "token_endpoint_auth_methods_supported"),
        scopes_supported: strings(&document, "scopes_supported"),
    })
}

impl AuthorizationServer {
    /// The issuer identifier as a document names it: the URL without the
    /// trailing slash an empty path gets when it is parsed.
    pub fn issuer_id(&self) -> String {
        self.issuer.to_string().trim_end_matches('/').to_owned()
    }

    /// How a client with a secret presents it: `basic` unless the server lists
    /// only the body form (RFC 8414's default is `client_secret_basic`).
    pub fn secret_in_body(&self) -> bool {
        !self.token_auth_methods.is_empty()
            && !self
                .token_auth_methods
                .iter()
                .any(|method| method == "client_secret_basic")
            && self
                .token_auth_methods
                .iter()
                .any(|method| method == "client_secret_post")
    }

    /// Every origin the flow will contact at this server, as `host[:port]`,
    /// in a stable order and without repeats.
    pub fn hosts(&self) -> Vec<String> {
        let mut hosts = vec![self.issuer.authority()];
        for url in [
            Some(&self.authorization_endpoint),
            Some(&self.token_endpoint),
            self.registration_endpoint.as_ref(),
            self.revocation_endpoint.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            let host = url.authority();
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        hosts
    }
}

// --- registration -----------------------------------------------------------------------

/// The body of a dynamic registration (RFC 7591): a native application with a
/// loopback redirect and no client secret.
pub fn registration_body(redirect_uri: &str, scopes: &[String]) -> Vec<u8> {
    let mut body = serde_json::json!({
        "client_name": "RapidLM",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    });
    if !scopes.is_empty() {
        body["scope"] = Value::String(scopes.join(" "));
    }
    body.to_string().into_bytes()
}

/// The identifier a registration returned. Any secret it also returned is not
/// read: the client is public.
pub fn parse_registration(body: &[u8]) -> Result<String, ProtocolError> {
    const WHAT: &str = "the registration reply";
    let document = object(body, WHAT)?;
    let client_id = document
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::Malformed {
            what: WHAT,
            why: "it has no client_id".to_owned(),
        })?;
    if !valid_client_id(client_id) {
        return Err(ProtocolError::Malformed {
            what: WHAT,
            why: "the client_id is empty, too long or not printable".to_owned(),
        });
    }
    if let Some(method) = document
        .get("token_endpoint_auth_method")
        .and_then(Value::as_str)
        && method != "none"
    {
        return Err(ProtocolError::NotAPublicClient);
    }
    Ok(client_id.to_owned())
}

/// A client identifier a request can carry: printable ASCII, at most 256 bytes.
pub fn valid_client_id(client_id: &str) -> bool {
    !client_id.is_empty()
        && client_id.len() <= crate::mcp_config::MAX_AUTH_CLIENT_ID_BYTES
        && client_id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

// --- the authorization request ------------------------------------------------------------

/// What the browser is sent to.
pub struct AuthorizationRequest<'a> {
    pub endpoint: &'a Url,
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub state: &'a str,
    pub challenge: &'a str,
    /// The canonical URL of the server being authorized (RFC 8707).
    pub resource: &'a str,
    pub scope: Option<&'a str>,
}

/// The authorization URL: the endpoint, whatever query it already had, and the
/// request. It carries the single-use `state` and the PKCE challenge — and
/// never the verifier.
pub fn authorization_url(request: &AuthorizationRequest<'_>) -> String {
    let mut pairs = vec![
        ("response_type", "code"),
        ("client_id", request.client_id),
        ("redirect_uri", request.redirect_uri),
        ("state", request.state),
        ("code_challenge", request.challenge),
        ("code_challenge_method", "S256"),
        ("resource", request.resource),
    ];
    if let Some(scope) = request.scope.filter(|scope| !scope.is_empty()) {
        pairs.push(("scope", scope));
    }
    let query = form_body(&pairs);
    let base = request.endpoint.to_string();
    let joiner = if request.endpoint.target().contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{base}{joiner}{query}")
}

// --- the callback ----------------------------------------------------------------------------

/// What the redirect carried, once it is known to be for this request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Callback {
    Code {
        code: Secret,
        /// The `iss` parameter, when the server sent one (RFC 9207).
        iss: Option<String>,
    },
    /// The person refused (`error=access_denied`).
    Denied,
    /// Any other `error`; the code only, in a closed alphabet.
    Failed { error: String },
}

/// Why a request to the listener is not the redirect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotTheRedirect {
    /// No `state`, or not the one this flow made.
    State,
    /// Neither a `code` nor an `error`.
    Empty,
}

/// Judge a redirect's query against the `state` the flow made.
pub fn judge_callback(query: &str, expected_state: &str) -> Result<Callback, NotTheRedirect> {
    let pairs = parse_query(query);
    let first = |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    // Compared in constant time; a missing or repeated state is not ours.
    let states = pairs.iter().filter(|(key, _)| key == "state").count();
    let state = first("state").ok_or(NotTheRedirect::State)?;
    if states != 1 || !same_secret(state, expected_state) {
        return Err(NotTheRedirect::State);
    }
    if let Some(error) = first("error") {
        let error: String = error
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect();
        return Ok(if error == "access_denied" {
            Callback::Denied
        } else {
            Callback::Failed { error }
        });
    }
    let code = first("code")
        .filter(|code| !code.is_empty() && code.len() <= 8 * 1024)
        .ok_or(NotTheRedirect::Empty)?;
    Ok(Callback::Code {
        code: Secret::new(code.to_owned()),
        iss: first("iss").map(str::to_owned),
    })
}

/// A pasted callback: the query of a URL (or of a bare `?query`), whatever
/// precedes it ignored.
pub fn query_of_pasted(pasted: &str) -> Option<&str> {
    let pasted = pasted.trim();
    let (_, query) = pasted.split_once('?')?;
    Some(query.split('#').next().unwrap_or_default())
}

// --- the token exchange -----------------------------------------------------------------------

/// The form of an authorization-code exchange.
pub struct ExchangeForm<'a> {
    pub code: &'a Secret,
    pub verifier: &'a Secret,
    pub redirect_uri: &'a str,
    pub client_id: &'a str,
    pub resource: &'a str,
    /// A client secret sent in the body (`client_secret_post`); `None` for a
    /// public client or one that sends it as a Basic header.
    pub secret_in_body: Option<&'a Secret>,
}

pub fn exchange_body(form: &ExchangeForm<'_>) -> String {
    let mut pairs = vec![
        ("grant_type", "authorization_code"),
        ("code", form.code.expose()),
        ("redirect_uri", form.redirect_uri),
        ("client_id", form.client_id),
        ("code_verifier", form.verifier.expose()),
        ("resource", form.resource),
    ];
    if let Some(secret) = form.secret_in_body {
        pairs.push(("client_secret", secret.expose()));
    }
    form_body(&pairs)
}

/// The form of a refresh (slice 3 sends it; the body is defined with the rest).
pub fn refresh_body(
    refresh_token: &Secret,
    client_id: &str,
    resource: &str,
    secret_in_body: Option<&Secret>,
) -> String {
    let mut pairs = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.expose()),
        ("client_id", client_id),
        ("resource", resource),
    ];
    if let Some(secret) = secret_in_body {
        pairs.push(("client_secret", secret.expose()));
    }
    form_body(&pairs)
}

/// The `Authorization` value of a client that authenticates with a Basic
/// header (RFC 6749 §2.3.1): the identifier and the secret, each form-encoded.
pub fn basic_authorization(client_id: &str, secret: &Secret) -> String {
    let pair = format!(
        "{}:{}",
        encode_component(client_id),
        encode_component(secret.expose())
    );
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}

/// What a token endpoint granted.
#[derive(Clone, Debug)]
pub struct Granted {
    pub access: Secret,
    pub refresh: Option<Secret>,
    /// Seconds from the moment the reply arrived; `None` is "until rejected".
    pub expires_in: Option<u64>,
    /// The scopes the server says it granted, when it says.
    pub scopes: Option<Vec<String>>,
}

fn token(
    document: &Value,
    key: &'static str,
    what: &'static str,
    limit: usize,
    empty_is_absent: bool,
) -> Result<Option<Secret>, ProtocolError> {
    let Some(value) = document.get(key) else {
        return Ok(None);
    };
    if empty_is_absent && matches!(value, Value::Null) {
        return Ok(None);
    }
    let text = value.as_str().ok_or(ProtocolError::Token {
        what,
        why: "it is not a string",
    })?;
    if text.is_empty() {
        // Some servers send `"refresh_token": ""` for "none".
        if empty_is_absent {
            return Ok(None);
        }
        return Err(ProtocolError::Token {
            what,
            why: "it is empty",
        });
    }
    if text.len() > limit {
        return Err(ProtocolError::Token {
            what,
            why: "it is longer than 8 KiB",
        });
    }
    // A header carries it: printable ASCII with no space.
    if !text.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err(ProtocolError::Token {
            what,
            why: "it has a byte a header cannot carry",
        });
    }
    Ok(Some(Secret::new(text.to_owned())))
}

/// Read a token endpoint's reply. `issuer` is the discovered one: a reply that
/// names another is refused.
pub fn parse_token_reply(status: u16, body: &[u8], issuer: &Url) -> Result<Granted, ProtocolError> {
    const WHAT: &str = "the token reply";
    if !(200..300).contains(&status) {
        // A refusal says why in a closed alphabet (RFC 6749 §5.2); nothing
        // else of it is kept.
        let error = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .map(|error| {
                error
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .take(64)
                    .collect::<String>()
            })
            .filter(|error| !error.is_empty())
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(ProtocolError::Rejected { error });
    }
    let document = object(body, WHAT)?;
    let token_type = document
        .get("token_type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !token_type.eq_ignore_ascii_case("bearer") {
        return Err(ProtocolError::TokenType {
            got: token_type.to_owned(),
        });
    }
    let access = token(
        &document,
        "access_token",
        "access token",
        MAX_ACCESS_TOKEN_BYTES,
        false,
    )?
    .ok_or(ProtocolError::Token {
        what: "access token",
        why: "the reply has none",
    })?;
    let refresh = token(
        &document,
        "refresh_token",
        "refresh token",
        MAX_REFRESH_TOKEN_BYTES,
        true,
    )?;
    let expires_in = match document.get("expires_in") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let seconds = value
                .as_u64()
                .or_else(|| {
                    value
                        .as_f64()
                        .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                        // An integral float below the lifetime limit.
                        .map(|f| f as u64)
                })
                // Some servers send the number as text.
                .or_else(|| {
                    value
                        .as_str()
                        .and_then(|text| text.trim().parse::<u64>().ok())
                })
                .ok_or(ProtocolError::Lifetime)?;
            if seconds == 0 || seconds > MAX_LIFETIME_SECS {
                return Err(ProtocolError::Lifetime);
            }
            Some(seconds)
        }
    };
    if let Some(iss) = document.get("iss").and_then(Value::as_str) {
        check_iss(iss, issuer)?;
    }
    let scopes = document
        .get("scope")
        .and_then(Value::as_str)
        .map(|scope| crate::mcp_auth::scopes_of(Some(scope)));
    Ok(Granted {
        access,
        refresh,
        expires_in,
        scopes,
    })
}

/// An `iss` (from the redirect or the reply) must be the discovered issuer.
pub fn check_iss(iss: &str, issuer: &Url) -> Result<(), ProtocolError> {
    if iss.trim_end_matches('/') == issuer.to_string().trim_end_matches('/') {
        Ok(())
    } else {
        Err(ProtocolError::IssMismatch {
            expected: issuer.to_string(),
            got: iss.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).expect("url")
    }

    #[test]
    fn urls_are_taken_apart_strictly() {
        let parsed = url("HTTPS://Auth.Example.COM:8443/tenant/a?x=1");
        assert!(parsed.is_https());
        assert_eq!(parsed.host(), "auth.example.com");
        assert_eq!(parsed.port(), 8443);
        assert_eq!(parsed.target(), "/tenant/a?x=1");
        assert_eq!(parsed.path(), "/tenant/a");
        assert_eq!(parsed.origin(), "https://auth.example.com:8443");
        assert_eq!(url("https://h.example").to_string(), "https://h.example/");
        assert_eq!(url("https://h.example:443/x").origin(), "https://h.example");
        assert_eq!(url("http://h.example?q=1").target(), "/?q=1");
        for (text, why) in [
            ("", UrlError::Empty),
            ("ftp://h.example/", UrlError::Scheme),
            ("//h.example/", UrlError::Scheme),
            ("https://user:pw@h.example/", UrlError::Userinfo),
            ("https://h.example/#frag", UrlError::Fragment),
            ("https:///x", UrlError::Host),
            ("https://h.example:0/", UrlError::Port),
            ("https://h.example:99999/", UrlError::Port),
            ("https://h.example:x/", UrlError::Port),
            ("https://h .example/", UrlError::Characters),
            ("https://h.example/a\\b", UrlError::Characters),
            ("https://h.exämple/", UrlError::Characters),
            ("https://-h.example/", UrlError::Host),
            ("https://[::1/", UrlError::Host),
        ] {
            assert_eq!(Url::parse(text), Err(why), "{text:?}");
        }
        assert_eq!(
            Url::parse(&format!("https://h.example/{}", "a".repeat(3000))),
            Err(UrlError::TooLong)
        );
        assert_eq!(url("http://[::1]:8080/x").host(), "[::1]");
    }

    #[test]
    fn loopback_is_this_machine_and_nothing_that_merely_looks_like_it() {
        for host in [
            "http://127.0.0.1/",
            "http://localhost:3000/",
            "http://[::1]/",
            "http://127.9.9.9/",
        ] {
            assert!(url(host).is_loopback(), "{host}");
        }
        for host in [
            "http://localhost.example/",
            "http://127.0.0.1.example/",
            "http://10.0.0.1/",
            "http://0.0.0.0/",
            "https://example.com/",
        ] {
            assert!(!url(host).is_loopback(), "{host}");
        }
    }

    #[test]
    fn well_known_locations_insert_the_suffix_between_host_and_path() {
        assert_eq!(
            url("https://h.example/mcp/x?q=1")
                .well_known("oauth-protected-resource")
                .to_string(),
            "https://h.example/.well-known/oauth-protected-resource/mcp/x"
        );
        assert_eq!(
            url("https://h.example/")
                .well_known("oauth-authorization-server")
                .to_string(),
            "https://h.example/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            url("https://h.example/tenant1/")
                .well_known("openid-configuration")
                .to_string(),
            "https://h.example/.well-known/openid-configuration/tenant1"
        );
    }

    #[test]
    fn a_location_is_joined_to_the_url_it_answered() {
        let base = url("https://h.example/a/b?q=1");
        assert_eq!(base.join("/c").unwrap().to_string(), "https://h.example/c");
        assert_eq!(base.join("c").unwrap().to_string(), "https://h.example/a/c");
        assert_eq!(
            base.join("https://other.example/x#f").unwrap().to_string(),
            "https://other.example/x"
        );
        assert_eq!(base.join("//other.example/x"), Err(UrlError::Scheme));
        assert_eq!(base.join(""), Err(UrlError::Empty));
        assert_eq!(base.join("ftp://x/"), Err(UrlError::Scheme));
        assert_eq!(base.join("javascript:alert(1)"), Err(UrlError::Scheme));
    }

    #[test]
    fn components_round_trip_and_unreserved_characters_stay() {
        let raw = "a b&c=d/é~._-";
        let encoded = encode_component(raw);
        assert_eq!(encoded, "a%20b%26c%3Dd%2F%C3%A9~._-");
        assert_eq!(decode_component(&encoded).as_deref(), Some(raw));
        assert_eq!(decode_component("a+b").as_deref(), Some("a b"));
        assert_eq!(decode_component("%zz"), None);
        assert_eq!(decode_component("%4"), None);
        assert_eq!(decode_component("%ff"), None);
        assert_eq!(
            parse_query("a=1&b=%20x&&c&a=2"),
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), " x".to_owned()),
                ("c".to_owned(), String::new()),
                ("a".to_owned(), "2".to_owned()),
            ]
        );
    }

    #[test]
    fn the_pkce_challenge_is_the_s256_of_the_verifier() {
        // RFC 7636 appendix B.
        assert_eq!(
            challenge_of("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let pair = Pkce::generate().unwrap();
        assert_eq!(pair.verifier().expose().len(), 43);
        assert_eq!(pair.challenge(), challenge_of(pair.verifier().expose()));
        let other = Pkce::generate().unwrap();
        assert_ne!(pair.verifier().expose(), other.verifier().expose());
        assert!(!format!("{pair:?}").contains(pair.verifier().expose()));
    }

    #[test]
    fn state_has_at_least_128_bits_and_is_not_repeated() {
        let first = new_state().unwrap();
        let second = new_state().unwrap();
        // 256 random bits, base64url without padding.
        assert_eq!(first.len(), 43, "{first}");
        assert_eq!(URL_SAFE_NO_PAD.decode(&first).unwrap().len(), 32);
        assert_ne!(first, second);
        assert!(
            first
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn a_secret_prints_as_nothing() {
        let secret = Secret::new("tok-canary-1".to_owned());
        assert_eq!(format!("{secret:?}"), "Secret(..)");
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "abcd"));
    }

    #[test]
    fn text_from_a_document_is_shown_as_printable_ascii_and_bounded() {
        assert_eq!(shown("ok\u{1b}[31m\nred"), "ok?[31m?red");
        assert_eq!(shown("é"), "?");
        let long = shown(&"a".repeat(500));
        assert!(long.len() <= MAX_SHOWN_BYTES + 3, "{}", long.len());
        assert!(long.ends_with("..."));
    }

    fn resource_doc(resource: &str, servers: &[&str]) -> Vec<u8> {
        serde_json::json!({
            "resource": resource,
            "authorization_servers": servers,
            "scopes_supported": ["read", "write"],
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn a_resource_document_must_describe_the_server_being_authorized() {
        let ok = parse_protected_resource(
            &resource_doc("https://mcp.example/mcp", &["https://as.example"]),
            "https://MCP.example/mcp?token=abc",
            false,
        )
        .expect("matches");
        assert_eq!(ok.authorization_servers, vec![url("https://as.example")]);
        assert_eq!(ok.scopes_supported, ["read", "write"]);
        // A trailing slash is the same resource.
        assert!(
            parse_protected_resource(
                &resource_doc("https://mcp.example/mcp/", &["https://as.example"]),
                "https://mcp.example/mcp",
                false
            )
            .is_ok()
        );
        let other = parse_protected_resource(
            &resource_doc("https://other.example/mcp", &["https://as.example"]),
            "https://mcp.example/mcp",
            false,
        )
        .unwrap_err();
        assert!(
            matches!(other, ProtocolError::ResourceMismatch { .. }),
            "{other}"
        );
        let none = parse_protected_resource(
            &resource_doc("https://mcp.example/mcp", &[]),
            "https://mcp.example/mcp",
            false,
        )
        .unwrap_err();
        assert_eq!(none, ProtocolError::NoAuthorizationServer);
    }

    #[test]
    fn plain_http_is_refused_unless_the_server_is_itself_on_this_machine() {
        let doc = resource_doc("http://127.0.0.1:9/mcp", &["http://127.0.0.1:10"]);
        assert!(parse_protected_resource(&doc, "http://127.0.0.1:9/mcp", true).is_ok());
        let refused = parse_protected_resource(&doc, "http://127.0.0.1:9/mcp", false).unwrap_err();
        assert!(
            matches!(refused, ProtocolError::NotHttps { .. }),
            "{refused}"
        );
        // A hosted server cannot send the flow to a plain-http address, this
        // machine's included.
        let doc = resource_doc("https://mcp.example/mcp", &["http://127.0.0.1:10"]);
        let refused = parse_protected_resource(&doc, "https://mcp.example/mcp", false).unwrap_err();
        assert!(
            matches!(refused, ProtocolError::NotHttps { .. }),
            "{refused}"
        );
        // Nor can a server on this machine name a plain-http host elsewhere.
        let doc = resource_doc("http://127.0.0.1:9/mcp", &["http://as.example"]);
        let refused = parse_protected_resource(&doc, "http://127.0.0.1:9/mcp", true).unwrap_err();
        assert!(
            matches!(refused, ProtocolError::NotHttps { .. }),
            "{refused}"
        );
        // One usable server among refused ones is enough.
        let doc = resource_doc(
            "https://mcp.example/mcp",
            &["http://as.example", "https://good.example"],
        );
        let ok = parse_protected_resource(&doc, "https://mcp.example/mcp", false).unwrap();
        assert_eq!(ok.authorization_servers, vec![url("https://good.example")]);
    }

    #[test]
    fn documents_over_64_kib_or_not_objects_are_refused() {
        let big = vec![b' '; MAX_DOCUMENT_BYTES + 1];
        assert!(matches!(
            parse_protected_resource(&big, "https://m.example/", false),
            Err(ProtocolError::TooLarge { .. })
        ));
        assert!(matches!(
            parse_protected_resource(b"[1]", "https://m.example/", false),
            Err(ProtocolError::Malformed { .. })
        ));
        assert!(matches!(
            parse_protected_resource(b"not json", "https://m.example/", false),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    fn server_doc(issuer: &str) -> Vec<u8> {
        serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "registration_endpoint": format!("{issuer}/register"),
            "revocation_endpoint": format!("{issuer}/revoke"),
            "code_challenge_methods_supported": ["plain", "S256"],
            "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn an_authorization_server_must_name_the_issuer_it_was_looked_up_by() {
        let issuer = url("https://as.example");
        let server =
            parse_authorization_server(&server_doc("https://as.example"), &issuer, false).unwrap();
        assert_eq!(server.token_endpoint, url("https://as.example/token"));
        assert!(server.registration_endpoint.is_some());
        assert!(server.revocation_endpoint.is_some());
        assert!(server.secret_in_body());
        assert_eq!(server.hosts(), ["as.example"]);
        let wrong = parse_authorization_server(&server_doc("https://evil.example"), &issuer, false)
            .unwrap_err();
        assert!(
            matches!(wrong, ProtocolError::IssuerMismatch { .. }),
            "{wrong}"
        );
    }

    #[test]
    fn a_server_that_does_not_say_it_supports_s256_is_refused() {
        let issuer = url("https://as.example");
        for methods in [serde_json::json!([]), serde_json::json!(["plain"])] {
            let mut doc: Value = serde_json::from_slice(&server_doc("https://as.example")).unwrap();
            doc["code_challenge_methods_supported"] = methods;
            let err =
                parse_authorization_server(doc.to_string().as_bytes(), &issuer, false).unwrap_err();
            assert_eq!(err, ProtocolError::PkceUnsupported);
        }
        let mut doc: Value = serde_json::from_slice(&server_doc("https://as.example")).unwrap();
        doc.as_object_mut()
            .unwrap()
            .remove("code_challenge_methods_supported");
        assert_eq!(
            parse_authorization_server(doc.to_string().as_bytes(), &issuer, false).unwrap_err(),
            ProtocolError::PkceUnsupported
        );
    }

    #[test]
    fn every_endpoint_of_a_server_document_must_be_https() {
        let issuer = url("https://as.example");
        for key in [
            "authorization_endpoint",
            "token_endpoint",
            "registration_endpoint",
            "revocation_endpoint",
        ] {
            let mut doc: Value = serde_json::from_slice(&server_doc("https://as.example")).unwrap();
            doc[key] = Value::String("http://as.example/x".to_owned());
            let err =
                parse_authorization_server(doc.to_string().as_bytes(), &issuer, false).unwrap_err();
            assert!(
                matches!(err, ProtocolError::NotHttps { .. }),
                "{key}: {err}"
            );
        }
        let mut doc: Value = serde_json::from_slice(&server_doc("https://as.example")).unwrap();
        doc.as_object_mut().unwrap().remove("token_endpoint");
        assert!(matches!(
            parse_authorization_server(doc.to_string().as_bytes(), &issuer, false),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn the_hosts_of_a_server_list_each_once() {
        let issuer = url("https://as.example");
        let mut doc: Value = serde_json::from_slice(&server_doc("https://as.example")).unwrap();
        doc["token_endpoint"] = Value::String("https://tokens.example:8443/t".to_owned());
        let server =
            parse_authorization_server(doc.to_string().as_bytes(), &issuer, false).unwrap();
        assert_eq!(server.hosts(), ["as.example", "tokens.example:8443"]);
    }

    #[test]
    fn a_registration_body_is_a_public_native_client_with_the_loopback_redirect() {
        let body = registration_body("http://127.0.0.1:5555/callback", &["read".to_owned()]);
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["token_endpoint_auth_method"], "none");
        assert_eq!(parsed["application_type"], "native");
        assert_eq!(parsed["redirect_uris"][0], "http://127.0.0.1:5555/callback");
        assert_eq!(parsed["scope"], "read");
        let bare: Value =
            serde_json::from_slice(&registration_body("http://127.0.0.1:1/callback", &[])).unwrap();
        assert!(bare.get("scope").is_none());
    }

    #[test]
    fn a_registration_reply_gives_the_identifier_and_never_a_secret() {
        let ok =
            parse_registration(br#"{"client_id":"abc-123","client_secret":"shh-canary"}"#).unwrap();
        assert_eq!(ok, "abc-123");
        assert_eq!(
            parse_registration(
                br#"{"client_id":"abc","token_endpoint_auth_method":"client_secret_basic"}"#
            ),
            Err(ProtocolError::NotAPublicClient)
        );
        assert!(
            parse_registration(br#"{"client_id":"abc","token_endpoint_auth_method":"none"}"#)
                .is_ok()
        );
        for bad in [
            &br#"{}"#[..],
            br#"{"client_id":""}"#,
            br#"{"client_id":"a b"}"#,
            br#"{"client_id":7}"#,
        ] {
            assert!(
                parse_registration(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let long = format!(r#"{{"client_id":"{}"}}"#, "x".repeat(257));
        assert!(parse_registration(long.as_bytes()).is_err());
    }

    #[test]
    fn the_authorization_url_carries_the_request_and_never_the_verifier() {
        let endpoint = url("https://as.example/authorize?tenant=t1");
        let pkce = Pkce::from_verifier("v".repeat(43));
        let built = authorization_url(&AuthorizationRequest {
            endpoint: &endpoint,
            client_id: "cid 1",
            redirect_uri: "http://127.0.0.1:5555/callback",
            state: "st-1",
            challenge: pkce.challenge(),
            resource: "https://mcp.example/mcp",
            scope: Some("read write"),
        });
        assert!(
            built.starts_with("https://as.example/authorize?tenant=t1&response_type=code"),
            "{built}"
        );
        let query = built.split_once('?').unwrap().1;
        let pairs = parse_query(query);
        let get = |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("client_id"), Some("cid 1"));
        assert_eq!(get("redirect_uri"), Some("http://127.0.0.1:5555/callback"));
        assert_eq!(get("state"), Some("st-1"));
        assert_eq!(get("code_challenge"), Some(pkce.challenge()));
        assert_eq!(get("code_challenge_method"), Some("S256"));
        assert_eq!(get("resource"), Some("https://mcp.example/mcp"));
        assert_eq!(get("scope"), Some("read write"));
        assert!(!built.contains(pkce.verifier().expose()));
        let bare = authorization_url(&AuthorizationRequest {
            endpoint: &url("https://as.example/authorize"),
            client_id: "c",
            redirect_uri: "http://127.0.0.1:1/callback",
            state: "s",
            challenge: "ch",
            resource: "https://mcp.example/mcp",
            scope: None,
        });
        assert!(
            bare.starts_with("https://as.example/authorize?response_type=code"),
            "{bare}"
        );
        assert!(!bare.contains("scope="));
    }

    #[test]
    fn a_redirect_is_ours_only_with_the_state_this_flow_made() {
        let state = "abcDEF_123-xyz";
        assert_eq!(
            judge_callback(&format!("code=C1&state={state}"), state),
            Ok(Callback::Code {
                code: Secret::new("C1".to_owned()),
                iss: None
            })
        );
        assert_eq!(
            judge_callback(
                &format!("state={state}&code=C%2F1&iss=https%3A%2F%2Fas.example"),
                state
            ),
            Ok(Callback::Code {
                code: Secret::new("C/1".to_owned()),
                iss: Some("https://as.example".to_owned())
            })
        );
        assert_eq!(judge_callback("code=C1", state), Err(NotTheRedirect::State));
        assert_eq!(
            judge_callback("code=C1&state=nope", state),
            Err(NotTheRedirect::State)
        );
        assert_eq!(
            judge_callback(&format!("code=C1&state={state}&state={state}"), state),
            Err(NotTheRedirect::State)
        );
        assert_eq!(
            judge_callback(&format!("state={state}"), state),
            Err(NotTheRedirect::Empty)
        );
        assert_eq!(
            judge_callback(&format!("state={state}&code="), state),
            Err(NotTheRedirect::Empty)
        );
        assert_eq!(
            judge_callback(&format!("error=access_denied&state={state}"), state),
            Ok(Callback::Denied)
        );
        assert_eq!(
            judge_callback(&format!("error=server%20error%1b[31m&state={state}"), state),
            Ok(Callback::Failed {
                error: "servererror31m".to_owned()
            })
        );
    }

    #[test]
    fn a_pasted_callback_is_read_for_its_query_only() {
        assert_eq!(
            query_of_pasted(" http://127.0.0.1:5/callback?code=a&state=b#x \n"),
            Some("code=a&state=b")
        );
        assert_eq!(query_of_pasted("?code=a"), Some("code=a"));
        assert_eq!(query_of_pasted("no query"), None);
    }

    #[test]
    fn the_exchange_body_carries_the_verifier_and_the_resource() {
        let code = Secret::new("code-canary".to_owned());
        let verifier = Secret::new("verifier-canary".to_owned());
        let body = exchange_body(&ExchangeForm {
            code: &code,
            verifier: &verifier,
            redirect_uri: "http://127.0.0.1:5555/callback",
            client_id: "cid",
            resource: "https://mcp.example/mcp",
            secret_in_body: None,
        });
        let pairs = parse_query(&body);
        let get = |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("grant_type"), Some("authorization_code"));
        assert_eq!(get("code"), Some("code-canary"));
        assert_eq!(get("code_verifier"), Some("verifier-canary"));
        assert_eq!(get("redirect_uri"), Some("http://127.0.0.1:5555/callback"));
        assert_eq!(get("resource"), Some("https://mcp.example/mcp"));
        assert_eq!(get("client_secret"), None);
        let secret = Secret::new("secret-canary".to_owned());
        let with = exchange_body(&ExchangeForm {
            code: &code,
            verifier: &verifier,
            redirect_uri: "r",
            client_id: "cid",
            resource: "x",
            secret_in_body: Some(&secret),
        });
        assert!(with.contains("client_secret=secret-canary"));
    }

    #[test]
    fn a_basic_header_form_encodes_each_part() {
        let secret = Secret::new("s:e/c".to_owned());
        let header = basic_authorization("cid", &secret);
        let encoded = header.strip_prefix("Basic ").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(decoded, b"cid:s%3Ae%2Fc");
    }

    fn reply(extra: Value) -> Vec<u8> {
        let mut base = serde_json::json!({"access_token": "at-1", "token_type": "Bearer"});
        for (key, value) in extra.as_object().unwrap() {
            base[key] = value.clone();
        }
        base.to_string().into_bytes()
    }

    #[test]
    fn a_good_token_reply_is_read() {
        let issuer = url("https://as.example");
        let granted = parse_token_reply(
            200,
            &reply(serde_json::json!({
                "token_type": "bearer", "refresh_token": "rt-1", "expires_in": 3600,
                "scope": "read write", "iss": "https://as.example/"
            })),
            &issuer,
        )
        .unwrap();
        assert_eq!(granted.access.expose(), "at-1");
        assert_eq!(granted.refresh.as_ref().map(Secret::expose), Some("rt-1"));
        assert_eq!(granted.expires_in, Some(3600));
        assert_eq!(
            granted.scopes,
            Some(vec!["read".to_owned(), "write".to_owned()])
        );
        let plain = parse_token_reply(200, &reply(serde_json::json!({})), &issuer).unwrap();
        assert_eq!(plain.expires_in, None);
        assert!(plain.refresh.is_none());
        let float = parse_token_reply(
            200,
            &reply(serde_json::json!({"expires_in": 3600.0})),
            &issuer,
        )
        .unwrap();
        assert_eq!(float.expires_in, Some(3600));
        // Servers that send the lifetime as text, and "no refresh token" as an
        // empty one or null, are read as such.
        let text = parse_token_reply(
            200,
            &reply(serde_json::json!({"expires_in": " 7200 ", "refresh_token": ""})),
            &issuer,
        )
        .unwrap();
        assert_eq!(text.expires_in, Some(7200));
        assert!(text.refresh.is_none());
        let null = parse_token_reply(
            200,
            &reply(serde_json::json!({"refresh_token": null})),
            &issuer,
        )
        .unwrap();
        assert!(null.refresh.is_none());
    }

    #[test]
    fn a_bad_token_reply_is_refused_with_a_typed_reason() {
        let issuer = url("https://as.example");
        let refused = |extra: Value| parse_token_reply(200, &reply(extra), &issuer).unwrap_err();
        assert!(matches!(
            refused(serde_json::json!({"token_type": "mac"})),
            ProtocolError::TokenType { .. }
        ));
        assert!(matches!(
            refused(serde_json::json!({"token_type": null})),
            ProtocolError::TokenType { .. }
        ));
        assert!(matches!(
            refused(serde_json::json!({"access_token": ""})),
            ProtocolError::Token {
                why: "it is empty",
                ..
            }
        ));
        assert!(matches!(
            refused(serde_json::json!({"access_token": "a".repeat(MAX_ACCESS_TOKEN_BYTES + 1)})),
            ProtocolError::Token { .. }
        ));
        assert!(matches!(
            refused(serde_json::json!({"access_token": "has space"})),
            ProtocolError::Token { .. }
        ));
        assert!(matches!(
            refused(serde_json::json!({"access_token": "line\nbreak"})),
            ProtocolError::Token { .. }
        ));
        assert!(matches!(
            refused(serde_json::json!({"refresh_token": "r".repeat(MAX_REFRESH_TOKEN_BYTES + 1)})),
            ProtocolError::Token { .. }
        ));
        for lifetime in [
            serde_json::json!(0),
            serde_json::json!(-5),
            serde_json::json!(MAX_LIFETIME_SECS + 1),
            serde_json::json!(1.5),
            serde_json::json!("soon"),
            serde_json::json!("-5"),
            serde_json::json!("0"),
        ] {
            assert_eq!(
                refused(serde_json::json!({"expires_in": lifetime})),
                ProtocolError::Lifetime
            );
        }
        assert!(
            parse_token_reply(
                200,
                &reply(serde_json::json!({"expires_in": MAX_LIFETIME_SECS})),
                &issuer
            )
            .is_ok()
        );
        assert!(matches!(
            refused(serde_json::json!({"iss": "https://evil.example"})),
            ProtocolError::IssMismatch { .. }
        ));
        let mut no_token: Value = serde_json::from_slice(&reply(serde_json::json!({}))).unwrap();
        no_token.as_object_mut().unwrap().remove("access_token");
        assert!(matches!(
            parse_token_reply(200, no_token.to_string().as_bytes(), &issuer),
            Err(ProtocolError::Token { .. })
        ));
    }

    #[test]
    fn a_refusal_from_the_token_endpoint_keeps_only_its_error_code() {
        let issuer = url("https://as.example");
        let err = parse_token_reply(
            400,
            br#"{"error":"invalid_grant","error_description":"secret-canary <script>"}"#,
            &issuer,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ProtocolError::Rejected {
                error: "invalid_grant".to_owned()
            }
        );
        assert!(!err.to_string().contains("secret-canary"));
        let blank = parse_token_reply(500, b"<html>", &issuer).unwrap_err();
        assert_eq!(
            blank,
            ProtocolError::Rejected {
                error: "HTTP 500".to_owned()
            }
        );
    }

    #[test]
    fn the_iss_of_a_redirect_must_be_the_discovered_issuer() {
        let issuer = url("https://as.example");
        assert!(check_iss("https://as.example", &issuer).is_ok());
        assert!(check_iss("https://as.example/", &issuer).is_ok());
        assert!(matches!(
            check_iss("https://evil.example", &issuer),
            Err(ProtocolError::IssMismatch { .. })
        ));
    }

    #[test]
    fn an_error_text_never_carries_a_control_byte() {
        let err = ProtocolError::Malformed {
            what: "the registration reply",
            why: "bad\u{1b}[31m\nnews".to_owned(),
        };
        let text = err.to_string();
        assert!(text.bytes().all(|b| (0x20..=0x7e).contains(&b)), "{text:?}");
    }
}
