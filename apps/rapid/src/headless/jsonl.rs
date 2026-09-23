//! Map kernel events to versioned JSONL stdout records.
//!
//! `rapid run --jsonl` writes only protocol records to stdout. Logs and other
//! diagnostics use a separate stderr writer. Lines are compact UTF-8 JSON
//! (`docs/api-contracts/headless-jsonl.md`).

use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use event_ledger::event::{ErasedEventEnvelope, EventKind};
use protocol::{ApiError, ErrorCode, RapidErrorClass, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

/// JSONL protocol schema written on every constructed record.
pub const JSONL_SCHEMA: u16 = 1;

/// Wall-clock "now" as RFC3339 (UTC, `Z` suffix) for a record's `time` field.
/// `time::error::Format` can only fail on an allocation failure formatting
/// into a `String`, never on the timestamp itself — fresh `now_utc()` is
/// always in-range for `Rfc3339`.
pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("formatting the current time as RFC3339 cannot fail")
}

/// Maximum UTF-8 bytes of one compact JSON object (excluding the trailing newline).
pub const MAX_JSONL_LINE_BYTES: usize = 1024 * 1024;

/// Maximum UTF-8 bytes accepted in one diagnostic line.
pub const MAX_DIAGNOSTIC_LINE_BYTES: usize = 8 * 1024;

/// Cooperative cancellation for encode/write. Callers must not detach writers.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// `--no-stream-events` drops `assistant.delta` only; lifecycle records stay.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct JsonlOptions {
    stream_events: bool,
}

/// Versioned stdout protocol record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JsonlRecord {
    schema: u16,
    #[serde(rename = "type")]
    record_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<SessionId>,
    seq: u64,
    time: String,
    data: Box<RawValue>,
}

/// Compact JSONL encoder. Owns the stdout-side writer exclusively.
pub struct JsonlWriter<W> {
    out: W,
    options: JsonlOptions,
    cancel: CancellationToken,
}

/// Human diagnostics. Never writes to the protocol writer.
pub struct JsonlDiagnostics<W> {
    err: W,
    cancel: CancellationToken,
}

/// Paired stdout protocol + stderr diagnostic writers for JSONL mode.
pub struct JsonlIo<O, E> {
    records: JsonlWriter<O>,
    diagnostics: JsonlDiagnostics<E>,
}

/// Process exit status for `rapid run --jsonl`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(i32)]
pub enum JsonlExitCode {
    Success = 0,
    Usage = 2,
    Policy = 3,
    Provider = 4,
    Runtime = 5,
    GoalIncomplete = 6,
    Sandbox = 7,
    ResourceExhausted = 8,
    /// The model correctly recognized it needed something only the user can
    /// supply and stopped to ask, rather than guessing — distinct from
    /// every error code above it (nothing actually went wrong) and from
    /// `Success` (the run did not produce a final answer): a script can
    /// branch on this specific code to re-run with the missing input
    /// instead of treating it as either a clean success or a failure to
    /// retry/alert on.
    NeedsContext = 9,
    /// A workflow run paused on a human decision (`rapid run`): nothing is
    /// wrong — the run is durably parked and `--resume` continues it.
    NeedsApproval = 10,
    Interrupted = 130,
}

/// Why a headless run is terminating. Business state stays in the kernel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JsonlRunOutcome {
    Success,
    Interrupted,
    GoalIncomplete { require_complete: bool },
    Failed(ApiError),
}

/// Typed failures for mapping/encoding/writing JSONL.
#[derive(Debug)]
pub enum JsonlError {
    Cancelled,
    LineTooLarge { limit: usize, observed: usize },
    DiagnosticNotSingleLine,
    EmptyDiagnostic,
    Encode(serde_json::Error),
    Io(io::Error),
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl JsonlOptions {
    pub const fn new() -> Self {
        Self {
            stream_events: true,
        }
    }

    /// `false` is `--no-stream-events`.
    pub const fn stream_events(mut self, enabled: bool) -> Self {
        self.stream_events = enabled;
        self
    }

    pub const fn stream_events_enabled(self) -> bool {
        self.stream_events
    }
}

impl Default for JsonlOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl JsonlRecord {
    /// Startup record. JSONL semantic version lives here, not in ledger seq.
    pub fn rapid_schema(time: impl Into<String>) -> Result<Self, JsonlError> {
        Ok(Self {
            schema: JSONL_SCHEMA,
            record_type: "rapid.schema".to_owned(),
            session_id: None,
            seq: 0,
            time: time.into(),
            data: raw_json(&serde_json::json!({ "version": JSONL_SCHEMA }))?,
        })
    }

    /// The model's final text (`EventKind::ModelCompleted`'s own JSONL type,
    /// constructed directly rather than through `from_event` — no committed
    /// ledger event backs a one-shot `rapid exec --jsonl` run).
    pub fn assistant_message(
        session_id: SessionId,
        seq: u64,
        time: impl Into<String>,
        text: &str,
    ) -> Result<Self, JsonlError> {
        Ok(Self {
            schema: JSONL_SCHEMA,
            record_type: "assistant.message".to_owned(),
            session_id: Some(session_id),
            seq,
            time: time.into(),
            data: raw_json(&serde_json::json!({ "text": text }))?,
        })
    }

