//! The project's rule file: `.rapidlm/rules.toml` — allow/ask/deny
//! command rules with inline tests, and the `/rules` writer that records
//! them from an answered approval.
//!
//! A rule is one line of intent: this command shape is fine (allow), this
//! one always asks (ask), this one is forbidden (deny). What makes a rule
//! trustworthy enough to load is its tests: every rule carries `match`
//! subjects it must hit and `not_match` subjects it must miss, evaluated
//! at load time — a rule whose tests fail is a load failure (the run
//! refuses typed, like a bad settings file), never a silently narrower or
//! wider pattern than the one its author believed they wrote.
//!
//! Loaded rules become ordinary `shell_exec(glob)` lattice rules after
//! every settings file's own — an explicit rule always outranks a
//! file-of-rules one. `/rules add <glob> <allow|ask|deny> [justification]`
//! is the smart-approval half: an approval answered "always" proposes the
//! rule, and the human decides whether the project learns it.

use crate::permissions::{RuleEffect, ToolPattern, ToolRule};

/// The rule file's name under the project marker.
pub const RULES_FILE: &str = "rules.toml";
/// Most rules one file may hold.
pub const MAX_RULES: usize = 128;
/// Longest one subject or justification.
pub const MAX_SUBJECT_BYTES: usize = 256;
/// Bound on the file itself.
pub const MAX_RULES_FILE_BYTES: usize = 128 * 1024;

/// One rule as the file writes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRule {
    pub pattern: ToolPattern,
    pub effect: RuleEffect,
    pub justification: Option<String>,
    pub must_match: Vec<String>,
    pub must_not_match: Vec<String>,
}

/// Typed rule-file failure. Names the rule and why; never echoes subjects
/// beyond what the error needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RulesError {
    TooLarge,
    InvalidToml(String),
    InvalidRule {
        index: usize,
        reason: String,
    },
    TooManyRules,
    TestFailed {
        index: usize,
        subject: String,
        expected: &'static str,
    },
}

impl std::fmt::Display for RulesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(f, "rules file exceeds {MAX_RULES_FILE_BYTES} bytes"),
            Self::InvalidToml(reason) => write!(f, "rules file is not valid TOML: {reason}"),
            Self::InvalidRule { index, reason } => {
                write!(f, "rule {index} is invalid: {reason}")
            }
            Self::TooManyRules => write!(f, "rules file exceeds {MAX_RULES} rules"),
            Self::TestFailed {
                index,
                subject,
                expected,
            } => write!(f, "rule {index}'s tests fail: {subject:?} must {expected}"),
        }
    }
}

impl std::error::Error for RulesError {}

