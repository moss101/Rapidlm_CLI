//! Content-minimized telemetry event, span, and metric sink.
//!
//! Records carry correlation IDs and bounded attributes. Default fields exclude
//! prompt, code, tool output, secrets, and URL query strings. Redaction runs
//! before any exporter sees a record. No collector is a supported no-network
//! mode. Threat: `T-012`.

#![forbid(unsafe_code)]

mod export;

pub use export::{
    DiagnosticManifest, ExportCategory, ExportPolicy, ExportScope, ManifestArtifact,
    RedactionEntry, RedactionKind, RetentionLabel, TRACE_EXPORT_MANIFEST_SCHEMA,
    TraceArtifactBuilder, TraceExport, export_trace,
};

use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt::{self, Debug, Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering, compiler_fence};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    AgentId, ControlLeaseId, JobId, SessionId, SpanId, TelemetryConfig, TelemetryContent,
    TelemetryMode, TraceContext, TraceId, TurnId, WorkspaceViewId,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Local/OTLP record schema version (not a public wire contract).
pub const TELEMETRY_RECORD_SCHEMA: u16 = 1;

/// Maximum attributes on one span/log/metric point.
pub const MAX_ATTRIBUTE_FIELDS: usize = 16;

/// Maximum UTF-8 bytes accepted in an attribute or label key.
pub const MAX_ATTRIBUTE_KEY_BYTES: usize = 64;

/// Maximum UTF-8 bytes accepted in an attribute, label, or log value.
pub const MAX_ATTRIBUTE_VALUE_BYTES: usize = 256;

/// Maximum UTF-8 bytes accepted in a span, log, or metric name.
pub const MAX_NAME_BYTES: usize = 128;

/// Maximum records retained by a bounded local/OTLP queue.
pub const MAX_LOCAL_QUEUE: usize = 1024;

/// Maximum distinct metric series (name + labels) retained.
pub const MAX_METRIC_SERIES: usize = 256;

/// Maximum registered secret canaries.
pub const MAX_CANARIES: usize = 1024;

/// Maximum accepted collector endpoint bytes.
pub const MAX_COLLECTOR_ENDPOINT_BYTES: usize = 256;

const FINGERPRINT_HEX_LEN: usize = 16;
const PLACEHOLDER_PREFIX: &str = "[REDACTED:secret:";
const PLACEHOLDER_SUFFIX: &str = "]";

/// Cooperative cancellation for emit/register loops.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

/// Typed telemetry failure. Messages never include payloads or secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryError {
    Cancelled,
    BoundExceeded,
    TooManyFields,
    TooManyCanaries,
    EmptyCanary,
    EmptyName,
    InvalidName,
    InvalidEndpoint,
    DestinationLocked,
    Closed,
    Unavailable,
    ContentDisabled,
}

/// Outcome of a sink emit. Exporter loss is never a caller-visible failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SinkOutcome {
    Accepted,
    Dropped,
}

/// Outcome of a facade emit after redaction and fan-out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmitOutcome {
    pub accepted: u32,
    pub dropped: u32,
    pub omitted_attributes: u32,
}

/// Operation result class used as a bounded span/metric dimension.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultClass {
    Ok,
    Error,
    Cancelled,
    Denied,
    Timeout,
}

/// Record kind after redaction. Sinks never see a pre-redaction form.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    Span,
    Log,
    Metric,
}

/// Metric instrument kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Counter,
    Histogram,
    Gauge,
}

/// Structured log severity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// How a field is classified before export.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FieldClass {
    Safe,
    Url,
    Forbidden,
}

/// SHA-256 prefix identifying a registered canary without revealing it.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct CanaryFingerprint {
    hex: [u8; FINGERPRINT_HEX_LEN],
}

/// Validated collector destination. Credentials and query strings are rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectorEndpoint {
    url: String,
}

/// Destination lock. A locked local policy cannot be broadened to a collector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExporterPolicy {
    dest_locked: bool,
    collector: Option<CollectorEndpoint>,
}

/// Field classification plus exact canary redaction. Applied before exporters.
pub struct RedactionPipeline {
    canaries: Vec<Canary>,
}

/// Bounded, content-minimized attribute map.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct AttributeSet {
    pub(crate) fields: BTreeMap<String, String>,
    pub(crate) omitted: u32,
}

/// Correlation IDs propagated on every record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CorrelationIds {
    pub trace_id: TraceId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_id: Option<SpanId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<SpanId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<AgentId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<JobId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_view_id: Option<WorkspaceViewId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_lease_id: Option<ControlLeaseId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_lineage_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_generation: Option<u64>,
}

/// Redacted record handed to sinks. Debug omits raw attribute values.
#[derive(Clone, PartialEq, Serialize)]
pub struct TelemetryRecord {
    pub schema: u16,
    pub kind: RecordKind,
    pub name: String,
    pub ts_unix_ms: u64,
    pub correlation: CorrelationIds,
    pub attributes: AttributeSet,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_class: Option<ResultClass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_level: Option<LogLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric_kind: Option<MetricKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric_value: Option<f64>,
}

/// Pluggable sink. Implementations must not perform network I/O unless a
/// collector is configured and the transport explicitly enables it.
pub trait TelemetrySink: Send + Sync {
    fn name(&self) -> &'static str;
    fn emit(&self, record: &TelemetryRecord, cancel: &CancellationToken) -> SinkOutcome;
    fn network_enabled(&self) -> bool;
}

/// Optional OTLP transport. Default implementations never open sockets.
pub trait OtlpTransport: Send + Sync {
    fn export(&self, payload: &[u8], cancel: &CancellationToken) -> Result<(), TelemetryError>;
    fn network_enabled(&self) -> bool;
}

/// Bounded in-process sink. Never opens a network connection.
pub struct LocalSink {
    bound: usize,
    queue: Mutex<VecDeque<TelemetryRecord>>,
    dropped: AtomicU64,
}

/// OpenTelemetry-shaped exporter. No collector ⇒ no network.
pub struct OtlpSink {
    collector: Option<CollectorEndpoint>,
    transport: Arc<dyn OtlpTransport>,
    queue: Mutex<VecDeque<Vec<u8>>>,
    dropped: AtomicU64,
    network_attempts: AtomicU64,
}

impl Debug for OtlpSink {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtlpSink")
            .field(
                "collector",
                &self.collector.as_ref().map(CollectorEndpoint::as_str),
            )
            .field("network_enabled", &self.network_enabled())
            .field("dropped", &self.dropped.load(Ordering::SeqCst))
            .field(
                "network_attempts",
                &self.network_attempts.load(Ordering::SeqCst),
            )
            .finish()
    }
}

/// Transport that never connects. Used when no collector is configured.
#[derive(Debug, Default)]
pub struct NoNetworkTransport;

/// Transport that records payloads without connecting (tests / local capture).
#[derive(Debug, Default)]
pub struct RecordingTransport {
    payloads: Mutex<Vec<Vec<u8>>>,
}

/// Transport that fails after capture. Used to prove exporter outage is non-blocking.
#[derive(Debug, Default)]
pub struct FailingTransport {
    attempts: AtomicU64,
}

