//! Bounded Tier-1 parse registry.
//!
//! Every PRD language has a registry entry. Programming languages use Tree-sitter
//! when the matching Cargo feature compiled a grammar; JSON/YAML/TOML/Markdown
//! use an explicit structured-text strategy. Timeout and parse failure return a
//! degraded outcome instead of panicking or inventing a successful tree.

use std::fmt;
use std::time::{Duration, Instant};

use crate::ingest::content::SourceLanguage;
use crate::ingest::walk::DEFAULT_MAX_FILE_BYTES;
use crate::repo_manifest::CancellationToken;

/// Default wall-clock budget for one parse.
pub const DEFAULT_PARSE_TIMEOUT: Duration = Duration::from_millis(250);

/// Default source-byte cap; matches the ingest full-text limit.
pub const DEFAULT_MAX_PARSE_BYTES: usize = DEFAULT_MAX_FILE_BYTES as usize;

/// Default CST node cap. Excess nodes discard the tree and degrade.
pub const DEFAULT_MAX_TREE_NODES: usize = 262_144;

const CANCEL_STRIDE: usize = 16;

/// Per-call resource bounds. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct ParseBudget {
    max_source_bytes: usize,
    max_tree_nodes: usize,
    timeout: Duration,
    cancel: CancellationToken,
}

/// How a language is parsed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ParseStrategy {
    TreeSitter,
    StructuredText,
}

/// Whether the CST/structure is usable at full confidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum StructuralConfidence {
    Full,
    Degraded,
}

/// Why a parse did not produce full structural confidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ParseDegradeReason {
    Cancelled,
    Timeout,
    SourceTooLarge,
    InvalidUtf8,
    ParseError,
    ParserUnavailable,
    TreeTooLarge,
}

/// One registered language and its parse backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ParserEntry {
    language: SourceLanguage,
    strategy: ParseStrategy,
    backend_available: bool,
}

/// Concrete syntax or structured-text result retained for later extraction.
#[derive(Clone, Debug)]
pub struct ParseTree {
    language: SourceLanguage,
    strategy: ParseStrategy,
    node_count: usize,
    has_error: bool,
    tree_sitter: Option<tree_sitter::Tree>,
}

/// Bounded parse result. Errors are outcomes, never panics.
#[derive(Clone, Debug)]
pub struct ParseOutcome {
    language: SourceLanguage,
    strategy: ParseStrategy,
    confidence: StructuralConfidence,
    degrade_reason: Option<ParseDegradeReason>,
    tree: Option<ParseTree>,
}

/// Stateless registry of Tier-1 parsers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ParserRegistry;

impl ParseBudget {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_source_bytes(mut self, value: usize) -> Self {
        self.max_source_bytes = value;
        self
    }

    pub fn max_tree_nodes(mut self, value: usize) -> Self {
        self.max_tree_nodes = value;
        self
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }

    pub fn max_source_bytes_value(&self) -> usize {
        self.max_source_bytes
    }

    pub fn max_tree_nodes_value(&self) -> usize {
        self.max_tree_nodes
    }

    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl Default for ParseBudget {
    fn default() -> Self {
        Self {
            max_source_bytes: DEFAULT_MAX_PARSE_BYTES,
            max_tree_nodes: DEFAULT_MAX_TREE_NODES,
            timeout: DEFAULT_PARSE_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }
}

impl ParseStrategy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TreeSitter => "tree_sitter",
            Self::StructuredText => "structured_text",
        }
    }

    pub const fn for_language(language: SourceLanguage) -> Self {
        match language {
            SourceLanguage::Json
            | SourceLanguage::Yaml
            | SourceLanguage::Toml
            | SourceLanguage::Markdown => Self::StructuredText,
            SourceLanguage::Rust
            | SourceLanguage::TypeScript
            | SourceLanguage::JavaScript
            | SourceLanguage::Python
            | SourceLanguage::Go
            | SourceLanguage::Java
            | SourceLanguage::C
            | SourceLanguage::Cpp
            | SourceLanguage::CSharp
            | SourceLanguage::Kotlin
            | SourceLanguage::Swift
            | SourceLanguage::Ruby
            | SourceLanguage::Bash => Self::TreeSitter,
        }
    }
}

impl fmt::Display for ParseStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ParseDegradeReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::SourceTooLarge => "source_too_large",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::ParseError => "parse_error",
            Self::ParserUnavailable => "parser_unavailable",
            Self::TreeTooLarge => "tree_too_large",
        }
    }
}

