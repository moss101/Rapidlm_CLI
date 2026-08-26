//! Code-aware source chunking with hard size caps and stable IDs.
//!
//! Definition symbols are preferred split points. Oversized symbols split at
//! inner syntax, then lines. Languages without usable symbols fall back to
//! paragraph/line packing. Overlap is capped and never expands a chunk past
//! the hard token/byte limits.

use std::cmp::Reverse;
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::{RepoId, RepoPath};

use crate::ingest::content::{CONTENT_HASH_LEN, ContentHash, SourceLanguage};
use crate::parse::registry::DEFAULT_PARSE_TIMEOUT;
use crate::parse::symbols::{SymbolKind, SymbolRecord};
use crate::repo_manifest::CancellationToken;

/// Default wall-clock budget for one chunk call.
pub const DEFAULT_CHUNK_TIMEOUT: Duration = DEFAULT_PARSE_TIMEOUT;

/// Default source-byte cap; matches the parse ingest limit.
pub const DEFAULT_MAX_SOURCE_BYTES: usize = crate::parse::registry::DEFAULT_MAX_PARSE_BYTES;

/// Hard default cap on one emitted chunk's UTF-8 bytes, including overlap.
pub const DEFAULT_MAX_CHUNK_BYTES: usize = 4_096;

/// Hard default cap on one emitted chunk's estimated tokens, including overlap.
pub const DEFAULT_MAX_CHUNK_TOKENS: u32 = 1_024;

/// Default maximum overlap bytes copied from the previous body.
pub const DEFAULT_OVERLAP_BYTES: usize = 256;

/// Default maximum overlap tokens copied from the previous body.
pub const DEFAULT_OVERLAP_TOKENS: u32 = 64;

/// Default maximum chunks emitted for one document.
pub const DEFAULT_MAX_CHUNKS: usize = 16_384;

/// UTF-8 character upper bound; a policy must admit at least one character.
const MIN_CHUNK_BYTES: usize = 4;

const CANCEL_STRIDE: usize = 16;
const ID_NAMESPACE: &[u8] = b"rapidlm.chunk.v1\0";

/// Per-call chunking bounds. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct ChunkPolicy {
    max_source_bytes: usize,
    max_chunk_bytes: usize,
    max_chunk_tokens: u32,
    overlap_bytes: usize,
    overlap_tokens: u32,
    max_chunks: usize,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Source document presented to the chunker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Document {
    repo_id: RepoId,
    path: RepoPath,
    language: Option<SourceLanguage>,
    text: String,
    content_hash: ContentHash,
}

/// Why a chunk was cut where it was.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ChunkKind {
    Symbol,
    Syntax,
    Paragraph,
    Line,
}

/// Stable identity derived from repo, path, locator, range, and chunk text.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct ChunkId(ContentHash);

/// One emitted span of `Document` text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChunkRecord {
    id: ChunkId,
    repo_id: RepoId,
    path: RepoPath,
    kind: ChunkKind,
    language: Option<SourceLanguage>,
    symbol_id: Option<String>,
    start_byte: u32,
    end_byte: u32,
    start_line: u32,
    end_line: u32,
    content_hash: ContentHash,
    text_hash: ContentHash,
    text: String,
    estimated_tokens: u32,
}

/// Typed chunking failure. Display never echoes source or host paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChunkError {
    Cancelled,
    Timeout,
    InvalidUtf8,
    SourceTooLarge,
    InvalidPolicy,
    TooManyChunks,
}

struct LineIndex {
    starts: Vec<u32>,
    len: u32,
}

#[derive(Clone)]
struct DefSpan {
    start: u32,
    end: u32,
    locator: String,
}

#[derive(Clone)]
struct Planned {
    start: u32,
    end: u32,
    kind: ChunkKind,
    locator: Option<String>,
}

struct ChunkCtx<'a> {
    text: &'a str,
    language: Option<SourceLanguage>,
    policy: &'a ChunkPolicy,
    lines: LineIndex,
    started: Instant,
    steps: usize,
    out: Vec<Planned>,
}