    /// One mid-turn routing decision (Modbit `MOD-005`: "routing must be
    /// auditable"). `reason` is a short machine-stable tag
    /// (`"retry_same"`/`"fallback_to"`/`"stop"`), not free text.
    /// `spent_usd_micros` is `None` (serializes as JSON `null`, never a
    /// fabricated `0`) when no attempt on `requested_model` this turn ever
    /// reported a real cost — same discipline as `session_finished`'s own
    /// `cost_usd_micros` field, and its usual source (`RouterDecisionRecord::
    /// spent_usd_micros`, `host.rs`) is itself built the same way.
    // The arguments are the fields of one wire record, in its order; a
    // builder here would hide that the record has exactly these.
    #[allow(clippy::too_many_arguments)]
    pub fn router_decision(
        session_id: SessionId,
        seq: u64,
        time: impl Into<String>,
        requested_model: &str,
        resolved_model: &str,
        reason: &str,
        spent_usd_micros: Option<u64>,
        policy_version: Option<&str>,
    ) -> Result<Self, JsonlError> {
        Ok(Self {
            schema: JSONL_SCHEMA,
            record_type: "router.decision".to_owned(),
            session_id: Some(session_id),
            seq,
            time: time.into(),
            data: raw_json(&serde_json::json!({
                "requested_model": requested_model,
                "resolved_model": resolved_model,
                "reason": reason,
                "spent_usd_micros": spent_usd_micros,
                "policy_version": policy_version,
            }))?,
        })
    }

    /// Process-end `session.finished` with the mapped exit code.
    /// `cost_usd_micros` is `None` (serializes as JSON `null`, never a
    /// fabricated `0`) when no step in the turn ever reported real cost —
    /// same "unknown is not confirmed zero" discipline as `CostAccumulator`
    /// (`host.rs`), whose `total()` is this field's usual source.
    pub fn session_finished(
        session_id: SessionId,
        seq: u64,
        time: impl Into<String>,
        exit_code: JsonlExitCode,
        cost_usd_micros: Option<u64>,
    ) -> Result<Self, JsonlError> {
        Ok(Self {
            schema: JSONL_SCHEMA,
            record_type: "session.finished".to_owned(),
            session_id: Some(session_id),
            seq,
            time: time.into(),
            data: raw_json(&serde_json::json!({
                "exit_code": exit_code.as_i32(),
                "cost_usd_micros": cost_usd_micros,
            }))?,
        })
    }

    /// Protocol `error` record. Diagnostics still go to stderr, not here.
    pub fn error(
        err: &ApiError,
        session_id: Option<SessionId>,
        seq: u64,
        time: impl Into<String>,
    ) -> Result<Self, JsonlError> {
        Ok(Self {
            schema: JSONL_SCHEMA,
            record_type: "error".to_owned(),
            session_id,
            seq,
            time: time.into(),
            data: raw_encoded(err)?,
        })
    }

    /// Map a committed kernel/ledger event. `None` means the record is filtered.
    pub fn from_event(
        event: &ErasedEventEnvelope,
        options: JsonlOptions,
    ) -> Result<Option<Self>, JsonlError> {
        let record_type = jsonl_type(event.kind());
        if record_type == "assistant.delta" && !options.stream_events {
            return Ok(None);
        }
        Ok(Some(Self {
            schema: JSONL_SCHEMA,
            record_type: record_type.to_owned(),
            session_id: Some(event.session_id()),
            seq: event.seq(),
            time: event.recorded_at().as_str().to_owned(),
            data: raw_json(event.payload())?,
        }))
    }

    pub fn schema(&self) -> u16 {
        self.schema
    }

    pub fn record_type(&self) -> &str {
        &self.record_type
    }

    pub fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn time(&self) -> &str {
        &self.time
    }

    pub fn data(&self) -> &RawValue {
        &self.data
    }
}

impl PartialEq for JsonlRecord {
    fn eq(&self, other: &Self) -> bool {
        self.schema == other.schema
            && self.record_type == other.record_type
            && self.session_id == other.session_id
            && self.seq == other.seq
            && self.time == other.time
            && self.data.get() == other.data.get()
    }
}

impl Eq for JsonlRecord {}

impl<W: Write> JsonlWriter<W> {
    pub fn new(out: W) -> Self {
        Self::with_options(out, JsonlOptions::new(), CancellationToken::new())
    }

    pub fn with_options(out: W, options: JsonlOptions, cancel: CancellationToken) -> Self {
        Self {
            out,
            options,
            cancel,
        }
    }

