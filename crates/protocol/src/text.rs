//! Rules for text that comes from outside (a file, a server) and is shown to a
//! person or put in front of a model.

/// Characters that are not control characters but still change how a line is
/// shown or split, or say something a reader cannot see: the Unicode line and
/// paragraph separators, the zero-width space and the direction marks, the
/// bidirectional embeddings, overrides and isolates, the word joiners and the
/// deprecated format characters, the byte-order mark, and the tag characters
/// and variation selectors that can carry text no screen shows (a way to hide
/// an instruction in a description). The zero-width joiner and non-joiner stay:
/// they are part of the spelling of Persian, Indic scripts and many emoji.
#[must_use]
pub fn changes_the_line(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{200B}'
            | '\u{200E}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{E0000}'..='\u{E007F}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

/// `text` as one clean line: control characters and the characters of
/// [`changes_the_line`] gone, runs of white space folded to one space, the ends
/// trimmed. What is left may be empty.
#[must_use]
pub fn one_clean_line(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|c| !c.is_control() && !changes_the_line(*c))
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_characters_that_hide_or_split_a_line_are_named_and_the_joiners_stay() {
        for c in [
            '\u{061C}',
            '\u{200B}',
            '\u{200E}',
            '\u{2028}',
            '\u{2029}',
            '\u{202A}',
            '\u{202E}',
            '\u{2060}',
            '\u{2066}',
            '\u{206F}',
            '\u{FEFF}',
            '\u{E0041}',
            '\u{E007F}',
            '\u{E0100}',
        ] {
            assert!(changes_the_line(c), "{c:?}");
        }
        for c in ['a', ' ', '\u{200C}', '\u{200D}', 'é', '🙂', '\u{0627}'] {
            assert!(!changes_the_line(c), "{c:?}");
        }
    }

    #[test]
    fn one_clean_line_folds_white_space_and_drops_what_cannot_be_seen() {
        assert_eq!(one_clean_line("  a \t b\n c  "), "a b c");
        // A tab or a line break between words is a gap, not nothing.
        assert_eq!(one_clean_line("a\tb\nc\r\nd"), "a b c d");
        assert_eq!(one_clean_line("a\u{1b}[31m\u{202e}b \u{2028}c"), "a[31mb c");
        // Tag characters spelling text no screen shows are gone, not read.
        assert_eq!(one_clean_line("ok\u{E0049}\u{E0067}\u{E006E}"), "ok");
        assert_eq!(one_clean_line(" \u{7} \u{200B} "), "");
        // The joiners are spelling.
        assert_eq!(one_clean_line("a\u{200D}b"), "a\u{200D}b");
    }
}
