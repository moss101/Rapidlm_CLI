//! How a provider connection is dialled (SEAM-02): through an HTTP proxy
//! when the environment names one, and only where a composition root's
//! [`DialGate`] permits.
//!
//! * [`ProxyConfig`] reads `https_proxy` / `HTTPS_PROXY`, `http_proxy`
//!   (lower case only, except on Windows, where names compare ignoring
//!   case: the upper-case name can be set from a request header in some
//!   server environments) and `no_proxy` / `NO_PROXY` from explicit
//!   environment pairs — the caller decides where they come from. Only
//!   `http://` proxies are supported: an `https://` proxy is refused when
//!   the configuration is read, not silently ignored. A loopback target
//!   (`localhost`, `127.0.0.0/8` in any spelling, `::1`) is always dialled
//!   directly — a local model server behind a corporate proxy variable is
//!   the common case. `NO_PROXY` takes names (and their subdomains, `.x` and
//!   `*.x` alike), exact addresses, address ranges (`10.0.0.0/8`) and `*`.
//! * An `https://` target through a proxy is a `CONNECT` tunnel with TLS to
//!   the target inside it: the proxy sees neither the request nor its
//!   credential. An `http://` target through a proxy is an absolute-form
//!   request to the proxy — which then sees the whole request, its bearer
//!   included, as anything on a plain-http path does.
//! * A proxied target is not resolved to be dialled (the proxy resolves it).
//!   Without a gate, a name that resolves locally to an address the
//!   transport's guard refuses is refused through the proxy too (best
//!   effort: a name only the proxy can resolve goes through). With a gate,
//!   the gate alone judges the target through a proxy — it is told the
//!   target and asked about the proxy's addresses.
//! * [`DialGate`] is asked before any connection is made, with the target
//!   the request is for, the proxy it goes through, and the addresses about
//!   to be dialled; it returns the addresses that may be dialled (an egress
//!   policy binds them to its own resolution) or refuses.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Instant;

use crate::provider::{CancellationToken, ProviderError};

/// Most bytes of a proxy's `CONNECT` response head read before giving up.
const MAX_CONNECT_RESPONSE_BYTES: usize = 8 * 1024;
/// Most `NO_PROXY` entries read; more is refused, not cut short.
const MAX_NO_PROXY_ENTRIES: usize = 256;

/// One HTTP proxy: where to dial it and, when its URL carried credentials,
/// the `Proxy-Authorization` value (never printed: `Debug` redacts it).
#[derive(Clone, Eq, PartialEq)]
pub struct ProxyTarget {
    host: String,
    port: u16,
    authorization: Option<String>,
}

impl std::fmt::Debug for ProxyTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyTarget")
            .field("host", &self.host)
            .field("port", &self.port)
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

impl ProxyTarget {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn authorization(&self) -> Option<&str> {
        self.authorization.as_deref()
    }
}

/// The proxies the environment names for provider connections.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProxyConfig {
    https: Option<ProxyTarget>,
    http: Option<ProxyTarget>,
    no_proxy: Vec<NoProxyEntry>,
}

/// One `NO_PROXY` entry.
#[derive(Clone, Debug, Eq, PartialEq)]
enum NoProxyEntry {
    /// `*`: nothing is proxied.
    Everything,
    /// A name and its subdomains, on any port or one.
    Name { name: String, port: Option<u16> },
    /// One address, on any port or one.
    Address { ip: IpAddr, port: Option<u16> },
    /// An address range (`10.0.0.0/8`, `fd00::/8`).
    Range { network: IpAddr, prefix: u8 },
}

/// Why the proxy configuration was refused. Never echoes a credential.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProxyConfigError {
    /// `<variable>` does not hold an `http://host[:port]` proxy URL.
    Invalid { variable: &'static str },
    /// `<variable>` names an `https://` proxy, which is not supported.
    HttpsProxyUnsupported { variable: &'static str },
    /// `<variable>` lists more entries than are read.
    TooManyEntries { variable: &'static str },
    /// One `<variable>` entry is not a name, an address or a range.
    InvalidEntry {
        variable: &'static str,
        entry: String,
    },
}

impl std::fmt::Display for ProxyConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid { variable } => {
                write!(f, "{variable} is not an http://host[:port] proxy URL")
            }
            Self::HttpsProxyUnsupported { variable } => write!(
                f,
                "{variable} names an https:// proxy; only http:// proxies are supported"
            ),
            Self::TooManyEntries { variable } => write!(
                f,
                "{variable} lists more than {MAX_NO_PROXY_ENTRIES} entries"
            ),
            Self::InvalidEntry { variable, entry } => write!(
                f,
                "{variable} entry {:?} is not a name, an address (with an optional port) or a \
range such as 10.0.0.0/8",
                shown_entry(entry)
            ),
        }
    }
}

