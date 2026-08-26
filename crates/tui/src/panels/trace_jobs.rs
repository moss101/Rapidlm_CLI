//! Trace/jobs inspector over kernel projections and artifact cursors.
//!
//! [`TraceJobsViewModel`] is a frontend projection. It never inspects PIDs,
//! never tails process output, and never issues cancel itself. Cancel and
//! log-page fetches return typed intents for the kernel client. Large log
//! bodies stay behind [`ArtifactCursor`]; only a bounded page is rendered.
//! Untrusted span names and log lines are sanitized. Secret-classified
//! content is redacted before it is retained in the view.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use protocol::{ArtifactRef, JobId, RedactionClass, SpanId, TraceId};

use crate::sanitize::sanitize_untrusted;
use crate::state::{AppState, CancellationToken, JobLifecycle, JobProjection};

/// Maximum projected jobs retained in one inspector.
pub const MAX_TRACE_JOBS: usize = crate::state::MAX_PROJECTED_JOBS;

/// Maximum projected spans retained in one inspector.
pub const MAX_TRACE_SPANS: usize = 256;

/// Render width is clamped to this many columns.
pub const MAX_TRACE_JOBS_COLS: u16 = 512;

/// Render height is clamped to this many rows.
pub const MAX_TRACE_JOBS_ROWS: u16 = 256;

/// Maximum UTF-8 bytes retained for one span name.
pub const MAX_SPAN_NAME_BYTES: usize = 128;

/// Maximum UTF-8 bytes retained as an inline log excerpt.
pub const MAX_INLINE_LOG_BYTES: usize = 4 * 1024;

/// Maximum lines retained from one inline excerpt.
pub const MAX_INLINE_LOG_LINES: usize = 256;

/// Visible lines per log/artifact page.
pub const MAX_LOG_PAGE_LINES: usize = 16;

/// Maximum UTF-8 bytes retained for one rendered log line.
pub const MAX_LOG_LINE_BYTES: usize = 256;

const CANCEL_STRIDE: usize = 8;
const REDACTED: &str = "[REDACTED]";
const ARTIFACT_PREFIX: &str = "artifact:";

/// Which list the local cursor applies to. Not domain state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum TraceJobsTab {
    #[default]
    Jobs,
    Traces,
}

/// Local row/page cursor. Not domain state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct TraceJobsSelection {
    tab: TraceJobsTab,
    row_index: usize,
    log_page: u32,
}

/// Supervised (session-attached) vs background (daemon-owned) job class.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum JobClass {
    #[default]
    Supervised,
    Background,
}

/// Observable span outcome. Never stores payload text.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum SpanStatus {
    #[default]
    Unspecified,
    Ok,
    Error,
    Cancelled,
}

/// Typed inspector failure. Display never echoes IDs, logs, or span names.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TraceJobsInspectError {
    Cancelled,
    BoundExceeded,
    InvalidSelection,
    InvalidField,
    AlreadyTerminal,
    MissingArtifact,
    PageOutOfRange,
}

/// Kernel-bound cancel request. The inspector does not apply it.
///
/// Carries the stable [`JobId`] the supervisor needs. No PID is stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct JobCancelIntent {
    job_id: JobId,
}

/// Artifact page handle. Offset is a byte cursor, never a process identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactCursor {
    artifact: ArtifactRef,
    offset: u64,
}

/// Kernel-bound log/artifact page request. The inspector does not fetch bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogViewIntent {
    job_id: Option<JobId>,
    span_id: Option<SpanId>,
    cursor: ArtifactCursor,
}

/// One bounded page of already-projected log/artifact text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogPage {
    job_id: Option<JobId>,
    span_id: Option<SpanId>,
    page: u32,
    page_count: u32,
    cursor: Option<ArtifactCursor>,
    lines: Vec<String>,
    truncated: bool,
    more: bool,
}

/// Observation copied from a kernel/telemetry span event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpanObservation {
    span_id: SpanId,
    trace_id: TraceId,
    parent_span_id: Option<SpanId>,
    name: String,
    status: SpanStatus,
    duration_ms: Option<u64>,
    artifact: Option<ArtifactRef>,
    excerpt: String,
    redaction: RedactionClass,
}

/// Observation copied from a job projection plus optional log artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobObservation {
    job_id: JobId,
    class: JobClass,
    state: JobLifecycle,
    exit_status: Option<i32>,
    artifact: Option<ArtifactRef>,
    cursor: u64,
    excerpt: String,
    truncated: bool,
    redaction: RedactionClass,
}

/// One projected span row. Secret plaintext is never stored here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpanRowView {
    span_id: SpanId,
    trace_id: TraceId,
    parent_span_id: Option<SpanId>,
    display_span: String,
    display_trace: String,
    display_parent: String,
    display_name: String,
    status: SpanStatus,
    duration_ms: Option<u64>,
    artifact: Option<ArtifactRef>,
    excerpt_lines: Vec<String>,
    redaction: RedactionClass,
}

/// One projected job row. Secret plaintext is never stored here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobRowView {
    job_id: JobId,
    display_id: String,
    class: JobClass,
    state: JobLifecycle,
    exit_status: Option<i32>,
    artifact: Option<ArtifactRef>,
    cursor: u64,
    excerpt_lines: Vec<String>,
    truncated: bool,
    redaction: RedactionClass,
}

/// Frontend-only projection. Never cancels jobs or reads process tables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraceJobsViewModel {
    spans: Vec<SpanRowView>,
    jobs: Vec<JobRowView>,
    selection: TraceJobsSelection,
}

/// One painted frame. `golden` omits trailing pad.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraceJobsFrame {
    width: u16,
    height: u16,
    lines: Vec<String>,
}

impl TraceJobsTab {
    pub const ALL: &'static [Self] = &[Self::Jobs, Self::Traces];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Jobs => "jobs",
            Self::Traces => "traces",
        }
    }
}

