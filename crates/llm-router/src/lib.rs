#![forbid(unsafe_code)]

pub mod catalog;
pub mod credentials;
pub mod fallback;
pub mod phase;
pub mod provider;
pub mod providers {
    pub mod anthropic;
    pub mod openai_compatible;
}
pub mod route;
pub mod usage;

pub use catalog::{
    CATALOG_SCHEMA_VERSION, CATALOG_SNAPSHOT_SCHEMA, CatalogConfig, CatalogEntry,
    CatalogEntryStatus, CatalogError, CatalogHash, CatalogModelSpec, CatalogSnapshot,
    CatalogStatusReason, ImpossibleMetadataKind, MAX_CATALOG_ENTRIES, MAX_CATALOG_PROVIDERS,
    ModelCatalog, ProviderCapEntry, ProviderCapIndex,
};
pub use credentials::{
    CREDENTIAL_SCHEMA_VERSION, CredentialKind, CredentialResolver, EPHEMERAL_CREDENTIAL_SCHEMA,
    EphemeralCredential, MAX_PROFILE_ID_BYTES, PROVIDER_PROFILE_SCHEMA, ProfileId, ProviderProfile,
    SecretRef, SecretValue,
};
pub use fallback::{
    classify_failure, AttemptProgress, DEFAULT_BACKOFF_BASE_MS, DEFAULT_BACKOFF_CAP_MS,
    DEFAULT_SAME_MODEL_RETRIES, FALLBACK_PLAN_SCHEMA, FALLBACK_POLICY_SCHEMA,
    FALLBACK_POLICY_VERSION, FallbackAction, FallbackController, FallbackError, FallbackPlan,
    FallbackPolicy, FallbackTrigger, FailureClass, MAX_EXPLICIT_ALTERNATES, MAX_FALLBACK_MODELS,
    MAX_SAME_MODEL_RETRIES, StopReason,
};
pub use phase::{
    parse_purpose_name, purpose_name, PhaseRoute, PHASE_ROUTE_SCHEMA, ReasoningEffort,
    ReasoningEffortParseError, REASONING_EFFORT_NAMES,
};
pub use provider::{
    CANONICAL_MODEL_REQUEST_SCHEMA, CancellationToken, CanonicalMessage, CanonicalModelRequest,
    CanonicalToolSpec, CatalogRevision, ContentPart, DataPolicyTag, FinishReason, LatencyClass,
    MAX_CONTENT_PARTS, MAX_DATA_POLICY_TAG_BYTES, MAX_DATA_POLICY_TAGS, MAX_MESSAGES,
    MAX_MODEL_ID_BYTES, MAX_PRICE_TABLE_VERSION_BYTES, MAX_PROVIDER_ID_BYTES, MAX_REGION_BYTES,
    MAX_REGIONS, MAX_REQUEST_ID_BYTES, MAX_STREAM_DELTA_BYTES, MAX_STREAM_EVENTS,
    MAX_TEXT_PART_BYTES, MAX_TOOL_CALL_ID_BYTES, MAX_TOOL_CALLS, MAX_TOOL_DESCRIPTION_BYTES,
    MAX_TOOL_NAME_BYTES, MAX_TOOL_PARAMETERS_BYTES, MAX_TOOLS, MAX_UNKNOWN_USAGE_FIELDS,
    MAX_UNKNOWN_USAGE_KEY_BYTES, MAX_UNKNOWN_USAGE_VALUE_BYTES, MODEL_DESCRIPTOR_SCHEMA,
    MODEL_STREAM_EVENT_SCHEMA, MessageRole, ModelCapabilities, ModelDescriptor, ModelId,
    ModelPrices, ModelPurpose, ModelRef, ModelRequestId, ModelStream, ModelStreamEvent,
    NORMALIZED_USAGE_SCHEMA, NormalizedUsage, PROVIDER_CAPABILITIES_SCHEMA,
    PROVIDER_SCHEMA_VERSION, PriceTableVersion, PrivacyClass, ProviderAdapter,
    ProviderCapabilities, ProviderError, ProviderId, ReasoningSupport, Region, ToolCall,
    ToolCallId, ToolName, UsageCost, UsageExtValue, UsageFieldSet,
};
pub use providers::anthropic::{
    ANTHROPIC_API_VERSION, ANTHROPIC_CONFIG_SCHEMA, ANTHROPIC_MESSAGES_PATH,
    ANTHROPIC_SCHEMA_VERSION, AnthropicAdapter, AnthropicConfig, AnthropicEndpoint,
    encode_anthropic_payload,
};
pub use providers::openai_compatible::{
    DEFAULT_HTTP_TIMEOUT, Http1Transport, HttpTransport, MAX_BASE_URL_BYTES,
    MAX_HTTP_REQUEST_BYTES, MAX_HTTP_RESPONSE_BYTES, OPENAI_COMPATIBLE_CONFIG_SCHEMA,
    OPENAI_COMPATIBLE_SCHEMA_VERSION, OpenAiApiStyle, OpenAiCompatibleAdapter,
    OpenAiCompatibleConfig, OpenAiCompatibleEndpoint, ProviderHttpRequest, ProviderHttpResponse,
    StaticWireAuth, WireAuthorization, encode_provider_payload,
};
pub use route::filter::{
    EligibleSet, HardRejection, LOCAL_ONLY_TAG, LOCAL_PROVIDER_ID, MAX_ELIGIBLE_MODELS,
    NO_TRAINING_TAG, NoRouteReason, RejectionReason, RouteFilterError, RouteRequest, eligible,
    hard_constraints, is_local_provider, is_no_training_tag,
};
pub use route::score::{
    score, select, LatencyBudgetsV1, QualityPrior, ReliabilityPrior, RouteDecision,
    RoutePolicyV1, RouteScoreError, RouteWeightsV1, ScoreBreakdown, ScoreComponents,
    ScoreDefaultsV1, MAX_CANDIDATE_SCORES, MAX_QUALITY_PRIORS, MAX_RELIABILITY_PRIORS,
    ROUTE_DECISION_SCHEMA, ROUTE_POLICY_SCHEMA, ROUTE_POLICY_VERSION, SCORE_BREAKDOWN_SCHEMA,
    SCORE_SCALE,
};
pub use usage::{
    AccountedCost, MODEL_USAGE_SCHEMA, MODEL_USAGE_SCHEMA_VERSION, ModelUsage, UsageAccumulator,
    UsageError,
};
