//! Syntax-fact extraction from bounded parse trees.
//!
//! Definitions, imports, and call-like references become [`SymbolRecord`]s with
//! structural locators. Extraction is cooperative and fact-capped; malformed
//! trees yield the facts that remain rather than failing closed.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::RepoPath;
use tree_sitter::Node;

use crate::ingest::content::SourceLanguage;
use crate::parse::registry::{
    DEFAULT_MAX_PARSE_BYTES, DEFAULT_PARSE_TIMEOUT, ParseBudget, ParseOutcome, ParseTree,
};
use crate::repo_manifest::CancellationToken;

/// Default wall-clock budget for one extraction.
pub const DEFAULT_EXTRACT_TIMEOUT: Duration = DEFAULT_PARSE_TIMEOUT;

/// Default source-byte cap; matches the parse ingest limit.
pub const DEFAULT_MAX_EXTRACT_BYTES: usize = DEFAULT_MAX_PARSE_BYTES;

/// Default maximum emitted facts, including references.
pub const DEFAULT_MAX_SYMBOLS: usize = 4_096;

/// Default maximum call-like reference facts inside [`DEFAULT_MAX_SYMBOLS`].
pub const DEFAULT_MAX_REFERENCES: usize = 1_024;

/// Default UTF-8 byte cap for a stored name, signature, or locator fragment.
pub const DEFAULT_MAX_NAME_BYTES: usize = 256;

/// Default CST walk depth. Deeper nodes are skipped, not panicked on.
pub const DEFAULT_MAX_WALK_DEPTH: usize = 256;

const CANCEL_STRIDE: usize = 16;
const FQ_SEPARATOR: &str = "::";

/// Per-call extraction bounds. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct SymbolBudget {
    max_source_bytes: usize,
    max_symbols: usize,
    max_references: usize,
    max_name_bytes: usize,
    max_walk_depth: usize,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Syntax-fact classification stored on each record.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SymbolKind {
    Module,
    Class,
    Interface,
    Type,
    Impl,
    Function,
    Method,
    Const,
    Macro,
    Import,
    Reference,
}

/// Half-open byte/line span. Lines are 0-based, matching the CST.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SourceRange {
    start_byte: u32,
    end_byte: u32,
    start_line: u32,
    end_line: u32,
}

/// One definition, import, or reference-ish fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolRecord {
    kind: SymbolKind,
    name: String,
    fq_name: String,
    locator: String,
    range: SourceRange,
    container: Option<String>,
    signature: Option<String>,
}

/// Typed extraction failure. Display never echoes source or host paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolError {
    Cancelled,
    Timeout,
    InvalidUtf8,
    SourceTooLarge,
}

#[derive(Clone, Copy)]
struct FactClass {
    kind: SymbolKind,
    container: bool,
}

struct ExtractCtx<'a> {
    path: &'a RepoPath,
    source: &'a [u8],
    budget: &'a SymbolBudget,
    started: Instant,
    visited: usize,
    references: usize,
    containers: Vec<(SymbolKind, String)>,
    seen: HashMap<(SymbolKind, String), u32>,
    out: Vec<SymbolRecord>,
}

impl SymbolBudget {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_source_bytes(mut self, value: usize) -> Self {
        self.max_source_bytes = value;
        self
    }

    pub fn max_symbols(mut self, value: usize) -> Self {
        self.max_symbols = value;
        self
    }

    pub fn max_references(mut self, value: usize) -> Self {
        self.max_references = value;
        self
    }

    pub fn max_name_bytes(mut self, value: usize) -> Self {
        self.max_name_bytes = value;
        self
    }

    pub fn max_walk_depth(mut self, value: usize) -> Self {
        self.max_walk_depth = value;
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

    pub fn max_symbols_value(&self) -> usize {
        self.max_symbols
    }

    pub fn max_references_value(&self) -> usize {
        self.max_references
    }

    pub fn max_name_bytes_value(&self) -> usize {
        self.max_name_bytes
    }

    pub fn max_walk_depth_value(&self) -> usize {
        self.max_walk_depth
    }

    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Copy timeout and cancellation from a parse budget; keep extraction caps.
    pub fn from_parse_budget(parse: &ParseBudget) -> Self {
        Self::new()
            .timeout(parse.timeout_value())
            .cancellation(parse.cancellation_token().clone())
            .max_source_bytes(parse.max_source_bytes_value())
    }
}

impl Default for SymbolBudget {
    fn default() -> Self {
        Self {
            max_source_bytes: DEFAULT_MAX_EXTRACT_BYTES,
            max_symbols: DEFAULT_MAX_SYMBOLS,
            max_references: DEFAULT_MAX_REFERENCES,
            max_name_bytes: DEFAULT_MAX_NAME_BYTES,
            max_walk_depth: DEFAULT_MAX_WALK_DEPTH,
            timeout: DEFAULT_EXTRACT_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }
}

impl SymbolKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Type => "type",
            Self::Impl => "impl",
            Self::Function => "function",
            Self::Method => "method",
            Self::Const => "const",
            Self::Macro => "macro",
            Self::Import => "import",
            Self::Reference => "reference",
        }
    }
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl SourceRange {
    pub fn start_byte(self) -> u32 {
        self.start_byte
    }

    pub fn end_byte(self) -> u32 {
        self.end_byte
    }

    pub fn start_line(self) -> u32 {
        self.start_line
    }

    pub fn end_line(self) -> u32 {
        self.end_line
    }
}