impl fmt::Display for ParseDegradeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ParserEntry {
    pub fn language(self) -> SourceLanguage {
        self.language
    }

    pub fn strategy(self) -> ParseStrategy {
        self.strategy
    }

    pub fn backend_available(self) -> bool {
        self.backend_available
    }
}

impl ParseTree {
    pub fn language(&self) -> SourceLanguage {
        self.language
    }

    pub fn strategy(&self) -> ParseStrategy {
        self.strategy
    }

    pub fn node_count(&self) -> usize {
        self.node_count
    }

    pub fn has_error(&self) -> bool {
        self.has_error
    }

    pub(crate) fn tree_sitter(&self) -> Option<&tree_sitter::Tree> {
        self.tree_sitter.as_ref()
    }
}

impl ParseOutcome {
    pub fn language(&self) -> SourceLanguage {
        self.language
    }

    pub fn strategy(&self) -> ParseStrategy {
        self.strategy
    }

    pub fn confidence(&self) -> StructuralConfidence {
        self.confidence
    }

    pub fn degrade_reason(&self) -> Option<ParseDegradeReason> {
        self.degrade_reason
    }

    pub fn tree(&self) -> Option<&ParseTree> {
        self.tree.as_ref()
    }

    pub fn has_error(&self) -> bool {
        match &self.tree {
            Some(tree) => tree.has_error,
            None => self.degrade_reason == Some(ParseDegradeReason::ParseError),
        }
    }
}

impl ParserRegistry {
    pub fn new() -> Self {
        Self
    }

    /// Registry row for one Tier-1 language. Always present.
    pub fn entry(language: SourceLanguage) -> ParserEntry {
        ParserEntry {
            language,
            strategy: ParseStrategy::for_language(language),
            backend_available: backend_available(language),
        }
    }

    pub fn entries() -> impl Iterator<Item = ParserEntry> {
        SourceLanguage::ALL.iter().copied().map(Self::entry)
    }

    /// Parse `bytes` with a registered strategy. Timeout and backend failure
    /// degrade; they never panic.
    pub fn parse(language: SourceLanguage, bytes: &[u8], budget: &ParseBudget) -> ParseOutcome {
        let started = Instant::now();
        let strategy = ParseStrategy::for_language(language);
        if let Some(reason) = preflight(bytes, budget) {
            return degraded(language, strategy, reason);
        }
        match strategy {
            ParseStrategy::TreeSitter => parse_tree_sitter(language, bytes, budget, started),
            ParseStrategy::StructuredText => {
                parse_structured_text(language, bytes, budget, started)
            }
        }
    }
}

fn preflight(bytes: &[u8], budget: &ParseBudget) -> Option<ParseDegradeReason> {
    if budget.cancel.is_cancelled() {
        return Some(ParseDegradeReason::Cancelled);
    }
    if budget.timeout.is_zero() {
        return Some(ParseDegradeReason::Timeout);
    }
    if bytes.len() > budget.max_source_bytes {
        return Some(ParseDegradeReason::SourceTooLarge);
    }
    if std::str::from_utf8(bytes).is_err() {
        return Some(ParseDegradeReason::InvalidUtf8);
    }
    None
}

fn budget_exhausted_after(started: Instant, budget: &ParseBudget) -> Option<ParseDegradeReason> {
    if budget.cancel.is_cancelled() {
        return Some(ParseDegradeReason::Cancelled);
    }
    if started.elapsed() > budget.timeout {
        return Some(ParseDegradeReason::Timeout);
    }
    None
}

fn parse_tree_sitter(
    language: SourceLanguage,
    bytes: &[u8],
    budget: &ParseBudget,
    started: Instant,
) -> ParseOutcome {
    let strategy = ParseStrategy::TreeSitter;
    let Some(grammar) = tree_sitter_grammar(language) else {
        return degraded(language, strategy, ParseDegradeReason::ParserUnavailable);
    };
    if let Some(reason) = budget_exhausted_after(started, budget) {
        return degraded(language, strategy, reason);
    }

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&grammar).is_err() {
        return degraded(language, strategy, ParseDegradeReason::ParserUnavailable);
    }
    let timeout_micros = u64::try_from(budget.timeout.as_micros()).unwrap_or(u64::MAX);
    parser.set_timeout_micros(timeout_micros.max(1));

    let Some(tree) = parser.parse(bytes, None) else {
        return degraded(language, strategy, ParseDegradeReason::Timeout);
    };
    if let Some(reason) = budget_exhausted_after(started, budget) {
        return degraded(language, strategy, reason);
    }

    let root = tree.root_node();
    let node_count = root.descendant_count();
    if node_count > budget.max_tree_nodes {
        return degraded(language, strategy, ParseDegradeReason::TreeTooLarge);
    }
    let has_error = root.has_error();
    let parsed = ParseTree {
        language,
        strategy,
        node_count,
        has_error,
        tree_sitter: Some(tree),
    };
    if has_error {
        ParseOutcome {
            language,
            strategy,
            confidence: StructuralConfidence::Degraded,
            degrade_reason: Some(ParseDegradeReason::ParseError),
            tree: Some(parsed),
        }
    } else {
        full(parsed)
    }
}