impl TraceJobsSelection {
    pub const fn new(tab: TraceJobsTab, row_index: usize) -> Self {
        Self {
            tab,
            row_index,
            log_page: 0,
        }
    }

    pub const fn tab(self) -> TraceJobsTab {
        self.tab
    }

    pub const fn row_index(self) -> usize {
        self.row_index
    }

    pub const fn log_page(self) -> u32 {
        self.log_page
    }

    pub const fn with_log_page(mut self, log_page: u32) -> Self {
        self.log_page = log_page;
        self
    }
}

impl JobClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supervised => "supervised",
            Self::Background => "background",
        }
    }
}

impl SpanStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

impl TraceJobsInspectError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "trace/jobs inspector cancelled",
            Self::BoundExceeded => "trace/jobs inspector resource bound exceeded",
            Self::InvalidSelection => "trace/jobs selection is out of range",
            Self::InvalidField => "trace/jobs observation field is invalid",
            Self::AlreadyTerminal => "job is already terminal",
            Self::MissingArtifact => "selected row has no artifact cursor",
            Self::PageOutOfRange => "log page is out of range",
        }
    }
}

impl JobCancelIntent {
    pub const fn job_id(self) -> JobId {
        self.job_id
    }

    /// Cancel is keyed by the durable job identity, never a live PID.
    pub const fn targets_stable_job_id(self) -> bool {
        true
    }

    /// Cancel must enter the kernel interrupt path, not a local kill.
    pub const fn requires_kernel_interrupt(self) -> bool {
        true
    }
}

impl ArtifactCursor {
    pub fn new(artifact: ArtifactRef, offset: u64) -> Self {
        Self { artifact, offset }
    }

    pub fn artifact(&self) -> &ArtifactRef {
        &self.artifact
    }

    pub const fn offset(&self) -> u64 {
        self.offset
    }
}

impl LogViewIntent {
    pub const fn job_id(&self) -> Option<JobId> {
        self.job_id
    }

    pub const fn span_id(&self) -> Option<SpanId> {
        self.span_id
    }

    pub fn cursor(&self) -> &ArtifactCursor {
        &self.cursor
    }

    /// Fetch uses the artifact store, never `/proc` or a process handle.
    pub const fn uses_artifact_cursor(&self) -> bool {
        true
    }
}

impl LogPage {
    pub const fn job_id(&self) -> Option<JobId> {
        self.job_id
    }

    pub const fn span_id(&self) -> Option<SpanId> {
        self.span_id
    }

    pub const fn page(&self) -> u32 {
        self.page
    }

    pub const fn page_count(&self) -> u32 {
        self.page_count
    }

    pub fn cursor(&self) -> Option<&ArtifactCursor> {
        self.cursor.as_ref()
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    pub const fn more(&self) -> bool {
        self.more
    }
}

impl SpanObservation {
    pub fn new(
        span_id: SpanId,
        trace_id: TraceId,
        name: impl Into<String>,
        status: SpanStatus,
    ) -> Result<Self, TraceJobsInspectError> {
        let observation = Self {
            span_id,
            trace_id,
            parent_span_id: None,
            name: name.into(),
            status,
            duration_ms: None,
            artifact: None,
            excerpt: String::new(),
            redaction: RedactionClass::Project,
        };
        observation.validate()?;
        Ok(observation)
    }

    pub fn with_parent(mut self, parent: SpanId) -> Self {
        self.parent_span_id = Some(parent);
        self
    }

    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    pub fn with_artifact(mut self, artifact: ArtifactRef) -> Self {
        self.artifact = Some(artifact);
        self
    }

    pub fn with_excerpt(
        mut self,
        excerpt: impl Into<String>,
    ) -> Result<Self, TraceJobsInspectError> {
        self.excerpt = excerpt.into();
        self.validate()?;
        Ok(self)
    }

    pub fn with_redaction(mut self, redaction: RedactionClass) -> Self {
        self.redaction = redaction;
        self
    }

    pub const fn span_id(&self) -> SpanId {
        self.span_id
    }

    pub const fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn status(&self) -> SpanStatus {
        self.status
    }

    pub const fn redaction(&self) -> RedactionClass {
        self.redaction
    }

    fn validate(&self) -> Result<(), TraceJobsInspectError> {
        if self.name.len() > MAX_SPAN_NAME_BYTES || self.name.contains('\0') {
            return Err(TraceJobsInspectError::InvalidField);
        }
        validate_excerpt(&self.excerpt)
    }
}

impl JobObservation {
    pub fn new(
        job_id: JobId,
        class: JobClass,
        state: JobLifecycle,
    ) -> Result<Self, TraceJobsInspectError> {
        Ok(Self {
            job_id,
            class,
            state,
            exit_status: None,
            artifact: None,
            cursor: 0,
            excerpt: String::new(),
            truncated: false,
            redaction: RedactionClass::Project,
        })
    }

    pub fn from_projection(job: &JobProjection) -> Self {
        Self {
            job_id: job.id(),
            class: JobClass::Supervised,
            state: job.state(),
            exit_status: job.exit_status(),
            artifact: None,
            cursor: 0,
            excerpt: String::new(),
            truncated: false,
            redaction: RedactionClass::Project,
        }
    }

    pub fn with_exit_status(mut self, exit_status: i32) -> Self {
        self.exit_status = Some(exit_status);
        self
    }

    pub fn with_log(
        mut self,
        artifact: ArtifactRef,
        cursor: u64,
        excerpt: impl Into<String>,
        truncated: bool,
    ) -> Result<Self, TraceJobsInspectError> {
        self.artifact = Some(artifact);
        self.cursor = cursor;
        self.excerpt = excerpt.into();
        self.truncated = truncated;
        validate_excerpt(&self.excerpt)?;
        Ok(self)
    }

    pub fn with_redaction(mut self, redaction: RedactionClass) -> Self {
        self.redaction = redaction;
        self
    }

    pub const fn job_id(&self) -> JobId {
        self.job_id
    }

