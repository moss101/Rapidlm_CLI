//! Versioned controller↔worker wire protocol.
//!
//! Messages are content-addressed and guest-relative. Host-path authority,
//! capability grants, and secret plaintext cannot be constructed or decoded.
//! Unknown or incompatible protocol majors fail negotiation (threat `T-010`).

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactId, ArtifactRef};
use crate::config::{NetworkMode, SandboxTier};
use crate::id::{IdParseError, JobId, RuntimeId, TraceId};
use crate::repo_path::{RepoPath, RepoPathError};

/// Wire schema name for every remote-worker message.
pub const REMOTE_WORKER_SCHEMA: &str = "rapidlm.remote_worker";

/// Supported protocol major. No other major is negotiable.
pub const REMOTE_WORKER_PROTOCOL_MAJOR: u16 = 1;

/// Supported protocol minor within [`REMOTE_WORKER_PROTOCOL_MAJOR`].
pub const REMOTE_WORKER_PROTOCOL_MINOR: u16 = 0;

/// Maximum advertised sandbox tiers, backends, or device capabilities.
pub const MAX_ADVERTISED_FLAGS: usize = 8;

/// Maximum Computer Use surfaces on one capabilities message.
pub const MAX_COMPUTER_USE_SURFACES: usize = 8;

/// Maximum handoff protocol versions advertised by one worker.
pub const MAX_HANDOFF_PROTOCOL_VERSIONS: usize = 8;

/// Maximum data-residency labels on one capabilities message.
pub const MAX_DATA_REGIONS: usize = 8;

/// Maximum worker labels.
pub const MAX_WORKER_LABELS: usize = 16;

/// Maximum UTF-8 bytes in a label key or value.
pub const MAX_LABEL_BYTES: usize = 64;

/// Maximum capability classes on one lease.
pub const MAX_CAPABILITY_CLASSES: usize = 16;

/// Maximum content-addressed inputs or outputs on one lease/result.
pub const MAX_ARTIFACT_REFS: usize = 32;

/// Maximum worker attestations on one message.
pub const MAX_ATTESTATIONS: usize = 16;

/// Maximum milliCPU units accepted on a lease or capability limit.
pub const MAX_CPU_MILLIS: u32 = 256_000;

/// Maximum memory (MiB) accepted on a lease or capability limit.
pub const MAX_MEMORY_MB: u32 = 65_536;

/// Maximum pids accepted on a lease or capability limit.
pub const MAX_PIDS: u32 = 4_096;

/// Maximum timeout milliseconds accepted on a lease or capability limit.
pub const MAX_TIMEOUT_MS: u32 = 3_600_000;

/// Maximum captured output bytes accepted on a lease or capability limit.
pub const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;

const HEX_TABLE: &[u8; 16] = b"0123456789abcdef";
const NONCE_LEN: usize = 16;
const SIGNATURE_LEN: usize = 32;

const FORBIDDEN_GUEST_PREFIXES: &[&str] = &[
    "etc",
    "dev",
    "proc",
    "sys",
    "root",
    "home",
    "users",
    "var/run/docker",
    "run/docker",
];

const HOST_AUTHORITY_FIELDS: &[&str] = &[
    "host_path",
    "hostpath",
    "bind",
    "bind_source",
    "source_path",
    "cwd_host",
    "root",
    "mount_from",
    "path",
    "windows_path",
    "unc",
];

const SECRET_FIELDS: &[&str] = &[
    "secret",
    "secrets",
    "plaintext",
    "payload",
    "content",
    "token",
    "api_key",
    "password",
];

macro_rules! string_enum {
    (
        $(#[$enum_meta:meta])*
        $name:ident,
        $err:ident,
        $expect:literal,
        { $($variant:ident => $wire:literal),+ $(,)? }
    ) => {
        $(#[$enum_meta])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $($variant),+
        }

        /// Parse failure for an unknown `$name` wire string.
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $err;

        impl $name {
            /// Every currently defined variant.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// Canonical wire form.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = $err;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                for item in Self::ALL {
                    if item.as_str() == s {
                        return Ok(*item);
                    }
                }
                Err($err)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                raw.parse()
                    .map_err(|_| de::Error::unknown_variant(&raw, &[$expect]))
            }
        }

        impl fmt::Display for $err {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!("unknown ", stringify!($name), " value"))
            }
        }

        impl Error for $err {}
    };
}

/// Failure when constructing or decoding a remote-worker message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteWorkerError {
    UnknownMajor { offered: u16 },
    IncompatibleMajor { offered: u16, supported: u16 },
    SchemaMismatch,
    UnknownKind,
    MissingField,
    DuplicateField,
    Empty,
    BoundExceeded,
    HostPathForbidden,
    ForbiddenGuestPath,
    InvalidId,
    InvalidArtifact,
    InvalidPath,
    InvalidHex,
    ExpiryRequired,
    CancelDeadlineRequired,
    CancelDeadlineAfterExpiry,
    TimeoutInvalid,
    ResourceLimit,
    CapabilityInconsistent,
    ZeroNonce,
    ZeroSignature,
    DuplicateValue,
    PercentInvalid,
}

/// Offered or negotiated remote-worker protocol version.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ProtocolVersion {
    major: u16,
    minor: u16,
}

/// Controller identity bound into a work lease. Distinct from a worker claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ControllerId(RuntimeId);

/// Authenticated-or-claimed worker identity. Claims are not authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct WorkerId(RuntimeId);

/// Work-lease identity. Distinct from an in-process capability [`crate::id::LeaseId`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct WorkLeaseId(RuntimeId);

/// 16-byte lease nonce. Wire form is 32 lowercase hex digits.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct LeaseNonce([u8; NONCE_LEN]);

/// 32-byte controller signature over the unsigned lease payload.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct LeaseSignature([u8; SIGNATURE_LEN]);

string_enum!(
    /// Worker OS family advertised during registration.
    WorkerPlatform,
    WorkerPlatformParseError,
    "a worker platform",
    { Linux => "linux", Windows => "windows", Macos => "macos" }
);

string_enum!(
    /// Worker CPU architecture.
    WorkerArch,
    WorkerArchParseError,
    "a worker architecture",
    { X64 => "x64", Arm64 => "arm64" }
);

string_enum!(
    /// Observed worker liveness class. Not a scheduling grant.
    WorkerHealth,
    WorkerHealthParseError,
    "a worker health class",
    { Healthy => "healthy", Degraded => "degraded", Unhealthy => "unhealthy" }
);

string_enum!(
    /// Device class a worker may host. iOS requires macOS at validation.
    DeviceCapability,
    DeviceCapabilityParseError,
    "a device capability",
    {
        IosSimulator => "ios-simulator",
        AndroidEmulator => "android-emulator",
        Desktop => "desktop",
    }
);

string_enum!(
    /// Computer Use surface a worker may host.
    ComputerUseSurface,
    ComputerUseSurfaceParseError,
    "a computer-use surface",
    {
        Browser => "browser",
        Desktop => "desktop",
        Tui => "tui",
        Android => "android",
        Ios => "ios",
    }
);

string_enum!(
    /// Content-addressed attestation class.
    WorkerAttestationClass,
    WorkerAttestationClassParseError,
    "a worker attestation class",
    {
        Platform => "platform",
        Firecracker => "firecracker",
        Jailer => "jailer",
        Rootfs => "rootfs",
    }
);

string_enum!(
    /// Closed capability class a lease may authorize. Not a transferable grant.
    CapabilityClass,
    CapabilityClassParseError,
    "a capability class",
    {
        FsRead => "fs.read",
        FsWrite => "fs.write",
        ProcExec => "proc.exec",
        NetConnect => "net.connect",
        GitWrite => "git.write",
        SecretUse => "secret.use",
        BrowserNavigate => "browser.navigate",
        BrowserDownload => "browser.download",
        MobileControl => "mobile.control",
        McpInvoke => "mcp.invoke",
        PluginInvoke => "plugin.invoke",
    }
);

string_enum!(
    /// Work-progress phase. Large output is an artifact reference, never a body.
    WorkPhase,
    WorkPhaseParseError,
    "a work phase",
    {
        Queued => "queued",
        Running => "running",
        Uploading => "uploading",
        Finalizing => "finalizing",
    }
);

string_enum!(
    /// Terminal work status. Expired-lease results are quarantined.
    WorkStatus,
    WorkStatusParseError,
    "a work status",
    {
        Succeeded => "succeeded",
        Failed => "failed",
        Cancelled => "cancelled",
        Quarantined => "quarantined",
    }
);

string_enum!(
    /// Why the controller cancelled a lease.
    WorkCancelReason,
    WorkCancelReasonParseError,
    "a work cancel reason",
    {
        ControllerRequested => "controller_requested",
        Expired => "expired",
        Policy => "policy",
        HeartbeatLost => "heartbeat_lost",
    }
);

string_enum!(
    /// Remote-worker message discriminator.
    WorkerMessageKind,
    WorkerMessageKindParseError,
    "a remote-worker message kind",
    {
        Registration => "registration",
        Capabilities => "capabilities",
        WorkLease => "work_lease",
        Progress => "progress",
        Result => "result",
        Cancel => "cancel",
    }
);

/// Content-addressed attestation statement. No host path or secret material.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct WorkerAttestation {
    class: WorkerAttestationClass,
    digest: ArtifactId,
}

/// Bounded resource and capability ceiling issued by the controller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityLimits {
    classes: Vec<CapabilityClass>,
    max_cpu_millis: u32,
    max_memory_mb: u32,
    max_pids: u32,
    max_timeout_ms: u32,
    max_output_bytes: u64,
    network: NetworkMode,
}

/// Guest-only sandbox policy carried on a work lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerSandboxSpec {
    tier: SandboxTier,
    image: ArtifactRef,
    cwd: RepoPath,
    network: NetworkMode,
    cpu_millis: u32,
    memory_mb: u32,
    pids: u32,
    timeout_ms: u32,
    output_limit: u64,
}