impl ChunkPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_source_bytes(mut self, value: usize) -> Self {
        self.max_source_bytes = value;
        self
    }

    pub fn max_chunk_bytes(mut self, value: usize) -> Self {
        self.max_chunk_bytes = value;
        self
    }

    pub fn max_chunk_tokens(mut self, value: u32) -> Self {
        self.max_chunk_tokens = value;
        self
    }

    pub fn overlap_bytes(mut self, value: usize) -> Self {
        self.overlap_bytes = value;
        self
    }

    pub fn overlap_tokens(mut self, value: u32) -> Self {
        self.overlap_tokens = value;
        self
    }

    pub fn max_chunks(mut self, value: usize) -> Self {
        self.max_chunks = value;
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

    pub fn max_chunk_bytes_value(&self) -> usize {
        self.max_chunk_bytes
    }

    pub fn max_chunk_tokens_value(&self) -> u32 {
        self.max_chunk_tokens
    }

    pub fn overlap_bytes_value(&self) -> usize {
        self.overlap_bytes
    }

    pub fn overlap_tokens_value(&self) -> u32 {
        self.overlap_tokens
    }

    pub fn max_chunks_value(&self) -> usize {
        self.max_chunks
    }

    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl Default for ChunkPolicy {
    fn default() -> Self {
        Self {
            max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
            max_chunk_bytes: DEFAULT_MAX_CHUNK_BYTES,
            max_chunk_tokens: DEFAULT_MAX_CHUNK_TOKENS,
            overlap_bytes: DEFAULT_OVERLAP_BYTES,
            overlap_tokens: DEFAULT_OVERLAP_TOKENS,
            max_chunks: DEFAULT_MAX_CHUNKS,
            timeout: DEFAULT_CHUNK_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }
}

impl Document {
    /// Build a document and hash `text` as the file content hash.
    pub fn new(
        repo_id: RepoId,
        path: RepoPath,
        language: Option<SourceLanguage>,
        text: impl Into<String>,
    ) -> Self {
        let text = text.into();
        let content_hash = ContentHash::from_bytes(text.as_bytes());
        Self {
            repo_id,
            path,
            language,
            text,
            content_hash,
        }
    }

    /// Build a document with a caller-supplied file content hash.
    pub fn with_content_hash(
        repo_id: RepoId,
        path: RepoPath,
        language: Option<SourceLanguage>,
        text: impl Into<String>,
        content_hash: ContentHash,
    ) -> Self {
        Self {
            repo_id,
            path,
            language,
            text: text.into(),
            content_hash,
        }
    }

    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn language(&self) -> Option<SourceLanguage> {
        self.language
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }
}

impl ChunkKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::Syntax => "syntax",
            Self::Paragraph => "paragraph",
            Self::Line => "line",
        }
    }
}

impl fmt::Display for ChunkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ChunkId {
    pub fn as_content_hash(self) -> ContentHash {
        self.0
    }
}

impl fmt::Display for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ChunkId").field(&self.0.to_string()).finish()
    }
}

impl ChunkRecord {
    pub fn id(&self) -> ChunkId {
        self.id
    }

    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn kind(&self) -> ChunkKind {
        self.kind
    }

    pub fn language(&self) -> Option<SourceLanguage> {
        self.language
    }

    pub fn symbol_id(&self) -> Option<&str> {
        self.symbol_id.as_deref()
    }

    pub fn start_byte(&self) -> u32 {
        self.start_byte
    }

    pub fn end_byte(&self) -> u32 {
        self.end_byte
    }

    pub fn start_line(&self) -> u32 {
        self.start_line
    }

    pub fn end_line(&self) -> u32 {
        self.end_line
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn text_hash(&self) -> ContentHash {
        self.text_hash
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn estimated_tokens(&self) -> u32 {
        self.estimated_tokens
    }
}

impl ChunkError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::SourceTooLarge => "source_too_large",
            Self::InvalidPolicy => "invalid_policy",
            Self::TooManyChunks => "too_many_chunks",
        }
    }
}

impl fmt::Display for ChunkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ChunkError {}

/// Chunk `document` around `symbols` under `policy`.
///
/// Bodies tile the source. Overlap may extend a chunk's start leftward but
/// never past the previous body's start, and never past the hard caps.
pub fn chunk(
    document: &Document,
    symbols: &[SymbolRecord],
    policy: &ChunkPolicy,
) -> Result<Vec<ChunkRecord>, ChunkError> {
    if policy.cancel.is_cancelled() {
        return Err(ChunkError::Cancelled);
    }
    if policy.timeout.is_zero() {
        return Err(ChunkError::Timeout);
    }
    if policy.max_chunk_bytes < MIN_CHUNK_BYTES || policy.max_chunk_tokens == 0 {
        return Err(ChunkError::InvalidPolicy);
    }
    if document.text.len() > policy.max_source_bytes {
        return Err(ChunkError::SourceTooLarge);
    }
    let Ok(len) = u32::try_from(document.text.len()) else {
        return Err(ChunkError::SourceTooLarge);
    };
    if !document.text.is_char_boundary(0) {
        return Err(ChunkError::InvalidUtf8);
    }
    if document.text.is_empty() {
        return Ok(Vec::new());
    }

    let lines = LineIndex::from_text(document.text.as_str(), len);
    let spans = def_spans(document.text.as_str(), &lines, symbols);
    let mut ctx = ChunkCtx {
        text: document.text.as_str(),
        language: document.language,
        policy,
        lines,
        started: Instant::now(),
        steps: 0,
        out: Vec::new(),
    };
    plan_interval(&mut ctx, 0, len, &spans)?;
    emit_records(document, &ctx)
}