    pub const fn class(&self) -> JobClass {
        self.class
    }

    pub const fn state(&self) -> JobLifecycle {
        self.state
    }

    pub const fn redaction(&self) -> RedactionClass {
        self.redaction
    }
}

impl SpanRowView {
    pub const fn span_id(&self) -> SpanId {
        self.span_id
    }

    pub const fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub const fn status(&self) -> SpanStatus {
        self.status
    }

    pub fn artifact(&self) -> Option<&ArtifactRef> {
        self.artifact.as_ref()
    }

    pub fn is_secret(&self) -> bool {
        self.redaction == RedactionClass::Secret
    }
}

impl JobRowView {
    pub const fn job_id(&self) -> JobId {
        self.job_id
    }

    pub fn display_id(&self) -> &str {
        &self.display_id
    }

    pub const fn class(&self) -> JobClass {
        self.class
    }

    pub const fn state(&self) -> JobLifecycle {
        self.state
    }

    pub const fn exit_status(&self) -> Option<i32> {
        self.exit_status
    }

    pub fn artifact(&self) -> Option<&ArtifactRef> {
        self.artifact.as_ref()
    }

    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn is_secret(&self) -> bool {
        self.redaction == RedactionClass::Secret
    }

    pub const fn is_cancellable(&self) -> bool {
        matches!(self.state, JobLifecycle::Started | JobLifecycle::Output)
    }
}

impl TraceJobsViewModel {
    /// Project spans and jobs. Domain state is not mutated.
    pub fn new(
        spans: &[SpanObservation],
        jobs: &[JobObservation],
        selection: TraceJobsSelection,
        cancel: &CancellationToken,
    ) -> Result<Self, TraceJobsInspectError> {
        check_cancel(cancel)?;
        if spans.len() > MAX_TRACE_SPANS || jobs.len() > MAX_TRACE_JOBS {
            return Err(TraceJobsInspectError::BoundExceeded);
        }
        let mut span_rows = Vec::with_capacity(spans.len());
        for (index, observation) in spans.iter().enumerate() {
            if index.is_multiple_of(CANCEL_STRIDE) {
                check_cancel(cancel)?;
            }
            observation.validate()?;
            span_rows.push(project_span(observation));
        }
        span_rows.sort_by(|a, b| a.trace_id.cmp(&b.trace_id).then(a.span_id.cmp(&b.span_id)));
        let mut job_rows = Vec::with_capacity(jobs.len());
        for (index, observation) in jobs.iter().enumerate() {
            if index.is_multiple_of(CANCEL_STRIDE) {
                check_cancel(cancel)?;
            }
            validate_excerpt(&observation.excerpt)?;
            job_rows.push(project_job(observation));
        }
        job_rows.sort_by_key(|row| row.job_id);
        let list_len = match selection.tab {
            TraceJobsTab::Jobs => job_rows.len(),
            TraceJobsTab::Traces => span_rows.len(),
        };
        let row_index = if list_len == 0 {
            0
        } else {
            selection.row_index.min(list_len - 1)
        };
        let log_page = clamp_log_page(
            match selection.tab {
                TraceJobsTab::Jobs => job_rows
                    .get(row_index)
                    .map(|row| page_count(row.excerpt_lines.len()))
                    .unwrap_or(1),
                TraceJobsTab::Traces => span_rows
                    .get(row_index)
                    .map(|row| page_count(row.excerpt_lines.len()))
                    .unwrap_or(1),
            },
            selection.log_page,
        );
        Ok(Self {
            spans: span_rows,
            jobs: job_rows,
            selection: TraceJobsSelection {
                tab: selection.tab,
                row_index,
                log_page,
            },
        })
    }

    pub fn from_observations(
        spans: &[SpanObservation],
        jobs: &[JobObservation],
        cancel: &CancellationToken,
    ) -> Result<Self, TraceJobsInspectError> {
        Self::new(spans, jobs, TraceJobsSelection::default(), cancel)
    }

    /// Project jobs from the session reducer. Spans stay caller-supplied.
    pub fn from_app_state(
        state: &AppState,
        spans: &[SpanObservation],
        cancel: &CancellationToken,
    ) -> Result<Self, TraceJobsInspectError> {
        check_cancel(cancel)?;
        if state.jobs().len() > MAX_TRACE_JOBS {
            return Err(TraceJobsInspectError::BoundExceeded);
        }
        let jobs: Vec<JobObservation> = state
            .jobs()
            .values()
            .map(JobObservation::from_projection)
            .collect();
        let model = Self::new(spans, &jobs, TraceJobsSelection::default(), cancel)?;
        match state.selected_job() {
            Some(selected) => Ok(model.select_job(selected).unwrap_or(model)),
            None => Ok(model),
        }
    }

    pub fn spans(&self) -> &[SpanRowView] {
        &self.spans
    }

    pub fn jobs(&self) -> &[JobRowView] {
        &self.jobs
    }

    pub fn selection(&self) -> TraceJobsSelection {
        self.selection
    }

    pub fn selected_job(&self) -> Option<&JobRowView> {
        if self.selection.tab != TraceJobsTab::Jobs {
            return None;
        }
        self.jobs.get(self.selection.row_index)
    }

    pub fn selected_span(&self) -> Option<&SpanRowView> {
        if self.selection.tab != TraceJobsTab::Traces {
            return None;
        }
        self.spans.get(self.selection.row_index)
    }

    /// This panel never kills processes or reads a process table.
    pub const fn inspects_process_table(&self) -> bool {
        false
    }

    /// This panel never persists job or span state.
    pub const fn mutates_store(&self) -> bool {
        false
    }

    pub fn select_tab(&self, tab: TraceJobsTab) -> Self {
        let mut next = self.clone();
        next.selection.tab = tab;
        next.selection.row_index = 0;
        next.selection.log_page = 0;
        next
    }