fn parse_structured_text(
    language: SourceLanguage,
    bytes: &[u8],
    budget: &ParseBudget,
    started: Instant,
) -> ParseOutcome {
    let strategy = ParseStrategy::StructuredText;
    // UTF-8 was accepted in preflight; this only reborrows.
    let Ok(text) = std::str::from_utf8(bytes) else {
        return degraded(language, strategy, ParseDegradeReason::InvalidUtf8);
    };
    if let Some(reason) = budget_exhausted_after(started, budget) {
        return degraded(language, strategy, reason);
    }

    let valid = match language {
        SourceLanguage::Json => json_structure_valid(bytes),
        SourceLanguage::Toml => toml_structure_valid(text),
        SourceLanguage::Yaml => yaml_structure_valid(text, budget),
        SourceLanguage::Markdown => markdown_structure_valid(text, budget),
        _ => false,
    };
    if let Some(reason) = budget_exhausted_after(started, budget) {
        return degraded(language, strategy, reason);
    }
    if !valid {
        return degraded(language, strategy, ParseDegradeReason::ParseError);
    }
    full(ParseTree {
        language,
        strategy,
        node_count: 1,
        has_error: false,
        tree_sitter: None,
    })
}

fn json_structure_valid(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde::de::IgnoredAny>(bytes).is_ok()
}

