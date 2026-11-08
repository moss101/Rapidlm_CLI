//! Image previews: emit what the terminal can actually show.
//!
//! Two dialects, both real: the Kitty graphics protocol (APC `GC`…
//! chunks carrying base64 PNG) and Sixel (`DCS q`… with a sixel-encoded
//! band). Both are pure functions over the image bytes — the caller
//! detects the terminal once and picks; a terminal that speaks neither
//! gets the caller's typed fallback line, never escape garbage.
//!
//! The encoders are deliberately narrow: PNG for Kitty (the protocol's
//! native format, `f=100` — a direct upload of the file's own bytes),
//! and a one-bit-per-pixel Sixel band for Sixel (enough to prove the
//! dialect without pretending to a color engine). A preview, not an
//! image viewer: large inputs are refused typed, not truncated into a
//! half picture.

/// Most bytes accepted (Kitty's practical chunk budget: a preview, not a
/// viewer — past this the caller shows the path instead).
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
/// Kitty payload chunk size (base64 of this many bytes per APC).
const KITTY_CHUNK: usize = 4096;

/// Which image dialect the terminal speaks, from the environment a
/// terminal sets for its children.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageDialect {
    /// The Kitty graphics protocol: `TERM` starting `xterm-kitty`, or
    /// `TERM_PROGRAM` naming Ghostty (it speaks the same APC).
    Kitty,
    /// Sixel: `TERM` containing `sixel` (mintty, WezTerm, foot, …).
    Sixel,
}

/// Detect the dialect from `TERM`/`TERM_PROGRAM` values (pure; callers
/// pass what they read). `None` when neither dialect is named — the
/// caller's fallback line is the whole preview then.
pub fn detect(term: &str, term_program: &str) -> Option<ImageDialect> {
    let term = term.to_ascii_lowercase();
    if term.starts_with("xterm-kitty") {
        return Some(ImageDialect::Kitty);
    }
    if term_program.eq_ignore_ascii_case("ghostty") {
        return Some(ImageDialect::Kitty);
    }
    if term.contains("sixel") {
        return Some(ImageDialect::Sixel);
    }
    None
}

/// The Kitty graphics escape for `png` bytes: base64 payload chunked into
/// APC `GC` controls (`m=1` on every chunk but the last, `m=0` ending),
/// the PNG passed through as the protocol's own `f=100` format. `None`
/// when the input is empty or over the preview bound — the caller shows
/// the typed fallback instead of a broken half image.
pub fn kitty_escape(png: &[u8]) -> Option<String> {
    if png.is_empty() || png.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let mut out = String::new();
    for chunk in png.chunks(KITTY_CHUNK) {
        let last = chunk.len() < KITTY_CHUNK;
        out.push_str("\x1b_Gf=100,m=");
        out.push(if last { '0' } else { '1' });
        out.push(';');
        out.push_str(&base64(chunk));
        out.push_str("\x1b\\");
    }
    Some(out)
}

/// A Sixel band for `rgba` bytes (`width * height * 4`, row-major): six
/// rows of pixels per sixel band, one bit per pixel — enough to show the
/// picture's shape, which is what a preview in a text grid owes. DCS `q`
/// … ST, raster attributes set to the image's own size. `None` when the
/// input is empty, not a whole pixel count, or over the bound.
pub fn sixel_escape(rgba: &[u8], width: u32, height: u32) -> Option<String> {
    if rgba.is_empty() || rgba.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let pixels = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?;
    if rgba.len() != pixels * 4 {
        return None;
    }
    let mut out = String::new();
    out.push_str("\x1bPq");
    out.push_str(&format!("\"1;1;{width};{height}"));
    let luminance = |x: usize, y: usize| -> u8 {
        let at = (y * width as usize + x) * 4;
        // Rec. 601 luma of the pixel, alpha-weighted: transparent is
        // white (paper), the band's background.
        let (r, g, b, a) = (
            rgba[at] as u32,
            rgba[at + 1] as u32,
            rgba[at + 2] as u32,
            rgba[at + 3] as u32,
        );
        let y601 = (299 * r + 587 * g + 114 * b) / 1000;
        ((y601 * a + 255 * (255 - a)) / 255) as u8
    };
    let mut y = 0usize;
    while y < height as usize {
        let band = (height as usize - y).min(6);
        let mut column = String::new();
        for x in 0..width as usize {
            // One sixel character: six vertical pixels, bit 0 the top.
            let mut bits = 0u8;
            for row in 0..band {
                if luminance(x, y + row) < 128 {
                    bits |= 1 << row;
                }
            }
            column.push(char::from_u32(0x3f + bits as u32)?);
        }
        out.push('#');
        out.push_str(&column);
        out.push('-');
        y += band;
    }
    out.push_str("\x1b\\");
    Some(out)
}

