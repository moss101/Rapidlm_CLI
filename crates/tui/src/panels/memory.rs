//! Memory inspector over durable context-engine records.
//!
//! [`MemoryViewModel`] is a frontend projection. It never writes, deletes, or
//! retargets memory. Delete/disable-write return typed intents for the kernel
//! client / context service. Untrusted source and body text is sanitized.
//! Secret-classified content is redacted before it is retained in the view.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use context_engine::{DEFAULT_MAX_MEMORY_RESULTS, MemoryRecord, MemoryScopeKind, MemorySourceKind};
use protocol::RedactionClass;

use crate::sanitize::sanitize_untrusted;
use crate::state::CancellationToken;

/// Maximum projected rows retained in one inspector.
pub const MAX_MEMORY_ITEMS: usize = DEFAULT_MAX_MEMORY_RESULTS as usize;

/// Render width is clamped to this many columns.
pub const MAX_MEMORY_COLS: u16 = 512;

/// Render height is clamped to this many rows.
pub const MAX_MEMORY_ROWS: u16 = 256;

/// Maximum UTF-8 bytes retained for one identifier or source token.
pub const MAX_MEMORY_ID_BYTES: usize = 128;

/// Maximum UTF-8 bytes retained for a timestamp string.
pub const MAX_TIMESTAMP_BYTES: usize = 40;

const CANCEL_STRIDE: usize = 8;
const REDACTED: &str = "[REDACTED]";
const EXPIRED_LABEL: &str = "EXPIRED";
const MAX_CONTENT_PREVIEW_CHARS: usize = 48;

const SCOPE_ORDER: &[MemoryScopeKind] = &[
    MemoryScopeKind::User,
    MemoryScopeKind::Project,
    MemoryScopeKind::Session,
];

/// Local row cursor. Not domain state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct MemorySelection {
    row_index: usize,
}

/// Typed inspector failure. Display never echoes content, IDs, or source text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MemoryInspectError {
    Cancelled,
    BoundExceeded,
    InvalidSelection,
    InvalidField,
    InvalidConfidence,
    AlreadyDisabled,
    ScopePromotionDenied,
    ScopeEditDenied,
}

/// Kernel-bound delete request. The inspector does not apply it.
///
/// Carries the stable identity the context service needs to persist and audit
/// the deletion. The view is not mutated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryDeleteIntent {
    memory_id: String,
    scope: MemoryScopeKind,
    content_hash: String,
}

/// Kernel-bound write-gate request. The inspector does not apply it.
///
/// `enabled` is always `false` for the disable-writes action.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct MemoryDisableWritesIntent {
    enabled: bool,
}

/// Observation copied from a kernel/context-service record.
///
/// This is not a store. Secret bodies must be classified so the view can
/// redact them before they become visible.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryObservation {
    id: String,
    scope: MemoryScopeKind,
    project_id: Option<String>,
    session_id: Option<String>,
    source_kind: MemorySourceKind,
    source_id: String,
    confidence: f64,
    content: String,
    content_hash: String,
    created_at: String,
    expires_at: Option<String>,
    redaction: RedactionClass,
}

/// One projected row. Secret plaintext is never stored here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryRowView {
    id: String,
    display_id: String,
    scope: MemoryScopeKind,
    project_id: Option<String>,
    session_id: Option<String>,
    source_kind: MemorySourceKind,
    display_source: String,
    confidence: String,
    expires_at: Option<String>,
    display_expiry: String,
    display_content: String,
    content_hash: String,
    redaction: RedactionClass,
    expired: bool,
}

/// Frontend-only projection. Never persists delete or write-gate changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryViewModel {
    rows: Vec<MemoryRowView>,
    selection: MemorySelection,
    writes_enabled: bool,
}

/// One painted frame. `golden` omits trailing pad.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryFrame {
    width: u16,
    height: u16,
    lines: Vec<String>,
}

impl MemorySelection {
    pub const fn new(row_index: usize) -> Self {
        Self { row_index }
    }

