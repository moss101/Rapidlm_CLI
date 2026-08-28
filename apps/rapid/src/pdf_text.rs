//! Minimal text-based PDF extractor (Claude Read parity for text PDFs).
//!
//! Scans the raw file for `stream ... endstream` objects whose dictionary
//! declares `FlateDecode` (inflated with flate2) or no filter (literal),
//! then harvests text-showing operators: `(text) Tj`, `(text) '`, and
//! `[ (a) -120 (b) ] TJ`. Complex PDFs (CID fonts, embedded images, OCR)
//! are out of scope — extraction yields what the operators literally say.

use flate2::read::ZlibDecoder;
use std::io::Read;

/// Extract readable text from a PDF byte stream. Returns `None` when the
/// input is not a PDF or contains no harvestable text operators.
pub fn extract_pdf_text(bytes: &[u8]) -> Option<String> {
    if !bytes.starts_with(b"%PDF-") {
        return None;
    }
    let mut collected: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    while let Some(stream_rel) = find(&bytes[cursor..], b"stream") {
        let stream_start = cursor + stream_rel + b"stream".len();
        // Skip the EOL right after the `stream` keyword.
        let mut data_start = stream_start;
        if bytes.get(data_start) == Some(&b'\r') {
            data_start += 1;
        }
        if bytes.get(data_start) == Some(&b'\n') {
            data_start += 1;
        }
        let Some(end_rel) = find(&bytes[data_start..], b"endstream") else {
            break;
        };
        let data_end = data_start + end_rel;
        // The dictionary precedes the stream keyword.
        let dict_start = cursor;
        let dict = &bytes[dict_start..stream_start];
        let is_flate = find(dict, b"FlateDecode").is_some();
        let content: Vec<u8> = if is_flate {
            inflate(&bytes[data_start..data_end])?
        } else {
            bytes[data_start..data_end].to_vec()
        };
        let page_text = harvest_text_operators(&content);
        if !page_text.trim().is_empty() {
            collected.push(page_text);
        }
        cursor = data_end + b"endstream".len();
        if cursor >= bytes.len() {
            break;
        }
    }
    if collected.is_empty() {
        None
    } else {
        Some(collected.join("\n"))
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    Some(out)
}

/// Pull `(text) Tj` / `(text) '` / `[(a) 1 (b)] TJ` strings from a content
/// stream. Literal-string escapes (\\n \\r \\t \\( \\) \\\\ \\ddd) are decoded.
fn harvest_text_operators(content: &[u8]) -> String {
    let text = String::from_utf8_lossy(content);
    let mut out = String::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == '(' {
            // Every literal in a content stream is treated as shown text:
            // text-based PDFs express their strings via Tj/TJ operands.
            let (literal, next) = read_literal_string(&bytes, i);
            out.push_str(&literal);
            out.push('\n');
            i = next;
            continue;
        }
        i += 1;
    }
    out
}

/// Read a PDF literal string starting at `bytes[start] == '('`; returns the
/// decoded text and the index just past the closing unescaped `)`.
fn read_literal_string(bytes: &[char], start: usize) -> (String, usize) {
    let mut out = String::new();
    let mut depth = 1usize;
    let mut i = start + 1;
    while i < bytes.len() && depth > 0 {
        let ch = bytes[i];
        if ch == '\\' {
            i += 1;
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '(' => out.push('('),
                ')' => out.push(')'),
                '\\' => out.push('\\'),
                digit if digit.is_ascii_digit() => {
                    // Octal escape: up to 3 digits.
                    let mut value: u32 = 0;
                    let mut count = 0;
                    while i < bytes.len() && bytes[i].is_ascii_digit() && count < 3 {
                        value = value * 8 + bytes[i].to_digit(8).unwrap_or(0);
                        i += 1;
                        count += 1;
                    }
                    if let Some(decoded) = char::from_u32(value) {
                        out.push(decoded);
                    }
                    continue;
                }
                other => out.push(other),
            }
            i += 1;
            continue;
        }
        if ch == '(' {
            depth += 1;
        }
        if ch == ')' {
            depth -= 1;
            if depth == 0 {
                return (out, i + 1);
            }
        }
        out.push(ch);
        i += 1;
    }
    (out, i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write as _;

    fn wrap_stream(dict_extra: &str, content: &[u8]) -> Vec<u8> {
        let mut pdf = format!("%PDF-1.4\n1 0 obj\n<< /Length {} {dict_extra} >>\nstream\n", content.len()).into_bytes();
        pdf.extend_from_slice(content);
        pdf.extend_from_slice(b"\nendstream\nendobj\n%%EOF");
        pdf
    }

    fn uncompressed_pdf(content: &str) -> Vec<u8> {
        wrap_stream("", format!("BT /F1 12 Tf ({content}) Tj ET").as_bytes())
    }

    fn flate_pdf(content: &str) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(format!("BT /F1 12 Tf ({content}) Tj ET").as_bytes())
            .expect("deflate");
        let compressed = encoder.finish().expect("finish");
        wrap_stream("/Filter /FlateDecode", &compressed)
    }

    #[test]
    fn extracts_text_from_uncompressed_stream() {
        let pdf = uncompressed_pdf("hello from pdf");
        let text = extract_pdf_text(&pdf).expect("text");
        assert!(text.contains("hello from pdf"), "{text}");
    }

    #[test]
    fn extracts_text_from_flate_stream() {
        let pdf = flate_pdf("compressed hello");
        let text = extract_pdf_text(&pdf).expect("text");
        assert!(text.contains("compressed hello"), "{text}");
    }

    #[test]
    fn tj_arrays_and_escapes_are_harvested() {
        let content = br"BT [(multi) -250 (part)] TJ ET BT (tab\tnl\nparen\(x\)) ' ET";
        let mut pdf = b"%PDF-1.4\n".to_vec();
        pdf.extend_from_slice(b"1 0 obj\n<< /Length 200 >>\nstream\n");
        pdf.extend_from_slice(content);
        pdf.extend_from_slice(b"\nendstream\nendobj\n%%EOF");
        let text = extract_pdf_text(&pdf).expect("text");
        assert!(text.contains("multi"), "{text}");
        assert!(text.contains("part"), "{text}");
        assert!(text.contains("tab\t"), "{text}");
        assert!(text.contains("paren(x)"), "{text}");
    }

    #[test]
    fn non_pdf_input_returns_none() {
        assert!(extract_pdf_text(b"plain text file").is_none());
        assert!(extract_pdf_text(&[]).is_none());
    }

    #[test]
    fn pdf_without_text_operators_yields_none() {
        let pdf = wrap_stream("", b"/Annots []");
        assert!(extract_pdf_text(&pdf).is_none());
    }
}
