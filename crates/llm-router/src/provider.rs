//! Canonical provider/model metadata and streaming request/response types.
//!
//! Provider metadata is a snapshot for one [`CatalogRevision`]. Unknown
//! provider usage keys are retained; missing cost is unknown, never zero.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use protocol::{ApiError, ArtifactRef, ErrorCode, TraceContext, TraceId, UNKNOWN_INTERNAL_MESSAGE};
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire schema name for [`ProviderCapabilities`].
pub const PROVIDER_CAPABILITIES_SCHEMA: &str = "rapidlm.provider_capabilities";

/// Wire schema name for [`ModelDescriptor`].
pub const MODEL_DESCRIPTOR_SCHEMA: &str = "rapidlm.model_descriptor";

/// Wire schema name for [`CanonicalModelRequest`].
pub const CANONICAL_MODEL_REQUEST_SCHEMA: &str = "rapidlm.canonical_model_request";

/// Wire schema name for [`NormalizedUsage`].
pub const NORMALIZED_USAGE_SCHEMA: &str = "rapidlm.normalized_usage";

/// Wire schema name for [`ModelStreamEvent`].
pub const MODEL_STREAM_EVENT_SCHEMA: &str = "rapidlm.model_stream_event";

/// v1 schema version for provider/model objects.
pub const PROVIDER_SCHEMA_VERSION: u16 = 1;

/// Maximum UTF-8 bytes for a provider identifier.
pub const MAX_PROVIDER_ID_BYTES: usize = 64;

/// Maximum UTF-8 bytes for a model identifier.
pub const MAX_MODEL_ID_BYTES: usize = 128;

/// Maximum UTF-8 bytes for a model request identifier.
pub const MAX_REQUEST_ID_BYTES: usize = 256;

/// Maximum UTF-8 bytes for a region token.
pub const MAX_REGION_BYTES: usize = 32;

/// Maximum regions on one descriptor.
pub const MAX_REGIONS: usize = 16;

/// Maximum UTF-8 bytes for a data-policy tag.
pub const MAX_DATA_POLICY_TAG_BYTES: usize = 64;

/// Maximum data-policy tags on one descriptor.
pub const MAX_DATA_POLICY_TAGS: usize = 16;

/// Maximum UTF-8 bytes for a price-table version token.
pub const MAX_PRICE_TABLE_VERSION_BYTES: usize = 64;

/// Maximum messages on one canonical request.
pub const MAX_MESSAGES: usize = 256;

/// Maximum content parts on one message.
pub const MAX_CONTENT_PARTS: usize = 32;

/// Maximum UTF-8 bytes for one text content part.
pub const MAX_TEXT_PART_BYTES: usize = 64 * 1024;

/// Maximum tools on one canonical request.
pub const MAX_TOOLS: usize = 64;

/// Maximum UTF-8 bytes for a tool name.
pub const MAX_TOOL_NAME_BYTES: usize = 128;

/// Maximum UTF-8 bytes for a tool description.
pub const MAX_TOOL_DESCRIPTION_BYTES: usize = 4096;

/// Maximum serialized bytes for one tool-parameter object.
pub const MAX_TOOL_PARAMETERS_BYTES: usize = 16 * 1024;

/// Maximum UTF-8 bytes for a tool-call identifier.
pub const MAX_TOOL_CALL_ID_BYTES: usize = 128;

/// Maximum tool calls attached to one assistant message.
pub const MAX_TOOL_CALLS: usize = 32;

/// Maximum stream events retained on one [`ModelStream`].
pub const MAX_STREAM_EVENTS: usize = 4096;

/// Maximum UTF-8 bytes for one text/tool-argument delta.
pub const MAX_STREAM_DELTA_BYTES: usize = 4096;

/// Maximum unknown usage fields retained.
pub const MAX_UNKNOWN_USAGE_FIELDS: usize = 16;

/// Maximum UTF-8 bytes for an unknown usage key.
pub const MAX_UNKNOWN_USAGE_KEY_BYTES: usize = 64;

/// Maximum UTF-8 bytes for an unknown string usage value.
pub const MAX_UNKNOWN_USAGE_VALUE_BYTES: usize = 256;

const CANCEL_CHECK_EVERY: usize = 16;

const CAPABILITY_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "tools",
    "streaming",
    "vision",
    "caching",
    "reasoning",
    "structured_output",
    "context_limit",
    "max_output",
    "usage_fields",
];

const DESCRIPTOR_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "provider",
    "model",
    "capabilities",
    "context_limit",
    "max_output",
    "prices",
    "regions",
    "data_policy_tags",
    "latency_class",
    "catalog_revision",
];

const REQUEST_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "request_id",
    "model",
    "purpose",
    "messages",
    "tools",
    "max_output_tokens",
    "catalog_revision",
    "trace",
];

const USAGE_WIRE_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "input_tokens",
    "cached_input_tokens",
    "uncached_input_tokens",
    "output_tokens",
    "reasoning_tokens",
    "tool_tokens",
    "cost",
];

/// Cooperative cancellation for provider construction and invoke.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Typed provider/model failure. Display never echoes provider bodies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderError {
    Cancelled,
    AuthFailed,
    RateLimited { retry_after_ms: Option<u64> },
    ContextTooLarge,
    InvalidRequest,
    Transient,
    Permanent,
    BoundExceeded,
    UnknownVariant,
}

/// Bounded provider identifier (`openai`, `anthropic`).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ProviderId(String);

/// Bounded model identifier (`gpt-4.1`, `claude-3-5-sonnet`).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ModelId(String);

/// Provider + model pin. Comparison uses the parsed form only.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ModelRef {
    provider: ProviderId,
    model: ModelId,
}

/// Bounded request correlation identifier.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ModelRequestId(String);

/// Immutable catalog snapshot generation. Zero is rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CatalogRevision(u64);

/// Bounded region token (`us`, `eu`, `us-east-1`).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Region(String);

/// Bounded data-policy tag (`no-training`, `eu-only`).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DataPolicyTag(String);

/// Named price table. Missing prices stay unknown; they are never guessed.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct PriceTableVersion(String);

/// Bounded tool name.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ToolName(String);

/// Bounded tool-call identifier.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ToolCallId(String);

/// Task class carried on a canonical request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum ModelPurpose {
    Chat,
    Code,
    Plan,
    Review,
    Compact,
    Embed,
    ComputerUse,
}

/// Privacy / residency class used by hard route filters.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum PrivacyClass {
    Unrestricted,
    NoTraining,
    Regional,
    LocalOnly,
}

/// Advertised latency bucket. Not a measured SLO.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum LatencyClass {
    Interactive,
    Standard,
    Batch,
}

/// Whether the provider exposes reasoning tokens / a reasoning path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ReasoningSupport {
    None,
    Exposed,
}

/// Why a completed stream stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    Cancelled,
}

/// Canonical cost. Absent provider cost is [`UsageCost::Unknown`], never zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum UsageCost {
    Unknown,
    Reported { usd_micros: u64 },
}

/// Scalar extra usage value retained from a provider payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageExtValue {
    Null,
    Bool(bool),
    U64(u64),
    I64(i64),
    Text(String),
}

/// Usage counters a provider is known to report.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct UsageFieldSet {
    input: bool,
    cached_input: bool,
    uncached_input: bool,
    output: bool,
    reasoning: bool,
    tool: bool,
    cost: bool,
}

/// Required capability subset used by hard route filters.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ModelCapabilities {
    tools: bool,
    streaming: bool,
    vision: bool,
    caching: bool,
    reasoning: bool,
    structured_output: bool,
}

/// Advertised provider/model capabilities for one catalog revision.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ProviderCapabilities {
    tools: bool,
    streaming: bool,
    vision: bool,
    caching: bool,
    reasoning: ReasoningSupport,
    structured_output: bool,
    context_limit: u32,
    max_output: u32,
    usage_fields: UsageFieldSet,
}

/// Versioned price row. `None` amounts mean unknown, not free.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ModelPrices {
    input_usd_micros_per_million: Option<u64>,
    output_usd_micros_per_million: Option<u64>,
    cached_input_usd_micros_per_million: Option<u64>,
    table_version: Option<PriceTableVersion>,
}

/// Immutable model metadata bound to one [`CatalogRevision`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelDescriptor {
    provider: ProviderId,
    model: ModelId,
    capabilities: ProviderCapabilities,
    context_limit: u32,
    max_output: u32,
    prices: ModelPrices,
    regions: Vec<Region>,
    data_policy_tags: Vec<DataPolicyTag>,
    latency_class: LatencyClass,
    catalog_revision: CatalogRevision,
}

/// Canonical usage. Extra provider keys are retained beside known fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    uncached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    tool_tokens: Option<u64>,
    cost: UsageCost,
    extra: BTreeMap<String, UsageExtValue>,
}

/// One canonical message part. Images are artifact refs, never inline bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentPart {
    Text { text: String },
    Image { artifact: ArtifactRef },
}

/// Assistant-emitted tool call recorded on a message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCall {
    call_id: ToolCallId,
    name: ToolName,
    arguments: String,
}