    pub const fn row_index(self) -> usize {
        self.row_index
    }
}

impl MemoryInspectError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "memory inspector cancelled",
            Self::BoundExceeded => "memory inspector resource bound exceeded",
            Self::InvalidSelection => "memory selection is out of range",
            Self::InvalidField => "memory observation field is invalid",
            Self::InvalidConfidence => "memory confidence is out of range",
            Self::AlreadyDisabled => "memory writes are already disabled",
            Self::ScopePromotionDenied => "memory cannot be edited into a higher scope",
            Self::ScopeEditDenied => "memory inspector cannot retarget scope",
        }
    }
}

impl MemoryDeleteIntent {
    pub fn memory_id(&self) -> &str {
        &self.memory_id
    }

    pub fn scope(&self) -> MemoryScopeKind {
        self.scope
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    /// Deletion must be committed and audited by the kernel, not this view.
    pub const fn requires_durable_audit(&self) -> bool {
        true
    }
}

impl MemoryDisableWritesIntent {
    pub const fn enabled(self) -> bool {
        self.enabled
    }

    /// Write-gate changes must be committed by the kernel, not this view.
    pub const fn requires_durable_audit(self) -> bool {
        true
    }
}

impl MemoryObservation {
    /// Construct an observation. Source and scope are explicit.
    pub fn new(
        id: impl Into<String>,
        scope: MemoryScopeKind,
        source_kind: MemorySourceKind,
        source_id: impl Into<String>,
        confidence: f64,
        content: impl Into<String>,
    ) -> Result<Self, MemoryInspectError> {
        let observation = Self {
            id: id.into(),
            scope,
            project_id: None,
            session_id: None,
            source_kind,
            source_id: source_id.into(),
            confidence,
            content: content.into(),
            content_hash: String::new(),
            created_at: String::new(),
            expires_at: None,
            redaction: RedactionClass::Project,
        };
        observation.validate_core()?;
        Ok(observation)
    }

    /// Copy a durable record. Redaction defaults to [`RedactionClass::Project`].
    pub fn from_record(record: &MemoryRecord) -> Self {
        Self {
            id: record.id().to_string(),
            scope: record.scope().kind(),
            project_id: record.scope().project_id().map(|id| id.to_string()),
            session_id: record.scope().session_id().map(|id| id.to_string()),
            source_kind: record.source().kind(),
            source_id: record.source().id().to_owned(),
            confidence: record.confidence(),
            content: record.content().to_owned(),
            content_hash: record.content_hash().to_string(),
            created_at: record.created_at().as_str().to_owned(),
            expires_at: record.expires_at().map(|ts| ts.as_str().to_owned()),
            redaction: RedactionClass::Project,
        }
    }

    pub fn with_project(mut self, project_id: impl Into<String>) -> Self {
        self.project_id = Some(project_id.into());
        self
    }

    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_created_at(mut self, created_at: impl Into<String>) -> Self {
        self.created_at = created_at.into();
        self
    }

    pub fn with_expires_at(mut self, expires_at: impl Into<String>) -> Self {
        self.expires_at = Some(expires_at.into());
        self
    }

    pub fn with_content_hash(mut self, content_hash: impl Into<String>) -> Self {
        self.content_hash = content_hash.into();
        self
    }

