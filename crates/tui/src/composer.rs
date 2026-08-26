//! Multi-line composer with history, paste, and an IME-safe text model.
//!
//! [`ComposerModel`] accepts typed [`ComposerCommand`]s. It does not read
//! terminal events and does not issue kernel commands. Submit emits one
//! user-input intent; cancel never discards the draft.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use crate::layout::MAX_COMPOSER_HEIGHT;
use crate::sanitize::sanitize_untrusted;
use crate::state::MAX_COMPOSER_BYTES;

/// Maximum retained submitted prompts (local chrome only).
pub const MAX_HISTORY_ENTRIES: usize = 128;

/// Cursor/selection motion. Independent of key bindings.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    BufferStart,
    BufferEnd,
    WordLeft,
    WordRight,
}

/// Edit, IME, history, submit, and cancel commands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComposerCommand {
    Insert(String),
    InsertNewline,
    Paste(String),
    Backspace,
    Delete,
    Move(Motion),
    Select(Motion),
    SelectAll,
    SetImePreedit { text: String, cursor: usize },
    CommitIme,
    CancelIme,
    HistoryPrev,
    HistoryNext,
    Submit,
    Cancel,
    SetWidth(u16),
}

/// Outcome of a command. Submit is a single user-input event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComposerEvent {
    None,
    Submit { text: String },
    Cancel,
}

/// Typed composer failure. Display never echoes untrusted payload text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ComposerError {
    TooLong,
    InvalidCursor,
}

/// IME-safe multi-line editor. Cursor and selection are grapheme offsets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposerModel {
    text: String,
    cursor: usize,
    sel_anchor: Option<usize>,
    preferred_col: usize,
    ime: Option<ImeState>,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
    draft_cursor: usize,
    width: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ImeState {
    text: String,
    cursor: usize,
}

/// Windowed render of committed text plus optional IME overlay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposerView {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    ime_active: bool,
    truncated: bool,
}

impl Default for ComposerModel {
    fn default() -> Self {
        Self::new()
    }
}

