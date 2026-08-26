//! User-previewable redacted diagnostic bundle export.
//!
//! `export_trace(scope, policy)` writes local content-addressed trace artifacts
//! and returns an [`ArtifactRef`] to a manifest that lists included categories
//! and redactions. The bundle is never sent externally from this module.
//! Content mode `off` cannot include raw prompt/code. Threat: `T-012`.

use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};

use protocol::{
    ArtifactId, ArtifactRef, RedactionClass, SessionId, TelemetryContent, TraceId, TurnId,
};
use serde::{Deserialize, Serialize};

use crate::{
    AttributeSet, CancellationToken, CorrelationIds, MAX_ATTRIBUTE_VALUE_BYTES, RecordKind,
    RedactionPipeline, TELEMETRY_RECORD_SCHEMA, TelemetryError, TelemetryRecord, is_forbidden_key,
    is_safe_name, normalize_key,
};

/// Manifest schema version (serialized as `schema`).
pub const TRACE_EXPORT_MANIFEST_SCHEMA: u16 = 1;

/// Maximum records accepted into one export.
pub const MAX_EXPORT_RECORDS: usize = 1024;

/// Maximum UTF-8 bytes per category artifact or the manifest.
pub const MAX_EXPORT_ARTIFACT_BYTES: usize = 1024 * 1024;

/// Maximum distinct category artifacts referenced by one manifest.
pub const MAX_EXPORT_ARTIFACTS: usize = 8;

const MANIFEST_MEDIA_TYPE: &str = "application/vnd.rapidlm.diagnostic-manifest.v1+json";
const SPANS_MEDIA_TYPE: &str = "application/vnd.rapidlm.trace-spans.v1+json";
const LOGS_MEDIA_TYPE: &str = "application/vnd.rapidlm.trace-logs.v1+json";
const METRICS_MEDIA_TYPE: &str = "application/vnd.rapidlm.trace-metrics.v1+json";

/// Record categories that can appear in a diagnostic bundle.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportCategory {
    Spans,
    Logs,
    Metrics,
}

/// Why a field, category, or payload was omitted from the bundle.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionKind {
    ContentModeOff,
    ForbiddenField,
    SecretCanary,
    DiagnosticRequired,
}

/// Retention label carried on the previewable manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionLabel {
    LocalPreview,
    LocalDiagnostic,
}

/// One redaction applied while building the bundle.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RedactionEntry {
    pub kind: RedactionKind,
    pub detail: String,
}

/// Artifact listed by the inspectable manifest. Metadata only.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ManifestArtifact {
    pub category: ExportCategory,
    #[serde(flatten)]
    pub artifact: ArtifactRef,
}

/// Inspectable diagnostic-bundle description. Contains no record payloads.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticManifest {
    pub schema: u16,
    pub content_mode: TelemetryContent,
    pub diagnostic: bool,
    pub include_content: bool,
    pub included_categories: Vec<ExportCategory>,
    pub omitted_categories: Vec<ExportCategory>,
    pub redactions: Vec<RedactionEntry>,
    pub artifacts: Vec<ManifestArtifact>,
    pub record_count: u32,
    pub omitted_records: u32,
    pub omitted_fields: u32,
    pub retention: RetentionLabel,
    pub shared: bool,
}

/// Filter over local sink records for one export.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExportScope {
    session_id: Option<SessionId>,
    trace_id: Option<TraceId>,
    turn_id: Option<TurnId>,
    since_unix_ms: Option<u64>,
    until_unix_ms: Option<u64>,
    categories: Vec<ExportCategory>,
}

/// Policy for what a diagnostic export may contain.
///
/// Content inclusion is explicit. When [`TelemetryContent`] is `off`, requesting
/// raw content fails closed instead of silently including prompt/code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportPolicy {
    content: TelemetryContent,
    diagnostic: bool,
    include_content: bool,
}

/// Collects records and canaries, then builds a local redacted bundle.
pub struct TraceArtifactBuilder {
    records: Vec<TelemetryRecord>,
    pipeline: RedactionPipeline,
}

/// Local, previewable export result. The manifest is inspectable before share.
pub struct TraceExport {
    manifest_ref: ArtifactRef,
    manifest: DiagnosticManifest,
    store: MemoryArtifactStore,
}

struct MemoryArtifactStore {
    blobs: BTreeMap<ArtifactId, Vec<u8>>,
}

#[derive(Serialize)]
struct CategoryArtifact<'a> {
    schema: u16,
    category: ExportCategory,
    redaction: RedactionClass,
    records: &'a [TelemetryRecord],
}

