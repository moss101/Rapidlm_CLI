//! The argument grammar custom commands and skills share.
//!
//! `$ARGUMENTS` is the raw string; `$1` … `$9` are its words, split as a shell
//! splits them (quotes group, a backslash escapes); `$$` is a literal dollar;
//! `$<name>` is the word at the position of a declared name (a skill declares
//! up to nine). Any other `$word` is literal, a missing word is empty, and a
//! template that uses none of these is followed by the raw string after a blank
//! line. Substitution is textual and done here: no shell, environment, file or
//! command is consulted, so what the arguments say is never run (S4).

/// Most positional arguments (`$1` through `$9`).
pub const MAX_POSITIONAL: usize = 9;

/// Split `args` into words.
///
/// Whitespace separates words. A quote opens a group only at the start of a
/// word and only when the same quote appears again later, so an apostrophe in
/// `don't` or a stray `"` stays literal rather than swallowing the rest. A
/// backslash escapes only a quote, a backslash or whitespace, which leaves
/// Windows paths such as `src\lib.rs` intact.
#[must_use]
pub fn split_args(args: &str) -> Vec<String> {
    let chars: Vec<char> = args.chars().collect();
    let mut words = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(q) => {
                if q == '"' && c == '\\' && matches!(chars.get(i + 1), Some('"' | '\\')) {
                    i += 1;
                    current.push(chars[i]);
                } else {
                    current.push(c);
                }
            }
            None if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None if (c == '"' || c == '\'') && !started && chars[i + 1..].contains(&c) => {
                quote = Some(c);
                started = true;
            }
            None if c == '\\'
                && chars
                    .get(i + 1)
                    .is_some_and(|n| matches!(n, '"' | '\'' | '\\') || n.is_whitespace()) =>
            {
                i += 1;
                current.push(chars[i]);
                started = true;
            }
            None => {
                current.push(c);
                started = true;
            }
        }
        i += 1;
    }
    if started {
        words.push(current);
    }
    words
}

/// What an expansion produced, and where in it the arguments are.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Expanded {
    pub text: String,
    /// Where in `text` the arguments end, for a caller that may cut `text`:
    /// the end of the first `$ARGUMENTS`, if the template has one anywhere;
    /// otherwise the end of the first `$1`…`$9` or declared name (a template
    /// that takes words by position chose which words). `None` when the
    /// template names none, and when `limit` stopped the expansion before the
    /// `$ARGUMENTS` it has.
    pub landed_end: Option<usize>,
    /// `limit` left something out: the rest of the template, which is not in
    /// `text`, would have added more than blanks.
    pub stopped: bool,
}

impl Expanded {
    /// A template used as written: nothing filled in, no argument in it.
    #[must_use]
    pub fn verbatim(template: &str) -> Self {
        Self {
            text: template.to_owned(),
            landed_end: None,
            stopped: false,
        }
    }
}

/// Where an expansion's output goes. Past the limit nothing more is kept; the
/// rest of the template is still read, to learn whether it would have added
/// anything but blanks.
struct Output {
    text: String,
    limit: usize,
    /// The limit is passed: what follows is only looked at.
    past: bool,
    /// Something other than blanks was left out.
    left_out: bool,
}

impl Output {
    fn put(&mut self, piece: &str) {
        if self.past {
            self.left_out |= !piece.trim().is_empty();
        } else {
            self.text.push_str(piece);
        }
    }

    /// Called before each piece: the output is past the limit once it is
    /// longer than it.
    fn check(&mut self) {
        self.past |= self.text.len() > self.limit;
    }
}

/// Fill `template` from the raw argument string `raw`, with `names` the
/// declared argument names (in order). Once the output is longer than `limit`
/// bytes the rest of the template is not kept: a caller that will cut the text
/// at `limit` anyway does not pay for a template that repeats a long argument
/// thousands of times. Whether the rest would have added anything but blanks
/// is still worked out (`stopped`), and so is whether it holds `$ARGUMENTS`.
/// The text may run past `limit` by what the last piece before the check
/// added: a stretch of the template and a value.
#[must_use]
pub fn expand(template: &str, names: &[String], raw: &str, limit: usize) -> Expanded {
    let raw = raw.trim();
    let positional = split_args(raw);
    let mut out = Output {
        text: String::with_capacity(template.len() + raw.len()),
        limit,
        past: false,
        left_out: false,
    };
    let mut whole_end = None;
    let mut word_end = None;
    let mut whole_later = false;
    let mut rest = template;
    while let Some(at) = rest.find('$') {
        out.check();
        out.put(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            out.put("$");
            rest = tail;
        } else if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            out.put(raw);
            if out.past {
                whole_later = true;
            } else {
                whole_end.get_or_insert(out.text.len());
            }
            rest = tail;
        } else if let Some(digit) = after.chars().next().filter(|c| ('1'..='9').contains(c)) {
            let index = digit as usize - '1' as usize;
            debug_assert!(index < MAX_POSITIONAL);
            if let Some(word) = positional.get(index) {
                out.put(word);
            }
            if !out.past {
                word_end.get_or_insert(out.text.len());
            }
            rest = &after[1..];
        } else if let Some((index, tail)) = declared_name(after, names) {
            if let Some(word) = positional.get(index) {
                out.put(word);
            }
            if !out.past {
                word_end.get_or_insert(out.text.len());
            }
            rest = tail;
        } else {
            out.put("$");
            rest = after;
        }
    }
    out.check();
    out.put(rest);
    // A template that has `$ARGUMENTS` counts on it, even past the limit.
    let has_whole = whole_end.is_some() || whole_later;
    Expanded {
        text: out.text,
        landed_end: if has_whole { whole_end } else { word_end },
        stopped: out.left_out,
    }
}