impl ComposerModel {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            sel_anchor: None,
            preferred_col: 0,
            ime: None,
            history: Vec::new(),
            history_index: None,
            draft: String::new(),
            draft_cursor: 0,
            width: 80,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        self.sel_anchor.map(|anchor| ordered(anchor, self.cursor))
    }

    pub fn ime_preedit(&self) -> Option<&str> {
        self.ime.as_ref().map(|ime| ime.text.as_str())
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    /// Wrapped row count clamped to the layout composer cap.
    pub fn preferred_height(&self) -> u16 {
        let count = self.display_lines().len().max(1);
        u16::try_from(count)
            .unwrap_or(MAX_COMPOSER_HEIGHT)
            .clamp(1, MAX_COMPOSER_HEIGHT)
    }

    pub fn apply(&mut self, command: ComposerCommand) -> Result<ComposerEvent, ComposerError> {
        match command {
            ComposerCommand::Insert(text) => self.insert(&text),
            ComposerCommand::InsertNewline => self.insert("\n"),
            ComposerCommand::Paste(text) => self.paste(&text),
            ComposerCommand::Backspace => {
                self.backspace();
                Ok(ComposerEvent::None)
            }
            ComposerCommand::Delete => {
                self.delete_forward();
                Ok(ComposerEvent::None)
            }
            ComposerCommand::Move(motion) => {
                self.move_cursor(motion, false);
                Ok(ComposerEvent::None)
            }
            ComposerCommand::Select(motion) => {
                self.move_cursor(motion, true);
                Ok(ComposerEvent::None)
            }
            ComposerCommand::SelectAll => {
                let end = grapheme_count(&self.text);
                self.sel_anchor = Some(0);
                self.cursor = end;
                self.preferred_col = self.caret_column();
                Ok(ComposerEvent::None)
            }
            ComposerCommand::SetImePreedit { text, cursor } => self.set_ime_preedit(text, cursor),
            ComposerCommand::CommitIme => self.commit_ime(),
            ComposerCommand::CancelIme => {
                self.ime = None;
                Ok(ComposerEvent::None)
            }
            ComposerCommand::HistoryPrev => {
                self.history_prev();
                Ok(ComposerEvent::None)
            }
            ComposerCommand::HistoryNext => {
                self.history_next();
                Ok(ComposerEvent::None)
            }
            ComposerCommand::Submit => Ok(self.submit()),
            ComposerCommand::Cancel => Ok(self.cancel()),
            ComposerCommand::SetWidth(width) => {
                self.width = width.max(1);
                self.preferred_col = self.caret_column();
                Ok(ComposerEvent::None)
            }
        }
    }

    /// Visible rows for `height`, wrapping at `width`. Cursor stays in frame.
    pub fn render(&self, width: u16, height: u16) -> ComposerView {
        let width = width.max(1);
        let height = height.clamp(1, MAX_COMPOSER_HEIGHT);
        let (lines, cursor_line, cursor_col, ime_active) = self.layout_at(width);
        let height = usize::from(height);
        let mut start = 0usize;
        if cursor_line >= height {
            start = cursor_line.saturating_add(1).saturating_sub(height);
        }
        let end = (start + height).min(lines.len());
        let truncated = end < lines.len() || start > 0;
        let shown = if start >= end {
            vec![String::new()]
        } else {
            lines[start..end].to_vec()
        };
        ComposerView {
            lines: shown,
            cursor_line: cursor_line.saturating_sub(start),
            cursor_col,
            ime_active,
            truncated,
        }
    }

    fn insert(&mut self, raw: &str) -> Result<ComposerEvent, ComposerError> {
        self.commit_ime_in_place()?;
        let cleaned = normalize_input(raw);
        if cleaned.is_empty() {
            return Ok(ComposerEvent::None);
        }
        self.replace_selection_with(&cleaned)?;
        Ok(ComposerEvent::None)
    }

    fn paste(&mut self, raw: &str) -> Result<ComposerEvent, ComposerError> {
        self.insert(raw)
    }

    fn backspace(&mut self) {
        if self.ime.is_some() {
            self.ime = None;
            return;
        }
        if self.delete_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let spans = grapheme_spans(&self.text);
        let idx = self.cursor.saturating_sub(1);
        if let Some(&(start, end)) = spans.get(idx) {
            self.text.replace_range(start..end, "");
            self.cursor = idx;
            self.preferred_col = self.caret_column();
        }
    }

    fn delete_forward(&mut self) {
        if self.ime.is_some() {
            self.ime = None;
            return;
        }
        if self.delete_selection() {
            return;
        }
        let spans = grapheme_spans(&self.text);
        if let Some(&(start, end)) = spans.get(self.cursor) {
            self.text.replace_range(start..end, "");
            self.preferred_col = self.caret_column();
        }
    }

    fn set_ime_preedit(
        &mut self,
        raw: String,
        cursor: usize,
    ) -> Result<ComposerEvent, ComposerError> {
        let cleaned = normalize_input(&raw);
        if cleaned.len() > MAX_COMPOSER_BYTES
            || self.text.len().saturating_add(cleaned.len()) > MAX_COMPOSER_BYTES
        {
            return Err(ComposerError::TooLong);
        }
        let count = grapheme_count(&cleaned);
        if cursor > count {
            return Err(ComposerError::InvalidCursor);
        }
        if cleaned.is_empty() {
            self.ime = None;
        } else {
            self.ime = Some(ImeState {
                text: cleaned,
                cursor,
            });
        }
        Ok(ComposerEvent::None)
    }

    fn commit_ime(&mut self) -> Result<ComposerEvent, ComposerError> {
        self.commit_ime_in_place()?;
        Ok(ComposerEvent::None)
    }

    fn commit_ime_in_place(&mut self) -> Result<(), ComposerError> {
        let Some(ime) = self.ime.take() else {
            return Ok(());
        };
        self.replace_selection_with(&ime.text)
    }

    fn submit(&mut self) -> ComposerEvent {
        let _ = self.commit_ime_in_place();
        let text = self.text.clone();
        if !text.is_empty() {
            push_history(&mut self.history, text.clone());
        }
        self.text.clear();
        self.cursor = 0;
        self.sel_anchor = None;
        self.preferred_col = 0;
        self.ime = None;
        self.history_index = None;
        self.draft.clear();
        self.draft_cursor = 0;
        ComposerEvent::Submit { text }
    }

    fn cancel(&mut self) -> ComposerEvent {
        self.ime = None;
        if self.history_index.is_some() {
            self.restore_draft();
        }
        ComposerEvent::Cancel
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        self.ime = None;
        match self.history_index {
            None => {
                self.snapshot_draft();
                let idx = self.history.len() - 1;
                self.load_history(idx);
            }
            Some(0) => {}
            Some(idx) => self.load_history(idx - 1),
        }
    }

    fn history_next(&mut self) {
        let Some(idx) = self.history_index else {
            return;
        };
        self.ime = None;
        if idx + 1 < self.history.len() {
            self.load_history(idx + 1);
        } else {
            self.restore_draft();
        }
    }

    fn snapshot_draft(&mut self) {
        self.draft.clone_from(&self.text);
        self.draft_cursor = self.cursor;
    }

    fn restore_draft(&mut self) {
        self.text.clone_from(&self.draft);
        let max = grapheme_count(&self.text);
        self.cursor = self.draft_cursor.min(max);
        self.sel_anchor = None;
        self.history_index = None;
        self.preferred_col = self.caret_column();
    }

    fn load_history(&mut self, idx: usize) {
        let Some(entry) = self.history.get(idx) else {
            return;
        };
        self.text.clone_from(entry);
        self.cursor = grapheme_count(&self.text);
        self.sel_anchor = None;
        self.history_index = Some(idx);
        self.preferred_col = self.caret_column();
    }

    fn replace_selection_with(&mut self, insert: &str) -> Result<(), ComposerError> {
        let (start, end) = self.selection().unwrap_or((self.cursor, self.cursor));
        let spans = grapheme_spans(&self.text);
        let start_b = byte_at(&spans, start);
        let end_b = byte_at(&spans, end);
        let next_len = self
            .text
            .len()
            .saturating_sub(end_b.saturating_sub(start_b))
            .saturating_add(insert.len());
        if next_len > MAX_COMPOSER_BYTES {
            return Err(ComposerError::TooLong);
        }
        self.text.replace_range(start_b..end_b, insert);
        self.cursor = start.saturating_add(grapheme_count(insert));
        self.sel_anchor = None;
        self.preferred_col = self.caret_column();
        Ok(())
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        if start == end {
            self.sel_anchor = None;
            return false;
        }
        let spans = grapheme_spans(&self.text);
        let start_b = byte_at(&spans, start);
        let end_b = byte_at(&spans, end);
        self.text.replace_range(start_b..end_b, "");
        self.cursor = start;
        self.sel_anchor = None;
        self.preferred_col = self.caret_column();
        true
    }

    fn move_cursor(&mut self, motion: Motion, extend: bool) {
        if self.ime.is_some() {
            self.ime = None;
        }
        if extend {
            if self.sel_anchor.is_none() {
                self.sel_anchor = Some(self.cursor);
            }
        } else if let Some(anchor) = self.sel_anchor.take()
            && matches!(motion, Motion::Left | Motion::Right)
        {
            let (start, end) = ordered(anchor, self.cursor);
            self.cursor = if motion == Motion::Left { start } else { end };
            self.preferred_col = self.caret_column();
            return;
        }
        let next = match motion {
            Motion::Left => self.cursor.saturating_sub(1),
            Motion::Right => {
                let max = grapheme_count(&self.text);
                self.cursor.saturating_add(1).min(max)
            }
            Motion::BufferStart => 0,
            Motion::BufferEnd => grapheme_count(&self.text),
            Motion::LineStart => self.line_start(self.cursor),
            Motion::LineEnd => self.line_end(self.cursor),
            Motion::WordLeft => self.word_left(),
            Motion::WordRight => self.word_right(),
            Motion::Up => self.vertical(-1),
            Motion::Down => self.vertical(1),
        };
        self.cursor = next;
        if !matches!(motion, Motion::Up | Motion::Down) {
            self.preferred_col = self.caret_column();
        }
        if !extend {
            self.sel_anchor = None;
        }
    }

    fn line_start(&self, grapheme: usize) -> usize {
        let spans = grapheme_spans(&self.text);
        let byte = byte_at(&spans, grapheme);
        let start = self.text[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0);
        grapheme_index_at(&spans, start)
    }

    fn line_end(&self, grapheme: usize) -> usize {
        let spans = grapheme_spans(&self.text);
        let byte = byte_at(&spans, grapheme);
        let end = self.text[byte..]
            .find('\n')
            .map(|i| byte + i)
            .unwrap_or(self.text.len());
        grapheme_index_at(&spans, end)
    }

    fn word_left(&self) -> usize {
        let spans = grapheme_spans(&self.text);
        if self.cursor == 0 || spans.is_empty() {
            return 0;
        }
        let mut i = self.cursor;
        while i > 0 && is_word_sep(span_str(&self.text, spans[i - 1])) {
            i -= 1;
        }
        while i > 0 && !is_word_sep(span_str(&self.text, spans[i - 1])) {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let spans = grapheme_spans(&self.text);
        let max = spans.len();
        let mut i = self.cursor;
        while i < max && !is_word_sep(span_str(&self.text, spans[i])) {
            i += 1;
        }
        while i < max && is_word_sep(span_str(&self.text, spans[i])) {
            i += 1;
        }
        i
    }

    fn vertical(&self, dir: i32) -> usize {
        let width = self.width.max(1);
        let (lines, line, _col, _) = self.layout_at(width);
        if lines.is_empty() {
            return 0;
        }
        let target = if dir < 0 {
            line.saturating_sub(1)
        } else {
            (line + 1).min(lines.len().saturating_sub(1))
        };
        if target == line {
            return if dir < 0 {
                0
            } else {
                grapheme_count(&self.text)
            };
        }
        let col = self.preferred_col.min(display_width(&lines[target]));
        let byte_in_line = col_to_byte(&lines[target], col);
        let mut offset = 0usize;
        for (i, row) in lines.iter().enumerate() {
            if i == target {
                let prefix = &self.display_text()[..offset];
                let local = grapheme_count(&row[..byte_in_line.min(row.len())]);
                return grapheme_count(prefix).saturating_add(local);
            }
            offset = offset.saturating_add(row.len());
            if i + 1 < lines.len() && self.hard_break_after(offset) {
                offset = offset.saturating_add(1);
            }
        }
        grapheme_count(&self.text)
    }

    fn hard_break_after(&self, byte: usize) -> bool {
        self.text.get(byte..byte.saturating_add(1)) == Some("\n")
    }

    fn caret_column(&self) -> usize {
        let width = self.width.max(1);
        let (_, _, col, _) = self.layout_at(width);
        col
    }

    fn display_text(&self) -> String {
        match &self.ime {
            None => self.text.clone(),
            Some(ime) => {
                let spans = grapheme_spans(&self.text);
                let at = byte_at(&spans, self.cursor);
                let mut out = String::with_capacity(self.text.len() + ime.text.len());
                out.push_str(&self.text[..at]);
                out.push_str(&ime.text);
                out.push_str(&self.text[at..]);
                out
            }
        }
    }

    fn display_lines(&self) -> Vec<String> {
        wrap_text(&self.display_text(), self.width.max(1))
    }

    fn layout_at(&self, width: u16) -> (Vec<String>, usize, usize, bool) {
        let (display, caret) = self.display_with_caret();
        let lines = wrap_text(&display, width);
        let (line, col) = caret_pos(&display, &lines, caret);
        (lines, line, col, self.ime.is_some())
    }

    fn display_with_caret(&self) -> (String, usize) {
        let spans = grapheme_spans(&self.text);
        let at = byte_at(&spans, self.cursor);
        match &self.ime {
            None => (self.text.clone(), at),
            Some(ime) => {
                let ime_spans = grapheme_spans(&ime.text);
                let ime_at = byte_at(&ime_spans, ime.cursor.min(ime_spans.len()));
                let mut out = String::with_capacity(self.text.len() + ime.text.len());
                out.push_str(&self.text[..at]);
                out.push_str(&ime.text);
                out.push_str(&self.text[at..]);
                (out, at.saturating_add(ime_at))
            }
        }
    }
}

impl ComposerView {
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor_line(&self) -> usize {
        self.cursor_line
    }

    pub fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    pub fn ime_active(&self) -> bool {
        self.ime_active
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Stable dump used by golden tests. Cursor is `|`.
    pub fn golden(&self) -> String {
        if self.lines.is_empty() {
            return "|".to_owned();
        }
        self.lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                if i == self.cursor_line {
                    let split = col_to_byte(line, self.cursor_col).min(line.len());
                    format!("{}|{}", &line[..split], &line[split..])
                } else {
                    line.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Display for ComposerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => f.write_str("composer text exceeds bound"),
            Self::InvalidCursor => f.write_str("composer cursor out of range"),
        }
    }
}

impl Error for ComposerError {}

fn normalize_input(raw: &str) -> String {
    sanitize_untrusted(raw).into_owned()
}

fn push_history(history: &mut Vec<String>, text: String) {
    if history.last().is_some_and(|last| last == &text) {
        return;
    }
    if history.len() >= MAX_HISTORY_ENTRIES {
        history.remove(0);
    }
    history.push(text);
}

fn ordered(a: usize, b: usize) -> (usize, usize) {
    if a <= b { (a, b) } else { (b, a) }
}

fn grapheme_count(text: &str) -> usize {
    grapheme_spans(text).len()
}

fn grapheme_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let end = next_boundary(text, i);
        spans.push((i, end));
        i = end;
    }
    spans
}