/// Canonical chat/tool message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalMessage {
    role: MessageRole,
    parts: Vec<ContentPart>,
    tool_call_id: Option<ToolCallId>,
    tool_calls: Vec<ToolCall>,
}

/// Closed message role.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Model-visible tool schema. Parameter JSON is a bounded object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalToolSpec {
    name: ToolName,
    description: String,
    parameters: Value,
}

/// Transport-neutral request sent to a [`ProviderAdapter`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalModelRequest {
    request_id: ModelRequestId,
    model: ModelRef,
    purpose: ModelPurpose,
    messages: Vec<CanonicalMessage>,
    tools: Vec<CanonicalToolSpec>,
    max_output_tokens: Option<u32>,
    catalog_revision: CatalogRevision,
    trace: TraceContext,
    reasoning_effort: Option<crate::phase::ReasoningEffort>,
}

/// Normalized stream event. Large payloads become artifacts upstream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelStreamEvent {
    TextDelta {
        text: String,
    },
    ToolCallStart {
        call_id: ToolCallId,
        name: ToolName,
    },
    ToolCallArgumentsDelta {
        call_id: ToolCallId,
        arguments_delta: String,
    },
    Usage(NormalizedUsage),
    Completed {
        finish: FinishReason,
        usage: NormalizedUsage,
    },
    Failed {
        error: ProviderError,
    },
}

/// Bounded collected stream returned by [`ProviderAdapter::invoke`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelStream {
    request_id: ModelRequestId,
    model: ModelRef,
    events: Vec<ModelStreamEvent>,
}

/// Common provider adapter surface. Adapters perform transport conversion only.
pub trait ProviderAdapter: Send + Sync {
    fn capabilities(&self) -> ProviderCapabilities;

    fn invoke(
        &self,
        req: CanonicalModelRequest,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send;
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

    pub fn check(&self) -> Result<(), ProviderError> {
        if self.is_cancelled() {
            Err(ProviderError::Cancelled)
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

impl ProviderError {
    /// Public error code when this failure has a wire mapping.
    ///
    /// [`ProviderError::Cancelled`] has no public code.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled | Self::InvalidRequest | Self::BoundExceeded | Self::UnknownVariant => {
                None
            }
            Self::AuthFailed => Some(ErrorCode::ProviderAuthFailed),
            Self::RateLimited { .. } => Some(ErrorCode::ProviderRateLimited),
            Self::ContextTooLarge => Some(ErrorCode::ProviderContextTooLarge),
            Self::Transient | Self::Permanent => Some(ErrorCode::InternalUnexpected),
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Transient)
    }

    /// Convert to the public envelope. Cancellation is not an API error.
    pub fn into_api_error(&self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = match self {
            Self::Cancelled => return None,
            Self::AuthFailed => "Provider authentication failed",
            Self::RateLimited { .. } => "Provider rate limited",
            Self::ContextTooLarge => "Provider context window exceeded",
            Self::InvalidRequest => return None,
            Self::Transient | Self::Permanent | Self::BoundExceeded | Self::UnknownVariant => {
                UNKNOWN_INTERNAL_MESSAGE
            }
        };
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, self)),
        )
    }

    fn as_kind_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::AuthFailed => "auth_failed",
            Self::RateLimited { .. } => "rate_limited",
            Self::ContextTooLarge => "context_too_large",
            Self::InvalidRequest => "invalid_request",
            Self::Transient => "transient",
            Self::Permanent => "permanent",
            Self::BoundExceeded => "bound_exceeded",
            Self::UnknownVariant => "unknown_variant",
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "provider operation cancelled",
            Self::AuthFailed => "provider authentication failed",
            Self::RateLimited { .. } => "provider rate limited",
            Self::ContextTooLarge => "provider context window exceeded",
            Self::InvalidRequest => "provider request is invalid",
            Self::Transient => "provider reported a transient failure",
            Self::Permanent => "provider reported a permanent failure",
            Self::BoundExceeded => "provider object exceeds a documented bound",
            Self::UnknownVariant => "unknown provider schema variant",
        })
    }
}

impl Error for ProviderError {}