    pub fn select_row(&self, index: usize) -> Result<Self, TraceJobsInspectError> {
        let list_len = self.active_len();
        if list_len == 0 {
            if index == 0 {
                return Ok(self.clone());
            }
            return Err(TraceJobsInspectError::InvalidSelection);
        }
        if index >= list_len {
            return Err(TraceJobsInspectError::InvalidSelection);
        }
        let mut next = self.clone();
        next.selection.row_index = index;
        next.selection.log_page = 0;
        Ok(next)
    }

    pub fn select_next(&self) -> Self {
        let mut next = self.clone();
        let list_len = self.active_len();
        if list_len > 0 && self.selection.row_index + 1 < list_len {
            next.selection.row_index += 1;
            next.selection.log_page = 0;
        }
        next
    }

    pub fn select_prev(&self) -> Self {
        let mut next = self.clone();
        let prev = self.selection.row_index.saturating_sub(1);
        if prev != self.selection.row_index {
            next.selection.row_index = prev;
            next.selection.log_page = 0;
        }
        next
    }

    pub fn select_job(&self, job_id: JobId) -> Result<Self, TraceJobsInspectError> {
        let index = self
            .jobs
            .iter()
            .position(|row| row.job_id == job_id)
            .ok_or(TraceJobsInspectError::InvalidSelection)?;
        let mut next = self.select_tab(TraceJobsTab::Jobs);
        next.selection.row_index = index;
        next.selection.log_page = 0;
        Ok(next)
    }

    pub fn select_span(&self, span_id: SpanId) -> Result<Self, TraceJobsInspectError> {
        let index = self
            .spans
            .iter()
            .position(|row| row.span_id == span_id)
            .ok_or(TraceJobsInspectError::InvalidSelection)?;
        let mut next = self.select_tab(TraceJobsTab::Traces);
        next.selection.row_index = index;
        next.selection.log_page = 0;
        Ok(next)
    }

    pub fn page_next(&self) -> Self {
        let mut next = self.clone();
        let count = self.active_page_count();
        if next.selection.log_page + 1 < count {
            next.selection.log_page += 1;
        }
        next
    }

    pub fn page_prev(&self) -> Self {
        let mut next = self.clone();
        next.selection.log_page = next.selection.log_page.saturating_sub(1);
        next
    }

    pub fn select_log_page(&self, page: u32) -> Result<Self, TraceJobsInspectError> {
        let count = self.active_page_count();
        if page >= count {
            return Err(TraceJobsInspectError::PageOutOfRange);
        }
        let mut next = self.clone();
        next.selection.log_page = page;
        Ok(next)
    }

    /// Request kernel cancel for the selected job. The view is not mutated.
    pub fn cancel(
        &self,
        cancel: &CancellationToken,
    ) -> Result<JobCancelIntent, TraceJobsInspectError> {
        check_cancel(cancel)?;
        let row = self
            .selected_job()
            .ok_or(TraceJobsInspectError::InvalidSelection)?;
        cancel_intent(row)
    }

    pub fn cancel_job(
        &self,
        job_id: JobId,
        cancel: &CancellationToken,
    ) -> Result<JobCancelIntent, TraceJobsInspectError> {
        check_cancel(cancel)?;
        let row = self
            .jobs
            .iter()
            .find(|row| row.job_id == job_id)
            .ok_or(TraceJobsInspectError::InvalidSelection)?;
        cancel_intent(row)
    }

    /// Bounded page of already-projected log/artifact text.
    pub fn view_logs(&self, cancel: &CancellationToken) -> Result<LogPage, TraceJobsInspectError> {
        check_cancel(cancel)?;
        match self.selection.tab {
            TraceJobsTab::Jobs => {
                let row = self
                    .selected_job()
                    .ok_or(TraceJobsInspectError::InvalidSelection)?;
                Ok(page_from_lines(
                    Some(row.job_id),
                    None,
                    &row.excerpt_lines,
                    row.artifact.as_ref(),
                    row.cursor,
                    row.truncated,
                    self.selection.log_page,
                ))
            }
            TraceJobsTab::Traces => {
                let row = self
                    .selected_span()
                    .ok_or(TraceJobsInspectError::InvalidSelection)?;
                Ok(page_from_lines(
                    None,
                    Some(row.span_id),
                    &row.excerpt_lines,
                    row.artifact.as_ref(),
                    row.artifact.as_ref().map(|a| a.bytes).unwrap_or(0),
                    false,
                    self.selection.log_page,
                ))
            }
        }
    }

    /// Cursor the kernel ArtifactReader should use for the next remote page.
    pub fn view_artifact(
        &self,
        cancel: &CancellationToken,
    ) -> Result<LogViewIntent, TraceJobsInspectError> {
        check_cancel(cancel)?;
        match self.selection.tab {
            TraceJobsTab::Jobs => {
                let row = self
                    .selected_job()
                    .ok_or(TraceJobsInspectError::InvalidSelection)?;
                let artifact = row
                    .artifact
                    .clone()
                    .ok_or(TraceJobsInspectError::MissingArtifact)?;
                Ok(LogViewIntent {
                    job_id: Some(row.job_id),
                    span_id: None,
                    cursor: ArtifactCursor {
                        artifact,
                        offset: page_offset(&row.excerpt_lines, self.selection.log_page),
                    },
                })
            }
            TraceJobsTab::Traces => {
                let row = self
                    .selected_span()
                    .ok_or(TraceJobsInspectError::InvalidSelection)?;
                let artifact = row
                    .artifact
                    .clone()
                    .ok_or(TraceJobsInspectError::MissingArtifact)?;
                Ok(LogViewIntent {
                    job_id: None,
                    span_id: Some(row.span_id),
                    cursor: ArtifactCursor {
                        artifact,
                        offset: page_offset(&row.excerpt_lines, self.selection.log_page),
                    },
                })
            }
        }
    }

