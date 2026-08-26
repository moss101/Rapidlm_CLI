//! Network action normalizer.
//!
//! Freezes scheme/host/port, resolved IP identities, and IP classes *before*
//! policy hashing. Redirect hops are a new action and always carry
//! revalidation metadata. DNS answers come from a trusted resolver, never
//! from the untrusted intent.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::command::CancellationToken;
use crate::capability::{CapabilityError, Hostname, NetworkScheme};

/// Maximum UTF-8 bytes for an untrusted URL or Location value.
pub const MAX_URL_BYTES: usize = 4096;

/// Maximum distinct resolved addresses retained for one target.
pub const MAX_RESOLVED_IPS: usize = 16;

/// Maximum redirect hops accepted as a new normalize request.
pub const MAX_REDIRECT_HOPS: u8 = 8;

const POLICY_TAG: &[u8] = b"rapidlm.canonical_network.v1";
const CANCEL_STRIDE: usize = 8;

/// Well-known IPv4 instance-metadata endpoints.
const METADATA_V4: &[[u8; 4]] = &[
    [169, 254, 169, 254],
    [169, 254, 170, 2],
    [169, 254, 170, 23],
    [100, 100, 100, 200],
];

/// AWS IMDS IPv6 (`fd00:ec2::254`).
const METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);

/// Untrusted connect or redirect request. Model/tool supplied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkIntent {
    url: String,
    hop: u8,
    previous: Option<RedirectHop>,
}

/// DNS name or literal address after origin parsing.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum CanonicalNetHost {
    Dns(Hostname),
    Ip(IpAddr),
}

/// Address class used by policy. One address may belong to several classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum IpClass {
    Loopback,
    Private,
    LinkLocal,
    MetadataLike,
    Unspecified,
    Public,
}

/// Previous hop bound into redirect revalidation metadata.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RedirectHop {
    scheme: NetworkScheme,
    host: CanonicalNetHost,
    port: u16,
}

/// Redirect metadata. Executors must re-normalize and re-authorize every hop.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RedirectRevalidation {
    hop: u8,
    previous: Option<RedirectHop>,
}

/// Normalized network target bound into policy hashing.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CanonicalNetworkTarget {
    scheme: NetworkScheme,
    host: CanonicalNetHost,
    port: u16,
    resolved_ips: Vec<IpAddr>,
    ip_classes: BTreeSet<IpClass>,
    redirect: RedirectRevalidation,
}

/// Resolves a DNS host into concrete addresses.
///
/// Search configuration and hosts-file identity come from this trusted
/// resolver, never from the intent.
pub trait NetworkResolver {
    fn resolve(&self, host: &Hostname) -> Result<Vec<IpAddr>, NetworkNormalizeError>;
}

/// Typed normalize failure. Display never echoes attacker-controlled input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkNormalizeError {
    Cancelled,
    EmptyUrl,
    TooLong,
    Nul,
    Control,
    UnknownScheme,
    InvalidUrl,
    InvalidHost,
    InvalidPort,
    Userinfo,
    UnresolvedHost,
    TooManyIps,
    TooManyRedirects,
}

impl NetworkIntent {
    /// Initial connect. `url` is an absolute `scheme://host[:port][…]` value.
    pub fn connect(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            hop: 0,
            previous: None,
        }
    }

    /// Follow a redirect `Location`. Relative locations fail during normalize.
    pub fn redirect(
        previous: &CanonicalNetworkTarget,
        location: impl Into<String>,
    ) -> Result<Self, NetworkNormalizeError> {
        let hop = previous
            .redirect
            .hop
            .checked_add(1)
            .ok_or(NetworkNormalizeError::TooManyRedirects)?;
        if hop > MAX_REDIRECT_HOPS {
            return Err(NetworkNormalizeError::TooManyRedirects);
        }
        Ok(Self {
            url: location.into(),
            hop,
            previous: Some(RedirectHop {
                scheme: previous.scheme,
                host: previous.host.clone(),
                port: previous.port,
            }),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn hop(&self) -> u8 {
        self.hop
    }

    pub fn previous(&self) -> Option<&RedirectHop> {
        self.previous.as_ref()
    }
}

impl CanonicalNetHost {
    pub fn dns(&self) -> Option<&Hostname> {
        match self {
            Self::Dns(host) => Some(host),
            Self::Ip(_) => None,
        }
    }

    pub fn ip(&self) -> Option<IpAddr> {
        match self {
            Self::Ip(ip) => Some(*ip),
            Self::Dns(_) => None,
        }
    }

    pub fn as_canonical_str(&self) -> String {
        match self {
            Self::Dns(host) => host.as_str().to_owned(),
            Self::Ip(ip) => ip.to_string(),
        }
    }
}

impl fmt::Display for CanonicalNetHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_canonical_str())
    }
}