/// Parse and validate the rule file body (pure): each rule's `match`
/// subjects must be matched by its pattern, each `not_match` subject must
/// not be, and both are how the file refuses to load a rule whose pattern
/// is not the one its author meant.
pub fn parse_rules(text: &str) -> Result<Vec<FileRule>, RulesError> {
    if text.len() > MAX_RULES_FILE_BYTES {
        return Err(RulesError::TooLarge);
    }
    let parsed: toml::Value =
        toml::from_str(text).map_err(|err| RulesError::InvalidToml(err.message().to_owned()))?;
    let Some(entries) = parsed.get("rule").and_then(toml::Value::as_array) else {
        // An empty (or no-[[rule]]) document is an empty rule set.
        return Ok(Vec::new());
    };
    if entries.len() > MAX_RULES {
        return Err(RulesError::TooManyRules);
    }
    let mut rules = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let invalid = |reason: &str| RulesError::InvalidRule {
            index,
            reason: reason.to_owned(),
        };
        let table = entry
            .as_table()
            .ok_or_else(|| invalid("each [[rule]] must be a table"))?;
        let pattern_text = table
            .get("pattern")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| invalid("pattern is required"))?;
        // The file's patterns are shell-command globs: `shell_exec(...)`
        // wrapping is what the lattice sees, and writing it in the file
        // too would be noise. `tool(glob)` forms still parse for the
        // rare non-shell rule.
        let pattern_text = if pattern_text.contains('(') {
            pattern_text.to_owned()
        } else {
            format!("shell_exec({pattern_text})")
        };
        let pattern = ToolPattern::parse(&pattern_text)
            .ok_or_else(|| invalid(&format!("{pattern_text:?} is not a tool pattern")))?;
        let effect = match table.get("decision").and_then(toml::Value::as_str) {
            Some("allow") => RuleEffect::Allow,
            Some("ask") => RuleEffect::Ask,
            Some("deny") => RuleEffect::Deny,
            other => {
                return Err(invalid(&format!(
                    "decision must be \"allow\", \"ask\" or \"deny\", not {other:?}"
                )));
            }
        };
        let bounded_subject = |value: &toml::Value| -> Option<String> {
            value
                .as_str()
                .filter(|subject| !subject.is_empty() && subject.len() <= MAX_SUBJECT_BYTES)
                .map(str::to_owned)
        };
        let justification = table
            .get("justification")
            .and_then(toml::Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| text.chars().take(256).collect::<String>());
        let mut must_match = Vec::new();
        let mut must_not_match = Vec::new();
        if let Some(tests) = table.get("test").and_then(toml::Value::as_array) {
            if tests.len() > 16 {
                return Err(invalid("at most 16 tests per rule"));
            }
            for test in tests {
                let test = test
                    .as_table()
                    .ok_or_else(|| invalid("each [[rule.test]] must be a table"))?;
                for (key, sink) in [
                    ("match", &mut must_match),
                    ("not_match", &mut must_not_match),
                ] {
                    if let Some(subject) = test.get(key).and_then(bounded_subject) {
                        sink.push(subject);
                    }
                }
            }
        }
        let rule = FileRule {
            pattern,
            effect,
            justification,
            must_match: must_match.clone(),
            must_not_match: must_not_match.clone(),
        };
        // The tests decide whether the pattern is the one its author
        // meant — a rule whose own tests fail never loads. They judge the
        // way the lattice judges: an allow rule is tested by *coverage*
        // (every command of a compound subject must be matched — an allow
        // never smuggles a `&& rm -rf` through), an ask/deny rule by *hit*
        // (matching any one command decides). A subject that cannot be
        // read as a script is judged whole.
        let judged = |subject: &str| -> bool {
            let argv = vec!["sh".to_owned(), "-c".to_owned(), subject.to_owned()];
            let parts = crate::permissions::ShellParts::of(&argv);
            // Coverage is per command only — never the whole subject.
            // The lattice's own `covers` never matches the joined argv
            // for an allow, because the star would span a `&&`'s second
            // command; the rule file's tests must not widen what they
            // came to prove.
            let covers = parts.known
                && !parts.commands.is_empty()
                && parts
                    .commands
                    .iter()
                    .all(|command| rule.pattern.matches("shell_exec", command));
            let hits = rule.pattern.matches("shell_exec", subject)
                || parts
                    .commands
                    .iter()
                    .chain(&parts.wrapped)
                    .any(|command| rule.pattern.matches("shell_exec", command));
            match rule.effect {
                RuleEffect::Allow => covers,
                RuleEffect::Ask | RuleEffect::Deny => hits,
            }
        };
        for subject in &rule.must_match {
            if !judged(subject) {
                return Err(RulesError::TestFailed {
                    index,
                    subject: subject.clone(),
                    expected: "match",
                });
            }
        }
        for subject in &rule.must_not_match {
            if judged(subject) {
                return Err(RulesError::TestFailed {
                    index,
                    subject: subject.clone(),
                    expected: "not match",
                });
            }
        }
        rules.push(rule);
    }
    Ok(rules)
}

/// Load the project's rules from `<root>/.rapidlm/rules.toml`. A missing
/// file is an empty rule set; one that exists and does not validate is an
/// error the caller refuses the run with — a broken rule file must never
/// read as "no rules".
pub fn load_rules(root: &std::path::Path) -> Result<Vec<FileRule>, RulesError> {
    let path = root
        .join(crate::interactive::PROJECT_MARKER)
        .join(RULES_FILE);
    let bytes = std::fs::read(&path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => RulesError::InvalidToml(String::new()),
        _ => RulesError::TooLarge,
    });
    match bytes {
        Err(_) if !path.exists() => Ok(Vec::new()),
        Err(err) => Err(err),
        Ok(bytes) if bytes.len() > MAX_RULES_FILE_BYTES => Err(RulesError::TooLarge),
        Ok(bytes) => parse_rules(&String::from_utf8_lossy(&bytes)),
    }
}

/// The lattice rules the file's rules become, in file order — appended
/// after every settings file's own rules, so an explicit rule always
/// outranks one from the file.
pub fn lattice_rules(rules: &[FileRule]) -> Vec<ToolRule> {
    rules
        .iter()
        .map(|rule| ToolRule {
            effect: rule.effect,
            pattern: rule.pattern.clone(),
        })
        .collect()
}

/// The `[[rule]]` block `/rules add` writes — the same shape the file
/// parses, with the justifying approval as its justification and the
/// approved command as its one test.
pub fn render_rule_block(
    pattern: &str,
    effect: RuleEffect,
    justification: &str,
    example: &str,
) -> String {
    let decision = match effect {
        RuleEffect::Allow => "allow",
        RuleEffect::Ask => "ask",
        RuleEffect::Deny => "deny",
    };
    format!(
        "\n[[rule]]\npattern = {pattern:?}\ndecision = {decision:?}\njustification = {justification:?}\n[[rule.test]]\nmatch = {example:?}\n"
    )
}