impl SymbolRecord {
    pub fn kind(&self) -> SymbolKind {
        self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn fq_name(&self) -> &str {
        &self.fq_name
    }

    pub fn locator(&self) -> &str {
        &self.locator
    }

    pub fn range(&self) -> SourceRange {
        self.range
    }

    pub fn container(&self) -> Option<&str> {
        self.container.as_deref()
    }

    pub fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }
}

impl SymbolError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::SourceTooLarge => "source_too_large",
        }
    }
}

impl fmt::Display for SymbolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for SymbolError {}

/// Extract definitions/imports/references-ish facts from a retained parse tree.
pub fn extract_symbols(
    tree: &ParseTree,
    path: &RepoPath,
    source: &[u8],
    budget: &SymbolBudget,
) -> Result<Vec<SymbolRecord>, SymbolError> {
    extract_language(tree.language(), Some(tree), path, source, budget)
}

/// Extract from a parse outcome. Missing trees still yield source-level facts
/// for languages that have a bounded fallback (Java).
pub fn extract_from_parse(
    outcome: &ParseOutcome,
    path: &RepoPath,
    source: &[u8],
    budget: &SymbolBudget,
) -> Result<Vec<SymbolRecord>, SymbolError> {
    extract_language(outcome.language(), outcome.tree(), path, source, budget)
}

fn extract_language(
    language: SourceLanguage,
    tree: Option<&ParseTree>,
    path: &RepoPath,
    source: &[u8],
    budget: &SymbolBudget,
) -> Result<Vec<SymbolRecord>, SymbolError> {
    if budget.cancel.is_cancelled() {
        return Err(SymbolError::Cancelled);
    }
    if budget.timeout.is_zero() {
        return Err(SymbolError::Timeout);
    }
    if std::str::from_utf8(source).is_err() {
        return Err(SymbolError::InvalidUtf8);
    }
    if source.len() > budget.max_source_bytes {
        return Err(SymbolError::SourceTooLarge);
    }

    let started = Instant::now();
    let mut ctx = ExtractCtx {
        path,
        source,
        budget,
        started,
        visited: 0,
        references: 0,
        containers: Vec::new(),
        seen: HashMap::new(),
        out: Vec::new(),
    };

    if let Some(ts) = tree.and_then(ParseTree::tree_sitter) {
        walk_node(ts.root_node(), 0, &mut ctx)?;
    } else {
        extract_without_cst(language, &mut ctx)?;
    }
    Ok(ctx.out)
}

fn walk_node(node: Node<'_>, depth: usize, ctx: &mut ExtractCtx<'_>) -> Result<(), SymbolError> {
    ctx.bump()?;
    if ctx.full() {
        return Ok(());
    }
    if depth > ctx.budget.max_walk_depth {
        return Ok(());
    }
    if node.is_error() || node.is_missing() {
        return walk_children(node, depth, ctx);
    }

    if let Some(class) = classify(node.kind()) {
        let kind = specialize_kind(class.kind, ctx);
        if kind == SymbolKind::Reference && ctx.references >= ctx.budget.max_references {
            return walk_children(node, depth, ctx);
        }
        if let Some(raw_name) = fact_name(node, ctx.source, ctx.budget.max_name_bytes) {
            let emitted = emit(
                ctx,
                kind,
                &raw_name,
                range_of(node),
                signature_of(node, ctx),
            );
            if emitted && class.container {
                ctx.containers.push((kind, raw_name));
                let result = walk_children(node, depth, ctx);
                ctx.containers.pop();
                return result;
            }
        }
    }
    walk_children(node, depth, ctx)
}

fn walk_children(
    node: Node<'_>,
    depth: usize,
    ctx: &mut ExtractCtx<'_>,
) -> Result<(), SymbolError> {
    let count = node.named_child_count();
    for i in 0..count {
        if ctx.full() {
            return Ok(());
        }
        if let Some(child) = node.named_child(i) {
            walk_node(child, depth.saturating_add(1), ctx)?;
        }
    }
    Ok(())
}