impl ExportCategory {
    pub const ALL: &'static [Self] = &[Self::Spans, Self::Logs, Self::Metrics];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spans => "spans",
            Self::Logs => "logs",
            Self::Metrics => "metrics",
        }
    }

    fn from_kind(kind: RecordKind) -> Self {
        match kind {
            RecordKind::Span => Self::Spans,
            RecordKind::Log => Self::Logs,
            RecordKind::Metric => Self::Metrics,
        }
    }

    fn media_type(self) -> &'static str {
        match self {
            Self::Spans => SPANS_MEDIA_TYPE,
            Self::Logs => LOGS_MEDIA_TYPE,
            Self::Metrics => METRICS_MEDIA_TYPE,
        }
    }
}

impl RedactionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContentModeOff => "content_mode_off",
            Self::ForbiddenField => "forbidden_field",
            Self::SecretCanary => "secret_canary",
            Self::DiagnosticRequired => "diagnostic_required",
        }
    }
}

impl RetentionLabel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalPreview => "local_preview",
            Self::LocalDiagnostic => "local_diagnostic",
        }
    }
}

impl ExportScope {
    /// Include every local record and category, subject to resource bounds.
    pub fn all() -> Self {
        Self::default()
    }

    pub fn session(session_id: SessionId) -> Self {
        Self {
            session_id: Some(session_id),
            ..Self::default()
        }
    }

    pub fn trace(trace_id: TraceId) -> Self {
        Self {
            trace_id: Some(trace_id),
            ..Self::default()
        }
    }

    pub fn with_turn(mut self, turn_id: TurnId) -> Self {
        self.turn_id = Some(turn_id);
        self
    }

    pub fn with_time_window(
        mut self,
        since_unix_ms: Option<u64>,
        until_unix_ms: Option<u64>,
    ) -> Self {
        self.since_unix_ms = since_unix_ms;
        self.until_unix_ms = until_unix_ms;
        self
    }

    pub fn with_categories(mut self, categories: impl Into<Vec<ExportCategory>>) -> Self {
        self.categories = categories.into();
        self
    }

    pub fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    pub fn trace_id(&self) -> Option<TraceId> {
        self.trace_id
    }

    pub fn turn_id(&self) -> Option<TurnId> {
        self.turn_id
    }

    pub fn categories(&self) -> &[ExportCategory] {
        &self.categories
    }

    fn allows_category(&self, category: ExportCategory) -> bool {
        self.categories.is_empty() || self.categories.contains(&category)
    }

    fn matches(&self, record: &TelemetryRecord) -> bool {
        if !self.allows_category(ExportCategory::from_kind(record.kind)) {
            return false;
        }
        if let Some(trace_id) = self.trace_id
            && record.correlation.trace_id != trace_id
        {
            return false;
        }
        if let Some(session_id) = self.session_id
            && record.correlation.session_id != Some(session_id)
        {
            return false;
        }
        if let Some(turn_id) = self.turn_id
            && record.correlation.turn_id != Some(turn_id)
        {
            return false;
        }
        if let Some(since) = self.since_unix_ms
            && record.ts_unix_ms < since
        {
            return false;
        }
        if let Some(until) = self.until_unix_ms
            && record.ts_unix_ms > until
        {
            return false;
        }
        true
    }
}

impl ExportPolicy {
    /// Default user preview: content off, no project-sensitive attribute values.
    pub fn preview_redacted() -> Self {
        Self {
            content: TelemetryContent::Off,
            diagnostic: false,
            include_content: false,
        }
    }

    /// Local diagnostic labels; still content-minimized while mode is `off`.
    pub fn diagnostic_redacted() -> Self {
        Self {
            content: TelemetryContent::Off,
            diagnostic: true,
            include_content: false,
        }
    }

    /// Request raw prompt/code bodies. Fails closed when content mode is `off`.
    pub fn request_content(mut self) -> Self {
        self.include_content = true;
        self
    }

    pub fn content(&self) -> TelemetryContent {
        self.content
    }

    pub fn diagnostic(&self) -> bool {
        self.diagnostic
    }

    pub fn include_content(&self) -> bool {
        self.include_content
    }

    fn content_allowed(&self) -> bool {
        self.include_content && !matches!(self.content, TelemetryContent::Off)
    }
}

impl Default for ExportPolicy {
    fn default() -> Self {
        Self::preview_redacted()
    }
}

