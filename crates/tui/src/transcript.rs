//! Viewport/windowed transcript virtualizer.
//!
//! [`TranscriptViewport`] maps a stable [`ScrollAnchor`] to the visible
//! [`RenderBlock`] range and lazy-wraps only sanitized text that intersects
//! the pane. The full history is never laid out each frame.
//!
//! This module is a frontend projection: it does not issue kernel commands
//! and does not load artifact payloads. Large tool/model bodies stay behind
//! [`ArtifactRef`] until a later reader fetches them.

use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use protocol::{ArtifactRef, RedactionClass};

use crate::layout::Rect;
use crate::sanitize::sanitize_untrusted;

/// Maximum wrapped rows a viewport will emit in one frame.
pub const MAX_VIEWPORT_HEIGHT: u16 = 256;

/// Maximum UTF-8 bytes retained as inline block text after sanitization.
pub const MAX_INLINE_TEXT_BYTES: usize = 64 * 1024;

/// Display placeholder for secret-class bodies. Never includes payload.
const REDACTED_PLACEHOLDER: &str = "[redacted]";

/// Display placeholder for an artifact that has not been loaded.
const ARTIFACT_PLACEHOLDER: &str = "[artifact]";

/// Stable identity of a transcript block. Local to this projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct BlockId(u64);

/// Discriminated render block. Matches `architecture/cli-tui.md`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RenderBlockKind {
    User,
    Assistant,
    Tool,
    Evidence,
    Error,
    System,
}

/// One user/assistant/tool/evidence/error/system block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderBlock {
    id: BlockId,
    kind: RenderBlockKind,
    text: String,
    artifact: Option<ArtifactRef>,
    redaction: RedactionClass,
    generation: u64,
    raw_line_count: u32,
    max_line_cols: u32,
    line_starts: Option<Box<[u32]>>,
    truncated: bool,
}

/// Ordered transcript history. Domain authority stays in the kernel.
#[derive(Clone, Debug, Default)]
pub struct Transcript {
    blocks: Vec<RenderBlock>,
    index: HashMap<BlockId, usize>,
    next_id: u64,
}

/// Identity-based scroll position. Survives inserts above/below the pane.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ScrollAnchor {
    block_id: BlockId,
    line: u32,
}

/// Windowed view over a [`Transcript`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptViewport {
    width: u16,
    height: u16,
    anchor: Option<ScrollAnchor>,
    follow_tail: bool,
}

/// One wrapped row inside the current pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisibleRow {
    block_id: BlockId,
    kind: RenderBlockKind,
    line_in_block: u32,
    text: String,
}

/// Visible block range plus the wrapped rows that fill the pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisibleWindow {
    first_block: Option<usize>,
    last_block: Option<usize>,
    rows: Vec<VisibleRow>,
    work: FrameWork,
}

/// Per-frame inspection counters. Used to prove virtualization bounds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct FrameWork {
    blocks_inspected: usize,
    blocks_wrapped: usize,
    source_lines_scanned: usize,
    rows_emitted: usize,
}

/// Typed virtualizer failure. Display never echoes untrusted payload text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TranscriptError {
    UnknownBlock,
    DuplicateBlock,
    IndexOutOfRange,
    /// Pending chunk queue is full: the producer must drain before pushing
    /// more (streaming backpressure).
    BackPressure,
}

impl BlockId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl RenderBlockKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::Evidence => "evidence",
            Self::Error => "error",
            Self::System => "system",
        }
    }
}

/// Map one `AppState.transcript()` entry onto the `(kind, text)` a
/// [`RenderBlock`] is built from. `TranscriptEntry` and `RenderBlockKind`
/// are deliberately different shapes — the former is the reducer's raw,
/// timeline-ordered log of what happened; the latter is this crate's
/// generic render-block taxonomy, which has no room for a separate "tool
/// status" field. The status is folded into the text itself, one glyph per
/// [`crate::state::ToolActivityStatus`] variant, matching the exact marker
/// set the production interactive renderer already used before this
/// function existed — in particular `ContextRequired` gets its own `❓`,
/// never the `✗` used for a real `Failed`, so a context-required stop is
/// never visually indistinguishable from a tool failure.
pub fn render_block_parts(entry: &crate::state::TranscriptEntry) -> (RenderBlockKind, String) {
    use crate::state::{ToolActivityStatus, TranscriptEntry};
    match entry {
        TranscriptEntry::User { text } => (RenderBlockKind::User, format!("> {text}")),
        TranscriptEntry::Assistant { text } => (RenderBlockKind::Assistant, text.clone()),
        TranscriptEntry::ToolActivity {
            tool,
            status,
            detail,
        } => {
            let marker = match status {
                ToolActivityStatus::Started => "→",
                ToolActivityStatus::Completed => "✓",
                ToolActivityStatus::Failed => "✗",
                ToolActivityStatus::Denied => "⛔",
                ToolActivityStatus::ApprovalRequired => "⏸",
                ToolActivityStatus::ContextRequired => "❓",
            };
            // A denial used to render as `⛔ workspace_write` alone, so the
            // user was told a call was refused but never why or what to do —
            // the reason existed, and went only to the model.
            let line = match detail {
                Some(detail) => format!("{marker} {tool}: {detail}"),
                None => format!("{marker} {tool}"),
            };
            (RenderBlockKind::Tool, line)
        }
        TranscriptEntry::TurnFailed { reason } => {
            (RenderBlockKind::Error, format!("(turn failed: {reason})"))
        }
        TranscriptEntry::TurnInterrupted => (RenderBlockKind::System, "(interrupted)".to_owned()),
        TranscriptEntry::CommandOutput { text } => (RenderBlockKind::System, text.clone()),
        TranscriptEntry::CommandError { text } => {
            (RenderBlockKind::Error, format!("(command error: {text})"))
        }
    }
}