fn byte_at(spans: &[(usize, usize)], grapheme: usize) -> usize {
    match spans.get(grapheme) {
        Some(&(start, _)) => start,
        None => spans.last().map(|&(_, end)| end).unwrap_or(0),
    }
}

fn grapheme_index_at(spans: &[(usize, usize)], byte: usize) -> usize {
    for (i, &(_, end)) in spans.iter().enumerate() {
        if byte < end {
            return i;
        }
    }
    spans.len()
}

fn span_str(text: &str, span: (usize, usize)) -> &str {
    text.get(span.0..span.1).unwrap_or("")
}

fn next_boundary(text: &str, start: usize) -> usize {
    if start >= text.len() {
        return text.len();
    }
    let mut chars = text[start..].char_indices().peekable();
    let Some((_, first)) = chars.next() else {
        return start;
    };
    if first == '\r' {
        if text[start + 1..].starts_with('\n') {
            return start + 2;
        }
        return start + first.len_utf8();
    }
    let mut end = start + first.len_utf8();
    let mut prev = first;
    while let Some((off, c)) = chars.peek().copied() {
        if continues_cluster(prev, c) {
            chars.next();
            end = start + off + c.len_utf8();
            if is_regional_indicator(prev) && is_regional_indicator(c) {
                break;
            }
            prev = c;
        } else {
            break;
        }
    }
    end
}