impl ProviderId {
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, ProviderError> {
        parse_token(raw.as_ref(), MAX_PROVIDER_ID_BYTES, TokenAlphabet::Ident).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ModelId {
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, ProviderError> {
        parse_token(raw.as_ref(), MAX_MODEL_ID_BYTES, TokenAlphabet::Model).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ModelRef {
    pub fn new(provider: ProviderId, model: ModelId) -> Self {
        Self { provider, model }
    }

    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }

    pub fn model(&self) -> &ModelId {
        &self.model
    }
}

impl ModelRequestId {
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, ProviderError> {
        let raw = raw.as_ref();
        if raw.is_empty() {
            return Err(ProviderError::InvalidRequest);
        }
        if raw.len() > MAX_REQUEST_ID_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl CatalogRevision {
    pub fn new(value: u64) -> Result<Self, ProviderError> {
        if value == 0 {
            Err(ProviderError::InvalidRequest)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Region {
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, ProviderError> {
        parse_token(raw.as_ref(), MAX_REGION_BYTES, TokenAlphabet::Ident).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl DataPolicyTag {
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, ProviderError> {
        parse_token(
            raw.as_ref(),
            MAX_DATA_POLICY_TAG_BYTES,
            TokenAlphabet::Ident,
        )
        .map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl PriceTableVersion {
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, ProviderError> {
        parse_token(
            raw.as_ref(),
            MAX_PRICE_TABLE_VERSION_BYTES,
            TokenAlphabet::Ident,
        )
        .map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ToolName {
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, ProviderError> {
        parse_token(raw.as_ref(), MAX_TOOL_NAME_BYTES, TokenAlphabet::Model).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ToolCallId {
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, ProviderError> {
        let raw = raw.as_ref();
        if raw.is_empty() {
            return Err(ProviderError::InvalidRequest);
        }
        if raw.len() > MAX_TOOL_CALL_ID_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ModelPurpose {
    pub const ALL: &'static [Self] = &[
        Self::Chat,
        Self::Code,
        Self::Plan,
        Self::Review,
        Self::Compact,
        Self::Embed,
        Self::ComputerUse,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Code => "code",
            Self::Plan => "plan",
            Self::Review => "review",
            Self::Compact => "compact",
            Self::Embed => "embed",
            Self::ComputerUse => "computer_use",
        }
    }
}

impl PrivacyClass {
    pub const ALL: &'static [Self] = &[
        Self::Unrestricted,
        Self::NoTraining,
        Self::Regional,
        Self::LocalOnly,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unrestricted => "unrestricted",
            Self::NoTraining => "no_training",
            Self::Regional => "regional",
            Self::LocalOnly => "local_only",
        }
    }
}

impl LatencyClass {
    pub const ALL: &'static [Self] = &[Self::Interactive, Self::Standard, Self::Batch];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Standard => "standard",
            Self::Batch => "batch",
        }
    }
}

impl ReasoningSupport {
    pub const ALL: &'static [Self] = &[Self::None, Self::Exposed];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Exposed => "exposed",
        }
    }
}

impl FinishReason {
    pub const ALL: &'static [Self] = &[Self::Stop, Self::Length, Self::ToolCalls, Self::Cancelled];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::Cancelled => "cancelled",
        }
    }
}

impl MessageRole {
    pub const ALL: &'static [Self] = &[Self::System, Self::User, Self::Assistant, Self::Tool];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

impl UsageFieldSet {
    pub const NONE: Self = Self {
        input: false,
        cached_input: false,
        uncached_input: false,
        output: false,
        reasoning: false,
        tool: false,
        cost: false,
    };

    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        input: bool,
        cached_input: bool,
        uncached_input: bool,
        output: bool,
        reasoning: bool,
        tool: bool,
        cost: bool,
    ) -> Self {
        Self {
            input,
            cached_input,
            uncached_input,
            output,
            reasoning,
            tool,
            cost,
        }
    }

    pub const fn input(self) -> bool {
        self.input
    }
    pub const fn cached_input(self) -> bool {
        self.cached_input
    }
    pub const fn uncached_input(self) -> bool {
        self.uncached_input
    }
    pub const fn output(self) -> bool {
        self.output
    }
    pub const fn reasoning(self) -> bool {
        self.reasoning
    }
    pub const fn tool(self) -> bool {
        self.tool
    }
    pub const fn cost(self) -> bool {
        self.cost
    }
}

impl ModelCapabilities {
    pub const NONE: Self = Self {
        tools: false,
        streaming: false,
        vision: false,
        caching: false,
        reasoning: false,
        structured_output: false,
    };

    pub const fn new(
        tools: bool,
        streaming: bool,
        vision: bool,
        caching: bool,
        reasoning: bool,
        structured_output: bool,
    ) -> Self {
        Self {
            tools,
            streaming,
            vision,
            caching,
            reasoning,
            structured_output,
        }
    }

    pub const fn tools(self) -> bool {
        self.tools
    }
    pub const fn streaming(self) -> bool {
        self.streaming
    }
    pub const fn vision(self) -> bool {
        self.vision
    }
    pub const fn caching(self) -> bool {
        self.caching
    }
    pub const fn reasoning(self) -> bool {
        self.reasoning
    }
    pub const fn structured_output(self) -> bool {
        self.structured_output
    }
}

impl ProviderCapabilities {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tools: bool,
        streaming: bool,
        vision: bool,
        caching: bool,
        reasoning: ReasoningSupport,
        structured_output: bool,
        context_limit: u32,
        max_output: u32,
        usage_fields: UsageFieldSet,
    ) -> Result<Self, ProviderError> {
        if context_limit == 0 || max_output == 0 {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self {
            tools,
            streaming,
            vision,
            caching,
            reasoning,
            structured_output,
            context_limit,
            max_output,
            usage_fields,
        })
    }

    pub const fn tools(&self) -> bool {
        self.tools
    }
    pub const fn streaming(&self) -> bool {
        self.streaming
    }
    pub const fn vision(&self) -> bool {
        self.vision
    }
    pub const fn caching(&self) -> bool {
        self.caching
    }
    pub const fn reasoning(&self) -> ReasoningSupport {
        self.reasoning
    }
    pub const fn structured_output(&self) -> bool {
        self.structured_output
    }
    pub const fn context_limit(&self) -> u32 {
        self.context_limit
    }
    pub const fn max_output(&self) -> u32 {
        self.max_output
    }
    pub const fn usage_fields(&self) -> UsageFieldSet {
        self.usage_fields
    }

    /// Hard capability check. Context size is compared separately.
    pub fn satisfies(&self, required: &ModelCapabilities) -> bool {
        (!required.tools || self.tools)
            && (!required.streaming || self.streaming)
            && (!required.vision || self.vision)
            && (!required.caching || self.caching)
            && (!required.reasoning || self.reasoning == ReasoningSupport::Exposed)
            && (!required.structured_output || self.structured_output)
    }
}

impl ModelPrices {
    pub const UNKNOWN: Self = Self {
        input_usd_micros_per_million: None,
        output_usd_micros_per_million: None,
        cached_input_usd_micros_per_million: None,
        table_version: None,
    };

    pub fn new(
        input_usd_micros_per_million: Option<u64>,
        output_usd_micros_per_million: Option<u64>,
        cached_input_usd_micros_per_million: Option<u64>,
        table_version: Option<PriceTableVersion>,
    ) -> Self {
        Self {
            input_usd_micros_per_million,
            output_usd_micros_per_million,
            cached_input_usd_micros_per_million,
            table_version,
        }
    }

    pub const fn input_usd_micros_per_million(&self) -> Option<u64> {
        self.input_usd_micros_per_million
    }
    pub const fn output_usd_micros_per_million(&self) -> Option<u64> {
        self.output_usd_micros_per_million
    }
    pub const fn cached_input_usd_micros_per_million(&self) -> Option<u64> {
        self.cached_input_usd_micros_per_million
    }
    pub fn table_version(&self) -> Option<&PriceTableVersion> {
        self.table_version.as_ref()
    }
}

impl ModelDescriptor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: ProviderId,
        model: ModelId,
        capabilities: ProviderCapabilities,
        prices: ModelPrices,
        regions: Vec<Region>,
        data_policy_tags: Vec<DataPolicyTag>,
        latency_class: LatencyClass,
        catalog_revision: CatalogRevision,
        cancel: &CancellationToken,
    ) -> Result<Self, ProviderError> {
        cancel.check()?;
        if regions.len() > MAX_REGIONS || data_policy_tags.len() > MAX_DATA_POLICY_TAGS {
            return Err(ProviderError::BoundExceeded);
        }
        for (i, _) in regions.iter().enumerate() {
            if i.is_multiple_of(CANCEL_CHECK_EVERY) {
                cancel.check()?;
            }
        }
        // Descriptor context/output must match the advertised capability snapshot.
        Ok(Self {
            context_limit: capabilities.context_limit,
            max_output: capabilities.max_output,
            provider,
            model,
            capabilities,
            prices,
            regions,
            data_policy_tags,
            latency_class,
            catalog_revision,
        })
    }

    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }
    pub fn model(&self) -> &ModelId {
        &self.model
    }
    pub fn model_ref(&self) -> ModelRef {
        ModelRef::new(self.provider.clone(), self.model.clone())
    }
    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
    pub const fn context_limit(&self) -> u32 {
        self.context_limit
    }
    pub const fn max_output(&self) -> u32 {
        self.max_output
    }
    pub fn prices(&self) -> &ModelPrices {
        &self.prices
    }
    pub fn regions(&self) -> &[Region] {
        &self.regions
    }
    pub fn data_policy_tags(&self) -> &[DataPolicyTag] {
        &self.data_policy_tags
    }
    pub const fn latency_class(&self) -> LatencyClass {
        self.latency_class
    }
    pub const fn catalog_revision(&self) -> CatalogRevision {
        self.catalog_revision
    }
}

impl NormalizedUsage {
    pub fn new(
        input_tokens: Option<u64>,
        cached_input_tokens: Option<u64>,
        uncached_input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
        tool_tokens: Option<u64>,
        cost: UsageCost,
    ) -> Self {
        Self {
            input_tokens,
            cached_input_tokens,
            uncached_input_tokens,
            output_tokens,
            reasoning_tokens,
            tool_tokens,
            cost,
            extra: BTreeMap::new(),
        }
    }

    /// Decode a provider usage object, keeping unknown scalar fields.
    pub fn from_provider_value(value: &Value) -> Result<Self, ProviderError> {
        let Value::Object(map) = value else {
            return Err(ProviderError::InvalidRequest);
        };
        let mut usage = Self::new(
            optional_u64(map.get("input_tokens"))?,
            optional_u64(map.get("cached_input_tokens"))?,
            optional_u64(map.get("uncached_input_tokens"))?,
            optional_u64(map.get("output_tokens"))?,
            optional_u64(map.get("reasoning_tokens"))?,
            optional_u64(map.get("tool_tokens"))?,
            parse_cost_value(map.get("cost"))?,
        );
        for (key, raw) in map {
            if is_reserved_usage_key(key) {
                continue;
            }
            usage.insert_extra(key, usage_ext_from_value(raw)?)?;
        }
        Ok(usage)
    }

    pub fn insert_extra(
        &mut self,
        key: impl Into<String>,
        value: UsageExtValue,
    ) -> Result<(), ProviderError> {
        let key = key.into();
        validate_usage_extra_key(&key)?;
        if let UsageExtValue::Text(text) = &value
            && text.len() > MAX_UNKNOWN_USAGE_VALUE_BYTES
        {
            return Err(ProviderError::BoundExceeded);
        }
        if !self.extra.contains_key(&key) && self.extra.len() >= MAX_UNKNOWN_USAGE_FIELDS {
            return Err(ProviderError::BoundExceeded);
        }
        self.extra.insert(key, value);
        Ok(())
    }

    pub const fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }
    pub const fn cached_input_tokens(&self) -> Option<u64> {
        self.cached_input_tokens
    }
    pub const fn uncached_input_tokens(&self) -> Option<u64> {
        self.uncached_input_tokens
    }
    pub const fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }
    pub const fn reasoning_tokens(&self) -> Option<u64> {
        self.reasoning_tokens
    }
    pub const fn tool_tokens(&self) -> Option<u64> {
        self.tool_tokens
    }
    pub const fn cost(&self) -> UsageCost {
        self.cost
    }
    pub fn extra(&self, key: &str) -> Option<&UsageExtValue> {
        self.extra.get(key)
    }
    pub fn extra_fields(&self) -> impl Iterator<Item = (&str, &UsageExtValue)> {
        self.extra.iter().map(|(k, v)| (k.as_str(), v))
    }
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Result<Self, ProviderError> {
        let text = text.into();
        if text.len() > MAX_TEXT_PART_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        Ok(Self::Text { text })
    }

    pub fn image(artifact: ArtifactRef) -> Self {
        Self::Image { artifact }
    }
}

impl ToolCall {
    pub fn new(
        call_id: ToolCallId,
        name: ToolName,
        arguments: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let arguments = arguments.into();
        if arguments.len() > MAX_TOOL_PARAMETERS_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        Ok(Self {
            call_id,
            name,
            arguments,
        })
    }

    pub fn call_id(&self) -> &ToolCallId {
        &self.call_id
    }
    pub fn name(&self) -> &ToolName {
        &self.name
    }
    pub fn arguments(&self) -> &str {
        &self.arguments
    }
}

impl CanonicalMessage {
    pub fn new(
        role: MessageRole,
        parts: Vec<ContentPart>,
        tool_call_id: Option<ToolCallId>,
        tool_calls: Vec<ToolCall>,
    ) -> Result<Self, ProviderError> {
        if parts.len() > MAX_CONTENT_PARTS || tool_calls.len() > MAX_TOOL_CALLS {
            return Err(ProviderError::BoundExceeded);
        }
        if role != MessageRole::Tool && tool_call_id.is_some() {
            return Err(ProviderError::InvalidRequest);
        }
        if role == MessageRole::Tool && tool_call_id.is_none() {
            return Err(ProviderError::InvalidRequest);
        }
        if role != MessageRole::Assistant && !tool_calls.is_empty() {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self {
            role,
            parts,
            tool_call_id,
            tool_calls,
        })
    }

    pub const fn role(&self) -> MessageRole {
        self.role
    }
    pub fn parts(&self) -> &[ContentPart] {
        &self.parts
    }
    pub fn tool_call_id(&self) -> Option<&ToolCallId> {
        self.tool_call_id.as_ref()
    }
    pub fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }
}

impl CanonicalToolSpec {
    pub fn new(
        name: ToolName,
        description: impl Into<String>,
        parameters: Value,
    ) -> Result<Self, ProviderError> {
        let description = description.into();
        if description.len() > MAX_TOOL_DESCRIPTION_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        if !parameters.is_object() {
            return Err(ProviderError::InvalidRequest);
        }
        let encoded = serde_json::to_vec(&parameters).map_err(|_| ProviderError::InvalidRequest)?;
        if encoded.len() > MAX_TOOL_PARAMETERS_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        Ok(Self {
            name,
            description,
            parameters,
        })
    }

