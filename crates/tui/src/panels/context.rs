//! Context inspector over a compiled [`ContextPacket`].
//!
//! [`ContextViewModel`] is a frontend projection. It never compiles context
//! and never writes pin state. Pin/unpin return typed intents after checking
//! the hard compiler capacity already recorded on the packet. Untrusted
//! locators are sanitized before they become visible.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use context_engine::{
    CompileReason, ContextPacket, ContextSource, DEFAULT_MAX_COMPILE_BLOCKS, DropReason, Freshness,
    TrustClass,
};

use crate::sanitize::sanitize_untrusted;
use crate::state::CancellationToken;

/// Maximum compiled plus dropped rows retained in one inspector.
pub const MAX_CONTEXT_BLOCKS: usize = DEFAULT_MAX_COMPILE_BLOCKS.saturating_mul(2);

/// Render width is clamped to this many columns.
pub const MAX_CONTEXT_COLS: u16 = 512;

/// Render height is clamped to this many rows.
pub const MAX_CONTEXT_ROWS: u16 = 256;

const CANCEL_STRIDE: usize = 8;
const STALE_LABEL: &str = "STALE";
const UNTRUSTED_LABEL: &str = "UNTRUSTED";

/// Tree grouping used by the inspector table.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum ContextGroup {
    Mandatory,
    Retrieved,
    Memory,
    ReadSet,
    Dropped,
}

/// Local row cursor. Not domain state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct ContextSelection {
    row_index: usize,
}

/// Typed inspector failure. Display never echoes locators or hashes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ContextInspectError {
    Cancelled,
    BoundExceeded,
    InvalidSelection,
    CapacityExceeded,
    AlreadyPinned,
    NotPinned,
}

/// Kernel-bound pin request. The inspector does not apply it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextPinIntent {
    locator: String,
    content_hash: String,
    tokens: u32,
    source: ContextSource,
}

/// Kernel-bound unpin request. The inspector does not apply it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextUnpinIntent {
    locator: String,
    content_hash: String,
    source: ContextSource,
}

/// One projected compiled or dropped block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextBlockView {
    locator: String,
    display_locator: String,
    content_hash: String,
    group: ContextGroup,
    source: ContextSource,
    reason: CompileReason,
    trust: TrustClass,
    freshness: Freshness,
    tokens: u32,
    pinned: bool,
    included: bool,
    drop_reason: Option<DropReason>,
}

/// Frontend-only projection. Never compiles or persists pins.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextViewModel {
    rows: Vec<ContextBlockView>,
    selection: ContextSelection,
    partitions: Vec<PartitionView>,
    included_tokens: u32,
    context_limit: u32,
    output_reserve: u32,
    safety_margin: u32,
    system_cap: u32,
    mandatory_non_system_tokens: u32,
    included_count: usize,
    max_blocks: usize,
}

/// One painted frame. `golden` omits trailing pad.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextFrame {
    width: u16,
    height: u16,
    lines: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PartitionView {
    source: ContextSource,
    used: u32,
    cap: u32,
}

impl ContextGroup {
    pub const ALL: &'static [Self] = &[
        Self::Mandatory,
        Self::Retrieved,
        Self::Memory,
        Self::ReadSet,
        Self::Dropped,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mandatory => "mandatory",
            Self::Retrieved => "retrieved",
            Self::Memory => "memory",
            Self::ReadSet => "read_set",
            Self::Dropped => "dropped",
        }
    }
}

impl ContextSelection {
    pub const fn new(row_index: usize) -> Self {
        Self { row_index }
    }

    pub const fn row_index(self) -> usize {
        self.row_index
    }
}

impl ContextInspectError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "context inspector cancelled",
            Self::BoundExceeded => "context inspector resource bound exceeded",
            Self::InvalidSelection => "context block selection is out of range",
            Self::CapacityExceeded => "pin exceeds hard compiler capacity",
            Self::AlreadyPinned => "context block is already pinned",
            Self::NotPinned => "context block is not pinned",
        }
    }
}

