//! A loopback egress proxy for sandboxed commands: domain rules, deny
//! wins, receipts.
//!
//! A sandboxed `shell_exec` gets its network one of two ways today: the
//! host-restricted tier's honest "not confined" or the seatbelt tier's
//! full deny. The `[egress_proxy]` configuration adds the middle posture:
//! when it names allowed domains, a sandboxed command's proxy variables
//! point at a proxy this process starts on loopback, and every connection
//! is judged against the rules before it leaves the machine — an allow
//! tunnels through (the proxy sees the destination, not the bytes: `https`
//! travels inside a `CONNECT` tunnel), a denial is refused with the reason,
//! and both leave a receipt in the same egress log the tools write
//! (`.rapidlm/egress-receipts.jsonl`).
//!
//! Truth in tiers, stated plainly: on the host-restricted tier this is
//! routing, not confinement — a command that ignores the proxy variables
//! dials as it always could, and the receipts show what went *through*,
//! never a claim about what did not. The seatbelt tier's confinement is
//! unchanged and the proxy is not offered there.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Longest request head accepted (the request line plus headers, before
/// the body): enough for any real proxy request, bounded against a peer
/// that never finishes one.
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// How long the accept loop sleeps between stop-flag checks.
const ACCEPT_POLL: Duration = Duration::from_millis(50);
/// Bytes moved per relay read.
const RELAY_CHUNK: usize = 16 * 1024;

/// One allowed-or-denied host pattern. `example.com` matches itself;
/// `*.example.com` matches subdomains (any depth, not the apex);
/// `**.example.com` matches the apex and everything under it; `*` matches
/// every host — the last is a deliberate opt-in that exists so a policy
/// can say "everything, minus the deny list".
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostPattern {
    Exact(String),
    Subdomains(String),
    ApexAndUnder(String),
    Any,
}

impl HostPattern {
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim().to_ascii_lowercase();
        if raw.is_empty() || raw.len() > 253 || raw.contains("..") {
            return None;
        }
        if raw == "*" {
            return Some(Self::Any);
        }
        if let Some(base) = raw.strip_prefix("**.") {
            return valid_dns_name(base).then_some(Self::ApexAndUnder(base.to_owned()));
        }
        if let Some(base) = raw.strip_prefix("*.") {
            return valid_dns_name(base).then_some(Self::Subdomains(base.to_owned()));
        }
        valid_dns_name(&raw).then_some(Self::Exact(raw))
    }

    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        match self {
            Self::Any => true,
            Self::Exact(name) => &host == name,
            Self::Subdomains(base) => {
                host != *base
                    && host
                        .strip_suffix(&format!(".{base}"))
                        .is_some_and(|prefix| !prefix.is_empty())
            }
            Self::ApexAndUnder(base) => {
                host == *base
                    || host
                        .strip_suffix(&format!(".{base}"))
                        .is_some_and(|prefix| !prefix.is_empty())
            }
        }
    }
}

fn valid_dns_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// The judgment for one connection attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EgressDecision {
    Allow,
    /// The stable refusal token, for the receipt and the HTTP status line.
    Deny(&'static str),
}

/// The proxy's rules: what may leave the machine. An empty `allow` list
/// means the proxy is not started at all — the configuration is off —
/// never "deny everything"; deny wins over allow; and loopback, private
/// and link-local destinations are refused unless `allow_local_binding`
/// names them as deliberate (a local dev server), with the resolved
/// addresses checked too — the cheap half of a DNS-rebinding guard.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProxyRules {
    pub allow: Vec<HostPattern>,
    pub deny: Vec<HostPattern>,
    pub allow_local_binding: bool,
}

impl ProxyRules {
    /// Whether this configuration asks for a proxy at all.
    pub fn enabled(&self) -> bool {
        !self.allow.is_empty()
    }