/// Optional GPU advertisement. Vendor/model are labels, not device paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GpuAdvertisement {
    vendor: String,
    model: String,
}

/// Worker-advertised capabilities. Claims are intersected by the controller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerCapabilities {
    worker_id: WorkerId,
    platform: WorkerPlatform,
    arch: WorkerArch,
    sandbox_tiers: Vec<SandboxTier>,
    backends: Vec<SandboxTier>,
    device_caps: Vec<DeviceCapability>,
    computer_use: Vec<ComputerUseSurface>,
    interactive_display: bool,
    handoff_protocol_versions: Vec<ProtocolVersion>,
    gpu: Option<GpuAdvertisement>,
    labels: Vec<WorkerLabel>,
    health: WorkerHealth,
    data_regions: Vec<String>,
    attestations: Vec<WorkerAttestation>,
}

/// Bounded worker label. Values cannot carry host-path authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerLabel {
    key: String,
    value: String,
}

/// Worker registration. `worker_id` is a claim; mTLS binds identity later.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerRegistration {
    protocol: ProtocolVersion,
    trace_id: TraceId,
    worker_id: WorkerId,
    platform: WorkerPlatform,
    arch: WorkerArch,
    nonce: LeaseNonce,
    attestation: WorkerAttestation,
}

/// Controller-issued work lease. Worker payload cannot broaden this object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkLease {
    protocol: ProtocolVersion,
    trace_id: TraceId,
    lease_id: WorkLeaseId,
    worker_id: WorkerId,
    controller_id: ControllerId,
    job_id: JobId,
    expires_at_unix_ms: u64,
    cancel_deadline_unix_ms: u64,
    task_digest: ArtifactId,
    input_artifacts: Vec<ArtifactRef>,
    sandbox_spec: WorkerSandboxSpec,
    capability_limits: CapabilityLimits,
    nonce: LeaseNonce,
    signature: LeaseSignature,
}

/// Bounded progress event. Large logs are artifact refs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkProgress {
    protocol: ProtocolVersion,
    trace_id: TraceId,
    lease_id: WorkLeaseId,
    seq: u64,
    phase: WorkPhase,
    percent: Option<u8>,
    artifact: Option<ArtifactRef>,
}

/// Worker result. Identity is the lease; no worker-id override field exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkResult {
    protocol: ProtocolVersion,
    trace_id: TraceId,
    lease_id: WorkLeaseId,
    status: WorkStatus,
    task_digest: ArtifactId,
    artifact_outputs: Vec<ArtifactRef>,
    resource_usage: ResourceUsage,
    worker_attestation: WorkerAttestation,
}

/// Observed resource counters. Never used as a privilege request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ResourceUsage {
    cpu_millis: u32,
    memory_mb_peak: u32,
    pids_peak: u32,
    output_bytes: u64,
}

/// Controller cancel. Deadline is mandatory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkCancel {
    protocol: ProtocolVersion,
    trace_id: TraceId,
    lease_id: WorkLeaseId,
    controller_id: ControllerId,
    reason: WorkCancelReason,
    issued_at_unix_ms: u64,
    deadline_unix_ms: u64,
}

/// Versioned remote-worker frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerMessage {
    Registration(WorkerRegistration),
    Capabilities(WorkerCapabilitiesMessage),
    WorkLease(WorkLease),
    Progress(WorkProgress),
    Result(WorkResult),
    Cancel(WorkCancel),
}

/// Capabilities advertisement framed as a versioned message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerCapabilitiesMessage {
    protocol: ProtocolVersion,
    trace_id: TraceId,
    capabilities: WorkerCapabilities,
}

impl fmt::Display for RemoteWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownMajor { offered } => {
                write!(f, "unknown remote-worker protocol major {offered}")
            }
            Self::IncompatibleMajor { offered, supported } => write!(
                f,
                "incompatible remote-worker protocol major {offered} (supported {supported})"
            ),
            Self::SchemaMismatch => f.write_str("remote-worker schema mismatch"),
            Self::UnknownKind => f.write_str("unknown remote-worker message kind"),
            Self::MissingField => f.write_str("remote-worker message missing a required field"),
            Self::DuplicateField => f.write_str("remote-worker message has a duplicate field"),
            Self::Empty => f.write_str("remote-worker message is empty"),
            Self::BoundExceeded => f.write_str("remote-worker message exceeds a documented bound"),
            Self::HostPathForbidden => {
                f.write_str("remote-worker message must not carry host-path authority")
            }
            Self::ForbiddenGuestPath => {
                f.write_str("remote-worker guest path is not an allowed workspace target")
            }
            Self::InvalidId => f.write_str("malformed remote-worker identifier"),
            Self::InvalidArtifact => f.write_str("malformed remote-worker artifact reference"),
            Self::InvalidPath => f.write_str("malformed remote-worker repository path"),
            Self::InvalidHex => f.write_str("malformed remote-worker hex field"),
            Self::ExpiryRequired => f.write_str("work lease expiry is required"),
            Self::CancelDeadlineRequired => f.write_str("work lease cancel deadline is required"),
            Self::CancelDeadlineAfterExpiry => {
                f.write_str("work lease cancel deadline must not exceed expiry")
            }
            Self::TimeoutInvalid => f.write_str("work lease timeout is invalid"),
            Self::ResourceLimit => f.write_str("work lease resource limit is invalid"),
            Self::CapabilityInconsistent => {
                f.write_str("worker capability advertisement is inconsistent")
            }
            Self::ZeroNonce => f.write_str("work lease nonce must be non-zero"),
            Self::ZeroSignature => f.write_str("work lease signature must be non-zero"),
            Self::DuplicateValue => f.write_str("remote-worker list contains a duplicate value"),
            Self::PercentInvalid => f.write_str("work progress percent must be 0..=100"),
        }
    }
}

impl Error for RemoteWorkerError {}

impl From<IdParseError> for RemoteWorkerError {
    fn from(_: IdParseError) -> Self {
        Self::InvalidId
    }
}

impl From<RepoPathError> for RemoteWorkerError {
    fn from(_: RepoPathError) -> Self {
        Self::InvalidPath
    }
}

impl ProtocolVersion {
    /// Currently supported protocol version.
    pub const V1: Self = Self {
        major: REMOTE_WORKER_PROTOCOL_MAJOR,
        minor: REMOTE_WORKER_PROTOCOL_MINOR,
    };

    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    pub const fn major(self) -> u16 {
        self.major
    }

    pub const fn minor(self) -> u16 {
        self.minor
    }

    /// Fail closed on unknown or incompatible majors. Same major uses `min(minor)`.
    pub fn negotiate(self) -> Result<Self, RemoteWorkerError> {
        if self.major != REMOTE_WORKER_PROTOCOL_MAJOR {
            if self.major == 0 || !known_major(self.major) {
                return Err(RemoteWorkerError::UnknownMajor {
                    offered: self.major,
                });
            }
            return Err(RemoteWorkerError::IncompatibleMajor {
                offered: self.major,
                supported: REMOTE_WORKER_PROTOCOL_MAJOR,
            });
        }
        Ok(Self {
            major: REMOTE_WORKER_PROTOCOL_MAJOR,
            // Negotiation caps the offered minor at the supported minor. The
            // cap is degenerate while the supported minor is 0 (u16.min(0));
            // restore `self.minor.min(REMOTE_WORKER_PROTOCOL_MINOR)` when the
            // supported minor is bumped above 0.
            minor: REMOTE_WORKER_PROTOCOL_MINOR,
        })
    }
}

impl Serialize for ProtocolVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ProtocolVersion", 2)?;
        state.serialize_field("major", &self.major)?;
        state.serialize_field("minor", &self.minor)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            major: u16,
            minor: u16,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self::new(raw.major, raw.minor))
    }
}

macro_rules! runtime_id_newtype {
    ($name:ident) => {
        impl $name {
            /// Allocate a fresh UUIDv7 identifier.
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(RuntimeId::new())
            }

            pub const fn from_runtime(id: RuntimeId) -> Self {
                Self(id)
            }

            pub const fn as_runtime(self) -> RuntimeId {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                RuntimeId::from_str(s).map(Self)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.0.serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                RuntimeId::deserialize(deserializer).map(Self)
            }
        }
    };
}

runtime_id_newtype!(ControllerId);
runtime_id_newtype!(WorkerId);
runtime_id_newtype!(WorkLeaseId);

impl LeaseNonce {
    pub fn from_bytes(bytes: [u8; NONCE_LEN]) -> Result<Self, RemoteWorkerError> {
        if bytes.iter().all(|b| *b == 0) {
            Err(RemoteWorkerError::ZeroNonce)
        } else {
            Ok(Self(bytes))
        }
    }

    pub const fn as_bytes(&self) -> &[u8; NONCE_LEN] {
        &self.0
    }

    fn wire_str(self) -> String {
        encode_hex(&self.0)
    }
}

impl fmt::Display for LeaseNonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.wire_str())
    }
}

impl fmt::Debug for LeaseNonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("LeaseNonce")
            .field(&self.to_string())
            .finish()
    }
}

impl FromStr for LeaseNonce {
    type Err = RemoteWorkerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_bytes(decode_hex::<NONCE_LEN>(s)?)
    }
}

impl Serialize for LeaseNonce {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.wire_str())
    }
}

impl<'de> Deserialize<'de> for LeaseNonce {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(de::Error::custom)
    }
}

impl LeaseSignature {
    pub fn from_bytes(bytes: [u8; SIGNATURE_LEN]) -> Result<Self, RemoteWorkerError> {
        if bytes.iter().all(|b| *b == 0) {
            Err(RemoteWorkerError::ZeroSignature)
        } else {
            Ok(Self(bytes))
        }
    }