impl ContextPinIntent {
    pub fn locator(&self) -> &str {
        &self.locator
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub fn tokens(&self) -> u32 {
        self.tokens
    }

    pub fn source(&self) -> ContextSource {
        self.source
    }
}

impl ContextUnpinIntent {
    pub fn locator(&self) -> &str {
        &self.locator
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub fn source(&self) -> ContextSource {
        self.source
    }
}

impl ContextBlockView {
    pub fn locator(&self) -> &str {
        &self.locator
    }

    pub fn display_locator(&self) -> &str {
        &self.display_locator
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub fn group(&self) -> ContextGroup {
        self.group
    }

    pub fn source(&self) -> ContextSource {
        self.source
    }

    pub fn reason(&self) -> CompileReason {
        self.reason
    }

    pub fn trust(&self) -> TrustClass {
        self.trust
    }

    pub fn freshness(&self) -> Freshness {
        self.freshness
    }

    pub fn tokens(&self) -> u32 {
        self.tokens
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    pub fn is_included(&self) -> bool {
        self.included
    }

    pub fn drop_reason(&self) -> Option<DropReason> {
        self.drop_reason
    }

    pub fn is_stale(&self) -> bool {
        self.freshness == Freshness::Stale
    }

    pub fn is_untrusted(&self) -> bool {
        self.trust == TrustClass::Untrusted
    }
}

impl ContextViewModel {
    /// Project `packet` plus selection. Pins are not written.
    pub fn new(
        packet: &ContextPacket,
        selection: ContextSelection,
        cancel: &CancellationToken,
    ) -> Result<Self, ContextInspectError> {
        Self::with_capacity(packet, selection, DEFAULT_MAX_COMPILE_BLOCKS, cancel)
    }

    pub fn with_capacity(
        packet: &ContextPacket,
        selection: ContextSelection,
        max_blocks: usize,
        cancel: &CancellationToken,
    ) -> Result<Self, ContextInspectError> {
        check_cancel(cancel)?;
        let mut rows = Vec::new();
        for (index, block) in packet.blocks().iter().enumerate() {
            if index.is_multiple_of(CANCEL_STRIDE) {
                check_cancel(cancel)?;
            }
            if rows.len() >= MAX_CONTEXT_BLOCKS {
                return Err(ContextInspectError::BoundExceeded);
            }
            rows.push(project_included(block));
        }
        for (index, dropped) in packet.dropped().iter().enumerate() {
            if index.is_multiple_of(CANCEL_STRIDE) {
                check_cancel(cancel)?;
            }
            if rows.len() >= MAX_CONTEXT_BLOCKS {
                return Err(ContextInspectError::BoundExceeded);
            }
            rows.push(project_dropped(dropped));
        }
        rows.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then(a.source.cmp(&b.source))
                .then(a.locator.as_str().cmp(b.locator.as_str()))
        });

        let partitions = ContextSource::ALL
            .iter()
            .map(|source| {
                let budget = packet.partitions().by_source(*source);
                PartitionView {
                    source: *source,
                    used: budget.used(),
                    cap: budget.cap(),
                }
            })
            .collect();

        let included_count = packet.blocks().len();
        let row_index = if rows.is_empty() {
            0
        } else {
            selection.row_index.min(rows.len() - 1)
        };
        Ok(Self {
            rows,
            selection: ContextSelection { row_index },
            partitions,
            included_tokens: packet.included_tokens(),
            context_limit: packet.partitions().context_limit(),
            output_reserve: packet.reserved_output(),
            safety_margin: packet.partitions().safety_margin(),
            system_cap: packet.partitions().system().cap(),
            mandatory_non_system_tokens: mandatory_non_system_tokens(packet),
            included_count,
            max_blocks,
        })
    }

    pub fn from_packet(
        packet: &ContextPacket,
        cancel: &CancellationToken,
    ) -> Result<Self, ContextInspectError> {
        Self::new(packet, ContextSelection::default(), cancel)
    }

    pub fn rows(&self) -> &[ContextBlockView] {
        &self.rows
    }

    pub fn selected_block(&self) -> Option<&ContextBlockView> {
        self.rows.get(self.selection.row_index)
    }

    pub fn selection(&self) -> ContextSelection {
        self.selection
    }

    pub fn included_tokens(&self) -> u32 {
        self.included_tokens
    }

    pub fn context_limit(&self) -> u32 {
        self.context_limit
    }