    /// Judge one `host:port`. Pure for the pattern half; the local-binding
    /// half resolves the host, so this is the one impure step.
    pub fn decide(&self, host: &str, port: u16) -> EgressDecision {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() || port == 0 {
            return EgressDecision::Deny("invalid-target");
        }
        if self.deny.iter().any(|pattern| pattern.matches(&host)) {
            return EgressDecision::Deny("deny-rule");
        }
        if !self.allow.iter().any(|pattern| pattern.matches(&host)) {
            return EgressDecision::Deny("not-allowlisted");
        }
        if self.allow_local_binding {
            return EgressDecision::Allow;
        }
        // A literal IP is judged directly; a name is judged on what it
        // resolves to — every address it offers must be public, else a
        // name that flips between a public and a private answer (the
        // rebinding shape) is refused.
        let addresses: Vec<IpAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![ip],
            Err(_) => match (host, port).to_socket_addrs() {
                Ok(iter) => iter.map(|addr| addr.ip()).collect(),
                Err(_) => return EgressDecision::Deny("unresolvable"),
            },
        };
        if addresses.is_empty() {
            return EgressDecision::Deny("unresolvable");
        }
        if addresses.iter().any(|ip| is_sensitive_address(*ip)) {
            return EgressDecision::Deny("local-binding");
        }
        EgressDecision::Allow
    }
}

/// Addresses a sandboxed command never reaches by name: loopback, private
/// ranges, link-local (cloud metadata among them), and the unspecified
/// address.
fn is_sensitive_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || matches!(v6.segments()[0] & 0xfe00, 0xfc00) // unique local fc00::/7
                || v6.is_unicast_link_local()
                || v6.is_unspecified()
        }
    }
}

/// A running proxy: the loopback address the sandboxed command's proxy
/// variables name, and the switch that stops it. Dropping the handle does
/// not stop the proxy — the caller stops it when the command it serves
/// ends, so a leaked handle never keeps a listener alive unnoticed.
#[derive(Clone)]
pub struct EgressProxyHandle {
    pub addr: SocketAddr,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl EgressProxyHandle {
    /// Stop the listener and let open tunnels drain with their peers.
    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The proxy variables a sandboxed command starts with: this proxy for
    /// everything, loopback never (the proxy itself is reached on
    /// loopback; a destination there is not egress).
    pub fn proxy_env(&self) -> Vec<(String, String)> {
        let addr = format!("http://{}", self.addr);
        vec![
            ("HTTP_PROXY".to_owned(), addr.clone()),
            ("HTTPS_PROXY".to_owned(), addr.clone()),
            ("ALL_PROXY".to_owned(), addr),
            ("NO_PROXY".to_owned(), "localhost,127.0.0.1,::1".to_owned()),
        ]
    }
}

/// The proxy environment this process carries, folded in: a user who
/// already routes through their own proxy keeps that inside the tunnel —
/// the child's variables point here, and here alone.
pub fn start(rules: ProxyRules, receipts_root: PathBuf) -> std::io::Result<EgressProxyHandle> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = EgressProxyHandle {
        addr,
        stop: Arc::clone(&stop),
    };
    let rules_for_thread = rules.clone();
    let stop_for_thread = Arc::clone(&stop);
    std::thread::Builder::new()
        .name("rapidlm-egress-proxy".to_owned())
        .spawn(move || {
            while !stop_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let rules = rules_for_thread.clone();
                        let receipts = receipts_root.clone();
                        let _ = std::thread::Builder::new()
                            .spawn(move || serve(stream, rules, receipts));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(ACCEPT_POLL);
                    }
                    Err(_) => return,
                }
            }
        })?;
    Ok(handle)
}