fn emit_records(document: &Document, ctx: &ChunkCtx<'_>) -> Result<Vec<ChunkRecord>, ChunkError> {
    if ctx.out.len() > ctx.policy.max_chunks {
        return Err(ChunkError::TooManyChunks);
    }
    let mut records = Vec::with_capacity(ctx.out.len());
    for (index, planned) in ctx.out.iter().enumerate() {
        let start = overlap_start(ctx, index, planned);
        let end = planned.end;
        if start >= end {
            return Err(ChunkError::InvalidPolicy);
        }
        let start_us = start as usize;
        let end_us = end as usize;
        if !ctx.text.is_char_boundary(start_us) || !ctx.text.is_char_boundary(end_us) {
            return Err(ChunkError::InvalidUtf8);
        }
        let text = ctx.text[start_us..end_us].to_string();
        if text.len() > ctx.policy.max_chunk_bytes {
            return Err(ChunkError::InvalidPolicy);
        }
        let estimated_tokens = estimate_tokens(&text);
        if estimated_tokens > ctx.policy.max_chunk_tokens {
            return Err(ChunkError::InvalidPolicy);
        }
        let text_hash = ContentHash::from_bytes(text.as_bytes());
        let locator = planned.locator.as_deref().unwrap_or("");
        let id = derive_chunk_id(
            document.repo_id,
            &document.path,
            locator,
            start,
            end,
            text_hash,
        );
        let start_line = ctx.lines.line_at(start);
        let end_line = if end == 0 {
            0
        } else {
            ctx.lines.line_at(end - 1).saturating_add(1)
        };
        records.push(ChunkRecord {
            id,
            repo_id: document.repo_id,
            path: document.path.clone(),
            kind: planned.kind,
            language: document.language,
            symbol_id: planned.locator.clone(),
            start_byte: start,
            end_byte: end,
            start_line,
            end_line,
            content_hash: document.content_hash,
            text_hash,
            text,
            estimated_tokens,
        });
    }
    Ok(records)
}

fn derive_chunk_id(
    repo_id: RepoId,
    path: &RepoPath,
    locator: &str,
    start: u32,
    end: u32,
    text_hash: ContentHash,
) -> ChunkId {
    let repo = repo_id.to_string();
    let mut preimage = Vec::with_capacity(
        ID_NAMESPACE.len()
            + repo.len()
            + path.as_str().len()
            + locator.len()
            + CONTENT_HASH_LEN
            + 24,
    );
    preimage.extend_from_slice(ID_NAMESPACE);
    preimage.extend_from_slice(repo.as_bytes());
    preimage.push(0);
    preimage.extend_from_slice(path.as_str().as_bytes());
    preimage.push(0);
    preimage.extend_from_slice(locator.as_bytes());
    preimage.push(0);
    preimage.extend_from_slice(&start.to_le_bytes());
    preimage.extend_from_slice(&end.to_le_bytes());
    preimage.push(0);
    preimage.extend_from_slice(text_hash.as_digest());
    ChunkId(ContentHash::from_bytes(&preimage))
}

fn overlap_start(ctx: &ChunkCtx<'_>, index: usize, planned: &Planned) -> u32 {
    if index == 0 {
        return planned.start;
    }
    let overlap_bytes = ctx.policy.overlap_bytes;
    let overlap_tokens = ctx.policy.overlap_tokens;
    if overlap_bytes == 0 || overlap_tokens == 0 {
        return planned.start;
    }
    let body_len = planned.end.saturating_sub(planned.start) as usize;
    let room = ctx.policy.max_chunk_bytes.saturating_sub(body_len);
    if room == 0 {
        return planned.start;
    }
    let cap = overlap_bytes.min(room);
    let floor = ctx.out[index - 1].start;
    let raw = planned.start.saturating_sub(cap as u32).max(floor);
    let mut start = snap_overlap_start(&ctx.lines, ctx.text, raw, planned.start, cap);
    while start < planned.start {
        let slice = &ctx.text[start as usize..planned.end as usize];
        let overlap = &ctx.text[start as usize..planned.start as usize];
        if slice.len() <= ctx.policy.max_chunk_bytes
            && estimate_tokens(slice) <= ctx.policy.max_chunk_tokens
            && overlap.len() <= overlap_bytes
            && estimate_tokens(overlap) <= overlap_tokens
        {
            return start;
        }
        let next_line = ctx.lines.line_at(start).saturating_add(1);
        let next = ctx.lines.line_start(next_line);
        if next <= start || next >= planned.start {
            start = planned.start;
            break;
        }
        start = next;
    }
    start
}

fn snap_overlap_start(lines: &LineIndex, text: &str, raw: u32, body_start: u32, cap: usize) -> u32 {
    if raw >= body_start {
        return body_start;
    }
    let line_start = lines.line_start(lines.line_at(raw));
    if line_start < body_start && (body_start - line_start) as usize <= cap {
        return line_start;
    }
    snap_char_right(text, raw, body_start)
}