fn classify(kind: &str) -> Option<FactClass> {
    let (kind, container) = match kind {
        "function_item"
        | "function_declaration"
        | "function_definition"
        | "function_signature"
        | "generator_function"
        | "generator_function_declaration" => (SymbolKind::Function, true),
        "method_definition"
        | "method_declaration"
        | "method_signature"
        | "abstract_method_signature" => (SymbolKind::Method, true),
        "struct_item"
        | "class_declaration"
        | "class_definition"
        | "class"
        | "enum_item"
        | "enum_declaration"
        | "union_item"
        | "abstract_class_declaration"
        | "record_declaration" => (SymbolKind::Class, true),
        "trait_item" | "interface_declaration" => (SymbolKind::Interface, true),
        "type_item" | "type_alias_declaration" | "type_spec" => (SymbolKind::Type, true),
        "mod_item" | "module" | "package_clause" => (SymbolKind::Module, true),
        "impl_item" => (SymbolKind::Impl, true),
        "const_item" | "static_item" | "const_declaration" => (SymbolKind::Const, false),
        "macro_definition" => (SymbolKind::Macro, true),
        "use_declaration" | "import_statement" | "import_from_statement" | "import_spec" => {
            (SymbolKind::Import, false)
        }
        "call_expression" | "call" | "macro_invocation" | "new_expression" => {
            (SymbolKind::Reference, false)
        }
        _ => return None,
    };
    Some(FactClass { kind, container })
}

fn specialize_kind(kind: SymbolKind, ctx: &ExtractCtx<'_>) -> SymbolKind {
    if kind != SymbolKind::Function {
        return kind;
    }
    match ctx.containers.last().map(|(kind, _)| *kind) {
        Some(SymbolKind::Class | SymbolKind::Interface | SymbolKind::Impl | SymbolKind::Type) => {
            SymbolKind::Method
        }
        _ => kind,
    }
}

fn fact_name(node: Node<'_>, source: &[u8], max_bytes: usize) -> Option<String> {
    let raw = match node.kind() {
        "impl_item" => field_text(node, "type", source),
        "use_declaration" => field_text(node, "argument", source),
        "import_statement" | "import_from_statement" => {
            field_text(node, "source", source).or_else(|| compact_node_text(node, source))
        }
        "import_spec" => field_text(node, "path", source)
            .or_else(|| field_text(node, "name", source))
            .or_else(|| compact_node_text(node, source)),
        "package_clause" => {
            named_descendant_text(node, &["package_identifier", "identifier"], source)
        }
        "call_expression" | "call" => call_name(node, source),
        "new_expression" => field_text(node, "constructor", source),
        "macro_invocation" => field_text(node, "macro", source),
        _ => field_text(node, "name", source),
    }?;
    let name = if matches!(
        node.kind(),
        "call_expression" | "call" | "new_expression" | "macro_invocation"
    ) {
        primary_name(&raw).to_string()
    } else {
        raw
    };
    let name = name.trim();
    if name.is_empty() {
        None
    } else {
        Some(truncate_chars(name, max_bytes))
    }
}

fn call_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    if let Some(func) = node.child_by_field_name("function") {
        if let Some(field) = func.child_by_field_name("field") {
            return node_text(field, source);
        }
        if let Some(attr) = func.child_by_field_name("attribute") {
            return node_text(attr, source);
        }
        return node_text(func, source);
    }
    None
}

fn signature_of(node: Node<'_>, ctx: &ExtractCtx<'_>) -> Option<String> {
    let text = match node.kind() {
        "function_item"
        | "function_declaration"
        | "function_definition"
        | "function_signature"
        | "method_definition"
        | "method_declaration"
        | "method_signature"
        | "abstract_method_signature"
        | "generator_function"
        | "generator_function_declaration" => field_text(node, "parameters", ctx.source),
        "impl_item" => field_text(node, "trait", ctx.source),
        _ => None,
    }?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(truncate_chars(trimmed, ctx.budget.max_name_bytes))
    }
}

fn emit(
    ctx: &mut ExtractCtx<'_>,
    kind: SymbolKind,
    name: &str,
    range: SourceRange,
    signature: Option<String>,
) -> bool {
    if ctx.full() {
        return false;
    }
    if kind == SymbolKind::Reference {
        if ctx.references >= ctx.budget.max_references {
            return false;
        }
        ctx.references = ctx.references.saturating_add(1);
    }

    let container = if ctx.containers.is_empty() {
        None
    } else {
        Some(
            ctx.containers
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>()
                .join(FQ_SEPARATOR),
        )
    };
    let fq_name = match &container {
        Some(parent) => {
            let mut fq = String::with_capacity(parent.len() + FQ_SEPARATOR.len() + name.len());
            fq.push_str(parent);
            fq.push_str(FQ_SEPARATOR);
            fq.push_str(name);
            fq
        }
        None => name.to_string(),
    };
    let occurrence = {
        let key = (kind, fq_name.clone());
        let slot = ctx.seen.entry(key).or_insert(0);
        let n = *slot;
        *slot = slot.saturating_add(1);
        n
    };
    let locator = build_locator(
        ctx.path,
        kind,
        &fq_name,
        occurrence,
        ctx.budget.max_name_bytes,
    );
    ctx.out.push(SymbolRecord {
        kind,
        name: name.to_string(),
        fq_name,
        locator,
        range,
        container,
        signature,
    });
    true
}

