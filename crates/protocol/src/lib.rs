#![forbid(unsafe_code)]

pub mod artifact;
pub mod config;
pub mod error;
pub mod host_env;
pub mod host_path;
pub mod id;
pub mod remote_worker;
pub mod repo_path;
pub mod trace;

pub use artifact::{
    ARTIFACT_DIGEST_LEN, ARTIFACT_ID_HEX_LEN, ARTIFACT_ID_PREFIX, ARTIFACT_REF_SCHEMA, ArtifactId,
    ArtifactIdParseError, ArtifactRef, RedactionClass, RedactionClassParseError,
};
pub use config::{
    AgentPoolConfig, AgentsConfig, BackgroundAgentRole, CONFIG_SCHEMA, ClipboardMode,
    ComputerUseConfig, ComputerUseDesktopConfig, ComputerUseRecording, ComputerUseVisionConfig,
    ConfigDocument, ConfigError, ContextConfig, CoordinateFallback, DEFAULT_MAX_INDEX_BYTES,
    DaemonConfig, DesktopIsolation, EmbeddingsMode, HandoffConfig, KnowledgeConfig,
    KnowledgeSuggestions, ManagedAgentsConfig, ModelPolicyName, ModelPolicyNameParseError,
    ModelsConfig, NetworkMode, OrchestrationConfig, OrchestrationMode, OrchestrationModeParseError,
    PlaybooksConfig, PublicRepoEventTriggers, RapidConfig, RecordingRedaction, SandboxConfig,
    SandboxTier, TelemetryConfig, TelemetryContent, TelemetryMode, TrajectoryCollection,
    TrajectoryConfig,
};
pub use error::{
    API_ERROR_SCHEMA, ApiError, ApiErrorBuildError, ErrorCode, ErrorCodeParseError, ErrorDetails,
    ErrorDetailsError, MAX_DETAIL_FIELDS, MAX_DETAIL_KEY_BYTES, MAX_DETAIL_VALUE_BYTES,
    MAX_MESSAGE_BYTES, RapidErrorClass, UNKNOWN_INTERNAL_MESSAGE,
};
pub use id::{
    AgentId, ContextItemId, ControlLeaseId, EventId, EvidenceId, GoalId, GraphId, HandoffId, Id,
    IdParseError, JobId, KnowledgeId, LeaseId, MessageId, NodeId, ProjectId, RepoId, RuntimeId,
    SessionId, TraceId, TrajectoryId, TurnId, WorkspaceViewId,
};
pub use remote_worker::{
    CapabilityClass, CapabilityClassParseError, CapabilityLimits, ComputerUseSurface,
    ComputerUseSurfaceParseError, ControllerId, DeviceCapability, DeviceCapabilityParseError,
    GpuAdvertisement, LeaseNonce, LeaseSignature, MAX_ADVERTISED_FLAGS, MAX_ARTIFACT_REFS,
    MAX_ATTESTATIONS, MAX_CAPABILITY_CLASSES, MAX_COMPUTER_USE_SURFACES, MAX_CPU_MILLIS,
    MAX_DATA_REGIONS, MAX_HANDOFF_PROTOCOL_VERSIONS, MAX_LABEL_BYTES, MAX_MEMORY_MB,
    MAX_OUTPUT_BYTES, MAX_PIDS, MAX_TIMEOUT_MS, MAX_WORKER_LABELS, ProtocolVersion,
    REMOTE_WORKER_PROTOCOL_MAJOR, REMOTE_WORKER_PROTOCOL_MINOR, REMOTE_WORKER_SCHEMA,
    RemoteWorkerError, ResourceUsage, WorkCancel, WorkCancelReason, WorkCancelReasonParseError,
    WorkLease, WorkLeaseId, WorkPhase, WorkPhaseParseError, WorkProgress, WorkResult, WorkStatus,
    WorkStatusParseError, WorkerArch, WorkerArchParseError, WorkerAttestation,
    WorkerAttestationClass, WorkerAttestationClassParseError, WorkerCapabilities,
    WorkerCapabilitiesMessage, WorkerHealth, WorkerHealthParseError, WorkerId, WorkerLabel,
    WorkerMessage, WorkerMessageKind, WorkerMessageKindParseError, WorkerPlatform,
    WorkerPlatformParseError, WorkerRegistration, WorkerSandboxSpec,
};
pub use repo_path::{MAX_REPO_PATH_BYTES, RepoPath, RepoPathError};
pub use trace::{
    Baggage, BaggageError, MAX_BAGGAGE_FIELDS, MAX_BAGGAGE_KEY_BYTES, MAX_BAGGAGE_VALUE_BYTES,
    SpanId, TRACE_CONTEXT_SCHEMA, TraceContext,
};