fn def_spans(text: &str, lines: &LineIndex, symbols: &[SymbolRecord]) -> Vec<DefSpan> {
    let mut raw: Vec<(DefSpan, bool)> = Vec::new();
    for symbol in symbols {
        if !is_definition(symbol.kind()) {
            continue;
        }
        let range = symbol.range();
        let start_b = snap_char_left(text, range.start_byte(), lines.len);
        let end_b = snap_char_right(text, range.end_byte(), lines.len);
        if end_b <= start_b {
            continue;
        }
        let (start, end) = line_expand(lines, start_b, end_b);
        if end <= start {
            continue;
        }
        let substantial = is_substantial(text, start, end);
        raw.push((
            DefSpan {
                start,
                end,
                locator: symbol.locator().to_string(),
            },
            substantial,
        ));
    }
    raw.sort_by(|a, b| {
        a.0.start
            .cmp(&b.0.start)
            .then_with(|| Reverse(a.0.end).cmp(&Reverse(b.0.end)))
    });

    let starts: Vec<u32> = raw.iter().map(|(span, _)| span.start).collect();
    for (index, (span, substantial)) in raw.iter_mut().enumerate() {
        if *substantial {
            continue;
        }
        let next = starts
            .iter()
            .skip(index + 1)
            .copied()
            .find(|start| *start > span.start)
            .unwrap_or(lines.len);
        span.end = next;
    }

    let mut spans: Vec<DefSpan> = raw
        .into_iter()
        .map(|(span, _)| span)
        .filter(|span| span.end > span.start)
        .collect();
    clip_partial_overlaps(&mut spans);
    spans
}

fn clip_partial_overlaps(spans: &mut [DefSpan]) {
    spans.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| Reverse(a.end).cmp(&Reverse(b.end)))
    });
    for i in 0..spans.len() {
        let start_i = spans[i].start;
        let end_i = spans[i].end;
        for j in (i + 1)..spans.len() {
            if spans[j].start >= end_i {
                break;
            }
            if spans[j].start >= start_i && spans[j].end <= end_i {
                continue;
            }
            if spans[j].start > start_i && spans[j].start < end_i && spans[j].end > end_i {
                spans[i].end = spans[j].start;
            }
        }
    }
}

fn is_definition(kind: SymbolKind) -> bool {
    !matches!(kind, SymbolKind::Import | SymbolKind::Reference)
}

fn is_substantial(text: &str, start: u32, end: u32) -> bool {
    let slice = &text[start as usize..end as usize];
    slice.contains('\n') || slice.contains('{') || slice.len() >= 32
}

fn is_definition_kind_prose(language: Option<SourceLanguage>) -> bool {
    matches!(language, Some(SourceLanguage::Markdown) | None)
}

fn plan_interval(
    ctx: &mut ChunkCtx<'_>,
    lo: u32,
    hi: u32,
    spans: &[DefSpan],
) -> Result<(), ChunkError> {
    ctx.check()?;
    if lo >= hi {
        return Ok(());
    }

    let mut covering: Vec<&DefSpan> = spans
        .iter()
        .filter(|span| span.start >= lo && span.end <= hi && span.end > span.start)
        .collect();
    covering.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| Reverse(a.end).cmp(&Reverse(b.end)))
    });

    let mut roots: Vec<&DefSpan> = Vec::new();
    for span in covering {
        if roots
            .iter()
            .any(|root| root.start <= span.start && root.end >= span.end)
        {
            continue;
        }
        roots.push(span);
    }
    roots.sort_by_key(|span| span.start);

    let mut cursor = lo;
    for root in roots {
        ctx.check()?;
        if root.start < cursor {
            continue;
        }
        if root.start > cursor {
            pack_gap(ctx, cursor, root.start)?;
        }
        if range_fits(ctx, root.start, root.end) {
            push_planned(
                ctx,
                Planned {
                    start: root.start,
                    end: root.end,
                    kind: ChunkKind::Symbol,
                    locator: Some(root.locator.clone()),
                },
            )?;
        } else {
            let inner: Vec<DefSpan> = spans
                .iter()
                .filter(|span| {
                    span.start >= root.start
                        && span.end <= root.end
                        && (span.start != root.start || span.end != root.end)
                })
                .cloned()
                .collect();
            if inner.is_empty() {
                pack_gap(ctx, root.start, root.end)?;
            } else {
                plan_interval(ctx, root.start, root.end, &inner)?;
            }
        }
        cursor = cursor.max(root.end);
    }
    if cursor < hi {
        pack_gap(ctx, cursor, hi)?;
    }
    Ok(())
}

fn pack_gap(ctx: &mut ChunkCtx<'_>, start: u32, end: u32) -> Result<(), ChunkError> {
    if start >= end {
        return Ok(());
    }
    let prose = is_definition_kind_prose(ctx.language);
    let mut cursor = start;
    while cursor < end {
        ctx.check()?;
        let next = take_chunk_end(ctx, cursor, end, prose)?;
        if next <= cursor {
            return Err(ChunkError::InvalidPolicy);
        }
        let kind = classify_gap(ctx, cursor, next, prose);
        push_planned(
            ctx,
            Planned {
                start: cursor,
                end: next,
                kind,
                locator: None,
            },
        )?;
        cursor = next;
    }
    Ok(())
}