fn build_locator(
    path: &RepoPath,
    kind: SymbolKind,
    fq_name: &str,
    occurrence: u32,
    max_name_bytes: usize,
) -> String {
    let mut locator = String::new();
    locator.push_str(path.as_str());
    locator.push('#');
    locator.push_str(kind.as_str());
    locator.push(':');
    locator.push_str(fq_name);
    if occurrence > 0 {
        locator.push('#');
        locator.push_str(&occurrence.to_string());
    }
    truncate_chars(&locator, max_name_bytes.saturating_mul(4).max(64))
}

fn extract_without_cst(
    language: SourceLanguage,
    ctx: &mut ExtractCtx<'_>,
) -> Result<(), SymbolError> {
    match language {
        SourceLanguage::Java | SourceLanguage::Kotlin => extract_jvm_source(ctx),
        _ => Ok(()),
    }
}

/// Bounded Java/Kotlin definition and import scan used when no CST was retained.
fn extract_jvm_source(ctx: &mut ExtractCtx<'_>) -> Result<(), SymbolError> {
    let text = match std::str::from_utf8(ctx.source) {
        Ok(text) => text,
        Err(_) => return Err(SymbolError::InvalidUtf8),
    };
    let tokens = jvm_tokens(text, ctx)?;
    let mut i = 0usize;
    let mut depth = 0usize;
    let mut type_depth: Option<usize> = None;
    let mut method_depth: Option<usize> = None;

    while i < tokens.len() {
        ctx.bump()?;
        if ctx.full() {
            break;
        }
        match tokens[i].kind {
            JvmTokKind::LBrace => {
                depth = depth.saturating_add(1);
                i += 1;
            }
            JvmTokKind::RBrace => {
                if type_depth == Some(depth) {
                    ctx.containers.pop();
                    type_depth = if ctx.containers.is_empty() {
                        None
                    } else {
                        Some(depth.saturating_sub(1))
                    };
                }
                if method_depth == Some(depth) {
                    method_depth = None;
                }
                depth = depth.saturating_sub(1);
                i += 1;
            }
            JvmTokKind::Ident => {
                let ident = tokens[i].text;
                if ident == "package" {
                    if let Some((name, next, range)) = dotted_name(&tokens, i + 1) {
                        emit(ctx, SymbolKind::Module, &name, range, None);
                        i = next;
                        continue;
                    }
                } else if ident == "import" {
                    let start =
                        if matches!(tokens.get(i + 1).map(|t| t.kind), Some(JvmTokKind::Ident))
                            && tokens.get(i + 1).map(|t| t.text) == Some("static")
                        {
                            i + 2
                        } else {
                            i + 1
                        };
                    if let Some((name, next, range)) = dotted_name(&tokens, start) {
                        emit(ctx, SymbolKind::Import, &name, range, None);
                        i = next;
                        continue;
                    }
                } else if matches!(ident, "class" | "interface" | "enum" | "record") {
                    if let Some(name_tok) =
                        tokens.get(i + 1).filter(|t| t.kind == JvmTokKind::Ident)
                    {
                        let kind = if ident == "interface" {
                            SymbolKind::Interface
                        } else {
                            SymbolKind::Class
                        };
                        if emit(ctx, kind, name_tok.text, name_tok.range, None) {
                            ctx.containers.push((kind, name_tok.text.to_string()));
                            type_depth = Some(depth.saturating_add(1));
                        }
                        i += 2;
                        continue;
                    }
                } else if type_depth == Some(depth) && method_depth.is_none() {
                    if tokens
                        .get(i + 1)
                        .is_some_and(|t| t.kind == JvmTokKind::LParen)
                        && !jvm_keyword(ident)
                    {
                        if emit(ctx, SymbolKind::Method, ident, tokens[i].range, None) {
                            method_depth = Some(depth.saturating_add(1));
                        }
                        i += 2;
                        continue;
                    }
                } else if method_depth.is_some()
                    && tokens
                        .get(i + 1)
                        .is_some_and(|t| t.kind == JvmTokKind::LParen)
                    && !jvm_keyword(ident)
                {
                    emit(ctx, SymbolKind::Reference, ident, tokens[i].range, None);
                    i += 2;
                    continue;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JvmTokKind {
    Ident,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Dot,
    Star,
    Semi,
    Other,
}

struct JvmTok<'a> {
    kind: JvmTokKind,
    text: &'a str,
    range: SourceRange,
}

fn jvm_tokens<'a>(text: &'a str, ctx: &mut ExtractCtx<'_>) -> Result<Vec<JvmTok<'a>>, SymbolError> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line = 0u32;
    while i < bytes.len() {
        ctx.bump()?;
        let start = i;
        let start_line = line;
        let b = bytes[i];
        match b {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    if bytes[i] == b'\n' {
                        line = line.saturating_add(1);
                    }
                    i += 1;
                }
                i = i.saturating_add(2).min(bytes.len());
            }
            b'"' | b'\'' => {
                let quote = b;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\n' {
                        line = line.saturating_add(1);
                    }
                    if bytes[i] == b'\\' {
                        i = i.saturating_add(2);
                        continue;
                    }
                    if bytes[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'{' => {
                out.push(JvmTok {
                    kind: JvmTokKind::LBrace,
                    text: "{",
                    range: byte_range(start, i + 1, start_line, line),
                });
                i += 1;
            }
            b'}' => {
                out.push(JvmTok {
                    kind: JvmTokKind::RBrace,
                    text: "}",
                    range: byte_range(start, i + 1, start_line, line),
                });
                i += 1;
            }
            b'(' => {
                out.push(JvmTok {
                    kind: JvmTokKind::LParen,
                    text: "(",
                    range: byte_range(start, i + 1, start_line, line),
                });
                i += 1;
            }
            b')' => {
                out.push(JvmTok {
                    kind: JvmTokKind::RParen,
                    text: ")",
                    range: byte_range(start, i + 1, start_line, line),
                });
                i += 1;
            }
            b'.' => {
                out.push(JvmTok {
                    kind: JvmTokKind::Dot,
                    text: ".",
                    range: byte_range(start, i + 1, start_line, line),
                });
                i += 1;
            }
            b'*' => {
                out.push(JvmTok {
                    kind: JvmTokKind::Star,
                    text: "*",
                    range: byte_range(start, i + 1, start_line, line),
                });
                i += 1;
            }
            b';' => {
                out.push(JvmTok {
                    kind: JvmTokKind::Semi,
                    text: ";",
                    range: byte_range(start, i + 1, start_line, line),
                });
                i += 1;
            }
            b'\n' => {
                line = line.saturating_add(1);
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            c if is_ident_start(c) => {
                i += 1;
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                let text = &text[start..i];
                out.push(JvmTok {
                    kind: JvmTokKind::Ident,
                    text,
                    range: byte_range(start, i, start_line, line),
                });
            }
            _ => {
                out.push(JvmTok {
                    kind: JvmTokKind::Other,
                    text: &text[start..start + 1],
                    range: byte_range(start, start + 1, start_line, line),
                });
                i += 1;
            }
        }
        if out.len() > ctx.budget.max_symbols.saturating_mul(8) {
            break;
        }
    }
    Ok(out)
}

fn dotted_name<'a>(tokens: &[JvmTok<'a>], mut i: usize) -> Option<(String, usize, SourceRange)> {
    let first = tokens.get(i)?;
    if first.kind != JvmTokKind::Ident {
        return None;
    }
    let start = first.range;
    let mut name = first.text.to_string();
    i += 1;
    loop {
        match tokens.get(i).map(|t| t.kind) {
            Some(JvmTokKind::Dot) => {
                name.push('.');
                i += 1;
                match tokens.get(i) {
                    Some(tok) if tok.kind == JvmTokKind::Ident => {
                        name.push_str(tok.text);
                        i += 1;
                    }
                    Some(tok) if tok.kind == JvmTokKind::Star => {
                        name.push('*');
                        i += 1;
                    }
                    _ => break,
                }
            }
            Some(JvmTokKind::Semi) => {
                let end = tokens[i].range;
                return Some((
                    name,
                    i + 1,
                    SourceRange {
                        start_byte: start.start_byte,
                        end_byte: end.end_byte,
                        start_line: start.start_line,
                        end_line: end.end_line,
                    },
                ));
            }
            _ => break,
        }
    }
    let last = tokens.get(i.saturating_sub(1)).unwrap_or(first);
    Some((
        name,
        i,
        SourceRange {
            start_byte: start.start_byte,
            end_byte: last.range.end_byte,
            start_line: start.start_line,
            end_line: last.range.end_line,
        },
    ))
}

fn jvm_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "if" | "for"
            | "while"
            | "switch"
            | "catch"
            | "synchronized"
            | "new"
            | "return"
            | "throw"
            | "else"
            | "do"
            | "try"
            | "this"
            | "super"
            | "class"
            | "interface"
            | "enum"
            | "record"
            | "package"
            | "import"
            | "static"
            | "public"
            | "private"
            | "protected"
            | "abstract"
            | "final"
            | "void"
            | "boolean"
            | "byte"
            | "short"
            | "int"
            | "long"
            | "float"
            | "double"
            | "char"
            | "true"
            | "false"
            | "null"
            | "extends"
            | "implements"
            | "throws"
    )
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

