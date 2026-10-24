//! The closed subset of YAML front matter that skill files and agent
//! definitions share.
//!
//! A document is `---`, a block of `key: value` lines, `---`, and then a body.
//! This module only *tokenizes* the block: which keys exist, which are allowed
//! and what their values mean is each consumer's schema (the skills loader
//! refuses privilege keys; an agent definition accepts `permission`, since all
//! it can do is narrow). What is read:
//!
//! - a scalar: `key: text`, bare or in double quotes (`\n`, `\t`, `\"` and `\\`
//!   are the only escapes);
//! - an empty list: `key: []`;
//! - a block list: `key:` followed by lines `  - item` (or `- item`);
//! - with [`Rules::nested_maps`], a one-level map: `key:` followed by indented
//!   lines `  name: value`, where `name` may be in double quotes (so it can hold
//!   spaces, parentheses and `*`);
//! - blank lines and `#` comments.
//!
//! Everything else — flow lists with items, single quotes, deeper nesting, tabs
//! or spaces before a key, a key with a space — is [`Syntax::Invalid`]. No
//! anchors, tags, multi-line scalars or merge keys exist here, by design.

/// One value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Value {
    Scalar(String),
    List(Vec<String>),
    /// `(name, value)` pairs in the order written (one level only).
    Map(Vec<(String, String)>),
}

/// The keys of a block, in the order written.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Fields {
    entries: Vec<(String, Value)>,
}

impl Fields {
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value)
    }

    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(key, _)| key.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// What is wrong with the block itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Syntax {
    /// Not the closed subset.
    Invalid,
    /// A scalar over [`Rules::max_scalar_bytes`].
    ScalarTooLarge,
}

/// Why [`decode`] stopped: the block is not well formed, or the caller's own
/// check on a line or a key said no.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stop<E> {
    Syntax(Syntax),
    Caller(E),
}

/// What the tokenizer accepts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rules {
    /// The longest scalar (and list item, and map value) read.
    pub max_scalar_bytes: usize,
    /// Accept a one-level map under a key.
    pub nested_maps: bool,
}