    pub const fn as_bytes(&self) -> &[u8; SIGNATURE_LEN] {
        &self.0
    }

    fn wire_str(self) -> String {
        encode_hex(&self.0)
    }
}

impl fmt::Display for LeaseSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.wire_str())
    }
}

impl fmt::Debug for LeaseSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("LeaseSignature")
            .field(&self.to_string())
            .finish()
    }
}

impl FromStr for LeaseSignature {
    type Err = RemoteWorkerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_bytes(decode_hex::<SIGNATURE_LEN>(s)?)
    }
}

impl Serialize for LeaseSignature {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.wire_str())
    }
}

impl<'de> Deserialize<'de> for LeaseSignature {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(de::Error::custom)
    }
}

impl WorkerAttestation {
    pub fn new(class: WorkerAttestationClass, digest: ArtifactId) -> Self {
        Self { class, digest }
    }

    pub const fn class(&self) -> WorkerAttestationClass {
        self.class
    }

    pub const fn digest(&self) -> ArtifactId {
        self.digest
    }
}

impl Serialize for WorkerAttestation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("WorkerAttestation", 2)?;
        state.serialize_field("class", &self.class)?;
        state.serialize_field("digest", &self.digest)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for WorkerAttestation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            class: WorkerAttestationClass,
            digest: ArtifactId,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self::new(raw.class, raw.digest))
    }
}

impl CapabilityLimits {
    pub fn new(
        classes: Vec<CapabilityClass>,
        max_cpu_millis: u32,
        max_memory_mb: u32,
        max_pids: u32,
        max_timeout_ms: u32,
        max_output_bytes: u64,
        network: NetworkMode,
    ) -> Result<Self, RemoteWorkerError> {
        if classes.len() > MAX_CAPABILITY_CLASSES {
            return Err(RemoteWorkerError::BoundExceeded);
        }
        if has_duplicate_classes(&classes) {
            return Err(RemoteWorkerError::DuplicateValue);
        }
        validate_resources(
            max_cpu_millis,
            max_memory_mb,
            max_pids,
            max_timeout_ms,
            max_output_bytes,
        )?;
        Ok(Self {
            classes,
            max_cpu_millis,
            max_memory_mb,
            max_pids,
            max_timeout_ms,
            max_output_bytes,
            network,
        })
    }

    pub fn classes(&self) -> &[CapabilityClass] {
        &self.classes
    }

    pub const fn max_cpu_millis(&self) -> u32 {
        self.max_cpu_millis
    }

    pub const fn max_memory_mb(&self) -> u32 {
        self.max_memory_mb
    }

    pub const fn max_pids(&self) -> u32 {
        self.max_pids
    }

    pub const fn max_timeout_ms(&self) -> u32 {
        self.max_timeout_ms
    }

    pub const fn max_output_bytes(&self) -> u64 {
        self.max_output_bytes
    }

    pub const fn network(&self) -> NetworkMode {
        self.network
    }
}

impl Serialize for CapabilityLimits {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CapabilityLimits", 7)?;
        state.serialize_field("classes", &self.classes)?;
        state.serialize_field("max_cpu_millis", &self.max_cpu_millis)?;
        state.serialize_field("max_memory_mb", &self.max_memory_mb)?;
        state.serialize_field("max_pids", &self.max_pids)?;
        state.serialize_field("max_timeout_ms", &self.max_timeout_ms)?;
        state.serialize_field("max_output_bytes", &self.max_output_bytes)?;
        state.serialize_field("network", &self.network)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CapabilityLimits {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            classes: Vec<CapabilityClass>,
            max_cpu_millis: u32,
            max_memory_mb: u32,
            max_pids: u32,
            max_timeout_ms: u32,
            max_output_bytes: u64,
            network: NetworkMode,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(
            raw.classes,
            raw.max_cpu_millis,
            raw.max_memory_mb,
            raw.max_pids,
            raw.max_timeout_ms,
            raw.max_output_bytes,
            raw.network,
        )
        .map_err(de::Error::custom)
    }
}

impl WorkerSandboxSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tier: SandboxTier,
        image: ArtifactRef,
        cwd: RepoPath,
        network: NetworkMode,
        cpu_millis: u32,
        memory_mb: u32,
        pids: u32,
        timeout_ms: u32,
        output_limit: u64,
    ) -> Result<Self, RemoteWorkerError> {
        validate_artifact(&image)?;
        validate_guest_path(&cwd)?;
        validate_resources(cpu_millis, memory_mb, pids, timeout_ms, output_limit)?;
        Ok(Self {
            tier,
            image,
            cwd,
            network,
            cpu_millis,
            memory_mb,
            pids,
            timeout_ms,
            output_limit,
        })
    }

    pub const fn tier(&self) -> SandboxTier {
        self.tier
    }

    pub fn image(&self) -> &ArtifactRef {
        &self.image
    }

    pub fn cwd(&self) -> &RepoPath {
        &self.cwd
    }

    pub const fn network(&self) -> NetworkMode {
        self.network
    }

    pub const fn cpu_millis(&self) -> u32 {
        self.cpu_millis
    }

    pub const fn memory_mb(&self) -> u32 {
        self.memory_mb
    }

    pub const fn pids(&self) -> u32 {
        self.pids
    }

    pub const fn timeout_ms(&self) -> u32 {
        self.timeout_ms
    }

    pub const fn output_limit(&self) -> u64 {
        self.output_limit
    }
}

impl Serialize for WorkerSandboxSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("WorkerSandboxSpec", 9)?;
        state.serialize_field("tier", &self.tier)?;
        state.serialize_field("image", &self.image)?;
        state.serialize_field("cwd", &self.cwd)?;
        state.serialize_field("network", &self.network)?;
        state.serialize_field("cpu_millis", &self.cpu_millis)?;
        state.serialize_field("memory_mb", &self.memory_mb)?;
        state.serialize_field("pids", &self.pids)?;
        state.serialize_field("timeout_ms", &self.timeout_ms)?;
        state.serialize_field("output_limit", &self.output_limit)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for WorkerSandboxSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            tier: SandboxTier,
            image: ArtifactRef,
            cwd: RepoPath,
            network: NetworkMode,
            cpu_millis: u32,
            memory_mb: u32,
            pids: u32,
            timeout_ms: u32,
            output_limit: u64,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(
            raw.tier,
            raw.image,
            raw.cwd,
            raw.network,
            raw.cpu_millis,
            raw.memory_mb,
            raw.pids,
            raw.timeout_ms,
            raw.output_limit,
        )
        .map_err(de::Error::custom)
    }
}