impl std::error::Error for ProxyConfigError {}

/// An entry as an error shows it: anything before an `@` (a credential in a
/// mistaken URL) withheld, and at most 64 characters.
fn shown_entry(entry: &str) -> String {
    let shown = match entry.rsplit_once('@') {
        Some((_, host)) => format!("[withheld]@{host}"),
        None => entry.to_owned(),
    };
    match shown.char_indices().nth(64) {
        Some((cut, _)) => format!("{}...", &shown[..cut]),
        None => shown,
    }
}

impl ProxyConfig {
    /// Read the proxy variables from `env`: `https_proxy` then
    /// `HTTPS_PROXY`, `http_proxy` (lower case only), `no_proxy` then
    /// `NO_PROXY`. A variable set but empty turns its proxy off rather than
    /// falling back to the other spelling. Names are compared ignoring case
    /// on Windows, as the system compares them. No variable set: no proxy.
    pub fn from_env(env: &[(String, String)]) -> Result<Self, ProxyConfigError> {
        // Exact spellings first, in order; then — on Windows, where names
        // compare ignoring case — any spelling. So the preference holds
        // everywhere; an error names the spelling looked up.
        let value = |names: &[&'static str]| -> Option<(&'static str, String)> {
            let exact = names.iter().find_map(|name| {
                env.iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| (*name, value.trim().to_owned()))
            });
            let any_case = || {
                names.iter().find_map(|name| {
                    env.iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(name))
                        .map(|(_, value)| (*name, value.trim().to_owned()))
                })
            };
            exact
                .or_else(|| if cfg!(windows) { any_case() } else { None })
                .filter(|(_, value)| !value.is_empty())
        };
        let https = value(&["https_proxy", "HTTPS_PROXY"])
            .map(|(name, url)| parse_proxy(name, &url))
            .transpose()?;
        let http = value(&["http_proxy"])
            .map(|(name, url)| parse_proxy(name, &url))
            .transpose()?;
        // Read only when a proxy is set: nothing else consults it.
        let no_proxy = match value(&["no_proxy", "NO_PROXY"]) {
            Some((name, list)) if https.is_some() || http.is_some() => parse_no_proxy(name, &list)?,
            _ => Vec::new(),
        };
        Ok(Self {
            https,
            http,
            no_proxy,
        })
    }

    /// Whether any proxy is configured.
    pub fn is_empty(&self) -> bool {
        self.https.is_none() && self.http.is_none()
    }

    /// The proxy a connection to `host:port` over `https` (or not) goes
    /// through; `None` to dial it directly.
    pub fn for_target(&self, https: bool, host: &str, port: u16) -> Option<&ProxyTarget> {
        let proxy = if https {
            self.https.as_ref()
        } else {
            self.http.as_ref()
        }?;
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        let address = literal_address(&host);
        if is_loopback_host(&host, address) || self.bypasses(&host, address, port) {
            return None;
        }
        Some(proxy)
    }

    fn bypasses(&self, host: &str, address: Option<IpAddr>, port: u16) -> bool {
        let on_port = |entry_port: &Option<u16>| entry_port.is_none_or(|entry| entry == port);
        self.no_proxy.iter().any(|entry| match entry {
            NoProxyEntry::Everything => true,
            // A name never matches an address by suffix (`0.1` is not a
            // domain of `10.0.0.1`).
            NoProxyEntry::Name { name, port } => {
                address.is_none()
                    && on_port(port)
                    && (host == name || host.ends_with(&format!(".{name}")))
            }
            NoProxyEntry::Address { ip, port } => address == Some(*ip) && on_port(port),
            NoProxyEntry::Range { network, prefix } => {
                address.is_some_and(|ip| in_range(ip, *network, *prefix))
            }
        })
    }
}

/// The address a host names literally, in any spelling the transport
/// accepts (`127.1` and `0x7f000001` included).
fn literal_address(host: &str) -> Option<IpAddr> {
    host.parse::<IpAddr>()
        .ok()
        .or_else(|| crate::providers::openai_compatible::decode_ipv4_literal(host).map(IpAddr::V4))
}

