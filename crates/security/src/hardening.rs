//! P12-024 fuzz targets over decoder boundaries. Malformed, oversized, and
//! hostile inputs must produce typed errors — never panics or unbounded
//! allocation. Run via `cargo test -p security --lib hardening`.

/// Fuzz-style loop over the ACP stdio JSON-RPC frame decoder.
/// Returns the number of inputs correctly rejected with typed errors.
pub fn fuzz_acp_frame_decoder(cases: &[&[u8]]) -> usize {
    use acp::stdio::{FrameReader, StdioError};
    let mut rejected = 0usize;
    for case in cases {
        let mut reader = FrameReader::new(*case, 1024).expect("reader bounds valid");
        match reader.read_frame(&acp::stdio::CancellationToken::new()) {
            // A clean parse of one line is acceptable; hostile input must be
            // rejected with typed errors. Either way: no panic, no hang.
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(
                StdioError::FrameTooLarge { .. }
                | StdioError::UnrecoverableFrame { .. } | StdioError::EmbeddedNewline
                | StdioError::LimitInvalid,
            ) => rejected += 1,
            Err(_) => {}
        }
    }
    rejected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_decoder_survives_hostile_inputs_without_panicking() {
        let oversized = vec![b'x'; 4096];
        let bomb_line = format!("{}{}", "{\"a\":".repeat(500), "1".to_owned() + &"}".repeat(500));
        let cases: Vec<&[u8]> = vec![
            b"",                                     // empty
            b"\n\n\n",                               // blank lines
            b"{\"jsonrpc\":\"2.0\"}\n",              // incomplete but well-formed JSON line
            &[0xff, 0xfe, 0x00, b'\n'],              // invalid UTF-8 payload
            &oversized,                              // exceeds 1024-byte reader bound
            b"{\"id\":1e999}\n",                     // hostile number
            bomb_line.as_bytes(),                    // nested JSON bomb
        ];
        let rejected = fuzz_acp_frame_decoder(&cases);
        assert!(
            rejected >= 2,
            "oversized and limit-violating inputs must be typed rejections"
        );
    }

    #[test]
    fn oversized_single_line_is_rejected_with_typed_bound_error() {
        let mut big = vec![b'a'; 2049];
        big.push(b'\n');
        let mut reader =
            acp::stdio::FrameReader::new(&big[..], 1024).expect("bounds");
        let outcome = reader.read_frame(&acp::stdio::CancellationToken::new());
        assert!(matches!(outcome, Err(acp::stdio::StdioError::FrameTooLarge { .. })));
    }
}