/// Append one rule to the project's rules file (creating it when absent),
/// validating the result parses back: a writer that could produce a file
/// its own loader rejects is how a rule "disappears".
pub fn append_rule(
    root: &std::path::Path,
    pattern: &str,
    effect: RuleEffect,
    justification: &str,
    example: &str,
) -> Result<(), RulesError> {
    let path = root
        .join(crate::interactive::PROJECT_MARKER)
        .join(RULES_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| {
            RulesError::InvalidToml("the rules directory could not be created".to_owned())
        })?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.len() > MAX_RULES_FILE_BYTES {
        return Err(RulesError::TooLarge);
    }
    let updated = existing + &render_rule_block(pattern, effect, justification, example);
    parse_rules(&updated)?;
    std::fs::write(&path, updated)
        .map_err(|_| RulesError::InvalidToml("the rules file could not be written".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_parse_with_wrapped_shell_globs_and_validate_their_tests() {
        let doc = r#"
[[rule]]
pattern = "cargo *"
decision = "allow"
justification = "builds are the project's own"
[[rule.test]]
match = "cargo build --release"
[[rule.test]]
match = "cargo test"
not_match = "cargo build && rm -rf /"

[[rule]]
pattern = "git push --force*"
decision = "deny"
justification = "history is shared"
[[rule.test]]
match = "git push --force origin main"
not_match = "git push origin main"
"#;
        let rules = parse_rules(doc).expect("rules");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].pattern.tool(), "shell_exec", "the glob is wrapped");
        assert_eq!(rules[0].effect, RuleEffect::Allow);
        assert_eq!(rules[0].must_match.len(), 2);
        assert_eq!(rules[1].effect, RuleEffect::Deny);

        // The lattice rules carry the effect and the wrapped pattern.
        let lattice = lattice_rules(&rules);
        assert_eq!(lattice[0].pattern.render(), "shell_exec(cargo *)");
        assert_eq!(lattice[1].pattern.render(), "shell_exec(git push --force*)");

        // An empty document is an empty rule set.
        assert!(parse_rules("").expect("empty").is_empty());
    }

    #[test]
    fn a_rule_whose_own_tests_fail_never_loads() {
        // The glob does not cover what the test says it must match.
        let bad_match = r#"
[[rule]]
pattern = "cargo build"
decision = "allow"
[[rule.test]]
match = "cargo test"
"#;
        assert!(matches!(
            parse_rules(bad_match),
            Err(RulesError::TestFailed { index: 0, .. })
        ));

        // And it must miss what the test says it must not match.
        let bad_not_match = r#"
[[rule]]
pattern = "cargo *"
decision = "allow"
[[rule.test]]
not_match = "cargo build"
"#;
        assert!(matches!(
            parse_rules(bad_not_match),
            Err(RulesError::TestFailed {
                expected: "not match",
                ..
            })
        ));

        // Bad decisions and bad patterns are named by index.
        assert!(matches!(
            parse_rules("[[rule]]\npattern = \"x\"\ndecision = \"maybe\"\n"),
            Err(RulesError::InvalidRule { index: 0, .. })
        ));
        assert!(parse_rules("[[rule]]\ndecision = \"allow\"\n").is_err());
    }

    #[test]
    fn appending_a_rule_round_trips_through_the_parser() {
        let dir = std::env::temp_dir().join(format!("rapidlm-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        append_rule(
            &dir,
            "cargo fmt *",
            RuleEffect::Allow,
            "approved in this session",
            "cargo fmt --all",
        )
        .expect("append");
        let rules = load_rules(&dir).expect("reload");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].pattern.render(), "shell_exec(cargo fmt *)");
        assert_eq!(rules[0].must_match, vec!["cargo fmt --all".to_owned()]);

        // A second append lands after the first, and the file still
        // parses whole.
        append_rule(
            &dir,
            "git push --force*",
            RuleEffect::Deny,
            "never",
            "git push --force origin main",
        )
        .expect("append again");
        assert_eq!(load_rules(&dir).expect("reload").len(), 2);

        // The rendered block is exactly the parse's inverse.
        let rendered = render_rule_block(
            "cargo fmt *",
            RuleEffect::Allow,
            "approved in this session",
            "cargo fmt --all",
        );
        let parsed = parse_rules(&rendered).expect("round trip");
        assert_eq!(parsed[0].pattern.render(), "shell_exec(cargo fmt *)");
        std::fs::remove_dir_all(&dir).ok();
    }
}