impl IpClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::Private => "private",
            Self::LinkLocal => "link_local",
            Self::MetadataLike => "metadata_like",
            Self::Unspecified => "unspecified",
            Self::Public => "public",
        }
    }
}

impl fmt::Display for IpClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl RedirectHop {
    pub fn scheme(&self) -> NetworkScheme {
        self.scheme
    }

    pub fn host(&self) -> &CanonicalNetHost {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl RedirectRevalidation {
    /// Every hop, including the first, must be revalidated after a redirect.
    pub const fn requires_revalidation(&self) -> bool {
        true
    }

    pub fn hop(&self) -> u8 {
        self.hop
    }

    pub fn previous(&self) -> Option<&RedirectHop> {
        self.previous.as_ref()
    }
}

impl CanonicalNetworkTarget {
    pub fn scheme(&self) -> NetworkScheme {
        self.scheme
    }

    pub fn host(&self) -> &CanonicalNetHost {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn resolved_ips(&self) -> &[IpAddr] {
        &self.resolved_ips
    }

    pub fn ip_classes(&self) -> &BTreeSet<IpClass> {
        &self.ip_classes
    }

    pub fn redirect(&self) -> &RedirectRevalidation {
        &self.redirect
    }

    /// Stable bytes for action-hash input. Raw URLs and userinfo are omitted.
    pub fn policy_bytes(&self) -> Vec<u8> {
        let host = self.host.as_canonical_str();
        let mut out = Vec::with_capacity(64 + host.len() + self.resolved_ips.len() * 16);
        out.extend_from_slice(POLICY_TAG);
        out.push(0);
        out.extend_from_slice(self.scheme.as_str().as_bytes());
        out.push(0);
        out.extend_from_slice(host.as_bytes());
        out.push(0);
        out.extend_from_slice(self.port.to_string().as_bytes());
        out.push(0);
        out.extend_from_slice(b"ips");
        out.push(0);
        for ip in &self.resolved_ips {
            out.extend_from_slice(ip.to_string().as_bytes());
            out.push(0);
        }
        out.extend_from_slice(b"classes");
        out.push(0);
        for class in &self.ip_classes {
            out.extend_from_slice(class.as_str().as_bytes());
            out.push(0);
        }
        out.extend_from_slice(b"redirect");
        out.push(0);
        out.extend_from_slice(self.redirect.hop.to_string().as_bytes());
        out.push(0);
        if let Some(prev) = &self.redirect.previous {
            out.extend_from_slice(prev.scheme.as_str().as_bytes());
            out.push(0);
            out.extend_from_slice(prev.host.as_canonical_str().as_bytes());
            out.push(0);
            out.extend_from_slice(prev.port.to_string().as_bytes());
            out.push(0);
        }
        out
    }
}

/// Normalize scheme/host/port, resolve IPs, and attach redirect metadata.
pub fn normalize_network<R: NetworkResolver + ?Sized>(
    intent: &NetworkIntent,
    resolver: &R,
    cancel: &CancellationToken,
) -> Result<CanonicalNetworkTarget, NetworkNormalizeError> {
    cancel_check(cancel)?;
    if intent.hop > MAX_REDIRECT_HOPS {
        return Err(NetworkNormalizeError::TooManyRedirects);
    }
    let (scheme, host, port) = parse_absolute_url(&intent.url)?;
    cancel_check(cancel)?;
    let resolved_ips = resolve_ips(&host, resolver, cancel)?;
    let ip_classes = classify_target(&host, &resolved_ips);
    Ok(CanonicalNetworkTarget {
        scheme,
        host,
        port,
        resolved_ips,
        ip_classes,
        redirect: RedirectRevalidation {
            hop: intent.hop,
            previous: intent.previous.clone(),
        },
    })
}

fn parse_absolute_url(
    url: &str,
) -> Result<(NetworkScheme, CanonicalNetHost, u16), NetworkNormalizeError> {
    validate_url_text(url)?;
    let (scheme_raw, rest) = url
        .split_once("://")
        .ok_or(NetworkNormalizeError::InvalidUrl)?;
    if scheme_raw.is_empty() || rest.is_empty() {
        return Err(NetworkNormalizeError::InvalidUrl);
    }
    let scheme: NetworkScheme = scheme_raw
        .to_ascii_lowercase()
        .parse()
        .map_err(map_scheme)?;
    if rest.contains('@') {
        return Err(NetworkNormalizeError::Userinfo);
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return Err(NetworkNormalizeError::InvalidHost);
    }
    let (host_raw, port) = split_authority(authority, scheme)?;
    let host = parse_host(host_raw)?;
    Ok((scheme, host, port))
}

fn split_authority(
    authority: &str,
    scheme: NetworkScheme,
) -> Result<(&str, u16), NetworkNormalizeError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (addr, after) = rest
            .split_once(']')
            .ok_or(NetworkNormalizeError::InvalidHost)?;
        if addr.is_empty() {
            return Err(NetworkNormalizeError::InvalidHost);
        }
        let port = match after {
            "" => scheme.default_port(),
            _ => {
                let port_raw = after
                    .strip_prefix(':')
                    .ok_or(NetworkNormalizeError::InvalidHost)?;
                parse_port(port_raw)?
            }
        };
        return Ok((addr, port));
    }
    if authority.contains('[') || authority.contains(']') {
        return Err(NetworkNormalizeError::InvalidHost);
    }
    match authority.rsplit_once(':') {
        Some((host, _port_raw)) if host.contains(':') => {
            // Unbracketed IPv6. Fail closed rather than guess the port split.
            Err(NetworkNormalizeError::InvalidHost)
        }
        Some((host, port_raw)) => {
            if host.is_empty() {
                return Err(NetworkNormalizeError::InvalidHost);
            }
            Ok((host, parse_port(port_raw)?))
        }
        None => Ok((authority, scheme.default_port())),
    }
}

fn parse_port(raw: &str) -> Result<u16, NetworkNormalizeError> {
    if raw.is_empty() {
        return Err(NetworkNormalizeError::InvalidPort);
    }
    if raw.len() > 1 && raw.starts_with('0') {
        return Err(NetworkNormalizeError::InvalidPort);
    }
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(NetworkNormalizeError::InvalidPort);
    }
    let port: u16 = raw
        .parse()
        .map_err(|_| NetworkNormalizeError::InvalidPort)?;
    if port == 0 {
        return Err(NetworkNormalizeError::InvalidPort);
    }
    Ok(port)
}