/// The text after the opening `---` line: `(front matter, body)`. A leading
/// byte-order mark is allowed; the closing `---` is a line of its own. `None`
/// without both fences.
#[must_use]
pub fn split(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = strip_one_newline(text.strip_prefix("---")?);
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// `text` without one leading line break.
#[must_use]
pub fn strip_one_newline(text: &str) -> &str {
    text.strip_prefix("\r\n")
        .or_else(|| text.strip_prefix('\n'))
        .unwrap_or(text)
}

/// Tokenize `front`. `on_line(index)` is called with each line's number
/// (from zero) before it is read, and `on_key(key)` with each key as soon as
/// it is read — before the rest of the line is judged and before the
/// duplicate check — so a consumer's refusal of a key comes ahead of any later
/// syntax error, as it does when the consumer reads the lines itself.
///
/// # Errors
///
/// [`Stop::Syntax`] for a block outside the subset or a repeated key,
/// [`Stop::Caller`] with whatever the callbacks returned.
pub fn decode<E>(
    front: &str,
    rules: Rules,
    on_line: &mut dyn FnMut(usize) -> Result<(), E>,
    on_key: &mut dyn FnMut(&str) -> Result<(), E>,
) -> Result<Fields, Stop<E>> {
    let mut entries: Vec<(String, Value)> = Vec::new();
    // A key that opened a block (`key:` and nothing after it) and what it has
    // collected so far.
    let mut open: Option<(String, Block)> = None;
    for (index, raw_line) in front.lines().enumerate() {
        on_line(index).map_err(Stop::Caller)?;
        if raw_line.trim().is_empty() || raw_line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(item) = list_item(raw_line, rules.max_scalar_bytes) {
            let item = item.map_err(Stop::Syntax)?;
            match open.as_mut() {
                Some((_, block)) => block.push_item(item).map_err(Stop::Syntax)?,
                None => return Err(Stop::Syntax(Syntax::Invalid)),
            }
            continue;
        }
        if raw_line.starts_with(' ') || raw_line.starts_with('\t') {
            // Indented, and not a list item: a map entry under an open key.
            if !rules.nested_maps {
                return Err(Stop::Syntax(Syntax::Invalid));
            }
            let Some((_, block)) = open.as_mut() else {
                return Err(Stop::Syntax(Syntax::Invalid));
            };
            let (name, value) =
                map_entry(raw_line, rules.max_scalar_bytes).map_err(Stop::Syntax)?;
            block.push_entry(name, value).map_err(Stop::Syntax)?;
            continue;
        }
        close(&mut entries, &mut open);
        let (key, value) = key_line(raw_line, rules.max_scalar_bytes).map_err(Stop::Syntax)?;
        on_key(&key).map_err(Stop::Caller)?;
        if entries.iter().any(|(existing, _)| existing == &key) {
            return Err(Stop::Syntax(Syntax::Invalid));
        }
        match value {
            None => open = Some((key, Block::default())),
            Some(value) => entries.push((key, value)),
        }
    }
    close(&mut entries, &mut open);
    Ok(Fields { entries })
}

/// A block under a key that has no value of its own.
#[derive(Default)]
struct Block {
    items: Vec<String>,
    pairs: Vec<(String, String)>,
}

impl Block {
    fn push_item(&mut self, item: String) -> Result<(), Syntax> {
        if !self.pairs.is_empty() {
            return Err(Syntax::Invalid);
        }
        self.items.push(item);
        Ok(())
    }

    fn push_entry(&mut self, name: String, value: String) -> Result<(), Syntax> {
        if !self.items.is_empty() || self.pairs.iter().any(|(existing, _)| existing == &name) {
            return Err(Syntax::Invalid);
        }
        self.pairs.push((name, value));
        Ok(())
    }
}

/// Finish the open key: a map when it collected entries, else a list (empty
/// when nothing followed the key).
fn close(entries: &mut Vec<(String, Value)>, open: &mut Option<(String, Block)>) {
    if let Some((key, block)) = open.take() {
        let value = if block.pairs.is_empty() {
            Value::List(block.items)
        } else {
            Value::Map(block.pairs)
        };
        entries.push((key, value));
    }
}

fn key_line(line: &str, max: usize) -> Result<(String, Option<Value>), Syntax> {
    let (raw_key, rest) = line.split_once(':').ok_or(Syntax::Invalid)?;
    let key = raw_key.trim();
    if key.is_empty() || key.contains(' ') {
        return Err(Syntax::Invalid);
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok((key.to_owned(), None));
    }
    if rest == "[]" {
        return Ok((key.to_owned(), Some(Value::List(Vec::new()))));
    }
    if rest.starts_with('[') {
        return Err(Syntax::Invalid);
    }
    Ok((key.to_owned(), Some(Value::Scalar(scalar(rest, max)?))))
}

/// `  - item`, `\t- item` or (the odd one) `- item` at the margin. `None` when
/// the line is not a list item at all.
fn list_item(line: &str, max: usize) -> Option<Result<String, Syntax>> {
    let trimmed = if let Some(rest) = line.strip_prefix("  - ") {
        rest
    } else if let Some(rest) = line.strip_prefix("\t- ") {
        rest
    } else {
        let rest = line.strip_prefix("- ")?;
        if line.starts_with('-') && !line.starts_with(' ') && !line.starts_with('\t') {
            return Some(Err(Syntax::Invalid));
        }
        rest
    };
    Some(scalar(trimmed.trim(), max))
}

/// `  name: value` or `  "any name": value`.
fn map_entry(line: &str, max: usize) -> Result<(String, String), Syntax> {
    let body = line.trim_start_matches([' ', '\t']);
    let (name, rest) = if let Some(quoted) = body.strip_prefix('"') {
        // The name is everything to the closing quote, with the same escapes.
        let mut end = None;
        let mut escaped = false;
        for (at, ch) in quoted.char_indices() {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                end = Some(at);
                break;
            }
        }
        let end = end.ok_or(Syntax::Invalid)?;
        let name = unescape(&quoted[..end])?;
        let after = quoted[end + 1..].trim_start();
        let rest = after.strip_prefix(':').ok_or(Syntax::Invalid)?;
        (name, rest)
    } else {
        let (name, rest) = body.split_once(':').ok_or(Syntax::Invalid)?;
        if name.is_empty() || name.contains(' ') || name.contains('"') {
            return Err(Syntax::Invalid);
        }
        (name.to_owned(), rest)
    };
    if name.is_empty() || name.len() > max || name.chars().any(char::is_control) {
        return Err(Syntax::Invalid);
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return Err(Syntax::Invalid);
    }
    Ok((name, scalar(rest, max)?))
}

fn scalar(raw: &str, max: usize) -> Result<String, Syntax> {
    if raw.len() > max {
        return Err(Syntax::ScalarTooLarge);
    }
    if raw.chars().any(char::is_control) {
        return Err(Syntax::Invalid);
    }
    if let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return unescape(inner);
    }
    if raw.starts_with('\'') {
        return Err(Syntax::Invalid);
    }
    Ok(raw.to_owned())
}

