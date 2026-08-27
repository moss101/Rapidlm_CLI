//! Sanitize untrusted model/tool/file text before any terminal backend sees it.
//!
//! Neutralizes OSC (8/52/title), CSI, DCS/APC/PM/SOS, C0/C1, and bidi format
//! controls so clipboard, title, hyperlink, and mode changes cannot fire.
//! Threat: `T-013`.

use std::borrow::Cow;

/// CSI / nF sequences longer than this are truncated and the tail is re-scanned.
pub const MAX_CONTROL_SEQUENCE_BYTES: usize = 4096;

const ESC: char = '\u{001B}';
const BEL: char = '\u{0007}';
const DEL: char = '\u{007F}';
const CSI_8BIT: char = '\u{009B}';
const ST_8BIT: char = '\u{009C}';
const OSC_8BIT: char = '\u{009D}';
const DCS_8BIT: char = '\u{0090}';
const SOS_8BIT: char = '\u{0098}';
const PM_8BIT: char = '\u{009E}';
const APC_8BIT: char = '\u{009F}';

/// Preserve printable text and newlines; neutralize terminal-active controls.
pub fn sanitize_untrusted(input: &str) -> Cow<'_, str> {
    if is_already_safe(input) {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(rewrite(input))
    }
}

fn is_already_safe(input: &str) -> bool {
    input.chars().all(is_passthrough_char)
}

fn is_passthrough_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | ' '..='~') || (c >= '\u{00A0}' && !is_neutralized_format(c))
}

fn is_neutralized_format(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{206A}'..='\u{206F}'
    )
}