impl TraceArtifactBuilder {
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
            pipeline: RedactionPipeline::new(),
        }
    }

    pub fn from_records(records: impl IntoIterator<Item = TelemetryRecord>) -> Self {
        Self {
            records: records.into_iter().collect(),
            pipeline: RedactionPipeline::new(),
        }
    }

    pub fn ingest(
        &mut self,
        records: impl IntoIterator<Item = TelemetryRecord>,
        cancel: &CancellationToken,
    ) -> Result<(), TelemetryError> {
        for record in records {
            cancel.check()?;
            if self.records.len() >= MAX_EXPORT_RECORDS {
                return Err(TelemetryError::BoundExceeded);
            }
            self.records.push(record);
        }
        Ok(())
    }

    pub fn register_canary(
        &mut self,
        plaintext: &[u8],
        cancel: &CancellationToken,
    ) -> Result<crate::CanaryFingerprint, TelemetryError> {
        self.pipeline.register_canary(plaintext, cancel)
    }

    /// Build a local redacted bundle. The returned [`ArtifactRef`] addresses
    /// the inspectable manifest, not an external share.
    pub fn export_trace(
        &self,
        scope: &ExportScope,
        policy: &ExportPolicy,
        cancel: &CancellationToken,
    ) -> Result<TraceExport, TelemetryError> {
        cancel.check()?;
        if policy.include_content && !policy.content_allowed() {
            return Err(TelemetryError::ContentDisabled);
        }

        let mut redactions = Vec::new();
        record_content_mode_redactions(policy, &mut redactions);

        let mut omitted_records = 0u32;
        let mut omitted_fields = 0u32;
        let mut by_category: BTreeMap<ExportCategory, Vec<TelemetryRecord>> = BTreeMap::new();

        for record in &self.records {
            cancel.check()?;
            if !scope.matches(record) {
                omitted_records = omitted_records.saturating_add(1);
                continue;
            }
            match sanitize_record(record, policy, &self.pipeline, &mut redactions, cancel) {
                Ok((sanitized, fields)) => {
                    omitted_fields = omitted_fields.saturating_add(fields);
                    let category = ExportCategory::from_kind(sanitized.kind);
                    let bucket = by_category.entry(category).or_default();
                    if bucket.len().saturating_add(1) > MAX_EXPORT_RECORDS {
                        return Err(TelemetryError::BoundExceeded);
                    }
                    bucket.push(sanitized);
                }
                Err(TelemetryError::Cancelled) => return Err(TelemetryError::Cancelled),
                Err(_) => {
                    omitted_records = omitted_records.saturating_add(1);
                }
            }
        }

        let mut store = MemoryArtifactStore::new();
        let mut artifacts = Vec::new();
        let mut included_categories = Vec::new();
        let mut omitted_categories = Vec::new();
        let mut record_count = 0u32;
        let artifact_class = if policy.diagnostic {
            RedactionClass::Project
        } else {
            RedactionClass::Public
        };

        for category in ExportCategory::ALL {
            cancel.check()?;
            if !scope.allows_category(*category) {
                omitted_categories.push(*category);
                push_redaction(
                    &mut redactions,
                    RedactionKind::ForbiddenField,
                    category.as_str(),
                );
                continue;
            }
            let Some(records) = by_category.get(category) else {
                omitted_categories.push(*category);
                continue;
            };
            if records.is_empty() {
                omitted_categories.push(*category);
                continue;
            }
            let envelope = CategoryArtifact {
                schema: TELEMETRY_RECORD_SCHEMA,
                category: *category,
                redaction: artifact_class,
                records,
            };
            let bytes = serde_json::to_vec(&envelope).map_err(|_| TelemetryError::Unavailable)?;
            if bytes.len() > MAX_EXPORT_ARTIFACT_BYTES {
                return Err(TelemetryError::BoundExceeded);
            }
            if self.pipeline.contains_canary_bytes(&bytes) {
                omitted_categories.push(*category);
                omitted_records = omitted_records.saturating_add(records.len() as u32);
                push_redaction(
                    &mut redactions,
                    RedactionKind::SecretCanary,
                    category.as_str(),
                );
                continue;
            }
            if artifacts.len() >= MAX_EXPORT_ARTIFACTS {
                return Err(TelemetryError::BoundExceeded);
            }
            let artifact = store.put(&bytes, category.media_type(), artifact_class, cancel)?;
            artifacts.push(ManifestArtifact {
                category: *category,
                artifact,
            });
            included_categories.push(*category);
            record_count = record_count.saturating_add(records.len() as u32);
        }

        let manifest = DiagnosticManifest {
            schema: TRACE_EXPORT_MANIFEST_SCHEMA,
            content_mode: TelemetryContent::Off,
            diagnostic: policy.diagnostic,
            include_content: false,
            included_categories,
            omitted_categories,
            redactions,
            artifacts,
            record_count,
            omitted_records,
            omitted_fields,
            retention: if policy.diagnostic {
                RetentionLabel::LocalDiagnostic
            } else {
                RetentionLabel::LocalPreview
            },
            shared: false,
        };
        let manifest_bytes =
            serde_json::to_vec(&manifest).map_err(|_| TelemetryError::Unavailable)?;
        if manifest_bytes.len() > MAX_EXPORT_ARTIFACT_BYTES {
            return Err(TelemetryError::BoundExceeded);
        }
        if self.pipeline.contains_canary_bytes(&manifest_bytes) {
            return Err(TelemetryError::Unavailable);
        }
        let manifest_ref = store.put(
            &manifest_bytes,
            MANIFEST_MEDIA_TYPE,
            RedactionClass::Public,
            cancel,
        )?;
        Ok(TraceExport {
            manifest_ref,
            manifest,
            store,
        })
    }
}