    pub fn name(&self) -> &ToolName {
        &self.name
    }
    pub fn description(&self) -> &str {
        &self.description
    }
    pub fn parameters(&self) -> &Value {
        &self.parameters
    }
}

impl CanonicalModelRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request_id: ModelRequestId,
        model: ModelRef,
        purpose: ModelPurpose,
        messages: Vec<CanonicalMessage>,
        tools: Vec<CanonicalToolSpec>,
        max_output_tokens: Option<u32>,
        catalog_revision: CatalogRevision,
        trace: TraceContext,
        cancel: &CancellationToken,
    ) -> Result<Self, ProviderError> {
        cancel.check()?;
        if messages.len() > MAX_MESSAGES || tools.len() > MAX_TOOLS {
            return Err(ProviderError::BoundExceeded);
        }
        for (i, _) in messages.iter().enumerate() {
            if i.is_multiple_of(CANCEL_CHECK_EVERY) {
                cancel.check()?;
            }
        }
        if let Some(max_output) = max_output_tokens
            && max_output == 0
        {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self {
            request_id,
            model,
            purpose,
            messages,
            tools,
            max_output_tokens,
            catalog_revision,
            trace,
            reasoning_effort: None,
        })
    }

    pub fn request_id(&self) -> &ModelRequestId {
        &self.request_id
    }
    pub fn model(&self) -> &ModelRef {
        &self.model
    }
    pub const fn purpose(&self) -> ModelPurpose {
        self.purpose
    }
    pub fn messages(&self) -> &[CanonicalMessage] {
        &self.messages
    }
    pub fn tools(&self) -> &[CanonicalToolSpec] {
        &self.tools
    }
    pub const fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }
    pub const fn catalog_revision(&self) -> CatalogRevision {
        self.catalog_revision
    }
    pub fn trace(&self) -> &TraceContext {
        &self.trace
    }
    /// Reasoning-effort request override; `None` means the provider default.
    pub const fn reasoning_effort(&self) -> Option<crate::phase::ReasoningEffort> {
        self.reasoning_effort
    }
    /// Builder-style effort override for requests from phases that want more
    /// or less deliberation than the provider default.
    pub fn with_reasoning_effort(
        mut self,
        effort: crate::phase::ReasoningEffort,
    ) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }
}

impl ModelStream {
    pub fn from_events(
        request_id: ModelRequestId,
        model: ModelRef,
        events: Vec<ModelStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<Self, ProviderError> {
        cancel.check()?;
        if events.len() > MAX_STREAM_EVENTS {
            return Err(ProviderError::BoundExceeded);
        }
        for (i, event) in events.iter().enumerate() {
            if i.is_multiple_of(CANCEL_CHECK_EVERY) {
                cancel.check()?;
            }
            match event {
                ModelStreamEvent::TextDelta { text }
                | ModelStreamEvent::ToolCallArgumentsDelta {
                    arguments_delta: text,
                    ..
                } if text.len() > MAX_STREAM_DELTA_BYTES => {
                    return Err(ProviderError::BoundExceeded);
                }
                _ => {}
            }
        }
        Ok(Self {
            request_id,
            model,
            events,
        })
    }

    pub fn request_id(&self) -> &ModelRequestId {
        &self.request_id
    }
    pub fn model(&self) -> &ModelRef {
        &self.model
    }
    pub fn events(&self) -> &[ModelStreamEvent] {
        &self.events
    }

    pub fn terminal_usage(&self) -> Option<&NormalizedUsage> {
        self.events.iter().rev().find_map(|event| match event {
            ModelStreamEvent::Completed { usage, .. } => Some(usage),
            _ => None,
        })
    }
}

impl fmt::Display for ModelPurpose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl fmt::Display for PrivacyClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl fmt::Display for LatencyClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl fmt::Display for ReasoningSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl fmt::Display for FinishReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl fmt::Display for MessageRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl fmt::Display for CatalogRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ModelPurpose {
    type Err = ProviderError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}
impl FromStr for PrivacyClass {
    type Err = ProviderError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}
impl FromStr for LatencyClass {
    type Err = ProviderError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}
impl FromStr for ReasoningSupport {
    type Err = ProviderError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}
impl FromStr for FinishReason {
    type Err = ProviderError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}
impl FromStr for MessageRole {
    type Err = ProviderError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

macro_rules! serde_str_enum {
    ($ty:ty) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                raw.parse().map_err(de::Error::custom)
            }
        }
    };
}

serde_str_enum!(ModelPurpose);
serde_str_enum!(PrivacyClass);
serde_str_enum!(LatencyClass);
serde_str_enum!(ReasoningSupport);
serde_str_enum!(FinishReason);
serde_str_enum!(MessageRole);

macro_rules! serde_token {
    ($ty:ty, $parse:path) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                $parse(&raw).map_err(de::Error::custom)
            }
        }
    };
}

serde_token!(ProviderId, ProviderId::parse);
serde_token!(ModelId, ModelId::parse);
serde_token!(ModelRequestId, ModelRequestId::parse);
serde_token!(Region, Region::parse);
serde_token!(DataPolicyTag, DataPolicyTag::parse);
serde_token!(PriceTableVersion, PriceTableVersion::parse);
serde_token!(ToolName, ToolName::parse);
serde_token!(ToolCallId, ToolCallId::parse);

impl Serialize for CatalogRevision {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for CatalogRevision {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u64::deserialize(deserializer)?;
        CatalogRevision::new(value).map_err(de::Error::custom)
    }
}

impl Serialize for ModelRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ModelRef", 2)?;
        state.serialize_field("provider", &self.provider)?;
        state.serialize_field("model", &self.model)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ModelRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            provider: ProviderId,
            model: ModelId,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self::new(raw.provider, raw.model))
    }
}

impl Serialize for UsageFieldSet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("UsageFieldSet", 7)?;
        state.serialize_field("input", &self.input)?;
        state.serialize_field("cached_input", &self.cached_input)?;
        state.serialize_field("uncached_input", &self.uncached_input)?;
        state.serialize_field("output", &self.output)?;
        state.serialize_field("reasoning", &self.reasoning)?;
        state.serialize_field("tool", &self.tool)?;
        state.serialize_field("cost", &self.cost)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for UsageFieldSet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            input: bool,
            cached_input: bool,
            uncached_input: bool,
            output: bool,
            reasoning: bool,
            tool: bool,
            cost: bool,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self::new(
            raw.input,
            raw.cached_input,
            raw.uncached_input,
            raw.output,
            raw.reasoning,
            raw.tool,
            raw.cost,
        ))
    }
}

impl Serialize for ProviderCapabilities {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state =
            serializer.serialize_struct("ProviderCapabilities", CAPABILITY_FIELDS.len())?;
        state.serialize_field("schema", PROVIDER_CAPABILITIES_SCHEMA)?;
        state.serialize_field("schema_version", &PROVIDER_SCHEMA_VERSION)?;
        state.serialize_field("tools", &self.tools)?;
        state.serialize_field("streaming", &self.streaming)?;
        state.serialize_field("vision", &self.vision)?;
        state.serialize_field("caching", &self.caching)?;
        state.serialize_field("reasoning", &self.reasoning)?;
        state.serialize_field("structured_output", &self.structured_output)?;
        state.serialize_field("context_limit", &self.context_limit)?;
        state.serialize_field("max_output", &self.max_output)?;
        state.serialize_field("usage_fields", &self.usage_fields)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ProviderCapabilities {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            tools: bool,
            streaming: bool,
            vision: bool,
            caching: bool,
            reasoning: ReasoningSupport,
            structured_output: bool,
            context_limit: u32,
            max_output: u32,
            usage_fields: UsageFieldSet,
        }
        let raw = Raw::deserialize(deserializer)?;
        check_schema(
            &raw.schema,
            raw.schema_version,
            PROVIDER_CAPABILITIES_SCHEMA,
        )
        .map_err(de::Error::custom)?;
        ProviderCapabilities::new(
            raw.tools,
            raw.streaming,
            raw.vision,
            raw.caching,
            raw.reasoning,
            raw.structured_output,
            raw.context_limit,
            raw.max_output,
            raw.usage_fields,
        )
        .map_err(de::Error::custom)
    }
}