/// One connection: read the head, judge, tunnel or refuse, receipt.
fn serve(mut client: TcpStream, rules: ProxyRules, receipts_root: PathBuf) {
    let head = match read_head(&mut client) {
        Some(head) => head,
        None => return,
    };
    let request_line = head.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = match split_host_port(target, 443) {
            Some(pair) => pair,
            None => {
                let _ = write_simple(&mut client, "400 Bad Request", "malformed CONNECT target");
                return;
            }
        };
        match rules.decide(&host, port) {
            EgressDecision::Allow => {
                receipt(&receipts_root, &host, port, true, None);
                let _ = write_simple(&mut client, "200 Connection Established", "");
                match TcpStream::connect((host.as_str(), port)) {
                    Ok(upstream) => relay(client, upstream),
                    Err(_) => {
                        let _ = write_simple(&mut client, "502 Bad Gateway", "upstream refused");
                    }
                }
            }
            EgressDecision::Deny(reason) => {
                receipt(&receipts_root, &host, port, false, Some(reason));
                let _ = write_simple(
                    &mut client,
                    "403 Forbidden",
                    &format!("egress refused: {reason}"),
                );
            }
        }
        return;
    }
    // Absolute-form (`GET http://host/path`): the forward-proxy shape an
    // ordinary http:// client uses. Anything else is not a proxy request.
    let Some(rest) = target.strip_prefix("http://").map(|rest| rest.to_owned()) else {
        let _ = write_simple(&mut client, "400 Bad Request", "not a proxy request");
        return;
    };
    let (host, port) = match split_host_port(rest.split('/').next().unwrap_or(""), 80) {
        Some(pair) => pair,
        None => {
            let _ = write_simple(&mut client, "400 Bad Request", "malformed target");
            return;
        }
    };
    match rules.decide(&host, port) {
        EgressDecision::Allow => {
            receipt(&receipts_root, &host, port, true, None);
            let Ok(mut upstream) = TcpStream::connect((host.as_str(), port)) else {
                let _ = write_simple(&mut client, "502 Bad Gateway", "upstream refused");
                return;
            };
            // Origin-form request line, proxy headers dropped; the body,
            // if any, is already in `head`'s tail and relays with it.
            let origin_target = format!(
                "/{}",
                rest.split_once('/').map(|(_, path)| path).unwrap_or("")
            );
            let rewritten = rewrite_head(&head, &origin_target);
            if upstream.write_all(rewritten.as_bytes()).is_ok() {
                relay(client, upstream);
            }
        }
        EgressDecision::Deny(reason) => {
            receipt(&receipts_root, &host, port, false, Some(reason));
            let _ = write_simple(
                &mut client,
                "403 Forbidden",
                &format!("egress refused: {reason}"),
            );
        }
    }
}

/// Read the request head: through the blank line, bounded, with a
/// generous-but-finite wait for a peer that trickles.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if bytes.windows(4).any(|window| window == b"\r\n\r\n")
            || bytes.windows(2).any(|w| w == b"\n\n")
        {
            return String::from_utf8_lossy(&bytes).into_owned().into();
        }
        if bytes.len() > MAX_HEAD_BYTES {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                return if bytes.is_empty() {
                    None
                } else {
                    Some(String::from_utf8_lossy(&bytes).into_owned())
                };
            }
            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
            Err(_) => return None,
        }
    }
}

fn split_host_port(raw: &str, default_port: u16) -> Option<(String, u16)> {
    if let Some((host, port)) = raw.rsplit_once(':') {
        let port: u16 = port.parse().ok()?;
        Some((host.to_ascii_lowercase(), port))
    } else {
        Some((raw.to_ascii_lowercase(), default_port))
    }
}