impl GpuAdvertisement {
    pub fn new(
        vendor: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, RemoteWorkerError> {
        let vendor = vendor.into();
        let model = model.into();
        validate_label_part(&vendor)?;
        validate_label_part(&model)?;
        Ok(Self { vendor, model })
    }

    pub fn vendor(&self) -> &str {
        &self.vendor
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

impl Serialize for GpuAdvertisement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("GpuAdvertisement", 2)?;
        state.serialize_field("vendor", &self.vendor)?;
        state.serialize_field("model", &self.model)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for GpuAdvertisement {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            vendor: String,
            model: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(raw.vendor, raw.model).map_err(de::Error::custom)
    }
}

impl WorkerLabel {
    pub fn new(
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, RemoteWorkerError> {
        let key = key.into();
        let value = value.into();
        validate_label_part(&key)?;
        validate_label_part(&value)?;
        Ok(Self { key, value })
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

impl Serialize for WorkerLabel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("WorkerLabel", 2)?;
        state.serialize_field("key", &self.key)?;
        state.serialize_field("value", &self.value)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for WorkerLabel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            key: String,
            value: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(raw.key, raw.value).map_err(de::Error::custom)
    }
}

impl WorkerCapabilities {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        worker_id: WorkerId,
        platform: WorkerPlatform,
        arch: WorkerArch,
        sandbox_tiers: Vec<SandboxTier>,
        backends: Vec<SandboxTier>,
        device_caps: Vec<DeviceCapability>,
        computer_use: Vec<ComputerUseSurface>,
        interactive_display: bool,
        handoff_protocol_versions: Vec<ProtocolVersion>,
        gpu: Option<GpuAdvertisement>,
        labels: Vec<WorkerLabel>,
        health: WorkerHealth,
        data_regions: Vec<String>,
        attestations: Vec<WorkerAttestation>,
    ) -> Result<Self, RemoteWorkerError> {
        if sandbox_tiers.is_empty()
            || sandbox_tiers.len() > MAX_ADVERTISED_FLAGS
            || backends.len() > MAX_ADVERTISED_FLAGS
            || device_caps.len() > MAX_ADVERTISED_FLAGS
            || computer_use.len() > MAX_COMPUTER_USE_SURFACES
            || handoff_protocol_versions.len() > MAX_HANDOFF_PROTOCOL_VERSIONS
            || labels.len() > MAX_WORKER_LABELS
            || data_regions.len() > MAX_DATA_REGIONS
            || attestations.is_empty()
            || attestations.len() > MAX_ATTESTATIONS
        {
            return Err(RemoteWorkerError::BoundExceeded);
        }
        if has_duplicate(&sandbox_tiers) || has_duplicate(&backends) || has_duplicate(&device_caps)
        {
            return Err(RemoteWorkerError::DuplicateValue);
        }
        if has_duplicate(&computer_use) || has_duplicate_regions(&data_regions) {
            return Err(RemoteWorkerError::DuplicateValue);
        }
        if has_duplicate_attestation_class(&attestations) || has_duplicate_label_keys(&labels) {
            return Err(RemoteWorkerError::DuplicateValue);
        }
        for region in &data_regions {
            validate_label_part(region)?;
        }
        for version in &handoff_protocol_versions {
            version.negotiate()?;
        }
        let ios_surface = computer_use.contains(&ComputerUseSurface::Ios);
        let ios_device = device_caps.contains(&DeviceCapability::IosSimulator);
        if (ios_surface || ios_device) && platform != WorkerPlatform::Macos {
            return Err(RemoteWorkerError::CapabilityInconsistent);
        }
        Ok(Self {
            worker_id,
            platform,
            arch,
            sandbox_tiers,
            backends,
            device_caps,
            computer_use,
            interactive_display,
            handoff_protocol_versions,
            gpu,
            labels,
            health,
            data_regions,
            attestations,
        })
    }

    pub const fn worker_id(&self) -> WorkerId {
        self.worker_id
    }

    pub const fn platform(&self) -> WorkerPlatform {
        self.platform
    }

    pub const fn arch(&self) -> WorkerArch {
        self.arch
    }

    pub fn sandbox_tiers(&self) -> &[SandboxTier] {
        &self.sandbox_tiers
    }

    pub fn backends(&self) -> &[SandboxTier] {
        &self.backends
    }

    pub fn device_caps(&self) -> &[DeviceCapability] {
        &self.device_caps
    }

    pub fn computer_use(&self) -> &[ComputerUseSurface] {
        &self.computer_use
    }

    pub const fn interactive_display(&self) -> bool {
        self.interactive_display
    }

    pub fn handoff_protocol_versions(&self) -> &[ProtocolVersion] {
        &self.handoff_protocol_versions
    }

    pub fn gpu(&self) -> Option<&GpuAdvertisement> {
        self.gpu.as_ref()
    }

    pub fn labels(&self) -> &[WorkerLabel] {
        &self.labels
    }

    pub const fn health(&self) -> WorkerHealth {
        self.health
    }

    pub fn data_regions(&self) -> &[String] {
        &self.data_regions
    }

    pub fn attestations(&self) -> &[WorkerAttestation] {
        &self.attestations
    }
}

impl Serialize for WorkerCapabilities {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("WorkerCapabilities", 14)?;
        state.serialize_field("worker_id", &self.worker_id)?;
        state.serialize_field("platform", &self.platform)?;
        state.serialize_field("arch", &self.arch)?;
        state.serialize_field("sandbox_tiers", &self.sandbox_tiers)?;
        state.serialize_field("backends", &self.backends)?;
        state.serialize_field("device_caps", &self.device_caps)?;
        state.serialize_field("computer_use", &self.computer_use)?;
        state.serialize_field("interactive_display", &self.interactive_display)?;
        state.serialize_field("handoff_protocol_versions", &self.handoff_protocol_versions)?;
        state.serialize_field("gpu", &self.gpu)?;
        state.serialize_field("labels", &self.labels)?;
        state.serialize_field("health", &self.health)?;
        state.serialize_field("data_regions", &self.data_regions)?;
        state.serialize_field("attestations", &self.attestations)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for WorkerCapabilities {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            worker_id: WorkerId,
            platform: WorkerPlatform,
            arch: WorkerArch,
            sandbox_tiers: Vec<SandboxTier>,
            backends: Vec<SandboxTier>,
            device_caps: Vec<DeviceCapability>,
            computer_use: Vec<ComputerUseSurface>,
            interactive_display: bool,
            handoff_protocol_versions: Vec<ProtocolVersion>,
            gpu: Option<GpuAdvertisement>,
            labels: Vec<WorkerLabel>,
            health: WorkerHealth,
            data_regions: Vec<String>,
            attestations: Vec<WorkerAttestation>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(
            raw.worker_id,
            raw.platform,
            raw.arch,
            raw.sandbox_tiers,
            raw.backends,
            raw.device_caps,
            raw.computer_use,
            raw.interactive_display,
            raw.handoff_protocol_versions,
            raw.gpu,
            raw.labels,
            raw.health,
            raw.data_regions,
            raw.attestations,
        )
        .map_err(de::Error::custom)
    }
}

impl WorkerRegistration {
    pub fn new(
        protocol: ProtocolVersion,
        trace_id: TraceId,
        worker_id: WorkerId,
        platform: WorkerPlatform,
        arch: WorkerArch,
        nonce: LeaseNonce,
        attestation: WorkerAttestation,
    ) -> Result<Self, RemoteWorkerError> {
        let protocol = protocol.negotiate()?;
        Ok(Self {
            protocol,
            trace_id,
            worker_id,
            platform,
            arch,
            nonce,
            attestation,
        })
    }

    pub const fn protocol(&self) -> ProtocolVersion {
        self.protocol
    }

    pub const fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub const fn worker_id(&self) -> WorkerId {
        self.worker_id
    }

    pub const fn platform(&self) -> WorkerPlatform {
        self.platform
    }

    pub const fn arch(&self) -> WorkerArch {
        self.arch
    }

    pub const fn nonce(&self) -> LeaseNonce {
        self.nonce
    }

    pub fn attestation(&self) -> &WorkerAttestation {
        &self.attestation
    }
}

impl WorkLease {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        protocol: ProtocolVersion,
        trace_id: TraceId,
        lease_id: WorkLeaseId,
        worker_id: WorkerId,
        controller_id: ControllerId,
        job_id: JobId,
        expires_at_unix_ms: u64,
        cancel_deadline_unix_ms: u64,
        task_digest: ArtifactId,
        input_artifacts: Vec<ArtifactRef>,
        sandbox_spec: WorkerSandboxSpec,
        capability_limits: CapabilityLimits,
        nonce: LeaseNonce,
        signature: LeaseSignature,
    ) -> Result<Self, RemoteWorkerError> {
        let protocol = protocol.negotiate()?;
        if expires_at_unix_ms == 0 {
            return Err(RemoteWorkerError::ExpiryRequired);
        }
        if cancel_deadline_unix_ms == 0 {
            return Err(RemoteWorkerError::CancelDeadlineRequired);
        }
        if cancel_deadline_unix_ms > expires_at_unix_ms {
            return Err(RemoteWorkerError::CancelDeadlineAfterExpiry);
        }
        if input_artifacts.len() > MAX_ARTIFACT_REFS {
            return Err(RemoteWorkerError::BoundExceeded);
        }
        if has_duplicate_artifact_ids(&input_artifacts) {
            return Err(RemoteWorkerError::DuplicateValue);
        }
        for artifact in &input_artifacts {
            validate_artifact(artifact)?;
        }
        if sandbox_spec.cpu_millis() > capability_limits.max_cpu_millis()
            || sandbox_spec.memory_mb() > capability_limits.max_memory_mb()
            || sandbox_spec.pids() > capability_limits.max_pids()
            || sandbox_spec.timeout_ms() > capability_limits.max_timeout_ms()
            || sandbox_spec.output_limit() > capability_limits.max_output_bytes()
        {
            return Err(RemoteWorkerError::ResourceLimit);
        }
        if sandbox_spec.network() == NetworkMode::Allow
            && capability_limits.network() == NetworkMode::Deny
        {
            return Err(RemoteWorkerError::ResourceLimit);
        }
        Ok(Self {
            protocol,
            trace_id,
            lease_id,
            worker_id,
            controller_id,
            job_id,
            expires_at_unix_ms,
            cancel_deadline_unix_ms,
            task_digest,
            input_artifacts,
            sandbox_spec,
            capability_limits,
            nonce,
            signature,
        })
    }

    pub const fn protocol(&self) -> ProtocolVersion {
        self.protocol
    }

    pub const fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub const fn lease_id(&self) -> WorkLeaseId {
        self.lease_id
    }

    pub const fn worker_id(&self) -> WorkerId {
        self.worker_id
    }

    pub const fn controller_id(&self) -> ControllerId {
        self.controller_id
    }

    pub const fn job_id(&self) -> JobId {
        self.job_id
    }

    pub const fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }

    pub const fn cancel_deadline_unix_ms(&self) -> u64 {
        self.cancel_deadline_unix_ms
    }

    pub const fn task_digest(&self) -> ArtifactId {
        self.task_digest
    }

    pub fn input_artifacts(&self) -> &[ArtifactRef] {
        &self.input_artifacts
    }

    pub fn sandbox_spec(&self) -> &WorkerSandboxSpec {
        &self.sandbox_spec
    }

    pub fn capability_limits(&self) -> &CapabilityLimits {
        &self.capability_limits
    }

    pub const fn nonce(&self) -> LeaseNonce {
        self.nonce
    }

    pub const fn signature(&self) -> LeaseSignature {
        self.signature
    }
}

impl WorkProgress {
    pub fn new(
        protocol: ProtocolVersion,
        trace_id: TraceId,
        lease_id: WorkLeaseId,
        seq: u64,
        phase: WorkPhase,
        percent: Option<u8>,
        artifact: Option<ArtifactRef>,
    ) -> Result<Self, RemoteWorkerError> {
        let protocol = protocol.negotiate()?;
        if let Some(percent) = percent
            && percent > 100
        {
            return Err(RemoteWorkerError::PercentInvalid);
        }
        if let Some(artifact) = &artifact {
            validate_artifact(artifact)?;
        }
        Ok(Self {
            protocol,
            trace_id,
            lease_id,
            seq,
            phase,
            percent,
            artifact,
        })
    }

    pub const fn protocol(&self) -> ProtocolVersion {
        self.protocol
    }

    pub const fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub const fn lease_id(&self) -> WorkLeaseId {
        self.lease_id
    }

    pub const fn seq(&self) -> u64 {
        self.seq
    }

    pub const fn phase(&self) -> WorkPhase {
        self.phase
    }

    pub const fn percent(&self) -> Option<u8> {
        self.percent
    }

    pub fn artifact(&self) -> Option<&ArtifactRef> {
        self.artifact.as_ref()
    }
}