fn parse_host(raw: &str) -> Result<CanonicalNetHost, NetworkNormalizeError> {
    if raw.is_empty() {
        return Err(NetworkNormalizeError::InvalidHost);
    }
    if raw.contains('%') {
        return Err(NetworkNormalizeError::InvalidHost);
    }
    if raw.chars().any(|c| !c.is_ascii()) {
        return Err(NetworkNormalizeError::InvalidHost);
    }
    if let Some(ip) = parse_literal_ip(raw)? {
        return Ok(CanonicalNetHost::Ip(ip));
    }
    if is_ambiguous_ip_encoding(raw) {
        return Err(NetworkNormalizeError::InvalidHost);
    }
    let trimmed = raw.strip_suffix('.').unwrap_or(raw);
    if trimmed.is_empty() || trimmed.starts_with('.') || trimmed.ends_with('.') {
        return Err(NetworkNormalizeError::InvalidHost);
    }
    let host = Hostname::parse(trimmed).map_err(map_host)?;
    Ok(CanonicalNetHost::Dns(host))
}

fn parse_literal_ip(raw: &str) -> Result<Option<IpAddr>, NetworkNormalizeError> {
    if looks_like_strict_ipv4(raw) {
        return Ok(Some(IpAddr::V4(parse_strict_ipv4(raw)?)));
    }
    if raw.contains(':') {
        let ip: Ipv6Addr = raw
            .parse()
            .map_err(|_| NetworkNormalizeError::InvalidHost)?;
        return Ok(Some(IpAddr::V6(ip)));
    }
    Ok(None)
}