impl ExtractCtx<'_> {
    fn bump(&mut self) -> Result<(), SymbolError> {
        self.visited = self.visited.saturating_add(1);
        if self.visited == 1 || self.visited.is_multiple_of(CANCEL_STRIDE) {
            if self.budget.cancel.is_cancelled() {
                return Err(SymbolError::Cancelled);
            }
            if self.started.elapsed() > self.budget.timeout {
                return Err(SymbolError::Timeout);
            }
        }
        Ok(())
    }

    fn full(&self) -> bool {
        self.out.len() >= self.budget.max_symbols
    }
}

fn range_of(node: Node<'_>) -> SourceRange {
    let start = node.start_position();
    let end = node.end_position();
    SourceRange {
        start_byte: saturate_u32(node.start_byte()),
        end_byte: saturate_u32(node.end_byte()),
        start_line: saturate_u32(start.row),
        end_line: saturate_u32(end.row),
    }
}

fn byte_range(start: usize, end: usize, start_line: u32, end_line: u32) -> SourceRange {
    SourceRange {
        start_byte: saturate_u32(start),
        end_byte: saturate_u32(end),
        start_line,
        end_line,
    }
}

fn saturate_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn field_text(node: Node<'_>, field: &str, source: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|child| node_text(child, source))
}

fn named_descendant_text(node: Node<'_>, kinds: &[&str], source: &[u8]) -> Option<String> {
    let count = node.named_child_count();
    for i in 0..count {
        let child = node.named_child(i)?;
        if kinds.contains(&child.kind()) {
            return node_text(child, source);
        }
        if let Some(found) = named_descendant_text(child, kinds, source) {
            return Some(found);
        }
    }
    None
}