fn is_loopback_host(host: &str, address: Option<IpAddr>) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || address.is_some_and(|ip| ip.is_loopback())
}

fn in_range(ip: IpAddr, network: IpAddr, prefix: u8) -> bool {
    match (ip, network) {
        (IpAddr::V4(ip), IpAddr::V4(network)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            u32::from(ip) & mask == u32::from(network) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(network)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            u128::from(ip) & mask == u128::from(network) & mask
        }
        _ => false,
    }
}

fn parse_no_proxy(
    variable: &'static str,
    list: &str,
) -> Result<Vec<NoProxyEntry>, ProxyConfigError> {
    let entries: Vec<&str> = list
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .filter(|entry| !entry.is_empty())
        .collect();
    if entries.len() > MAX_NO_PROXY_ENTRIES {
        return Err(ProxyConfigError::TooManyEntries { variable });
    }
    entries
        .into_iter()
        .map(|raw| {
            let invalid = ProxyConfigError::InvalidEntry {
                variable,
                entry: raw.to_owned(),
            };
            let entry = raw.to_ascii_lowercase();
            if entry == "*" {
                return Ok(NoProxyEntry::Everything);
            }
            if let Some((network, prefix)) = entry.split_once('/') {
                let network = network
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .map_err(|_| invalid.clone())?;
                let prefix = prefix.parse::<u8>().map_err(|_| invalid.clone())?;
                let bits = if network.is_ipv4() { 32 } else { 128 };
                if prefix > bits {
                    return Err(invalid.clone());
                }
                return Ok(NoProxyEntry::Range { network, prefix });
            }
            // `[addr]:port`, `host:port`, a bare IPv6 address, or a name.
            let (host, port) = if let Some(bracketed) = entry.strip_prefix('[') {
                let (host, after) = bracketed.split_once(']').ok_or(invalid.clone())?;
                let port = match after {
                    "" => None,
                    rest => Some(
                        rest.strip_prefix(':')
                            .and_then(|port| port.parse::<u16>().ok())
                            .ok_or(invalid.clone())?,
                    ),
                };
                (host.to_owned(), port)
            } else if entry.matches(':').count() == 1 {
                let (host, port) = entry.split_once(':').ok_or(invalid.clone())?;
                (
                    host.to_owned(),
                    Some(port.parse::<u16>().map_err(|_| invalid.clone())?),
                )
            } else {
                (entry.clone(), None)
            };
            if let Some(ip) = literal_address(&host) {
                return Ok(NoProxyEntry::Address { ip, port });
            }
            let name = host.trim_start_matches("*.").trim_start_matches('.');
            if name.is_empty() || name.contains(['*', '/', ' ']) {
                return Err(invalid.clone());
            }
            Ok(NoProxyEntry::Name {
                name: name.to_owned(),
                port,
            })
        })
        .collect()
}

fn parse_proxy(variable: &'static str, url: &str) -> Result<ProxyTarget, ProxyConfigError> {
    let invalid = ProxyConfigError::Invalid { variable };
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        // `host:port` with no scheme is read as `http://` (common usage).
        None => ("http".to_owned(), url),
    };
    match scheme.as_str() {
        "http" => {}
        "https" => return Err(ProxyConfigError::HttpsProxyUnsupported { variable }),
        _ => return Err(invalid),
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, authority),
    };
    let (host, port) = if let Some(bracketed) = hostport.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']').ok_or(invalid.clone())?;
        let port = match after {
            "" => 80,
            rest => rest
                .strip_prefix(':')
                .and_then(|port| port.parse().ok())
                .ok_or(invalid.clone())?,
        };
        (host.to_owned(), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), port.parse().map_err(|_| invalid.clone())?),
            None => (hostport.to_owned(), 80),
        }
    };
    if host.is_empty()
        || port == 0
        || host
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '/')
    {
        return Err(invalid);
    }
    // Basic credentials are `user:password`; a user alone has an empty one.
    let authorization = userinfo
        .map(|userinfo| {
            percent_decode(userinfo)
                .map(|mut decoded| {
                    if !decoded.contains(&b':') {
                        decoded.push(b':');
                    }
                    decoded
                })
                .ok_or(invalid)
        })
        .transpose()?
        .map(|userinfo| format!("Basic {}", base64(&userinfo)));
    Ok(ProxyTarget {
        host,
        port,
        authorization,
    })
}