impl RenderBlock {
    /// Sanitize `text` and build a block. Secret redaction drops the body.
    pub fn new(id: BlockId, kind: RenderBlockKind, text: &str) -> Self {
        Self::build(id, kind, text, None, RedactionClass::Public)
    }

    pub fn with_redaction(mut self, redaction: RedactionClass) -> Self {
        if redaction == RedactionClass::Secret {
            self.text.clear();
            self.truncated = false;
            self.reindex_lines();
        }
        self.redaction = redaction;
        self
    }

    pub fn with_artifact(mut self, artifact: ArtifactRef) -> Self {
        if artifact.redaction == RedactionClass::Secret {
            self.text.clear();
            self.redaction = RedactionClass::Secret;
            self.truncated = false;
            self.reindex_lines();
        }
        self.artifact = Some(artifact);
        self
    }

    pub fn id(&self) -> BlockId {
        self.id
    }

    pub fn kind(&self) -> RenderBlockKind {
        self.kind
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn artifact(&self) -> Option<&ArtifactRef> {
        self.artifact.as_ref()
    }

    pub fn redaction(&self) -> RedactionClass {
        self.redaction
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn raw_line_count(&self) -> u32 {
        self.raw_line_count
    }

    /// Append a streamed delta. The viewport anchor is not rewritten here.
    pub fn append_delta(&mut self, delta: &str) {
        if self.redaction == RedactionClass::Secret {
            self.generation = self.generation.saturating_add(1);
            return;
        }
        let sanitized = sanitize_untrusted(delta);
        if sanitized.is_empty() {
            self.generation = self.generation.saturating_add(1);
            return;
        }
        if self.text.len() >= MAX_INLINE_TEXT_BYTES {
            self.truncated = true;
            self.generation = self.generation.saturating_add(1);
            return;
        }
        let remaining = MAX_INLINE_TEXT_BYTES - self.text.len();
        let take = floor_char_boundary(sanitized.as_ref(), remaining);
        if take < sanitized.len() {
            self.truncated = true;
        }
        self.text.push_str(&sanitized[..take]);
        self.generation = self.generation.saturating_add(1);
        self.reindex_lines();
    }

    fn build(
        id: BlockId,
        kind: RenderBlockKind,
        text: &str,
        artifact: Option<ArtifactRef>,
        redaction: RedactionClass,
    ) -> Self {
        let (text, truncated, redaction) = ingest_text(text, artifact.as_ref(), redaction);
        let mut block = Self {
            id,
            kind,
            text,
            artifact,
            redaction,
            generation: 0,
            raw_line_count: 0,
            max_line_cols: 0,
            line_starts: None,
            truncated,
        };
        block.reindex_lines();
        block
    }

    fn reindex_lines(&mut self) {
        let (count, max_cols, starts) = index_lines(&self.text);
        self.raw_line_count = count;
        self.max_line_cols = max_cols;
        self.line_starts = starts;
    }

    fn display_source(&self) -> DisplaySource<'_> {
        if self.redaction == RedactionClass::Secret {
            return DisplaySource::Placeholder(REDACTED_PLACEHOLDER);
        }
        if !self.text.is_empty() {
            return DisplaySource::Text(&self.text);
        }
        if self.artifact.is_some() {
            return DisplaySource::Placeholder(ARTIFACT_PLACEHOLDER);
        }
        DisplaySource::Empty
    }

    fn visual_line_count(&self, width: u16) -> u32 {
        match self.display_source() {
            DisplaySource::Empty => 0,
            DisplaySource::Placeholder(_) => 1,
            DisplaySource::Text(text) => wrapped_line_count(text, width, self),
        }
    }
}

enum DisplaySource<'a> {
    Empty,
    Placeholder(&'static str),
    Text(&'a str),
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn blocks(&self) -> &[RenderBlock] {
        &self.blocks
    }

    pub fn get(&self, id: BlockId) -> Option<&RenderBlock> {
        self.index.get(&id).and_then(|&i| self.blocks.get(i))
    }

    pub fn index_of(&self, id: BlockId) -> Option<usize> {
        self.index.get(&id).copied()
    }

    /// Append a sanitized block and return its id.
    pub fn push(&mut self, kind: RenderBlockKind, text: &str) -> BlockId {
        let id = self.alloc_id();
        let block = RenderBlock::new(id, kind, text);
        self.index.insert(id, self.blocks.len());
        self.blocks.push(block);
        id
    }

    /// Append one `AppState.transcript()` entry via [`render_block_parts`].
    pub fn push_entry(&mut self, entry: &crate::state::TranscriptEntry) -> BlockId {
        let (kind, text) = render_block_parts(entry);
        self.push(kind, &text)
    }

    /// Append an already-built block. IDs must be unique.
    pub fn push_block(&mut self, block: RenderBlock) -> Result<BlockId, TranscriptError> {
        if self.index.contains_key(&block.id) {
            return Err(TranscriptError::DuplicateBlock);
        }
        self.next_id = self.next_id.max(block.id.0.saturating_add(1));
        self.index.insert(block.id, self.blocks.len());
        let id = block.id;
        self.blocks.push(block);
        Ok(id)
    }

    /// Insert a block at `index`, shifting later blocks down.
    pub fn insert(
        &mut self,
        index: usize,
        kind: RenderBlockKind,
        text: &str,
    ) -> Result<BlockId, TranscriptError> {
        if index > self.blocks.len() {
            return Err(TranscriptError::IndexOutOfRange);
        }
        let id = self.alloc_id();
        let block = RenderBlock::new(id, kind, text);
        self.blocks.insert(index, block);
        self.reindex_from(index);
        Ok(id)
    }

    /// Stream a delta into an existing block without moving the scroll anchor.
    pub fn append_delta(&mut self, id: BlockId, delta: &str) -> Result<(), TranscriptError> {
        let index = *self.index.get(&id).ok_or(TranscriptError::UnknownBlock)?;
        self.blocks[index].append_delta(delta);
        Ok(())
    }

    fn alloc_id(&mut self) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    fn reindex_from(&mut self, start: usize) {
        for (i, block) in self.blocks.iter().enumerate().skip(start) {
            self.index.insert(block.id, i);
        }
    }
}

impl ScrollAnchor {
    pub const fn new(block_id: BlockId, line: u32) -> Self {
        Self { block_id, line }
    }

    pub const fn block_id(self) -> BlockId {
        self.block_id
    }

    pub const fn line(self) -> u32 {
        self.line
    }
}

impl TranscriptViewport {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height: height.min(MAX_VIEWPORT_HEIGHT),
            anchor: None,
            follow_tail: true,
        }
    }

