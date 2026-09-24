//! How a provider connection is dialled (SEAM-02): through an HTTP proxy
//! when the environment names one, and only where a composition root's
//! [`DialGate`] permits.
//!
//! * [`ProxyConfig`] reads `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` (either
//!   case) from explicit environment pairs — the caller decides where they
//!   come from. Only `http://` proxies are supported: an `https://` proxy
//!   is refused when the configuration is read, not silently ignored. A
//!   loopback target (`localhost`, `127.0.0.0/8`, `::1`) is always dialled
//!   directly — a local model server behind a corporate proxy variable is
//!   the common case.
//! * An `https://` target through a proxy is a `CONNECT` tunnel with TLS to
//!   the target inside it; an `http://` target through a proxy is an
//!   absolute-form request to the proxy.
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
/// Most `NO_PROXY` entries honoured.
const MAX_NO_PROXY_ENTRIES: usize = 64;

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
    no_proxy: Vec<String>,
}

/// Why the proxy configuration was refused. Never echoes a credential.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProxyConfigError {
    /// `<variable>` does not hold an `http://host[:port]` proxy URL.
    Invalid { variable: &'static str },
    /// `<variable>` names an `https://` proxy, which is not supported.
    HttpsProxyUnsupported { variable: &'static str },
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
        }
    }
}

impl std::error::Error for ProxyConfigError {}

impl ProxyConfig {
    /// Read the proxy variables from `env` (upper case wins over lower
    /// case, as most tools read them). No variable set: no proxy.
    pub fn from_env(env: &[(String, String)]) -> Result<Self, ProxyConfigError> {
        let value = |upper: &'static str, lower: &'static str| -> Option<(&'static str, String)> {
            [upper, lower].into_iter().find_map(|name| {
                env.iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.trim().to_owned())
                    .filter(|value| !value.is_empty())
                    .map(|value| (name, value))
            })
        };
        let https = value("HTTPS_PROXY", "https_proxy")
            .map(|(name, url)| parse_proxy(name, &url))
            .transpose()?;
        let http = value("HTTP_PROXY", "http_proxy")
            .map(|(name, url)| parse_proxy(name, &url))
            .transpose()?;
        let no_proxy = value("NO_PROXY", "no_proxy")
            .map(|(_, list)| {
                list.split(',')
                    .map(|entry| entry.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|entry| !entry.is_empty())
                    .take(MAX_NO_PROXY_ENTRIES)
                    .collect()
            })
            .unwrap_or_default();
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
        if is_loopback_host(&host) || self.bypasses(&host, port) {
            return None;
        }
        Some(proxy)
    }

    fn bypasses(&self, host: &str, port: u16) -> bool {
        self.no_proxy.iter().any(|entry| {
            if entry == "*" {
                return true;
            }
            let (name, entry_port) = match entry.rsplit_once(':') {
                Some((name, entry_port)) if !name.contains(':') => {
                    (name, entry_port.parse::<u16>().ok())
                }
                _ => (entry.as_str(), None),
            };
            if entry_port.is_some_and(|entry_port| entry_port != port) {
                return false;
            }
            host == name || host.ends_with(&format!(".{name}"))
        })
    }
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
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
        let port = match after.strip_prefix(':') {
            Some(port) => port.parse().map_err(|_| invalid.clone())?,
            None => 80,
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
    let authorization = userinfo
        .map(|userinfo| percent_decode(userinfo).ok_or(invalid))
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
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
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
/// (`ProviderError::Connection` reads as "could not reach it").
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
    while !head.ends_with(b"\r\n\r\n") {
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
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or(ProviderError::Connection)?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(ProviderError::Connection)
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
    fn proxy_variables_are_read_upper_case_first_and_credentials_never_printed() {
        let config = ProxyConfig::from_env(&env(&[
            ("https_proxy", "http://lower:1"),
            ("HTTPS_PROXY", "http://user:s3cret@proxy.corp.example:3128/"),
            ("HTTP_PROXY", "proxy.corp.example:8080"),
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
            let err = ProxyConfig::from_env(&env(&[("HTTP_PROXY", bad)])).expect_err(bad);
            assert!(!err.to_string().contains("pw"), "{err}");
            assert!(err.to_string().contains("HTTP_PROXY"), "{err}");
        }
    }

    #[test]
    fn a_connect_tunnel_needs_a_2xx_from_the_proxy() {
        use std::net::TcpListener;
        for (answer, ok) in [
            ("HTTP/1.1 200 Connection established\r\n\r\n", true),
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