/// `%XX` escapes decoded (a proxy URL's user and password may carry
/// reserved characters that way); `None` for a malformed escape.
fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let pair = bytes.get(index + 1..index + 3)?;
            if !pair.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    Some(out)
}

/// Standard base64 with padding (for `Proxy-Authorization: Basic`).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(
                    ALPHABET[((triple >> (18 - 6 * index)) & 0x3f) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Asked before a provider connection is dialled (S10: egress only through
/// policy). `target` is where the request is for — scheme, host, port —
/// `via` the proxy it goes through, if any, and `addrs` the addresses about
/// to be dialled (the proxy's when there is one). Returns the addresses that
/// may be dialled — a gate that resolves the name itself returns its own
/// resolution, so a second, different DNS answer is never used — or refuses
/// (`ProviderError::Connection` reads as "could not reach it"). The name to
/// resolve is the proxy's when `via` is set (the transport dials the proxy),
/// the target's otherwise; `addrs` is empty when the system resolver found
/// nothing. Through a proxy the transport does not look at the target's
/// addresses at all: a gate that must keep a name off refused addresses
/// judges `target` itself. What the gate returns is dialled only on the port being dialled
/// and only where the transport's address guard allows it.
pub trait DialGate: Send + Sync {
    fn permit(
        &self,
        target: DialTarget<'_>,
        via: Option<&ProxyTarget>,
        addrs: &[SocketAddr],
    ) -> Result<Vec<SocketAddr>, ProviderError>;
}

/// The origin a provider request is for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DialTarget<'a> {
    pub https: bool,
    pub host: &'a str,
    pub port: u16,
}

/// Open a `CONNECT` tunnel to `host:port` through the proxy `tcp` is
/// connected to. `Ok` once the proxy answered 2xx; the stream then carries
/// bytes to the target.
pub(crate) fn connect_tunnel(
    tcp: &mut TcpStream,
    host: &str,
    port: u16,
    authorization: Option<&str>,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(), ProviderError> {
    cancel.check()?;
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(authorization) = authorization {
        request.push_str("Proxy-Authorization: ");
        request.push_str(authorization);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    tcp.write_all(request.as_bytes())
        .map_err(|_| ProviderError::Connection)?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    // Byte by byte, so nothing past the proxy's head (the target's TLS
    // handshake) is consumed here.
    while !(head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n")) {
        cancel.check()?;
        if Instant::now() >= deadline || head.len() >= MAX_CONNECT_RESPONSE_BYTES {
            return Err(ProviderError::Connection);
        }
        match tcp.read(&mut byte) {
            Ok(0) => return Err(ProviderError::Connection),
            Ok(_) => head.push(byte[0]),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(ProviderError::Connection),
        }
    }
    let status_line = head
        .split(|byte| *byte == b'\n')
        .next()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .unwrap_or_default();
    if !status_line.starts_with("HTTP/") {
        return Err(ProviderError::Connection);
    }
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or(ProviderError::Connection)?;
    match status {
        200..=299 => Ok(()),
        // The proxy refused its own credentials: asking again sends the same
        // ones (and can lock a directory account). An authentication failure
        // is the class nothing retries.
        407 => Err(ProviderError::AuthFailed),
        _ => Err(ProviderError::Connection),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn proxy_variables_are_read_lower_case_first_and_credentials_never_printed() {
        let config = ProxyConfig::from_env(&env(&[
            ("HTTPS_PROXY", "http://upper:1"),
            ("https_proxy", "http://user:s3cret@proxy.corp.example:3128/"),
            ("http_proxy", "proxy.corp.example:8080"),
        ]))
        .expect("config");
        let https = config
            .for_target(true, "api.example.com", 443)
            .expect("proxied");
        assert_eq!((https.host(), https.port()), ("proxy.corp.example", 3128));
        assert_eq!(https.authorization(), Some("Basic dXNlcjpzM2NyZXQ="));
        assert!(!format!("{config:?}").contains("s3cret"));
        assert!(!format!("{config:?}").contains("dXNlcjpzM2NyZXQ"));
        let http = config
            .for_target(false, "gw.example.com", 80)
            .expect("proxied");
        assert_eq!((http.host(), http.port()), ("proxy.corp.example", 8080));
        assert!(ProxyConfig::from_env(&[]).expect("none").is_empty());
        // Reserved characters in the user or password arrive percent-encoded.
        let encoded =
            ProxyConfig::from_env(&env(&[("HTTPS_PROXY", "http://us%40er:p%3Ass@proxy:1")]))
                .expect("config");
        assert_eq!(
            encoded
                .for_target(true, "api.example.com", 443)
                .and_then(ProxyTarget::authorization),
            Some(format!("Basic {}", base64(b"us@er:p:ss")).as_str())
        );
        assert!(ProxyConfig::from_env(&env(&[("HTTPS_PROXY", "http://u:%zz@proxy:1")])).is_err());
        assert!(ProxyConfig::from_env(&env(&[("HTTPS_PROXY", "http://u:%+1@proxy:1")])).is_err());
        // A user alone is `user:` (an empty password).
        let user_only =
            ProxyConfig::from_env(&env(&[("HTTPS_PROXY", "http://user@proxy:1")])).expect("config");
        assert_eq!(
            user_only
                .for_target(true, "api.example.com", 443)
                .and_then(ProxyTarget::authorization),
            Some(format!("Basic {}", base64(b"user:")).as_str())
        );
        assert!(ProxyConfig::from_env(&env(&[("HTTPS_PROXY", "http://[::1]x")])).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn the_upper_case_http_proxy_is_not_read_outside_windows() {
        // The upper-case name can arrive from a request header in some
        // server environments.
        let upper_only =
            ProxyConfig::from_env(&env(&[("HTTP_PROXY", "http://proxy:3128")])).expect("config");
        assert!(upper_only.for_target(false, "gw.example.com", 80).is_none());
    }

    #[test]
    fn no_proxy_takes_names_addresses_and_ranges_and_refuses_what_it_cannot_read() {
        let config = ProxyConfig::from_env(&env(&[
            ("https_proxy", "http://proxy:3128"),
            (
                "no_proxy",
                "*.corp.example, 10.0.0.0/8, 192.168.1.7, [fd00::1]:8443, 0.1",
            ),
        ]))
        .expect("config");
        for (host, port, direct) in [
            ("api.corp.example", 443, true),
            ("corp.example", 443, true),
            ("10.1.2.3", 11434, true),
            ("192.168.1.7", 443, true),
            ("192.168.1.8", 443, false),
            ("fd00::1", 8443, true),
            ("fd00::1", 443, false),
            // A name entry never matches an address by suffix.
            ("172.16.0.1", 443, false),
            ("api.example.com", 443, false),
        ] {
            assert_eq!(
                config.for_target(true, host, port).is_none(),
                direct,
                "{host}:{port}"
            );
        }
        // Loopback in any spelling the transport accepts goes direct.
        for local in ["127.1", "0x7f000001", "127.000.000.001"] {
            assert!(config.for_target(true, local, 443).is_none(), "{local}");
        }
        let many = vec!["a.example"; MAX_NO_PROXY_ENTRIES + 1].join(",");
        assert_eq!(
            ProxyConfig::from_env(&env(&[
                ("https_proxy", "http://proxy:3128"),
                ("no_proxy", many.as_str()),
            ]))
            .expect_err("too many"),
            ProxyConfigError::TooManyEntries {
                variable: "no_proxy"
            }
        );
        for bad in ["10.0.0.0/40", "host:port", "192.168.*"] {
            let err = ProxyConfig::from_env(&env(&[
                ("https_proxy", "http://proxy:3128"),
                ("no_proxy", bad),
            ]))
            .expect_err(bad);
            assert!(err.to_string().contains(bad), "the entry is named: {err}");
        }
        // Entries may be separated by whitespace too; and without a proxy
        // nothing reads the list, so a bad one breaks nothing.
        let spaced = ProxyConfig::from_env(&env(&[
            ("https_proxy", "http://proxy:3128"),
            ("no_proxy", "a.example b.example"),
        ]))
        .expect("config");
        assert!(spaced.for_target(true, "b.example", 443).is_none());
        assert!(ProxyConfig::from_env(&env(&[("no_proxy", "192.168.*")])).is_ok());
        // A mistaken URL in the list: its credential is never shown.
        let err = ProxyConfig::from_env(&env(&[
            ("https_proxy", "http://proxy:3128"),
            ("no_proxy", "http://svc:hunter2@internal.example"),
        ]))
        .expect_err("a URL is not an entry");
        assert!(!err.to_string().contains("hunter2"), "{err}");
        assert!(
            err.to_string().contains("[withheld]@internal.example"),
            "{err}"
        );
        // Set but empty turns a proxy off, on every system.
        let off = ProxyConfig::from_env(&env(&[
            ("https_proxy", ""),
            ("HTTPS_PROXY", "http://proxy:3128"),
        ]))
        .expect("config");
        assert!(off.for_target(true, "api.example.com", 443).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_any_spelling_of_a_name_is_read() {
        let config =
            ProxyConfig::from_env(&env(&[("Https_Proxy", "http://proxy:3128")])).expect("config");
        assert!(config.for_target(true, "api.example.com", 443).is_some());
    }

    #[test]
    fn no_proxy_and_loopback_are_dialled_directly() {
        let config = ProxyConfig::from_env(&env(&[
            ("HTTPS_PROXY", "http://proxy:3128"),
            (
                "NO_PROXY",
                "internal.example, .corp.example,gw.example:8443",
            ),
        ]))
        .expect("config");
        assert!(config.for_target(true, "internal.example", 443).is_none());
        assert!(
            config
                .for_target(true, "api.internal.example", 443)
                .is_none()
        );
        assert!(config.for_target(true, "x.corp.example", 443).is_none());
        assert!(config.for_target(true, "gw.example", 8443).is_none());
        assert!(
            config.for_target(true, "gw.example", 443).is_some(),
            "port-specific entry"
        );
        assert!(
            config
                .for_target(true, "notinternal.example", 443)
                .is_some()
        );
        for local in ["localhost", "127.0.0.1", "::1", "[::1]", "a.localhost"] {
            assert!(config.for_target(true, local, 443).is_none(), "{local}");
        }
        let everything = ProxyConfig::from_env(&env(&[
            ("HTTPS_PROXY", "http://proxy:3128"),
            ("no_proxy", "*"),
        ]))
        .expect("config");
        assert!(
            everything
                .for_target(true, "api.example.com", 443)
                .is_none()
        );
    }

    #[test]
    fn an_unusable_proxy_url_is_refused_naming_the_variable_not_its_value() {
        let err = ProxyConfig::from_env(&env(&[("HTTPS_PROXY", "https://proxy:3128")]))
            .expect_err("https proxy");
        assert_eq!(
            err,
            ProxyConfigError::HttpsProxyUnsupported {
                variable: "HTTPS_PROXY"
            }
        );
        for bad in [
            "socks5://proxy:1080",
            "http://user:pw@:3128",
            "http://proxy:notaport",
        ] {
            let err = ProxyConfig::from_env(&env(&[("http_proxy", bad)])).expect_err(bad);
            assert!(!err.to_string().contains("pw"), "{err}");
            assert!(err.to_string().contains("http_proxy"), "{err}");
        }
    }

    #[test]
    fn a_connect_tunnel_needs_a_2xx_from_the_proxy() {
        use std::net::TcpListener;
        for (answer, ok) in [
            ("HTTP/1.1 200 Connection established\r\n\r\n", true),
            ("HTTP/1.1 200 OK\n\n", true),
            ("SPDY 200 OK\r\n\r\n", false),
            ("HTTP/1.1 407 Proxy Authentication Required\r\n\r\n", false),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr");
            let proxy = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 1024];
                let read = stream.read(&mut buf).expect("read");
                stream.write_all(answer.as_bytes()).expect("write");
                String::from_utf8_lossy(&buf[..read]).into_owned()
            });
            let mut tcp = TcpStream::connect(addr).expect("connect");
            let result = connect_tunnel(
                &mut tcp,
                "api.example.com",
                443,
                Some("Basic abc"),
                &CancellationToken::new(),
                Instant::now() + std::time::Duration::from_secs(5),
            );
            let request = proxy.join().expect("proxy");
            assert!(
                request.starts_with("CONNECT api.example.com:443 HTTP/1.1\r\n"),
                "{request}"
            );
            assert!(
                request.contains("Proxy-Authorization: Basic abc\r\n"),
                "{request}"
            );
            assert_eq!(result.is_ok(), ok, "{answer}");
            if answer.contains(" 407 ") {
                assert_eq!(result, Err(ProviderError::AuthFailed), "not retried");
            }
        }
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
    }
}