    pub fn output_reserve(&self) -> u32 {
        self.output_reserve
    }

    pub fn safety_margin(&self) -> u32 {
        self.safety_margin
    }

    pub fn max_blocks(&self) -> usize {
        self.max_blocks
    }

    /// This panel never compiles context or persists pin state.
    pub const fn mutates_compiler(&self) -> bool {
        false
    }

    pub fn select_row(&self, index: usize) -> Result<Self, ContextInspectError> {
        if self.rows.is_empty() {
            if index == 0 {
                return Ok(self.clone());
            }
            return Err(ContextInspectError::InvalidSelection);
        }
        if index >= self.rows.len() {
            return Err(ContextInspectError::InvalidSelection);
        }
        let mut next = self.clone();
        next.selection.row_index = index;
        Ok(next)
    }

    pub fn select_next(&self) -> Self {
        let mut next = self.clone();
        if !self.rows.is_empty() && self.selection.row_index + 1 < self.rows.len() {
            next.selection.row_index += 1;
        }
        next
    }

    pub fn select_prev(&self) -> Self {
        let mut next = self.clone();
        next.selection.row_index = self.selection.row_index.saturating_sub(1);
        next
    }

    pub fn select_locator(&self, locator: &str) -> Result<Self, ContextInspectError> {
        let index = self
            .rows
            .iter()
            .position(|row| row.locator == locator)
            .ok_or(ContextInspectError::InvalidSelection)?;
        self.select_row(index)
    }

    /// Request a pin. Capacity is checked against the compiled packet.
    ///
    /// Success is only [`ContextPinIntent`]. The view is not mutated.
    pub fn pin(&self, cancel: &CancellationToken) -> Result<ContextPinIntent, ContextInspectError> {
        check_cancel(cancel)?;
        let row = self
            .selected_block()
            .ok_or(ContextInspectError::InvalidSelection)?;
        self.pin_row(row)
    }

    pub fn pin_locator(
        &self,
        locator: &str,
        cancel: &CancellationToken,
    ) -> Result<ContextPinIntent, ContextInspectError> {
        check_cancel(cancel)?;
        let row = self
            .rows
            .iter()
            .find(|row| row.locator == locator)
            .ok_or(ContextInspectError::InvalidSelection)?;
        self.pin_row(row)
    }

    /// Request an unpin. The view is not mutated.
    pub fn unpin(
        &self,
        cancel: &CancellationToken,
    ) -> Result<ContextUnpinIntent, ContextInspectError> {
        check_cancel(cancel)?;
        let row = self
            .selected_block()
            .ok_or(ContextInspectError::InvalidSelection)?;
        self.unpin_row(row)
    }

    pub fn unpin_locator(
        &self,
        locator: &str,
        cancel: &CancellationToken,
    ) -> Result<ContextUnpinIntent, ContextInspectError> {
        check_cancel(cancel)?;
        let row = self
            .rows
            .iter()
            .find(|row| row.locator == locator)
            .ok_or(ContextInspectError::InvalidSelection)?;
        self.unpin_row(row)
    }

    pub fn render(&self, width: u16, height: u16) -> ContextFrame {
        let width = width.min(MAX_CONTEXT_COLS);
        let height = height.min(MAX_CONTEXT_ROWS);
        if width == 0 || height == 0 {
            return ContextFrame {
                width,
                height,
                lines: Vec::new(),
            };
        }
        let mut lines = vec![format!(
            "context tokens:{}/{} reserved:{} safety:{}",
            self.included_tokens, self.context_limit, self.output_reserve, self.safety_margin
        )];
        lines.push("partitions:".to_owned());
        for part in &self.partitions {
            lines.push(format!(
                "  {} {}/{}",
                part.source.as_str(),
                part.used,
                part.cap
            ));
        }
        for group in ContextGroup::ALL {
            lines.push(format!("{}:", group.as_str()));
            let mut any = false;
            for (index, row) in self.rows.iter().enumerate() {
                if row.group != *group {
                    continue;
                }
                any = true;
                let marker = if index == self.selection.row_index {
                    '>'
                } else {
                    ' '
                };
                lines.push(format!("{marker} {}", format_row(row)));
            }
            if !any {
                lines.push("  (empty)".to_owned());
            }
        }
        if lines.len() > usize::from(height) {
            lines.truncate(usize::from(height));
        }
        ContextFrame {
            width,
            height,
            lines,
        }
    }