fn toml_structure_valid(text: &str) -> bool {
    toml::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

fn yaml_structure_valid(text: &str, budget: &ParseBudget) -> bool {
    // YAML 1.2 is not reimplemented here. The structured-text strategy accepts
    // UTF-8 documents that stay inside the indent/flow-depth bound.
    bounded_text_scan(text, budget, 64)
}

fn markdown_structure_valid(text: &str, budget: &ParseBudget) -> bool {
    bounded_text_scan(text, budget, 128)
}

fn bounded_text_scan(text: &str, budget: &ParseBudget, max_flow_depth: usize) -> bool {
    let mut depth = 0usize;
    for (i, ch) in text.chars().enumerate() {
        if i == 0 || i.is_multiple_of(CANCEL_STRIDE) {
            if budget.cancel.is_cancelled() {
                return false;
            }
        }
        match ch {
            '{' | '[' => {
                depth = depth.saturating_add(1);
                if depth > max_flow_depth {
                    return false;
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    true
}

fn backend_available(language: SourceLanguage) -> bool {
    match ParseStrategy::for_language(language) {
        ParseStrategy::StructuredText => true,
        ParseStrategy::TreeSitter => tree_sitter_grammar(language).is_some(),
    }
}

macro_rules! cfg_language {
    ($feature:literal, $lang:expr) => {{
        #[cfg(feature = $feature)]
        {
            Some(tree_sitter::Language::from($lang))
        }
        #[cfg(not(feature = $feature))]
        {
            None
        }
    }};
}

fn tree_sitter_grammar(language: SourceLanguage) -> Option<tree_sitter::Language> {
    match language {
        SourceLanguage::Rust => cfg_language!("lang-rust", tree_sitter_rust::LANGUAGE),
        SourceLanguage::TypeScript => {
            cfg_language!(
                "lang-typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT
            )
        }
        SourceLanguage::JavaScript => {
            cfg_language!("lang-javascript", tree_sitter_javascript::LANGUAGE)
        }
        SourceLanguage::Python => cfg_language!("lang-python", tree_sitter_python::LANGUAGE),
        SourceLanguage::Go => cfg_language!("lang-go", tree_sitter_go::LANGUAGE),
        SourceLanguage::Java
        | SourceLanguage::C
        | SourceLanguage::Cpp
        | SourceLanguage::CSharp
        | SourceLanguage::Kotlin
        | SourceLanguage::Swift
        | SourceLanguage::Ruby
        | SourceLanguage::Bash
        | SourceLanguage::Json
        | SourceLanguage::Yaml
        | SourceLanguage::Toml
        | SourceLanguage::Markdown => None,
    }
}

fn full(tree: ParseTree) -> ParseOutcome {
    ParseOutcome {
        language: tree.language,
        strategy: tree.strategy,
        confidence: StructuralConfidence::Full,
        degrade_reason: None,
        tree: Some(tree),
    }
}

fn degraded(
    language: SourceLanguage,
    strategy: ParseStrategy,
    reason: ParseDegradeReason,
) -> ParseOutcome {
    ParseOutcome {
        language,
        strategy,
        confidence: StructuralConfidence::Degraded,
        degrade_reason: Some(reason),
        tree: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(language: SourceLanguage, bytes: &[u8]) -> ParseOutcome {
        ParserRegistry::parse(language, bytes, &ParseBudget::new())
    }

    #[test]
    fn all_tier1_languages_have_registry_entries() {
        let mut seen = 0usize;
        for language in SourceLanguage::ALL {
            let entry = ParserRegistry::entry(*language);
            assert_eq!(entry.language(), *language);
            let expected = match language {
                SourceLanguage::Json
                | SourceLanguage::Yaml
                | SourceLanguage::Toml
                | SourceLanguage::Markdown => ParseStrategy::StructuredText,
                _ => ParseStrategy::TreeSitter,
            };
            assert_eq!(entry.strategy(), expected, "{}", language.as_str());
            seen += 1;
        }
        assert_eq!(seen, SourceLanguage::ALL.len());
        assert_eq!(ParserRegistry::entries().count(), SourceLanguage::ALL.len());
    }

    #[test]
    fn structured_text_strategy_covers_json_yaml_toml_markdown() {
        for language in [
            SourceLanguage::Json,
            SourceLanguage::Yaml,
            SourceLanguage::Toml,
            SourceLanguage::Markdown,
        ] {
            let entry = ParserRegistry::entry(language);
            assert_eq!(entry.strategy(), ParseStrategy::StructuredText);
            assert!(entry.backend_available());
        }
    }

    #[test]
    fn rust_parse_returns_full_tree() {
        let outcome = parse(SourceLanguage::Rust, b"fn main() {}\n");
        assert_eq!(outcome.language(), SourceLanguage::Rust);
        assert_eq!(outcome.strategy(), ParseStrategy::TreeSitter);
        assert_eq!(outcome.confidence(), StructuralConfidence::Full);
        assert_eq!(outcome.degrade_reason(), None);
        let tree = outcome.tree().expect("tree");
        assert!(!tree.has_error());
        assert!(tree.node_count() > 0);
        assert!(tree.tree_sitter().is_some());
    }

    #[test]
    fn typescript_javascript_python_go_parse_when_featured() {
        let cases: &[(SourceLanguage, &[u8])] = &[
            (
                SourceLanguage::TypeScript,
                b"export function add(a: number, b: number): number { return a + b; }\n",
            ),
            (
                SourceLanguage::JavaScript,
                b"export function add(a, b) { return a + b; }\n",
            ),
            (
                SourceLanguage::Python,
                b"def add(a, b):\n    return a + b\n",
            ),
            (
                SourceLanguage::Go,
                b"package main\nfunc Add(a, b int) int { return a + b }\n",
            ),
        ];
        for (language, src) in cases {
            let outcome = parse(*language, src);
            assert!(
                ParserRegistry::entry(*language).backend_available(),
                "{}",
                language.as_str()
            );
            assert_eq!(
                outcome.confidence(),
                StructuralConfidence::Full,
                "{}",
                language.as_str()
            );
            assert!(outcome.tree().is_some());
        }
    }

    #[test]
    fn malformed_rust_is_degraded_with_partial_tree() {
        let outcome = parse(SourceLanguage::Rust, b"fn main( {\n");
        assert_eq!(outcome.confidence(), StructuralConfidence::Degraded);
        assert_eq!(
            outcome.degrade_reason(),
            Some(ParseDegradeReason::ParseError)
        );
        let tree = outcome.tree().expect("partial tree");
        assert!(tree.has_error());
        assert!(outcome.has_error());
    }

    #[test]
    fn zero_timeout_degrades_without_panic() {
        let outcome = ParserRegistry::parse(
            SourceLanguage::Rust,
            b"fn main() {}\n",
            &ParseBudget::new().timeout(Duration::ZERO),
        );
        assert_eq!(outcome.confidence(), StructuralConfidence::Degraded);
        assert_eq!(outcome.degrade_reason(), Some(ParseDegradeReason::Timeout));
        assert!(outcome.tree().is_none());
    }

    #[test]
    fn cancelled_parse_degrades() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = ParserRegistry::parse(
            SourceLanguage::Rust,
            b"fn main() {}\n",
            &ParseBudget::new().cancellation(cancel),
        );
        assert_eq!(outcome.confidence(), StructuralConfidence::Degraded);
        assert_eq!(
            outcome.degrade_reason(),
            Some(ParseDegradeReason::Cancelled)
        );
        assert_eq!(ParseDegradeReason::Cancelled.to_string(), "cancelled");
        assert!(
            !outcome
                .degrade_reason()
                .unwrap()
                .to_string()
                .contains("main")
        );
    }

    #[test]
    fn oversized_source_degrades() {
        let outcome = ParserRegistry::parse(
            SourceLanguage::Rust,
            b"fn main() {}\n",
            &ParseBudget::new().max_source_bytes(4),
        );
        assert_eq!(
            outcome.degrade_reason(),
            Some(ParseDegradeReason::SourceTooLarge)
        );
        assert!(outcome.tree().is_none());
    }

    #[test]
    fn invalid_utf8_degrades() {
        let outcome = parse(SourceLanguage::Rust, b"fn main() { \xff }");
        assert_eq!(
            outcome.degrade_reason(),
            Some(ParseDegradeReason::InvalidUtf8)
        );
        assert!(outcome.tree().is_none());
    }

    #[test]
    fn missing_tree_sitter_backend_is_explicit_unavailable() {
        for language in [
            SourceLanguage::Java,
            SourceLanguage::C,
            SourceLanguage::Cpp,
            SourceLanguage::CSharp,
            SourceLanguage::Kotlin,
            SourceLanguage::Swift,
            SourceLanguage::Ruby,
            SourceLanguage::Bash,
        ] {
            let entry = ParserRegistry::entry(language);
            assert_eq!(entry.strategy(), ParseStrategy::TreeSitter);
            assert!(!entry.backend_available());
            let outcome = parse(language, b"class A {}\n");
            assert_eq!(outcome.confidence(), StructuralConfidence::Degraded);
            assert_eq!(
                outcome.degrade_reason(),
                Some(ParseDegradeReason::ParserUnavailable)
            );
            assert!(outcome.tree().is_none());
        }
    }

    #[test]
    fn json_and_toml_structured_text_validate() {
        let json_ok = parse(SourceLanguage::Json, br#"{"a":1}"#);
        assert_eq!(json_ok.confidence(), StructuralConfidence::Full);
        assert_eq!(json_ok.strategy(), ParseStrategy::StructuredText);
        assert!(json_ok.tree().is_some());

        let json_bad = parse(SourceLanguage::Json, b"{");
        assert_eq!(
            json_bad.degrade_reason(),
            Some(ParseDegradeReason::ParseError)
        );
        assert!(json_bad.tree().is_none());

        let toml_ok = parse(SourceLanguage::Toml, b"a = 1\n");
        assert_eq!(toml_ok.confidence(), StructuralConfidence::Full);

        let toml_bad = parse(SourceLanguage::Toml, b"a = [\n");
        assert_eq!(
            toml_bad.degrade_reason(),
            Some(ParseDegradeReason::ParseError)
        );
    }

    #[test]
    fn yaml_and_markdown_use_structured_text() {
        let yaml = parse(SourceLanguage::Yaml, b"a: 1\n");
        assert_eq!(yaml.strategy(), ParseStrategy::StructuredText);
        assert_eq!(yaml.confidence(), StructuralConfidence::Full);

        let md = parse(SourceLanguage::Markdown, b"# Title\n\nparagraph\n");
        assert_eq!(md.strategy(), ParseStrategy::StructuredText);
        assert_eq!(md.confidence(), StructuralConfidence::Full);
    }

    #[test]
    fn node_budget_degrades_without_retaining_tree() {
        let outcome = ParserRegistry::parse(
            SourceLanguage::Rust,
            b"fn main() { let x = 1; }\n",
            &ParseBudget::new().max_tree_nodes(1),
        );
        assert_eq!(
            outcome.degrade_reason(),
            Some(ParseDegradeReason::TreeTooLarge)
        );
        assert!(outcome.tree().is_none());
    }

    #[test]
    fn degrade_messages_do_not_echo_source() {
        let outcome = parse(SourceLanguage::Json, b"{\"secret\": true");
        let reason = outcome.degrade_reason().expect("reason");
        let text = reason.to_string();
        assert_eq!(text, "parse_error");
        assert!(!text.contains("secret"));
    }
}