impl ResourceUsage {
    pub fn new(
        cpu_millis: u32,
        memory_mb_peak: u32,
        pids_peak: u32,
        output_bytes: u64,
    ) -> Result<Self, RemoteWorkerError> {
        if cpu_millis > MAX_CPU_MILLIS
            || memory_mb_peak > MAX_MEMORY_MB
            || pids_peak > MAX_PIDS
            || output_bytes > MAX_OUTPUT_BYTES
        {
            return Err(RemoteWorkerError::BoundExceeded);
        }
        Ok(Self {
            cpu_millis,
            memory_mb_peak,
            pids_peak,
            output_bytes,
        })
    }

    pub const fn cpu_millis(self) -> u32 {
        self.cpu_millis
    }

    pub const fn memory_mb_peak(self) -> u32 {
        self.memory_mb_peak
    }

    pub const fn pids_peak(self) -> u32 {
        self.pids_peak
    }

    pub const fn output_bytes(self) -> u64 {
        self.output_bytes
    }
}

impl Serialize for ResourceUsage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ResourceUsage", 4)?;
        state.serialize_field("cpu_millis", &self.cpu_millis)?;
        state.serialize_field("memory_mb_peak", &self.memory_mb_peak)?;
        state.serialize_field("pids_peak", &self.pids_peak)?;
        state.serialize_field("output_bytes", &self.output_bytes)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ResourceUsage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            cpu_millis: u32,
            memory_mb_peak: u32,
            pids_peak: u32,
            output_bytes: u64,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(
            raw.cpu_millis,
            raw.memory_mb_peak,
            raw.pids_peak,
            raw.output_bytes,
        )
        .map_err(de::Error::custom)
    }
}

impl WorkResult {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        protocol: ProtocolVersion,
        trace_id: TraceId,
        lease_id: WorkLeaseId,
        status: WorkStatus,
        task_digest: ArtifactId,
        artifact_outputs: Vec<ArtifactRef>,
        resource_usage: ResourceUsage,
        worker_attestation: WorkerAttestation,
    ) -> Result<Self, RemoteWorkerError> {
        let protocol = protocol.negotiate()?;
        if artifact_outputs.len() > MAX_ARTIFACT_REFS {
            return Err(RemoteWorkerError::BoundExceeded);
        }
        if has_duplicate_artifact_ids(&artifact_outputs) {
            return Err(RemoteWorkerError::DuplicateValue);
        }
        for artifact in &artifact_outputs {
            validate_artifact(artifact)?;
        }
        Ok(Self {
            protocol,
            trace_id,
            lease_id,
            status,
            task_digest,
            artifact_outputs,
            resource_usage,
            worker_attestation,
        })
    }

    pub const fn protocol(&self) -> ProtocolVersion {
        self.protocol
    }

    pub const fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub const fn lease_id(&self) -> WorkLeaseId {
        self.lease_id
    }

    pub const fn status(&self) -> WorkStatus {
        self.status
    }

    pub const fn task_digest(&self) -> ArtifactId {
        self.task_digest
    }

    pub fn artifact_outputs(&self) -> &[ArtifactRef] {
        &self.artifact_outputs
    }

    pub const fn resource_usage(&self) -> ResourceUsage {
        self.resource_usage
    }

    pub fn worker_attestation(&self) -> &WorkerAttestation {
        &self.worker_attestation
    }
}

impl WorkCancel {
    pub fn new(
        protocol: ProtocolVersion,
        trace_id: TraceId,
        lease_id: WorkLeaseId,
        controller_id: ControllerId,
        reason: WorkCancelReason,
        issued_at_unix_ms: u64,
        deadline_unix_ms: u64,
    ) -> Result<Self, RemoteWorkerError> {
        let protocol = protocol.negotiate()?;
        if issued_at_unix_ms == 0 || deadline_unix_ms == 0 {
            return Err(RemoteWorkerError::CancelDeadlineRequired);
        }
        if deadline_unix_ms < issued_at_unix_ms {
            return Err(RemoteWorkerError::CancelDeadlineAfterExpiry);
        }
        Ok(Self {
            protocol,
            trace_id,
            lease_id,
            controller_id,
            reason,
            issued_at_unix_ms,
            deadline_unix_ms,
        })
    }

    pub const fn protocol(&self) -> ProtocolVersion {
        self.protocol
    }

    pub const fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub const fn lease_id(&self) -> WorkLeaseId {
        self.lease_id
    }

    pub const fn controller_id(&self) -> ControllerId {
        self.controller_id
    }

    pub const fn reason(&self) -> WorkCancelReason {
        self.reason
    }

    pub const fn issued_at_unix_ms(&self) -> u64 {
        self.issued_at_unix_ms
    }

    pub const fn deadline_unix_ms(&self) -> u64 {
        self.deadline_unix_ms
    }
}

impl WorkerCapabilitiesMessage {
    pub fn new(
        protocol: ProtocolVersion,
        trace_id: TraceId,
        capabilities: WorkerCapabilities,
    ) -> Result<Self, RemoteWorkerError> {
        let protocol = protocol.negotiate()?;
        Ok(Self {
            protocol,
            trace_id,
            capabilities,
        })
    }

    pub const fn protocol(&self) -> ProtocolVersion {
        self.protocol
    }

    pub const fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub fn capabilities(&self) -> &WorkerCapabilities {
        &self.capabilities
    }
}

impl WorkerMessage {
    pub const fn kind(&self) -> WorkerMessageKind {
        match self {
            Self::Registration(_) => WorkerMessageKind::Registration,
            Self::Capabilities(_) => WorkerMessageKind::Capabilities,
            Self::WorkLease(_) => WorkerMessageKind::WorkLease,
            Self::Progress(_) => WorkerMessageKind::Progress,
            Self::Result(_) => WorkerMessageKind::Result,
            Self::Cancel(_) => WorkerMessageKind::Cancel,
        }
    }

    pub const fn protocol(&self) -> ProtocolVersion {
        match self {
            Self::Registration(m) => m.protocol(),
            Self::Capabilities(m) => m.protocol(),
            Self::WorkLease(m) => m.protocol(),
            Self::Progress(m) => m.protocol(),
            Self::Result(m) => m.protocol(),
            Self::Cancel(m) => m.protocol(),
        }
    }

    pub const fn trace_id(&self) -> TraceId {
        match self {
            Self::Registration(m) => m.trace_id(),
            Self::Capabilities(m) => m.trace_id(),
            Self::WorkLease(m) => m.trace_id(),
            Self::Progress(m) => m.trace_id(),
            Self::Result(m) => m.trace_id(),
            Self::Cancel(m) => m.trace_id(),
        }
    }
}

impl Serialize for WorkerMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Registration(m) => serialize_registration(m, serializer),
            Self::Capabilities(m) => serialize_capabilities(m, serializer),
            Self::WorkLease(m) => serialize_work_lease(m, serializer),
            Self::Progress(m) => serialize_progress(m, serializer),
            Self::Result(m) => serialize_result(m, serializer),
            Self::Cancel(m) => serialize_cancel(m, serializer),
        }
    }
}

impl<'de> Deserialize<'de> for WorkerMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(WorkerMessageVisitor)
    }
}

struct WorkerMessageVisitor;

impl<'de> Visitor<'de> for WorkerMessageVisitor {
    type Value = WorkerMessage;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a RapidLM remote-worker message")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut fields = serde_json::Map::new();
        while let Some(key) = access.next_key::<String>()? {
            reject_reserved_field(&key).map_err(de::Error::custom)?;
            if fields.contains_key(&key) {
                return Err(de::Error::custom(RemoteWorkerError::DuplicateField));
            }
            let value: serde_json::Value = access.next_value()?;
            fields.insert(key, value);
        }
        decode_message(fields).map_err(de::Error::custom)
    }
}

fn serialize_header<S: SerializeStruct>(
    state: &mut S,
    protocol: ProtocolVersion,
    kind: WorkerMessageKind,
    trace_id: TraceId,
) -> Result<(), S::Error> {
    state.serialize_field("schema", REMOTE_WORKER_SCHEMA)?;
    state.serialize_field("protocol", &protocol)?;
    state.serialize_field("kind", &kind)?;
    state.serialize_field("trace_id", &trace_id)?;
    Ok(())
}

fn serialize_registration<S: Serializer>(
    m: &WorkerRegistration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut state = serializer.serialize_struct("WorkerMessage", 9)?;
    serialize_header(
        &mut state,
        m.protocol,
        WorkerMessageKind::Registration,
        m.trace_id,
    )?;
    state.serialize_field("worker_id", &m.worker_id)?;
    state.serialize_field("platform", &m.platform)?;
    state.serialize_field("arch", &m.arch)?;
    state.serialize_field("nonce", &m.nonce)?;
    state.serialize_field("attestation", &m.attestation)?;
    state.end()
}

fn serialize_capabilities<S: Serializer>(
    m: &WorkerCapabilitiesMessage,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut state = serializer.serialize_struct("WorkerMessage", 5)?;
    serialize_header(
        &mut state,
        m.protocol,
        WorkerMessageKind::Capabilities,
        m.trace_id,
    )?;
    state.serialize_field("capabilities", &m.capabilities)?;
    state.end()
}

fn serialize_work_lease<S: Serializer>(m: &WorkLease, serializer: S) -> Result<S::Ok, S::Error> {
    let mut state = serializer.serialize_struct("WorkerMessage", 16)?;
    serialize_header(
        &mut state,
        m.protocol,
        WorkerMessageKind::WorkLease,
        m.trace_id,
    )?;
    state.serialize_field("lease_id", &m.lease_id)?;
    state.serialize_field("worker_id", &m.worker_id)?;
    state.serialize_field("controller_id", &m.controller_id)?;
    state.serialize_field("job_id", &m.job_id)?;
    state.serialize_field("expires_at_unix_ms", &m.expires_at_unix_ms)?;
    state.serialize_field("cancel_deadline_unix_ms", &m.cancel_deadline_unix_ms)?;
    state.serialize_field("task_digest", &m.task_digest)?;
    state.serialize_field("input_artifacts", &m.input_artifacts)?;
    state.serialize_field("sandbox_spec", &m.sandbox_spec)?;
    state.serialize_field("capability_limits", &m.capability_limits)?;
    state.serialize_field("nonce", &m.nonce)?;
    state.serialize_field("signature", &m.signature)?;
    state.end()
}