    pub fn with_redaction(mut self, redaction: RedactionClass) -> Self {
        self.redaction = redaction;
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn scope(&self) -> MemoryScopeKind {
        self.scope
    }

    pub fn source_kind(&self) -> MemorySourceKind {
        self.source_kind
    }

    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    pub fn confidence(&self) -> f64 {
        self.confidence
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn redaction(&self) -> RedactionClass {
        self.redaction
    }

    pub fn expires_at(&self) -> Option<&str> {
        self.expires_at.as_deref()
    }

    fn validate_core(&self) -> Result<(), MemoryInspectError> {
        if !bounded_id(&self.id) {
            return Err(MemoryInspectError::InvalidField);
        }
        if !bounded_id(&self.source_id) {
            return Err(MemoryInspectError::InvalidField);
        }
        if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
            return Err(MemoryInspectError::InvalidConfidence);
        }
        if self.content.contains('\0') {
            return Err(MemoryInspectError::InvalidField);
        }
        Ok(())
    }

    fn validate_scope(&self) -> Result<(), MemoryInspectError> {
        self.validate_core()?;
        if let Some(project_id) = self.project_id.as_deref()
            && !bounded_id(project_id) {
                return Err(MemoryInspectError::InvalidField);
            }
        if let Some(session_id) = self.session_id.as_deref()
            && !bounded_id(session_id) {
                return Err(MemoryInspectError::InvalidField);
            }
        if !bounded_timestamp(&self.created_at) {
            return Err(MemoryInspectError::InvalidField);
        }
        if let Some(expires) = self.expires_at.as_deref()
            && (!bounded_timestamp(expires) || expires.is_empty()) {
                return Err(MemoryInspectError::InvalidField);
            }
        match self.scope {
            MemoryScopeKind::User => Ok(()),
            MemoryScopeKind::Project => {
                if self.project_id.as_deref().is_none_or(str::is_empty) {
                    Err(MemoryInspectError::InvalidField)
                } else {
                    Ok(())
                }
            }
            MemoryScopeKind::Session => {
                if self.project_id.as_deref().is_none_or(str::is_empty)
                    || self.session_id.as_deref().is_none_or(str::is_empty)
                {
                    Err(MemoryInspectError::InvalidField)
                } else {
                    Ok(())
                }
            }
        }
    }
}

impl MemoryRowView {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn display_id(&self) -> &str {
        &self.display_id
    }

    pub fn scope(&self) -> MemoryScopeKind {
        self.scope
    }

    pub fn source_kind(&self) -> MemorySourceKind {
        self.source_kind
    }

    pub fn display_source(&self) -> &str {
        &self.display_source
    }

    pub fn confidence(&self) -> &str {
        &self.confidence
    }

    pub fn expires_at(&self) -> Option<&str> {
        self.expires_at.as_deref()
    }

    pub fn display_expiry(&self) -> &str {
        &self.display_expiry
    }

    pub fn display_content(&self) -> &str {
        &self.display_content
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub fn redaction(&self) -> RedactionClass {
        self.redaction
    }

    pub fn is_expired(&self) -> bool {
        self.expired
    }

    pub fn is_secret(&self) -> bool {
        self.redaction == RedactionClass::Secret
    }
}

impl MemoryViewModel {
    /// Project observations plus selection. Memory is not mutated.
    pub fn new(
        observations: &[MemoryObservation],
        writes_enabled: bool,
        selection: MemorySelection,
        cancel: &CancellationToken,
    ) -> Result<Self, MemoryInspectError> {
        Self::with_clock(observations, writes_enabled, None, selection, cancel)
    }

    pub fn with_clock(
        observations: &[MemoryObservation],
        writes_enabled: bool,
        now: Option<&str>,
        selection: MemorySelection,
        cancel: &CancellationToken,
    ) -> Result<Self, MemoryInspectError> {
        check_cancel(cancel)?;
        if let Some(now) = now
            && (!bounded_timestamp(now) || now.is_empty()) {
                return Err(MemoryInspectError::InvalidField);
            }
        if observations.len() > MAX_MEMORY_ITEMS {
            return Err(MemoryInspectError::BoundExceeded);
        }
        let mut rows = Vec::with_capacity(observations.len());
        for (index, observation) in observations.iter().enumerate() {
            if index.is_multiple_of(CANCEL_STRIDE) {
                check_cancel(cancel)?;
            }
            observation.validate_scope()?;
            rows.push(project_row(observation, now));
        }
        rows.sort_by(|a, b| {
            scope_list_rank(a.scope)
                .cmp(&scope_list_rank(b.scope))
                .then(a.source_kind.as_str().cmp(b.source_kind.as_str()))
                .then(a.id.as_str().cmp(b.id.as_str()))
        });
        let row_index = if rows.is_empty() {
            0
        } else {
            selection.row_index.min(rows.len() - 1)
        };
        Ok(Self {
            rows,
            selection: MemorySelection { row_index },
            writes_enabled,
        })
    }

