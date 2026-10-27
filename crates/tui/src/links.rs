//! Clickable links: OSC 8 hyperlinks around the URLs transcript rows
//! already carry.
//!
//! The renderer paints rows that may contain URLs (a model answer citing a
//! doc, a review finding, a build log). Terminals that speak OSC 8 turn a
//! wrapped span into a real link — openable with the mouse or the
//! keyboard — and terminals that do not render exactly the text they
//! always did, because the escape's payload is the URL's own characters.
//!
//! The transformation is a pure function over one painted row: it must
//! not disturb the row's other ANSI styling, so an escape sequence inside
//! the scan simply suspends URL detection until the sequence ends — a URL
//! an escape interrupts was never one URL to begin with.

/// The OSC 8 hyperlink wrappers: begin (`ESC ] 8 ; ; URL ST`), end
/// (`ESC ] 8 ; ; ST`). The empty URL ends the hyperlink.
pub const OSC8_END: &str = "\x1b]8;;\x1b\\";

/// Wrap every complete URL in `row` in an OSC 8 hyperlink. URLs start at
/// `http://` or `https://` (case-insensitive) and run while the
/// characters are URL-shaped; a trailing sentence punctuation (`,.;:!?)`)
/// stays outside the link, the way a reader expects. ANSI escape
/// sequences elsewhere in the row pass through untouched and suspend the
/// URL scan while they run.
pub fn linkify(row: &str) -> String {
    let bytes = row.as_bytes();
    let mut out = String::with_capacity(row.len());
    let mut at = 0usize;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == 0x1b {
            // An escape sequence: copy it whole (CSI … a final byte in
            // 0x40..=0x7e, or OSC … ST), then continue scanning.
            let (end, matched) = escape_end(bytes, at);
            out.push_str(&row[at..end]);
            at = end;
            if !matched {
                // A malformed tail: copy the rest and stop.
                break;
            }
            continue;
        }
        if let Some(len) = url_len(bytes, at) {
            let start = at;
            let url = &row[start..start + len];
            // Trailing sentence punctuation is not part of the link.
            let trimmed = url.trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
            out.push_str("\x1b]8;;");
            out.push_str(trimmed);
            out.push_str("\x1b\\");
            out.push_str(trimmed);
            out.push_str(OSC8_END);
            out.push_str(&url[trimmed.len()..]);
            at = start + len;
            continue;
        }
        // One UTF-8 character at a time: never split a code point.
        let char_len = utf8_len(byte);
        let end = (at + char_len).min(bytes.len());
        out.push_str(&row[at..end]);
        at = end;
    }
    out
}

/// The length of the URL starting at `at`, if the prefix is `http://` or
/// `https://`.
fn url_len(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = &bytes[at..];
    let https = rest.len() >= 8 && rest[..8].eq_ignore_ascii_case(b"https://");
    let http = !https && rest.len() >= 7 && rest[..7].eq_ignore_ascii_case(b"http://");
    if !https && !http {
        return None;
    }
    let mut end = if https { 8 } else { 7 };
    while end < rest.len() && is_url_byte(rest[end]) {
        end += 1;
    }
    Some(end)
}

fn is_url_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'/' | b':'
                | b'.'
                | b'?'
                | b'#'
                | b'%'
                | b'&'
                | b'='
                | b'_'
                | b'-'
                | b'~'
                | b'+'
                | b'@'
                | b'['
                | b']'
        )
}

/// The end offset of the escape sequence starting at `at`, and whether it
/// was recognized (a malformed escape yields `(len, false)`).
fn escape_end(bytes: &[u8], at: usize) -> (usize, bool) {
    debug_assert_eq!(bytes[at], 0x1b);
    if at + 1 >= bytes.len() {
        return (bytes.len(), false);
    }
    match bytes[at + 1] {
        b'[' => {
            // CSI: parameters/intermediates until a final byte 0x40..=0x7e.
            let mut i = at + 2;
            while i < bytes.len() {
                let byte = bytes[i];
                if (0x40..=0x7e).contains(&byte) {
                    return (i + 1, true);
                }
                i += 1;
            }
            (bytes.len(), false)
        }
        b']' => {
            // OSC: until BEL or ST (ESC \).
            let mut i = at + 2;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return (i + 1, true);
                }
                if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                    return (i + 2, true);
                }
                i += 1;
            }
            (bytes.len(), false)
        }
        // A two-byte escape (colors, charset): the next byte ends it.
        _ => (at + 2, true),
    }
}

fn utf8_len(byte: u8) -> usize {
    match byte {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_urls_become_links_and_text_around_them_survives() {
        let linked = linkify("see https://docs.rs/serde for the trait");
        assert!(
            linked.starts_with("see \x1b]8;;https://docs.rs/serde\x1b\\"),
            "{linked:?}"
        );
        assert!(
            linked.contains("https://docs.rs/serde\x1b]8;;\x1b\\ for the trait"),
            "{linked:?}"
        );
        // The plain URL is still readable in the output: what a terminal
        // without OSC 8 shows is the original text.
        assert!(linked.contains("https://docs.rs/serde"));

        // http, capital scheme, ports, anchors, queries.
        for url in [
            "http://127.0.0.1:8787/v1?a=1&b=2#frag",
            "HTTPS://example.com/x",
        ] {
            let linked = linkify(&format!("go {url} now"));
            assert!(linked.contains("\x1b]8;;"), "{url}");
            assert!(linked.contains(url), "the text survives: {linked:?}");
        }
    }

    #[test]
    fn sentence_punctuation_stays_outside_the_link() {
        let linked = linkify("read https://docs.rs/serde. Then this.");
        assert!(
            linked.contains("https://docs.rs/serde\x1b]8;;\x1b\\."),
            "the period is plain text: {linked:?}"
        );
    }

    #[test]
    fn ansi_styling_passes_through_and_suspends_the_scan() {
        // A styled row: color on, a URL, color off.
        let row = "\x1b[32msee https://example.com/x\x1b[0m ok";
        let linked = linkify(row);
        assert!(linked.starts_with("\x1b[32msee "), "{linked:?}");
        assert!(linked.contains("]8;;"), "{linked:?}");
        assert!(linked.ends_with("\x1b[0m ok"), "{linked:?}");

        // An escape inside what would be a URL ends it: the visible half
        // is a URL in its own right and links as itself; the styled tail
        // after the escape passes through untouched.
        let broken = "https://exam\x1b[31mple.com";
        let linked = linkify(broken);
        assert!(
            linked.starts_with("\x1b]8;;https://exam"),
            "the pre-escape half links: {linked:?}"
        );
        assert!(
            linked.ends_with("\x1b[31mple.com"),
            "the styled tail is untouched: {linked:?}"
        );

        // Malformed escape tails copy whole rather than loop.
        assert_eq!(linkify("text \x1b"), "text \x1b");
    }

    #[test]
    fn rows_without_urls_are_returned_untouched() {
        for row in ["", "plain text", "见 https 的文档", "no scheme here://x"] {
            assert_eq!(linkify(row), row, "{row:?}");
        }
    }
}
