//! `web_fetch` internals: SSRF classification, HTML-to-text extraction, and
//! byte-bounded page fetching on top of the llm-router transport primitive.
//!
//! The SSRF guard is a pure resolve-and-classify pass: every resolved address
//! for the URL's host must be public unless the host is on the settings
//! allowlist (loopback/private/link-local are refused by default, matching the
//! reference CLIs' local-fetch protections).

use std::net::{IpAddr, ToSocketAddrs};

use llm_router::http_get;

/// Hard byte cap for one fetched page.
pub const MAX_FETCH_BYTES: usize = 100 * 1024;
/// Marker appended when a page was cut by the byte cap.
pub const FETCH_TRUNCATION_MARKER: &str = "\n[truncated by byte cap]";

/// Why a fetch refused to run. Display is model-safe (static text + host).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchRefusal {
    /// Only http/https are supported.
    UnsupportedScheme(String),
    /// The host resolves only to private/loopback/link-local addresses.
    PrivateTargetBlocked { host: String },
    /// The URL did not parse as a fetchable host+port.
    InvalidUrl(String),
}

impl FetchRefusal {
    /// Model-visible one-liner.
    pub fn detail(&self) -> String {
        match self {
            Self::UnsupportedScheme(scheme) => {
                format!("unsupported URL scheme {scheme:?} (use http or https)")
            }
            Self::PrivateTargetBlocked { host } => format!(
                "refused: {host} resolves to a private/loopback address and is not on \
                 the fetch allowlist"
            ),
            Self::InvalidUrl(url) => format!("invalid URL: {url:?}"),
        }
    }
}

/// Scheme + host + port split of a fetchable URL.
struct SplitUrl<'a> {
    scheme: String,
    host: &'a str,
    port: u16,
}

fn split_url(url: &str) -> Option<SplitUrl<'_>> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() {
        return None;
    }
    // Strip userinfo if present (never forward credentials).
    let host_port = authority.rsplit('@').next()?;
    // Only treat a trailing `:<digits>` as a port — IPv6 literals contain
    // colons of their own.
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let port: u16 = port.parse().unwrap_or(0);
            (host, port)
        }
        _ => (host_port, if scheme == "https" { 443 } else { 80 }),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    Some(SplitUrl {
        scheme,
        host,
        port,
    })
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Resolve the URL's host and decide whether the fetch may proceed. Every
/// resolved address must be public unless `host` is on the allowlist (exact,
/// case-insensitive match).
pub fn classify_fetch(url: &str, allowlist: &[String]) -> Result<(), FetchRefusal> {
    let split = split_url(url).ok_or_else(|| FetchRefusal::InvalidUrl(url.to_owned()))?;
    if split.scheme != "http" && split.scheme != "https" {
        return Err(FetchRefusal::UnsupportedScheme(split.scheme.clone()));
    }
    let allowlisted = allowlist
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(split.host));
    if allowlisted {
        return Ok(());
    }
    let addrs = (split.host, split.port)
        .to_socket_addrs()
        .map_err(|_| FetchRefusal::InvalidUrl(url.to_owned()))?;
    let mut saw_any = false;
    for addr in addrs {
        saw_any = true;
        if is_private_ip(addr.ip()) {
            return Err(FetchRefusal::PrivateTargetBlocked {
                host: split.host.to_owned(),
            });
        }
    }
    if !saw_any {
        return Err(FetchRefusal::InvalidUrl(url.to_owned()));
    }
    Ok(())
}

fn strip_block(source: &str, block: &str) -> String {
    let open = format!("<{block}");
    let close = format!("</{block}>");
    let lower = source.to_ascii_lowercase();
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    loop {
        let Some(start_rel) = lower[cursor..].find(&open) else {
            out.push_str(&source[cursor..]);
            break;
        };
        let start = cursor + start_rel;
        out.push_str(&source[cursor..start]);
        match lower[start..].find(&close) {
            Some(end_rel) => cursor = start + end_rel + close.len(),
            None => {
                cursor = source.len();
                break;
            }
        }
    }
    out
}

fn decode_entities(input: &str) -> String {
    const ENTITIES: [(&str, &str); 6] = [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&nbsp;", " "),
    ];
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    'outer: while !rest.is_empty() {
        if rest.starts_with('&') {
            for (entity, replacement) in ENTITIES {
                if let Some(tail) = rest.strip_prefix(entity) {
                    out.push_str(replacement);
                    rest = tail;
                    continue 'outer;
                }
            }
        }
        // Advance one full character (multi-byte safe).
        let mut boundary = 1;
        while !rest.is_char_boundary(boundary) && boundary < rest.len() {
            boundary += 1;
        }
        out.push_str(&rest[..boundary]);
        rest = &rest[boundary..];
    }
    out
}