impl Default for TraceArtifactBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for TraceArtifactBuilder {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("TraceArtifactBuilder")
            .field("records", &self.records.len())
            .field("canaries", &self.pipeline.canary_count())
            .finish()
    }
}

/// Build a local redacted bundle from already-collected records.
///
/// Returns a [`TraceExport`] whose [`TraceExport::artifact_ref`] is the
/// inspectable manifest. External sharing is never performed here.
pub fn export_trace(
    records: &[TelemetryRecord],
    scope: &ExportScope,
    policy: &ExportPolicy,
    cancel: &CancellationToken,
) -> Result<TraceExport, TelemetryError> {
    TraceArtifactBuilder::from_records(records.iter().cloned()).export_trace(scope, policy, cancel)
}

impl TraceExport {
    /// Content-address of the inspectable manifest.
    pub fn artifact_ref(&self) -> ArtifactRef {
        self.manifest_ref.clone()
    }

    /// User-previewable description of included categories and redactions.
    pub fn manifest(&self) -> &DiagnosticManifest {
        &self.manifest
    }

    /// Verified local bytes for a referenced artifact. Never a network fetch.
    pub fn local_artifact(&self, id: &ArtifactId) -> Result<&[u8], TelemetryError> {
        self.store.get(id)
    }

    pub fn local_artifact_ids(&self) -> impl Iterator<Item = ArtifactId> + '_ {
        self.store.blobs.keys().copied()
    }
}

impl From<TraceExport> for ArtifactRef {
    fn from(export: TraceExport) -> Self {
        export.manifest_ref
    }
}

impl Debug for TraceExport {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("TraceExport")
            .field("manifest_ref", &self.manifest_ref.id)
            .field("included_categories", &self.manifest.included_categories)
            .field("redaction_count", &self.manifest.redactions.len())
            .field("record_count", &self.manifest.record_count)
            .field("shared", &self.manifest.shared)
            .field("blobs", &self.store.blobs.len())
            .finish()
    }
}

impl MemoryArtifactStore {
    fn new() -> Self {
        Self {
            blobs: BTreeMap::new(),
        }
    }

    fn put(
        &mut self,
        bytes: &[u8],
        media_type: &str,
        redaction: RedactionClass,
        cancel: &CancellationToken,
    ) -> Result<ArtifactRef, TelemetryError> {
        cancel.check()?;
        if bytes.len() > MAX_EXPORT_ARTIFACT_BYTES {
            return Err(TelemetryError::BoundExceeded);
        }
        let id = ArtifactId::from_bytes(bytes);
        self.blobs.entry(id).or_insert_with(|| bytes.to_vec());
        Ok(ArtifactRef::new(
            id,
            media_type,
            bytes.len() as u64,
            redaction,
        ))
    }

    fn get(&self, id: &ArtifactId) -> Result<&[u8], TelemetryError> {
        self.blobs
            .get(id)
            .map(Vec::as_slice)
            .ok_or(TelemetryError::Unavailable)
    }
}

impl RedactionPipeline {
    fn contains_canary_bytes(&self, bytes: &[u8]) -> bool {
        std::str::from_utf8(bytes)
            .map(|text| self.contains_canary(text))
            .unwrap_or(false)
    }
}