    pub fn render(&self, width: u16, height: u16) -> TraceJobsFrame {
        let width = width.min(MAX_TRACE_JOBS_COLS);
        let height = height.min(MAX_TRACE_JOBS_ROWS);
        if width == 0 || height == 0 {
            return TraceJobsFrame {
                width,
                height,
                lines: Vec::new(),
            };
        }
        let mut lines = vec![format!("trace/jobs tab:{}", self.selection.tab.as_str())];
        lines.push("jobs:".to_owned());
        if self.jobs.is_empty() {
            lines.push("  (empty)".to_owned());
        } else {
            for (index, row) in self.jobs.iter().enumerate() {
                let marker = if self.selection.tab == TraceJobsTab::Jobs
                    && index == self.selection.row_index
                {
                    '>'
                } else {
                    ' '
                };
                lines.push(format!("{marker} {}", format_job_row(row)));
            }
        }
        lines.push("traces:".to_owned());
        if self.spans.is_empty() {
            lines.push("  (empty)".to_owned());
        } else {
            for (index, row) in self.spans.iter().enumerate() {
                let marker = if self.selection.tab == TraceJobsTab::Traces
                    && index == self.selection.row_index
                {
                    '>'
                } else {
                    ' '
                };
                lines.push(format!("{marker} {}", format_span_row(row)));
            }
        }
        if let Ok(page) = self.view_logs(&CancellationToken::new()) {
            let more = if page.more { "yes" } else { "no" };
            let cursor = page
                .cursor
                .as_ref()
                .map(|cursor| cursor.offset.to_string())
                .unwrap_or_else(|| "-".to_owned());
            lines.push(format!(
                "log page:{}/{} cursor:{cursor} more:{more}",
                page.page.saturating_add(1),
                page.page_count
            ));
            if page.lines.is_empty() {
                lines.push("  (empty)".to_owned());
            } else {
                for line in &page.lines {
                    lines.push(format!("  {line}"));
                }
            }
        }
        if lines.len() > usize::from(height) {
            lines.truncate(usize::from(height));
        }
        TraceJobsFrame {
            width,
            height,
            lines,
        }
    }

    fn active_len(&self) -> usize {
        match self.selection.tab {
            TraceJobsTab::Jobs => self.jobs.len(),
            TraceJobsTab::Traces => self.spans.len(),
        }
    }