fn compact_node_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    let raw = node.utf8_text(source).ok()?;
    let mut out = String::new();
    let mut prev_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
                prev_space = true;
            }
        } else {
            prev_space = false;
            out.push(ch);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn node_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    let raw = node.utf8_text(source).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn primary_name(text: &str) -> &str {
    let text = text.trim();
    if let Some(idx) = text.rfind("::") {
        text.get(idx.saturating_add(2)..).unwrap_or(text)
    } else if let Some(idx) = text.rfind('.') {
        text.get(idx.saturating_add(1)..).unwrap_or(text)
    } else {
        text
    }
}

fn truncate_chars(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = 0;
    for (idx, ch) in text.char_indices() {
        let next = idx + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::registry::{ParseBudget, ParserRegistry, StructuralConfidence};

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn parse_extract(language: SourceLanguage, rel: &str, src: &str) -> Vec<SymbolRecord> {
        let outcome = ParserRegistry::parse(language, src.as_bytes(), &ParseBudget::new());
        extract_from_parse(&outcome, &path(rel), src.as_bytes(), &SymbolBudget::new())
            .expect("extract")
    }

    fn kinds_of(records: &[SymbolRecord], kind: SymbolKind) -> Vec<&str> {
        records
            .iter()
            .filter(|r| r.kind() == kind)
            .map(SymbolRecord::name)
            .collect()
    }

    #[test]
    fn rust_extracts_definitions_imports_and_calls() {
        let src = r#"
use std::fmt::Display;

pub struct Parser;

impl Parser {
    pub fn parse(&self) -> u32 {
        helper()
    }
}

fn helper() -> u32 { 1 }
"#;
        let symbols = parse_extract(SourceLanguage::Rust, "src/lib.rs", src);
        assert!(
            kinds_of(&symbols, SymbolKind::Import)
                .iter()
                .any(|n| n.contains("Display"))
        );
        assert_eq!(kinds_of(&symbols, SymbolKind::Class), vec!["Parser"]);
        assert!(kinds_of(&symbols, SymbolKind::Impl).contains(&"Parser"));
        assert!(kinds_of(&symbols, SymbolKind::Method).contains(&"parse"));
        assert!(kinds_of(&symbols, SymbolKind::Function).contains(&"helper"));
        assert!(kinds_of(&symbols, SymbolKind::Reference).contains(&"helper"));

        let method = symbols
            .iter()
            .find(|s| s.kind() == SymbolKind::Method && s.name() == "parse")
            .expect("method");
        assert_eq!(method.container(), Some("Parser"));
        assert_eq!(method.fq_name(), "Parser::parse");
        assert_eq!(method.locator(), "src/lib.rs#method:Parser::parse");
        assert!(method.range().end_byte() > method.range().start_byte());
    }

    #[test]
    fn typescript_extracts_function_class_and_import() {
        let src = r#"
import { readFile } from "fs";

export class Reader {
  load(path: string): string {
    return readFile(path);
  }
}

export function add(a: number, b: number): number {
  return a + b;
}
"#;
        let symbols = parse_extract(SourceLanguage::TypeScript, "src/reader.ts", src);
        assert!(
            kinds_of(&symbols, SymbolKind::Import)
                .iter()
                .any(|n| n.contains("fs"))
        );
        assert!(kinds_of(&symbols, SymbolKind::Class).contains(&"Reader"));
        assert!(kinds_of(&symbols, SymbolKind::Method).contains(&"load"));
        assert!(kinds_of(&symbols, SymbolKind::Function).contains(&"add"));
        assert!(kinds_of(&symbols, SymbolKind::Reference).contains(&"readFile"));
    }

    #[test]
    fn javascript_extracts_function_class_and_import() {
        let src = r#"
import { readFile } from "fs";

export class Loader {
  fetch(path) {
    return readFile(path);
  }
}

export function sum(a, b) {
  return a + b;
}
"#;
        let symbols = parse_extract(SourceLanguage::JavaScript, "src/loader.js", src);
        assert!(
            kinds_of(&symbols, SymbolKind::Import)
                .iter()
                .any(|n| n.contains("fs"))
        );
        assert!(kinds_of(&symbols, SymbolKind::Class).contains(&"Loader"));
        assert!(kinds_of(&symbols, SymbolKind::Method).contains(&"fetch"));
        assert!(kinds_of(&symbols, SymbolKind::Function).contains(&"sum"));
        assert!(kinds_of(&symbols, SymbolKind::Reference).contains(&"readFile"));
    }

    #[test]
    fn python_extracts_class_function_and_import() {
        let src = r#"
from os import path

class Worker:
    def run(self):
        helper()

def helper():
    return 1
"#;
        let symbols = parse_extract(SourceLanguage::Python, "pkg/worker.py", src);
        assert!(
            kinds_of(&symbols, SymbolKind::Import)
                .iter()
                .any(|n| n.contains("os") || n.contains("path"))
        );
        assert!(kinds_of(&symbols, SymbolKind::Class).contains(&"Worker"));
        assert!(kinds_of(&symbols, SymbolKind::Method).contains(&"run"));
        assert!(kinds_of(&symbols, SymbolKind::Function).contains(&"helper"));
        assert!(kinds_of(&symbols, SymbolKind::Reference).contains(&"helper"));
    }

    #[test]
    fn go_extracts_package_func_type_and_import() {
        let src = r#"
package worker

import "fmt"

type Parser struct{}

func (p Parser) Parse() int { return helper() }

func helper() int { return 1 }
"#;
        let symbols = parse_extract(SourceLanguage::Go, "worker.go", src);
        assert!(kinds_of(&symbols, SymbolKind::Module).contains(&"worker"));
        assert!(
            kinds_of(&symbols, SymbolKind::Import)
                .iter()
                .any(|n| n.contains("fmt"))
        );
        assert!(kinds_of(&symbols, SymbolKind::Type).contains(&"Parser"));
        assert!(kinds_of(&symbols, SymbolKind::Method).contains(&"Parse"));
        assert!(kinds_of(&symbols, SymbolKind::Function).contains(&"helper"));
        assert!(kinds_of(&symbols, SymbolKind::Reference).contains(&"helper"));
    }

    #[test]
    fn java_source_fallback_extracts_package_class_method_import() {
        let src = r#"
package com.example.app;

import java.util.List;

public class Worker {
    public Worker() {}
    public List<String> run(String name) {
        return helper(name);
    }
    private List<String> helper(String name) {
        return List.of(name);
    }
}
"#;
        let outcome =
            ParserRegistry::parse(SourceLanguage::Java, src.as_bytes(), &ParseBudget::new());
        assert!(outcome.tree().is_none());
        let symbols = extract_from_parse(
            &outcome,
            &path("src/Worker.java"),
            src.as_bytes(),
            &SymbolBudget::new(),
        )
        .expect("java extract");
        assert!(kinds_of(&symbols, SymbolKind::Module).contains(&"com.example.app"));
        assert!(
            kinds_of(&symbols, SymbolKind::Import)
                .iter()
                .any(|n| n.contains("java.util.List"))
        );
        assert!(kinds_of(&symbols, SymbolKind::Class).contains(&"Worker"));
        assert!(kinds_of(&symbols, SymbolKind::Method).contains(&"run"));
        assert!(kinds_of(&symbols, SymbolKind::Method).contains(&"helper"));
        assert!(kinds_of(&symbols, SymbolKind::Method).contains(&"Worker"));
        assert!(kinds_of(&symbols, SymbolKind::Reference).contains(&"helper"));
        let run = symbols
            .iter()
            .find(|s| s.kind() == SymbolKind::Method && s.name() == "run")
            .expect("run");
        assert_eq!(run.container(), Some("Worker"));
        assert_eq!(run.locator(), "src/Worker.java#method:Worker::run");
    }

    #[test]
    fn malformed_rust_yields_partial_bounded_facts() {
        let src = "fn good() { helper(); }\nfn broken(\nstruct Foo {}\nfn helper() {}\n";
        let outcome =
            ParserRegistry::parse(SourceLanguage::Rust, src.as_bytes(), &ParseBudget::new());
        assert_eq!(outcome.confidence(), StructuralConfidence::Degraded);
        assert!(outcome.tree().is_some_and(|t| t.has_error()));
        let symbols = extract_from_parse(
            &outcome,
            &path("src/broken.rs"),
            src.as_bytes(),
            &SymbolBudget::new(),
        )
        .expect("partial");
        assert!(
            kinds_of(&symbols, SymbolKind::Function).contains(&"good")
                || kinds_of(&symbols, SymbolKind::Function).contains(&"helper")
                || kinds_of(&symbols, SymbolKind::Class).contains(&"Foo")
        );
        assert!(symbols.len() <= DEFAULT_MAX_SYMBOLS);
    }

    #[test]
    fn malformed_java_yields_partial_facts() {
        let src = "package com.x;\npublic class Foo {\n  void ok() {}\n  void broken(\n}\n";
        let symbols = extract_from_parse(
            &ParserRegistry::parse(SourceLanguage::Java, src.as_bytes(), &ParseBudget::new()),
            &path("Foo.java"),
            src.as_bytes(),
            &SymbolBudget::new(),
        )
        .expect("partial java");
        assert!(kinds_of(&symbols, SymbolKind::Class).contains(&"Foo"));
        assert!(kinds_of(&symbols, SymbolKind::Method).contains(&"ok"));
    }

    #[test]
    fn locators_are_stable_for_identical_source() {
        let src = "fn alpha() {}\nfn beta() {}\n";
        let a = parse_extract(SourceLanguage::Rust, "src/a.rs", src);
        let b = parse_extract(SourceLanguage::Rust, "src/a.rs", src);
        let loc_a: Vec<_> = a.iter().map(SymbolRecord::locator).collect();
        let loc_b: Vec<_> = b.iter().map(SymbolRecord::locator).collect();
        assert_eq!(loc_a, loc_b);
        assert!(loc_a.contains(&"src/a.rs#function:alpha"));
    }

    #[test]
    fn duplicate_names_disambiguate_locators() {
        let src = "fn dup() {}\nfn dup() {}\n";
        let symbols = parse_extract(SourceLanguage::Rust, "src/d.rs", src);
        let dups: Vec<_> = symbols
            .iter()
            .filter(|s| s.kind() == SymbolKind::Function && s.name() == "dup")
            .map(SymbolRecord::locator)
            .collect();
        assert_eq!(
            dups,
            vec!["src/d.rs#function:dup", "src/d.rs#function:dup#1"]
        );
    }

    #[test]
    fn symbol_cap_is_bounded() {
        let src = "fn a() { b(); c(); d(); }\nfn b() {}\nfn c() {}\nfn d() {}\n";
        let outcome =
            ParserRegistry::parse(SourceLanguage::Rust, src.as_bytes(), &ParseBudget::new());
        let tree = outcome.tree().expect("tree");
        let symbols = extract_symbols(
            tree,
            &path("src/cap.rs"),
            src.as_bytes(),
            &SymbolBudget::new().max_symbols(2),
        )
        .expect("capped");
        assert_eq!(symbols.len(), 2);
    }

    #[test]
    fn cancelled_extract_is_typed() {
        let src = "fn main() {}\n";
        let outcome =
            ParserRegistry::parse(SourceLanguage::Rust, src.as_bytes(), &ParseBudget::new());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = extract_from_parse(
            &outcome,
            &path("src/main.rs"),
            src.as_bytes(),
            &SymbolBudget::new().cancellation(cancel),
        )
        .expect_err("cancelled");
        assert_eq!(err, SymbolError::Cancelled);
        assert_eq!(err.to_string(), "cancelled");
        assert!(!err.to_string().contains("main"));
    }

    #[test]
    fn zero_timeout_is_typed() {
        let src = "fn main() {}\n";
        let outcome =
            ParserRegistry::parse(SourceLanguage::Rust, src.as_bytes(), &ParseBudget::new());
        let err = extract_from_parse(
            &outcome,
            &path("src/main.rs"),
            src.as_bytes(),
            &SymbolBudget::new().timeout(Duration::ZERO),
        )
        .expect_err("timeout");
        assert_eq!(err, SymbolError::Timeout);
    }

    #[test]
    fn invalid_utf8_is_typed_and_silent() {
        let outcome =
            ParserRegistry::parse(SourceLanguage::Rust, b"fn main() {}\n", &ParseBudget::new());
        let err = extract_from_parse(
            &outcome,
            &path("src/main.rs"),
            b"fn main() { \xff }",
            &SymbolBudget::new(),
        )
        .expect_err("utf8");
        assert_eq!(err, SymbolError::InvalidUtf8);
        assert!(!err.to_string().contains("main"));
    }

    #[test]
    fn oversized_source_is_typed() {
        let src = "fn main() {}\n";
        let outcome =
            ParserRegistry::parse(SourceLanguage::Rust, src.as_bytes(), &ParseBudget::new());
        let err = extract_from_parse(
            &outcome,
            &path("src/main.rs"),
            src.as_bytes(),
            &SymbolBudget::new().max_source_bytes(4),
        )
        .expect_err("too large");
        assert_eq!(err, SymbolError::SourceTooLarge);
    }
}
