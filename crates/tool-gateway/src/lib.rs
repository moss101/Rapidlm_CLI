#![forbid(unsafe_code)]

pub mod dispatch;
pub mod repair;
pub mod schema;
pub mod validate;

pub use dispatch::{
    ArtifactSink, Authorization, CapabilityBroker, CapabilityDescriptor, CapabilityNeed,
    DispatchActor, DispatchError, MAX_EXCERPT_BYTES, MAX_INLINE_RESULT_BYTES,
    MAX_RESULT_ARTIFACT_BYTES, MAX_SUMMARY_BYTES, MemoryArtifactSink, RawToolOutput,
    TOOL_RESULT_SCHEMA, ToolContinuation, ToolDispatcher, ToolExecutor, ToolRegistry,
    ToolResultEnvelope, ToolResultStatus, describe_capability, dispatch, is_privileged_tool,
};
pub use repair::{
    ConfidenceClass, InvariantVerdict, MAX_FEEDBACK_BYTES, MAX_REPAIR_PASSES, MAX_REPAIR_RULES,
    RepairError, RepairFeedback, RepairOutcome, RepairRecord, RepairRule, check_invariant, repair,
};
pub use schema::{
    DENIED_ARGUMENT_NAMES, GatewayTool, SchemaError, TOOL_CATALOG_SCHEMA,
    TOOL_CATALOG_SCHEMA_VERSION, ToolCatalog, ToolSchema, V1_CATALOG_HASH, V1_TOOL_COUNT,
    denied_argument_name,
};
pub use validate::{
    CancellationToken, CanonicalToolInvocation, MAX_ARGUMENT_BYTES, MAX_CALL_ID_BYTES,
    MAX_ENVELOPE_BYTES, MAX_ISSUES, MAX_JSON_DEPTH, MAX_JSON_NODES, MAX_PATCH_TOTAL_BYTES,
    RepairIssue, TOOL_INVOCATION_SCHEMA, ToolCall, ValidateError, validate, validate_bytes,
    validate_issues,
};