    fn pin_row(&self, row: &ContextBlockView) -> Result<ContextPinIntent, ContextInspectError> {
        if !row.source.is_optional() {
            return Err(ContextInspectError::InvalidSelection);
        }
        if row.pinned {
            return Err(ContextInspectError::AlreadyPinned);
        }
        self.check_pin_capacity(row)?;
        Ok(ContextPinIntent {
            locator: row.locator.clone(),
            content_hash: row.content_hash.clone(),
            tokens: row.tokens,
            source: row.source,
        })
    }

    fn unpin_row(&self, row: &ContextBlockView) -> Result<ContextUnpinIntent, ContextInspectError> {
        if !row.pinned {
            return Err(ContextInspectError::NotPinned);
        }
        Ok(ContextUnpinIntent {
            locator: row.locator.clone(),
            content_hash: row.content_hash.clone(),
            source: row.source,
        })
    }

    fn check_pin_capacity(&self, row: &ContextBlockView) -> Result<(), ContextInspectError> {
        if row.included {
            return Ok(());
        }
        if self.included_count.saturating_add(1) > self.max_blocks {
            return Err(ContextInspectError::CapacityExceeded);
        }
        let after_reserved = self
            .context_limit
            .saturating_sub(self.output_reserve)
            .saturating_sub(self.safety_margin)
            .saturating_sub(self.system_cap);
        let new_mandatory = self.mandatory_non_system_tokens.saturating_add(row.tokens);
        if new_mandatory > after_reserved {
            return Err(ContextInspectError::CapacityExceeded);
        }
        Ok(())
    }
}

impl ContextFrame {
    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Stable dump used by golden tests. Trailing pad is omitted.
    pub fn golden(&self) -> String {
        self.lines.join("\n")
    }