fn continues_cluster(prev: char, c: char) -> bool {
    if is_extend(c) || c == '\u{200D}' {
        return true;
    }
    if prev == '\u{200D}' {
        return true;
    }
    if is_regional_indicator(prev) && is_regional_indicator(c) {
        return true;
    }
    hangul_continues(prev, c)
}

fn is_extend(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{0483}'..='\u{0489}'
            | '\u{0591}'..='\u{05BD}'
            | '\u{05BF}'
            | '\u{05C1}'..='\u{05C2}'
            | '\u{05C4}'..='\u{05C5}'
            | '\u{05C7}'
            | '\u{0610}'..='\u{061A}'
            | '\u{064B}'..='\u{065F}'
            | '\u{0670}'
            | '\u{06D6}'..='\u{06DC}'
            | '\u{06DF}'..='\u{06E4}'
            | '\u{06E7}'..='\u{06E8}'
            | '\u{06EA}'..='\u{06ED}'
            | '\u{0711}'
            | '\u{0730}'..='\u{074A}'
            | '\u{07A6}'..='\u{07B0}'
            | '\u{07EB}'..='\u{07F3}'
            | '\u{07FD}'
            | '\u{0816}'..='\u{0819}'
            | '\u{081B}'..='\u{0823}'
            | '\u{0825}'..='\u{0827}'
            | '\u{0829}'..='\u{082D}'
            | '\u{0859}'..='\u{085B}'
            | '\u{0898}'..='\u{089F}'
            | '\u{08CA}'..='\u{08E1}'
            | '\u{08E3}'..='\u{0903}'
            | '\u{093A}'..='\u{094F}'
            | '\u{0951}'..='\u{0957}'
            | '\u{0962}'..='\u{0963}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{3099}'..='\u{309A}'
            | '\u{A66F}'..='\u{A67D}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FE20}'..='\u{FE2F}'
            | '\u{1F3FB}'..='\u{1F3FF}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

fn hangul_continues(prev: char, c: char) -> bool {
    let l = is_hangul_l(prev);
    let v = is_hangul_v(prev);
    let t = is_hangul_t(prev);
    let lv = is_hangul_lv(prev);
    let lvt = is_hangul_lvt(prev);
    (l && (is_hangul_l(c) || is_hangul_v(c) || is_hangul_lv(c) || is_hangul_lvt(c)))
        || ((lv || v) && (is_hangul_v(c) || is_hangul_t(c)))
        || ((lvt || t) && is_hangul_t(c))
}

fn is_hangul_l(c: char) -> bool {
    matches!(c, '\u{1100}'..='\u{115F}' | '\u{A960}'..='\u{A97C}')
}

fn is_hangul_v(c: char) -> bool {
    matches!(c, '\u{1160}'..='\u{11A7}' | '\u{D7B0}'..='\u{D7C6}')
}

fn is_hangul_t(c: char) -> bool {
    matches!(c, '\u{11A8}'..='\u{11FF}' | '\u{D7CB}'..='\u{D7FB}')
}

fn is_hangul_syllable(c: char) -> bool {
    matches!(c, '\u{AC00}'..='\u{D7A3}')
}

fn hangul_syllable_index(c: char) -> Option<u32> {
    if !is_hangul_syllable(c) {
        return None;
    }
    Some(u32::from(c) - 0xAC00)
}

fn is_hangul_lv(c: char) -> bool {
    hangul_syllable_index(c).is_some_and(|s| s % 28 == 0)
}

fn is_hangul_lvt(c: char) -> bool {
    hangul_syllable_index(c).is_some_and(|s| s % 28 != 0)
}

fn is_word_sep(g: &str) -> bool {
    g.chars()
        .next()
        .is_none_or(|c| c.is_whitespace() || c.is_ascii_punctuation())
}

fn wrap_text(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    if text.is_empty() {
        lines.push(String::new());
        return lines;
    }
    for raw in text.split_inclusive('\n') {
        let (line, nl) = match raw.strip_suffix('\n') {
            Some(body) => (body, true),
            None => (raw, false),
        };
        if line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut cols = 0usize;
        let mut i = 0;
        while i < line.len() {
            let end = next_boundary(line, i);
            let g = &line[i..end];
            let w = grapheme_display_width(g);
            if cols + w > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                cols = 0;
            }
            current.push_str(g);
            cols = cols.saturating_add(w);
            i = end;
        }
        lines.push(current);
        let _ = nl;
    }
    if text.ends_with('\n') {
        lines.push(String::new());
    }
    lines
}