/// In-process metric series with a cardinality guard.
struct MetricsRegistry {
    series: Mutex<BTreeMap<String, f64>>,
    cardinality_dropped: AtomicU64,
}

/// Content-minimized telemetry facade with pluggable sinks.
pub struct Telemetry {
    content: TelemetryContent,
    policy: ExporterPolicy,
    redaction: Mutex<RedactionPipeline>,
    sinks: Vec<Arc<dyn TelemetrySink>>,
    metrics: MetricsRegistry,
    dropped: AtomicU64,
    omitted_attributes: AtomicU64,
    closed: AtomicU64,
}

impl Debug for Telemetry {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let canaries = self.redaction.lock().map(|p| p.canary_count()).unwrap_or(0);
        f.debug_struct("Telemetry")
            .field("content", &self.content)
            .field("dest_locked", &self.policy.dest_locked)
            .field("collector", &self.policy.collector.is_some())
            .field("sinks", &self.sinks.len())
            .field("canaries", &canaries)
            .field("dropped", &self.dropped.load(Ordering::SeqCst))
            .finish()
    }
}

struct Canary {
    fingerprint: CanaryFingerprint,
    needle: Vec<u8>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<(), TelemetryError> {
        if self.is_cancelled() {
            Err(TelemetryError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for TelemetryError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "telemetry operation cancelled",
            Self::BoundExceeded => "telemetry bound exceeded",
            Self::TooManyFields => "telemetry attribute field bound exceeded",
            Self::TooManyCanaries => "telemetry canary bound exceeded",
            Self::EmptyCanary => "telemetry canary must be non-empty",
            Self::EmptyName => "telemetry name must be non-empty",
            Self::InvalidName => "telemetry name is not a bounded identifier",
            Self::InvalidEndpoint => "collector endpoint rejected",
            Self::DestinationLocked => "exporter destination is locked",
            Self::Closed => "telemetry sink is closed",
            Self::Unavailable => "telemetry sink unavailable",
            Self::ContentDisabled => "telemetry content export is disabled",
        })
    }
}

impl Error for TelemetryError {}

impl ResultClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::Denied => "denied",
            Self::Timeout => "timeout",
        }
    }
}

impl CanaryFingerprint {
    fn from_secret(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut hex = [0u8; FINGERPRINT_HEX_LEN];
        write_hex_lower(&digest[..8], &mut hex);
        Self { hex }
    }

    pub fn as_hex(&self) -> &str {
        std::str::from_utf8(&self.hex).unwrap_or("????????????????")
    }
}

impl Display for CanaryFingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_hex())
    }
}

impl Debug for CanaryFingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CanaryFingerprint")
            .field(&self.as_hex())
            .finish()
    }
}