    pub fn from_records(
        records: &[MemoryRecord],
        writes_enabled: bool,
        cancel: &CancellationToken,
    ) -> Result<Self, MemoryInspectError> {
        check_cancel(cancel)?;
        if records.len() > MAX_MEMORY_ITEMS {
            return Err(MemoryInspectError::BoundExceeded);
        }
        let observations: Vec<MemoryObservation> = records
            .iter()
            .enumerate()
            .map(|(index, record)| {
                if index.is_multiple_of(CANCEL_STRIDE) {
                    check_cancel(cancel)?;
                }
                Ok(MemoryObservation::from_record(record))
            })
            .collect::<Result<_, _>>()?;
        Self::new(
            &observations,
            writes_enabled,
            MemorySelection::default(),
            cancel,
        )
    }

    pub fn from_observations(
        observations: &[MemoryObservation],
        writes_enabled: bool,
        cancel: &CancellationToken,
    ) -> Result<Self, MemoryInspectError> {
        Self::new(
            observations,
            writes_enabled,
            MemorySelection::default(),
            cancel,
        )
    }

    pub fn rows(&self) -> &[MemoryRowView] {
        &self.rows
    }

    pub fn selected_row(&self) -> Option<&MemoryRowView> {
        self.rows.get(self.selection.row_index)
    }

    pub fn selection(&self) -> MemorySelection {
        self.selection
    }

    pub fn writes_enabled(&self) -> bool {
        self.writes_enabled
    }

    /// This panel never persists memory or write-gate state.
    pub const fn mutates_store(&self) -> bool {
        false
    }