impl Serialize for ModelPrices {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ModelPrices", 4)?;
        state.serialize_field(
            "input_usd_micros_per_million",
            &self.input_usd_micros_per_million,
        )?;
        state.serialize_field(
            "output_usd_micros_per_million",
            &self.output_usd_micros_per_million,
        )?;
        state.serialize_field(
            "cached_input_usd_micros_per_million",
            &self.cached_input_usd_micros_per_million,
        )?;
        state.serialize_field("table_version", &self.table_version)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ModelPrices {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            input_usd_micros_per_million: Option<u64>,
            output_usd_micros_per_million: Option<u64>,
            cached_input_usd_micros_per_million: Option<u64>,
            table_version: Option<PriceTableVersion>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self::new(
            raw.input_usd_micros_per_million,
            raw.output_usd_micros_per_million,
            raw.cached_input_usd_micros_per_million,
            raw.table_version,
        ))
    }
}

impl Serialize for ModelDescriptor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ModelDescriptor", DESCRIPTOR_FIELDS.len())?;
        state.serialize_field("schema", MODEL_DESCRIPTOR_SCHEMA)?;
        state.serialize_field("schema_version", &PROVIDER_SCHEMA_VERSION)?;
        state.serialize_field("provider", &self.provider)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("capabilities", &self.capabilities)?;
        state.serialize_field("context_limit", &self.context_limit)?;
        state.serialize_field("max_output", &self.max_output)?;
        state.serialize_field("prices", &self.prices)?;
        state.serialize_field("regions", &self.regions)?;
        state.serialize_field("data_policy_tags", &self.data_policy_tags)?;
        state.serialize_field("latency_class", &self.latency_class)?;
        state.serialize_field("catalog_revision", &self.catalog_revision)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ModelDescriptor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            provider: ProviderId,
            model: ModelId,
            capabilities: ProviderCapabilities,
            context_limit: u32,
            max_output: u32,
            prices: ModelPrices,
            regions: Vec<Region>,
            data_policy_tags: Vec<DataPolicyTag>,
            latency_class: LatencyClass,
            catalog_revision: CatalogRevision,
        }
        let raw = Raw::deserialize(deserializer)?;
        check_schema(&raw.schema, raw.schema_version, MODEL_DESCRIPTOR_SCHEMA)
            .map_err(de::Error::custom)?;
        if raw.context_limit != raw.capabilities.context_limit
            || raw.max_output != raw.capabilities.max_output
        {
            return Err(de::Error::custom(
                "descriptor context/output must match capabilities",
            ));
        }
        ModelDescriptor::new(
            raw.provider,
            raw.model,
            raw.capabilities,
            raw.prices,
            raw.regions,
            raw.data_policy_tags,
            raw.latency_class,
            raw.catalog_revision,
            &CancellationToken::new(),
        )
        .map_err(de::Error::custom)
    }
}

impl Serialize for UsageCost {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unknown => {
                let mut state = serializer.serialize_struct("UsageCost", 1)?;
                state.serialize_field("kind", "unknown")?;
                state.end()
            }
            Self::Reported { usd_micros } => {
                let mut state = serializer.serialize_struct("UsageCost", 2)?;
                state.serialize_field("kind", "reported")?;
                state.serialize_field("usd_micros", usd_micros)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for UsageCost {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            kind: String,
            usd_micros: Option<u64>,
        }
        let raw = Raw::deserialize(deserializer)?;
        match raw.kind.as_str() {
            "unknown" => {
                if raw.usd_micros.is_some() {
                    return Err(de::Error::custom("unknown cost cannot include usd_micros"));
                }
                Ok(Self::Unknown)
            }
            "reported" => {
                let usd_micros = raw
                    .usd_micros
                    .ok_or_else(|| de::Error::missing_field("usd_micros"))?;
                Ok(Self::Reported { usd_micros })
            }
            _ => Err(de::Error::custom("unknown usage cost kind")),
        }
    }
}

impl Serialize for NormalizedUsage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(USAGE_WIRE_FIELDS.len() + self.extra.len()))?;
        map.serialize_entry("schema", NORMALIZED_USAGE_SCHEMA)?;
        map.serialize_entry("schema_version", &PROVIDER_SCHEMA_VERSION)?;
        map.serialize_entry("input_tokens", &self.input_tokens)?;
        map.serialize_entry("cached_input_tokens", &self.cached_input_tokens)?;
        map.serialize_entry("uncached_input_tokens", &self.uncached_input_tokens)?;
        map.serialize_entry("output_tokens", &self.output_tokens)?;
        map.serialize_entry("reasoning_tokens", &self.reasoning_tokens)?;
        map.serialize_entry("tool_tokens", &self.tool_tokens)?;
        map.serialize_entry("cost", &self.cost)?;
        for (key, value) in &self.extra {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for NormalizedUsage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(NormalizedUsageVisitor)
    }
}

struct NormalizedUsageVisitor;

impl<'de> Visitor<'de> for NormalizedUsageVisitor {
    type Value = NormalizedUsage;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a normalized usage object")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut schema = None;
        let mut schema_version = None;
        let mut input_tokens = None;
        let mut cached_input_tokens = None;
        let mut uncached_input_tokens = None;
        let mut output_tokens = None;
        let mut reasoning_tokens = None;
        let mut tool_tokens = None;
        let mut cost = None;
        let mut extra = BTreeMap::new();

        while let Some(key) = access.next_key::<String>()? {
            match key.as_str() {
                "schema" => assign_once(&mut schema, access.next_value()?, "schema")?,
                "schema_version" => {
                    assign_once(&mut schema_version, access.next_value()?, "schema_version")?;
                }
                "input_tokens" => {
                    assign_once(&mut input_tokens, access.next_value()?, "input_tokens")?;
                }
                "cached_input_tokens" => assign_once(
                    &mut cached_input_tokens,
                    access.next_value()?,
                    "cached_input_tokens",
                )?,
                "uncached_input_tokens" => assign_once(
                    &mut uncached_input_tokens,
                    access.next_value()?,
                    "uncached_input_tokens",
                )?,
                "output_tokens" => {
                    assign_once(&mut output_tokens, access.next_value()?, "output_tokens")?;
                }
                "reasoning_tokens" => assign_once(
                    &mut reasoning_tokens,
                    access.next_value()?,
                    "reasoning_tokens",
                )?,
                "tool_tokens" => {
                    assign_once(&mut tool_tokens, access.next_value()?, "tool_tokens")?
                }
                "cost" => assign_once(&mut cost, access.next_value()?, "cost")?,
                other => {
                    let value: UsageExtValue = access.next_value()?;
                    if extra.contains_key(other) {
                        return Err(de::Error::duplicate_field("unknown usage field"));
                    }
                    if extra.len() >= MAX_UNKNOWN_USAGE_FIELDS {
                        return Err(de::Error::custom("unknown usage field bound exceeded"));
                    }
                    validate_usage_extra_key(other).map_err(de::Error::custom)?;
                    if let UsageExtValue::Text(text) = &value
                        && text.len() > MAX_UNKNOWN_USAGE_VALUE_BYTES
                    {
                        return Err(de::Error::custom("unknown usage value exceeds bound"));
                    }
                    extra.insert(other.to_owned(), value);
                }
            }
        }

        let schema: String = schema.ok_or_else(|| de::Error::missing_field("schema"))?;
        let schema_version: u16 =
            schema_version.ok_or_else(|| de::Error::missing_field("schema_version"))?;
        check_schema(&schema, schema_version, NORMALIZED_USAGE_SCHEMA)
            .map_err(de::Error::custom)?;
        let cost = cost.unwrap_or(UsageCost::Unknown);
        Ok(NormalizedUsage {
            input_tokens: input_tokens.flatten(),
            cached_input_tokens: cached_input_tokens.flatten(),
            uncached_input_tokens: uncached_input_tokens.flatten(),
            output_tokens: output_tokens.flatten(),
            reasoning_tokens: reasoning_tokens.flatten(),
            tool_tokens: tool_tokens.flatten(),
            cost,
            extra,
        })
    }
}

impl Serialize for UsageExtValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_none(),
            Self::Bool(v) => serializer.serialize_bool(*v),
            Self::U64(v) => serializer.serialize_u64(*v),
            Self::I64(v) => serializer.serialize_i64(*v),
            Self::Text(v) => serializer.serialize_str(v),
        }
    }
}

impl<'de> Deserialize<'de> for UsageExtValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ExtVisitor;
        impl Visitor<'_> for ExtVisitor {
            type Value = UsageExtValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a scalar usage extension")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UsageExtValue::Null)
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UsageExtValue::Null)
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(UsageExtValue::Bool(v))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(UsageExtValue::U64(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v >= 0 {
                    Ok(UsageExtValue::U64(v as u64))
                } else {
                    Ok(UsageExtValue::I64(v))
                }
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v.len() > MAX_UNKNOWN_USAGE_VALUE_BYTES {
                    return Err(E::custom("unknown usage value exceeds bound"));
                }
                Ok(UsageExtValue::Text(v.to_owned()))
            }
        }
        deserializer.deserialize_any(ExtVisitor)
    }
}