impl CollectorEndpoint {
    /// Parse an opt-in collector URL. Userinfo and query strings are rejected.
    pub fn parse(raw: &str) -> Result<Self, TelemetryError> {
        if raw.is_empty() || raw.len() > MAX_COLLECTOR_ENDPOINT_BYTES {
            return Err(TelemetryError::InvalidEndpoint);
        }
        if raw.contains('\0') || raw.contains('?') || raw.contains('#') || raw.contains('@') {
            return Err(TelemetryError::InvalidEndpoint);
        }
        let (scheme, rest) = raw
            .split_once("://")
            .ok_or(TelemetryError::InvalidEndpoint)?;
        if scheme != "https" && scheme != "http" {
            return Err(TelemetryError::InvalidEndpoint);
        }
        let host = rest.split('/').next().unwrap_or("");
        if host.is_empty() {
            return Err(TelemetryError::InvalidEndpoint);
        }
        let loopback = host == "127.0.0.1" || host == "localhost" || host.starts_with("127.0.0.1:");
        if scheme == "http" && !loopback {
            return Err(TelemetryError::InvalidEndpoint);
        }
        if host.chars().any(|c| c.is_whitespace() || c == '\\') {
            return Err(TelemetryError::InvalidEndpoint);
        }
        Ok(Self {
            url: raw.to_owned(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.url
    }
}

impl ExporterPolicy {
    /// Consumer default: local-only, destination not locked.
    pub fn local() -> Self {
        Self {
            dest_locked: false,
            collector: None,
        }
    }

    /// Enterprise lock: destination cannot be broadened past `collector`.
    pub fn locked(collector: Option<CollectorEndpoint>) -> Self {
        Self {
            dest_locked: true,
            collector,
        }
    }

    pub fn dest_locked(&self) -> bool {
        self.dest_locked
    }

    pub fn collector(&self) -> Option<&CollectorEndpoint> {
        self.collector.as_ref()
    }

    fn authorize_collector(
        &self,
        requested: Option<&CollectorEndpoint>,
    ) -> Result<(), TelemetryError> {
        if !self.dest_locked {
            return Ok(());
        }
        match (&self.collector, requested) {
            (None, None) => Ok(()),
            (None, Some(_)) => Err(TelemetryError::DestinationLocked),
            (Some(locked), Some(requested)) if locked == requested => Ok(()),
            (Some(_), _) => Err(TelemetryError::DestinationLocked),
        }
    }
}

impl Default for ExporterPolicy {
    fn default() -> Self {
        Self::local()
    }
}

impl RedactionPipeline {
    pub fn new() -> Self {
        Self {
            canaries: Vec::new(),
        }
    }

    pub fn canary_count(&self) -> usize {
        self.canaries.len()
    }

    /// Register an exact-value canary. Privileged: caller already holds plaintext.
    pub fn register_canary(
        &mut self,
        plaintext: &[u8],
        cancel: &CancellationToken,
    ) -> Result<CanaryFingerprint, TelemetryError> {
        cancel.check()?;
        if plaintext.is_empty() {
            return Err(TelemetryError::EmptyCanary);
        }
        if plaintext.len() > MAX_ATTRIBUTE_VALUE_BYTES {
            return Err(TelemetryError::BoundExceeded);
        }
        if let Some(existing) = self
            .canaries
            .iter()
            .find(|c| c.needle == plaintext)
            .map(|c| c.fingerprint)
        {
            return Ok(existing);
        }
        if self.canaries.len() >= MAX_CANARIES {
            return Err(TelemetryError::TooManyCanaries);
        }
        let fingerprint = CanaryFingerprint::from_secret(plaintext);
        self.canaries.push(Canary {
            fingerprint,
            needle: plaintext.to_vec(),
        });
        Ok(fingerprint)
    }

    pub(crate) fn redact_text(
        &self,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<String, TelemetryError> {
        cancel.check()?;
        if text.len() > MAX_ATTRIBUTE_VALUE_BYTES {
            return Err(TelemetryError::BoundExceeded);
        }
        let mut out = text.to_owned();
        for canary in &self.canaries {
            cancel.check()?;
            let Ok(needle) = std::str::from_utf8(&canary.needle) else {
                continue;
            };
            if needle.is_empty() || !out.contains(needle) {
                continue;
            }
            let placeholder = placeholder(canary.fingerprint);
            out = out.replace(needle, &placeholder);
        }
        if out.len() > MAX_ATTRIBUTE_VALUE_BYTES {
            out.truncate(MAX_ATTRIBUTE_VALUE_BYTES);
        }
        Ok(out)
    }

    pub(crate) fn contains_canary(&self, text: &str) -> bool {
        self.canaries.iter().any(|c| {
            std::str::from_utf8(&c.needle)
                .map(|needle| !needle.is_empty() && text.contains(needle))
                .unwrap_or(false)
        })
    }
}

impl Default for RedactionPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for RedactionPipeline {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedactionPipeline")
            .field("canary_count", &self.canaries.len())
            .finish()
    }
}

impl Drop for Canary {
    fn drop(&mut self) {
        wipe(&mut self.needle);
    }
}

impl AttributeSet {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn omitted(&self) -> u32 {
        self.omitted
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.fields.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    fn insert_sanitized(
        &mut self,
        key: &str,
        value: &str,
        pipeline: &RedactionPipeline,
        content: TelemetryContent,
        cancel: &CancellationToken,
    ) -> Result<(), TelemetryError> {
        cancel.check()?;
        if key.len() > MAX_ATTRIBUTE_KEY_BYTES || value.len() > MAX_ATTRIBUTE_VALUE_BYTES {
            self.omitted = self.omitted.saturating_add(1);
            return Ok(());
        }
        let normalized = normalize_key(key);
        if normalized.is_empty() {
            self.omitted = self.omitted.saturating_add(1);
            return Ok(());
        }
        match classify_field(&normalized) {
            FieldClass::Forbidden => {
                self.omitted = self.omitted.saturating_add(1);
                return Ok(());
            }
            FieldClass::Url | FieldClass::Safe => {}
        }
        if content != TelemetryContent::Off {
            // Only `off` is defined; any future mode still fails closed here.
            self.omitted = self.omitted.saturating_add(1);
            return Ok(());
        }
        if !self.fields.contains_key(&normalized) && self.fields.len() >= MAX_ATTRIBUTE_FIELDS {
            return Err(TelemetryError::TooManyFields);
        }
        let mut candidate =
            if classify_field(&normalized) == FieldClass::Url || looks_like_url(value) {
                strip_url_query(value)
            } else {
                value.to_owned()
            };
        match pipeline.redact_text(&candidate, cancel) {
            Ok(redacted) => candidate = redacted,
            Err(TelemetryError::Cancelled) => return Err(TelemetryError::Cancelled),
            Err(_) => {
                self.omitted = self.omitted.saturating_add(1);
                return Ok(());
            }
        }
        if pipeline.contains_canary(&candidate) {
            self.omitted = self.omitted.saturating_add(1);
            return Ok(());
        }
        self.fields.insert(normalized, candidate);
        Ok(())
    }
}

impl CorrelationIds {
    pub fn from_trace(ctx: &TraceContext) -> Self {
        let baggage = ctx.baggage();
        Self {
            trace_id: ctx.trace_id(),
            span_id: None,
            parent_span_id: ctx.parent_span_id(),
            session_id: parse_opt(baggage.get("session_id")),
            turn_id: parse_opt(baggage.get("turn_id")),
            agent_id: parse_opt(baggage.get("agent_id")),
            parent_agent_id: parse_opt(baggage.get("parent_agent_id")),
            job_id: parse_opt(baggage.get("job_id")),
            workspace_view_id: parse_opt(baggage.get("workspace_view_id")),
            control_lease_id: parse_opt(baggage.get("control_lease_id")),
            agent_lineage_id: bounded_opt(baggage.get("agent_lineage_id")),
            tool_call_id: bounded_opt(baggage.get("tool_call_id")),
            model_call_id: bounded_opt(baggage.get("model_call_id")),
            observation_id: bounded_opt(baggage.get("observation_id")),
            execution_generation: baggage
                .get("execution_generation")
                .and_then(|v| v.parse().ok()),
        }
    }

    pub fn with_span(mut self, span_id: SpanId) -> Self {
        self.span_id = Some(span_id);
        self
    }

    /// Redact free-form baggage strings; omit a field if a canary is still present.
    fn redact_freeform_strings(
        &mut self,
        pipeline: &RedactionPipeline,
        cancel: &CancellationToken,
    ) -> Result<u32, TelemetryError> {
        let mut omitted = 0u32;
        omitted = omitted.saturating_add(redact_or_omit_freeform(
            &mut self.agent_lineage_id,
            pipeline,
            cancel,
        )?);
        omitted = omitted.saturating_add(redact_or_omit_freeform(
            &mut self.tool_call_id,
            pipeline,
            cancel,
        )?);
        omitted = omitted.saturating_add(redact_or_omit_freeform(
            &mut self.model_call_id,
            pipeline,
            cancel,
        )?);
        omitted = omitted.saturating_add(redact_or_omit_freeform(
            &mut self.observation_id,
            pipeline,
            cancel,
        )?);
        Ok(omitted)
    }
}

impl Debug for TelemetryRecord {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelemetryRecord")
            .field("schema", &self.schema)
            .field("kind", &self.kind)
            .field("name", &self.name)
            .field("ts_unix_ms", &self.ts_unix_ms)
            .field("trace_id", &self.correlation.trace_id)
            .field("span_id", &self.correlation.span_id)
            .field("attribute_count", &self.attributes.len())
            .field("omitted_attributes", &self.attributes.omitted())
            .field("result_class", &self.result_class)
            .finish()
    }
}

impl TelemetryRecord {
    fn to_json(&self) -> Result<Vec<u8>, TelemetryError> {
        serde_json::to_vec(self).map_err(|_| TelemetryError::Unavailable)
    }
}

impl LocalSink {
    pub fn bounded(bound: usize) -> Self {
        Self {
            bound: bound.clamp(1, MAX_LOCAL_QUEUE),
            queue: Mutex::new(VecDeque::new()),
            dropped: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> Result<Vec<TelemetryRecord>, TelemetryError> {
        let guard = self.queue.lock().map_err(|_| TelemetryError::Unavailable)?;
        Ok(guard.iter().cloned().collect())
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }
}

impl TelemetrySink for LocalSink {
    fn name(&self) -> &'static str {
        "local"
    }

    fn emit(&self, record: &TelemetryRecord, cancel: &CancellationToken) -> SinkOutcome {
        if cancel.is_cancelled() {
            return SinkOutcome::Dropped;
        }
        match self.queue.lock() {
            Ok(mut queue) => {
                if queue.len() >= self.bound {
                    self.dropped.fetch_add(1, Ordering::SeqCst);
                    return SinkOutcome::Dropped;
                }
                queue.push_back(record.clone());
                SinkOutcome::Accepted
            }
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::SeqCst);
                SinkOutcome::Dropped
            }
        }
    }

    fn network_enabled(&self) -> bool {
        false
    }
}

impl OtlpSink {
    /// Supported no-network mode: serialize locally, never connect.
    pub fn without_collector() -> Self {
        Self {
            collector: None,
            transport: Arc::new(NoNetworkTransport),
            queue: Mutex::new(VecDeque::new()),
            dropped: AtomicU64::new(0),
            network_attempts: AtomicU64::new(0),
        }
    }

    pub fn with_collector(
        endpoint: CollectorEndpoint,
        transport: Arc<dyn OtlpTransport>,
        policy: &ExporterPolicy,
    ) -> Result<Self, TelemetryError> {
        policy.authorize_collector(Some(&endpoint))?;
        Ok(Self {
            collector: Some(endpoint),
            transport,
            queue: Mutex::new(VecDeque::new()),
            dropped: AtomicU64::new(0),
            network_attempts: AtomicU64::new(0),
        })
    }

    pub fn collector(&self) -> Option<&CollectorEndpoint> {
        self.collector.as_ref()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }

    pub fn network_attempts(&self) -> u64 {
        self.network_attempts.load(Ordering::SeqCst)
    }

    pub fn captured_payloads(&self) -> Result<Vec<Vec<u8>>, TelemetryError> {
        let guard = self.queue.lock().map_err(|_| TelemetryError::Unavailable)?;
        Ok(guard.iter().cloned().collect())
    }
}

impl TelemetrySink for OtlpSink {
    fn name(&self) -> &'static str {
        "otlp"
    }

    fn emit(&self, record: &TelemetryRecord, cancel: &CancellationToken) -> SinkOutcome {
        if cancel.is_cancelled() {
            return SinkOutcome::Dropped;
        }
        let Ok(payload) = record.to_json() else {
            self.dropped.fetch_add(1, Ordering::SeqCst);
            return SinkOutcome::Dropped;
        };
        match self.queue.lock() {
            Ok(mut queue) => {
                if queue.len() >= MAX_LOCAL_QUEUE {
                    self.dropped.fetch_add(1, Ordering::SeqCst);
                    return SinkOutcome::Dropped;
                }
                queue.push_back(payload.clone());
            }
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::SeqCst);
                return SinkOutcome::Dropped;
            }
        }
        if self.collector.is_none() {
            return SinkOutcome::Accepted;
        }
        self.network_attempts.fetch_add(1, Ordering::SeqCst);
        match self.transport.export(&payload, cancel) {
            Ok(()) => SinkOutcome::Accepted,
            Err(TelemetryError::Cancelled) => SinkOutcome::Dropped,
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::SeqCst);
                SinkOutcome::Dropped
            }
        }
    }

    fn network_enabled(&self) -> bool {
        self.collector.is_some() && self.transport.network_enabled()
    }
}