fn classify_gap(ctx: &ChunkCtx<'_>, start: u32, end: u32, prose: bool) -> ChunkKind {
    let start_line = ctx.lines.line_at(start);
    let last_line = ctx.lines.line_at(end.saturating_sub(1));
    if start_line == last_line {
        return ChunkKind::Line;
    }
    if prose {
        ChunkKind::Paragraph
    } else {
        ChunkKind::Syntax
    }
}

fn take_chunk_end(
    ctx: &mut ChunkCtx<'_>,
    start: u32,
    limit: u32,
    prose: bool,
) -> Result<u32, ChunkError> {
    ctx.check()?;
    if prose {
        take_prose_end(ctx, start, limit)
    } else {
        take_line_end(ctx, start, limit)
    }
}

fn take_prose_end(ctx: &mut ChunkCtx<'_>, start: u32, limit: u32) -> Result<u32, ChunkError> {
    let mut end = start;
    let mut saw_content = false;
    let mut line = ctx.lines.line_at(start);
    while ctx.lines.line_start(line) < limit {
        ctx.check()?;
        let line_end = ctx.lines.line_end(line).min(limit);
        if !range_fits(ctx, start, line_end) {
            if end == start {
                return hard_split_end(ctx, start, line_end);
            }
            break;
        }
        end = line_end;
        if line_is_blank(ctx, line) {
            if saw_content {
                break;
            }
        } else {
            saw_content = true;
        }
        line = line.saturating_add(1);
        if usize::try_from(line).unwrap_or(usize::MAX) >= ctx.lines.starts.len() {
            break;
        }
    }
    if end == start {
        hard_split_end(ctx, start, limit)
    } else {
        Ok(end)
    }
}

fn take_line_end(ctx: &mut ChunkCtx<'_>, start: u32, limit: u32) -> Result<u32, ChunkError> {
    let mut end = start;
    let mut line = ctx.lines.line_at(start);
    while ctx.lines.line_start(line) < limit {
        ctx.check()?;
        let line_end = ctx.lines.line_end(line).min(limit);
        if !range_fits(ctx, start, line_end) {
            if end == start {
                return hard_split_end(ctx, start, line_end);
            }
            break;
        }
        end = line_end;
        line = line.saturating_add(1);
        if usize::try_from(line).unwrap_or(usize::MAX) >= ctx.lines.starts.len() {
            break;
        }
    }
    if end == start {
        hard_split_end(ctx, start, limit)
    } else {
        Ok(end)
    }
}

fn hard_split_end(ctx: &ChunkCtx<'_>, start: u32, limit: u32) -> Result<u32, ChunkError> {
    let s = start as usize;
    let e = limit as usize;
    let mut i = s;
    while i < e {
        let Some(ch) = ctx.text[i..].chars().next() else {
            break;
        };
        let n = ch.len_utf8();
        let next = i + n;
        if next - s > ctx.policy.max_chunk_bytes {
            break;
        }
        if estimate_tokens(&ctx.text[s..next]) > ctx.policy.max_chunk_tokens {
            break;
        }
        i = next;
    }
    if i == s {
        Err(ChunkError::InvalidPolicy)
    } else {
        u32::try_from(i).map_err(|_| ChunkError::SourceTooLarge)
    }
}

fn range_fits(ctx: &ChunkCtx<'_>, start: u32, end: u32) -> bool {
    if end <= start {
        return false;
    }
    let s = start as usize;
    let e = end as usize;
    if e > ctx.text.len() || !ctx.text.is_char_boundary(s) || !ctx.text.is_char_boundary(e) {
        return false;
    }
    let slice = &ctx.text[s..e];
    slice.len() <= ctx.policy.max_chunk_bytes
        && estimate_tokens(slice) <= ctx.policy.max_chunk_tokens
}

fn push_planned(ctx: &mut ChunkCtx<'_>, planned: Planned) -> Result<(), ChunkError> {
    if ctx.out.len() >= ctx.policy.max_chunks {
        return Err(ChunkError::TooManyChunks);
    }
    ctx.out.push(planned);
    Ok(())
}

fn estimate_tokens(text: &str) -> u32 {
    let n = text.chars().count();
    if n == 0 {
        0
    } else {
        u32::try_from(n.div_ceil(2)).unwrap_or(u32::MAX)
    }
}

fn line_is_blank(ctx: &ChunkCtx<'_>, line: u32) -> bool {
    let start = ctx.lines.line_start(line) as usize;
    let end = ctx.lines.line_end(line) as usize;
    if start >= end || end > ctx.text.len() {
        return true;
    }
    ctx.text[start..end].trim().is_empty()
}