impl Serialize for ContentPart {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text { text } => {
                let mut state = serializer.serialize_struct("ContentPart", 2)?;
                state.serialize_field("kind", "text")?;
                state.serialize_field("text", text)?;
                state.end()
            }
            Self::Image { artifact } => {
                let mut state = serializer.serialize_struct("ContentPart", 2)?;
                state.serialize_field("kind", "image")?;
                state.serialize_field("artifact", artifact)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ContentPart {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            kind: String,
            text: Option<String>,
            artifact: Option<ArtifactRef>,
        }
        let raw = Raw::deserialize(deserializer)?;
        match raw.kind.as_str() {
            "text" => {
                let text = raw.text.ok_or_else(|| de::Error::missing_field("text"))?;
                ContentPart::text(text).map_err(de::Error::custom)
            }
            "image" => {
                let artifact = raw
                    .artifact
                    .ok_or_else(|| de::Error::missing_field("artifact"))?;
                Ok(ContentPart::image(artifact))
            }
            _ => Err(de::Error::custom("unknown content part kind")),
        }
    }
}

impl Serialize for ToolCall {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ToolCall", 3)?;
        state.serialize_field("call_id", &self.call_id)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("arguments", &self.arguments)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ToolCall {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            call_id: ToolCallId,
            name: ToolName,
            arguments: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        ToolCall::new(raw.call_id, raw.name, raw.arguments).map_err(de::Error::custom)
    }
}

impl Serialize for CanonicalMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CanonicalMessage", 4)?;
        state.serialize_field("role", &self.role)?;
        state.serialize_field("parts", &self.parts)?;
        state.serialize_field("tool_call_id", &self.tool_call_id)?;
        state.serialize_field("tool_calls", &self.tool_calls)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CanonicalMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            role: MessageRole,
            parts: Vec<ContentPart>,
            tool_call_id: Option<ToolCallId>,
            tool_calls: Vec<ToolCall>,
        }
        let raw = Raw::deserialize(deserializer)?;
        CanonicalMessage::new(raw.role, raw.parts, raw.tool_call_id, raw.tool_calls)
            .map_err(de::Error::custom)
    }
}

impl Serialize for CanonicalToolSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CanonicalToolSpec", 3)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("description", &self.description)?;
        state.serialize_field("parameters", &self.parameters)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CanonicalToolSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            name: ToolName,
            description: String,
            parameters: Value,
        }
        let raw = Raw::deserialize(deserializer)?;
        CanonicalToolSpec::new(raw.name, raw.description, raw.parameters).map_err(de::Error::custom)
    }
}

impl Serialize for CanonicalModelRequest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state =
            serializer.serialize_struct("CanonicalModelRequest", REQUEST_FIELDS.len())?;
        state.serialize_field("schema", CANONICAL_MODEL_REQUEST_SCHEMA)?;
        state.serialize_field("schema_version", &PROVIDER_SCHEMA_VERSION)?;
        state.serialize_field("request_id", &self.request_id)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("purpose", &self.purpose)?;
        state.serialize_field("messages", &self.messages)?;
        state.serialize_field("tools", &self.tools)?;
        state.serialize_field("max_output_tokens", &self.max_output_tokens)?;
        state.serialize_field("catalog_revision", &self.catalog_revision)?;
        state.serialize_field("trace", &self.trace)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CanonicalModelRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            request_id: ModelRequestId,
            model: ModelRef,
            purpose: ModelPurpose,
            messages: Vec<CanonicalMessage>,
            tools: Vec<CanonicalToolSpec>,
            max_output_tokens: Option<u32>,
            catalog_revision: CatalogRevision,
            trace: TraceContext,
        }
        let raw = Raw::deserialize(deserializer)?;
        check_schema(
            &raw.schema,
            raw.schema_version,
            CANONICAL_MODEL_REQUEST_SCHEMA,
        )
        .map_err(de::Error::custom)?;
        CanonicalModelRequest::new(
            raw.request_id,
            raw.model,
            raw.purpose,
            raw.messages,
            raw.tools,
            raw.max_output_tokens,
            raw.catalog_revision,
            raw.trace,
            &CancellationToken::new(),
        )
        .map_err(de::Error::custom)
    }
}

impl Serialize for ModelStreamEvent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::TextDelta { text } => {
                let mut state = serializer.serialize_struct("ModelStreamEvent", 3)?;
                state.serialize_field("schema", MODEL_STREAM_EVENT_SCHEMA)?;
                state.serialize_field("schema_version", &PROVIDER_SCHEMA_VERSION)?;
                state.serialize_field("kind", "text_delta")?;
                state.serialize_field("text", text)?;
                state.end()
            }
            Self::ToolCallStart { call_id, name } => {
                let mut state = serializer.serialize_struct("ModelStreamEvent", 5)?;
                state.serialize_field("schema", MODEL_STREAM_EVENT_SCHEMA)?;
                state.serialize_field("schema_version", &PROVIDER_SCHEMA_VERSION)?;
                state.serialize_field("kind", "tool_call_start")?;
                state.serialize_field("call_id", call_id)?;
                state.serialize_field("name", name)?;
                state.end()
            }
            Self::ToolCallArgumentsDelta {
                call_id,
                arguments_delta,
            } => {
                let mut state = serializer.serialize_struct("ModelStreamEvent", 5)?;
                state.serialize_field("schema", MODEL_STREAM_EVENT_SCHEMA)?;
                state.serialize_field("schema_version", &PROVIDER_SCHEMA_VERSION)?;
                state.serialize_field("kind", "tool_call_arguments_delta")?;
                state.serialize_field("call_id", call_id)?;
                state.serialize_field("arguments_delta", arguments_delta)?;
                state.end()
            }
            Self::Usage(usage) => {
                let mut state = serializer.serialize_struct("ModelStreamEvent", 4)?;
                state.serialize_field("schema", MODEL_STREAM_EVENT_SCHEMA)?;
                state.serialize_field("schema_version", &PROVIDER_SCHEMA_VERSION)?;
                state.serialize_field("kind", "usage")?;
                state.serialize_field("usage", usage)?;
                state.end()
            }
            Self::Completed { finish, usage } => {
                let mut state = serializer.serialize_struct("ModelStreamEvent", 5)?;
                state.serialize_field("schema", MODEL_STREAM_EVENT_SCHEMA)?;
                state.serialize_field("schema_version", &PROVIDER_SCHEMA_VERSION)?;
                state.serialize_field("kind", "completed")?;
                state.serialize_field("finish", finish)?;
                state.serialize_field("usage", usage)?;
                state.end()
            }
            Self::Failed { error } => {
                let mut state = serializer.serialize_struct("ModelStreamEvent", 4)?;
                state.serialize_field("schema", MODEL_STREAM_EVENT_SCHEMA)?;
                state.serialize_field("schema_version", &PROVIDER_SCHEMA_VERSION)?;
                state.serialize_field("kind", "failed")?;
                state.serialize_field("error", error.as_kind_str())?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ModelStreamEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            kind: String,
            text: Option<String>,
            call_id: Option<ToolCallId>,
            name: Option<ToolName>,
            arguments_delta: Option<String>,
            usage: Option<NormalizedUsage>,
            finish: Option<FinishReason>,
            error: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        check_schema(&raw.schema, raw.schema_version, MODEL_STREAM_EVENT_SCHEMA)
            .map_err(de::Error::custom)?;
        match raw.kind.as_str() {
            "text_delta" => {
                let text = raw.text.ok_or_else(|| de::Error::missing_field("text"))?;
                if text.len() > MAX_STREAM_DELTA_BYTES {
                    return Err(de::Error::custom("text delta exceeds bound"));
                }
                Ok(Self::TextDelta { text })
            }
            "tool_call_start" => Ok(Self::ToolCallStart {
                call_id: raw
                    .call_id
                    .ok_or_else(|| de::Error::missing_field("call_id"))?,
                name: raw.name.ok_or_else(|| de::Error::missing_field("name"))?,
            }),
            "tool_call_arguments_delta" => {
                let arguments_delta = raw
                    .arguments_delta
                    .ok_or_else(|| de::Error::missing_field("arguments_delta"))?;
                if arguments_delta.len() > MAX_STREAM_DELTA_BYTES {
                    return Err(de::Error::custom("arguments delta exceeds bound"));
                }
                Ok(Self::ToolCallArgumentsDelta {
                    call_id: raw
                        .call_id
                        .ok_or_else(|| de::Error::missing_field("call_id"))?,
                    arguments_delta,
                })
            }
            "usage" => Ok(Self::Usage(
                raw.usage.ok_or_else(|| de::Error::missing_field("usage"))?,
            )),
            "completed" => Ok(Self::Completed {
                finish: raw
                    .finish
                    .ok_or_else(|| de::Error::missing_field("finish"))?,
                usage: raw.usage.ok_or_else(|| de::Error::missing_field("usage"))?,
            }),
            "failed" => {
                let kind = raw.error.ok_or_else(|| de::Error::missing_field("error"))?;
                Ok(Self::Failed {
                    error: parse_error_kind(&kind).map_err(de::Error::custom)?,
                })
            }
            _ => Err(de::Error::custom("unknown model stream event kind")),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TokenAlphabet {
    Ident,
    Model,
}

fn parse_token(
    raw: &str,
    max_bytes: usize,
    alphabet: TokenAlphabet,
) -> Result<String, ProviderError> {
    if raw.is_empty() {
        return Err(ProviderError::InvalidRequest);
    }
    if raw.len() > max_bytes {
        return Err(ProviderError::BoundExceeded);
    }
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        return Err(ProviderError::InvalidRequest);
    };
    let first_ok = match alphabet {
        TokenAlphabet::Ident => first.is_ascii_lowercase(),
        TokenAlphabet::Model => first.is_ascii_alphanumeric(),
    };
    if !first_ok {
        return Err(ProviderError::InvalidRequest);
    }
    let mut prev_sep = false;
    for ch in chars {
        let sep = match alphabet {
            TokenAlphabet::Ident => ch == '-',
            TokenAlphabet::Model => matches!(ch, '-' | '_' | '.'),
        };
        let ok = match alphabet {
            TokenAlphabet::Ident => ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-',
            TokenAlphabet::Model => ch.is_ascii_alphanumeric() || sep,
        };
        if !ok || (sep && prev_sep) {
            return Err(ProviderError::InvalidRequest);
        }
        prev_sep = sep;
    }
    if raw.ends_with('-') || raw.ends_with('.') || raw.ends_with('_') {
        return Err(ProviderError::InvalidRequest);
    }
    Ok(raw.to_owned())
}

fn parse_closed<T: Copy>(
    raw: &str,
    all: &[T],
    as_str: fn(T) -> &'static str,
) -> Result<T, ProviderError> {
    for item in all {
        if as_str(*item) == raw {
            return Ok(*item);
        }
    }
    Err(ProviderError::UnknownVariant)
}

fn check_schema(schema: &str, version: u16, expected: &str) -> Result<(), ProviderError> {
    if schema != expected || version != PROVIDER_SCHEMA_VERSION {
        Err(ProviderError::UnknownVariant)
    } else {
        Ok(())
    }
}

fn assign_once<T, E: de::Error>(
    slot: &mut Option<T>,
    value: T,
    field: &'static str,
) -> Result<(), E> {
    if slot.is_some() {
        Err(E::duplicate_field(field))
    } else {
        *slot = Some(value);
        Ok(())
    }
}

fn is_reserved_usage_key(key: &str) -> bool {
    USAGE_WIRE_FIELDS.contains(&key)
}

fn validate_usage_extra_key(key: &str) -> Result<(), ProviderError> {
    if key.is_empty() || is_reserved_usage_key(key) {
        return Err(ProviderError::InvalidRequest);
    }
    if key.len() > MAX_UNKNOWN_USAGE_KEY_BYTES {
        return Err(ProviderError::BoundExceeded);
    }
    Ok(())
}

fn optional_u64(value: Option<&Value>) -> Result<Option<u64>, ProviderError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_u64().ok_or(ProviderError::InvalidRequest).map(Some),
        Some(_) => Err(ProviderError::InvalidRequest),
    }
}