fn record_content_mode_redactions(policy: &ExportPolicy, redactions: &mut Vec<RedactionEntry>) {
    push_redaction(redactions, RedactionKind::ContentModeOff, "prompt");
    push_redaction(redactions, RedactionKind::ContentModeOff, "code");
    push_redaction(redactions, RedactionKind::ContentModeOff, "tool_output");
    push_redaction(redactions, RedactionKind::ContentModeOff, "secret");
    push_redaction(redactions, RedactionKind::ContentModeOff, "file_contents");
    push_redaction(redactions, RedactionKind::ContentModeOff, "screenshot");
    if !policy.diagnostic {
        push_redaction(
            redactions,
            RedactionKind::DiagnosticRequired,
            "project_sensitive_attributes",
        );
    }
}

fn sanitize_record(
    record: &TelemetryRecord,
    policy: &ExportPolicy,
    pipeline: &RedactionPipeline,
    redactions: &mut Vec<RedactionEntry>,
    cancel: &CancellationToken,
) -> Result<(TelemetryRecord, u32), TelemetryError> {
    cancel.check()?;
    if record.name.is_empty()
        || record.name.len() > crate::MAX_NAME_BYTES
        || !is_safe_name(&record.name)
        || pipeline.contains_canary(&record.name)
    {
        return Err(TelemetryError::InvalidName);
    }

    let mut sanitized = record.clone();
    let mut omitted = 0u32;
    omitted = omitted.saturating_add(filter_attributes(
        &mut sanitized.attributes,
        policy,
        pipeline,
        redactions,
        cancel,
    )?);
    omitted = omitted.saturating_add(redact_correlation(
        &mut sanitized.correlation,
        pipeline,
        redactions,
        cancel,
    )?);
    Ok((sanitized, omitted))
}

fn filter_attributes(
    attrs: &mut AttributeSet,
    policy: &ExportPolicy,
    pipeline: &RedactionPipeline,
    redactions: &mut Vec<RedactionEntry>,
    cancel: &CancellationToken,
) -> Result<u32, TelemetryError> {
    cancel.check()?;
    let keys: Vec<String> = attrs.fields.keys().cloned().collect();
    let mut omitted = 0u32;
    for key in keys {
        cancel.check()?;
        let Some(value) = attrs.fields.get(&key).cloned() else {
            continue;
        };
        let normalized = normalize_key(&key);
        if is_forbidden_key(&normalized) || is_content_key(&normalized) {
            attrs.fields.remove(&key);
            omitted = omitted.saturating_add(1);
            push_redaction(redactions, RedactionKind::ForbiddenField, &normalized);
            continue;
        }
        if !policy.diagnostic && !is_public_export_key(&normalized) {
            attrs.fields.remove(&key);
            omitted = omitted.saturating_add(1);
            push_redaction(redactions, RedactionKind::DiagnosticRequired, &normalized);
            continue;
        }
        if value.len() > MAX_ATTRIBUTE_VALUE_BYTES {
            attrs.fields.remove(&key);
            omitted = omitted.saturating_add(1);
            continue;
        }
        let redacted = match pipeline.redact_text(&value, cancel) {
            Ok(text) => text,
            Err(TelemetryError::Cancelled) => return Err(TelemetryError::Cancelled),
            Err(_) => {
                attrs.fields.remove(&key);
                omitted = omitted.saturating_add(1);
                continue;
            }
        };
        if pipeline.contains_canary(&redacted) {
            attrs.fields.remove(&key);
            omitted = omitted.saturating_add(1);
            push_redaction(redactions, RedactionKind::SecretCanary, &normalized);
            continue;
        }
        attrs.fields.insert(key, redacted);
    }
    attrs.omitted = attrs.omitted.saturating_add(omitted);
    Ok(omitted)
}

fn redact_correlation(
    correlation: &mut CorrelationIds,
    pipeline: &RedactionPipeline,
    redactions: &mut Vec<RedactionEntry>,
    cancel: &CancellationToken,
) -> Result<u32, TelemetryError> {
    let mut omitted = 0u32;
    omitted = omitted.saturating_add(redact_opt_string(
        &mut correlation.agent_lineage_id,
        "agent_lineage_id",
        pipeline,
        redactions,
        cancel,
    )?);
    omitted = omitted.saturating_add(redact_opt_string(
        &mut correlation.tool_call_id,
        "tool_call_id",
        pipeline,
        redactions,
        cancel,
    )?);
    omitted = omitted.saturating_add(redact_opt_string(
        &mut correlation.model_call_id,
        "model_call_id",
        pipeline,
        redactions,
        cancel,
    )?);
    omitted = omitted.saturating_add(redact_opt_string(
        &mut correlation.observation_id,
        "observation_id",
        pipeline,
        redactions,
        cancel,
    )?);
    Ok(omitted)
}