    fn active_page_count(&self) -> u32 {
        match self.selection.tab {
            TraceJobsTab::Jobs => self
                .jobs
                .get(self.selection.row_index)
                .map(|row| page_count(row.excerpt_lines.len()))
                .unwrap_or(1),
            TraceJobsTab::Traces => self
                .spans
                .get(self.selection.row_index)
                .map(|row| page_count(row.excerpt_lines.len()))
                .unwrap_or(1),
        }
    }
}

impl TraceJobsFrame {
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

impl Display for TraceJobsInspectError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for TraceJobsInspectError {}

fn project_span(observation: &SpanObservation) -> SpanRowView {
    let secret = observation.redaction == RedactionClass::Secret;
    let display_name = if secret {
        REDACTED.to_owned()
    } else {
        sanitize_untrusted(&observation.name).into_owned()
    };
    let excerpt_lines = if secret {
        vec![REDACTED.to_owned()]
            .into_iter()
            .filter(|_| !observation.excerpt.is_empty())
            .collect()
    } else {
        project_excerpt(&observation.excerpt)
    };
    SpanRowView {
        display_span: observation.span_id.to_string(),
        display_trace: observation.trace_id.to_string(),
        display_parent: observation
            .parent_span_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        span_id: observation.span_id,
        trace_id: observation.trace_id,
        parent_span_id: observation.parent_span_id,
        display_name,
        status: observation.status,
        duration_ms: observation.duration_ms,
        artifact: observation.artifact.clone(),
        excerpt_lines,
        redaction: observation.redaction,
    }
}

fn project_job(observation: &JobObservation) -> JobRowView {
    let secret = observation.redaction == RedactionClass::Secret;
    let excerpt_lines = if secret {
        if observation.excerpt.is_empty() {
            Vec::new()
        } else {
            vec![REDACTED.to_owned()]
        }
    } else {
        project_excerpt(&observation.excerpt)
    };
    JobRowView {
        display_id: observation.job_id.to_string(),
        job_id: observation.job_id,
        class: observation.class,
        state: observation.state,
        exit_status: observation.exit_status,
        artifact: observation.artifact.clone(),
        cursor: observation.cursor,
        excerpt_lines,
        truncated: observation.truncated,
        redaction: observation.redaction,
    }
}

fn cancel_intent(row: &JobRowView) -> Result<JobCancelIntent, TraceJobsInspectError> {
    if !row.is_cancellable() {
        return Err(TraceJobsInspectError::AlreadyTerminal);
    }
    Ok(JobCancelIntent { job_id: row.job_id })
}

fn format_job_row(row: &JobRowView) -> String {
    let exit = row
        .exit_status
        .map(|code| code.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let mut line = format!(
        "{} class:{} state:{} exit:{exit} cursor:{}",
        row.display_id,
        row.class.as_str(),
        job_state_str(row.state),
        row.cursor
    );
    if let Some(artifact) = row.artifact.as_ref() {
        line.push(' ');
        line.push_str(ARTIFACT_PREFIX);
        line.push_str(&artifact.id.to_string());
    }
    if row.truncated {
        line.push_str(" truncated");
    }
    line
}

fn format_span_row(row: &SpanRowView) -> String {
    let duration = row
        .duration_ms
        .map(|ms| format!("{ms}ms"))
        .unwrap_or_else(|| "-".to_owned());
    let mut line = format!(
        "{} trace:{} parent:{} name:{} status:{} {duration}",
        row.display_span,
        row.display_trace,
        row.display_parent,
        row.display_name,
        row.status.as_str()
    );
    if let Some(artifact) = row.artifact.as_ref() {
        line.push(' ');
        line.push_str(ARTIFACT_PREFIX);
        line.push_str(&artifact.id.to_string());
    }
    line
}

fn job_state_str(state: JobLifecycle) -> &'static str {
    match state {
        JobLifecycle::Started => "started",
        JobLifecycle::Output => "output",
        JobLifecycle::Completed => "completed",
        JobLifecycle::OrphanReconciled => "orphan_reconciled",
    }
}

fn project_excerpt(excerpt: &str) -> Vec<String> {
    let sanitized = sanitize_untrusted(excerpt);
    let mut lines = Vec::new();
    for raw in sanitized.lines() {
        if lines.len() >= MAX_INLINE_LOG_LINES {
            break;
        }
        lines.push(fit_bytes(raw, MAX_LOG_LINE_BYTES));
    }
    lines
}

fn page_from_lines(
    job_id: Option<JobId>,
    span_id: Option<SpanId>,
    lines: &[String],
    artifact: Option<&ArtifactRef>,
    captured_cursor: u64,
    truncated: bool,
    page: u32,
) -> LogPage {
    let count = page_count(lines.len());
    let page = clamp_log_page(count, page);
    let start = usize::try_from(page)
        .unwrap_or(0)
        .saturating_mul(MAX_LOG_PAGE_LINES);
    let end = start.saturating_add(MAX_LOG_PAGE_LINES).min(lines.len());
    let window = if start >= lines.len() {
        Vec::new()
    } else {
        lines[start..end].to_vec()
    };
    let local_more = page + 1 < count;
    let remote_more = truncated
        || artifact
            .map(|artifact| {
                artifact.bytes > captured_cursor || artifact.bytes > excerpt_bytes(lines)
            })
            .unwrap_or(false);
    let cursor = artifact.map(|artifact| ArtifactCursor {
        artifact: artifact.clone(),
        offset: page_offset(lines, page),
    });
    LogPage {
        job_id,
        span_id,
        page,
        page_count: count,
        cursor,
        lines: window,
        truncated,
        more: local_more || remote_more,
    }
}

fn page_count(lines: usize) -> u32 {
    if lines == 0 {
        1
    } else {
        u32::try_from(lines.div_ceil(MAX_LOG_PAGE_LINES)).unwrap_or(1)
    }
}

fn clamp_log_page(page_count: u32, page: u32) -> u32 {
    page.min(page_count.saturating_sub(1))
}

fn page_offset(lines: &[String], page: u32) -> u64 {
    let start = usize::try_from(page)
        .unwrap_or(0)
        .saturating_mul(MAX_LOG_PAGE_LINES);
    excerpt_bytes(&lines[..start.min(lines.len())])
}

fn excerpt_bytes(lines: &[String]) -> u64 {
    lines.iter().map(|line| line.len() as u64 + 1).sum()
}

fn validate_excerpt(excerpt: &str) -> Result<(), TraceJobsInspectError> {
    if excerpt.len() > MAX_INLINE_LOG_BYTES || excerpt.contains('\0') {
        return Err(TraceJobsInspectError::InvalidField);
    }
    Ok(())
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

fn fit_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), TraceJobsInspectError> {
    if cancel.is_cancelled() {
        Err(TraceJobsInspectError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_ledger::event::{
        ActorKind, ActorRef, ErasedEventEnvelope, EventEnvelope, EventKind, RecordedAt,
    };
    use protocol::{ArtifactId, EventId, SessionId, TraceId};
    use serde_json::Value;

    use crate::state::{LocalUiEvent, UiEvent, reduce};

    const JOB_A: &str = "019c0000-0000-7000-8000-000000000019";
    const JOB_B: &str = "019c0000-0000-7000-8000-00000000001b";
    const TRACE: &str = "8f000000-0000-7000-8000-000000000017";
    const SPAN_A: &str = "018f3c8a-7e2b-7a11-8c4d-0123456789ab";
    const SPAN_B: &str = "018f3c8a-7e2b-7a13-8c4d-0123456789ab";
    const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000010";
    const ACTOR_ID: &str = "019c0000-0000-7000-8000-000000000016";
    const CREATED_AT: &str = "2026-08-14T15:20:04.123Z";
    const UPDATED_AT: &str = "2026-08-14T15:21:00.000Z";

    const GOLDEN_80: &str = "\
trace/jobs tab:jobs
jobs:
> 019c0000-0000-7000-8000-000000000019 class:supervised state:started exit:- cursor:40 artifact:sha256:2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae truncated
  019c0000-0000-7000-8000-00000000001b class:background state:completed exit:0 cursor:0
traces:
  018f3c8a-7e2b-7a11-8c4d-0123456789ab trace:8f000000-0000-7000-8000-000000000017 parent:- name:tool.exec status:ok 12ms
  018f3c8a-7e2b-7a13-8c4d-0123456789ab trace:8f000000-0000-7000-8000-000000000017 parent:018f3c8a-7e2b-7a11-8c4d-0123456789ab name:[REDACTED] status:error -
log page:1/3 cursor:0 more:yes
  job-a line 00
  job-a line 01
  job-a line 02
  job-a line 03
  job-a line 04
  job-a line 05
  job-a line 06
  job-a line 07
  job-a line 08
  job-a line 09
  job-a line 10
  job-a line 11
  job-a line 12
  job-a line 13
  job-a line 14
  job-a line 15";

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn job_id(raw: &str) -> JobId {
        raw.parse().expect("job")
    }

    fn span_id(raw: &str) -> SpanId {
        raw.parse().expect("span")
    }

    fn trace_id() -> TraceId {
        TRACE.parse().expect("trace")
    }

    fn log_artifact(bytes: u64) -> ArtifactRef {
        ArtifactRef::new(
            ArtifactId::from_bytes(b"foo"),
            "text/plain",
            bytes,
            RedactionClass::Project,
        )
    }

    fn many_lines(prefix: &str, count: usize) -> String {
        (0..count)
            .map(|i| format!("{prefix} line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fixture_jobs() -> Vec<JobObservation> {
        vec![
            JobObservation::new(job_id(JOB_A), JobClass::Supervised, JobLifecycle::Started)
                .expect("job a")
                .with_log(log_artifact(4096), 40, many_lines("job-a", 40), true)
                .expect("job a log"),
            JobObservation::new(job_id(JOB_B), JobClass::Background, JobLifecycle::Completed)
                .expect("job b")
                .with_exit_status(0),
        ]
    }

    fn fixture_spans() -> Vec<SpanObservation> {
        vec![
            SpanObservation::new(span_id(SPAN_A), trace_id(), "tool.exec", SpanStatus::Ok)
                .expect("span a")
                .with_duration_ms(12),
            SpanObservation::new(span_id(SPAN_B), trace_id(), "secret.op", SpanStatus::Error)
                .expect("span b")
                .with_parent(span_id(SPAN_A))
                .with_redaction(RedactionClass::Secret),
        ]
    }

    fn fixture_model() -> TraceJobsViewModel {
        TraceJobsViewModel::from_observations(&fixture_spans(), &fixture_jobs(), &cancel())
            .expect("model")
    }

    fn envelope(seq: u64, kind: EventKind, payload: Value) -> ErasedEventEnvelope {
        EventEnvelope::new(
            format!("019c0000-0000-7000-8000-{seq:012x}")
                .parse::<EventId>()
                .expect("event id"),
            SESSION_ID.parse::<SessionId>().expect("session"),
            seq,
            if seq == 1 { CREATED_AT } else { UPDATED_AT }
                .parse::<RecordedAt>()
                .expect("recorded_at"),
            ActorRef::new(ActorKind::System, ACTOR_ID).expect("actor"),
            TRACE.parse::<TraceId>().expect("trace"),
            kind,
            RedactionClass::Project,
            payload,
        )
    }

    #[test]
    fn golden_80_120_200() {
        let model = fixture_model();
        assert_eq!(model.render(80, 32).golden(), GOLDEN_80);
        assert_eq!(model.render(120, 32).golden(), GOLDEN_80);
        assert_eq!(model.render(200, 32).golden(), GOLDEN_80);
        assert_eq!(model.render(80, 32).text().lines().count(), 32);
        assert!(
            model
                .render(80, 32)
                .text()
                .lines()
                .all(|line| line.chars().count() == 80)
        );
    }

    #[test]
    fn lists_supervised_and_background_jobs_and_spans() {
        let model = fixture_model();
        assert_eq!(model.jobs()[0].class(), JobClass::Supervised);
        assert_eq!(model.jobs()[0].state(), JobLifecycle::Started);
        assert_eq!(model.jobs()[1].class(), JobClass::Background);
        assert_eq!(model.jobs()[1].state(), JobLifecycle::Completed);
        assert_eq!(model.spans()[0].status(), SpanStatus::Ok);
        let golden = model.render(80, 32).golden();
        assert!(golden.contains("class:supervised"));
        assert!(golden.contains("class:background"));
        assert!(golden.contains("state:started"));
        assert!(golden.contains("state:completed"));
        assert!(golden.contains("name:tool.exec"));
        assert!(golden.contains("status:ok"));
    }

    #[test]
    fn cancel_intent_targets_stable_job_id() {
        let model = fixture_model();
        let intent = model.cancel(&cancel()).expect("cancel");
        assert_eq!(intent.job_id(), job_id(JOB_A));
        assert!(intent.targets_stable_job_id());
        assert!(intent.requires_kernel_interrupt());
        assert!(!model.inspects_process_table());
        assert!(!model.mutates_store());
        assert_eq!(model.jobs().len(), 2);
        let by_id = model.cancel_job(job_id(JOB_A), &cancel()).expect("by id");
        assert_eq!(by_id.job_id(), job_id(JOB_A));
        assert_eq!(
            model.cancel_job(job_id(JOB_B), &cancel()),
            Err(TraceJobsInspectError::AlreadyTerminal)
        );
        assert_eq!(
            model
                .select_job(job_id(JOB_B))
                .expect("select terminal")
                .cancel(&cancel()),
            Err(TraceJobsInspectError::AlreadyTerminal)
        );
    }

    #[test]
    fn large_output_is_paged() {
        let model = fixture_model();
        let page0 = model.view_logs(&cancel()).expect("page0");
        assert_eq!(page0.page(), 0);
        assert_eq!(page0.page_count(), 3);
        assert_eq!(page0.lines().len(), MAX_LOG_PAGE_LINES);
        assert!(page0.more());
        assert!(page0.lines().iter().any(|line| line.contains("line 00")));
        assert!(!page0.lines().iter().any(|line| line.contains("line 16")));
        let page1 = model.page_next().view_logs(&cancel()).expect("page1");
        assert_eq!(page1.page(), 1);
        assert!(page1.lines().iter().any(|line| line.contains("line 16")));
        assert!(!page1.lines().iter().any(|line| line.contains("line 00")));
        let page2 = model
            .page_next()
            .page_next()
            .view_logs(&cancel())
            .expect("page2");
        assert_eq!(page2.page(), 2);
        assert_eq!(page2.lines().len(), 8);
        assert_eq!(
            model.select_log_page(9),
            Err(TraceJobsInspectError::PageOutOfRange)
        );
        let golden = model.render(80, 32).golden();
        assert!(golden.contains("log page:1/3"));
        assert!(!golden.contains("job-a line 16"));
        let paged = model.page_next().render(80, 32).golden();
        assert!(paged.contains("log page:2/3"));
        assert!(paged.contains("job-a line 16"));
    }

    #[test]
    fn viewing_uses_artifact_cursor_not_pid() {
        let model = fixture_model();
        let intent = model.view_artifact(&cancel()).expect("cursor");
        assert_eq!(intent.job_id(), Some(job_id(JOB_A)));
        assert!(intent.uses_artifact_cursor());
        assert_eq!(intent.cursor().offset(), 0);
        assert_eq!(intent.cursor().artifact().bytes, 4096);
        let next = model
            .page_next()
            .view_artifact(&cancel())
            .expect("page cursor");
        assert!(next.cursor().offset() > 0);
        let debug = format!("{intent:?}");
        assert!(!debug.to_ascii_lowercase().contains("pid"));
        assert!(
            !model
                .render(80, 24)
                .golden()
                .to_ascii_lowercase()
                .contains("pid")
        );
    }

    #[test]
    fn secret_classified_content_is_redacted() {
        let model = fixture_model();
        let secret = model.select_span(span_id(SPAN_B)).expect("select secret");
        assert!(secret.selected_span().expect("span").is_secret());
        assert_eq!(
            secret.selected_span().expect("span").display_name(),
            REDACTED
        );
        let golden = secret.render(80, 24).golden();
        assert!(golden.contains("[REDACTED]"));
        assert!(!golden.contains("secret.op"));
    }

    #[test]
    fn sanitizes_untrusted_names_and_logs() {
        let dirty = "line\u{001B}]8;;https://evil.example\u{0007}ok";
        let job = JobObservation::new(job_id(JOB_A), JobClass::Supervised, JobLifecycle::Output)
            .expect("job")
            .with_log(log_artifact(32), 32, dirty, false)
            .expect("log");
        let span = SpanObservation::new(
            span_id(SPAN_A),
            trace_id(),
            "name\u{001B}]52;c;c2VjcmV0\u{0007}",
            SpanStatus::Ok,
        )
        .expect("span");
        let model =
            TraceJobsViewModel::from_observations(&[span], &[job], &cancel()).expect("model");
        let page = model.view_logs(&cancel()).expect("logs");
        assert!(!page.lines().join("\n").contains('\u{001B}'));
        assert!(!page.lines().join("\n").contains("https://evil.example"));
        let golden = model.render(80, 16).golden();
        assert!(!golden.contains('\u{001B}'));
        assert!(!golden.contains("https://evil.example"));
        assert!(golden.contains("name:name"));
    }

    #[test]
    fn navigation_moves_selection_and_tab() {
        let model = fixture_model();
        assert_eq!(
            model.selected_job().map(JobRowView::job_id),
            Some(job_id(JOB_A))
        );
        let next = model.select_next();
        assert_eq!(
            next.selected_job().map(JobRowView::job_id),
            Some(job_id(JOB_B))
        );
        assert!(next.render(80, 24).golden().contains(&format!("> {JOB_B}")));
        let traces = next.select_tab(TraceJobsTab::Traces);
        assert_eq!(
            traces.selected_span().map(SpanRowView::span_id),
            Some(span_id(SPAN_A))
        );
        assert!(
            traces
                .render(80, 24)
                .golden()
                .starts_with("trace/jobs tab:traces")
        );
    }

    #[test]
    fn construct_and_actions_honor_cancellation() {
        let token = cancel();
        token.cancel();
        assert_eq!(
            TraceJobsViewModel::from_observations(&fixture_spans(), &fixture_jobs(), &token),
            Err(TraceJobsInspectError::Cancelled)
        );
        let model = fixture_model();
        assert_eq!(model.cancel(&token), Err(TraceJobsInspectError::Cancelled));
        assert_eq!(
            model.view_logs(&token),
            Err(TraceJobsInspectError::Cancelled)
        );
        assert_eq!(
            model.view_artifact(&token),
            Err(TraceJobsInspectError::Cancelled)
        );
    }

    #[test]
    fn bound_exceeded_fails_closed() {
        let job = JobObservation::new(job_id(JOB_A), JobClass::Supervised, JobLifecycle::Started)
            .expect("job");
        let too_many = vec![job; MAX_TRACE_JOBS + 1];
        assert_eq!(
            TraceJobsViewModel::from_observations(&[], &too_many, &cancel()),
            Err(TraceJobsInspectError::BoundExceeded)
        );
        let huge = "x".repeat(MAX_INLINE_LOG_BYTES + 1);
        assert_eq!(
            JobObservation::new(job_id(JOB_A), JobClass::Supervised, JobLifecycle::Started)
                .expect("job")
                .with_log(log_artifact(1), 1, huge, true),
            Err(TraceJobsInspectError::InvalidField)
        );
    }

    #[test]
    fn from_app_state_projects_jobs_without_inventing_pids() {
        let mut state = AppState::new();
        state = reduce(
            state,
            &UiEvent::Kernel(envelope(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        state = reduce(
            state,
            &UiEvent::Kernel(envelope(
                2,
                EventKind::JobStarted,
                serde_json::json!({"job_id": JOB_A}),
            )),
        );
        state = reduce(
            state,
            &UiEvent::Local(LocalUiEvent::SelectJob(Some(job_id(JOB_A)))),
        );
        let model = TraceJobsViewModel::from_app_state(&state, &[], &cancel()).expect("model");
        assert_eq!(
            model.selected_job().map(JobRowView::job_id),
            Some(job_id(JOB_A))
        );
        assert_eq!(model.jobs()[0].state(), JobLifecycle::Started);
        assert!(model.jobs()[0].artifact().is_none());
        assert!(!model.inspects_process_table());
        let intent = model.cancel(&cancel()).expect("cancel");
        assert_eq!(intent.job_id(), job_id(JOB_A));
    }

    #[test]
    fn missing_artifact_refuses_cursor_view() {
        let job = JobObservation::new(job_id(JOB_B), JobClass::Background, JobLifecycle::Completed)
            .expect("job")
            .with_exit_status(0);
        let model = TraceJobsViewModel::from_observations(&[], &[job], &cancel()).expect("model");
        assert_eq!(
            model.view_artifact(&cancel()),
            Err(TraceJobsInspectError::MissingArtifact)
        );
        let page = model.view_logs(&cancel()).expect("empty logs");
        assert!(page.lines().is_empty());
        assert!(!page.more());
    }
}