fn unescape(inner: &str) -> Result<String, Syntax> {
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(_) | None => return Err(Syntax::Invalid),
            }
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: Rules = Rules {
        max_scalar_bytes: 1024,
        nested_maps: false,
    };

    fn read(front: &str, rules: Rules) -> Result<Fields, Stop<()>> {
        decode(front, rules, &mut |_| Ok(()), &mut |_| Ok(()))
    }

    fn scalar_of(fields: &Fields, key: &str) -> Option<String> {
        match fields.get(key) {
            Some(Value::Scalar(text)) => Some(text.clone()),
            _ => None,
        }
    }

    #[test]
    fn a_document_is_split_at_its_two_fences() {
        let (front, body) = split("---\nname: x\n---\nbody\nmore").expect("split");
        assert_eq!(front, "name: x\n");
        assert_eq!(body, "body\nmore");
        // A byte-order mark and CRLF line ends are allowed.
        let (front, body) = split("\u{feff}---\r\nname: x\r\n---\r\nbody").expect("split");
        assert_eq!(front, "name: x\r\n");
        assert_eq!(body, "body");
        // Both fences, and the closing one a line of its own.
        assert!(split("name: x\n---\n").is_none());
        assert!(split("---\nname: x\n").is_none());
        assert!(split("---\nname: x\n--- not a fence\n").is_none());
        assert_eq!(split("---\n---\nonly body"), Some(("", "only body")));
    }

    #[test]
    fn scalars_lists_and_comments_are_read() {
        let fields = read(
            "# a comment\nname: reviewer\ndescription: \"A \\\"quoted\\\" line\\twith a tab\"\n\nempty: []\ntools:\n  - read\n  - git\nmore:\n\t- tabbed\n",
            RULES,
        )
        .expect("read");
        assert_eq!(scalar_of(&fields, "name").as_deref(), Some("reviewer"));
        assert_eq!(
            scalar_of(&fields, "description").as_deref(),
            Some("A \"quoted\" line\twith a tab")
        );
        assert_eq!(fields.get("empty"), Some(&Value::List(Vec::new())));
        assert_eq!(
            fields.get("tools"),
            Some(&Value::List(vec!["read".into(), "git".into()]))
        );
        assert_eq!(
            fields.get("more"),
            Some(&Value::List(vec!["tabbed".into()]))
        );
        assert_eq!(
            fields.keys().collect::<Vec<_>>(),
            ["name", "description", "empty", "tools", "more"],
            "the order written"
        );
        // A key with nothing after it is an empty list.
        let fields = read("lonely:\nnext: 1\n", RULES).expect("read");
        assert_eq!(fields.get("lonely"), Some(&Value::List(Vec::new())));
    }

    #[test]
    fn what_is_not_the_subset_is_refused() {
        for bad in [
            "key: [a, b]\n",
            "key: 'single'\n",
            "  indented: 1\n",
            "two words: 1\n",
            "no colon here\n",
            "key: a\nkey: b\n",
            "- stray item\n",
            "key: \"bad \\x escape\"\n",
            "key: a\u{0}b\n",
            "key:\n  - a\n  nested: map\n",
        ] {
            assert_eq!(
                read(bad, RULES).map(|_| ()),
                Err(Stop::Syntax(Syntax::Invalid)),
                "{bad:?}"
            );
        }
        let long = format!("key: {}\n", "x".repeat(2000));
        assert_eq!(
            read(&long, RULES).map(|_| ()),
            Err(Stop::Syntax(Syntax::ScalarTooLarge))
        );
    }

    #[test]
    fn a_one_level_map_with_quoted_keys_is_read_only_when_asked_for() {
        let maps = Rules {
            nested_maps: true,
            ..RULES
        };
        let front = "permission:\n  \"workspace_patch(docs/*)\": ask\n  shell_exec: deny\n  \"a \\\"b\\\"\": ask\nafter: x\n";
        let fields = read(front, maps).expect("read");
        assert_eq!(
            fields.get("permission"),
            Some(&Value::Map(vec![
                ("workspace_patch(docs/*)".into(), "ask".into()),
                ("shell_exec".into(), "deny".into()),
                ("a \"b\"".into(), "ask".into()),
            ]))
        );
        assert_eq!(scalar_of(&fields, "after").as_deref(), Some("x"));
        // Without the rule an indented entry is the same error it always was.
        assert_eq!(
            read(front, RULES).map(|_| ()),
            Err(Stop::Syntax(Syntax::Invalid))
        );
        for bad in [
            "permission:\n  \"unterminated: ask\n",
            "permission:\n  \"a\" ask\n",
            "permission:\n  name:\n",
            "permission:\n  a: x\n  a: y\n",
            "permission:\n  - item\n  a: x\n",
            "permission:\n  a: x\n  - item\n",
            "  orphan: x\n",
            "permission:\n  two words: x\n",
        ] {
            assert_eq!(
                read(bad, maps).map(|_| ()),
                Err(Stop::Syntax(Syntax::Invalid)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_callers_checks_come_when_the_line_and_the_key_are_read() {
        // A refused key stops the read before a later syntax error is reached.
        let mut keys = Vec::new();
        let outcome = decode(
            "name: x\nhooks: y\nbroken line\n",
            RULES,
            &mut |_| Ok(()),
            &mut |key| {
                keys.push(key.to_owned());
                if key == "hooks" {
                    Err("refused")
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(outcome.map(|_| ()), Err(Stop::Caller("refused")));
        assert_eq!(keys, ["name", "hooks"]);
        // The line callback sees every line, from zero, before the line is read.
        let mut lines = Vec::new();
        let _ = decode(
            "a: 1\nb: 2\n\nc: 3\n",
            RULES,
            &mut |index| {
                lines.push(index);
                Ok::<(), ()>(())
            },
            &mut |_| Ok(()),
        );
        assert_eq!(lines, [0, 1, 2, 3]);
        let stopped = decode(
            "a: 1\nb: 2\n",
            RULES,
            &mut |index| if index == 1 { Err("late") } else { Ok(()) },
            &mut |_| Ok(()),
        );
        assert_eq!(stopped.map(|_| ()), Err(Stop::Caller("late")));
    }
}