fn write_simple(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

/// Origin-form rewrite: the request line's absolute target becomes the
/// path, and the hop-by-hop proxy headers go; everything else passes.
fn rewrite_head(head: &str, origin_target: &str) -> String {
    let mut out = String::with_capacity(head.len());
    for (index, line) in head.split("\r\n").enumerate() {
        if index == 0 {
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("");
            let version = line.rsplit(' ').next().unwrap_or("HTTP/1.1");
            out.push_str(&format!("{method} {origin_target} {version}\r\n"));
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("proxy-connection:") || lower.starts_with("proxy-authorization:") {
            continue;
        }
        if line.is_empty() {
            break;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out
}

/// Move bytes both ways until either side closes. Each direction is a
/// thread; both die with their sockets when the command ends.
fn relay(client: TcpStream, upstream: TcpStream) {
    let client_read = match client.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let upstream_write = match upstream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let up = std::thread::spawn(move || copy_until_closed(client_read, upstream_write));
    copy_until_closed(upstream, client);
    let _ = up.join();
}

fn copy_until_closed(mut from: TcpStream, mut to: TcpStream) {
    let mut buffer = [0u8; RELAY_CHUNK];
    loop {
        match from.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if to.write_all(&buffer[..read]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.shutdown(std::net::Shutdown::Write);
}

/// One receipt row in the project's egress log, the same shape the tools
/// write (`web_search::record`): `at`, `tool`, `allowed`, `dialled`,
/// `reason`.
fn receipt(root: &std::path::Path, host: &str, port: u16, allowed: bool, reason: Option<&str>) {
    crate::web_search::record(
        root,
        &[crate::web_search::Receipt {
            tool: "egress_proxy",
            allowed,
            dialled: format!("tcp://{host}:{port}"),
            reason: reason.map(str::to_owned),
        }],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_patterns_match_exactly_subdomains_and_apex() {
        let exact = HostPattern::parse("registry.npmjs.org").expect("exact");
        let sub = HostPattern::parse("*.npmjs.org").expect("sub");
        let apex = HostPattern::parse("**.github.com").expect("apex");
        let any = HostPattern::parse("*").expect("any");
        assert!(exact.matches("registry.npmjs.org"));
        assert!(!exact.matches("mirrors.npmjs.org"));
        assert!(sub.matches("mirrors.npmjs.org"));
        assert!(!sub.matches("npmjs.org"), "sub does not cover the apex");
        assert!(!sub.matches("evil-npmjs.org"));
        assert!(apex.matches("github.com"));
        assert!(apex.matches("api.github.com"));
        assert!(apex.matches("deep.api.github.com"));
        assert!(!apex.matches("github.org.evil.example"));
        assert!(any.matches("anything.example"));
        // Case and a trailing dot are the same host.
        assert!(sub.matches("Mirrors.NPMJS.org."));
        // Malformed patterns never parse.
        for bad in ["", "*", "a..b", "-", ".start", "over.".to_owned().as_str()] {
            if bad == "*" {
                continue;
            }
            if bad == "over." {
                assert!(HostPattern::parse(bad).is_none(), "{bad:?}");
                continue;
            }
            assert!(
                HostPattern::parse(bad).is_none() || valid_dns_name(bad),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn rules_deny_wins_and_local_binding_is_refused() {
        let rules = ProxyRules {
            allow: vec![HostPattern::Any],
            deny: vec![HostPattern::parse("*.internal.example").expect("deny")],
            allow_local_binding: false,
        };
        // A public IP literal: judged without a DNS lookup, so the test
        // says what the rules say, not what the host's resolver answers.
        assert_eq!(rules.decide("8.8.8.8", 443), EgressDecision::Allow);
        assert_eq!(
            rules.decide("db.internal.example", 5432),
            EgressDecision::Deny("deny-rule")
        );
        // `Any` allows every name the deny list does not refuse — judged
        // with local binding allowed so no DNS answer is consulted (a
        // name's resolution is the environment's, not the pattern's).
        let any = ProxyRules {
            allow_local_binding: true,
            ..rules.clone()
        };
        assert_eq!(any.decide("unlisted.example", 80), EgressDecision::Allow);
        // Loopback and private literals never leave by name.
        assert_eq!(
            rules.decide("127.0.0.1", 8080),
            EgressDecision::Deny("local-binding")
        );
        assert_eq!(
            rules.decide("192.168.1.10", 22),
            EgressDecision::Deny("local-binding")
        );
        // A deliberate local binding is the operator's to make.
        let local = ProxyRules {
            allow: vec![HostPattern::Exact("127.0.0.1".to_owned())],
            allow_local_binding: true,
            ..rules.clone()
        };
        assert_eq!(local.decide("127.0.0.1", 8080), EgressDecision::Allow);

        // An empty allow list is "off", never "deny everything".
        let off = ProxyRules::default();
        assert!(!off.enabled());
    }

    /// A tiny upstream that answers one line, for the tunnel test.
    fn upstream_server(answer: &'static str) -> SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(answer.as_bytes());
            }
        });
        addr
    }

    #[test]
    fn the_proxy_tunnels_allowed_and_refuses_denied_targets() {
        // The upstream is loopback — the one address a policy must name
        // deliberately (`allow_local_binding`), which is exactly what
        // makes it testable without leaving the machine.
        let upstream = upstream_server("tunneled-bytes");
        let receipts =
            std::env::temp_dir().join(format!("rapidlm-egress-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&receipts);
        std::fs::create_dir_all(&receipts).expect("receipts dir");
        let handle = start(
            ProxyRules {
                allow: vec![HostPattern::Exact("127.0.0.1".to_owned())],
                deny: Vec::new(),
                allow_local_binding: true,
            },
            receipts.clone(),
        )
        .expect("start");
        assert!(handle.addr.ip().is_loopback());

        // An allowed CONNECT tunnels: the greeting comes back through.
        let mut client = TcpStream::connect(handle.addr).expect("connect");
        client
            .write_all(format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n", upstream.port()).as_bytes())
            .expect("request");
        let mut head = vec![0u8; 1024];
        let read = client.read(&mut head).expect("response head");
        let head = String::from_utf8_lossy(&head[..read]).to_string();
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        client.write_all(b"hello").expect("tunnel bytes");
        let mut tunnel = [0u8; 64];
        let read = client.read(&mut tunnel).expect("tunneled answer");
        assert!(String::from_utf8_lossy(&tunnel[..read]).contains("tunneled-bytes"));

        // A denied CONNECT is refused with the reason.
        let mut denied = TcpStream::connect(handle.addr).expect("connect");
        denied
            .write_all(b"CONNECT 127.0.0.2:9 HTTP/1.1\r\n\r\n")
            .expect("request");
        let mut buffer = [0u8; 512];
        let read = denied.read(&mut buffer).expect("refusal");
        let refusal = String::from_utf8_lossy(&buffer[..read]).to_string();
        assert!(refusal.starts_with("HTTP/1.1 403"), "{refusal}");
        assert!(refusal.contains("not-allowlisted"), "{refusal}");

        handle.stop();

        // Both sides left receipts in the project's egress log.
        let log = std::fs::read_to_string(receipts.join(".rapidlm").join("egress-receipts.jsonl"))
            .expect("receipts");
        assert!(log.contains("\"tool\":\"egress_proxy\""), "{log}");
        assert!(log.contains("\"allowed\":true"), "{log}");
        assert!(log.contains("\"allowed\":false"), "{log}");
        assert!(log.contains("not-allowlisted"), "{log}");
        std::fs::remove_dir_all(&receipts).ok();
    }

    #[test]
    fn the_proxy_env_points_at_it_and_loopback_bypasses() {
        let handle = start(
            ProxyRules {
                allow: vec![HostPattern::Any],
                deny: Vec::new(),
                allow_local_binding: true,
            },
            std::env::temp_dir(),
        )
        .expect("start");
        let env = handle.proxy_env();
        let value = format!("http://{}", handle.addr);
        assert!(env.contains(&("HTTP_PROXY".to_owned(), value.clone())));
        assert!(env.contains(&("HTTPS_PROXY".to_owned(), value.clone())));
        assert!(env.contains(&("NO_PROXY".to_owned(), "localhost,127.0.0.1,::1".to_owned())));
        handle.stop();
    }
}