impl OtlpTransport for NoNetworkTransport {
    fn export(&self, _payload: &[u8], cancel: &CancellationToken) -> Result<(), TelemetryError> {
        cancel.check()
    }

    fn network_enabled(&self) -> bool {
        false
    }
}

impl RecordingTransport {
    pub fn payloads(&self) -> Result<Vec<Vec<u8>>, TelemetryError> {
        let guard = self
            .payloads
            .lock()
            .map_err(|_| TelemetryError::Unavailable)?;
        Ok(guard.iter().cloned().collect())
    }
}

impl OtlpTransport for RecordingTransport {
    fn export(&self, payload: &[u8], cancel: &CancellationToken) -> Result<(), TelemetryError> {
        cancel.check()?;
        match self.payloads.lock() {
            Ok(mut guard) => {
                if guard.len() >= MAX_LOCAL_QUEUE {
                    return Err(TelemetryError::BoundExceeded);
                }
                guard.push(payload.to_vec());
                Ok(())
            }
            Err(_) => Err(TelemetryError::Unavailable),
        }
    }

    fn network_enabled(&self) -> bool {
        false
    }
}

impl FailingTransport {
    pub fn attempts(&self) -> u64 {
        self.attempts.load(Ordering::SeqCst)
    }
}

impl OtlpTransport for FailingTransport {
    fn export(&self, _payload: &[u8], cancel: &CancellationToken) -> Result<(), TelemetryError> {
        cancel.check()?;
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(TelemetryError::Unavailable)
    }

    fn network_enabled(&self) -> bool {
        false
    }
}

impl MetricsRegistry {
    fn new() -> Self {
        Self {
            series: Mutex::new(BTreeMap::new()),
            cardinality_dropped: AtomicU64::new(0),
        }
    }

    fn record(&self, key: String, value: f64, kind: MetricKind) -> Result<bool, TelemetryError> {
        let mut series = self
            .series
            .lock()
            .map_err(|_| TelemetryError::Unavailable)?;
        if !series.contains_key(&key) && series.len() >= MAX_METRIC_SERIES {
            self.cardinality_dropped.fetch_add(1, Ordering::SeqCst);
            return Ok(false);
        }
        match kind {
            MetricKind::Counter => {
                let entry = series.entry(key).or_insert(0.0);
                *entry += value;
            }
            MetricKind::Histogram | MetricKind::Gauge => {
                series.insert(key, value);
            }
        }
        Ok(true)
    }

    fn cardinality_dropped(&self) -> u64 {
        self.cardinality_dropped.load(Ordering::SeqCst)
    }

    fn snapshot(&self) -> Result<BTreeMap<String, f64>, TelemetryError> {
        let series = self
            .series
            .lock()
            .map_err(|_| TelemetryError::Unavailable)?;
        Ok(series.clone())
    }
}

impl Telemetry {
    /// Local-first facade. Mode `local` and no collector ⇒ no network.
    pub fn new(config: TelemetryConfig) -> Self {
        // Unknown future modes stay local-only; never enable a collector here.
        let _local_only = matches!(config.mode, TelemetryMode::Local);
        Telemetry {
            content: config.content,
            policy: ExporterPolicy::local(),
            redaction: Mutex::new(RedactionPipeline::new()),
            sinks: vec![
                Arc::new(LocalSink::bounded(MAX_LOCAL_QUEUE)),
                Arc::new(OtlpSink::without_collector()),
            ],
            metrics: MetricsRegistry::new(),
            dropped: AtomicU64::new(0),
            omitted_attributes: AtomicU64::new(0),
            closed: AtomicU64::new(0),
        }
    }

    pub fn builder(config: TelemetryConfig, policy: ExporterPolicy) -> TelemetryBuilder {
        let _local_only = matches!(config.mode, TelemetryMode::Local);
        TelemetryBuilder {
            content: config.content,
            policy,
            redaction: RedactionPipeline::new(),
            sinks: Vec::new(),
        }
    }

    pub fn register_canary(
        &self,
        plaintext: &[u8],
        cancel: &CancellationToken,
    ) -> Result<CanaryFingerprint, TelemetryError> {
        cancel.check()?;
        let mut pipeline = self
            .redaction
            .lock()
            .map_err(|_| TelemetryError::Unavailable)?;
        pipeline.register_canary(plaintext, cancel)
    }

    pub fn emit_span(
        &self,
        name: &str,
        ctx: &TraceContext,
        attributes: &[(&str, &str)],
        result_class: ResultClass,
        latency_ms: u64,
        cancel: &CancellationToken,
    ) -> Result<EmitOutcome, TelemetryError> {
        let span_id = SpanId::new();
        let correlation = CorrelationIds::from_trace(ctx).with_span(span_id);
        self.emit_record(
            RecordKind::Span,
            name,
            correlation,
            attributes,
            Some(result_class),
            Some(latency_ms),
            None,
            None,
            None,
            cancel,
        )
    }