fn serialize_progress<S: Serializer>(m: &WorkProgress, serializer: S) -> Result<S::Ok, S::Error> {
    let mut state = serializer.serialize_struct("WorkerMessage", 9)?;
    serialize_header(
        &mut state,
        m.protocol,
        WorkerMessageKind::Progress,
        m.trace_id,
    )?;
    state.serialize_field("lease_id", &m.lease_id)?;
    state.serialize_field("seq", &m.seq)?;
    state.serialize_field("phase", &m.phase)?;
    state.serialize_field("percent", &m.percent)?;
    state.serialize_field("artifact", &m.artifact)?;
    state.end()
}

fn serialize_result<S: Serializer>(m: &WorkResult, serializer: S) -> Result<S::Ok, S::Error> {
    let mut state = serializer.serialize_struct("WorkerMessage", 10)?;
    serialize_header(
        &mut state,
        m.protocol,
        WorkerMessageKind::Result,
        m.trace_id,
    )?;
    state.serialize_field("lease_id", &m.lease_id)?;
    state.serialize_field("status", &m.status)?;
    state.serialize_field("task_digest", &m.task_digest)?;
    state.serialize_field("artifact_outputs", &m.artifact_outputs)?;
    state.serialize_field("resource_usage", &m.resource_usage)?;
    state.serialize_field("worker_attestation", &m.worker_attestation)?;
    state.end()
}

fn serialize_cancel<S: Serializer>(m: &WorkCancel, serializer: S) -> Result<S::Ok, S::Error> {
    let mut state = serializer.serialize_struct("WorkerMessage", 9)?;
    serialize_header(
        &mut state,
        m.protocol,
        WorkerMessageKind::Cancel,
        m.trace_id,
    )?;
    state.serialize_field("lease_id", &m.lease_id)?;
    state.serialize_field("controller_id", &m.controller_id)?;
    state.serialize_field("reason", &m.reason)?;
    state.serialize_field("issued_at_unix_ms", &m.issued_at_unix_ms)?;
    state.serialize_field("deadline_unix_ms", &m.deadline_unix_ms)?;
    state.end()
}

fn decode_message(
    mut fields: serde_json::Map<String, serde_json::Value>,
) -> Result<WorkerMessage, RemoteWorkerError> {
    let schema = take_string(&mut fields, "schema")?;
    if schema != REMOTE_WORKER_SCHEMA {
        return Err(RemoteWorkerError::SchemaMismatch);
    }
    let protocol = take_protocol(&mut fields, "protocol")?;
    protocol.negotiate()?;
    let kind = WorkerMessageKind::from_str(&take_string(&mut fields, "kind")?)
        .map_err(|_| RemoteWorkerError::UnknownKind)?;
    let trace_id = take_parse::<TraceId>(&mut fields, "trace_id")?;
    let message = match kind {
        WorkerMessageKind::Registration => {
            let worker_id = take_parse::<WorkerId>(&mut fields, "worker_id")?;
            let platform = take_enum(&mut fields, "platform")?;
            let arch = take_enum(&mut fields, "arch")?;
            let nonce = take_parse::<LeaseNonce>(&mut fields, "nonce")?;
            let attestation = take_json(&mut fields, "attestation")?;
            WorkerMessage::Registration(WorkerRegistration::new(
                protocol,
                trace_id,
                worker_id,
                platform,
                arch,
                nonce,
                attestation,
            )?)
        }
        WorkerMessageKind::Capabilities => {
            let capabilities = take_json(&mut fields, "capabilities")?;
            WorkerMessage::Capabilities(WorkerCapabilitiesMessage::new(
                protocol,
                trace_id,
                capabilities,
            )?)
        }
        WorkerMessageKind::WorkLease => {
            let lease_id = take_parse::<WorkLeaseId>(&mut fields, "lease_id")?;
            let worker_id = take_parse::<WorkerId>(&mut fields, "worker_id")?;
            let controller_id = take_parse::<ControllerId>(&mut fields, "controller_id")?;
            let job_id = take_parse::<JobId>(&mut fields, "job_id")?;
            let expires_at_unix_ms = take_u64(&mut fields, "expires_at_unix_ms")?;
            let cancel_deadline_unix_ms = take_u64(&mut fields, "cancel_deadline_unix_ms")?;
            let task_digest = take_parse::<ArtifactId>(&mut fields, "task_digest")?;
            let input_artifacts = take_json(&mut fields, "input_artifacts")?;
            let sandbox_spec = take_json(&mut fields, "sandbox_spec")?;
            let capability_limits = take_json(&mut fields, "capability_limits")?;
            let nonce = take_parse::<LeaseNonce>(&mut fields, "nonce")?;
            let signature = take_parse::<LeaseSignature>(&mut fields, "signature")?;
            WorkerMessage::WorkLease(WorkLease::new(
                protocol,
                trace_id,
                lease_id,
                worker_id,
                controller_id,
                job_id,
                expires_at_unix_ms,
                cancel_deadline_unix_ms,
                task_digest,
                input_artifacts,
                sandbox_spec,
                capability_limits,
                nonce,
                signature,
            )?)
        }
        WorkerMessageKind::Progress => {
            let lease_id = take_parse::<WorkLeaseId>(&mut fields, "lease_id")?;
            let seq = take_u64(&mut fields, "seq")?;
            let phase = take_enum(&mut fields, "phase")?;
            let percent = take_opt_u8(&mut fields, "percent")?;
            let artifact = take_opt_json(&mut fields, "artifact")?;
            WorkerMessage::Progress(WorkProgress::new(
                protocol, trace_id, lease_id, seq, phase, percent, artifact,
            )?)
        }
        WorkerMessageKind::Result => {
            let lease_id = take_parse::<WorkLeaseId>(&mut fields, "lease_id")?;
            let status = take_enum(&mut fields, "status")?;
            let task_digest = take_parse::<ArtifactId>(&mut fields, "task_digest")?;
            let artifact_outputs = take_json(&mut fields, "artifact_outputs")?;
            let resource_usage = take_json(&mut fields, "resource_usage")?;
            let worker_attestation = take_json(&mut fields, "worker_attestation")?;
            WorkerMessage::Result(WorkResult::new(
                protocol,
                trace_id,
                lease_id,
                status,
                task_digest,
                artifact_outputs,
                resource_usage,
                worker_attestation,
            )?)
        }
        WorkerMessageKind::Cancel => {
            let lease_id = take_parse::<WorkLeaseId>(&mut fields, "lease_id")?;
            let controller_id = take_parse::<ControllerId>(&mut fields, "controller_id")?;
            let reason = take_enum(&mut fields, "reason")?;
            let issued_at_unix_ms = take_u64(&mut fields, "issued_at_unix_ms")?;
            let deadline_unix_ms = take_u64(&mut fields, "deadline_unix_ms")?;
            WorkerMessage::Cancel(WorkCancel::new(
                protocol,
                trace_id,
                lease_id,
                controller_id,
                reason,
                issued_at_unix_ms,
                deadline_unix_ms,
            )?)
        }
    };
    if !fields.is_empty() {
        let leftover = fields.keys().next().cloned().unwrap_or_default();
        reject_reserved_field(&leftover)?;
        return Err(RemoteWorkerError::UnknownKind);
    }
    Ok(message)
}