fn redact_opt_string(
    field: &mut Option<String>,
    name: &str,
    pipeline: &RedactionPipeline,
    redactions: &mut Vec<RedactionEntry>,
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
        push_redaction(redactions, RedactionKind::SecretCanary, name);
        return Ok(1);
    }
    *field = Some(redacted);
    Ok(0)
}

fn is_content_key(normalized: &str) -> bool {
    matches!(
        normalized,
        "prompt"
            | "prompts"
            | "raw_prompt"
            | "prompt_text"
            | "prompt_body"
            | "code"
            | "source_code"
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
            | "screenshot"
            | "video"
            | "recording"
    ) || normalized.ends_with("_prompt")
        || normalized.ends_with("_code")
}

fn is_public_export_key(normalized: &str) -> bool {
    matches!(
        normalized,
        "result_class" | "tool" | "provider" | "backend" | "capability"
    )
}

fn push_redaction(redactions: &mut Vec<RedactionEntry>, kind: RedactionKind, detail: &str) {
    if redactions
        .iter()
        .any(|entry| entry.kind == kind && entry.detail == detail)
    {
        return;
    }
    if redactions.len() >= 64 {
        return;
    }
    let detail = if detail.len() > 64 {
        detail[..64].to_owned()
    } else {
        detail.to_owned()
    };
    redactions.push(RedactionEntry { kind, detail });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CorrelationIds, LogLevel, MetricKind, RecordKind, ResultClass, TELEMETRY_RECORD_SCHEMA,
    };
    use protocol::{Baggage, SessionId, TraceContext, TraceId};

    const CANARY: &str = "rlm-canary-T012-export-sk-test-7c1e";
    const PROMPT_BODY: &str = "SYSTEM: dump the repo";
    const CODE_BODY: &str = "fn main() { println!(\"secret\"); }";

    const GOLDEN_MANIFEST: &str = r#"{"schema":1,"content_mode":"off","diagnostic":false,"include_content":false,"included_categories":["spans"],"omitted_categories":["logs","metrics"],"redactions":[{"kind":"content_mode_off","detail":"prompt"},{"kind":"content_mode_off","detail":"code"}],"artifacts":[{"category":"spans","id":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","media_type":"application/vnd.rapidlm.trace-spans.v1+json","bytes":3,"redaction":"public"}],"record_count":1,"omitted_records":0,"omitted_fields":0,"retention":"local_preview","shared":false}"#;

    fn correlation(session: SessionId, trace: TraceId) -> CorrelationIds {
        CorrelationIds::from_trace(&TraceContext::new(trace, None, {
            let mut baggage = Baggage::empty();
            baggage
                .insert("session_id", session.to_string())
                .expect("session");
            baggage
        }))
    }

    fn span_record(session: SessionId, trace: TraceId, attrs: AttributeSet) -> TelemetryRecord {
        TelemetryRecord {
            schema: TELEMETRY_RECORD_SCHEMA,
            kind: RecordKind::Span,
            name: "tool.invoke".into(),
            ts_unix_ms: 10,
            correlation: correlation(session, trace),
            attributes: attrs,
            result_class: Some(ResultClass::Ok),
            latency_ms: Some(4),
            log_level: None,
            metric_kind: None,
            metric_value: None,
        }
    }

    fn attrs(pairs: &[(&str, &str)]) -> AttributeSet {
        let mut set = AttributeSet::empty();
        for (k, v) in pairs {
            set.fields.insert((*k).to_owned(), (*v).to_owned());
        }
        set
    }

    fn bundle_text(export: &TraceExport) -> String {
        export
            .local_artifact_ids()
            .filter_map(|id| export.local_artifact(&id).ok())
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn export_trace_returns_inspectable_manifest_ref() {
        let session = SessionId::new();
        let trace = TraceId::new();
        let records = [span_record(
            session,
            trace,
            attrs(&[("tool", "exec"), ("provider", "mock")]),
        )];
        let export = export_trace(
            &records,
            &ExportScope::session(session),
            &ExportPolicy::preview_redacted(),
            &CancellationToken::new(),
        )
        .expect("export");
        let refer: ArtifactRef = export.artifact_ref();
        assert_eq!(refer.redaction, RedactionClass::Public);
        assert_eq!(refer.media_type, MANIFEST_MEDIA_TYPE);
        assert!(!export.manifest().shared);
        assert_eq!(export.manifest().content_mode, TelemetryContent::Off);
        assert!(
            export
                .manifest()
                .included_categories
                .contains(&ExportCategory::Spans)
        );
        assert!(
            export
                .manifest()
                .redactions
                .iter()
                .any(|r| r.kind == RedactionKind::ContentModeOff && r.detail == "prompt")
        );
        assert!(
            export
                .manifest()
                .redactions
                .iter()
                .any(|r| r.kind == RedactionKind::ContentModeOff && r.detail == "code")
        );
        let preview = export.local_artifact(&refer.id).expect("manifest bytes");
        let decoded: DiagnosticManifest = serde_json::from_slice(preview).expect("manifest json");
        assert_eq!(decoded, *export.manifest());
        assert!(!decoded.shared);
    }

    #[test]
    fn content_mode_off_never_includes_prompt_or_code() {
        let session = SessionId::new();
        let trace = TraceId::new();
        let records = [span_record(
            session,
            trace,
            attrs(&[
                ("prompt", PROMPT_BODY),
                ("code", CODE_BODY),
                ("tool_output", "ls -la"),
                ("tool", "exec"),
            ]),
        )];
        let export = export_trace(
            &records,
            &ExportScope::all(),
            &ExportPolicy::diagnostic_redacted(),
            &CancellationToken::new(),
        )
        .expect("export");
        let json = bundle_text(&export);
        let debug = format!("{export:?}");
        for leaked in [PROMPT_BODY, CODE_BODY, "ls -la"] {
            assert!(!json.contains(leaked), "bundle leaked {leaked}: {json}");
            assert!(!debug.contains(leaked), "debug leaked {leaked}: {debug}");
        }
        assert!(export.manifest().omitted_fields >= 3);
        assert!(!export.manifest().include_content);
    }

    #[test]
    fn requesting_content_while_off_is_typed_failure() {
        let session = SessionId::new();
        let trace = TraceId::new();
        let records = [span_record(
            session,
            trace,
            attrs(&[("prompt", PROMPT_BODY), ("code", CODE_BODY)]),
        )];
        let err = export_trace(
            &records,
            &ExportScope::all(),
            &ExportPolicy::preview_redacted().request_content(),
            &CancellationToken::new(),
        )
        .expect_err("must not silently include content");
        assert_eq!(err, TelemetryError::ContentDisabled);
    }

    #[test]
    fn secret_canary_absent_from_exported_bundle() {
        let session = SessionId::new();
        let trace = TraceId::new();
        let mut builder = TraceArtifactBuilder::from_records([span_record(
            session,
            trace,
            attrs(&[
                ("tool", "exec"),
                ("note", &format!("hdr {CANARY} tail")),
                ("prompt", CANARY),
            ]),
        )]);
        builder
            .register_canary(CANARY.as_bytes(), &CancellationToken::new())
            .expect("canary");
        let export = builder
            .export_trace(
                &ExportScope::all(),
                &ExportPolicy::diagnostic_redacted(),
                &CancellationToken::new(),
            )
            .expect("export");
        let json = bundle_text(&export);
        let debug = format!("{export:?} {:?}", export.manifest());
        assert!(!json.contains(CANARY), "bundle leaked canary: {json}");
        assert!(!debug.contains(CANARY), "debug leaked canary: {debug}");
        assert!(!json.contains(PROMPT_BODY));
    }

    #[test]
    fn non_diagnostic_strips_project_sensitive_attributes() {
        let session = SessionId::new();
        let trace = TraceId::new();
        let records = [span_record(
            session,
            trace,
            attrs(&[("tool", "exec"), ("note", "repo-relative path hint")]),
        )];
        let export = export_trace(
            &records,
            &ExportScope::all(),
            &ExportPolicy::preview_redacted(),
            &CancellationToken::new(),
        )
        .expect("export");
        let json = bundle_text(&export);
        assert!(!json.contains("repo-relative path hint"));
        assert!(json.contains("\"tool\":\"exec\""));
        assert!(
            export
                .manifest()
                .redactions
                .iter()
                .any(|r| r.kind == RedactionKind::DiagnosticRequired)
        );
    }

    #[test]
    fn scope_filters_session_and_category() {
        let keep = SessionId::new();
        let drop = SessionId::new();
        let trace = TraceId::new();
        let span = span_record(keep, trace, attrs(&[("tool", "fs")]));
        let mut other = span_record(drop, trace, attrs(&[("tool", "net")]));
        other.name = "model.call".into();
        let mut metric = span.clone();
        metric.kind = RecordKind::Metric;
        metric.name = "tool.calls".into();
        metric.metric_kind = Some(MetricKind::Counter);
        metric.metric_value = Some(1.0);
        let mut log = span.clone();
        log.kind = RecordKind::Log;
        log.log_level = Some(LogLevel::Info);
        let export = export_trace(
            &[span, other, metric, log],
            &ExportScope::session(keep).with_categories(vec![ExportCategory::Spans]),
            &ExportPolicy::preview_redacted(),
            &CancellationToken::new(),
        )
        .expect("export");
        assert_eq!(
            export.manifest().included_categories,
            vec![ExportCategory::Spans]
        );
        assert!(
            export
                .manifest()
                .omitted_categories
                .contains(&ExportCategory::Logs)
        );
        assert!(
            export
                .manifest()
                .omitted_categories
                .contains(&ExportCategory::Metrics)
        );
        let json = bundle_text(&export);
        assert!(json.contains("tool.invoke"));
        assert!(!json.contains("model.call"));
        assert!(!json.contains("tool.calls"));
    }

    #[test]
    fn cancel_and_bounds_are_typed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = export_trace(
            &[],
            &ExportScope::all(),
            &ExportPolicy::preview_redacted(),
            &cancel,
        )
        .expect_err("cancelled");
        assert_eq!(err, TelemetryError::Cancelled);

        let session = SessionId::new();
        let trace = TraceId::new();
        let record = span_record(session, trace, attrs(&[("tool", "exec")]));
        let mut builder = TraceArtifactBuilder::new();
        let overflow = vec![record; MAX_EXPORT_RECORDS + 1];
        let err = builder
            .ingest(overflow, &CancellationToken::new())
            .expect_err("bound");
        assert_eq!(err, TelemetryError::BoundExceeded);
    }

    #[test]
    fn manifest_golden_round_trip_lists_categories_and_redactions() {
        let artifact = ArtifactRef::new(
            ArtifactId::from_bytes(b"abc"),
            SPANS_MEDIA_TYPE,
            3,
            RedactionClass::Public,
        );
        let manifest = DiagnosticManifest {
            schema: TRACE_EXPORT_MANIFEST_SCHEMA,
            content_mode: TelemetryContent::Off,
            diagnostic: false,
            include_content: false,
            included_categories: vec![ExportCategory::Spans],
            omitted_categories: vec![ExportCategory::Logs, ExportCategory::Metrics],
            redactions: vec![
                RedactionEntry {
                    kind: RedactionKind::ContentModeOff,
                    detail: "prompt".into(),
                },
                RedactionEntry {
                    kind: RedactionKind::ContentModeOff,
                    detail: "code".into(),
                },
            ],
            artifacts: vec![ManifestArtifact {
                category: ExportCategory::Spans,
                artifact,
            }],
            record_count: 1,
            omitted_records: 0,
            omitted_fields: 0,
            retention: RetentionLabel::LocalPreview,
            shared: false,
        };
        let json = serde_json::to_string(&manifest).expect("serialize");
        assert_eq!(json, GOLDEN_MANIFEST);
        let decoded: DiagnosticManifest = serde_json::from_str(GOLDEN_MANIFEST).expect("decode");
        assert_eq!(decoded, manifest);
        assert!(!json.contains("prompt_text"));
        assert!(!json.contains(PROMPT_BODY));
        assert!(!json.contains(CODE_BODY));
        assert!(!json.contains(CANARY));
        assert_eq!(json.matches("\"shared\":false").count(), 1);
    }

    #[test]
    fn export_does_not_share_and_debug_omits_payloads() {
        let session = SessionId::new();
        let trace = TraceId::new();
        let export = export_trace(
            &[span_record(session, trace, attrs(&[("tool", "exec")]))],
            &ExportScope::all(),
            &ExportPolicy::preview_redacted(),
            &CancellationToken::new(),
        )
        .expect("export");
        assert!(!export.manifest().shared);
        let debug = format!("{export:?}");
        assert!(debug.contains("included_categories"));
        assert!(!debug.contains("SYSTEM"));
        let refer: ArtifactRef = export.into();
        assert_eq!(refer.media_type, MANIFEST_MEDIA_TYPE);
    }
}