fn rewrite(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(c) = rest.chars().next() {
        if c == ESC {
            rest = skip_esc_sequence(rest);
            continue;
        }
        if is_c1(c) {
            rest = skip_c1_sequence(rest, c);
            continue;
        }
        if c == '\r' {
            out.push('\n');
            rest = &rest[1..];
            if rest.starts_with('\n') {
                rest = &rest[1..];
            }
            continue;
        }
        if is_passthrough_char(c) {
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn is_c1(c: char) -> bool {
    matches!(c, '\u{0080}'..='\u{009F}')
}

fn is_string_introducer(c: char) -> bool {
    matches!(c, ']' | 'P' | 'X' | '^' | '_')
}

fn skip_esc_sequence(rest: &str) -> &str {
    let after_esc = &rest[ESC.len_utf8()..];
    let Some(next) = after_esc.chars().next() else {
        return after_esc;
    };
    let after_next = &after_esc[next.len_utf8()..];
    match next {
        '[' => skip_csi_body(after_next),
        c if is_string_introducer(c) => skip_control_string(after_next),
        '\\' => after_next,
        c if is_intermediate_byte(c) => skip_nf_sequence(after_esc),
        c if is_esc_final_byte(c) => after_next,
        _ => after_esc,
    }
}

fn skip_c1_sequence(rest: &str, introducer: char) -> &str {
    let after = &rest[introducer.len_utf8()..];
    match introducer {
        CSI_8BIT => skip_csi_body(after),
        OSC_8BIT | DCS_8BIT | SOS_8BIT | PM_8BIT | APC_8BIT => skip_control_string(after),
        ST_8BIT => after,
        _ => after,
    }
}

fn is_intermediate_byte(c: char) -> bool {
    matches!(c, '\u{0020}'..='\u{002F}')
}

fn is_esc_final_byte(c: char) -> bool {
    matches!(c, '\u{0030}'..='\u{007E}')
}

fn skip_csi_body(mut rest: &str) -> &str {
    let mut consumed = 0usize;
    while let Some(c) = rest.chars().next() {
        let width = c.len_utf8();
        consumed = consumed.saturating_add(width);
        if consumed > MAX_CONTROL_SEQUENCE_BYTES {
            return rest;
        }
        if c == ESC || is_c1(c) {
            return rest;
        }
        let code = c as u32;
        if code < 0x20 || c == DEL {
            rest = &rest[width..];
            continue;
        }
        if (0x20..=0x3F).contains(&code) {
            rest = &rest[width..];
            continue;
        }
        if (0x40..=0x7E).contains(&code) {
            return &rest[width..];
        }
        return rest;
    }
    rest
}

fn skip_nf_sequence(mut rest: &str) -> &str {
    let mut consumed = 0usize;
    let mut saw_intermediate = false;
    while let Some(c) = rest.chars().next() {
        let width = c.len_utf8();
        consumed = consumed.saturating_add(width);
        if consumed > MAX_CONTROL_SEQUENCE_BYTES {
            return rest;
        }
        if is_intermediate_byte(c) {
            saw_intermediate = true;
            rest = &rest[width..];
            continue;
        }
        if saw_intermediate && is_esc_final_byte(c) {
            return &rest[width..];
        }
        return rest;
    }
    rest
}

fn skip_control_string(mut rest: &str) -> &str {
    while let Some(c) = rest.chars().next() {
        if c == BEL || c == ST_8BIT {
            return &rest[c.len_utf8()..];
        }
        if c == ESC {
            let after_esc = &rest[ESC.len_utf8()..];
            if let Some(after_backslash) = after_esc.strip_prefix('\\') {
                return after_backslash;
            }
            return rest;
        }
        rest = &rest[c.len_utf8()..];
    }
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    type CorpusCase = (&'static str, &'static str, &'static str);

    /// Deterministic fuzz corpus: OSC 8/52, CSI, ESC, bidi, and control edges.
    const FUZZ_CORPUS: &[CorpusCase] = &[
        ("plain", "hello world", "hello world"),
        ("empty", "", ""),
        ("tab_lf", "a\tb\nc", "a\tb\nc"),
        ("unicode", "hello 世界 🦀", "hello 世界 🦀"),
        (
            "osc8_bel",
            "\u{1b}]8;;https://evil.example/x\u{07}visible\u{1b}]8;;\u{07}",
            "visible",
        ),
        (
            "osc8_st",
            "\u{1b}]8;;https://evil.example/x\u{1b}\\click",
            "click",
        ),
        (
            "osc8_params",
            "\u{1b}]8;id=1:foo=bar;https://phish.test\u{07}lab\u{1b}]8;;\u{07}",
            "lab",
        ),
        (
            "osc52_clipboard",
            "pre\u{1b}]52;c;c2VjcmV0\u{07}post",
            "prepost",
        ),
        ("osc0_title", "\u{1b}]0;pwned-title\u{07}ok", "ok"),
        ("osc1_icon", "\u{1b}]1;icon\u{07}ok", "ok"),
        ("osc2_title", "\u{1b}]2;window\u{07}ok", "ok"),
        ("osc_st8", "\u{1b}]0;title\u{9c}ok", "ok"),
        ("csi_sgr", "\u{1b}[31mred\u{1b}[0m", "red"),
        ("csi_truecolor", "\u{1b}[38:2:255:0:0mrgb", "rgb"),
        ("csi_altscreen_on", "\u{1b}[?1049hsecret", "secret"),
        ("csi_altscreen_off", "keep\u{1b}[?1049l", "keep"),
        ("csi_alt47", "\u{1b}[?47hbody", "body"),
        ("csi_erase", "\u{1b}[2J\u{1b}[Hhome", "home"),
        ("csi_cup", "\u{1b}[10;10Hhere", "here"),
        ("csi_private", "\u{1b}[>cdev", "dev"),
        ("esc_ris", "\u{1b}creset", "reset"),
        ("esc_save_restore", "\u{1b}7x\u{1b}8", "x"),
        ("esc_charset", "\u{1b}(Blatin", "latin"),
        ("esc_dec_aln", "\u{1b}#8fill", "fill"),
        ("esc_utf8", "\u{1b}%Gutf", "utf"),
        ("dcs", "\u{1b}P1$r\u{1b}\\after", "after"),
        ("apc", "\u{1b}_payload\u{07}after", "after"),
        ("pm", "\u{1b}^payload\u{1b}\\after", "after"),
        ("sos", "\u{1b}Xpayload\u{9c}after", "after"),
        ("bel", "a\u{07}b", "ab"),
        ("backspace", "ab\u{08}c", "abc"),
        ("tab_kept", "col\tcol", "col\tcol"),
        ("nul", "a\u{00}b", "ab"),
        ("del", "a\u{7f}b", "ab"),
        ("cr_overwrite", "secret\rpub", "secret\npub"),
        ("crlf", "a\r\nb", "a\nb"),
        ("lone_lf", "a\nb", "a\nb"),
        ("bidi_rlo", "safe\u{202e}ext\u{202c}", "safeext"),
        ("bidi_lre", "a\u{202a}b\u{202c}c", "abc"),
        ("bidi_isolate", "a\u{2066}b\u{2069}c", "abc"),
        ("bidi_rlm", "a\u{200f}b\u{200e}c", "abc"),
        ("bidi_alm", "a\u{061c}b", "ab"),
        ("c1_csi", "\u{9b}31mred", "red"),
        ("c1_osc52", "\u{9d}52;c;QQ==\u{9c}x", "x"),
        ("c1_st_only", "a\u{9c}b", "ab"),
        ("incomplete_esc", "ok\u{1b}", "ok"),
        ("incomplete_csi", "ok\u{1b}[31", "ok"),
        ("incomplete_osc", "ok\u{1b}]52;c;AAAA", "ok"),
        ("incomplete_osc8", "ok\u{1b}]8;;https://x", "ok"),
        ("esc_then_text", "\u{1b}hello", "ello"),
        ("double_esc", "\u{1b}\u{1b}[0mplain", "plain"),
        (
            "mixed_tool_output",
            "out:\u{1b}[32mok\u{1b}[0m \u{1b}]8;;https://x\u{07}link\u{1b}]8;;\u{07}\n\u{202e}bid\u{202c}",
            "out:ok link\nbid",
        ),
    ];

    fn assert_inert(label: &str, text: &str) {
        for (i, c) in text.char_indices() {
            let code = c as u32;
            assert!(
                c == '\t'
                    || c == '\n'
                    || (0x20..=0x7E).contains(&code)
                    || (c >= '\u{00A0}' && !is_neutralized_format(c)),
                "{label}: residual control U+{code:04X} at {i} in {text:?}"
            );
        }
        assert!(!text.contains(ESC), "{label}: residual ESC in {text:?}");
        assert!(!text.contains('\r'), "{label}: residual CR in {text:?}");
        assert!(
            !text.contains(BEL),
            "{label}: residual BEL (OSC terminator / alert) in {text:?}"
        );
    }

    #[test]
    fn corpus_golden_and_inert() {
        for (name, input, expected) in FUZZ_CORPUS {
            let got = sanitize_untrusted(input);
            assert_eq!(got.as_ref(), *expected, "golden {name}");
            assert_inert(name, got.as_ref());
        }
    }

    #[test]
    fn clean_text_is_borrowed() {
        match sanitize_untrusted("printable\tline\n世界") {
            Cow::Borrowed(s) => assert_eq!(s, "printable\tline\n世界"),
            Cow::Owned(_) => panic!("clean input must stay borrowed"),
        }
    }

    #[test]
    fn dirty_text_is_owned() {
        match sanitize_untrusted("\u{1b}[31mx") {
            Cow::Owned(s) => assert_eq!(s, "x"),
            Cow::Borrowed(_) => panic!("control input must be rewritten"),
        }
    }

    #[test]
    fn sanitize_is_idempotent() {
        for (name, input, _) in FUZZ_CORPUS {
            let once = sanitize_untrusted(input);
            let twice = sanitize_untrusted(once.as_ref());
            assert_eq!(once.as_ref(), twice.as_ref(), "idempotent {name}");
            assert!(
                matches!(twice, Cow::Borrowed(_)),
                "second pass must borrow {name}"
            );
        }
    }

    #[test]
    fn fuzz_inject_complete_controls_around_printable() {
        let payloads: &[&str] = &[
            "\u{1b}]8;;https://evil.test\u{07}",
            "\u{1b}]8;;https://evil.test\u{1b}\\",
            "\u{1b}]52;c;c2VjcmV0\u{07}",
            "\u{1b}]0;title\u{07}",
            "\u{1b}[?1049h",
            "\u{1b}[?1049l",
            "\u{1b}[2J",
            "\u{1b}[31;1m",
            "\u{1b}c",
            "\u{1b}(B",
            "\u{07}",
            "\u{08}",
            "\u{00}",
            "\u{7f}",
            "\u{202e}",
            "\u{202c}",
            "\u{2066}",
            "\u{2069}",
            "\u{9b}0m",
            "\u{9d}52;c;QQ==\u{9c}",
        ];
        for (i, ctrl) in payloads.iter().enumerate() {
            let input = format!("hello{ctrl}world");
            let got = sanitize_untrusted(&input);
            assert_inert(&format!("inject-{i}"), got.as_ref());
            assert_eq!(
                got.as_ref(),
                "helloworld",
                "inject-{i}: extra residue {got:?}"
            );
        }
    }

    #[test]
    fn fuzz_incomplete_sequences_fail_closed() {
        let payloads: &[&str] = &[
            "\u{1b}",
            "\u{1b}[999",
            "\u{1b}]52;c;OPEN",
            "\u{1b}]8;;https://x",
        ];
        for (i, ctrl) in payloads.iter().enumerate() {
            let input = format!("hello{ctrl}world");
            let got = sanitize_untrusted(&input);
            assert_inert(&format!("open-{i}"), got.as_ref());
            assert!(
                got.as_ref() == "hello" || got.starts_with("hello"),
                "open-{i}: lost prefix: {got:?} from {input:?}"
            );
        }
    }

    #[test]
    fn chunked_sanitize_cannot_reassemble_csi() {
        let first = sanitize_untrusted("\u{1b}[31");
        let second = sanitize_untrusted("mRED");
        let joined = format!("{first}{second}");
        assert_inert("chunked", &joined);
        assert_eq!(joined, "mRED");
    }

    #[test]
    fn fuzz_corpus_cartesian_pairs_stay_inert() {
        for (left_name, left, _) in FUZZ_CORPUS.iter().take(16) {
            for (right_name, right, _) in FUZZ_CORPUS.iter().rev().take(16) {
                let input = format!("{left}{right}");
                let got = sanitize_untrusted(&input);
                assert_inert(&format!("{left_name}+{right_name}"), got.as_ref());
                let again = sanitize_untrusted(got.as_ref());
                assert_eq!(got.as_ref(), again.as_ref());
            }
        }
    }

    #[test]
    fn long_csi_is_bounded_and_inert() {
        let mut input = String::from("\u{1b}[");
        input.extend(std::iter::repeat_n('0', MAX_CONTROL_SEQUENCE_BYTES + 32));
        input.push('m');
        input.push_str("tail");
        let got = sanitize_untrusted(&input);
        assert_inert("long-csi", got.as_ref());
        assert!(got.ends_with("tail"), "long-csi lost tail: {got:?}");
        assert!(!got.contains(ESC));
    }

    #[test]
    fn unterminated_osc_does_not_leak_introducer() {
        let input = format!("pre\u{1b}]52;c;{}", "A".repeat(128));
        let got = sanitize_untrusted(&input);
        assert_eq!(got.as_ref(), "pre");
        assert_inert("open-osc", got.as_ref());
    }

    #[test]
    fn cr_cannot_overwrite_prior_text() {
        let got = sanitize_untrusted("password=hunter2\ruser=public");
        assert_eq!(got.as_ref(), "password=hunter2\nuser=public");
        assert_inert("cr-overwrite", got.as_ref());
    }
}