fn parse_cost_value(value: Option<&Value>) -> Result<UsageCost, ProviderError> {
    match value {
        None | Some(Value::Null) => Ok(UsageCost::Unknown),
        Some(Value::Object(map)) => {
            let kind = map
                .get("kind")
                .and_then(Value::as_str)
                .ok_or(ProviderError::InvalidRequest)?;
            match kind {
                "unknown" => Ok(UsageCost::Unknown),
                "reported" => {
                    let usd_micros = map
                        .get("usd_micros")
                        .and_then(Value::as_u64)
                        .ok_or(ProviderError::InvalidRequest)?;
                    Ok(UsageCost::Reported { usd_micros })
                }
                _ => Err(ProviderError::UnknownVariant),
            }
        }
        Some(_) => Err(ProviderError::InvalidRequest),
    }
}

fn usage_ext_from_value(value: &Value) -> Result<UsageExtValue, ProviderError> {
    match value {
        Value::Null => Ok(UsageExtValue::Null),
        Value::Bool(v) => Ok(UsageExtValue::Bool(*v)),
        Value::Number(n) => {
            if let Some(v) = n.as_u64() {
                Ok(UsageExtValue::U64(v))
            } else if let Some(v) = n.as_i64() {
                Ok(UsageExtValue::I64(v))
            } else {
                Err(ProviderError::InvalidRequest)
            }
        }
        Value::String(v) => {
            if v.len() > MAX_UNKNOWN_USAGE_VALUE_BYTES {
                return Err(ProviderError::BoundExceeded);
            }
            Ok(UsageExtValue::Text(v.clone()))
        }
        Value::Array(_) | Value::Object(_) => Err(ProviderError::InvalidRequest),
    }
}