    /// Size the viewport from a layout transcript pane.
    pub fn from_rect(rect: Rect) -> Self {
        Self::new(rect.width(), rect.height())
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn follow_tail(&self) -> bool {
        self.follow_tail
    }

    pub fn anchor(&self) -> Option<ScrollAnchor> {
        self.anchor
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height.min(MAX_VIEWPORT_HEIGHT);
    }

    pub fn resize_rect(&mut self, rect: Rect) {
        self.resize(rect.width(), rect.height());
    }

    /// Pin the viewport to a block-local wrapped line and stop following tail.
    pub fn set_anchor(&mut self, anchor: ScrollAnchor) {
        self.anchor = Some(anchor);
        self.follow_tail = false;
    }

    pub fn follow_end(&mut self) {
        self.follow_tail = true;
    }

    /// Scroll by wrapped lines. Landing on the last line re-enables tail follow.
    pub fn scroll_by(&mut self, transcript: &Transcript, delta: i64) {
        if transcript.is_empty() || self.height == 0 {
            return;
        }
        if delta == 0 {
            self.sync_follow_tail(transcript);
            return;
        }
        let (mut idx, mut line) = if self.follow_tail {
            self.tail_first_row(transcript)
        } else {
            resolve_anchor(transcript, self.anchor, self.width)
        };
        let mut remaining = delta;
        if remaining > 0 {
            while remaining > 0 {
                let count = visual_count(transcript, idx, self.width);
                let avail = count.saturating_sub(line.saturating_add(1));
                if remaining <= i64::from(avail) {
                    line = line.saturating_add(remaining as u32);
                    break;
                }
                remaining -= i64::from(avail) + 1;
                if idx + 1 >= transcript.blocks.len() {
                    line = count.saturating_sub(1);
                    break;
                }
                idx += 1;
                line = 0;
            }
        } else {
            while remaining < 0 {
                if line > 0 {
                    let mag = remaining.unsigned_abs().min(u64::from(u32::MAX)) as u32;
                    let step = line.min(mag);
                    line -= step;
                    remaining += i64::from(step);
                    continue;
                }
                if idx == 0 {
                    break;
                }
                idx -= 1;
                let count = visual_count(transcript, idx, self.width);
                line = count.saturating_sub(1);
                remaining += 1;
            }
        }
        if let Some(block) = transcript.blocks.get(idx) {
            self.anchor = Some(ScrollAnchor::new(block.id, line));
        }
        self.sync_follow_tail(transcript);
    }

    /// Map the current anchor to the visible block range and wrap only that window.
    pub fn visible(&self, transcript: &Transcript) -> VisibleWindow {
        let mut work = FrameWork::default();
        if transcript.is_empty() || self.width == 0 || self.height == 0 {
            return VisibleWindow {
                first_block: None,
                last_block: None,
                rows: Vec::new(),
                work,
            };
        }
        let height = usize::from(self.height.min(MAX_VIEWPORT_HEIGHT));
        let mut collected = Vec::with_capacity(height);
        let (start_idx, start_line) = if self.follow_tail {
            self.tail_first_row(transcript)
        } else {
            resolve_anchor(transcript, self.anchor, self.width)
        };
        fill_forward(
            transcript,
            self.width,
            start_idx,
            start_line,
            height,
            &mut collected,
            &mut work,
        );
        work.rows_emitted = collected.len();
        let first_block = collected
            .first()
            .and_then(|row| transcript.index_of(row.block_id));
        let last_block = collected
            .last()
            .and_then(|row| transcript.index_of(row.block_id));
        VisibleWindow {
            first_block,
            last_block,
            rows: collected,
            work,
        }
    }

    fn tail_first_row(&self, transcript: &Transcript) -> (usize, u32) {
        let height = u32::from(self.height.min(MAX_VIEWPORT_HEIGHT));
        if height == 0 || transcript.is_empty() {
            return (0, 0);
        }
        let mut remaining = height;
        let mut idx = transcript.blocks.len() - 1;
        loop {
            let count = visual_count(transcript, idx, self.width);
            if count >= remaining {
                return (idx, count.saturating_sub(remaining));
            }
            remaining = remaining.saturating_sub(count);
            if idx == 0 {
                return (0, 0);
            }
            idx -= 1;
        }
    }

    fn sync_follow_tail(&mut self, transcript: &Transcript) {
        let Some(anchor) = self.anchor else {
            self.follow_tail = true;
            return;
        };
        let Some(last) = transcript.blocks.last() else {
            self.follow_tail = true;
            return;
        };
        if last.id != anchor.block_id {
            self.follow_tail = false;
            return;
        }
        let last_line = last.visual_line_count(self.width).saturating_sub(1);
        self.follow_tail = anchor.line >= last_line;
    }
}

impl VisibleWindow {
    pub fn rows(&self) -> &[VisibleRow] {
        &self.rows
    }