fn looks_like_strict_ipv4(raw: &str) -> bool {
    let mut dots = 0usize;
    if raw.is_empty() {
        return false;
    }
    for b in raw.bytes() {
        if b == b'.' {
            dots += 1;
        } else if !b.is_ascii_digit() {
            return false;
        }
    }
    dots == 3
}

fn parse_strict_ipv4(raw: &str) -> Result<Ipv4Addr, NetworkNormalizeError> {
    let mut octets = [0u8; 4];
    let mut idx = 0usize;
    for part in raw.split('.') {
        if idx >= 4 {
            return Err(NetworkNormalizeError::InvalidHost);
        }
        if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
            return Err(NetworkNormalizeError::InvalidHost);
        }
        octets[idx] = part
            .parse()
            .map_err(|_| NetworkNormalizeError::InvalidHost)?;
        idx += 1;
    }
    if idx != 4 {
        return Err(NetworkNormalizeError::InvalidHost);
    }
    Ok(Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]))
}

fn is_ambiguous_ip_encoding(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("0x") || lower.contains("0x") {
        return true;
    }
    if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    if raw.contains('.') && raw.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return true;
    }
    false
}

fn resolve_ips<R: NetworkResolver + ?Sized>(
    host: &CanonicalNetHost,
    resolver: &R,
    cancel: &CancellationToken,
) -> Result<Vec<IpAddr>, NetworkNormalizeError> {
    cancel_check(cancel)?;
    let mut ips = match host {
        CanonicalNetHost::Ip(ip) => vec![*ip],
        CanonicalNetHost::Dns(name) => resolver.resolve(name)?,
    };
    if ips.is_empty() {
        return Err(NetworkNormalizeError::UnresolvedHost);
    }
    if ips.len() > MAX_RESOLVED_IPS {
        return Err(NetworkNormalizeError::TooManyIps);
    }
    for (i, ip) in ips.iter().enumerate() {
        if i.is_multiple_of(CANCEL_STRIDE) {
            cancel_check(cancel)?;
        }
        let _ = ip;
    }
    ips.sort();
    ips.dedup();
    if ips.len() > MAX_RESOLVED_IPS {
        return Err(NetworkNormalizeError::TooManyIps);
    }
    Ok(ips)
}

fn classify_target(host: &CanonicalNetHost, ips: &[IpAddr]) -> BTreeSet<IpClass> {
    let mut classes = BTreeSet::new();
    for ip in ips {
        classes.extend(classify_ip(*ip));
    }
    if let CanonicalNetHost::Dns(name) = host {
        if is_metadata_hostname(name.as_str()) {
            classes.insert(IpClass::MetadataLike);
        }
        if is_loopback_hostname(name.as_str()) {
            classes.insert(IpClass::Loopback);
        }
    }
    classes
}

fn classify_ip(ip: IpAddr) -> BTreeSet<IpClass> {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                classify_v4(v4)
            } else {
                classify_v6(v6)
            }
        }
    }
}

fn classify_v4(ip: Ipv4Addr) -> BTreeSet<IpClass> {
    let mut classes = BTreeSet::new();
    if is_metadata_v4(ip) {
        classes.insert(IpClass::MetadataLike);
    }
    if ip.is_loopback() {
        classes.insert(IpClass::Loopback);
    } else if ip.is_unspecified() {
        classes.insert(IpClass::Unspecified);
    } else if ip.is_link_local() {
        classes.insert(IpClass::LinkLocal);
    } else if ip.is_private() || is_shared_v4(ip) {
        classes.insert(IpClass::Private);
    } else if classes.is_empty() {
        classes.insert(IpClass::Public);
    }
    if classes.is_empty() {
        classes.insert(IpClass::Public);
    }
    classes
}