fn parse_error_kind(kind: &str) -> Result<ProviderError, ProviderError> {
    match kind {
        "cancelled" => Ok(ProviderError::Cancelled),
        "auth_failed" => Ok(ProviderError::AuthFailed),
        "rate_limited" => Ok(ProviderError::RateLimited {
            retry_after_ms: None,
        }),
        "context_too_large" => Ok(ProviderError::ContextTooLarge),
        "invalid_request" => Ok(ProviderError::InvalidRequest),
        "transient" => Ok(ProviderError::Transient),
        "permanent" => Ok(ProviderError::Permanent),
        "bound_exceeded" => Ok(ProviderError::BoundExceeded),
        "unknown_variant" => Ok(ProviderError::UnknownVariant),
        _ => Err(ProviderError::UnknownVariant),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{ArtifactId, RedactionClass, TraceId};

    const GOLDEN_TRACE: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const GOLDEN_REQUEST_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";

    const GOLDEN_CAPABILITIES: &str = r#"{"schema":"rapidlm.provider_capabilities","schema_version":1,"tools":true,"streaming":true,"vision":false,"caching":true,"reasoning":"exposed","structured_output":true,"context_limit":128000,"max_output":8192,"usage_fields":{"input":true,"cached_input":true,"uncached_input":true,"output":true,"reasoning":true,"tool":false,"cost":false}}"#;

    const GOLDEN_DESCRIPTOR: &str = r#"{"schema":"rapidlm.model_descriptor","schema_version":1,"provider":"openai","model":"gpt-4.1","capabilities":{"schema":"rapidlm.provider_capabilities","schema_version":1,"tools":true,"streaming":true,"vision":false,"caching":true,"reasoning":"exposed","structured_output":true,"context_limit":128000,"max_output":8192,"usage_fields":{"input":true,"cached_input":true,"uncached_input":true,"output":true,"reasoning":true,"tool":false,"cost":false}},"context_limit":128000,"max_output":8192,"prices":{"input_usd_micros_per_million":2000000,"output_usd_micros_per_million":8000000,"cached_input_usd_micros_per_million":500000,"table_version":"openai-2026-04"},"regions":["us","eu"],"data_policy_tags":["no-training"],"latency_class":"interactive","catalog_revision":7}"#;

    const GOLDEN_USAGE: &str = r#"{"schema":"rapidlm.normalized_usage","schema_version":1,"input_tokens":12,"cached_input_tokens":4,"uncached_input_tokens":8,"output_tokens":3,"reasoning_tokens":null,"tool_tokens":null,"cost":{"kind":"unknown"},"audio_tokens":7}"#;

    const GOLDEN_REQUEST: &str = r#"{"schema":"rapidlm.canonical_model_request","schema_version":1,"request_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","model":{"provider":"openai","model":"gpt-4.1"},"purpose":"code","messages":[{"role":"user","parts":[{"kind":"text","text":"hello"}],"tool_call_id":null,"tool_calls":[]}],"tools":[],"max_output_tokens":256,"catalog_revision":7,"trace":{"trace_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","parent_span_id":null,"baggage":{}}}"#;

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn caps() -> ProviderCapabilities {
        ProviderCapabilities::new(
            true,
            true,
            false,
            true,
            ReasoningSupport::Exposed,
            true,
            128_000,
            8192,
            UsageFieldSet::new(true, true, true, true, true, false, false),
        )
        .expect("caps")
    }

    fn descriptor(revision: u64) -> ModelDescriptor {
        ModelDescriptor::new(
            ProviderId::parse("openai").expect("provider"),
            ModelId::parse("gpt-4.1").expect("model"),
            caps(),
            ModelPrices::new(
                Some(2_000_000),
                Some(8_000_000),
                Some(500_000),
                Some(PriceTableVersion::parse("openai-2026-04").expect("price table")),
            ),
            vec![
                Region::parse("us").expect("us"),
                Region::parse("eu").expect("eu"),
            ],
            vec![DataPolicyTag::parse("no-training").expect("tag")],
            LatencyClass::Interactive,
            CatalogRevision::new(revision).expect("revision"),
            &live(),
        )
        .expect("descriptor")
    }

    fn golden_trace() -> TraceContext {
        TraceContext::new(
            GOLDEN_TRACE.parse::<TraceId>().expect("trace"),
            None,
            protocol::Baggage::empty(),
        )
    }

    fn request() -> CanonicalModelRequest {
        CanonicalModelRequest::new(
            ModelRequestId::parse(GOLDEN_REQUEST_ID).expect("id"),
            ModelRef::new(
                ProviderId::parse("openai").expect("provider"),
                ModelId::parse("gpt-4.1").expect("model"),
            ),
            ModelPurpose::Code,
            vec![
                CanonicalMessage::new(
                    MessageRole::User,
                    vec![ContentPart::text("hello").expect("text")],
                    None,
                    vec![],
                )
                .expect("message"),
            ],
            vec![],
            Some(256),
            CatalogRevision::new(7).expect("revision"),
            golden_trace(),
            &live(),
        )
        .expect("request")
    }

    #[test]
    fn capabilities_golden_round_trips() {
        let json = serde_json::to_string(&caps()).expect("serialize");
        assert_eq!(json, GOLDEN_CAPABILITIES);
        let decoded: ProviderCapabilities =
            serde_json::from_str(GOLDEN_CAPABILITIES).expect("decode");
        assert_eq!(decoded, caps());
    }

    #[test]
    fn descriptor_golden_round_trips() {
        let json = serde_json::to_string(&descriptor(7)).expect("serialize");
        assert_eq!(json, GOLDEN_DESCRIPTOR);
        let decoded: ModelDescriptor = serde_json::from_str(GOLDEN_DESCRIPTOR).expect("decode");
        assert_eq!(decoded, descriptor(7));
    }

    #[test]
    fn request_golden_round_trips() {
        let json = serde_json::to_string(&request()).expect("serialize");
        assert_eq!(json, GOLDEN_REQUEST);
        let decoded: CanonicalModelRequest = serde_json::from_str(GOLDEN_REQUEST).expect("decode");
        assert_eq!(decoded, request());
    }

    #[test]
    fn usage_golden_retains_unknown_fields() {
        let mut usage = NormalizedUsage::new(
            Some(12),
            Some(4),
            Some(8),
            Some(3),
            None,
            None,
            UsageCost::Unknown,
        );
        usage
            .insert_extra("audio_tokens", UsageExtValue::U64(7))
            .expect("extra");
        let json = serde_json::to_string(&usage).expect("serialize");
        assert_eq!(json, GOLDEN_USAGE);
        let decoded: NormalizedUsage = serde_json::from_str(GOLDEN_USAGE).expect("decode");
        assert_eq!(decoded.input_tokens(), Some(12));
        assert_eq!(decoded.cost(), UsageCost::Unknown);
        assert_eq!(decoded.extra("audio_tokens"), Some(&UsageExtValue::U64(7)));
        assert_eq!(decoded, usage);
    }

    #[test]
    fn unknown_provider_usage_does_not_break_canonical_fields() {
        let raw = serde_json::json!({
            "input_tokens": 20,
            "output_tokens": 5,
            "audio_tokens": 9,
            "prompt_audio_ms": 15,
            "nested_ignored_is_invalid": null
        });
        let usage = NormalizedUsage::from_provider_value(&raw).expect("usage");
        assert_eq!(usage.input_tokens(), Some(20));
        assert_eq!(usage.output_tokens(), Some(5));
        assert_eq!(usage.reasoning_tokens(), None);
        assert_eq!(usage.cost(), UsageCost::Unknown);
        assert_ne!(usage.cost(), UsageCost::Reported { usd_micros: 0 });
        assert_eq!(usage.extra("audio_tokens"), Some(&UsageExtValue::U64(9)));
        assert_eq!(
            usage.extra("prompt_audio_ms"),
            Some(&UsageExtValue::U64(15))
        );
    }

    #[test]
    fn missing_usage_cost_is_unknown_not_zero() {
        let usage = NormalizedUsage::from_provider_value(&serde_json::json!({})).expect("empty");
        assert_eq!(usage.input_tokens(), None);
        assert_eq!(usage.cost(), UsageCost::Unknown);
        assert_ne!(usage.cost(), UsageCost::Reported { usd_micros: 0 });
    }

    #[test]
    fn metadata_is_immutable_for_one_catalog_revision() {
        let first = descriptor(7);
        let clone = first.clone();
        assert_eq!(first, clone);
        assert_eq!(
            first.catalog_revision(),
            CatalogRevision::new(7).expect("rev")
        );
        assert_eq!(first.capabilities(), clone.capabilities());
        let next = descriptor(8);
        assert_ne!(first.catalog_revision(), next.catalog_revision());
        assert_eq!(first.model_ref(), next.model_ref());
        assert_eq!(first.capabilities(), next.capabilities());
    }

    #[test]
    fn unknown_capability_fields_fail_closed() {
        let json = r#"{"schema":"rapidlm.provider_capabilities","schema_version":1,"tools":true,"streaming":true,"vision":false,"caching":true,"reasoning":"exposed","structured_output":true,"context_limit":128000,"max_output":8192,"usage_fields":{"input":true,"cached_input":true,"uncached_input":true,"output":true,"reasoning":true,"tool":false,"cost":false},"secret_grant":true}"#;
        assert!(serde_json::from_str::<ProviderCapabilities>(json).is_err());
    }

    #[test]
    fn required_capabilities_are_hard_filters() {
        let advertised = caps();
        assert!(advertised.satisfies(&ModelCapabilities::new(true, true, false, true, true, true)));
        assert!(!advertised.satisfies(&ModelCapabilities::new(true, true, true, true, true, true)));
        let no_reason = ProviderCapabilities::new(
            true,
            true,
            false,
            true,
            ReasoningSupport::None,
            true,
            8_000,
            1024,
            UsageFieldSet::NONE,
        )
        .expect("caps");
        assert!(!no_reason.satisfies(&ModelCapabilities::new(
            false, false, false, false, true, false
        )));
    }

    #[test]
    fn provider_errors_map_to_protocol_codes() {
        let trace = GOLDEN_TRACE.parse::<TraceId>().expect("trace");
        let auth = ProviderError::AuthFailed
            .into_api_error(trace)
            .expect("auth");
        assert_eq!(auth.code(), ErrorCode::ProviderAuthFailed);
        assert!(!auth.retryable());
        let limited = ProviderError::RateLimited {
            retry_after_ms: Some(250),
        }
        .into_api_error(trace)
        .expect("rate");
        assert_eq!(limited.code(), ErrorCode::ProviderRateLimited);
        assert!(limited.retryable());
        assert!(ProviderError::Cancelled.into_api_error(trace).is_none());
        assert!(
            ProviderError::RateLimited {
                retry_after_ms: None
            }
            .is_retryable()
        );
        assert!(!ProviderError::AuthFailed.is_retryable());
    }

    #[test]
    fn cancellation_is_honored() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            ModelDescriptor::new(
                ProviderId::parse("openai").expect("p"),
                ModelId::parse("gpt-4.1").expect("m"),
                caps(),
                ModelPrices::UNKNOWN,
                vec![],
                vec![],
                LatencyClass::Standard,
                CatalogRevision::new(1).expect("rev"),
                &cancel,
            ),
            Err(ProviderError::Cancelled)
        );
    }

    #[test]
    fn stream_events_are_bounded_and_carry_usage() {
        let usage = NormalizedUsage::new(
            Some(1),
            None,
            Some(1),
            Some(1),
            None,
            None,
            UsageCost::Unknown,
        );
        let stream = ModelStream::from_events(
            ModelRequestId::parse("req-1").expect("id"),
            ModelRef::new(
                ProviderId::parse("openai").expect("p"),
                ModelId::parse("gpt-4.1").expect("m"),
            ),
            vec![
                ModelStreamEvent::TextDelta {
                    text: "hi".to_owned(),
                },
                ModelStreamEvent::Completed {
                    finish: FinishReason::Stop,
                    usage: usage.clone(),
                },
            ],
            &live(),
        )
        .expect("stream");
        assert_eq!(stream.terminal_usage(), Some(&usage));
        let too_big = "x".repeat(MAX_STREAM_DELTA_BYTES + 1);
        assert_eq!(
            ModelStream::from_events(
                ModelRequestId::parse("req-2").expect("id"),
                stream.model().clone(),
                vec![ModelStreamEvent::TextDelta { text: too_big }],
                &live(),
            ),
            Err(ProviderError::BoundExceeded)
        );
    }

    #[test]
    fn image_parts_are_artifact_refs() {
        let artifact = ArtifactRef::new(
            ArtifactId::from_bytes(b"vision-fixture"),
            "image/png",
            16,
            RedactionClass::Project,
        );
        let part = ContentPart::image(artifact.clone());
        let json = serde_json::to_value(&part).expect("json");
        assert_eq!(json["kind"], "image");
        assert!(json.get("bytes").is_none());
        let decoded: ContentPart = serde_json::from_value(json).expect("decode");
        assert_eq!(decoded, part);
    }

    #[test]
    fn zero_catalog_revision_is_rejected() {
        assert_eq!(CatalogRevision::new(0), Err(ProviderError::InvalidRequest));
    }
}