    pub fn first_block(&self) -> Option<usize> {
        self.first_block
    }

    pub fn last_block(&self) -> Option<usize> {
        self.last_block
    }

    pub fn work(&self) -> FrameWork {
        self.work
    }

    /// Stable one-window dump used by golden tests.
    pub fn golden(&self) -> String {
        self.rows
            .iter()
            .map(|row| format!("{}|{}", row.kind.as_str(), row.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl VisibleRow {
    pub fn block_id(&self) -> BlockId {
        self.block_id
    }

    pub fn kind(&self) -> RenderBlockKind {
        self.kind
    }

    pub fn line_in_block(&self) -> u32 {
        self.line_in_block
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl FrameWork {
    pub const fn blocks_inspected(self) -> usize {
        self.blocks_inspected
    }

    pub const fn blocks_wrapped(self) -> usize {
        self.blocks_wrapped
    }

    pub const fn source_lines_scanned(self) -> usize {
        self.source_lines_scanned
    }

    pub const fn rows_emitted(self) -> usize {
        self.rows_emitted
    }

    /// True when this frame stayed within a viewport-sized working set.
    pub fn is_bounded(self, height: u16) -> bool {
        let cap = usize::from(height.max(1)).saturating_add(2);
        self.blocks_wrapped <= cap
            && self.rows_emitted <= usize::from(height)
            && self.source_lines_scanned <= cap.saturating_mul(4)
    }
}

impl Display for TranscriptError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownBlock => f.write_str("unknown transcript block"),
            Self::DuplicateBlock => f.write_str("duplicate transcript block id"),
            Self::BackPressure | Self::IndexOutOfRange => {
                f.write_str("transcript insert index out of range")
            }
        }
    }
}

impl Error for TranscriptError {}

fn ingest_text(
    text: &str,
    artifact: Option<&ArtifactRef>,
    redaction: RedactionClass,
) -> (String, bool, RedactionClass) {
    if redaction == RedactionClass::Secret
        || artifact.is_some_and(|a| a.redaction == RedactionClass::Secret)
    {
        return (String::new(), false, RedactionClass::Secret);
    }
    let sanitized = sanitize_untrusted(text);
    if sanitized.len() <= MAX_INLINE_TEXT_BYTES {
        return (sanitized.into_owned(), false, redaction);
    }
    let take = floor_char_boundary(sanitized.as_ref(), MAX_INLINE_TEXT_BYTES);
    (sanitized[..take].to_owned(), true, redaction)
}

fn index_lines(text: &str) -> (u32, u32, Option<Box<[u32]>>) {
    if text.is_empty() {
        return (0, 0, None);
    }
    let mut count = 1u32;
    let mut max_cols = 0u32;
    let mut cols = 0u32;
    let mut starts = Vec::new();
    starts.push(0u32);
    for (i, c) in text.char_indices() {
        if c == '\n' {
            max_cols = max_cols.max(cols);
            cols = 0;
            count = count.saturating_add(1);
            let next = (i + 1) as u32;
            starts.push(next);
        } else {
            cols = cols.saturating_add(display_cols(c) as u32);
        }
    }
    max_cols = max_cols.max(cols);
    if count <= 1 {
        (count, max_cols, None)
    } else {
        (count, max_cols, Some(starts.into_boxed_slice()))
    }
}

fn source_line_at<'a>(text: &'a str, line_starts: Option<&[u32]>, index: u32) -> &'a str {
    let Some(starts) = line_starts else {
        return if index == 0 { text } else { "" };
    };
    let i = index as usize;
    let Some(&start) = starts.get(i) else {
        return "";
    };
    let end = starts
        .get(i + 1)
        .copied()
        .map(usize::try_from)
        .and_then(Result::ok)
        .unwrap_or(text.len());
    let start = usize::try_from(start).unwrap_or(0).min(text.len());
    let end = end.min(text.len()).max(start);
    let slice = &text[start..end];
    slice.strip_suffix('\n').unwrap_or(slice)
}

fn wrapped_line_count(text: &str, width: u16, block: &RenderBlock) -> u32 {
    if width == 0 {
        return 0;
    }
    if block.max_line_cols <= u32::from(width) {
        return block.raw_line_count;
    }
    let mut total = 0u32;
    for i in 0..block.raw_line_count {
        let line = source_line_at(text, block.line_starts.as_deref(), i);
        total = total.saturating_add(wrap_row_count(line, width));
    }
    total.max(1)
}

fn wrap_row_count(line: &str, width: u16) -> u32 {
    if width == 0 {
        return 0;
    }
    let width = usize::from(width);
    let mut rows = 1u32;
    let mut cols = 0usize;
    for c in line.chars() {
        let w = display_cols(c);
        if cols + w > width && cols > 0 {
            rows = rows.saturating_add(1);
            cols = w;
        } else {
            cols = cols.saturating_add(w);
        }
    }
    rows
}

fn wrap_line(line: &str, width: u16) -> Vec<&str> {
    if width == 0 {
        return Vec::new();
    }
    if line.is_empty() {
        return vec![""];
    }
    let width = usize::from(width);
    let mut rows = Vec::new();
    let mut start = 0usize;
    let mut cols = 0usize;
    for (i, c) in line.char_indices() {
        let w = display_cols(c);
        if cols + w > width && cols > 0 {
            rows.push(&line[start..i]);
            start = i;
            cols = w;
        } else {
            cols = cols.saturating_add(w);
        }
    }
    rows.push(&line[start..]);
    rows
}

fn display_cols(c: char) -> usize {
    match c {
        '\t' => 1,
        c if is_wide(c) => 2,
        _ => 1,
    }
}

fn is_wide(c: char) -> bool {
    matches!(
        c,
        '\u{1100}'..='\u{115F}'
            | '\u{2329}'..='\u{232A}'
            | '\u{2E80}'..='\u{A4CF}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE10}'..='\u{FE19}'
            | '\u{FE30}'..='\u{FE6F}'
            | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}'
            | '\u{1F300}'..='\u{1F64F}'
            | '\u{1F900}'..='\u{1F9FF}'
    )
}

fn floor_char_boundary(text: &str, max_bytes: usize) -> usize {
    if max_bytes >= text.len() {
        return text.len();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn visual_count(transcript: &Transcript, index: usize, width: u16) -> u32 {
    transcript
        .blocks
        .get(index)
        .map(|block| block.visual_line_count(width))
        .unwrap_or(0)
}

fn resolve_anchor(
    transcript: &Transcript,
    anchor: Option<ScrollAnchor>,
    width: u16,
) -> (usize, u32) {
    let Some(anchor) = anchor else {
        return (0, 0);
    };
    let Some(idx) = transcript.index_of(anchor.block_id) else {
        return (0, 0);
    };
    let count = visual_count(transcript, idx, width);
    let line = if count == 0 {
        0
    } else {
        anchor.line.min(count.saturating_sub(1))
    };
    (idx, line)
}

fn fill_forward(
    transcript: &Transcript,
    width: u16,
    start_idx: usize,
    start_line: u32,
    height: usize,
    out: &mut Vec<VisibleRow>,
    work: &mut FrameWork,
) {
    let mut line = start_line;
    for idx in start_idx..transcript.blocks.len() {
        if out.len() >= height {
            break;
        }
        emit_block_window(
            &transcript.blocks[idx],
            width,
            line,
            height - out.len(),
            out,
            work,
        );
        line = 0;
    }
}

fn emit_block_window(
    block: &RenderBlock,
    width: u16,
    start_line: u32,
    max_rows: usize,
    out: &mut Vec<VisibleRow>,
    work: &mut FrameWork,
) {
    work.blocks_inspected = work.blocks_inspected.saturating_add(1);
    if max_rows == 0 || width == 0 {
        return;
    }
    match block.display_source() {
        DisplaySource::Empty => {}
        DisplaySource::Placeholder(text) => {
            if start_line == 0 {
                work.blocks_wrapped = work.blocks_wrapped.saturating_add(1);
                work.source_lines_scanned = work.source_lines_scanned.saturating_add(1);
                out.push(VisibleRow {
                    block_id: block.id,
                    kind: block.kind,
                    line_in_block: 0,
                    text: text.to_owned(),
                });
            }
        }
        DisplaySource::Text(text) => {
            work.blocks_wrapped = work.blocks_wrapped.saturating_add(1);
            emit_text_window(block, text, width, start_line, max_rows, out, work);
        }
    }
}

fn emit_text_window(
    block: &RenderBlock,
    text: &str,
    width: u16,
    start_line: u32,
    max_rows: usize,
    out: &mut Vec<VisibleRow>,
    work: &mut FrameWork,
) {
    let limit = out.len().saturating_add(max_rows);
    let no_wrap = block.max_line_cols <= u32::from(width);
    if no_wrap {
        let begin = start_line.min(block.raw_line_count);
        let end = (begin as usize)
            .saturating_add(max_rows)
            .min(block.raw_line_count as usize);
        for line_idx in begin as usize..end {
            work.source_lines_scanned = work.source_lines_scanned.saturating_add(1);
            let line = source_line_at(text, block.line_starts.as_deref(), line_idx as u32);
            out.push(VisibleRow {
                block_id: block.id,
                kind: block.kind,
                line_in_block: line_idx as u32,
                text: line.to_owned(),
            });
        }
        return;
    }
    let mut wrapped_idx = 0u32;
    for src_idx in 0..block.raw_line_count {
        if out.len() >= limit && wrapped_idx >= start_line {
            break;
        }
        work.source_lines_scanned = work.source_lines_scanned.saturating_add(1);
        let source = source_line_at(text, block.line_starts.as_deref(), src_idx);
        for piece in wrap_line(source, width) {
            if wrapped_idx >= start_line && out.len() < limit {
                out.push(VisibleRow {
                    block_id: block.id,
                    kind: block.kind,
                    line_in_block: wrapped_idx,
                    text: piece.to_owned(),
                });
            }
            wrapped_idx = wrapped_idx.saturating_add(1);
            if out.len() >= limit && wrapped_idx > start_line {
                return;
            }
        }
    }
}

/// Maximum bytes in one coalesced chunk before it is sealed.
pub const MAX_COALESCED_CHUNK_BYTES: usize = 8 * 1024;
/// Maximum sealed chunks held before backpressure is signalled.
pub const MAX_PENDING_CHUNKS: usize = 64;

/// P10-005 streaming coalescer: merges consecutive model deltas into bounded
/// chunks so the transcript stores fewer, larger blocks under high-frequency
/// streams. When `MAX_PENDING_CHUNKS` sealed chunks are waiting, `push`
/// returns [`TranscriptError::BackPressure`] and the producer must drain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamCoalescer {
    max_chunk_bytes: usize,
    max_pending_chunks: usize,
    current: String,
    sealed: Vec<String>,
}

impl StreamCoalescer {
    pub fn new(max_chunk_bytes: usize, max_pending_chunks: usize) -> Self {
        Self {
            max_chunk_bytes: max_chunk_bytes.max(1),
            max_pending_chunks: max_pending_chunks.max(1),
            current: String::new(),
            sealed: Vec::new(),
        }
    }

    pub fn pending_chunks(&self) -> usize {
        self.sealed.len() + usize::from(!self.current.is_empty())
    }

    pub fn pending_bytes(&self) -> usize {
        self.current.len() + self.sealed.iter().map(|c| c.len()).sum::<usize>()
    }

    /// Merge one delta. Empty deltas are ignored; sealed-chunk overflow
    /// signals backpressure without dropping text.
    pub fn push(&mut self, delta: &str) -> Result<(), TranscriptError> {
        if delta.is_empty() {
            return Ok(());
        }
        // Reject before mutating whenever this delta could need a new slot.
        if self.pending_chunks() >= self.max_pending_chunks
            && (!self.current.is_empty()
                || self.current.len() + delta.len() >= self.max_chunk_bytes)
        {
            return Err(TranscriptError::BackPressure);
        }
        self.current.push_str(delta);
        while self.current.len() >= self.max_chunk_bytes {
            if self.sealed.len() >= self.max_pending_chunks {
                // Queue full: leave text intact and signal pressure.
                return Err(TranscriptError::BackPressure);
            }
            // `max_chunk_bytes` is an arbitrary byte count, not necessarily a
            // char boundary in `self.current` (arbitrary streamed model
            // output routinely has multi-byte UTF-8 straddling it) —
            // `String::split_off` panics on a non-boundary index. Round up
            // to the next boundary rather than down: this is a batching
            // threshold, not a hard cap, so a sealed chunk landing a few
            // bytes over is harmless, and rounding up (instead of down)
            // guarantees forward progress even if a single character is
            // wider than `max_chunk_bytes` itself.
            let mut split_at = self.max_chunk_bytes;
            while split_at < self.current.len() && !self.current.is_char_boundary(split_at) {
                split_at += 1;
            }
            let rest = self.current.split_off(split_at);
            let sealed = std::mem::take(&mut self.current);
            self.current = rest;
            self.sealed.push(sealed);
        }
        Ok(())
    }

    /// Drain all coalesced chunks (oldest first), including a partial tail.
    pub fn drain(&mut self) -> Vec<String> {
        let mut out = std::mem::take(&mut self.sealed);
        if !self.current.is_empty() {
            out.push(std::mem::take(&mut self.current));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{UiMode, compute_layout};
    use protocol::ArtifactId;

    const GOLDEN_80: &str = "\
user|hello world
assistant|this line wraps at width eighty after enough characters are written
tool|ok
system|ready";

    const GOLDEN_NARROW: &str = "\
user|hello wo
user|rld
assistant|this lin
assistant|e wraps 
assistant|at width
assistant| eighty 
assistant|after en
assistant|ough cha
assistant|racters 
assistant|are writ
assistant|ten
tool|ok";

    fn fixture() -> Transcript {
        let mut t = Transcript::new();
        t.push(RenderBlockKind::User, "hello world");
        t.push(
            RenderBlockKind::Assistant,
            "this line wraps at width eighty after enough characters are written",
        );
        t.push(RenderBlockKind::Tool, "ok");
        t.push(RenderBlockKind::System, "ready");
        t
    }

    fn secret_artifact() -> ArtifactRef {
        ArtifactRef {
            id: ArtifactId::from_bytes(b"secret-body"),
            media_type: "text/plain".to_owned(),
            bytes: 32,
            redaction: RedactionClass::Secret,
        }
    }

    fn public_artifact() -> ArtifactRef {
        ArtifactRef {
            id: ArtifactId::from_bytes(b"tool-output"),
            media_type: "text/plain".to_owned(),
            bytes: 2048,
            redaction: RedactionClass::Public,
        }
    }

    #[test]
    fn wrap_splits_at_width() {
        assert_eq!(wrap_line("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap_line("", 8), vec![""]);
        assert_eq!(wrap_row_count("abcdefghij", 4), 3);
    }

    #[test]
    fn sanitize_on_ingest_strips_osc() {
        let block = RenderBlock::new(
            BlockId::new(1),
            RenderBlockKind::Assistant,
            "pre\u{1b}]8;;https://evil.example\u{07}link\u{1b}]8;;\u{07}post",
        );
        assert_eq!(block.text(), "prelinkpost");
        assert!(!block.text().contains('\u{1b}'));
    }

    #[test]
    fn secret_body_is_never_rendered() {
        let block = RenderBlock::new(BlockId::new(1), RenderBlockKind::Tool, "password=hunter2")
            .with_redaction(RedactionClass::Secret);
        assert!(block.text().is_empty());
        let mut t = Transcript::new();
        t.push_block(block).expect("push");
        let window = TranscriptViewport::new(80, 8).visible(&t);
        assert_eq!(window.rows()[0].text(), REDACTED_PLACEHOLDER);
        assert!(!window.golden().contains("hunter2"));
    }

    #[test]
    fn artifact_is_placeholder_not_payload() {
        let block = RenderBlock::new(BlockId::new(1), RenderBlockKind::Tool, "")
            .with_artifact(public_artifact());
        let mut t = Transcript::new();
        t.push_block(block).expect("push");
        let window = TranscriptViewport::new(80, 4).visible(&t);
        assert_eq!(window.rows()[0].text(), ARTIFACT_PLACEHOLDER);
        assert!(!window.golden().contains("tool-output"));
    }

    #[test]
    fn secret_artifact_cannot_leak_via_inline_text() {
        let block = RenderBlock::new(BlockId::new(1), RenderBlockKind::Tool, "token=abc")
            .with_artifact(secret_artifact());
        assert!(block.text().is_empty());
        let mut t = Transcript::new();
        t.push_block(block).expect("push");
        let window = TranscriptViewport::new(80, 4).visible(&t);
        assert_eq!(window.rows()[0].text(), REDACTED_PLACEHOLDER);
        assert!(!window.golden().contains("token"));
    }

    #[test]
    fn empty_and_zero_size_yield_no_rows() {
        let t = Transcript::new();
        assert!(
            TranscriptViewport::new(80, 20)
                .visible(&t)
                .rows()
                .is_empty()
        );
        let mut t = fixture();
        assert!(TranscriptViewport::new(0, 20).visible(&t).rows().is_empty());
        assert!(TranscriptViewport::new(80, 0).visible(&t).rows().is_empty());
        t.push(RenderBlockKind::User, "");
        let last = t.blocks().last().expect("block");
        assert_eq!(last.raw_line_count(), 0);
    }

    #[test]
    fn golden_80_and_narrow() {
        let t = fixture();
        let wide = TranscriptViewport::new(80, 8).visible(&t);
        assert_eq!(wide.golden(), GOLDEN_80);
        let mut narrow = TranscriptViewport::new(8, 12);
        narrow.set_anchor(ScrollAnchor::new(BlockId::new(0), 0));
        assert_eq!(narrow.visible(&t).golden(), GOLDEN_NARROW);
    }

    #[test]
    fn layout_rect_sizes_viewport() {
        let frame = Rect::new(0, 0, 80, 24);
        let layout = compute_layout(frame, UiMode::Transcript);
        let vp = TranscriptViewport::from_rect(layout.transcript());
        assert_eq!(vp.width(), 80);
        assert_eq!(vp.height(), 20);
    }

    #[test]
    fn anchor_survives_insert_above() {
        let mut t = Transcript::new();
        let first = t.push(RenderBlockKind::User, "keep-me");
        t.push(RenderBlockKind::Assistant, "below");
        let mut vp = TranscriptViewport::new(80, 4);
        vp.set_anchor(ScrollAnchor::new(first, 0));
        t.insert(0, RenderBlockKind::System, "inserted-above")
            .expect("insert");
        let window = vp.visible(&t);
        assert_eq!(window.rows()[0].block_id(), first);
        assert_eq!(window.rows()[0].text(), "keep-me");
        assert_eq!(window.first_block(), Some(1));
    }

    #[test]
    fn anchor_survives_insert_below() {
        let mut t = Transcript::new();
        let first = t.push(RenderBlockKind::User, "keep-me");
        let mut vp = TranscriptViewport::new(80, 3);
        vp.set_anchor(ScrollAnchor::new(first, 0));
        t.push(RenderBlockKind::System, "inserted-below");
        let window = vp.visible(&t);
        assert_eq!(window.rows()[0].block_id(), first);
        assert_eq!(window.rows()[0].text(), "keep-me");
    }

    #[test]
    fn anchor_survives_streamed_deltas_above_and_below() {
        let mut t = Transcript::new();
        let above = t.push(RenderBlockKind::User, "above");
        let mid = t.push(RenderBlockKind::Assistant, "mid");
        let below = t.push(RenderBlockKind::Tool, "below");
        let mut vp = TranscriptViewport::new(40, 1);
        vp.set_anchor(ScrollAnchor::new(mid, 0));
        t.append_delta(above, "\nmore-above-line").expect("delta");
        t.append_delta(below, "\nmore-below-line").expect("delta");
        let window = vp.visible(&t);
        assert_eq!(window.rows().len(), 1);
        assert_eq!(window.rows()[0].block_id(), mid);
        assert_eq!(window.rows()[0].text(), "mid");
    }

    #[test]
    fn streamed_delta_into_anchored_block_keeps_line() {
        let mut t = Transcript::new();
        let mid = t.push(RenderBlockKind::Assistant, "line-a");
        let mut vp = TranscriptViewport::new(40, 1);
        vp.set_anchor(ScrollAnchor::new(mid, 0));
        t.append_delta(mid, "\nline-b\nline-c").expect("delta");
        let window = vp.visible(&t);
        assert_eq!(window.rows()[0].text(), "line-a");
        assert_eq!(window.rows()[0].line_in_block(), 0);
    }

    #[test]
    fn follow_tail_tracks_new_content() {
        let mut t = Transcript::new();
        t.push(RenderBlockKind::User, "old");
        let vp = TranscriptViewport::new(40, 1);
        assert!(vp.follow_tail());
        assert_eq!(vp.visible(&t).rows()[0].text(), "old");
        t.push(RenderBlockKind::Assistant, "new");
        assert_eq!(vp.visible(&t).rows()[0].text(), "new");
    }

    #[test]
    fn scroll_up_disables_follow_tail() {
        let mut t = Transcript::new();
        t.push(RenderBlockKind::User, "one");
        t.push(RenderBlockKind::Assistant, "two");
        let mut vp = TranscriptViewport::new(40, 1);
        vp.scroll_by(&t, -1);
        assert!(!vp.follow_tail());
        assert_eq!(vp.visible(&t).rows()[0].text(), "one");
        t.push(RenderBlockKind::System, "three");
        assert_eq!(vp.visible(&t).rows()[0].text(), "one");
        vp.scroll_by(&t, 2);
        assert!(vp.follow_tail());
        assert_eq!(vp.visible(&t).rows()[0].text(), "three");
    }

    #[test]
    fn chunked_delta_cannot_reassemble_csi() {
        let mut t = Transcript::new();
        let id = t.push(RenderBlockKind::Assistant, "\u{1b}[31");
        t.append_delta(id, "mRED").expect("delta");
        let text = t.get(id).expect("block").text();
        assert_eq!(text, "mRED");
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn hundred_k_line_scroll_is_bounded() {
        const LINES: usize = 100_000;
        let mut t = Transcript::new();
        for i in 0..LINES {
            t.push(RenderBlockKind::System, &format!("L{i}"));
        }
        let mut vp = TranscriptViewport::new(40, 20);
        vp.set_anchor(ScrollAnchor::new(BlockId::new(50_000), 0));

        for _ in 0..64 {
            let window = vp.visible(&t);
            assert_eq!(window.rows().len(), 20);
            assert!(
                window.work().is_bounded(20),
                "frame work not bounded: {:?}",
                window.work()
            );
            assert!(window.rows().iter().all(|row| row.text().starts_with('L')));
            assert_eq!(
                window.rows()[0].block_id(),
                vp.anchor().expect("anchor").block_id()
            );
            vp.scroll_by(&t, 3);
        }

        t.insert(0, RenderBlockKind::User, "head").expect("insert");
        t.push(RenderBlockKind::User, "tail");
        let window = vp.visible(&t);
        assert!(window.work().is_bounded(20));
        assert_eq!(window.rows().len(), 20);
        assert_ne!(window.rows()[0].text(), "head");
        assert_ne!(window.rows().last().expect("row").text(), "tail");
    }

    #[test]
    fn hundred_k_line_single_block_tail_is_bounded() {
        let mut body = String::with_capacity(MAX_INLINE_TEXT_BYTES);
        let mut lines = 0u32;
        while body.len() + 8 < MAX_INLINE_TEXT_BYTES {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&format!("R{lines:05}"));
            lines += 1;
        }
        let mut t = Transcript::new();
        t.push(RenderBlockKind::Assistant, &body);
        let vp = TranscriptViewport::new(20, 16);
        let window = vp.visible(&t);
        assert_eq!(window.rows().len(), 16);
        assert!(window.work().is_bounded(16));
        assert!(
            window.rows().last().expect("row").text().starts_with('R'),
            "{}",
            window.golden()
        );
        assert!(lines > 16);
    }

    #[test]
    fn inline_text_is_capped() {
        let huge = "a".repeat(MAX_INLINE_TEXT_BYTES + 64);
        let block = RenderBlock::new(BlockId::new(9), RenderBlockKind::User, &huge);
        assert!(block.truncated());
        assert_eq!(block.text().len(), MAX_INLINE_TEXT_BYTES);
        let mut t = Transcript::new();
        let id = t.push(RenderBlockKind::User, "seed");
        t.append_delta(id, &huge).expect("delta");
        assert!(t.get(id).expect("block").truncated());
        assert!(t.get(id).expect("block").text().len() <= MAX_INLINE_TEXT_BYTES);
    }

    #[test]
    fn duplicate_block_id_is_typed_error() {
        let mut t = Transcript::new();
        let block = RenderBlock::new(BlockId::new(1), RenderBlockKind::User, "a");
        t.push_block(block.clone()).expect("first");
        assert_eq!(
            t.push_block(block).expect_err("dup"),
            TranscriptError::DuplicateBlock
        );
        assert_eq!(
            t.append_delta(BlockId::new(99), "x").expect_err("missing"),
            TranscriptError::UnknownBlock
        );
    }
}

#[cfg(test)]
mod coalescer_tests {
    use super::*;

    #[test]
    fn merges_consecutive_deltas_into_fewer_chunks_than_events() {
        let mut c = StreamCoalescer::new(MAX_COALESCED_CHUNK_BYTES, MAX_PENDING_CHUNKS);
        for i in 0..100 {
            c.push(&format!("d{i} ")).expect("push");
        }
        let drained = c.drain();
        assert_eq!(drained.len(), 1, "small deltas merge into one chunk");
        assert!(drained[0].starts_with("d0 "));
        assert!(drained[0].ends_with("d99 "));
    }

    #[test]
    fn seals_chunks_at_byte_bound_and_signals_backpressure_when_full() {
        let mut c = StreamCoalescer::new(16, 2);
        // Seal exactly two 16-byte chunks (capacity 2).
        c.push(&"a".repeat(16)).expect("seal 1");
        c.push(&"b".repeat(16)).expect("seal 2");
        assert_eq!(c.pending_chunks(), 2);
        // Any further chunk with the queue full must fail closed.
        let outcome = c.push(&"c".repeat(16));
        assert_eq!(outcome, Err(TranscriptError::BackPressure));
        // Draining relieves pressure.
        let drained = c.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0], "a".repeat(16));
        assert_eq!(drained[1], "b".repeat(16));
        assert_eq!(c.pending_chunks(), 0);
        c.push("ok").expect("push after drain");
        assert_eq!(c.drain(), vec!["ok".to_owned()]);
    }

    #[test]
    fn seal_boundary_landing_inside_a_multibyte_char_does_not_panic() {
        // max_chunk_bytes=3 lands inside '€'s 3-byte UTF-8 encoding (bytes
        // 1..4 of "a€") — the exact shape that made a naive
        // `split_off(max_chunk_bytes)` panic on a non-char-boundary index.
        // Real streamed model deltas routinely contain multi-byte UTF-8
        // (non-English text, emoji, box-drawing characters), so this must
        // never panic regardless of where max_chunk_bytes happens to fall.
        let mut c = StreamCoalescer::new(3, 10);
        c.push("a€")
            .expect("push must not panic on a straddling multi-byte char");
        assert_eq!(c.drain().join(""), "a€", "no text may be dropped");
    }

    #[test]
    fn empty_delta_is_ignored_and_partial_tail_drains() {
        let mut c = StreamCoalescer::new(64, 4);
        c.push("").expect("empty ok");
        assert_eq!(c.pending_chunks(), 0);
        c.push("tail").expect("tail");
        assert_eq!(c.drain(), vec!["tail".to_owned()]);
        assert_eq!(c.pending_bytes(), 0);
    }
}