/// Strip HTML down to readable text: script/style/head blocks removed, tags
/// dropped, common entities decoded, whitespace collapsed. Pure function.
pub fn html_to_text(html: &str) -> String {
    let without_blocks = strip_block(&strip_block(&strip_block(html, "script"), "style"), "head");
    // Pass 1: tags become boundary spaces; text passes through.
    let mut with_boundaries = String::with_capacity(without_blocks.len());
    let mut in_tag = false;
    for ch in without_blocks.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                with_boundaries.push(' ');
            }
            c if !in_tag => with_boundaries.push(c),
            _ => {}
        }
    }
    // Pass 2: decode the common entities.
    let decoded = decode_entities(&with_boundaries);
    // Pass 3: collapse whitespace runs.
    let mut text = String::with_capacity(decoded.len());
    let mut pending_space = false;
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !text.is_empty() {
                text.push(' ');
            }
            pending_space = false;
            text.push(ch);
        }
    }
    text.trim().to_owned()
}


/// Fetch `url` and return bounded readable text. `allowlist` admits
/// private/loopback hosts that the SSRF guard would otherwise refuse.
pub fn fetch_page(
    url: &str,
    allowlist: &[String],
    max_bytes: usize,
) -> Result<String, FetchRefusal> {
    classify_fetch(url, allowlist)?;
    let body = http_get(
        url,
        // The caller already ran the SSRF classification; private hosts that
        // survived it are allowlisted.
        true,
        max_bytes,
        std::time::Duration::from_secs(30),
        &llm_router::provider::CancellationToken::new(),
    )
    .map_err(|_| FetchRefusal::InvalidUrl(url.to_owned()))?;
    let text = String::from_utf8_lossy(&body);
    let mut readable = if text.trim_start().starts_with('<') {
        html_to_text(&text)
    } else {
        text.trim().to_owned()
    };
    if body.len() >= max_bytes {
        readable.push_str(FETCH_TRUNCATION_MARKER);
    }
    Ok(readable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_blocks_tags_and_entities() {
        let html = "<html><head><title>t</title><style>.x{}</style></head>\
                    <body><script>var a = 1;</script><h1>Hello</h1>\
                    <p>A &amp; B &lt;tag&gt; &quot;q&quot; &#39;s&#39;&nbsp;end</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("A & B <tag> \"q\" 's' end"), "{text}");
        assert!(!text.contains("var a"), "{text}");
        assert!(!text.contains(".x{}"), "{text}");
        assert!(!text.contains("<title>"), "{text}");
        assert!(!text.contains("  "), "{text}");
    }

    #[test]
    fn classify_refuses_private_and_loopback_without_allowlist() {
        let refusal = classify_fetch("http://127.0.0.1:9/x", &[]).unwrap_err();
        assert!(matches!(refusal, FetchRefusal::PrivateTargetBlocked { .. }));
        let refusal = classify_fetch("http://192.168.1.10/x", &[]).unwrap_err();
        let refusal = classify_fetch("http://192.168.1.10/x", &[]).unwrap_err();
        assert!(matches!(refusal, FetchRefusal::PrivateTargetBlocked { .. }));
        let refusal = classify_fetch("http://[::1]/x", &[]).unwrap_err();
        assert!(matches!(refusal, FetchRefusal::PrivateTargetBlocked { .. }));
    }

    #[test]
    fn allowlist_admits_listed_hosts_only() {
        let allowlist = vec!["127.0.0.1".to_owned()];
        assert!(classify_fetch("http://127.0.0.1:9/x", &allowlist).is_ok());
        let refusal = classify_fetch("http://10.0.0.5/x", &allowlist).unwrap_err();
        assert!(matches!(refusal, FetchRefusal::PrivateTargetBlocked { .. }));
        // Hostname resolving to a public address is fine.
        assert!(classify_fetch("https://example.com/", &[]).is_ok());
    }

    #[test]
    fn invalid_urls_and_schemes_are_typed_refusals() {
        assert!(matches!(
            classify_fetch("ftp://example.com/file", &[]),
            Err(FetchRefusal::UnsupportedScheme(_))
        ));
        assert!(classify_fetch("http://", &[]).is_err());
        assert!(classify_fetch("not a url", &[]).is_err());
    }

    #[test]
    fn split_url_handles_ports_and_userinfo() {
        let split = split_url("http://user:pass@example.com:8080/path?q=1").expect("split");
        assert_eq!(split.host, "example.com");
        assert_eq!(split.port, 8080);
        let split = split_url("https://example.com").expect("split");
        assert_eq!(split.port, 443);
    }
}