fn line_expand(lines: &LineIndex, start: u32, end: u32) -> (u32, u32) {
    if end <= start {
        return (start, end);
    }
    let start_line = lines.line_at(start);
    let last_line = lines.line_at(end.saturating_sub(1));
    (lines.line_start(start_line), lines.line_end(last_line))
}

fn snap_char_left(text: &str, byte: u32, len: u32) -> u32 {
    let mut i = byte.min(len) as usize;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i as u32
}

fn snap_char_right(text: &str, byte: u32, len: u32) -> u32 {
    let mut i = (byte as usize).min(len as usize);
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i.min(len as usize) as u32
}

impl LineIndex {
    fn from_text(text: &str, len: u32) -> Self {
        let mut starts = Vec::new();
        starts.push(0);
        for (i, ch) in text.char_indices() {
            if ch == '\n' {
                let next = i + ch.len_utf8();
                if next < text.len() {
                    starts.push(next as u32);
                }
            }
        }
        Self { starts, len }
    }

    fn line_at(&self, byte: u32) -> u32 {
        let byte = byte.min(self.len);
        match self.starts.binary_search(&byte) {
            Ok(index) => index as u32,
            Err(index) => index.saturating_sub(1) as u32,
        }
    }

    fn line_start(&self, line: u32) -> u32 {
        self.starts.get(line as usize).copied().unwrap_or(self.len)
    }

    fn line_end(&self, line: u32) -> u32 {
        self.starts
            .get(line as usize + 1)
            .copied()
            .unwrap_or(self.len)
    }
}

