#![forbid(unsafe_code)]

pub mod backend;
pub mod backends;

pub use backend::{
    BackendDoctorReport, BackendHealth, HealthReason, IsolationStrength, MountCapability,
    MountMode, NetworkCapability, ResourceCapability, ResourceUsage, SandboxBackend,
    SandboxCapabilities, SandboxError, SandboxExecRequest, SandboxExecResult, SandboxExit,
    SandboxExitReason, SandboxHandle, SandboxId, SandboxManager, SandboxMount, SandboxNetwork,
    SandboxSpec, isolation_rank, supports_spec,
};
pub use backends::container::{
    ContainerBackend, ContainerNetwork, ContainerPlan, ContainerRuntime,
    MAX_LIVE_CONTAINER_SANDBOXES,
};
pub use backends::gvisor::{
    GvisorBackend, GvisorFeatures, GvisorNetwork, GvisorPlan, GvisorRuntime, GvisorSupport,
    MAX_LIVE_GVISOR_SANDBOXES,
};
pub use backends::host_restricted::{
    HostNetworkHelper, HostRestrictedBackend, HostRestrictedPlan, MAX_LIVE_HOST_SANDBOXES,
};
pub use backends::remote::{
    ControllerId, MAX_LIVE_REMOTE_SANDBOXES, RemoteBackend, RemoteInput, RemoteLeaseParties,
    RemoteMount, RemoteMountSource, RemoteSandboxSpec, RemoteWorkLease, WorkLeaseId,
    WorkLeaseIssuer, WorkerAttestation, WorkerAttestationClass, WorkerId, WorkerPlatform,
    WorkerProfile,
};
pub use backends::seatbelt::{MAX_LIVE_SEATBELT_SANDBOXES, SeatbeltBackend};