fn classify_v6(ip: Ipv6Addr) -> BTreeSet<IpClass> {
    let mut classes = BTreeSet::new();
    if is_metadata_v6(ip) {
        classes.insert(IpClass::MetadataLike);
    }
    if ip.is_loopback() {
        classes.insert(IpClass::Loopback);
    } else if ip.is_unspecified() {
        classes.insert(IpClass::Unspecified);
    } else if ip.is_unicast_link_local() {
        classes.insert(IpClass::LinkLocal);
    } else if ip.is_unique_local() {
        classes.insert(IpClass::Private);
    } else if classes.is_empty() {
        classes.insert(IpClass::Public);
    }
    if classes.is_empty() {
        classes.insert(IpClass::Public);
    }
    classes
}

fn is_metadata_v4(ip: Ipv4Addr) -> bool {
    METADATA_V4.contains(&ip.octets())
}

/// RFC 6598 shared address space `100.64.0.0/10` (CGNAT).
fn is_shared_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0xc0) == 64
}

fn is_metadata_v6(ip: Ipv6Addr) -> bool {
    ip == METADATA_V6
}

fn is_metadata_hostname(host: &str) -> bool {
    matches!(
        host,
        "metadata" | "metadata.google.internal" | "metadata.internal" | "instance-data"
    )
}

fn is_loopback_hostname(host: &str) -> bool {
    matches!(
        host,
        "localhost" | "localhost.localdomain" | "ip6-localhost" | "ip6-loopback"
    )
}

fn validate_url_text(url: &str) -> Result<(), NetworkNormalizeError> {
    if url.is_empty() {
        return Err(NetworkNormalizeError::EmptyUrl);
    }
    if url.len() > MAX_URL_BYTES {
        return Err(NetworkNormalizeError::TooLong);
    }
    if url.contains('\0') {
        return Err(NetworkNormalizeError::Nul);
    }
    if url.chars().any(char::is_control) {
        return Err(NetworkNormalizeError::Control);
    }
    if url.chars().any(char::is_whitespace) {
        return Err(NetworkNormalizeError::InvalidUrl);
    }
    Ok(())
}

fn cancel_check(cancel: &CancellationToken) -> Result<(), NetworkNormalizeError> {
    if cancel.is_cancelled() {
        Err(NetworkNormalizeError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_scheme(err: CapabilityError) -> NetworkNormalizeError {
    match err {
        CapabilityError::UnknownScheme => NetworkNormalizeError::UnknownScheme,
        _ => NetworkNormalizeError::InvalidUrl,
    }
}

fn map_host(err: CapabilityError) -> NetworkNormalizeError {
    match err {
        CapabilityError::Empty => NetworkNormalizeError::InvalidHost,
        CapabilityError::TooLong => NetworkNormalizeError::TooLong,
        CapabilityError::Nul => NetworkNormalizeError::Nul,
        CapabilityError::Control => NetworkNormalizeError::Control,
        CapabilityError::UnknownScheme => NetworkNormalizeError::UnknownScheme,
        CapabilityError::InvalidPort => NetworkNormalizeError::InvalidPort,
        _ => NetworkNormalizeError::InvalidHost,
    }
}

impl fmt::Display for NetworkNormalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "network normalization cancelled",
            Self::EmptyUrl => "network url is empty",
            Self::TooLong => "network field exceeds bound",
            Self::Nul => "network field contains NUL",
            Self::Control => "network field contains a control character",
            Self::UnknownScheme => "unknown network scheme",
            Self::InvalidUrl => "network url is invalid",
            Self::InvalidHost => "invalid hostname",
            Self::InvalidPort => "invalid port",
            Self::Userinfo => "network url must not contain userinfo",
            Self::UnresolvedHost => "hostname could not be resolved",
            Self::TooManyIps => "resolved address list exceeds bound",
            Self::TooManyRedirects => "redirect hop limit exceeded",
        })
    }
}