impl ChunkCtx<'_> {
    fn check(&mut self) -> Result<(), ChunkError> {
        self.steps = self.steps.saturating_add(1);
        if self.steps == 1 || self.steps.is_multiple_of(CANCEL_STRIDE) {
            if self.policy.cancel.is_cancelled() {
                return Err(ChunkError::Cancelled);
            }
            if self.started.elapsed() >= self.policy.timeout {
                return Err(ChunkError::Timeout);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::registry::{ParseBudget, ParserRegistry};
    use crate::parse::symbols::{SymbolBudget, extract_from_parse};

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn doc(rel: &str, language: Option<SourceLanguage>, src: &str) -> Document {
        Document::new(RepoId::new(), path(rel), language, src)
    }

    fn extract(language: SourceLanguage, rel: &str, src: &str) -> Vec<SymbolRecord> {
        let outcome = ParserRegistry::parse(language, src.as_bytes(), &ParseBudget::new());
        extract_from_parse(&outcome, &path(rel), src.as_bytes(), &SymbolBudget::new())
            .expect("extract")
    }

    fn assert_hard_caps(chunks: &[ChunkRecord], policy: &ChunkPolicy) {
        for chunk in chunks {
            assert!(
                chunk.text().len() <= policy.max_chunk_bytes_value(),
                "byte cap {} > {}",
                chunk.text().len(),
                policy.max_chunk_bytes_value()
            );
            assert!(
                chunk.estimated_tokens() <= policy.max_chunk_tokens_value(),
                "token cap {} > {}",
                chunk.estimated_tokens(),
                policy.max_chunk_tokens_value()
            );
            assert_eq!(
                chunk.text_hash(),
                ContentHash::from_bytes(chunk.text().as_bytes())
            );
        }
    }

    fn assert_bodies_cover(src: &str, chunks: &[ChunkRecord]) {
        assert!(!chunks.is_empty() || src.is_empty());
        let mut covered = 0u32;
        for chunk in chunks {
            assert!(chunk.end_byte() > chunk.start_byte());
            assert!(
                chunk.start_byte() <= covered,
                "gap before {}",
                chunk.start_byte()
            );
            assert!(chunk.end_byte() as usize <= src.len());
            covered = covered.max(chunk.end_byte());
        }
        assert_eq!(covered as usize, src.len());
    }

    #[test]
    fn rust_chunks_align_to_function_symbols() {
        let src = "fn alpha() {\n    1\n}\nfn beta() {\n    2\n}\n";
        let symbols = extract(SourceLanguage::Rust, "src/lib.rs", src);
        let document = doc("src/lib.rs", Some(SourceLanguage::Rust), src);
        let policy = ChunkPolicy::new().overlap_bytes(0).overlap_tokens(0);
        let chunks = chunk(&document, &symbols, &policy).expect("chunk");
        assert_hard_caps(&chunks, &policy);
        assert_bodies_cover(src, &chunks);

        let named: Vec<_> = chunks
            .iter()
            .filter(|c| c.kind() == ChunkKind::Symbol)
            .collect();
        assert!(named.iter().any(|c| c.text().contains("fn alpha")));
        assert!(named.iter().any(|c| c.text().contains("fn beta")));
        assert!(named.iter().any(|c| {
            c.symbol_id()
                .is_some_and(|id| id.contains("function:alpha"))
        }));
        assert!(
            named
                .iter()
                .any(|c| { c.symbol_id().is_some_and(|id| id.contains("function:beta")) })
        );
        for c in &named {
            assert!(!c.text().contains("fn alpha") || !c.text().contains("fn beta"));
        }
    }

    #[test]
    fn markdown_uses_paragraph_boundaries() {
        let src = "First paragraph.\nStill first.\n\nSecond paragraph.\n";
        let document = doc("README.md", Some(SourceLanguage::Markdown), src);
        let policy = ChunkPolicy::new();
        let chunks = chunk(&document, &[], &policy).expect("chunk");
        assert_hard_caps(&chunks, &policy);
        assert_bodies_cover(src, &chunks);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().any(|c| c.kind() == ChunkKind::Paragraph));
        let first = chunks
            .iter()
            .find(|c| c.text().contains("First paragraph"))
            .expect("first");
        let second = chunks
            .iter()
            .find(|c| c.text().contains("Second paragraph"))
            .expect("second");
        assert_ne!(first.id(), second.id());
        assert!(!first.text().contains("Second paragraph"));
    }

    #[test]
    fn no_chunk_exceeds_hard_byte_or_token_cap() {
        let src = "fn a() { 1 }\nfn b() { 2 }\nfn c() { 3 }\nfn d() { 4 }\n";
        let symbols = extract(SourceLanguage::Rust, "src/tiny.rs", src);
        let document = doc("src/tiny.rs", Some(SourceLanguage::Rust), src);
        let policy = ChunkPolicy::new()
            .max_chunk_bytes(24)
            .max_chunk_tokens(12)
            .overlap_bytes(4)
            .overlap_tokens(2);
        let chunks = chunk(&document, &symbols, &policy).expect("chunk");
        assert!(!chunks.is_empty());
        assert_hard_caps(&chunks, &policy);
        assert_bodies_cover(src, &chunks);
    }

    #[test]
    fn oversized_line_hard_splits_without_exceeding_cap() {
        let src = "a".repeat(80);
        let document = doc("blob.txt", None, &src);
        let policy = ChunkPolicy::new()
            .max_chunk_bytes(16)
            .max_chunk_tokens(8)
            .overlap_bytes(0)
            .overlap_tokens(0);
        let chunks = chunk(&document, &[], &policy).expect("chunk");
        assert!(chunks.len() > 1);
        assert_hard_caps(&chunks, &policy);
        assert_bodies_cover(&src, &chunks);
        assert!(chunks.iter().all(|c| c.kind() == ChunkKind::Line));
    }

    #[test]
    fn chunk_ids_are_stable_for_identical_input() {
        let src = "fn alpha() { 1 }\nfn beta() { 2 }\n";
        let symbols = extract(SourceLanguage::Rust, "src/a.rs", src);
        let document = doc("src/a.rs", Some(SourceLanguage::Rust), src);
        let policy = ChunkPolicy::new();
        let a = chunk(&document, &symbols, &policy).expect("a");
        let b = chunk(&document, &symbols, &policy).expect("b");
        let ids_a: Vec<_> = a.iter().map(|c| c.id().to_string()).collect();
        let ids_b: Vec<_> = b.iter().map(|c| c.id().to_string()).collect();
        assert_eq!(ids_a, ids_b);
        assert!(ids_a.iter().all(|id| id.starts_with("sha256:")));
    }

    #[test]
    fn chunk_ids_change_when_content_changes() {
        let a_src = "fn alpha() { 1 }\n";
        let b_src = "fn alpha() { 2 }\n";
        let a_syms = extract(SourceLanguage::Rust, "src/a.rs", a_src);
        let b_syms = extract(SourceLanguage::Rust, "src/a.rs", b_src);
        let a = chunk(
            &doc("src/a.rs", Some(SourceLanguage::Rust), a_src),
            &a_syms,
            &ChunkPolicy::new(),
        )
        .expect("a");
        let b = chunk(
            &doc("src/a.rs", Some(SourceLanguage::Rust), b_src),
            &b_syms,
            &ChunkPolicy::new(),
        )
        .expect("b");
        assert_ne!(
            a.iter().map(|c| c.id().to_string()).collect::<Vec<_>>(),
            b.iter().map(|c| c.id().to_string()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn chunk_ids_change_when_range_changes() {
        let a_src = "fn alpha() { 1 }\n";
        let b_src = "\nfn alpha() { 1 }\n";
        let a_syms = extract(SourceLanguage::Rust, "src/a.rs", a_src);
        let b_syms = extract(SourceLanguage::Rust, "src/a.rs", b_src);
        let policy = ChunkPolicy::new().overlap_bytes(0).overlap_tokens(0);
        let a = chunk(
            &doc("src/a.rs", Some(SourceLanguage::Rust), a_src),
            &a_syms,
            &policy,
        )
        .expect("a");
        let b = chunk(
            &doc("src/a.rs", Some(SourceLanguage::Rust), b_src),
            &b_syms,
            &policy,
        )
        .expect("b");
        let a_fn = a
            .iter()
            .find(|c| {
                c.symbol_id()
                    .is_some_and(|id| id.contains("function:alpha"))
            })
            .expect("a fn");
        let b_fn = b
            .iter()
            .find(|c| {
                c.symbol_id()
                    .is_some_and(|id| id.contains("function:alpha"))
            })
            .expect("b fn");
        assert_ne!(a_fn.start_byte(), b_fn.start_byte());
        assert_ne!(a_fn.id(), b_fn.id());
    }

    #[test]
    fn overlap_is_capped_and_does_not_break_hard_cap() {
        let src = "fn alpha() {\n    111111\n}\nfn beta() {\n    222222\n}\n";
        let symbols = extract(SourceLanguage::Rust, "src/o.rs", src);
        let document = doc("src/o.rs", Some(SourceLanguage::Rust), src);
        let policy = ChunkPolicy::new()
            .max_chunk_bytes(64)
            .max_chunk_tokens(32)
            .overlap_bytes(8)
            .overlap_tokens(4);
        let chunks = chunk(&document, &symbols, &policy).expect("chunk");
        assert_hard_caps(&chunks, &policy);
        assert!(chunks.len() >= 2);
        let later = chunks
            .iter()
            .find(|c| c.text().contains("fn beta"))
            .expect("beta");
        if later.start_byte() < later.text().find("fn beta").unwrap() as u32 + later.start_byte() {
            let prefix_len = later.text().find("fn beta").unwrap();
            assert!(prefix_len <= policy.overlap_bytes_value());
        }
    }

    #[test]
    fn java_name_only_symbols_still_cover_the_file() {
        let src = "package com.x;\n\npublic class Worker {\n    void run() { helper(); }\n    void helper() { }\n}\n";
        let symbols = extract(SourceLanguage::Java, "Worker.java", src);
        let document = doc("Worker.java", Some(SourceLanguage::Java), src);
        let policy = ChunkPolicy::new();
        let chunks = chunk(&document, &symbols, &policy).expect("chunk");
        assert_hard_caps(&chunks, &policy);
        assert_bodies_cover(src, &chunks);
        assert!(chunks.iter().any(|c| c.text().contains("class Worker")));
        assert!(chunks.iter().any(|c| c.text().contains("void run")));
    }

    #[test]
    fn empty_document_yields_no_chunks() {
        let document = doc("empty.rs", Some(SourceLanguage::Rust), "");
        let chunks = chunk(&document, &[], &ChunkPolicy::new()).expect("empty");
        assert!(chunks.is_empty());
    }

    #[test]
    fn cancelled_chunk_is_typed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = chunk(
            &doc("src/a.rs", Some(SourceLanguage::Rust), "fn a() {}\n"),
            &[],
            &ChunkPolicy::new().cancellation(cancel),
        )
        .expect_err("cancelled");
        assert_eq!(err, ChunkError::Cancelled);
        assert_eq!(err.to_string(), "cancelled");
        assert!(!err.to_string().contains("fn"));
    }

    #[test]
    fn zero_timeout_is_typed() {
        let err = chunk(
            &doc("src/a.rs", Some(SourceLanguage::Rust), "fn a() {}\n"),
            &[],
            &ChunkPolicy::new().timeout(Duration::ZERO),
        )
        .expect_err("timeout");
        assert_eq!(err, ChunkError::Timeout);
    }

    #[test]
    fn oversized_source_is_typed() {
        let err = chunk(
            &doc("src/a.rs", Some(SourceLanguage::Rust), "fn a() {}\n"),
            &[],
            &ChunkPolicy::new().max_source_bytes(4),
        )
        .expect_err("too large");
        assert_eq!(err, ChunkError::SourceTooLarge);
        assert!(!err.to_string().contains("fn"));
    }

    #[test]
    fn invalid_policy_is_typed() {
        let err = chunk(
            &doc("src/a.rs", Some(SourceLanguage::Rust), "fn a() {}\n"),
            &[],
            &ChunkPolicy::new().max_chunk_bytes(1),
        )
        .expect_err("policy");
        assert_eq!(err, ChunkError::InvalidPolicy);
    }

    #[test]
    fn too_many_chunks_is_typed() {
        let src = "fn a() { 1 }\nfn b() { 2 }\nfn c() { 3 }\n";
        let symbols = extract(SourceLanguage::Rust, "src/n.rs", src);
        let err = chunk(
            &doc("src/n.rs", Some(SourceLanguage::Rust), src),
            &symbols,
            &ChunkPolicy::new()
                .max_chunk_bytes(16)
                .max_chunk_tokens(8)
                .overlap_bytes(0)
                .max_chunks(1),
        )
        .expect_err("too many");
        assert_eq!(err, ChunkError::TooManyChunks);
    }
}