    pub fn emit_log(
        &self,
        level: LogLevel,
        name: &str,
        ctx: &TraceContext,
        message: &str,
        attributes: &[(&str, &str)],
        cancel: &CancellationToken,
    ) -> Result<EmitOutcome, TelemetryError> {
        // `message` is a forbidden content key; operational text uses `status`.
        let mut attrs = attributes.to_vec();
        attrs.push(("status", message));
        self.emit_record(
            RecordKind::Log,
            name,
            CorrelationIds::from_trace(ctx),
            &attrs,
            None,
            None,
            Some(level),
            None,
            None,
            cancel,
        )
    }

    pub fn emit_metric(
        &self,
        kind: MetricKind,
        name: &str,
        value: f64,
        ctx: &TraceContext,
        labels: &[(&str, &str)],
        cancel: &CancellationToken,
    ) -> Result<EmitOutcome, TelemetryError> {
        cancel.check()?;
        validate_metric_name(name)?;
        let pipeline = self
            .redaction
            .lock()
            .map_err(|_| TelemetryError::Unavailable)?;
        if pipeline.contains_canary(name) {
            return Err(TelemetryError::InvalidName);
        }
        let mut attrs = AttributeSet::empty();
        for (key, value) in labels {
            if classify_field(&normalize_key(key)) != FieldClass::Safe {
                attrs.omitted = attrs.omitted.saturating_add(1);
                continue;
            }
            attrs.insert_sanitized(key, value, &pipeline, self.content, cancel)?;
        }
        let mut correlation = CorrelationIds::from_trace(ctx);
        let omitted_ids = correlation.redact_freeform_strings(&pipeline, cancel)?;
        attrs.omitted = attrs.omitted.saturating_add(omitted_ids);
        drop(pipeline);
        let series_key = metric_series_key(name, &attrs);
        let accepted_series = self.metrics.record(series_key, value, kind)?;
        if !accepted_series {
            self.dropped.fetch_add(1, Ordering::SeqCst);
            return Ok(EmitOutcome {
                accepted: 0,
                dropped: 1,
                omitted_attributes: attrs.omitted,
            });
        }
        self.emit_prepared(
            TelemetryRecord {
                schema: TELEMETRY_RECORD_SCHEMA,
                kind: RecordKind::Metric,
                name: name.to_owned(),
                ts_unix_ms: unix_ms(),
                correlation,
                attributes: attrs,
                result_class: None,
                latency_ms: None,
                log_level: None,
                metric_kind: Some(kind),
                metric_value: Some(value),
            },
            cancel,
        )
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }

    pub fn omitted_attributes(&self) -> u64 {
        self.omitted_attributes.load(Ordering::SeqCst)
    }

    pub fn metric_cardinality_dropped(&self) -> u64 {
        self.metrics.cardinality_dropped()
    }

    pub fn metric_snapshot(&self) -> Result<BTreeMap<String, f64>, TelemetryError> {
        self.metrics.snapshot()
    }

    pub fn policy(&self) -> &ExporterPolicy {
        &self.policy
    }

    pub fn sinks(&self) -> &[Arc<dyn TelemetrySink>] {
        &self.sinks
    }