    pub fn options(&self) -> JsonlOptions {
        self.options
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Write one compact JSON object and a trailing newline, then flush.
    pub fn write(&mut self, record: &JsonlRecord) -> Result<(), JsonlError> {
        if self.cancel.is_cancelled() {
            return Err(JsonlError::Cancelled);
        }
        let mut buf = serde_json::to_vec(record).map_err(JsonlError::Encode)?;
        if buf.len() > MAX_JSONL_LINE_BYTES {
            return Err(JsonlError::LineTooLarge {
                limit: MAX_JSONL_LINE_BYTES,
                observed: buf.len(),
            });
        }
        buf.push(b'\n');
        self.out.write_all(&buf).map_err(JsonlError::Io)?;
        self.out.flush().map_err(JsonlError::Io)?;
        Ok(())
    }

    /// Map and write a kernel event. `Ok(false)` means the event was filtered.
    pub fn write_event(&mut self, event: &ErasedEventEnvelope) -> Result<bool, JsonlError> {
        match JsonlRecord::from_event(event, self.options)? {
            Some(record) => {
                self.write(&record)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

impl<W: Write> JsonlDiagnostics<W> {
    pub fn new(err: W) -> Self {
        Self {
            err,
            cancel: CancellationToken::new(),
        }
    }

    pub fn with_cancel(err: W, cancel: CancellationToken) -> Self {
        Self { err, cancel }
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// One diagnostic line on stderr. Newlines are rejected, not rewritten.
    pub fn write_line(&mut self, message: &str) -> Result<(), JsonlError> {
        if self.cancel.is_cancelled() {
            return Err(JsonlError::Cancelled);
        }
        if message.is_empty() {
            return Err(JsonlError::EmptyDiagnostic);
        }
        if message.contains('\n') || message.contains('\r') {
            return Err(JsonlError::DiagnosticNotSingleLine);
        }
        if message.len() > MAX_DIAGNOSTIC_LINE_BYTES {
            return Err(JsonlError::LineTooLarge {
                limit: MAX_DIAGNOSTIC_LINE_BYTES,
                observed: message.len(),
            });
        }
        self.err
            .write_all(message.as_bytes())
            .map_err(JsonlError::Io)?;
        self.err.write_all(b"\n").map_err(JsonlError::Io)?;
        self.err.flush().map_err(JsonlError::Io)?;
        Ok(())
    }
}

impl<O: Write, E: Write> JsonlIo<O, E> {
    pub fn new(out: O, err: E) -> Self {
        let cancel = CancellationToken::new();
        Self {
            records: JsonlWriter::with_options(out, JsonlOptions::new(), cancel.clone()),
            diagnostics: JsonlDiagnostics::with_cancel(err, cancel),
        }
    }

    pub fn with_options(out: O, err: E, options: JsonlOptions, cancel: CancellationToken) -> Self {
        Self {
            records: JsonlWriter::with_options(out, options, cancel.clone()),
            diagnostics: JsonlDiagnostics::with_cancel(err, cancel),
        }
    }

    pub fn records(&mut self) -> &mut JsonlWriter<O> {
        &mut self.records
    }

    pub fn diagnostics(&mut self) -> &mut JsonlDiagnostics<E> {
        &mut self.diagnostics
    }
}

impl JsonlIo<io::Stdout, io::Stderr> {
    /// Stdout is protocol-only. Diagnostics must use [`Self::diagnostics`].
    pub fn stdio() -> Self {
        Self::new(io::stdout(), io::stderr())
    }
}

impl JsonlExitCode {
    /// Every code, with the one-line meaning `docs/getting-started.md`
    /// prints for it. The doc's table is checked against this list by
    /// `getting_started_lists_exactly_the_exit_codes`, so the numbers a
    /// script author reads cannot drift from the numbers the binary exits
    /// with. `every_exit_code_is_in_all` keeps this list exhaustive.
    pub const ALL: &[(JsonlExitCode, &str)] = &[
        (Self::Success, "the run produced its final answer"),
        (
            Self::Usage,
            "bad arguments, missing or invalid configuration, unknown session",
        ),
        (
            Self::Policy,
            "a policy or permission decision stopped the run",
        ),
        (Self::Provider, "the model provider failed or refused"),
        (
            Self::Runtime,
            "the agent turn failed for a reason other than the provider",
        ),
        (
            Self::GoalIncomplete,
            "the run ended with its goal's completion criteria unmet",
        ),
        (
            Self::Sandbox,
            "the sandbox refused or could not run a command",
        ),
        (
            Self::ResourceExhausted,
            "a budget (tokens, cost, time, or turns) ran out",
        ),
        (
            Self::NeedsContext,
            "the model stopped to ask for something only you can supply; re-run with it",
        ),
        (
            Self::NeedsApproval,
            "paused for a human decision: a workflow run (`rapid run --resume <id>` continues it) or an exec turn a hook asked about (`rapid resume <session>`, then /approvals)",
        ),
        (
            Self::Interrupted,
            "interrupted (Ctrl-C or an external cancel)",
        ),
    ];

    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Map a public API error. `--require-complete` is the only path to `6`.
    pub fn from_api_error(err: &ApiError, require_complete: bool) -> Self {
        from_error_code(err.code(), require_complete)
    }

    pub fn from_error_code(code: ErrorCode, require_complete: bool) -> Self {
        from_error_code(code, require_complete)
    }

    pub fn from_outcome(outcome: &JsonlRunOutcome) -> Self {
        match outcome {
            JsonlRunOutcome::Success => Self::Success,
            JsonlRunOutcome::Interrupted => Self::Interrupted,
            JsonlRunOutcome::GoalIncomplete { require_complete } => {
                if *require_complete {
                    Self::GoalIncomplete
                } else {
                    Self::Success
                }
            }
            JsonlRunOutcome::Failed(err) => Self::from_api_error(err, false),
        }
    }
}

impl JsonlRunOutcome {
    pub fn exit_code(&self) -> JsonlExitCode {
        JsonlExitCode::from_outcome(self)
    }
}

/// Ledger kinds use protocol names except the JSONL-required remaps below.
pub fn jsonl_type(kind: EventKind) -> &'static str {
    match kind {
        EventKind::SessionCreated => "session.started",
        EventKind::SessionClosed => "session.finished",
        EventKind::ModelStreamDelta => "assistant.delta",
        EventKind::ModelCompleted => "assistant.message",
        EventKind::ToolApprovalRequired | EventKind::ApprovalRequested => "approval.required",
        other => other.as_str(),
    }
}

fn raw_json(value: &Value) -> Result<Box<RawValue>, JsonlError> {
    raw_encoded(value)
}

fn raw_encoded<T: Serialize>(value: &T) -> Result<Box<RawValue>, JsonlError> {
    let encoded = serde_json::to_string(value).map_err(JsonlError::Encode)?;
    RawValue::from_string(encoded).map_err(JsonlError::Encode)
}

fn from_error_code(code: ErrorCode, require_complete: bool) -> JsonlExitCode {
    if require_complete
        && matches!(
            code,
            ErrorCode::GoalEvidenceMissing | ErrorCode::GoalBudgetExhausted
        )
    {
        return JsonlExitCode::GoalIncomplete;
    }
    if matches!(code, ErrorCode::AuthRequired) {
        return JsonlExitCode::Provider;
    }
    match code.class() {
        RapidErrorClass::UserInput => JsonlExitCode::Usage,
        RapidErrorClass::PolicyDenied | RapidErrorClass::ApprovalRequired => JsonlExitCode::Policy,
        RapidErrorClass::ProviderTransient | RapidErrorClass::ProviderPermanent => {
            JsonlExitCode::Provider
        }
        RapidErrorClass::Cancelled => JsonlExitCode::Interrupted,
        // Split out from the generic `Runtime` bucket: a CI script can act on
        // "the sandbox couldn't run this" or "a budget/limit was hit" without
        // parsing the JSON error body, the same way it already can for usage/
        // policy/provider failures.
        RapidErrorClass::SandboxFailed => JsonlExitCode::Sandbox,
        RapidErrorClass::ResourceExhausted => JsonlExitCode::ResourceExhausted,
        RapidErrorClass::ToolFailed
        | RapidErrorClass::Conflict
        | RapidErrorClass::Corruption
        | RapidErrorClass::Unsupported
        | RapidErrorClass::Internal => JsonlExitCode::Runtime,
    }
}

impl fmt::Display for JsonlExitCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_i32())
    }
}

impl fmt::Display for JsonlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("jsonl write cancelled"),
            Self::LineTooLarge { limit, observed } => {
                write!(f, "jsonl line exceeds {limit} bytes ({observed})")
            }
            Self::DiagnosticNotSingleLine => {
                f.write_str("diagnostic must be a single line without CR/LF")
            }
            Self::EmptyDiagnostic => f.write_str("diagnostic must be non-empty"),
            Self::Encode(err) => write!(f, "jsonl encode failed: {err}"),
            Self::Io(err) => write!(f, "jsonl write failed: {err}"),
        }
    }
}