fn caret_pos(text: &str, lines: &[String], caret_byte: usize) -> (usize, usize) {
    if lines.is_empty() {
        return (0, 0);
    }
    let caret_byte = caret_byte.min(text.len());
    let mut consumed = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let line_end = consumed.saturating_add(line.len());
        let hard = text.get(line_end..line_end.saturating_add(1)) == Some("\n");
        let row_end = if hard { line_end + 1 } else { line_end };
        let last = i + 1 == lines.len();
        if caret_byte < row_end
            || (caret_byte == row_end && last && !hard)
            || caret_byte <= line_end
        {
            let into = caret_byte.saturating_sub(consumed).min(line.len());
            if caret_byte >= line_end && hard && caret_byte == line_end {
                return (i, display_width(line));
            }
            if caret_byte >= line_end && hard && !last {
                consumed = row_end;
                continue;
            }
            return (i, display_width(&line[..into]));
        }
        consumed = row_end;
    }
    let last = lines.len() - 1;
    (last, display_width(&lines[last]))
}

fn display_width(text: &str) -> usize {
    let mut cols = 0usize;
    let mut i = 0;
    while i < text.len() {
        let end = next_boundary(text, i);
        cols = cols.saturating_add(grapheme_display_width(&text[i..end]));
        i = end;
    }
    cols
}