/// [`expand`], with the raw string added after a blank line when the template
/// used none of the placeholders: what a custom command is.
#[must_use]
pub fn fill(template: &str, names: &[String], raw: &str) -> String {
    let raw = raw.trim();
    let mut expanded = expand(template, names, raw, usize::MAX);
    if expanded.landed_end.is_none() && !raw.is_empty() {
        expanded.text.push_str("\n\n");
        expanded.text.push_str(raw);
    }
    expanded.text
}

/// The declared name that `after` (the text following a `$`) starts with, as a
/// whole identifier — `$version` is the name, `$versions` is not — with its
/// position and the text after it.
fn declared_name<'a>(after: &'a str, names: &[String]) -> Option<(usize, &'a str)> {
    let first = after.chars().next()?;
    if !first.is_ascii_lowercase() {
        return None;
    }
    let end = after
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(after.len());
    let word = &after[..end];
    names
        .iter()
        .position(|name| name == word)
        .map(|index| (index, &after[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    fn fill_with(template: &str, declared: &[&str], raw: &str) -> String {
        fill(template, &names(declared), raw)
    }

    #[test]
    fn the_placeholders_fill_from_the_raw_string() {
        assert_eq!(
            fill_with(
                "all: $ARGUMENTS; first: $1; second: $2",
                &[],
                r#"a "b c" d"#
            ),
            r#"all: a "b c" d; first: a; second: b c"#
        );
        assert_eq!(
            fill_with("$1$2$3", &[], "x y"),
            "xy",
            "a missing word is empty"
        );
        assert_eq!(
            fill_with("cost $$5 and $$", &[], "ignored"),
            "cost $5 and $\n\nignored"
        );
        // Words are split as a shell splits them: quotes group, a backslash escapes.
        assert_eq!(
            fill_with("$1|$2|$3", &[], r#"'one two' three\ four five"#),
            "one two|three four|five"
        );
    }

    #[test]
    fn a_declared_name_is_the_word_at_its_position() {
        let declared = ["version", "channel"];
        assert_eq!(
            fill_with(
                "release $version on $channel",
                &declared,
                r#"1.2.3 "stable branch""#
            ),
            "release 1.2.3 on stable branch"
        );
        // A name is a whole identifier: a longer word that starts with it, a
        // name nobody declared and a capitalised word are literal.
        assert_eq!(
            fill_with(
                "$versions $other $Version $version_ $version",
                &declared,
                "9"
            ),
            "$versions $other $Version $version_ 9"
        );
        // ASCII letters, digits and `_` after a name make a longer word; any
        // other character ends the name.
        assert_eq!(
            fill_with(
                "$versionX $version2 $version\u{c9} $version-x $version.",
                &declared,
                "9"
            ),
            "$versionX $version2 9\u{c9} 9-x 9."
        );
        // Names and numbers address the same words.
        assert_eq!(fill_with("$1 = $version", &declared, "7 8"), "7 = 7");
        // A declared name with no word is empty.
        assert_eq!(fill_with("[$channel]", &declared, "only-one"), "[]");
        // With no names declared, `$name` is literal.
        assert_eq!(fill_with("$version", &[], "9"), "$version\n\n9");
    }

    #[test]
    fn a_template_that_uses_none_gets_the_arguments_after_a_blank_line() {
        assert_eq!(
            fill_with("Do the thing.", &["a"], "  x y  "),
            "Do the thing.\n\nx y"
        );
        assert_eq!(fill_with("Do the thing.", &["a"], "   "), "Do the thing.");
        assert_eq!(
            fill_with("Use $1.", &[], "x"),
            "Use x.",
            "a template that uses one does not also append"
        );
    }

    fn landed_end(template: &str, declared: &[&str], raw: &str) -> Option<usize> {
        expand(template, &names(declared), raw, usize::MAX).landed_end
    }

    #[test]
    fn the_arguments_land_where_the_first_placeholder_that_stands_for_them_ends() {
        assert_eq!(landed_end("none", &[], "x"), None);
        assert_eq!(
            landed_end("cost $$5", &[], "x"),
            None,
            "an escaped dollar is not an argument"
        );
        assert_eq!(landed_end("$a", &["a"], "x"), Some(1));
        assert_eq!(landed_end("x $a", &["a"], "x"), Some(3));
        assert_eq!(landed_end("ab $1 cd $2", &[], "w"), Some(4));
        assert_eq!(
            landed_end("[$$] $ARGUMENTS", &[], "w"),
            Some(5),
            "an offset in the output, where `$$` has become one byte"
        );
        assert_eq!(
            landed_end("$1 then $ARGUMENTS", &[], "w"),
            Some(8),
            "the whole string is what `$ARGUMENTS` stands for, not the first word"
        );
        assert_eq!(
            landed_end("$ARGUMENTS and $ARGUMENTS", &[], "a b c"),
            Some(5),
            "the first use, not the last"
        );
        assert_eq!(
            landed_end("go $2 $1", &[], "only"),
            Some(3),
            "a placeholder with no word is still a use"
        );
    }

    #[test]
    fn a_stop_before_the_whole_string_is_reached_lands_nothing() {
        let template = format!("$1 {} $1 $ARGUMENTS", "y".repeat(40));
        let stopped = expand(&template, &[], "w", 10);
        assert!(stopped.stopped);
        assert_eq!(
            stopped.landed_end, None,
            "the template counts on `$ARGUMENTS`, which the text does not reach"
        );
        let words_only = format!("$1 {} $1 $1", "y".repeat(40));
        let stopped = expand(&words_only, &[], "w", 10);
        assert!(stopped.stopped);
        assert_eq!(stopped.landed_end, Some(1));
        assert!(!expand(&words_only, &[], "w", usize::MAX).stopped);
        // An escaped dollar is not `$ARGUMENTS`: this template does not count on it.
        let escaped = format!("$1 {} $1 $$ARGUMENTS", "y".repeat(40));
        let stopped = expand(&escaped, &[], "w", 10);
        assert!(stopped.stopped);
        assert_eq!(stopped.landed_end, Some(1));
        let odd = format!("$1 {} $1 $$$ARGUMENTS", "y".repeat(40));
        assert_eq!(
            expand(&odd, &[], "w", 10).landed_end,
            None,
            "an escaped dollar and then the real thing"
        );
    }

    #[test]
    fn what_the_limit_leaves_out_is_a_stop_only_if_it_would_have_added_more_than_blanks() {
        // Blank lines and empty values past the limit add nothing.
        let blanks = expand("aaaa\n\n\n$1\n$2\n", &[], "", 3);
        assert!(!blanks.stopped, "{blanks:?}");
        assert_eq!(blanks.landed_end, Some(7));
        // A word, text, an argument or a dollar past it does.
        for template in [
            "aaaa$1 more text",
            "aaaa$1$2 $1 w",
            "aaaa$1 $ARGUMENTS",
            "aaaa$1 $$",
            "aaaa$1 $other",
        ] {
            assert!(expand(template, &[], "w", 3).stopped, "{template:?}");
        }
        assert!(!expand("aaaa$1 $2", &[], "w", 3).stopped, "an empty value");
    }

    #[test]
    fn the_output_is_bounded_however_often_the_template_repeats_an_argument() {
        let template = "$1".repeat(30_000);
        let raw = "a".repeat(4096);
        let limit = 16 * 1024;
        let bounded = expand(&template, &[], &raw, limit);
        assert!(bounded.text.len() > limit, "it stops after the limit");
        assert!(bounded.text.len() <= limit + raw.len());
        assert!(
            bounded.text.capacity() < 1 << 20,
            "nothing near the {} bytes in full was allocated: {}",
            30_000 * raw.len(),
            bounded.text.capacity()
        );
    }

    #[test]
    fn a_limit_changes_only_how_much_of_the_end_is_left_out() {
        let template = "head $1\n".repeat(200);
        let whole = expand(&template, &[], "word", usize::MAX);
        let bounded = expand(&template, &[], "word", 100);
        assert!(bounded.text.len() < whole.text.len());
        assert!(whole.text.starts_with(&bounded.text));
        assert!(bounded.stopped && !whole.stopped);
        assert_eq!(bounded.landed_end, whole.landed_end);
        // A limit the output never reaches changes nothing.
        assert_eq!(expand(&template, &[], "word", whole.text.len()), whole);
    }

    #[test]
    fn an_unknown_dollar_word_stays_literal() {
        assert_eq!(
            fill_with("$HOME $(date) ${x} $ARGUMENTSX", &[], "v"),
            "$HOME $(date) ${x} vX"
        );
    }
}