impl Error for NetworkNormalizeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct MapResolver {
        records: BTreeMap<String, Vec<IpAddr>>,
    }

    impl MapResolver {
        fn new() -> Self {
            Self {
                records: BTreeMap::new(),
            }
        }

        fn with(mut self, host: &str, ips: &[&str]) -> Self {
            let addrs = ips
                .iter()
                .map(|ip| ip.parse().expect("fixture ip"))
                .collect();
            self.records.insert(host.to_ascii_lowercase(), addrs);
            self
        }
    }

    impl NetworkResolver for MapResolver {
        fn resolve(&self, host: &Hostname) -> Result<Vec<IpAddr>, NetworkNormalizeError> {
            self.records
                .get(host.as_str())
                .cloned()
                .ok_or(NetworkNormalizeError::UnresolvedHost)
        }
    }

    fn fixture() -> MapResolver {
        MapResolver::new()
            .with("example.com", &["93.184.216.34"])
            .with(
                "dual.example.com",
                &["93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946"],
            )
            .with("rebind.example.com", &["93.184.216.34", "169.254.169.254"])
            .with("localhost", &["127.0.0.1", "::1"])
            .with("metadata.google.internal", &["169.254.169.254"])
            .with("metadata", &["169.254.169.254"])
            .with("private.internal", &["10.0.0.8"])
            .with("link.internal", &["169.254.1.1"])
    }

    fn normalize(url: &str) -> CanonicalNetworkTarget {
        normalize_network(
            &NetworkIntent::connect(url),
            &fixture(),
            &CancellationToken::new(),
        )
        .expect("normalize")
    }

    fn reject(url: &str) -> NetworkNormalizeError {
        normalize_network(
            &NetworkIntent::connect(url),
            &fixture(),
            &CancellationToken::new(),
        )
        .expect_err("reject")
    }

    fn classes_of(url: &str) -> BTreeSet<IpClass> {
        normalize(url).ip_classes().clone()
    }

    #[test]
    fn hostname_case_and_default_port_normalize_consistently() {
        let a = normalize("HTTPS://Example.COM");
        let b = normalize("https://example.com:443");
        let c = normalize("https://example.com.");
        let d = normalize("https://example.com/path?q=1#frag");
        assert_eq!(a.scheme(), NetworkScheme::Https);
        assert_eq!(a.port(), 443);
        assert_eq!(a.host().as_canonical_str(), "example.com");
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a, d);
        assert_eq!(a.policy_bytes(), b.policy_bytes());
        let http_a = normalize("HTTP://EXAMPLE.COM");
        let http_b = normalize("http://example.com:80");
        assert_eq!(http_a.scheme(), NetworkScheme::Http);
        assert_eq!(http_a.port(), 80);
        assert_eq!(http_a, http_b);
        let ws = normalize("WS://Example.COM");
        assert_eq!(ws.scheme(), NetworkScheme::Ws);
        assert_eq!(ws.port(), 80);
        assert_ne!(ws, http_a);
        let wss = normalize("wss://example.com");
        assert_eq!(wss.scheme(), NetworkScheme::Wss);
        assert_eq!(wss.port(), 443);
        assert_ne!(wss, a);
    }

    #[test]
    fn classifies_loopback_private_link_local_and_metadata() {
        let loopback_v4 = classes_of("http://127.0.0.1");
        assert!(loopback_v4.contains(&IpClass::Loopback));
        assert!(!loopback_v4.contains(&IpClass::Public));

        let loopback_v6 = classes_of("http://[::1]");
        assert!(loopback_v6.contains(&IpClass::Loopback));

        let mapped = classes_of("http://[::ffff:127.0.0.1]");
        assert!(mapped.contains(&IpClass::Loopback));

        let localhost = classes_of("http://localhost");
        assert!(localhost.contains(&IpClass::Loopback));

        let private = classes_of("http://10.1.2.3");
        assert!(private.contains(&IpClass::Private));
        assert_eq!(classes_of("http://192.168.1.1"), private_only());
        assert!(classes_of("http://172.16.5.5").contains(&IpClass::Private));
        assert!(classes_of("http://100.64.1.1").contains(&IpClass::Private));
        assert!(classes_of("http://[fd12:3456:789a::1]").contains(&IpClass::Private));
        assert!(classes_of("http://private.internal").contains(&IpClass::Private));

        let link = classes_of("http://169.254.1.1");
        assert!(link.contains(&IpClass::LinkLocal));
        assert!(classes_of("http://[fe80::1]").contains(&IpClass::LinkLocal));
        assert!(classes_of("http://link.internal").contains(&IpClass::LinkLocal));

        let imds = classes_of("http://169.254.169.254");
        assert!(imds.contains(&IpClass::MetadataLike));
        assert!(imds.contains(&IpClass::LinkLocal));
        assert!(classes_of("http://169.254.170.2").contains(&IpClass::MetadataLike));
        assert!(classes_of("http://169.254.170.23").contains(&IpClass::MetadataLike));
        assert!(classes_of("http://100.100.100.200").contains(&IpClass::MetadataLike));
        assert!(classes_of("http://[fd00:ec2::254]").contains(&IpClass::MetadataLike));
        assert!(classes_of("http://metadata.google.internal").contains(&IpClass::MetadataLike));
        assert!(classes_of("http://metadata").contains(&IpClass::MetadataLike));

        let public = classes_of("https://example.com");
        assert_eq!(public, BTreeSet::from([IpClass::Public]));
    }

    fn private_only() -> BTreeSet<IpClass> {
        BTreeSet::from([IpClass::Private])
    }

    #[test]
    fn dns_rebinding_to_metadata_is_classified_not_public_only() {
        // T-005: a public A record plus IMDS must surface MetadataLike.
        let target = normalize("https://rebind.example.com");
        assert!(target.ip_classes().contains(&IpClass::MetadataLike));
        assert!(target.ip_classes().contains(&IpClass::LinkLocal));
        assert!(target.ip_classes().contains(&IpClass::Public));
        assert_eq!(target.resolved_ips().len(), 2);
    }

    #[test]
    fn userinfo_and_ambiguous_ip_encodings_fail_closed() {
        // T-005: credentials and dword/hex/octal IPs must not mint a target.
        let samples = [
            "https://user:pass@example.com",
            "https://allowed.com@169.254.169.254/",
            "http://2130706433/",
            "http://0x7f000001/",
            "http://0177.0.0.1/",
            "http://127.1/",
        ];
        for url in samples {
            let err = reject(url);
            assert!(
                matches!(
                    err,
                    NetworkNormalizeError::Userinfo | NetworkNormalizeError::InvalidHost
                ),
                "{url} => {err:?}"
            );
            let msg = err.to_string();
            assert!(!msg.contains("pass"));
            assert!(!msg.contains("allowed.com"));
            assert!(!msg.contains("169.254"));
            assert!(!msg.contains("2130706433"));
            assert!(!msg.contains("0x7f"));
        }
    }

    #[test]
    fn unknown_scheme_and_relative_locations_fail_closed() {
        assert_eq!(
            reject("file://example.com"),
            NetworkNormalizeError::UnknownScheme
        );
        assert_eq!(
            reject("ftp://example.com"),
            NetworkNormalizeError::UnknownScheme
        );
        assert_eq!(
            reject("javascript:alert(1)"),
            NetworkNormalizeError::InvalidUrl
        );
        assert_eq!(reject("//example.com"), NetworkNormalizeError::InvalidUrl);
        assert_eq!(reject("/relative"), NetworkNormalizeError::InvalidUrl);
        assert!(
            !NetworkNormalizeError::UnknownScheme
                .to_string()
                .contains("file")
        );
    }

    #[test]
    fn redirect_revalidation_metadata_is_bound_and_capped() {
        let first = normalize("https://example.com/start");
        assert_eq!(first.redirect().hop(), 0);
        assert!(first.redirect().previous().is_none());
        assert!(first.redirect().requires_revalidation());

        let second = normalize_network(
            &NetworkIntent::redirect(&first, "https://EXAMPLE.COM/next").expect("redirect"),
            &fixture(),
            &CancellationToken::new(),
        )
        .expect("second hop");
        assert_eq!(second.redirect().hop(), 1);
        assert!(second.redirect().requires_revalidation());
        let prev = second.redirect().previous().expect("previous");
        assert_eq!(prev.scheme(), NetworkScheme::Https);
        assert_eq!(prev.host().as_canonical_str(), "example.com");
        assert_eq!(prev.port(), 443);
        assert_ne!(first.policy_bytes(), second.policy_bytes());

        let mut current = second;
        for _ in 2..=MAX_REDIRECT_HOPS {
            current = normalize_network(
                &NetworkIntent::redirect(&current, "https://example.com/hop").expect("hop"),
                &fixture(),
                &CancellationToken::new(),
            )
            .expect("in-bound hop");
        }
        assert_eq!(current.redirect().hop(), MAX_REDIRECT_HOPS);
        let over = NetworkIntent::redirect(&current, "https://example.com/over");
        assert_eq!(over.unwrap_err(), NetworkNormalizeError::TooManyRedirects);

        let forbidden = normalize_network(
            &NetworkIntent::redirect(&first, "http://169.254.169.254/latest/meta-data/")
                .expect("imds redirect"),
            &fixture(),
            &CancellationToken::new(),
        )
        .expect("classified redirect");
        assert!(forbidden.ip_classes().contains(&IpClass::MetadataLike));
        assert_eq!(forbidden.scheme(), NetworkScheme::Http);
        assert_eq!(forbidden.port(), 80);
    }

    #[test]
    fn unresolved_and_too_many_ips_fail_closed() {
        assert_eq!(
            reject("https://missing.example.com"),
            NetworkNormalizeError::UnresolvedHost
        );
        let mut ips = Vec::new();
        for i in 0..=MAX_RESOLVED_IPS {
            ips.push(format!("1.2.3.{i}"));
        }
        let refs: Vec<&str> = ips.iter().map(String::as_str).collect();
        let resolver = MapResolver::new().with("many.example.com", &refs);
        let err = normalize_network(
            &NetworkIntent::connect("https://many.example.com"),
            &resolver,
            &CancellationToken::new(),
        )
        .expect_err("too many");
        assert_eq!(err, NetworkNormalizeError::TooManyIps);
        assert!(!err.to_string().contains("many.example.com"));
    }

    #[test]
    fn bounds_nul_control_and_cancel_fail_closed() {
        assert_eq!(reject(""), NetworkNormalizeError::EmptyUrl);
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert_eq!(reject(&long), NetworkNormalizeError::TooLong);
        assert_eq!(reject("https://example.com/\0"), NetworkNormalizeError::Nul);
        assert_eq!(
            reject("https://example.com/\n"),
            NetworkNormalizeError::Control
        );
        assert_eq!(
            reject("https://example.com:0"),
            NetworkNormalizeError::InvalidPort
        );
        assert_eq!(
            reject("https://example.com:080"),
            NetworkNormalizeError::InvalidPort
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = normalize_network(
            &NetworkIntent::connect("https://example.com"),
            &fixture(),
            &cancel,
        )
        .expect_err("cancelled");
        assert_eq!(err, NetworkNormalizeError::Cancelled);
    }

    #[test]
    fn policy_bytes_are_stable_and_omit_userinfo() {
        let target = normalize("https://example.com:443/secret-path");
        let expected = b"rapidlm.canonical_network.v1\0https\0example.com\0443\0ips\093.184.216.34\0classes\0public\0redirect\00\0";
        assert_eq!(target.policy_bytes(), expected);
        let text = String::from_utf8(target.policy_bytes()).expect("utf8");
        assert!(!text.contains("secret-path"));
        assert!(!text.contains("user"));
        assert!(!text.contains('@'));
    }

    #[test]
    fn literal_ips_do_not_consult_resolver() {
        struct PanicResolver;
        impl NetworkResolver for PanicResolver {
            fn resolve(&self, _: &Hostname) -> Result<Vec<IpAddr>, NetworkNormalizeError> {
                panic!("literal ip must not resolve");
            }
        }
        let target = normalize_network(
            &NetworkIntent::connect("http://127.0.0.1:8080"),
            &PanicResolver,
            &CancellationToken::new(),
        )
        .expect("literal");
        assert_eq!(target.host().ip(), Some(IpAddr::from(Ipv4Addr::LOCALHOST)));
        assert_eq!(target.port(), 8080);
        assert_eq!(target.resolved_ips(), &[IpAddr::from(Ipv4Addr::LOCALHOST)]);
        assert!(target.ip_classes().contains(&IpClass::Loopback));
    }
}