fn grapheme_display_width(g: &str) -> usize {
    g.chars().next().map(display_cols).unwrap_or(0)
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

fn col_to_byte(line: &str, col: usize) -> usize {
    let mut cols = 0usize;
    let mut i = 0;
    while i < line.len() {
        if cols >= col {
            return i;
        }
        let end = next_boundary(line, i);
        cols = cols.saturating_add(grapheme_display_width(&line[i..end]));
        i = end;
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_EMPTY: &str = "|";
    const GOLDEN_MULTILINE: &str = "alpha\nbe|ta\ngamma";

    fn apply_ok(model: &mut ComposerModel, command: ComposerCommand) -> ComposerEvent {
        model.apply(command).expect("command")
    }

    #[test]
    fn grapheme_cursor_treats_combining_mark_as_one() {
        let mut model = ComposerModel::new();
        apply_ok(&mut model, ComposerCommand::Insert("e\u{0301}x".into()));
        assert_eq!(grapheme_count(model.text()), 2);
        apply_ok(&mut model, ComposerCommand::Move(Motion::BufferStart));
        apply_ok(&mut model, ComposerCommand::Move(Motion::Right));
        assert_eq!(model.cursor(), 1);
        apply_ok(&mut model, ComposerCommand::Backspace);
        assert_eq!(model.text(), "x");
        assert_eq!(model.cursor(), 0);
    }

    #[test]
    fn grapheme_zwj_emoji_and_flag_are_single_clusters() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let flag = "\u{1F1EF}\u{1F1F5}";
        assert_eq!(grapheme_count(family), 1);
        assert_eq!(grapheme_count(flag), 1);
        let mut model = ComposerModel::new();
        apply_ok(
            &mut model,
            ComposerCommand::Insert(format!("{family}{flag}")),
        );
        assert_eq!(grapheme_count(model.text()), 2);
        apply_ok(&mut model, ComposerCommand::Backspace);
        assert_eq!(model.text(), family);
        apply_ok(&mut model, ComposerCommand::Backspace);
        assert!(model.text().is_empty());
    }

    #[test]
    fn multiline_paste_inserts_normalized_lines() {
        let mut model = ComposerModel::new();
        apply_ok(&mut model, ComposerCommand::Insert("pre".into()));
        apply_ok(&mut model, ComposerCommand::Move(Motion::Left));
        apply_ok(
            &mut model,
            ComposerCommand::Paste("one\r\ntwo\rthree".into()),
        );
        assert_eq!(model.text(), "prone\ntwo\nthreee");
        assert_eq!(model.cursor(), grapheme_count("prone\ntwo\nthree"));
        assert!(model.text().contains('\n'));
        assert!(!model.text().contains('\r'));
        let mut fresh = ComposerModel::new();
        apply_ok(
            &mut fresh,
            ComposerCommand::Paste("alpha\r\nbeta\ngamma".into()),
        );
        assert_eq!(fresh.text(), "alpha\nbeta\ngamma");
        assert_eq!(
            apply_ok(&mut fresh, ComposerCommand::Submit),
            ComposerEvent::Submit {
                text: "alpha\nbeta\ngamma".into(),
            }
        );
    }

    #[test]
    fn paste_strips_terminal_escapes() {
        let mut model = ComposerModel::new();
        apply_ok(
            &mut model,
            ComposerCommand::Paste(
                "pre\u{1b}]8;;https://evil.example\u{07}link\u{1b}]8;;\u{07}post".into(),
            ),
        );
        assert_eq!(model.text(), "prelinkpost");
        assert!(!model.text().contains('\u{1b}'));
    }

    #[test]
    fn paste_over_budget_is_typed_error() {
        let mut model = ComposerModel::new();
        let too_long = "x".repeat(MAX_COMPOSER_BYTES + 1);
        assert_eq!(
            model.apply(ComposerCommand::Paste(too_long)),
            Err(ComposerError::TooLong)
        );
        assert!(model.text().is_empty());
    }

    #[test]
    fn history_recalls_submits_and_restores_draft() {
        let mut model = ComposerModel::new();
        apply_ok(&mut model, ComposerCommand::Insert("first".into()));
        apply_ok(&mut model, ComposerCommand::Submit);
        apply_ok(&mut model, ComposerCommand::Insert("second".into()));
        apply_ok(&mut model, ComposerCommand::Submit);
        apply_ok(&mut model, ComposerCommand::Insert("draft".into()));
        apply_ok(&mut model, ComposerCommand::HistoryPrev);
        assert_eq!(model.text(), "second");
        apply_ok(&mut model, ComposerCommand::HistoryPrev);
        assert_eq!(model.text(), "first");
        apply_ok(&mut model, ComposerCommand::HistoryPrev);
        assert_eq!(model.text(), "first");
        apply_ok(&mut model, ComposerCommand::HistoryNext);
        assert_eq!(model.text(), "second");
        apply_ok(&mut model, ComposerCommand::HistoryNext);
        assert_eq!(model.text(), "draft");
        assert_eq!(model.history(), &["first".to_owned(), "second".to_owned()]);
    }

    #[test]
    fn submit_produces_one_user_input_event() {
        let mut model = ComposerModel::new();
        apply_ok(
            &mut model,
            ComposerCommand::Paste("line1\nline2\nline3".into()),
        );
        let event = apply_ok(&mut model, ComposerCommand::Submit);
        assert_eq!(
            event,
            ComposerEvent::Submit {
                text: "line1\nline2\nline3".into(),
            }
        );
        assert!(model.text().is_empty());
        assert_eq!(model.history().len(), 1);
    }

    #[test]
    fn cancel_does_not_lose_draft() {
        let mut model = ComposerModel::new();
        apply_ok(&mut model, ComposerCommand::Insert("keep me".into()));
        let event = apply_ok(&mut model, ComposerCommand::Cancel);
        assert_eq!(event, ComposerEvent::Cancel);
        assert_eq!(model.text(), "keep me");
        assert_eq!(model.cursor(), grapheme_count("keep me"));
    }

    #[test]
    fn cancel_while_browsing_history_restores_draft() {
        let mut model = ComposerModel::new();
        apply_ok(&mut model, ComposerCommand::Insert("old".into()));
        apply_ok(&mut model, ComposerCommand::Submit);
        apply_ok(&mut model, ComposerCommand::Insert("draft".into()));
        apply_ok(&mut model, ComposerCommand::HistoryPrev);
        assert_eq!(model.text(), "old");
        let event = apply_ok(&mut model, ComposerCommand::Cancel);
        assert_eq!(event, ComposerEvent::Cancel);
        assert_eq!(model.text(), "draft");
    }

    #[test]
    fn ime_preedit_is_not_committed_until_commit() {
        let mut model = ComposerModel::new();
        apply_ok(&mut model, ComposerCommand::Insert("ab".into()));
        apply_ok(&mut model, ComposerCommand::Move(Motion::Left));
        apply_ok(
            &mut model,
            ComposerCommand::SetImePreedit {
                text: "かん".into(),
                cursor: 1,
            },
        );
        assert_eq!(model.text(), "ab");
        assert_eq!(model.ime_preedit(), Some("かん"));
        apply_ok(&mut model, ComposerCommand::CancelIme);
        assert_eq!(model.text(), "ab");
        assert!(model.ime_preedit().is_none());
        apply_ok(
            &mut model,
            ComposerCommand::SetImePreedit {
                text: "漢".into(),
                cursor: 1,
            },
        );
        apply_ok(&mut model, ComposerCommand::CommitIme);
        assert_eq!(model.text(), "a漢b");
        assert!(model.ime_preedit().is_none());
    }

    #[test]
    fn cancel_drops_ime_but_keeps_draft() {
        let mut model = ComposerModel::new();
        apply_ok(&mut model, ComposerCommand::Insert("draft".into()));
        apply_ok(
            &mut model,
            ComposerCommand::SetImePreedit {
                text: "へんかん".into(),
                cursor: 2,
            },
        );
        apply_ok(&mut model, ComposerCommand::Cancel);
        assert_eq!(model.text(), "draft");
        assert!(model.ime_preedit().is_none());
    }

    #[test]
    fn selection_replace_and_delete() {
        let mut model = ComposerModel::new();
        apply_ok(&mut model, ComposerCommand::Insert("abcdef".into()));
        apply_ok(&mut model, ComposerCommand::Move(Motion::BufferStart));
        apply_ok(&mut model, ComposerCommand::Select(Motion::Right));
        apply_ok(&mut model, ComposerCommand::Select(Motion::Right));
        apply_ok(&mut model, ComposerCommand::Select(Motion::Right));
        assert_eq!(model.selection(), Some((0, 3)));
        apply_ok(&mut model, ComposerCommand::Insert("XYZ".into()));
        assert_eq!(model.text(), "XYZdef");
        apply_ok(&mut model, ComposerCommand::SelectAll);
        apply_ok(&mut model, ComposerCommand::Delete);
        assert!(model.text().is_empty());
    }

    #[test]
    fn golden_empty_and_multiline() {
        let mut model = ComposerModel::new();
        assert_eq!(model.render(80, 3).golden(), GOLDEN_EMPTY);
        apply_ok(
            &mut model,
            ComposerCommand::Insert("alpha\nbeta\ngamma".into()),
        );
        apply_ok(&mut model, ComposerCommand::Move(Motion::Up));
        apply_ok(&mut model, ComposerCommand::Move(Motion::Left));
        apply_ok(&mut model, ComposerCommand::Move(Motion::Left));
        assert_eq!(model.render(80, 8).golden(), GOLDEN_MULTILINE);
    }

    #[test]
    fn preferred_height_is_clamped() {
        let mut model = ComposerModel::new();
        let mut body = String::new();
        for i in 0..20 {
            if i > 0 {
                body.push('\n');
            }
            body.push_str("row");
        }
        apply_ok(&mut model, ComposerCommand::Insert(body));
        assert_eq!(model.preferred_height(), MAX_COMPOSER_HEIGHT);
        let view = model.render(80, 20);
        assert_eq!(view.lines().len(), usize::from(MAX_COMPOSER_HEIGHT));
        assert!(view.truncated());
    }
}