    pub fn close(&self) {
        self.closed.store(1, Ordering::SeqCst);
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_record(
        &self,
        kind: RecordKind,
        name: &str,
        correlation: CorrelationIds,
        attributes: &[(&str, &str)],
        result_class: Option<ResultClass>,
        latency_ms: Option<u64>,
        log_level: Option<LogLevel>,
        metric_kind: Option<MetricKind>,
        metric_value: Option<f64>,
        cancel: &CancellationToken,
    ) -> Result<EmitOutcome, TelemetryError> {
        cancel.check()?;
        if self.closed.load(Ordering::SeqCst) != 0 {
            self.dropped.fetch_add(1, Ordering::SeqCst);
            return Err(TelemetryError::Closed);
        }
        let pipeline = self
            .redaction
            .lock()
            .map_err(|_| TelemetryError::Unavailable)?;
        let name = sanitize_name(name, &pipeline, cancel)?;
        let mut attrs = AttributeSet::empty();
        for (key, value) in attributes {
            attrs.insert_sanitized(key, value, &pipeline, self.content, cancel)?;
        }
        if let Some(class) = result_class {
            let _ = attrs.insert_sanitized(
                "result_class",
                class.as_str(),
                &pipeline,
                self.content,
                cancel,
            );
        }
        let mut correlation = correlation;
        let omitted_ids = correlation.redact_freeform_strings(&pipeline, cancel)?;
        attrs.omitted = attrs.omitted.saturating_add(omitted_ids);
        drop(pipeline);
        self.emit_prepared(
            TelemetryRecord {
                schema: TELEMETRY_RECORD_SCHEMA,
                kind,
                name,
                ts_unix_ms: unix_ms(),
                correlation,
                attributes: attrs,
                result_class,
                latency_ms,
                log_level,
                metric_kind,
                metric_value,
            },
            cancel,
        )
    }

    fn emit_prepared(
        &self,
        record: TelemetryRecord,
        cancel: &CancellationToken,
    ) -> Result<EmitOutcome, TelemetryError> {
        cancel.check()?;
        self.omitted_attributes
            .fetch_add(u64::from(record.attributes.omitted()), Ordering::SeqCst);
        let mut accepted = 0u32;
        let mut dropped = 0u32;
        for sink in &self.sinks {
            match sink.emit(&record, cancel) {
                SinkOutcome::Accepted => accepted = accepted.saturating_add(1),
                SinkOutcome::Dropped => {
                    dropped = dropped.saturating_add(1);
                    self.dropped.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        Ok(EmitOutcome {
            accepted,
            dropped,
            omitted_attributes: record.attributes.omitted(),
        })
    }
}

/// Builder that refuses to broaden a locked exporter policy.
pub struct TelemetryBuilder {
    content: TelemetryContent,
    policy: ExporterPolicy,
    redaction: RedactionPipeline,
    sinks: Vec<Arc<dyn TelemetrySink>>,
}

impl TelemetryBuilder {
    pub fn local_sink(mut self, sink: LocalSink) -> Self {
        self.sinks.push(Arc::new(sink));
        self
    }

    pub fn otlp_sink(mut self, sink: OtlpSink) -> Result<Self, TelemetryError> {
        self.policy.authorize_collector(sink.collector())?;
        // Mode is local-only in v1 config; a collector still requires opt-in
        // construction, never implicit network from `Telemetry::new`.
        self.sinks.push(Arc::new(sink));
        Ok(self)
    }

    pub fn register_canary(
        mut self,
        plaintext: &[u8],
        cancel: &CancellationToken,
    ) -> Result<Self, TelemetryError> {
        self.redaction.register_canary(plaintext, cancel)?;
        Ok(self)
    }

    pub fn build(self) -> Telemetry {
        Telemetry {
            content: self.content,
            policy: self.policy,
            redaction: Mutex::new(self.redaction),
            sinks: self.sinks,
            metrics: MetricsRegistry::new(),
            dropped: AtomicU64::new(0),
            omitted_attributes: AtomicU64::new(0),
            closed: AtomicU64::new(0),
        }
    }
}

fn sanitize_name(
    name: &str,
    pipeline: &RedactionPipeline,
    cancel: &CancellationToken,
) -> Result<String, TelemetryError> {
    cancel.check()?;
    if name.is_empty() {
        return Err(TelemetryError::EmptyName);
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(TelemetryError::BoundExceeded);
    }
    if !is_safe_name(name) || pipeline.contains_canary(name) {
        return Err(TelemetryError::InvalidName);
    }
    Ok(name.to_owned())
}

fn validate_metric_name(name: &str) -> Result<(), TelemetryError> {
    if name.is_empty() {
        return Err(TelemetryError::EmptyName);
    }
    if name.len() > MAX_NAME_BYTES || !is_safe_name(name) {
        return Err(TelemetryError::InvalidName);
    }
    Ok(())
}

pub(crate) fn is_safe_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_')
}

pub(crate) fn classify_field(normalized_key: &str) -> FieldClass {
    if is_forbidden_key(normalized_key) {
        return FieldClass::Forbidden;
    }
    if matches!(
        normalized_key,
        "url" | "uri" | "href" | "endpoint" | "target_url" | "request_url"
    ) {
        return FieldClass::Url;
    }
    FieldClass::Safe
}

pub(crate) fn is_forbidden_key(normalized_key: &str) -> bool {
    matches!(
        normalized_key,
        "prompt"
            | "prompts"
            | "raw_prompt"
            | "prompt_text"
            | "prompt_body"
            | "code"
            | "source_code"
            | "secret"
            | "secrets"
            | "password"
            | "passwd"
            | "token"
            | "api_key"
            | "apikey"
            | "credential"
            | "credentials"
            | "authorization"
            | "chain_of_thought"
            | "hidden_cot"
            | "cot"
            | "file_contents"
            | "file_content"
            | "shell_output"
            | "tool_output"
            | "tool_result"
            | "raw_output"
            | "stdout"
            | "stderr"
            | "content"
            | "body"
            | "payload"
            | "message"
            | "url_query"
            | "query"
            | "query_string"
            | "querystring"
            | "screenshot"
            | "video"
            | "recording"
    ) || normalized_key.ends_with("_prompt")
        || normalized_key.ends_with("_secret")
        || normalized_key.ends_with("_token")
        || normalized_key.ends_with("_code")
        || normalized_key.contains("chain_of_thought")
}

pub(crate) fn normalize_key(key: &str) -> String {
    key.trim().to_ascii_lowercase().replace(['-', '.'], "_")
}

fn looks_like_url(value: &str) -> bool {
    let lower = value.trim().as_bytes();
    lower.starts_with(b"http://") || lower.starts_with(b"https://")
}

fn strip_url_query(value: &str) -> String {
    match value.find('?') {
        Some(idx) => value[..idx].to_owned(),
        None => value.to_owned(),
    }
}

fn placeholder(fingerprint: CanaryFingerprint) -> String {
    let mut out = String::with_capacity(
        PLACEHOLDER_PREFIX.len() + FINGERPRINT_HEX_LEN + PLACEHOLDER_SUFFIX.len(),
    );
    out.push_str(PLACEHOLDER_PREFIX);
    out.push_str(fingerprint.as_hex());
    out.push_str(PLACEHOLDER_SUFFIX);
    out
}

fn parse_opt<T: std::str::FromStr>(value: Option<&str>) -> Option<T> {
    value.and_then(|v| v.parse().ok())
}

fn bounded_opt(value: Option<&str>) -> Option<String> {
    value.and_then(|v| {
        if v.is_empty() || v.len() > MAX_ATTRIBUTE_VALUE_BYTES {
            None
        } else {
            Some(v.to_owned())
        }
    })
}

/// Replace exact canaries; omit the field if a canary is still present after redaction.
fn redact_or_omit_freeform(
    field: &mut Option<String>,
    pipeline: &RedactionPipeline,
    cancel: &CancellationToken,
) -> Result<u32, TelemetryError> {
    let Some(value) = field.as_deref() else {
        return Ok(0);
    };
    let redacted = match pipeline.redact_text(value, cancel) {
        Ok(text) => text,
        Err(TelemetryError::Cancelled) => return Err(TelemetryError::Cancelled),
        Err(_) => {
            *field = None;
            return Ok(1);
        }
    };
    if pipeline.contains_canary(&redacted) {
        *field = None;
        return Ok(1);
    }
    *field = Some(redacted);
    Ok(0)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn metric_series_key(name: &str, attrs: &AttributeSet) -> String {
    let mut key = name.to_owned();
    for (k, v) in attrs.iter() {
        key.push('\u{1f}');
        key.push_str(k);
        key.push('=');
        key.push_str(v);
    }
    key
}

fn write_hex_lower(bytes: &[u8], out: &mut [u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, byte) in bytes.iter().copied().enumerate() {
        let at = i * 2;
        if at + 1 >= out.len() {
            break;
        }
        out[at] = HEX[(byte >> 4) as usize];
        out[at + 1] = HEX[(byte & 0x0f) as usize];
    }
}

fn wipe(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        *byte = 0;
    }
    compiler_fence(Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Baggage;

    const CANARY: &str = "rlm-canary-T012-telemetry-sk-test-9f3a";

    fn ctx_with_session() -> (TraceContext, SessionId) {
        let session = SessionId::new();
        let mut baggage = Baggage::empty();
        baggage
            .insert("session_id", session.to_string())
            .expect("session baggage");
        (TraceContext::new(TraceId::new(), None, baggage), session)
    }

    fn telemetry_with_canary() -> (Telemetry, Arc<LocalSink>, Arc<OtlpSink>) {
        let local = Arc::new(LocalSink::bounded(64));
        let otlp = Arc::new(OtlpSink::without_collector());
        let mut pipeline = RedactionPipeline::new();
        pipeline
            .register_canary(CANARY.as_bytes(), &CancellationToken::new())
            .expect("register");
        let tel = Telemetry {
            content: TelemetryContent::Off,
            policy: ExporterPolicy::local(),
            redaction: Mutex::new(pipeline),
            sinks: vec![
                Arc::clone(&local) as Arc<dyn TelemetrySink>,
                Arc::clone(&otlp) as Arc<dyn TelemetrySink>,
            ],
            metrics: MetricsRegistry::new(),
            dropped: AtomicU64::new(0),
            omitted_attributes: AtomicU64::new(0),
            closed: AtomicU64::new(0),
        };
        (tel, local, otlp)
    }

    fn serialized(records: &[TelemetryRecord]) -> String {
        records
            .iter()
            .map(|r| serde_json::to_string(r).expect("json"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn new_default_is_local_no_network() {
        let tel = Telemetry::new(TelemetryConfig::default());
        assert!(!tel.sinks().iter().any(|s| s.network_enabled()));
        assert!(tel.policy().collector().is_none());
        assert_eq!(TelemetryConfig::default().mode, TelemetryMode::Local);
        assert_eq!(TelemetryConfig::default().content, TelemetryContent::Off);
    }

    #[test]
    fn no_collector_is_supported_no_network_mode() {
        let sink = OtlpSink::without_collector();
        assert!(sink.collector().is_none());
        assert!(!sink.network_enabled());
        let (ctx, _) = ctx_with_session();
        let record = TelemetryRecord {
            schema: TELEMETRY_RECORD_SCHEMA,
            kind: RecordKind::Span,
            name: "tool.invoke".into(),
            ts_unix_ms: 1,
            correlation: CorrelationIds::from_trace(&ctx),
            attributes: AttributeSet::empty(),
            result_class: Some(ResultClass::Ok),
            latency_ms: Some(3),
            log_level: None,
            metric_kind: None,
            metric_value: None,
        };
        assert_eq!(
            sink.emit(&record, &CancellationToken::new()),
            SinkOutcome::Accepted
        );
        assert_eq!(sink.network_attempts(), 0);
        assert!(!sink.captured_payloads().expect("payloads").is_empty());
    }

    #[test]
    fn secret_canary_absent_from_sinks() {
        let (tel, local, otlp) = telemetry_with_canary();
        let (ctx, _) = ctx_with_session();
        let cancel = CancellationToken::new();
        tel.emit_span(
            "tool.invoke",
            &ctx,
            &[
                ("tool", "exec"),
                ("url", &format!("https://example.com/hook?token={CANARY}")),
                ("note", &format!("header {CANARY} trailing")),
            ],
            ResultClass::Ok,
            12,
            &cancel,
        )
        .expect("emit");

        let records = local.snapshot().expect("local");
        let json = serialized(&records);
        let otlp_text = otlp
            .captured_payloads()
            .expect("otlp")
            .into_iter()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        let debug = format!("{tel:?} {records:?}");
        for leaked in [CANARY, "sk-test"] {
            assert!(!json.contains(leaked), "local leaked {leaked}: {json}");
            assert!(
                !otlp_text.contains(leaked),
                "otlp leaked {leaked}: {otlp_text}"
            );
            assert!(!debug.contains(leaked), "debug leaked {leaked}: {debug}");
        }
        let url = records[0].attributes.get("url").expect("url kept");
        assert_eq!(url, "https://example.com/hook");
        assert!(!url.contains('?'));
        let note = records[0].attributes.get("note").expect("note kept");
        assert!(note.contains("[REDACTED:secret:"));
        assert!(!note.contains(CANARY));
    }

    #[test]
    fn protected_keys_are_omitted_not_exported() {
        let (tel, local, _) = telemetry_with_canary();
        let (ctx, _) = ctx_with_session();
        let outcome = tel
            .emit_span(
                "model.call",
                &ctx,
                &[
                    ("prompt", CANARY),
                    ("PROMPT", CANARY),
                    ("raw-prompt", CANARY),
                    ("api.key", CANARY),
                    ("tool_output", "ls -la"),
                    ("code", "fn main() {}"),
                    ("patch_code", "fn secret_impl() {}"),
                    ("query", "token=1"),
                    ("provider", "openai"),
                ],
                ResultClass::Ok,
                4,
                &CancellationToken::new(),
            )
            .expect("emit");
        assert!(outcome.omitted_attributes >= 8);
        let rec = &local.snapshot().expect("snap")[0];
        for key in [
            "prompt",
            "raw_prompt",
            "api_key",
            "tool_output",
            "code",
            "patch_code",
            "query",
        ] {
            assert!(rec.attributes.get(key).is_none(), "exported {key}");
        }
        assert_eq!(rec.attributes.get("provider"), Some("openai"));
        let json = serialized(std::slice::from_ref(rec));
        assert!(!json.contains(CANARY));
        assert!(!json.contains("fn main"));
        assert!(!json.contains("secret_impl"));
    }

    #[test]
    fn redaction_happens_before_exporter() {
        let failing = Arc::new(FailingTransport::default());
        let endpoint = CollectorEndpoint::parse("https://collector.example.invalid/v1/traces")
            .expect("endpoint");
        let otlp = OtlpSink::with_collector(
            endpoint,
            Arc::clone(&failing) as Arc<dyn OtlpTransport>,
            &ExporterPolicy::local(),
        )
        .expect("otlp");
        let local = LocalSink::bounded(8);
        let mut pipeline = RedactionPipeline::new();
        pipeline
            .register_canary(CANARY.as_bytes(), &CancellationToken::new())
            .expect("canary");
        let tel = Telemetry {
            content: TelemetryContent::Off,
            policy: ExporterPolicy::local(),
            redaction: Mutex::new(pipeline),
            sinks: vec![
                Arc::new(local) as Arc<dyn TelemetrySink>,
                Arc::new(otlp) as Arc<dyn TelemetrySink>,
            ],
            metrics: MetricsRegistry::new(),
            dropped: AtomicU64::new(0),
            omitted_attributes: AtomicU64::new(0),
            closed: AtomicU64::new(0),
        };
        let (ctx, _) = ctx_with_session();
        let outcome = tel
            .emit_span(
                "process.exec",
                &ctx,
                &[("detail", CANARY)],
                ResultClass::Error,
                9,
                &CancellationToken::new(),
            )
            .expect("emit must not fail on exporter outage");
        assert!(outcome.accepted >= 1);
        assert!(failing.attempts() >= 1);
        assert!(tel.dropped() >= 1);
    }

    #[test]
    fn exporter_outage_does_not_block_and_queue_drops() {
        let local = LocalSink::bounded(1);
        let (ctx, _) = ctx_with_session();
        let rec = TelemetryRecord {
            schema: TELEMETRY_RECORD_SCHEMA,
            kind: RecordKind::Span,
            name: "sandbox.start".into(),
            ts_unix_ms: 1,
            correlation: CorrelationIds::from_trace(&ctx),
            attributes: AttributeSet::empty(),
            result_class: Some(ResultClass::Ok),
            latency_ms: Some(1),
            log_level: None,
            metric_kind: None,
            metric_value: None,
        };
        let cancel = CancellationToken::new();
        assert_eq!(local.emit(&rec, &cancel), SinkOutcome::Accepted);
        assert_eq!(local.emit(&rec, &cancel), SinkOutcome::Dropped);
        assert_eq!(local.dropped(), 1);
        assert_eq!(local.snapshot().expect("snap").len(), 1);
    }

    #[test]
    fn trace_ids_correlate_model_tool_process() {
        let (tel, local, _) = telemetry_with_canary();
        let (ctx, session) = ctx_with_session();
        let cancel = CancellationToken::new();
        tel.emit_span(
            "model.call",
            &ctx,
            &[("provider", "mock")],
            ResultClass::Ok,
            10,
            &cancel,
        )
        .expect("model");
        tel.emit_span(
            "tool.invoke",
            &ctx,
            &[("tool", "fs")],
            ResultClass::Ok,
            4,
            &cancel,
        )
        .expect("tool");
        tel.emit_span(
            "process.exec",
            &ctx,
            &[("backend", "local")],
            ResultClass::Ok,
            20,
            &cancel,
        )
        .expect("process");
        let records = local.snapshot().expect("snap");
        assert_eq!(records.len(), 3);
        let trace = ctx.trace_id();
        for rec in &records {
            assert_eq!(rec.correlation.trace_id, trace);
            assert_eq!(rec.correlation.session_id, Some(session));
            assert!(rec.correlation.span_id.is_some());
        }
        let names: Vec<_> = records.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["model.call", "tool.invoke", "process.exec"]);
    }

    #[test]
    fn metric_cardinality_guard_and_forbidden_labels() {
        let (tel, local, _) = telemetry_with_canary();
        let (ctx, _) = ctx_with_session();
        let cancel = CancellationToken::new();
        tel.emit_metric(
            MetricKind::Counter,
            "sandbox.failures",
            1.0,
            &ctx,
            &[("prompt", CANARY), ("backend", "container")],
            &cancel,
        )
        .expect("metric");
        let rec = &local.snapshot().expect("snap")[0];
        assert!(rec.attributes.get("prompt").is_none());
        assert_eq!(rec.attributes.get("backend"), Some("container"));
        assert!(!serialized(std::slice::from_ref(rec)).contains(CANARY));

        for i in 0..(MAX_METRIC_SERIES + 4) {
            let _ = tel.emit_metric(
                MetricKind::Counter,
                "agent.queue",
                1.0,
                &ctx,
                &[("backend", &format!("b{i}"))],
                &cancel,
            );
        }
        assert!(tel.metric_cardinality_dropped() >= 1);
        assert!(tel.metric_snapshot().expect("metrics").len() <= MAX_METRIC_SERIES);
    }

    #[test]
    fn locked_local_policy_rejects_collector() {
        let policy = ExporterPolicy::locked(None);
        let endpoint = CollectorEndpoint::parse("https://collector.example.invalid/v1").unwrap();
        let err = OtlpSink::with_collector(endpoint, Arc::new(NoNetworkTransport), &policy)
            .expect_err("must not broaden");
        assert_eq!(err, TelemetryError::DestinationLocked);
    }

    #[test]
    fn collector_endpoint_rejects_secrets_and_query() {
        for raw in [
            "",
            "https://user:pass@collector.example.invalid/v1",
            "https://collector.example.invalid/v1?token=abc",
            "http://evil.example.invalid/v1",
            "ftp://127.0.0.1/v1",
        ] {
            assert_eq!(
                CollectorEndpoint::parse(raw),
                Err(TelemetryError::InvalidEndpoint),
                "accepted {raw}"
            );
        }
        assert!(CollectorEndpoint::parse("http://127.0.0.1:4318/v1/traces").is_ok());
        assert!(CollectorEndpoint::parse("https://collector.example.invalid/v1/traces").is_ok());
    }

    #[test]
    fn cancel_and_closed_are_typed() {
        let (tel, _, _) = telemetry_with_canary();
        let (ctx, _) = ctx_with_session();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            tel.emit_span("tool.invoke", &ctx, &[], ResultClass::Ok, 1, &cancel),
            Err(TelemetryError::Cancelled)
        );
        let live = CancellationToken::new();
        tel.close();
        assert_eq!(
            tel.emit_span("tool.invoke", &ctx, &[], ResultClass::Ok, 1, &live),
            Err(TelemetryError::Closed)
        );
    }

    #[test]
    fn log_status_is_redacted_and_message_key_omitted() {
        let (tel, local, _) = telemetry_with_canary();
        let (ctx, _) = ctx_with_session();
        tel.emit_log(
            LogLevel::Warn,
            "tool.invoke",
            &ctx,
            &format!("denied {CANARY}"),
            &[("message", CANARY)],
            &CancellationToken::new(),
        )
        .expect("log");
        let rec = &local.snapshot().expect("snap")[0];
        assert!(rec.attributes.get("message").is_none());
        let status = rec.attributes.get("status").expect("status");
        assert!(status.contains("[REDACTED:secret:"));
        assert!(!status.contains(CANARY));
        assert!(!serialized(std::slice::from_ref(rec)).contains(CANARY));
    }

    #[test]
    fn span_name_cannot_smuggle_canary() {
        let (tel, _, _) = telemetry_with_canary();
        let (ctx, _) = ctx_with_session();
        assert_eq!(
            tel.emit_span(
                CANARY,
                &ctx,
                &[],
                ResultClass::Ok,
                1,
                &CancellationToken::new()
            ),
            Err(TelemetryError::InvalidName)
        );
    }

    #[test]
    fn correlation_baggage_canary_absent_from_sinks() {
        let (tel, local, otlp) = telemetry_with_canary();
        let session = SessionId::new();
        let mut baggage = Baggage::empty();
        baggage
            .insert("session_id", session.to_string())
            .expect("session");
        baggage.insert("tool_call_id", CANARY).expect("tool");
        baggage.insert("model_call_id", CANARY).expect("model");
        baggage.insert("agent_lineage_id", CANARY).expect("lineage");
        baggage
            .insert("observation_id", format!("obs-{CANARY}"))
            .expect("observation");
        let ctx = TraceContext::new(TraceId::new(), None, baggage);
        let cancel = CancellationToken::new();
        tel.emit_span(
            "tool.invoke",
            &ctx,
            &[("tool", "exec")],
            ResultClass::Ok,
            3,
            &cancel,
        )
        .expect("span");
        tel.emit_log(LogLevel::Info, "tool.invoke", &ctx, "ok", &[], &cancel)
            .expect("log");
        tel.emit_metric(
            MetricKind::Counter,
            "tool.calls",
            1.0,
            &ctx,
            &[("backend", "local")],
            &cancel,
        )
        .expect("metric");

        let records = local.snapshot().expect("local");
        assert_eq!(records.len(), 3);
        let json = serialized(&records);
        let otlp_text = otlp
            .captured_payloads()
            .expect("otlp")
            .into_iter()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        let debug = format!("{tel:?} {records:?}");
        for leaked in [CANARY, "sk-test"] {
            assert!(!json.contains(leaked), "local leaked {leaked}: {json}");
            assert!(
                !otlp_text.contains(leaked),
                "otlp leaked {leaked}: {otlp_text}"
            );
            assert!(!debug.contains(leaked), "debug leaked {leaked}: {debug}");
        }
        for rec in &records {
            assert_eq!(rec.correlation.session_id, Some(session));
            assert_ne!(
                rec.correlation.tool_call_id.as_deref(),
                Some(CANARY),
                "tool_call_id kept canary"
            );
            assert_ne!(
                rec.correlation.model_call_id.as_deref(),
                Some(CANARY),
                "model_call_id kept canary"
            );
            assert_ne!(
                rec.correlation.agent_lineage_id.as_deref(),
                Some(CANARY),
                "agent_lineage_id kept canary"
            );
            if let Some(obs) = rec.correlation.observation_id.as_deref() {
                assert!(!obs.contains(CANARY), "observation_id kept canary: {obs}");
            }
        }
    }

    #[test]
    fn pipeline_debug_omits_canary_plaintext() {
        let mut pipeline = RedactionPipeline::new();
        pipeline
            .register_canary(CANARY.as_bytes(), &CancellationToken::new())
            .expect("reg");
        let debug = format!("{pipeline:?}");
        assert!(!debug.contains(CANARY));
        assert!(debug.contains("canary_count"));
    }
}