fn take_value(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<serde_json::Value, RemoteWorkerError> {
    fields.remove(key).ok_or(RemoteWorkerError::MissingField)
}

fn take_string(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<String, RemoteWorkerError> {
    match take_value(fields, key)? {
        serde_json::Value::String(value) => Ok(value),
        _ => Err(RemoteWorkerError::InvalidId),
    }
}

fn take_parse<T: FromStr>(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<T, RemoteWorkerError> {
    take_string(fields, key)?
        .parse()
        .map_err(|_| RemoteWorkerError::InvalidId)
}

fn take_enum<T: FromStr>(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<T, RemoteWorkerError> {
    take_string(fields, key)?
        .parse()
        .map_err(|_| RemoteWorkerError::UnknownKind)
}

fn take_u64(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<u64, RemoteWorkerError> {
    match take_value(fields, key)? {
        serde_json::Value::Number(n) => n.as_u64().ok_or(RemoteWorkerError::TimeoutInvalid),
        _ => Err(RemoteWorkerError::TimeoutInvalid),
    }
}

fn take_opt_u8(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<u8>, RemoteWorkerError> {
    match fields.remove(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => {
            let value = n.as_u64().ok_or(RemoteWorkerError::PercentInvalid)?;
            if value > 100 {
                return Err(RemoteWorkerError::PercentInvalid);
            }
            Ok(Some(value as u8))
        }
        Some(_) => Err(RemoteWorkerError::PercentInvalid),
    }
}

fn take_protocol(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<ProtocolVersion, RemoteWorkerError> {
    take_json(fields, key)
}

fn take_json<T: for<'de> Deserialize<'de>>(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<T, RemoteWorkerError> {
    let value = take_value(fields, key)?;
    serde_json::from_value(value).map_err(|_| RemoteWorkerError::SchemaMismatch)
}

fn take_opt_json<T: for<'de> Deserialize<'de>>(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<T>, RemoteWorkerError> {
    match fields.remove(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value)
            .map(Some)
            .map_err(|_| RemoteWorkerError::SchemaMismatch),
    }
}

fn known_major(major: u16) -> bool {
    major == REMOTE_WORKER_PROTOCOL_MAJOR
}

fn reject_reserved_field(field: &str) -> Result<(), RemoteWorkerError> {
    let lower = field.to_ascii_lowercase();
    if HOST_AUTHORITY_FIELDS.contains(&lower.as_str()) {
        return Err(RemoteWorkerError::HostPathForbidden);
    }
    if SECRET_FIELDS.contains(&lower.as_str()) {
        return Err(RemoteWorkerError::HostPathForbidden);
    }
    Ok(())
}

fn validate_resources(
    cpu_millis: u32,
    memory_mb: u32,
    pids: u32,
    timeout_ms: u32,
    output_bytes: u64,
) -> Result<(), RemoteWorkerError> {
    if cpu_millis == 0 || cpu_millis > MAX_CPU_MILLIS {
        return Err(RemoteWorkerError::ResourceLimit);
    }
    if memory_mb == 0 || memory_mb > MAX_MEMORY_MB {
        return Err(RemoteWorkerError::ResourceLimit);
    }
    if pids == 0 || pids > MAX_PIDS {
        return Err(RemoteWorkerError::ResourceLimit);
    }
    if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
        return Err(RemoteWorkerError::TimeoutInvalid);
    }
    if output_bytes == 0 || output_bytes > MAX_OUTPUT_BYTES {
        return Err(RemoteWorkerError::ResourceLimit);
    }
    Ok(())
}

fn validate_artifact(artifact: &ArtifactRef) -> Result<(), RemoteWorkerError> {
    if looks_like_host_path(&artifact.media_type) {
        return Err(RemoteWorkerError::HostPathForbidden);
    }
    if artifact.media_type.is_empty() || artifact.bytes == 0 {
        return Err(RemoteWorkerError::InvalidArtifact);
    }
    Ok(())
}

fn validate_guest_path(path: &RepoPath) -> Result<(), RemoteWorkerError> {
    if looks_like_host_path(path.as_str()) || is_docker_socket(path.as_str()) {
        return Err(RemoteWorkerError::HostPathForbidden);
    }
    if is_forbidden_guest(path.as_str()) {
        return Err(RemoteWorkerError::ForbiddenGuestPath);
    }
    Ok(())
}

fn validate_label_part(value: &str) -> Result<(), RemoteWorkerError> {
    if value.is_empty() || value.len() > MAX_LABEL_BYTES {
        return Err(RemoteWorkerError::BoundExceeded);
    }
    if looks_like_host_path(value) || is_docker_socket(value) {
        return Err(RemoteWorkerError::HostPathForbidden);
    }
    Ok(())
}

fn looks_like_host_path(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return true;
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.contains("/home/")
        || lower.contains("/users/")
        || lower.contains("/etc/")
        || lower.contains("/var/run/docker")
        || lower.contains("docker.sock")
        || lower.contains("\\\\")
}

fn is_forbidden_guest(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    FORBIDDEN_GUEST_PREFIXES.iter().any(|prefix| {
        lower == *prefix
            || (lower.starts_with(prefix) && lower.as_bytes().get(prefix.len()) == Some(&b'/'))
    })
}

fn is_docker_socket(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let trimmed = lower.trim_end_matches('/');
    trimmed == "docker.sock" || trimmed.ends_with("/docker.sock")
}

fn has_duplicate<T: PartialEq>(items: &[T]) -> bool {
    items
        .iter()
        .enumerate()
        .any(|(i, item)| items[i + 1..].iter().any(|other| other == item))
}

fn has_duplicate_classes(items: &[CapabilityClass]) -> bool {
    has_duplicate(items)
}

fn has_duplicate_regions(items: &[String]) -> bool {
    has_duplicate(items)
}

fn has_duplicate_label_keys(items: &[WorkerLabel]) -> bool {
    items
        .iter()
        .enumerate()
        .any(|(i, item)| items[i + 1..].iter().any(|other| other.key == item.key))
}

fn has_duplicate_attestation_class(items: &[WorkerAttestation]) -> bool {
    items
        .iter()
        .enumerate()
        .any(|(i, item)| items[i + 1..].iter().any(|other| other.class == item.class))
}

fn has_duplicate_artifact_ids(items: &[ArtifactRef]) -> bool {
    items
        .iter()
        .enumerate()
        .any(|(i, item)| items[i + 1..].iter().any(|other| other.id == item.id))
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX_TABLE[(byte >> 4) as usize] as char);
        out.push(HEX_TABLE[(byte & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex<const N: usize>(s: &str) -> Result<[u8; N], RemoteWorkerError> {
    if s.len() != N * 2 {
        return Err(RemoteWorkerError::InvalidHex);
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = (hex_nibble(bytes[i * 2])? << 4) | hex_nibble(bytes[i * 2 + 1])?;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8, RemoteWorkerError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(RemoteWorkerError::InvalidHex),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::RedactionClass;

    const WORKER: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const CONTROLLER: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ac";
    const LEASE: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";
    const TRACE: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ae";
    const JOB: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789af";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    const SIGNATURE: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const GOLDEN_LEASE: &str = r#"{"schema":"rapidlm.remote_worker","protocol":{"major":1,"minor":0},"kind":"work_lease","trace_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ae","lease_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ad","worker_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","controller_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ac","job_id":"018f3c8a-7e2b-7a10-8c4d-0123456789af","expires_at_unix_ms":1700000060000,"cancel_deadline_unix_ms":1700000045000,"task_digest":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","input_artifacts":[{"id":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","media_type":"application/vnd.rapidlm.snapshot","bytes":12,"redaction":"project"}],"sandbox_spec":{"tier":"remote-worker","image":{"id":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","media_type":"application/vnd.rapidlm.rootfs","bytes":3,"redaction":"project"},"cwd":"src","network":"deny","cpu_millis":1000,"memory_mb":256,"pids":64,"timeout_ms":30000,"output_limit":1048576},"capability_limits":{"classes":["fs.read","proc.exec"],"max_cpu_millis":2000,"max_memory_mb":512,"max_pids":128,"max_timeout_ms":60000,"max_output_bytes":2097152,"network":"deny"},"nonce":"0123456789abcdef0123456789abcdef","signature":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"}"#;

    fn worker_id() -> WorkerId {
        WORKER.parse().expect("worker")
    }

    fn controller_id() -> ControllerId {
        CONTROLLER.parse().expect("controller")
    }

    fn lease_id() -> WorkLeaseId {
        LEASE.parse().expect("lease")
    }

    fn trace_id() -> TraceId {
        TRACE.parse().expect("trace")
    }

    fn job_id() -> JobId {
        JOB.parse().expect("job")
    }

    fn nonce() -> LeaseNonce {
        NONCE.parse().expect("nonce")
    }

    fn signature() -> LeaseSignature {
        SIGNATURE.parse().expect("signature")
    }

    fn rootfs() -> ArtifactRef {
        ArtifactRef::new(
            ArtifactId::from_bytes(b"abc"),
            "application/vnd.rapidlm.rootfs",
            3,
            RedactionClass::Project,
        )
    }

    fn snapshot() -> ArtifactRef {
        ArtifactRef::new(
            ArtifactId::from_bytes(b""),
            "application/vnd.rapidlm.snapshot",
            12,
            RedactionClass::Project,
        )
    }

    fn spec() -> WorkerSandboxSpec {
        WorkerSandboxSpec::new(
            SandboxTier::RemoteWorker,
            rootfs(),
            RepoPath::parse("src").expect("cwd"),
            NetworkMode::Deny,
            1_000,
            256,
            64,
            30_000,
            1_048_576,
        )
        .expect("spec")
    }

    fn limits() -> CapabilityLimits {
        CapabilityLimits::new(
            vec![CapabilityClass::FsRead, CapabilityClass::ProcExec],
            2_000,
            512,
            128,
            60_000,
            2_097_152,
            NetworkMode::Deny,
        )
        .expect("limits")
    }

    fn lease() -> WorkLease {
        WorkLease::new(
            ProtocolVersion::V1,
            trace_id(),
            lease_id(),
            worker_id(),
            controller_id(),
            job_id(),
            1_700_000_060_000,
            1_700_000_045_000,
            ArtifactId::from_bytes(b"abc"),
            vec![snapshot()],
            spec(),
            limits(),
            nonce(),
            signature(),
        )
        .expect("lease")
    }

    fn attestation() -> WorkerAttestation {
        WorkerAttestation::new(
            WorkerAttestationClass::Platform,
            ArtifactId::from_bytes(b"abc"),
        )
    }

    fn capabilities() -> WorkerCapabilities {
        WorkerCapabilities::new(
            worker_id(),
            WorkerPlatform::Linux,
            WorkerArch::X64,
            vec![SandboxTier::RemoteWorker, SandboxTier::Gvisor],
            vec![SandboxTier::RemoteWorker],
            vec![DeviceCapability::Desktop],
            vec![ComputerUseSurface::Browser, ComputerUseSurface::Tui],
            false,
            vec![ProtocolVersion::V1],
            None,
            vec![WorkerLabel::new("role", "build").expect("label")],
            WorkerHealth::Healthy,
            vec!["us".to_owned()],
            vec![attestation()],
        )
        .expect("capabilities")
    }

    #[test]
    fn negotiate_rejects_unknown_and_incompatible_majors() {
        assert_eq!(
            ProtocolVersion::new(2, 0).negotiate(),
            Err(RemoteWorkerError::UnknownMajor { offered: 2 })
        );
        assert_eq!(
            ProtocolVersion::new(0, 1).negotiate(),
            Err(RemoteWorkerError::UnknownMajor { offered: 0 })
        );
        assert_eq!(
            ProtocolVersion::new(99, 4).negotiate(),
            Err(RemoteWorkerError::UnknownMajor { offered: 99 })
        );
        assert_eq!(
            ProtocolVersion::new(1, 9).negotiate(),
            Ok(ProtocolVersion::V1)
        );
        assert_eq!(ProtocolVersion::V1.negotiate(), Ok(ProtocolVersion::V1));
    }

    #[test]
    fn work_lease_golden_round_trips_without_host_paths() {
        let message = WorkerMessage::WorkLease(lease());
        let json = serde_json::to_string(&message).expect("serialize");
        assert_eq!(json, GOLDEN_LEASE);
        let decoded: WorkerMessage = serde_json::from_str(GOLDEN_LEASE).expect("deserialize");
        assert_eq!(decoded, message);
        assert!(!json.contains("host_path"));
        assert!(!json.contains("/etc/"));
        assert!(!json.contains("/Users/"));
        match decoded {
            WorkerMessage::WorkLease(lease) => {
                assert_eq!(lease.worker_id(), worker_id());
                assert_eq!(lease.controller_id(), controller_id());
                assert_eq!(lease.expires_at_unix_ms(), 1_700_000_060_000);
                assert_eq!(lease.task_digest(), ArtifactId::from_bytes(b"abc"));
                assert_eq!(lease.input_artifacts().len(), 1);
                assert_eq!(lease.sandbox_spec().tier(), SandboxTier::RemoteWorker);
                assert_eq!(lease.capability_limits().max_memory_mb(), 512);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn every_message_kind_round_trips() {
        let registration = WorkerMessage::Registration(
            WorkerRegistration::new(
                ProtocolVersion::V1,
                trace_id(),
                worker_id(),
                WorkerPlatform::Linux,
                WorkerArch::X64,
                nonce(),
                attestation(),
            )
            .expect("registration"),
        );
        let capabilities = WorkerMessage::Capabilities(
            WorkerCapabilitiesMessage::new(ProtocolVersion::V1, trace_id(), capabilities())
                .expect("capabilities"),
        );
        let progress = WorkerMessage::Progress(
            WorkProgress::new(
                ProtocolVersion::V1,
                trace_id(),
                lease_id(),
                3,
                WorkPhase::Running,
                Some(40),
                Some(snapshot()),
            )
            .expect("progress"),
        );
        let result = WorkerMessage::Result(
            WorkResult::new(
                ProtocolVersion::V1,
                trace_id(),
                lease_id(),
                WorkStatus::Succeeded,
                ArtifactId::from_bytes(b"abc"),
                vec![snapshot()],
                ResourceUsage::new(10, 32, 4, 128).expect("usage"),
                attestation(),
            )
            .expect("result"),
        );
        let cancel = WorkerMessage::Cancel(
            WorkCancel::new(
                ProtocolVersion::V1,
                trace_id(),
                lease_id(),
                controller_id(),
                WorkCancelReason::ControllerRequested,
                1_700_000_000_000,
                1_700_000_001_000,
            )
            .expect("cancel"),
        );
        for message in [
            registration,
            capabilities,
            WorkerMessage::WorkLease(lease()),
            progress,
            result,
            cancel,
        ] {
            let json = serde_json::to_string(&message).expect("serialize");
            let decoded: WorkerMessage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, message);
            assert_eq!(decoded.protocol(), ProtocolVersion::V1);
            assert!(!json.contains("host_path"));
            assert!(!json.contains("plaintext"));
            assert!(!json.contains("stdout"));
        }
    }

    #[test]
    fn unknown_major_on_the_wire_fails_closed() {
        let json = GOLDEN_LEASE.replace(
            r#""protocol":{"major":1,"minor":0}"#,
            r#""protocol":{"major":2,"minor":0}"#,
        );
        let err = serde_json::from_str::<WorkerMessage>(&json).expect_err("major 2");
        assert!(
            err.to_string()
                .contains("unknown remote-worker protocol major 2")
        );
    }

    #[test]
    fn host_path_authority_cannot_enter_the_protocol() {
        let mut value: serde_json::Value = serde_json::from_str(GOLDEN_LEASE).expect("json");
        value["host_path"] = serde_json::Value::String("/etc/passwd".into());
        assert!(serde_json::from_value::<WorkerMessage>(value).is_err());

        let abs = GOLDEN_LEASE.replace(r#""cwd":"src""#, r#""cwd":"/etc/passwd""#);
        assert!(serde_json::from_str::<WorkerMessage>(&abs).is_err());

        let win = GOLDEN_LEASE.replace(r#""cwd":"src""#, r#""cwd":"C:\\\\Windows\\\\System32""#);
        assert!(serde_json::from_str::<WorkerMessage>(&win).is_err());

        assert_eq!(
            WorkerSandboxSpec::new(
                SandboxTier::RemoteWorker,
                rootfs(),
                RepoPath::parse("etc/passwd").expect("rel"),
                NetworkMode::Deny,
                1_000,
                256,
                64,
                30_000,
                1_048_576,
            )
            .expect_err("guest etc"),
            RemoteWorkerError::ForbiddenGuestPath
        );
        assert_eq!(
            WorkerLabel::new("root", "/Users/admin/.ssh").expect_err("label"),
            RemoteWorkerError::HostPathForbidden
        );
    }

    #[test]
    fn worker_cannot_smuggle_capability_grants_or_identity_override() {
        let mut lease_json: serde_json::Value = serde_json::from_str(GOLDEN_LEASE).expect("json");
        lease_json["capability_grants"] = serde_json::json!(["fs.write", "secret.use"]);
        assert!(serde_json::from_value::<WorkerMessage>(lease_json).is_err());

        let result = WorkerMessage::Result(
            WorkResult::new(
                ProtocolVersion::V1,
                trace_id(),
                lease_id(),
                WorkStatus::Succeeded,
                ArtifactId::from_bytes(b"abc"),
                vec![snapshot()],
                ResourceUsage::new(10, 32, 4, 128).expect("usage"),
                attestation(),
            )
            .expect("result"),
        );
        let mut result_json = serde_json::to_value(&result).expect("value");
        result_json["worker_id"] = serde_json::Value::String(CONTROLLER.into());
        assert!(serde_json::from_value::<WorkerMessage>(result_json).is_err());

        let mut stdout = serde_json::to_value(&result).expect("value");
        stdout["stdout"] = serde_json::Value::String("secret-token".into());
        assert!(serde_json::from_value::<WorkerMessage>(stdout).is_err());
    }

    #[test]
    fn expiry_and_cancel_are_mandatory() {
        assert_eq!(
            WorkLease::new(
                ProtocolVersion::V1,
                trace_id(),
                lease_id(),
                worker_id(),
                controller_id(),
                job_id(),
                0,
                1_700_000_045_000,
                ArtifactId::from_bytes(b"abc"),
                vec![snapshot()],
                spec(),
                limits(),
                nonce(),
                signature(),
            )
            .expect_err("expiry"),
            RemoteWorkerError::ExpiryRequired
        );
        assert_eq!(
            WorkLease::new(
                ProtocolVersion::V1,
                trace_id(),
                lease_id(),
                worker_id(),
                controller_id(),
                job_id(),
                1_700_000_060_000,
                0,
                ArtifactId::from_bytes(b"abc"),
                vec![snapshot()],
                spec(),
                limits(),
                nonce(),
                signature(),
            )
            .expect_err("cancel"),
            RemoteWorkerError::CancelDeadlineRequired
        );
        assert_eq!(
            WorkLease::new(
                ProtocolVersion::V1,
                trace_id(),
                lease_id(),
                worker_id(),
                controller_id(),
                job_id(),
                1_700_000_045_000,
                1_700_000_060_000,
                ArtifactId::from_bytes(b"abc"),
                vec![snapshot()],
                spec(),
                limits(),
                nonce(),
                signature(),
            )
            .expect_err("order"),
            RemoteWorkerError::CancelDeadlineAfterExpiry
        );
    }

    #[test]
    fn ios_capability_requires_macos() {
        let err = WorkerCapabilities::new(
            worker_id(),
            WorkerPlatform::Linux,
            WorkerArch::X64,
            vec![SandboxTier::RemoteWorker],
            vec![SandboxTier::RemoteWorker],
            vec![DeviceCapability::IosSimulator],
            vec![ComputerUseSurface::Ios],
            true,
            vec![ProtocolVersion::V1],
            None,
            Vec::new(),
            WorkerHealth::Healthy,
            vec!["us".to_owned()],
            vec![attestation()],
        )
        .expect_err("ios");
        assert_eq!(err, RemoteWorkerError::CapabilityInconsistent);
    }

    #[test]
    fn sandbox_spec_cannot_exceed_capability_limits() {
        let wide = WorkerSandboxSpec::new(
            SandboxTier::RemoteWorker,
            rootfs(),
            RepoPath::parse("src").expect("cwd"),
            NetworkMode::Allow,
            4_000,
            256,
            64,
            30_000,
            1_048_576,
        )
        .expect("wide");
        let err = WorkLease::new(
            ProtocolVersion::V1,
            trace_id(),
            lease_id(),
            worker_id(),
            controller_id(),
            job_id(),
            1_700_000_060_000,
            1_700_000_045_000,
            ArtifactId::from_bytes(b"abc"),
            vec![snapshot()],
            wide,
            limits(),
            nonce(),
            signature(),
        )
        .expect_err("limit");
        assert_eq!(err, RemoteWorkerError::ResourceLimit);
    }

    #[test]
    fn zero_nonce_and_signature_are_rejected() {
        assert_eq!(
            LeaseNonce::from_bytes([0; 16]),
            Err(RemoteWorkerError::ZeroNonce)
        );
        assert_eq!(
            LeaseSignature::from_bytes([0; 32]),
            Err(RemoteWorkerError::ZeroSignature)
        );
        assert!(
            "00112233445566778899AABBCCDDEEFF00112233445566778899aabbccddeeff"
                .parse::<LeaseSignature>()
                .is_err()
        );
    }

    #[test]
    fn secret_and_stdout_fields_are_rejected() {
        let mut json: serde_json::Value = serde_json::from_str(GOLDEN_LEASE).expect("json");
        json["secret"] = serde_json::Value::String("super-secret-password".into());
        assert!(serde_json::from_value::<WorkerMessage>(json).is_err());
    }
}