    pub fn select_row(&self, index: usize) -> Result<Self, MemoryInspectError> {
        if self.rows.is_empty() {
            if index == 0 {
                return Ok(self.clone());
            }
            return Err(MemoryInspectError::InvalidSelection);
        }
        if index >= self.rows.len() {
            return Err(MemoryInspectError::InvalidSelection);
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

    pub fn select_id(&self, memory_id: &str) -> Result<Self, MemoryInspectError> {
        let index = self
            .rows
            .iter()
            .position(|row| row.id == memory_id)
            .ok_or(MemoryInspectError::InvalidSelection)?;
        self.select_row(index)
    }

    /// Request a durable delete. The view is not mutated.
    pub fn delete(
        &self,
        cancel: &CancellationToken,
    ) -> Result<MemoryDeleteIntent, MemoryInspectError> {
        check_cancel(cancel)?;
        let row = self
            .selected_row()
            .ok_or(MemoryInspectError::InvalidSelection)?;
        Ok(delete_intent(row))
    }

    pub fn delete_id(
        &self,
        memory_id: &str,
        cancel: &CancellationToken,
    ) -> Result<MemoryDeleteIntent, MemoryInspectError> {
        check_cancel(cancel)?;
        let row = self
            .rows
            .iter()
            .find(|row| row.id == memory_id)
            .ok_or(MemoryInspectError::InvalidSelection)?;
        Ok(delete_intent(row))
    }

    /// Request that the context service stop accepting new writes.
    pub fn disable_writes(
        &self,
        cancel: &CancellationToken,
    ) -> Result<MemoryDisableWritesIntent, MemoryInspectError> {
        check_cancel(cancel)?;
        if !self.writes_enabled {
            return Err(MemoryInspectError::AlreadyDisabled);
        }
        Ok(MemoryDisableWritesIntent { enabled: false })
    }

    /// Scope retarget is refused. Higher scopes fail as promotion.
    pub fn change_scope(
        &self,
        target: MemoryScopeKind,
        cancel: &CancellationToken,
    ) -> Result<(), MemoryInspectError> {
        check_cancel(cancel)?;
        let row = self
            .selected_row()
            .ok_or(MemoryInspectError::InvalidSelection)?;
        if scope_width(target) > scope_width(row.scope) {
            return Err(MemoryInspectError::ScopePromotionDenied);
        }
        Err(MemoryInspectError::ScopeEditDenied)
    }

    pub fn render(&self, width: u16, height: u16) -> MemoryFrame {
        let width = width.min(MAX_MEMORY_COLS);
        let height = height.min(MAX_MEMORY_ROWS);
        if width == 0 || height == 0 {
            return MemoryFrame {
                width,
                height,
                lines: Vec::new(),
            };
        }
        let writes = if self.writes_enabled {
            "enabled"
        } else {
            "disabled"
        };
        let mut lines = vec![format!("memory writes:{writes}")];
        for scope in SCOPE_ORDER {
            lines.push(format!("{}:", scope.as_str()));
            let mut any = false;
            for (index, row) in self.rows.iter().enumerate() {
                if row.scope != *scope {
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
        MemoryFrame {
            width,
            height,
            lines,
        }
    }
}

impl MemoryFrame {
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

impl Display for MemoryInspectError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for MemoryInspectError {}

fn project_row(observation: &MemoryObservation, now: Option<&str>) -> MemoryRowView {
    let secret = observation.redaction == RedactionClass::Secret;
    let display_content = if secret {
        REDACTED.to_owned()
    } else {
        preview_content(&sanitize_untrusted(&observation.content))
    };
    let expires_at = observation.expires_at.clone();
    let expired = match (expires_at.as_deref(), now) {
        (Some(expires), Some(now)) => expires <= now,
        _ => false,
    };
    let display_expiry = match expires_at.as_deref() {
        Some(expires) => sanitize_untrusted(expires).into_owned(),
        None => "none".to_owned(),
    };
    MemoryRowView {
        display_id: sanitize_untrusted(&observation.id).into_owned(),
        id: observation.id.clone(),
        scope: observation.scope,
        project_id: observation.project_id.clone(),
        session_id: observation.session_id.clone(),
        source_kind: observation.source_kind,
        display_source: sanitize_untrusted(observation.source_kind.as_str()).into_owned(),
        confidence: format!("{:.2}", observation.confidence),
        expires_at,
        display_expiry,
        display_content,
        content_hash: observation.content_hash.clone(),
        redaction: observation.redaction,
        expired,
    }
}

fn delete_intent(row: &MemoryRowView) -> MemoryDeleteIntent {
    MemoryDeleteIntent {
        memory_id: row.id.clone(),
        scope: row.scope,
        content_hash: row.content_hash.clone(),
    }
}

fn format_row(row: &MemoryRowView) -> String {
    let mut line = format!(
        "{} source:{} confidence:{} expiry:{}",
        row.display_id, row.display_source, row.confidence, row.display_expiry
    );
    if row.expired {
        line.push(' ');
        line.push_str(EXPIRED_LABEL);
    }
    line.push(' ');
    line.push_str(&row.display_content);
    line
}

fn preview_content(text: &str) -> String {
    let mut out: String = text.chars().take(MAX_CONTENT_PREVIEW_CHARS).collect();
    out.retain(|c| c != '\n' && c != '\t');
    out
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

fn bounded_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_MEMORY_ID_BYTES && !value.contains('\0')
}

fn bounded_timestamp(value: &str) -> bool {
    value.len() <= MAX_TIMESTAMP_BYTES && !value.contains('\0')
}

fn scope_list_rank(scope: MemoryScopeKind) -> u8 {
    match scope {
        MemoryScopeKind::User => 0,
        MemoryScopeKind::Project => 1,
        MemoryScopeKind::Session => 2,
    }
}

fn scope_width(scope: MemoryScopeKind) -> u8 {
    match scope {
        MemoryScopeKind::Session => 0,
        MemoryScopeKind::Project => 1,
        MemoryScopeKind::User => 2,
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), MemoryInspectError> {
    if cancel.is_cancelled() {
        Err(MemoryInspectError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_engine::{MemoryLimits, MemoryScope, MemorySource, MemoryStore, MemoryWrite};
    use protocol::{ProjectId, SessionId};

    const GOLDEN_80: &str = "\
memory writes:enabled
user:
> pref-exact source:user confidence:0.90 expiry:none prefers exact ranges
project:
  build-cmd source:agent confidence:0.70 expiry:2026-12-31T00:00:00Z cargo test -p tui
session:
  last-error source:tool confidence:0.40 expiry:2026-08-15T00:00:00Z EXPIRED rustc failed
  api-token source:user confidence:1.00 expiry:none [REDACTED]";

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn fixture_items() -> Vec<MemoryObservation> {
        vec![
            MemoryObservation::new(
                "pref-exact",
                MemoryScopeKind::User,
                MemorySourceKind::User,
                "user-1",
                0.9,
                "prefers exact ranges",
            )
            .expect("user"),
            MemoryObservation::new(
                "build-cmd",
                MemoryScopeKind::Project,
                MemorySourceKind::Agent,
                "agent-1",
                0.7,
                "cargo test -p tui",
            )
            .expect("project")
            .with_project("proj-1")
            .with_expires_at("2026-12-31T00:00:00Z"),
            MemoryObservation::new(
                "last-error",
                MemoryScopeKind::Session,
                MemorySourceKind::Tool,
                "tool-1",
                0.4,
                "rustc failed",
            )
            .expect("session")
            .with_project("proj-1")
            .with_session("sess-1")
            .with_expires_at("2026-08-15T00:00:00Z"),
            MemoryObservation::new(
                "api-token",
                MemoryScopeKind::Session,
                MemorySourceKind::User,
                "user-1",
                1.0,
                "sk-super-secret-token",
            )
            .expect("secret")
            .with_project("proj-1")
            .with_session("sess-1")
            .with_redaction(RedactionClass::Secret),
        ]
    }

    fn fixture_model() -> MemoryViewModel {
        MemoryViewModel::with_clock(
            &fixture_items(),
            true,
            Some("2026-08-16T00:00:00Z"),
            MemorySelection::default(),
            &cancel(),
        )
        .expect("model")
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
    fn lists_by_scope_source_confidence_expiry() {
        let model = fixture_model();
        let scopes: Vec<MemoryScopeKind> = model.rows().iter().map(MemoryRowView::scope).collect();
        assert_eq!(
            scopes,
            vec![
                MemoryScopeKind::User,
                MemoryScopeKind::Project,
                MemoryScopeKind::Session,
                MemoryScopeKind::Session,
            ]
        );
        let user = &model.rows()[0];
        assert_eq!(user.source_kind(), MemorySourceKind::User);
        assert_eq!(user.confidence(), "0.90");
        assert_eq!(user.display_expiry(), "none");
        let project = &model.rows()[1];
        assert_eq!(project.source_kind(), MemorySourceKind::Agent);
        assert_eq!(project.confidence(), "0.70");
        assert_eq!(project.expires_at(), Some("2026-12-31T00:00:00Z"));
        let golden = model.render(80, 24).golden();
        assert!(golden.contains("user:"));
        assert!(golden.contains("project:"));
        assert!(golden.contains("session:"));
        assert!(golden.contains("source:user"));
        assert!(golden.contains("confidence:0.90"));
        assert!(golden.contains("expiry:2026-12-31T00:00:00Z"));
        assert!(golden.contains("EXPIRED"));
    }

    #[test]
    fn secret_classified_content_is_redacted_by_default() {
        let model = fixture_model();
        let secret = model
            .rows()
            .iter()
            .find(|row| row.id() == "api-token")
            .expect("secret");
        assert!(secret.is_secret());
        assert_eq!(secret.display_content(), REDACTED);
        let golden = model.render(80, 24).golden();
        assert!(golden.contains("[REDACTED]"));
        assert!(!golden.contains("sk-super-secret-token"));
        assert!(!model.rows().iter().any(|row| {
            row.display_content().contains("sk-super-secret-token")
                || row.display_id().contains("sk-super-secret-token")
        }));
    }

    #[test]
    fn delete_returns_intent_without_mutating() {
        let model = fixture_model();
        let intent = model.delete_id("pref-exact", &cancel()).expect("delete");
        assert_eq!(intent.memory_id(), "pref-exact");
        assert_eq!(intent.scope(), MemoryScopeKind::User);
        assert!(intent.requires_durable_audit());
        assert!(!model.mutates_store());
        assert_eq!(model.rows().len(), 4);
        assert!(model.rows().iter().any(|row| row.id() == "pref-exact"));
        assert_eq!(
            model.delete_id("missing", &cancel()),
            Err(MemoryInspectError::InvalidSelection)
        );
    }

    #[test]
    fn disable_writes_returns_intent_without_mutating() {
        let model = fixture_model();
        let intent = model.disable_writes(&cancel()).expect("disable");
        assert!(!intent.enabled());
        assert!(intent.requires_durable_audit());
        assert!(model.writes_enabled());
        assert_eq!(
            model.render(80, 8).golden().lines().next(),
            Some("memory writes:enabled")
        );
        let disabled = MemoryViewModel::from_observations(&fixture_items(), false, &cancel())
            .expect("disabled model");
        assert_eq!(
            disabled.disable_writes(&cancel()),
            Err(MemoryInspectError::AlreadyDisabled)
        );
        assert!(
            disabled
                .render(80, 8)
                .golden()
                .contains("memory writes:disabled")
        );
    }

    #[test]
    fn cannot_edit_memory_into_a_higher_scope() {
        let model = fixture_model()
            .select_id("last-error")
            .expect("select session");
        assert_eq!(
            model.change_scope(MemoryScopeKind::Project, &cancel()),
            Err(MemoryInspectError::ScopePromotionDenied)
        );
        assert_eq!(
            model.change_scope(MemoryScopeKind::User, &cancel()),
            Err(MemoryInspectError::ScopePromotionDenied)
        );
        assert_eq!(
            MemoryInspectError::ScopePromotionDenied.as_str(),
            "memory cannot be edited into a higher scope"
        );
        let user = fixture_model();
        assert_eq!(
            user.change_scope(MemoryScopeKind::Session, &cancel()),
            Err(MemoryInspectError::ScopeEditDenied)
        );
        assert_eq!(
            user.change_scope(MemoryScopeKind::User, &cancel()),
            Err(MemoryInspectError::ScopeEditDenied)
        );
    }

    #[test]
    fn navigation_moves_selection() {
        let model = fixture_model();
        assert_eq!(
            model.selected_row().map(MemoryRowView::id),
            Some("pref-exact")
        );
        let next = model.select_next();
        assert_eq!(
            next.selected_row().map(MemoryRowView::id),
            Some("build-cmd")
        );
        assert!(next.render(80, 24).golden().contains("> build-cmd "));
        let prev = next.select_prev();
        assert_eq!(
            prev.selected_row().map(MemoryRowView::id),
            Some("pref-exact")
        );
    }

    #[test]
    fn sanitizes_untrusted_source_and_content() {
        let dirty = "note\u{001B}]8;;https://evil.example\u{0007}body";
        let observation = MemoryObservation::new(
            "dirty-id",
            MemoryScopeKind::User,
            MemorySourceKind::User,
            "src\u{001B}]52;c;c2VjcmV0\u{0007}",
            0.5,
            dirty,
        )
        .expect("obs");
        let model =
            MemoryViewModel::from_observations(&[observation], true, &cancel()).expect("model");
        let row = &model.rows()[0];
        assert!(!row.display_content().contains('\u{001B}'));
        assert!(!row.display_content().contains("https://evil.example"));
        let golden = model.render(80, 12).golden();
        assert!(!golden.contains('\u{001B}'));
        assert!(!golden.contains("https://evil.example"));
    }

    #[test]
    fn construct_honors_cancellation() {
        let token = cancel();
        token.cancel();
        assert_eq!(
            MemoryViewModel::from_observations(&fixture_items(), true, &token),
            Err(MemoryInspectError::Cancelled)
        );
    }

    #[test]
    fn delete_and_disable_honor_cancellation() {
        let model = fixture_model();
        let token = cancel();
        token.cancel();
        assert_eq!(model.delete(&token), Err(MemoryInspectError::Cancelled));
        assert_eq!(
            model.disable_writes(&token),
            Err(MemoryInspectError::Cancelled)
        );
    }

    #[test]
    fn bound_exceeded_fails_closed() {
        let item = MemoryObservation::new(
            "x",
            MemoryScopeKind::User,
            MemorySourceKind::User,
            "user",
            0.1,
            "n",
        )
        .expect("item");
        let too_many = vec![item; MAX_MEMORY_ITEMS + 1];
        assert_eq!(
            MemoryViewModel::from_observations(&too_many, true, &cancel()),
            Err(MemoryInspectError::BoundExceeded)
        );
    }

    #[test]
    fn invalid_confidence_fails_closed() {
        assert_eq!(
            MemoryObservation::new(
                "x",
                MemoryScopeKind::User,
                MemorySourceKind::User,
                "user",
                1.5,
                "n",
            ),
            Err(MemoryInspectError::InvalidConfidence)
        );
    }

    #[test]
    fn from_records_projects_scope_source_confidence_expiry() {
        let mut store = MemoryStore::open_in_memory(MemoryLimits::new()).expect("store");
        let project = ProjectId::new();
        let session = SessionId::new();
        store
            .write_memory(MemoryWrite::new(
                MemoryScope::User,
                MemorySource::new(MemorySourceKind::User, "user-1"),
                0.8,
                "remember this",
            ))
            .expect("user");
        store
            .write_memory(
                MemoryWrite::new(
                    MemoryScope::Session {
                        session_id: session,
                        project_id: project,
                    },
                    MemorySource::new(MemorySourceKind::Agent, "agent-1"),
                    0.25,
                    "session note",
                )
                .expires_at("2099-01-01T00:00:00Z".parse().expect("ts")),
            )
            .expect("session");
        let records = store
            .retrieve(
                &context_engine::MemoryQuery::new()
                    .project(project)
                    .session(session),
            )
            .expect("retrieve");
        let model = MemoryViewModel::from_records(&records, true, &cancel()).expect("model");
        assert_eq!(model.rows().len(), 2);
        let user = model
            .rows()
            .iter()
            .find(|row| row.scope() == MemoryScopeKind::User)
            .expect("user row");
        assert_eq!(user.source_kind(), MemorySourceKind::User);
        assert_eq!(user.confidence(), "0.80");
        assert_eq!(user.display_expiry(), "none");
        assert_eq!(user.display_content(), "remember this");
        let session_row = model
            .rows()
            .iter()
            .find(|row| row.scope() == MemoryScopeKind::Session)
            .expect("session row");
        assert_eq!(session_row.source_kind(), MemorySourceKind::Agent);
        assert_eq!(session_row.confidence(), "0.25");
        assert_eq!(session_row.expires_at(), Some("2099-01-01T00:00:00Z"));
        let intent = model
            .delete_id(session_row.id(), &cancel())
            .expect("delete record");
        assert_eq!(intent.memory_id(), session_row.id());
        assert_eq!(intent.scope(), MemoryScopeKind::Session);
        assert!(intent.requires_durable_audit());
        assert_eq!(model.rows().len(), 2);
    }

    #[test]
    fn project_scope_without_project_id_fails_closed() {
        let observation = MemoryObservation::new(
            "orphan",
            MemoryScopeKind::Project,
            MemorySourceKind::User,
            "user-1",
            0.5,
            "note",
        )
        .expect("obs");
        assert_eq!(
            MemoryViewModel::from_observations(&[observation], true, &cancel()),
            Err(MemoryInspectError::InvalidField)
        );
    }
}