    /// Exact-width rows written into the pane, padded/truncated to `height`.
    pub fn text(&self) -> String {
        let width = usize::from(self.width);
        let height = usize::from(self.height);
        let mut rows = Vec::with_capacity(height);
        for i in 0..height {
            let src = self.lines.get(i).map(String::as_str).unwrap_or("");
            rows.push(fit_width(src, width));
        }
        rows.join("\n")
    }
}

impl Display for ContextInspectError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ContextInspectError {}

fn project_included(block: &context_engine::ContextBlock) -> ContextBlockView {
    let locator = block.locator().to_owned();
    ContextBlockView {
        display_locator: sanitize_untrusted(&locator).into_owned(),
        locator,
        content_hash: block.content_hash().to_string(),
        group: group_for(block.source(), block.is_mandatory(), true),
        source: block.source(),
        reason: block.reason(),
        trust: block.trust(),
        freshness: block.freshness(),
        tokens: block.estimated_tokens(),
        pinned: block.is_pinned(),
        included: true,
        drop_reason: None,
    }
}

fn project_dropped(dropped: &context_engine::DroppedBlock) -> ContextBlockView {
    let locator = dropped.locator().to_owned();
    ContextBlockView {
        display_locator: sanitize_untrusted(&locator).into_owned(),
        locator,
        content_hash: dropped.content_hash().to_string(),
        group: ContextGroup::Dropped,
        source: dropped.source(),
        reason: dropped_reason(dropped.source()),
        trust: dropped_trust(dropped.source()),
        freshness: Freshness::Unknown,
        tokens: dropped.estimated_tokens(),
        pinned: false,
        included: false,
        drop_reason: Some(dropped.reason()),
    }
}

fn group_for(source: ContextSource, mandatory: bool, included: bool) -> ContextGroup {
    if !included {
        return ContextGroup::Dropped;
    }
    if mandatory {
        return ContextGroup::Mandatory;
    }
    match source {
        ContextSource::Retrieved => ContextGroup::Retrieved,
        ContextSource::Memory => ContextGroup::Memory,
        ContextSource::ReadSet => ContextGroup::ReadSet,
        ContextSource::System | ContextSource::User | ContextSource::Goal | ContextSource::Diff => {
            ContextGroup::Mandatory
        }
    }
}

fn format_row(row: &ContextBlockView) -> String {
    let pin = if row.pinned { "yes" } else { "no" };
    let mut line = format!(
        "{} tokens:{} source:{} reason:{} trust:{} freshness:{} pin:{pin}",
        row.display_locator,
        row.tokens,
        row.source.as_str(),
        row.reason.as_str(),
        row.trust.as_str(),
        row.freshness.as_str(),
    );
    if row.is_stale() {
        line.push(' ');
        line.push_str(STALE_LABEL);
    }
    if row.is_untrusted() {
        line.push(' ');
        line.push_str(UNTRUSTED_LABEL);
    }
    if let Some(reason) = row.drop_reason {
        line.push_str(" dropped:");
        line.push_str(reason.as_str());
    }
    line
}

fn mandatory_non_system_tokens(packet: &ContextPacket) -> u32 {
    packet
        .blocks()
        .iter()
        .filter(|block| block.is_mandatory() && block.source() != ContextSource::System)
        .fold(0u32, |acc, block| {
            acc.saturating_add(block.estimated_tokens())
        })
}

fn fit_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cols = text.chars().count();
    if cols == width {
        return text.to_owned();
    }
    if cols < width {
        let mut out = text.to_owned();
        out.extend(std::iter::repeat_n(' ', width - cols));
        return out;
    }
    text.chars().take(width).collect()
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), ContextInspectError> {
    if cancel.is_cancelled() {
        Err(ContextInspectError::Cancelled)
    } else {
        Ok(())
    }
}

fn dropped_reason(source: ContextSource) -> CompileReason {
    match source {
        ContextSource::System => CompileReason::System,
        ContextSource::User => CompileReason::User,
        ContextSource::Goal => CompileReason::Goal,
        ContextSource::Diff => CompileReason::Diff,
        ContextSource::Retrieved => CompileReason::Retrieved,
        ContextSource::Memory => CompileReason::Memory,
        ContextSource::ReadSet => CompileReason::ReadSet,
    }
}

fn dropped_trust(source: ContextSource) -> TrustClass {
    match source {
        ContextSource::System
        | ContextSource::User
        | ContextSource::Goal
        | ContextSource::Memory => TrustClass::Project,
        ContextSource::Diff | ContextSource::Retrieved | ContextSource::ReadSet => {
            TrustClass::Untrusted
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_engine::{CompileContext, CompileInput, compile};

    const GOLDEN_80: &str = "\
context tokens:65/500 reserved:50 safety:0
partitions:
  system 8/8
  user 10/10
  goal 5/5
  diff 12/12
  retrieved 20/250
  memory 6/103
  read_set 4/62
mandatory:
> sys tokens:8 source:system reason:system trust:project freshness:unknown pin:no
  user tokens:10 source:user reason:user trust:project freshness:unknown pin:no
  goal tokens:5 source:goal reason:goal trust:project freshness:unknown pin:no
  src/lib.rs:10-20 tokens:12 source:diff reason:error trust:untrusted freshness:fresh pin:no UNTRUSTED
retrieved:
  src/main.rs:1-40 tokens:20 source:retrieved reason:retrieved trust:untrusted freshness:stale pin:no STALE UNTRUSTED
memory:
  prefers-exact-ranges tokens:6 source:memory reason:memory trust:project freshness:unknown pin:no
read_set:
  src/lib.rs:1-8 tokens:4 source:read_set reason:read_set trust:untrusted freshness:fresh pin:no UNTRUSTED
dropped:
  (empty)";

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn block(locator: &str, tokens: u32) -> CompileInput {
        CompileInput::new(locator, locator).tokens(tokens)
    }

    fn request(limit: u32, reserve: u32) -> CompileContext {
        CompileContext::new(limit, reserve).safety_margin(0)
    }

    fn fixture_packet() -> ContextPacket {
        let req = request(500, 50)
            .system(block("sys", 8))
            .user(block("user", 10))
            .goal_block(block("goal", 5))
            .diff(
                block("src/lib.rs:10-20", 12)
                    .reason(CompileReason::Error)
                    .freshness(Freshness::Fresh),
            )
            .retrieved(
                block("src/main.rs:1-40", 20)
                    .score(80)
                    .freshness(Freshness::Stale),
            )
            .memory(block("prefers-exact-ranges", 6).score(40))
            .read_set(block("src/lib.rs:1-8", 4).freshness(Freshness::Fresh));
        compile(&req).expect("compile")
    }

    fn fixture_model() -> ContextViewModel {
        ContextViewModel::from_packet(&fixture_packet(), &cancel()).expect("model")
    }

    #[test]
    fn golden_80_120_200() {
        let model = fixture_model();
        assert_eq!(model.render(80, 24).golden(), GOLDEN_80);
        assert_eq!(model.render(120, 24).golden(), GOLDEN_80);
        assert_eq!(model.render(200, 24).golden(), GOLDEN_80);
        assert_eq!(model.render(80, 24).text().lines().count(), 24);
        assert!(
            model
                .render(80, 24)
                .text()
                .lines()
                .all(|line| line.chars().count() == 80)
        );
    }

    #[test]
    fn shows_tokens_source_reason_trust_freshness() {
        let model = fixture_model();
        let diff = model
            .rows()
            .iter()
            .find(|row| row.locator() == "src/lib.rs:10-20")
            .expect("diff");
        assert_eq!(diff.tokens(), 12);
        assert_eq!(diff.source(), ContextSource::Diff);
        assert_eq!(diff.reason(), CompileReason::Error);
        assert_eq!(diff.trust(), TrustClass::Untrusted);
        assert_eq!(diff.freshness(), Freshness::Fresh);
        assert_eq!(diff.group(), ContextGroup::Mandatory);
    }

    #[test]
    fn stale_and_untrusted_labels_visible() {
        let golden = fixture_model().render(80, 24).golden();
        assert!(golden.contains("STALE"));
        assert!(golden.contains("UNTRUSTED"));
        assert!(golden.contains("freshness:stale"));
        assert!(golden.contains("trust:untrusted"));
    }

    #[test]
    fn groups_by_mandatory_retrieved_memory_read_set() {
        let model = fixture_model();
        let groups: Vec<ContextGroup> = model.rows().iter().map(ContextBlockView::group).collect();
        assert!(groups.contains(&ContextGroup::Mandatory));
        assert!(groups.contains(&ContextGroup::Retrieved));
        assert!(groups.contains(&ContextGroup::Memory));
        assert!(groups.contains(&ContextGroup::ReadSet));
        let golden = model.render(80, 24).golden();
        assert!(golden.contains("mandatory:"));
        assert!(golden.contains("retrieved:"));
        assert!(golden.contains("memory:"));
        assert!(golden.contains("read_set:"));
        assert!(golden.contains("partitions:"));
    }

    #[test]
    fn pin_of_included_optional_returns_intent() {
        let model = fixture_model();
        let intent = model
            .pin_locator("src/main.rs:1-40", &cancel())
            .expect("pin");
        assert_eq!(intent.locator(), "src/main.rs:1-40");
        assert_eq!(intent.source(), ContextSource::Retrieved);
        assert_eq!(intent.tokens(), 20);
        assert!(!model.mutates_compiler());
        assert!(
            !model
                .rows()
                .iter()
                .find(|row| row.locator() == "src/main.rs:1-40")
                .expect("row")
                .is_pinned()
        );
    }

    #[test]
    fn pin_exceeding_token_capacity_fails_closed() {
        let req = request(50, 20)
            .user(block("user", 10))
            .retrieved(block("huge-pin", 30).score(1));
        let packet = compile(&req).expect("compile dropped pin");
        assert!(
            packet
                .dropped()
                .iter()
                .any(|dropped| dropped.locator() == "huge-pin")
        );
        let model = ContextViewModel::from_packet(&packet, &cancel()).expect("model");
        assert_eq!(
            model.pin_locator("huge-pin", &cancel()),
            Err(ContextInspectError::CapacityExceeded)
        );
        assert_eq!(
            ContextInspectError::CapacityExceeded.as_str(),
            "pin exceeds hard compiler capacity"
        );
    }

    #[test]
    fn pin_exceeding_block_capacity_fails_closed() {
        let req = request(80, 20)
            .user(block("user", 10))
            .retrieved(block("keep", 20).score(90))
            .retrieved(block("overflow", 15).score(1));
        let packet = compile(&req).expect("compile");
        assert!(
            packet
                .dropped()
                .iter()
                .any(|dropped| dropped.locator() == "overflow")
        );
        let model = ContextViewModel::with_capacity(
            &packet,
            ContextSelection::default(),
            packet.blocks().len(),
            &cancel(),
        )
        .expect("model");
        assert_eq!(
            model.pin_locator("overflow", &cancel()),
            Err(ContextInspectError::CapacityExceeded)
        );
    }

    #[test]
    fn unpin_returns_intent_without_mutating() {
        let req = request(100, 20)
            .user(block("user", 10))
            .retrieved(block("keep-pin", 20).score(1))
            .pin_locator("keep-pin");
        let packet = compile(&req).expect("pinned");
        let model = ContextViewModel::from_packet(&packet, &cancel()).expect("model");
        let pinned = model
            .rows()
            .iter()
            .find(|row| row.locator() == "keep-pin")
            .expect("row");
        assert!(pinned.is_pinned());
        assert_eq!(pinned.group(), ContextGroup::Mandatory);
        let intent = model.unpin_locator("keep-pin", &cancel()).expect("unpin");
        assert_eq!(intent.locator(), "keep-pin");
        assert!(
            model
                .rows()
                .iter()
                .find(|row| row.locator() == "keep-pin")
                .expect("still")
                .is_pinned()
        );
        assert_eq!(
            model.unpin_locator("user", &cancel()),
            Err(ContextInspectError::NotPinned)
        );
    }

    #[test]
    fn pin_of_mandatory_source_is_rejected() {
        let model = fixture_model();
        assert_eq!(
            model.pin_locator("sys", &cancel()),
            Err(ContextInspectError::InvalidSelection)
        );
    }

    #[test]
    fn already_pinned_fails_closed() {
        let req = request(100, 20)
            .user(block("user", 10))
            .retrieved(block("keep-pin", 20).score(1))
            .pin_locator("keep-pin");
        let packet = compile(&req).expect("pinned");
        let model = ContextViewModel::from_packet(&packet, &cancel()).expect("model");
        assert_eq!(
            model.pin_locator("keep-pin", &cancel()),
            Err(ContextInspectError::AlreadyPinned)
        );
    }

    #[test]
    fn navigation_moves_selection() {
        let model = fixture_model();
        assert_eq!(
            model.selected_block().map(ContextBlockView::locator),
            Some("sys")
        );
        let next = model.select_next();
        assert_eq!(
            next.selected_block().map(ContextBlockView::locator),
            Some("user")
        );
        assert!(next.render(80, 24).golden().contains("> user "));
        let prev = next.select_prev();
        assert_eq!(
            prev.selected_block().map(ContextBlockView::locator),
            Some("sys")
        );
    }

    #[test]
    fn sanitizes_untrusted_locator() {
        let dirty = "src/\u{001B}]8;;https://evil.example\u{0007}lib.rs";
        let req = request(100, 20).user(block(dirty, 10));
        let packet = compile(&req).expect("compile");
        let model = ContextViewModel::from_packet(&packet, &cancel()).expect("model");
        let row = model
            .rows()
            .iter()
            .find(|row| row.locator() == dirty)
            .expect("row");
        assert!(!row.display_locator().contains('\u{001B}'));
        assert!(!row.display_locator().contains("https://evil.example"));
        let golden = model.render(80, 24).golden();
        assert!(!golden.contains('\u{001B}'));
        assert!(!golden.contains("https://evil.example"));
    }

    #[test]
    fn construct_honors_cancellation() {
        let token = cancel();
        token.cancel();
        assert_eq!(
            ContextViewModel::from_packet(&fixture_packet(), &token),
            Err(ContextInspectError::Cancelled)
        );
    }

    #[test]
    fn pin_honors_cancellation() {
        let model = fixture_model();
        let token = cancel();
        token.cancel();
        assert_eq!(
            model.pin_locator("src/main.rs:1-40", &token),
            Err(ContextInspectError::Cancelled)
        );
    }
}