impl Error for JsonlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::Cancelled
            | Self::LineTooLarge { .. }
            | Self::DiagnosticNotSingleLine
            | Self::EmptyDiagnostic => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_exit_code_is_in_all() {
        // A variant added to the enum must be added to `ALL` too, or the
        // doc that is checked against `ALL` silently stops describing it.
        // The match below is exhaustive over the enum, so a new variant is
        // a compile error here — and this loop then requires it in `ALL`.
        let every = [
            JsonlExitCode::Success,
            JsonlExitCode::Usage,
            JsonlExitCode::Policy,
            JsonlExitCode::Provider,
            JsonlExitCode::Runtime,
            JsonlExitCode::GoalIncomplete,
            JsonlExitCode::Sandbox,
            JsonlExitCode::ResourceExhausted,
            JsonlExitCode::NeedsContext,
            JsonlExitCode::NeedsApproval,
            JsonlExitCode::Interrupted,
        ];
        for code in every {
            match code {
                JsonlExitCode::Success
                | JsonlExitCode::Usage
                | JsonlExitCode::Policy
                | JsonlExitCode::Provider
                | JsonlExitCode::Runtime
                | JsonlExitCode::GoalIncomplete
                | JsonlExitCode::Sandbox
                | JsonlExitCode::ResourceExhausted
                | JsonlExitCode::NeedsContext
                | JsonlExitCode::NeedsApproval
                | JsonlExitCode::Interrupted => {}
            }
            assert!(
                JsonlExitCode::ALL.iter().any(|(listed, _)| *listed == code),
                "{code:?} ({}) is missing from JsonlExitCode::ALL",
                code.as_i32()
            );
        }
        assert_eq!(
            JsonlExitCode::ALL.len(),
            every.len(),
            "ALL must carry each code once"
        );
        let mut numbers: Vec<i32> = JsonlExitCode::ALL.iter().map(|(c, _)| c.as_i32()).collect();
        numbers.dedup();
        assert_eq!(
            numbers.len(),
            JsonlExitCode::ALL.len(),
            "two codes share a number"
        );
    }
    use event_ledger::event::{ActorRef, EventEnvelope, RecordedAt};
    use protocol::{RedactionClass, TraceId};
    use serde_json::json;
    use std::collections::BTreeMap;

    const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000002";
    const EVENT_ID: &str = "019c0000-0000-7000-8000-000000000001";
    const ACTOR_ID: &str = "019c0000-0000-7000-8000-000000000003";
    const TRACE_ID: &str = "8f000000-0000-7000-8000-000000000004";
    const ERROR_TRACE: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const TIME: &str = "2026-08-14T15:20:04.123Z";

    const GOLDEN_SCHEMA: &str = r#"{"schema":1,"type":"rapid.schema","seq":0,"time":"2026-08-14T15:20:04.123Z","data":{"version":1}}"#;
    const GOLDEN_SESSION_STARTED: &str = r#"{"schema":1,"type":"session.started","session_id":"019c0000-0000-7000-8000-000000000002","seq":1,"time":"2026-08-14T15:20:04.123Z","data":{}}"#;
    const GOLDEN_ASSISTANT_DELTA: &str = r#"{"schema":1,"type":"assistant.delta","session_id":"019c0000-0000-7000-8000-000000000002","seq":2,"time":"2026-08-14T15:20:04.123Z","data":{"text":"Hi"}}"#;
    const GOLDEN_ASSISTANT_MESSAGE: &str = r#"{"schema":1,"type":"assistant.message","session_id":"019c0000-0000-7000-8000-000000000002","seq":3,"time":"2026-08-14T15:20:04.123Z","data":{"text":"Hello"}}"#;
    const GOLDEN_TOOL_COMPLETED: &str = r#"{"schema":1,"type":"tool.completed","session_id":"019c0000-0000-7000-8000-000000000002","seq":42,"time":"2026-08-14T15:20:04.123Z","data":{}}"#;
    const GOLDEN_APPROVAL_REQUIRED: &str = r#"{"schema":1,"type":"approval.required","session_id":"019c0000-0000-7000-8000-000000000002","seq":5,"time":"2026-08-14T15:20:04.123Z","data":{"capability":"fs.write"}}"#;
    const GOLDEN_ERROR: &str = r#"{"schema":1,"type":"error","session_id":"019c0000-0000-7000-8000-000000000002","seq":0,"time":"2026-08-14T15:20:04.123Z","data":{"code":"policy.denied","message":"Action denied by project policy","retryable":false,"trace_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","details":{}}}"#;
    const GOLDEN_SESSION_FINISHED: &str = r#"{"schema":1,"type":"session.finished","session_id":"019c0000-0000-7000-8000-000000000002","seq":7,"time":"2026-08-14T15:20:04.123Z","data":{"cost_usd_micros":null,"exit_code":0}}"#;
    const GOLDEN_EXIT_CODES: &str = r#"{"agent.concurrency_limit":8,"auth.required":4,"browser.stale_observation":5,"config.invalid":2,"context.index_unavailable":5,"goal.budget_exhausted":8,"goal.evidence_missing":2,"goal.invalid_transition":5,"internal.unexpected":5,"mcp.server_untrusted":3,"mobile.capability_unavailable":5,"plugin.capability_denied":3,"policy.approval_required":3,"policy.denied":3,"policy.lease_invalid":3,"process.timeout":8,"provider.auth_failed":4,"provider.context_too_large":4,"provider.rate_limited":4,"sandbox.tier_unavailable":7,"session.conflict":5,"session.not_found":2,"storage.corrupt":5,"tool.invalid_arguments":2,"workspace.merge_conflict":5,"workspace.preimage_mismatch":5}"#;
    const GOLDEN_EXIT_CODES_REQUIRE_COMPLETE: &str =
        r#"{"goal.budget_exhausted":6,"goal.evidence_missing":6}"#;
    const GOLDEN_OUTCOMES: &str = r#"{"goal_incomplete_require_complete":6,"goal_incomplete_without_flag":0,"interrupted":130,"policy":3,"provider":4,"resource_exhausted":8,"runtime":5,"sandbox":7,"success":0,"usage":2}"#;

    #[test]
    fn now_rfc3339_produces_a_real_parseable_recent_utc_timestamp() {
        let stamp = now_rfc3339();
        assert!(stamp.ends_with('Z'), "{stamp}");
        let parsed: event_ledger::event::RecordedAt = stamp
            .parse()
            .expect("must round-trip through the ledger's own RFC3339 parser");
        assert_eq!(parsed.as_str(), stamp);
    }

    #[test]
    fn router_decision_carries_requested_resolved_and_reason() {
        let record = JsonlRecord::router_decision(
            session_id(),
            1,
            TIME,
            "provider-a/model-a",
            "provider-b/model-b",
            "fallback_to",
            None,
            None,
        )
        .expect("router decision");
        assert_eq!(
            encode(&record),
            format!(
                r#"{{"schema":1,"type":"router.decision","session_id":"{SESSION_ID}","seq":1,"time":"{TIME}","data":{{"policy_version":null,"reason":"fallback_to","requested_model":"provider-a/model-a","resolved_model":"provider-b/model-b","spent_usd_micros":null}}}}"#
            )
        );
    }

    #[test]
    fn router_decision_carries_a_real_spent_amount_when_reported() {
        let record = JsonlRecord::router_decision(
            session_id(),
            1,
            TIME,
            "provider-a/model-a",
            "provider-b/model-b",
            "fallback_to",
            Some(4_200),
            None,
        )
        .expect("router decision");
        assert_eq!(
            encode(&record),
            format!(
                r#"{{"schema":1,"type":"router.decision","session_id":"{SESSION_ID}","seq":1,"time":"{TIME}","data":{{"policy_version":null,"reason":"fallback_to","requested_model":"provider-a/model-a","resolved_model":"provider-b/model-b","spent_usd_micros":4200}}}}"#
            )
        );
    }

    #[test]
    fn router_decision_carries_a_real_policy_version_when_reported() {
        let record = JsonlRecord::router_decision(
            session_id(),
            1,
            TIME,
            "provider-a/model-a",
            "provider-b/model-b",
            "fallback_to",
            None,
            Some("abcd1234abcd1234"),
        )
        .expect("router decision");
        assert_eq!(
            encode(&record),
            format!(
                r#"{{"schema":1,"type":"router.decision","session_id":"{SESSION_ID}","seq":1,"time":"{TIME}","data":{{"policy_version":"abcd1234abcd1234","reason":"fallback_to","requested_model":"provider-a/model-a","resolved_model":"provider-b/model-b","spent_usd_micros":null}}}}"#
            )
        );
    }

    fn session_id() -> SessionId {
        SESSION_ID.parse().expect("session id")
    }

    fn envelope(kind: EventKind, seq: u64, payload: Value) -> ErasedEventEnvelope {
        EventEnvelope::new(
            EVENT_ID.parse().expect("event id"),
            session_id(),
            seq,
            TIME.parse::<RecordedAt>().expect("recorded_at"),
            ActorRef::agent(ACTOR_ID.parse().expect("actor id")),
            TRACE_ID.parse::<TraceId>().expect("trace id"),
            kind,
            RedactionClass::Project,
            payload,
        )
    }

    fn policy_error() -> ApiError {
        ApiError::new(
            ErrorCode::PolicyDenied,
            "Action denied by project policy",
            ERROR_TRACE.parse().expect("error trace"),
        )
        .expect("api error")
    }

    fn encode(record: &JsonlRecord) -> String {
        let mut out = Vec::new();
        JsonlWriter::new(&mut out).write(record).expect("write");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.ends_with('\n'), "missing trailing newline: {text:?}");
        assert_eq!(
            text.bytes().filter(|b| *b == b'\n').count(),
            1,
            "pretty-printed or multi-line JSON: {text:?}"
        );
        assert!(
            !text.contains(": ") && !text.contains(", "),
            "pretty/spaced JSON: {text}"
        );
        text[..text.len() - 1].to_owned()
    }

    #[test]
    fn golden_record_shapes() {
        assert_eq!(
            encode(&JsonlRecord::rapid_schema(TIME).expect("schema")),
            GOLDEN_SCHEMA
        );

        let started = JsonlRecord::from_event(
            &envelope(EventKind::SessionCreated, 1, json!({})),
            JsonlOptions::new(),
        )
        .expect("map")
        .expect("session.started");
        assert_eq!(encode(&started), GOLDEN_SESSION_STARTED);

        let delta = JsonlRecord::from_event(
            &envelope(EventKind::ModelStreamDelta, 2, json!({"text": "Hi"})),
            JsonlOptions::new(),
        )
        .expect("map")
        .expect("assistant.delta");
        assert_eq!(encode(&delta), GOLDEN_ASSISTANT_DELTA);

        let message = JsonlRecord::from_event(
            &envelope(EventKind::ModelCompleted, 3, json!({"text": "Hello"})),
            JsonlOptions::new(),
        )
        .expect("map")
        .expect("assistant.message");
        assert_eq!(encode(&message), GOLDEN_ASSISTANT_MESSAGE);

        // Same wire shape whether the record comes from a committed ledger
        // event or is constructed directly for a one-shot, non-durable run.
        let direct_message =
            JsonlRecord::assistant_message(session_id(), 3, TIME, "Hello").expect("message");
        assert_eq!(encode(&direct_message), GOLDEN_ASSISTANT_MESSAGE);

        let tool = JsonlRecord::from_event(
            &envelope(EventKind::ToolCompleted, 42, json!({})),
            JsonlOptions::new(),
        )
        .expect("map")
        .expect("tool.completed");
        assert_eq!(encode(&tool), GOLDEN_TOOL_COMPLETED);

        let approval = JsonlRecord::from_event(
            &envelope(
                EventKind::ToolApprovalRequired,
                5,
                json!({"capability": "fs.write"}),
            ),
            JsonlOptions::new(),
        )
        .expect("map")
        .expect("approval.required");
        assert_eq!(encode(&approval), GOLDEN_APPROVAL_REQUIRED);

        let approval_requested = JsonlRecord::from_event(
            &envelope(EventKind::ApprovalRequested, 6, json!({})),
            JsonlOptions::new(),
        )
        .expect("map")
        .expect("approval.requested remap");
        assert_eq!(approval_requested.record_type(), "approval.required");

        let err = JsonlRecord::error(&policy_error(), Some(session_id()), 0, TIME).expect("error");
        assert_eq!(encode(&err), GOLDEN_ERROR);

        let finished =
            JsonlRecord::session_finished(session_id(), 7, TIME, JsonlExitCode::Success, None)
                .expect("finished");
        assert_eq!(encode(&finished), GOLDEN_SESSION_FINISHED);

        let closed = JsonlRecord::from_event(
            &envelope(EventKind::SessionClosed, 8, json!({})),
            JsonlOptions::new(),
        )
        .expect("map")
        .expect("session.closed remap");
        assert_eq!(closed.record_type(), "session.finished");
    }

    #[test]
    fn golden_exit_code_mapping() {
        let mut mapped = BTreeMap::new();
        for code in ErrorCode::ALL {
            mapped.insert(
                code.as_str().to_owned(),
                JsonlExitCode::from_error_code(*code, false).as_i32(),
            );
        }
        let json = serde_json::to_string(&mapped).expect("serialize mapping");
        assert_eq!(json, GOLDEN_EXIT_CODES);

        let mut require = BTreeMap::new();
        for code in [
            ErrorCode::GoalEvidenceMissing,
            ErrorCode::GoalBudgetExhausted,
        ] {
            require.insert(
                code.as_str().to_owned(),
                JsonlExitCode::from_error_code(code, true).as_i32(),
            );
        }
        let require_json = serde_json::to_string(&require).expect("serialize require-complete");
        assert_eq!(require_json, GOLDEN_EXIT_CODES_REQUIRE_COMPLETE);

        let outcomes = BTreeMap::from([
            ("success", JsonlRunOutcome::Success.exit_code().as_i32()),
            ("usage", JsonlExitCode::Usage.as_i32()),
            ("policy", JsonlExitCode::Policy.as_i32()),
            ("provider", JsonlExitCode::Provider.as_i32()),
            ("runtime", JsonlExitCode::Runtime.as_i32()),
            ("sandbox", JsonlExitCode::Sandbox.as_i32()),
            (
                "resource_exhausted",
                JsonlExitCode::ResourceExhausted.as_i32(),
            ),
            (
                "goal_incomplete_require_complete",
                JsonlRunOutcome::GoalIncomplete {
                    require_complete: true,
                }
                .exit_code()
                .as_i32(),
            ),
            (
                "goal_incomplete_without_flag",
                JsonlRunOutcome::GoalIncomplete {
                    require_complete: false,
                }
                .exit_code()
                .as_i32(),
            ),
            (
                "interrupted",
                JsonlRunOutcome::Interrupted.exit_code().as_i32(),
            ),
        ]);
        let outcomes_json = serde_json::to_string(&outcomes).expect("serialize outcomes");
        assert_eq!(outcomes_json, GOLDEN_OUTCOMES);

        assert_eq!(
            JsonlExitCode::from_api_error(&policy_error(), false),
            JsonlExitCode::Policy
        );
        assert_eq!(JsonlExitCode::Success.as_i32(), 0);
        assert_eq!(JsonlExitCode::Interrupted.as_i32(), 130);
    }

    #[test]
    fn sandbox_and_resource_exhausted_split_from_generic_runtime() {
        assert_eq!(
            JsonlExitCode::from_error_code(ErrorCode::SandboxTierUnavailable, false),
            JsonlExitCode::Sandbox
        );
        for code in [
            ErrorCode::GoalBudgetExhausted,
            ErrorCode::AgentConcurrencyLimit,
            ErrorCode::ProcessTimeout,
        ] {
            assert_eq!(
                JsonlExitCode::from_error_code(code, false),
                JsonlExitCode::ResourceExhausted,
                "{code} should map to ResourceExhausted when completion is not required"
            );
        }
        // `--require-complete` still wins over the class-based mapping for the
        // two goal-shaped codes: a script that asked for goal completion should
        // see the goal-incomplete signal, not a generic resource-exhausted one.
        assert_eq!(
            JsonlExitCode::from_error_code(ErrorCode::GoalBudgetExhausted, true),
            JsonlExitCode::GoalIncomplete
        );
        assert_eq!(JsonlExitCode::Sandbox.as_i32(), 7);
        assert_eq!(JsonlExitCode::ResourceExhausted.as_i32(), 8);
    }

    #[test]
    fn no_stream_events_drops_only_assistant_delta() {
        let options = JsonlOptions::new().stream_events(false);
        assert!(
            JsonlRecord::from_event(
                &envelope(EventKind::ModelStreamDelta, 2, json!({"text": "Hi"})),
                options,
            )
            .expect("map")
            .is_none()
        );
        let lifecycle =
            JsonlRecord::from_event(&envelope(EventKind::TurnCompleted, 4, json!({})), options)
                .expect("map")
                .expect("turn.completed stays");
        assert_eq!(lifecycle.record_type(), "turn.completed");

        let mut out = Vec::new();
        {
            let mut writer = JsonlWriter::with_options(&mut out, options, CancellationToken::new());
            assert!(
                !writer
                    .write_event(&envelope(
                        EventKind::ModelStreamDelta,
                        2,
                        json!({"text": "Hi"})
                    ))
                    .expect("filtered write")
            );
        }
        assert!(out.is_empty(), "filtered delta leaked to stdout: {out:?}");
        {
            let mut writer = JsonlWriter::with_options(&mut out, options, CancellationToken::new());
            assert!(
                writer
                    .write_event(&envelope(EventKind::TurnStarted, 1, json!({})))
                    .expect("turn write")
            );
        }
        assert!(!out.is_empty());
    }

    #[test]
    fn diagnostics_never_write_stdout() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        {
            let mut io = JsonlIo::new(&mut stdout, &mut stderr);
            io.diagnostics()
                .write_line("index warmup failed")
                .expect("diagnostic");
        }
        assert!(
            stdout.is_empty(),
            "diagnostic logger wrote to stdout: {:?}",
            String::from_utf8_lossy(&stdout)
        );
        {
            let mut io = JsonlIo::new(&mut stdout, &mut stderr);
            io.records()
                .write(&JsonlRecord::rapid_schema(TIME).expect("schema"))
                .expect("protocol");
        }
        assert_eq!(stderr, b"index warmup failed\n");
        assert!(
            !stderr
                .windows(b"rapid.schema".len())
                .any(|w| w == b"rapid.schema"),
            "protocol record leaked to stderr"
        );
        let stdout_text = String::from_utf8(stdout).expect("utf8");
        assert!(stdout_text.starts_with(GOLDEN_SCHEMA));
        assert!(stdout_text.ends_with('\n'));
        assert!(!stdout_text.contains("index warmup failed"));
    }

    #[test]
    fn every_event_kind_maps_to_a_non_empty_type() {
        for kind in EventKind::ALL {
            let mapped = jsonl_type(*kind);
            assert!(!mapped.is_empty(), "{kind} mapped empty");
            assert!(!mapped.contains('\n'), "{kind} type is multi-line");
        }
        assert_eq!(jsonl_type(EventKind::SessionCreated), "session.started");
        assert_eq!(jsonl_type(EventKind::SessionClosed), "session.finished");
        assert_eq!(jsonl_type(EventKind::ModelStreamDelta), "assistant.delta");
        assert_eq!(jsonl_type(EventKind::ModelCompleted), "assistant.message");
        assert_eq!(
            jsonl_type(EventKind::ToolApprovalRequired),
            "approval.required"
        );
        assert_eq!(
            jsonl_type(EventKind::ApprovalRequested),
            "approval.required"
        );
        assert_eq!(jsonl_type(EventKind::ArtifactCreated), "artifact.created");
        assert_eq!(jsonl_type(EventKind::GoalCreated), "goal.created");
        assert_eq!(jsonl_type(EventKind::AgentSpawned), "agent.spawned");
        assert_eq!(jsonl_type(EventKind::EvidenceRecorded), "evidence.recorded");
        assert_eq!(jsonl_type(EventKind::ToolStarted), "tool.started");
        assert_eq!(jsonl_type(EventKind::TurnStarted), "turn.started");
    }

    #[test]
    fn cancel_stops_protocol_and_diagnostics() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut io = JsonlIo::with_options(&mut out, &mut err, JsonlOptions::new(), cancel);
            assert!(matches!(
                io.records()
                    .write(&JsonlRecord::rapid_schema(TIME).expect("schema")),
                Err(JsonlError::Cancelled)
            ));
            assert!(matches!(
                io.diagnostics().write_line("still cancelled"),
                Err(JsonlError::Cancelled)
            ));
        }
        assert!(out.is_empty());
        assert!(err.is_empty());
    }

    #[test]
    fn diagnostic_rejects_multiline_and_empty() {
        let mut err = Vec::new();
        let mut diag = JsonlDiagnostics::new(&mut err);
        assert!(matches!(
            diag.write_line(""),
            Err(JsonlError::EmptyDiagnostic)
        ));
        assert!(matches!(
            diag.write_line("a\nb"),
            Err(JsonlError::DiagnosticNotSingleLine)
        ));
        assert!(err.is_empty());
    }

    #[test]
    fn record_round_trips_golden_json() {
        let decoded: JsonlRecord =
            serde_json::from_str(GOLDEN_SESSION_STARTED).expect("deserialize");
        assert_eq!(decoded.schema(), JSONL_SCHEMA);
        assert_eq!(decoded.record_type(), "session.started");
        assert_eq!(decoded.session_id(), Some(session_id()));
        assert_eq!(decoded.seq(), 1);
        assert_eq!(decoded.time(), TIME);
        assert_eq!(decoded.data().get(), "{}");
        assert_eq!(encode(&decoded), GOLDEN_SESSION_STARTED);
    }
}