/// Standard base64 (alphabet `A–Z a–z 0–9 + /`, `=` padding) — the Kitty
/// protocol's own payload alphabet.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let word = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(word >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(word >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(word >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[word as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialects_are_detected_from_the_environment_and_nothing_else() {
        assert_eq!(detect("xterm-kitty", ""), Some(ImageDialect::Kitty));
        assert_eq!(detect("", "GhostTy"), Some(ImageDialect::Kitty));
        assert_eq!(
            detect("xterm-256color-sixel", ""),
            Some(ImageDialect::Sixel)
        );
        assert_eq!(detect("xterm-256color", ""), None);
        assert_eq!(detect("", "Apple_Terminal"), None);
        // A terminal that merely mentions kitty in passing is not it.
        assert_eq!(detect("screen", "kitty-in-name-only"), None);
    }

    #[test]
    fn kitty_escapes_chunk_the_payload_with_the_last_chunk_marked() {
        let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4, 5];
        let escape = kitty_escape(&png).expect("escape");
        // One 9-byte payload fits a single chunk: m=0 (the last).
        assert!(escape.starts_with("\x1b_Gf=100,m=0;"), "{escape:?}");
        // A payload past one chunk carries m=1 chunks before the m=0 end.
        let big = [0x89u8, b'P', b'N', b'G'].repeat(KITTY_CHUNK / 4 + 1);
        let big_escape = kitty_escape(&big).expect("big escape");
        assert!(big_escape.contains("m=1;"), "{big_escape:?}");
        let last_apc = big_escape.rsplit("\x1b_G").next().unwrap_or_default();
        assert!(
            last_apc.starts_with("f=100,m=0;"),
            "the final chunk is m=0: {last_apc:?}"
        );
        // The last chunk is m=0 and carries the tail bytes' base64.
        assert!(
            escape.contains("m=0;"),
            "the final chunk ends the transmission: {escape:?}"
        );
        // Every chunk is a whole APC control, and the sequence ends ST.
        assert!(escape.ends_with("\x1b\\"), "{escape:?}");
        assert!(
            escape.matches("\x1b_G").count() >= 1,
            "the payload is chunked into APC controls: {escape:?}"
        );

        // Empty and oversized inputs are None — the caller's fallback.
        assert!(kitty_escape(&[]).is_none());
        assert!(kitty_escape(&vec![0u8; MAX_IMAGE_BYTES + 1]).is_none());
    }

    #[test]
    fn sixel_bands_encode_a_black_square_on_white() {
        // A 2x2 image: the left pixel black, the right white, fully
        // opaque. Band = 2 rows of sixels (both in one sixel char's bits
        // 0 and 1).
        let width = 2;
        let height = 2;
        let black = [0u8, 0, 0, 255];
        let white = [255u8, 255, 255, 255];
        let rgba: Vec<u8> = [black, white, black, white].concat();
        let escape = sixel_escape(&rgba, width, height).expect("escape");
        assert!(escape.starts_with("\x1bPq\"1;1;2;2"), "{escape:?}");
        assert!(escape.ends_with("\x1b\\"), "{escape:?}");
        // The left column's sixel char has bits 0|1 set (?=0x3f+3 →
        // `B`); the right column is empty (?).
        assert!(
            escape.contains("#B?-"),
            "black column B, white column ?: {escape:?}"
        );
        assert!(
            escape.contains("?") || escape.contains("B"),
            "the band carries the picture: {escape:?}"
        );

        // Transparent pixels read as paper (white): the same shape, all
        // alpha zero, encodes an empty band.
        let transparent: Vec<u8> = vec![0u8; 16];
        let escape = sixel_escape(&transparent, 2, 2).expect("escape");
        assert!(escape.contains("#??-"), "transparent is white: {escape:?}");

        // Not a whole pixel count: refused, not guessed.
        assert!(sixel_escape(&[0u8; 7], 2, 2).is_none());
        assert!(sixel_escape(&[], 2, 2).is_none());
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(&[]), "");
    }
}
