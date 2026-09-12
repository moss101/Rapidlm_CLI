#![forbid(unsafe_code)]

pub mod config {
    pub mod loader;
    pub mod security_merge;
}

pub mod project {
    pub mod trust;
}

pub mod service;
pub mod service_graph;

pub mod session {
    pub mod fork;
    pub mod projection;
    pub mod service;
}

pub mod turn {
    pub mod guard;
}

pub mod cancel;

pub mod client;
/// Unix-socket IPC only. The module carried half-finished `cfg(not(unix))`
/// placeholders that never compiled — the client's frame I/O needs `Read` +
/// `Write` on a stream type the placeholder did not provide, and the server
/// called `cfg(unix)`-only functions from un-gated code — and no CI job had
/// run on Windows to say so. Nothing in the binary consumes this module yet
/// (`rapid daemon`/`rapid acp` are roadmap), so on other platforms it is
/// absent rather than pretend-present.
#[cfg(unix)]
pub mod ipc;
pub mod recovery;

pub use cancel::{
    CancelError, CancelLifetime, CancelOwner, CancelToken, CancellationTree, MAX_CANCEL_CHILDREN,
    MAX_CANCEL_DEPTH,
};
pub use client::{
    ApprovalDecision, EventStream, EventStreamError, FinishTurn, InProcessKernelClient, Interrupt,
    InterruptReason, KernelClient, ResolveApproval, RewindResult, RewindSession, SubmitTurn,
    SubscribeEvents, TurnHandle, TurnOutcome,
};
pub use config::loader::{
    CancellationToken, ConfigLoadError, ConfigLoadResult, ConfigOrigin, ConfigOverride,
    ConfigSources, ConfigText, ENV_PREFIX, MAX_CONFIG_DOCUMENT_BYTES, MAX_OVERRIDE_ENTRIES,
    MAX_OVERRIDE_KEY_BYTES, MAX_OVERRIDE_VALUE_BYTES, UnknownConfigKey, config_key_from_env_name,
    load_config,
};
pub use config::security_merge::{
    EffectiveSecurityConfig, FsPolicy, MAX_FS_ROOTS, MAX_NETWORK_HOST_BYTES, MAX_NETWORK_HOSTS,
    MAX_PLUGIN_ID_BYTES, MAX_PLUGIN_IDS, NetworkHost, NetworkPolicy, PluginId, PluginPolicy,
    SecurityField, SecurityMergeError, SecurityScope, merge_security,
};
pub use project::trust::{
    CanonicalRoot, DeviceHint, IdentityFingerprint, MAX_CANONICAL_ROOT_BYTES, MAX_CATALOG_BYTES,
    MAX_FINGERPRINT_BYTES, MAX_TRUST_RECORDS, PROJECT_TRUST_SCHEMA, ProjectIdentity,
    ProjectTrustError, ProjectTrustStore, TrustStatus,
};
pub use recovery::{
    ClassifyError, InflightModel, InflightProjection, InflightTool, MAX_INFLIGHT_ID_BYTES,
    MAX_INFLIGHT_MODELS, MAX_INFLIGHT_TOOLS, RecoveryAction, RecoveryActions, RecoveryDisposition,
    RecoveryError, RecoveryManager, classify_inflight, park_recovered_goal, project_inflight,
    recover_session,
};
pub use service::{
    HEALTH_SNAPSHOT_SCHEMA, HealthSnapshot, HealthState, LifecycleService, MAX_DEPENDENCIES,
    MAX_SERVICE_ID_BYTES, MAX_SERVICES, ServiceContext, ServiceDecl, ServiceError,
    ServiceFailureKind, ServiceHealth, ServiceId, ServiceIdError, ServiceStatus,
    validate_dependency_graph,
};
pub use service_graph::{DEFAULT_ROLLBACK_QUIESCE, GraphPhase, ServiceGraph};
pub use session::fork::ForkSession;
pub use session::projection::{
    Criterion, EvidenceRequirement, GoalBudget, GoalSnapshot, GoalState, GoalStopReason, GoalUsage,
    MAX_ACTIVE_AGENTS, MAX_CRITERIA, MAX_GOAL_STATEMENT_BYTES, MAX_REPLAY_EVENTS, ProjectionError,
    ProjectionInvariant, SESSION_SNAPSHOT_SCHEMA, SessionSnapshot, SessionStatus, apply, replay,
};
pub use session::service::{CreateSession, SessionError, SessionService};
pub use turn::guard::{TurnGuardError, TurnLease, TurnSubmissionGuard};
