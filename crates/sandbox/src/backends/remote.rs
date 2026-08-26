//! Controller-side Firecracker-class remote microVM protocol.
//!
//! The worker receives a signed [`RemoteWorkLease`] whose [`RemoteSandboxSpec`]
//! names content-addressed snapshot/artifact refs only. Host path authority is
//! never encoded. Expiry and cancellation deadlines are mandatory. Jailer/KVM
//! hardening belongs to the worker; this crate issues and verifies the lease.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use capability_broker::{CancellationToken, Capability, CapabilityLease};
use protocol::{
    ArtifactId, ArtifactRef, LeaseId, RedactionClass, RepoPath, RuntimeId, SandboxTier,
};

use crate::backend::{
    BackendHealth, HealthReason, MAX_MEMORY_MB, MAX_MOUNTS, MAX_OUTPUT_BYTES, MAX_PIDS,
    MAX_TIMEOUT, MountCapability, MountMode, NetworkCapability, ResourceCapability, SandboxBackend,
    SandboxCapabilities, SandboxError, SandboxExecRequest, SandboxExecResult, SandboxHandle,
    SandboxId, SandboxNetwork, SandboxSpec, supports_spec,
};

/// Maximum prepared remote sandboxes retained by one backend.
pub const MAX_LIVE_REMOTE_SANDBOXES: usize = 64;

/// Wire schema name for [`RemoteSandboxSpec`].
pub const REMOTE_SANDBOX_SPEC_SCHEMA: &str = "rapidlm.remote_sandbox_spec";

/// v1 schema version for [`RemoteSandboxSpec`].
pub const REMOTE_SANDBOX_SPEC_SCHEMA_VERSION: u16 = 1;

/// Wire schema name for [`RemoteWorkLease`].
pub const REMOTE_WORK_LEASE_SCHEMA: &str = "rapidlm.remote_work_lease";

/// v1 schema version for [`RemoteWorkLease`].
pub const REMOTE_WORK_LEASE_SCHEMA_VERSION: u16 = 1;

/// Maximum required/presented worker attestations on one spec or profile.
pub const MAX_ATTESTATIONS: usize = 16;

const BINDING_TAG: &[u8] = b"rapidlm.remote_work_lease.v1";
const TASK_TAG: &[u8] = b"rapidlm.remote_task.v1";
const HMAC_BLOCK: usize = 64;
const REMOTE_VERSION: &str = "remote-microvm";

const SNAPSHOT_MEDIA: &str = "application/vnd.rapidlm.snapshot";
const ARTIFACT_MEDIA: &str = "application/vnd.rapidlm.artifact";
const ROOTFS_MEDIA: &str = "application/vnd.rapidlm.rootfs";

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

/// Controller identity bound into a work lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ControllerId(RuntimeId);

/// Remote worker identity bound into a work lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct WorkerId(RuntimeId);

/// Work-lease identity. Distinct from an in-process [`LeaseId`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct WorkLeaseId(RuntimeId);

/// Platform that may host a Firecracker-class microVM.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WorkerPlatform {
    LinuxKvm,
}

/// Attestation class the worker must present before a lease is issued.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WorkerAttestationClass {
    Platform,
    Firecracker,
    Jailer,
    Rootfs,
}

/// Content-addressed attestation requirement or presented statement.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct WorkerAttestation {
    class: WorkerAttestationClass,
    digest: ArtifactId,
}

/// Source of a guest mount. Never a host path.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum RemoteMountSource {
    Artifact(ArtifactRef),
    Temp,
}

/// Guest mount policy. Artifact sources carry an immutable content hash.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RemoteMount {
    source: RemoteMountSource,
    target: RepoPath,
    mode: MountMode,
}

/// Immutable snapshot/artifact input referenced by hash.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RemoteInput {
    artifact: ArtifactRef,
    target: RepoPath,
    mode: MountMode,
}

/// Controller-side Firecracker sandbox spec sent to a remote worker.
#[derive(Clone, Eq, PartialEq)]
pub struct RemoteSandboxSpec {
    image: ArtifactRef,
    inputs: Vec<RemoteInput>,
    mounts: Vec<RemoteMount>,
    cwd: RepoPath,
    network: SandboxNetwork,
    cpu_millis: u32,
    memory_mb: u32,
    pids: u32,
    timeout: Duration,
    output_limit: u64,
    required_attestations: Vec<WorkerAttestation>,
    expires_at_unix_ms: u64,
    cancel_deadline_unix_ms: u64,
    secret_count: usize,
}

/// Incremental constructor. [`RemoteSandboxSpecBuilder::build`] validates bounds.
pub struct RemoteSandboxSpecBuilder {
    image: Option<ArtifactRef>,
    inputs: Vec<RemoteInput>,
    mounts: Vec<RemoteMount>,
    cwd: Option<RepoPath>,
    network: SandboxNetwork,
    cpu_millis: u32,
    memory_mb: u32,
    pids: u32,
    timeout: Duration,
    output_limit: u64,
    required_attestations: Vec<WorkerAttestation>,
    expires_at: Option<SystemTime>,
    cancel_deadline: Option<SystemTime>,
    secret_count: usize,
}

/// MAC key used to sign controller-issued work leases.
pub struct WorkLeaseIssuer {
    key: [u8; 32],
}

/// Controller and worker identities bound into a work lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct RemoteLeaseParties {
    controller_id: ControllerId,
    worker_id: WorkerId,
}

/// Signed work lease. Capability-lease tokens and host paths are absent.
#[derive(Clone, Eq, PartialEq)]
pub struct RemoteWorkLease {
    lease_id: WorkLeaseId,
    controller_id: ControllerId,
    worker_id: WorkerId,
    capability_lease_id: LeaseId,
    task_digest: ArtifactId,
    spec: RemoteSandboxSpec,
    expires_at_unix_ms: u64,
    cancel_deadline_unix_ms: u64,
    nonce: [u8; 16],
    signature: [u8; 32],
}

/// Advertised Firecracker-class worker. Incomplete profiles are not healthy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerProfile {
    id: WorkerId,
    platform: WorkerPlatform,
    attestations: Vec<WorkerAttestation>,
}

/// Remote/Firecracker backend. Isolation is microVM; no silent downgrade.
pub struct RemoteBackend {
    caps: SandboxCapabilities,
    issuer: WorkLeaseIssuer,
    controller_id: ControllerId,
    profile: Option<WorkerProfile>,
    sessions: Mutex<HashMap<SandboxId, PreparedSession>>,
}

struct PreparedSession {
    handle: SandboxHandle,
    capability_lease_id: LeaseId,
    work_lease: RemoteWorkLease,
}

impl ControllerId {
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

impl WorkerId {
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

impl WorkLeaseId {
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

impl WorkerPlatform {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinuxKvm => "linux-kvm",
        }
    }

    pub const fn hosts_firecracker(self) -> bool {
        matches!(self, Self::LinuxKvm)
    }
}

impl WorkerAttestationClass {
    pub const ALL: &'static [Self] = &[
        Self::Platform,
        Self::Firecracker,
        Self::Jailer,
        Self::Rootfs,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::Firecracker => "firecracker",
            Self::Jailer => "jailer",
            Self::Rootfs => "rootfs",
        }
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

impl RemoteInput {
    pub fn new(
        artifact: ArtifactRef,
        target: RepoPath,
        mode: MountMode,
    ) -> Result<Self, SandboxError> {
        validate_artifact_ref(&artifact)?;
        validate_guest_target(&target)?;
        if matches!(mode, MountMode::Temp) {
            return Err(SandboxError::InvalidSpec);
        }
        Ok(Self {
            artifact,
            target,
            mode,
        })
    }

    pub fn artifact(&self) -> &ArtifactRef {
        &self.artifact
    }

    pub fn target(&self) -> &RepoPath {
        &self.target
    }

    pub const fn mode(&self) -> MountMode {
        self.mode
    }

    pub const fn hash(&self) -> ArtifactId {
        self.artifact.id
    }
}

impl RemoteMount {
    pub fn artifact(
        artifact: ArtifactRef,
        target: RepoPath,
        mode: MountMode,
    ) -> Result<Self, SandboxError> {
        let input = RemoteInput::new(artifact, target, mode)?;
        Ok(Self {
            source: RemoteMountSource::Artifact(input.artifact),
            target: input.target,
            mode: input.mode,
        })
    }

    pub fn temp(target: RepoPath) -> Result<Self, SandboxError> {
        validate_guest_target(&target)?;
        Ok(Self {
            source: RemoteMountSource::Temp,
            target,
            mode: MountMode::Temp,
        })
    }

    pub fn source(&self) -> &RemoteMountSource {
        &self.source
    }

    pub fn target(&self) -> &RepoPath {
        &self.target
    }

    pub const fn mode(&self) -> MountMode {
        self.mode
    }
}

impl RemoteSandboxSpec {
    pub fn builder() -> RemoteSandboxSpecBuilder {
        RemoteSandboxSpecBuilder {
            image: None,
            inputs: Vec::new(),
            mounts: Vec::new(),
            cwd: None,
            network: SandboxNetwork::None,
            cpu_millis: 1_000,
            memory_mb: 256,
            pids: 64,
            timeout: Duration::from_secs(30),
            output_limit: 1024 * 1024,
            required_attestations: Vec::new(),
            expires_at: None,
            cancel_deadline: None,
            secret_count: 0,
        }
    }

    /// Project a local spec into the remote protocol. Bind/host sources fail.
    pub fn from_sandbox_spec(
        spec: &SandboxSpec,
        inputs: impl IntoIterator<Item = RemoteInput>,
        required_attestations: impl IntoIterator<Item = WorkerAttestation>,
        expires_at: SystemTime,
        cancel_deadline: SystemTime,
        now: SystemTime,
    ) -> Result<Self, SandboxError> {
        if spec.tier() != SandboxTier::RemoteWorker {
            return Err(SandboxError::TierUnavailable);
        }
        let image = parse_rootfs_image(spec.image())?;
        let mut builder = Self::builder()
            .image(image)
            .cwd(spec.cwd().clone())
            .network(spec.network())
            .cpu_millis(spec.cpu_millis())
            .memory_mb(spec.memory_mb())
            .pids(spec.pids())
            .timeout(spec.timeout())
            .output_limit(spec.output_limit())
            .expires_at(expires_at)
            .cancel_deadline(cancel_deadline)
            .secret_count(spec.secrets().len());
        for input in inputs {
            builder = builder.input(input);
        }
        for mount in spec.mounts() {
            if mount.source().is_some() {
                return Err(SandboxError::ForbiddenMount);
            }
            if !matches!(mount.mode(), MountMode::Temp) {
                return Err(SandboxError::ForbiddenMount);
            }
            builder = builder.mount(RemoteMount::temp(mount.target().clone())?);
        }
        for attestation in required_attestations {
            builder = builder.require_attestation(attestation);
        }
        builder.build(now)
    }

    pub fn image(&self) -> &ArtifactRef {
        &self.image
    }

    pub fn inputs(&self) -> &[RemoteInput] {
        &self.inputs
    }

    pub fn mounts(&self) -> &[RemoteMount] {
        &self.mounts
    }

    pub fn cwd(&self) -> &RepoPath {
        &self.cwd
    }

    pub const fn network(&self) -> SandboxNetwork {
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

    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    pub const fn output_limit(&self) -> u64 {
        self.output_limit
    }

    pub fn required_attestations(&self) -> &[WorkerAttestation] {
        &self.required_attestations
    }

    pub const fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }

    pub const fn cancel_deadline_unix_ms(&self) -> u64 {
        self.cancel_deadline_unix_ms
    }

    pub const fn secret_count(&self) -> usize {
        self.secret_count
    }

    pub const fn tier(&self) -> SandboxTier {
        SandboxTier::RemoteWorker
    }

    /// Image digest plus every immutable input hash, in spec order.
    pub fn input_hashes(&self) -> impl Iterator<Item = ArtifactId> + '_ {
        std::iter::once(self.image.id).chain(self.inputs.iter().map(RemoteInput::hash))
    }

    pub fn canonical_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"schema\":\"");
        out.push_str(REMOTE_SANDBOX_SPEC_SCHEMA);
        out.push_str("\",\"schema_version\":");
        out.push_str(&REMOTE_SANDBOX_SPEC_SCHEMA_VERSION.to_string());
        out.push_str(",\"tier\":\"remote-worker\",\"image\":");
        push_artifact_json(&mut out, &self.image);
        out.push_str(",\"inputs\":[");
        for (i, input) in self.inputs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('{');
            push_artifact_fields(&mut out, &input.artifact);
            out.push_str(",\"target\":\"");
            push_json_str(&mut out, input.target.as_str());
            out.push_str("\",\"mode\":\"");
            out.push_str(mount_mode_str(input.mode));
            out.push_str("\"}");
        }
        out.push_str("],\"mounts\":[");
        for (i, mount) in self.mounts.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"source\":");
            match &mount.source {
                RemoteMountSource::Artifact(artifact) => push_artifact_json(&mut out, artifact),
                RemoteMountSource::Temp => out.push_str("\"temp\""),
            }
            out.push_str(",\"target\":\"");
            push_json_str(&mut out, mount.target.as_str());
            out.push_str("\",\"mode\":\"");
            out.push_str(mount_mode_str(mount.mode));
            out.push_str("\"}");
        }
        out.push_str("],\"cwd\":\"");
        push_json_str(&mut out, self.cwd.as_str());
        out.push_str("\",\"network\":\"");
        out.push_str(network_str(self.network));
        out.push_str("\",\"cpu_millis\":");
        out.push_str(&self.cpu_millis.to_string());
        out.push_str(",\"memory_mb\":");
        out.push_str(&self.memory_mb.to_string());
        out.push_str(",\"pids\":");
        out.push_str(&self.pids.to_string());
        out.push_str(",\"timeout_ms\":");
        out.push_str(&millis_of(self.timeout).to_string());
        out.push_str(",\"output_limit\":");
        out.push_str(&self.output_limit.to_string());
        out.push_str(",\"required_attestations\":[");
        for (i, attestation) in self.required_attestations.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"class\":\"");
            out.push_str(attestation.class.as_str());
            out.push_str("\",\"digest\":\"");
            out.push_str(&attestation.digest.to_string());
            out.push_str("\"}");
        }
        out.push_str("],\"expires_at_unix_ms\":");
        out.push_str(&self.expires_at_unix_ms.to_string());
        out.push_str(",\"cancel_deadline_unix_ms\":");
        out.push_str(&self.cancel_deadline_unix_ms.to_string());
        out.push_str(",\"secret_count\":");
        out.push_str(&self.secret_count.to_string());
        out.push('}');
        out
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        self.canonical_json().into_bytes()
    }
}

impl RemoteSandboxSpecBuilder {
    pub fn image(mut self, image: ArtifactRef) -> Self {
        self.image = Some(image);
        self
    }

    pub fn input(mut self, input: RemoteInput) -> Self {
        self.inputs.push(input);
        self
    }

    pub fn mount(mut self, mount: RemoteMount) -> Self {
        self.mounts.push(mount);
        self
    }

    pub fn cwd(mut self, cwd: RepoPath) -> Self {
        self.cwd = Some(cwd);
        self
    }

    pub fn network(mut self, network: SandboxNetwork) -> Self {
        self.network = network;
        self
    }

    pub fn cpu_millis(mut self, cpu_millis: u32) -> Self {
        self.cpu_millis = cpu_millis;
        self
    }

    pub fn memory_mb(mut self, memory_mb: u32) -> Self {
        self.memory_mb = memory_mb;
        self
    }

    pub fn pids(mut self, pids: u32) -> Self {
        self.pids = pids;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn output_limit(mut self, output_limit: u64) -> Self {
        self.output_limit = output_limit;
        self
    }

    pub fn require_attestation(mut self, attestation: WorkerAttestation) -> Self {
        self.required_attestations.push(attestation);
        self
    }

    pub fn expires_at(mut self, expires_at: SystemTime) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    pub fn cancel_deadline(mut self, cancel_deadline: SystemTime) -> Self {
        self.cancel_deadline = Some(cancel_deadline);
        self
    }

    pub fn secret_count(mut self, secret_count: usize) -> Self {
        self.secret_count = secret_count;
        self
    }

    pub fn build(self, now: SystemTime) -> Result<RemoteSandboxSpec, SandboxError> {
        let image = self.image.ok_or(SandboxError::Empty)?;
        validate_artifact_ref(&image)?;
        if image.media_type != ROOTFS_MEDIA {
            return Err(SandboxError::InvalidSpec);
        }
        let cwd = self.cwd.ok_or(SandboxError::Empty)?;
        validate_guest_target(&cwd)?;
        let expires_at = self.expires_at.ok_or(SandboxError::TimeoutInvalid)?;
        let cancel_deadline = self.cancel_deadline.ok_or(SandboxError::TimeoutInvalid)?;
        if self.inputs.len() > MAX_MOUNTS || self.mounts.len() > MAX_MOUNTS {
            return Err(SandboxError::TooManyMounts);
        }
        if self.required_attestations.is_empty()
            || self.required_attestations.len() > MAX_ATTESTATIONS
        {
            return Err(SandboxError::InvalidSpec);
        }
        if !has_class(
            &self.required_attestations,
            WorkerAttestationClass::Firecracker,
        ) || !has_class(
            &self.required_attestations,
            WorkerAttestationClass::Platform,
        ) {
            return Err(SandboxError::InvalidSpec);
        }
        let mut seen_targets = HashMap::new();
        for input in &self.inputs {
            validate_artifact_ref(&input.artifact)?;
            validate_guest_target(&input.target)?;
            if matches!(input.mode, MountMode::Temp) {
                return Err(SandboxError::InvalidSpec);
            }
            insert_unique_target(&mut seen_targets, input.target.as_str())?;
        }
        for mount in &self.mounts {
            validate_guest_target(&mount.target)?;
            match &mount.source {
                RemoteMountSource::Artifact(artifact) => {
                    validate_artifact_ref(artifact)?;
                    if matches!(mount.mode, MountMode::Temp) {
                        return Err(SandboxError::InvalidSpec);
                    }
                }
                RemoteMountSource::Temp => {
                    if !matches!(mount.mode, MountMode::Temp) {
                        return Err(SandboxError::InvalidSpec);
                    }
                }
            }
            insert_unique_target(&mut seen_targets, mount.target.as_str())?;
        }
        if self.cpu_millis == 0 || self.cpu_millis > crate::backend::MAX_CPU_MILLIS {
            return Err(SandboxError::ResourceLimit);
        }
        if self.memory_mb == 0 || self.memory_mb > MAX_MEMORY_MB {
            return Err(SandboxError::ResourceLimit);
        }
        if self.pids == 0 || self.pids > MAX_PIDS {
            return Err(SandboxError::ResourceLimit);
        }
        if self.timeout.is_zero() || self.timeout > MAX_TIMEOUT {
            return Err(SandboxError::TimeoutInvalid);
        }
        if self.output_limit == 0 || self.output_limit > MAX_OUTPUT_BYTES {
            return Err(SandboxError::OutputLimitInvalid);
        }
        let now_ms = unix_ms(now)?;
        let expires_ms = unix_ms(expires_at)?;
        let cancel_ms = unix_ms(cancel_deadline)?;
        if expires_ms <= now_ms || cancel_ms <= now_ms {
            return Err(SandboxError::TimeoutInvalid);
        }
        if cancel_ms > expires_ms {
            return Err(SandboxError::TimeoutInvalid);
        }
        let timeout_ms = millis_of(self.timeout);
        if now_ms.saturating_add(timeout_ms) > expires_ms {
            return Err(SandboxError::TimeoutInvalid);
        }
        if duplicate_attestation_class(&self.required_attestations) {
            return Err(SandboxError::InvalidSpec);
        }
        Ok(RemoteSandboxSpec {
            image,
            inputs: self.inputs,
            mounts: self.mounts,
            cwd,
            network: self.network,
            cpu_millis: self.cpu_millis,
            memory_mb: self.memory_mb,
            pids: self.pids,
            timeout: self.timeout,
            output_limit: self.output_limit,
            required_attestations: self.required_attestations,
            expires_at_unix_ms: expires_ms,
            cancel_deadline_unix_ms: cancel_ms,
            secret_count: self.secret_count,
        })
    }
}

impl WorkLeaseIssuer {
    pub fn from_key(key: [u8; 32]) -> Result<Self, SandboxError> {
        if key.iter().all(|b| *b == 0) {
            return Err(SandboxError::LeaseInvalid);
        }
        Ok(Self { key })
    }

    pub fn ephemeral() -> Self {
        loop {
            let mut seed = Vec::with_capacity(64);
            for _ in 0..4 {
                seed.extend_from_slice(RuntimeId::new().as_uuid().as_bytes());
            }
            let key = *ArtifactId::from_bytes(&seed).as_digest();
            if let Ok(issuer) = Self::from_key(key) {
                return issuer;
            }
        }
    }

    fn mac(&self, lease: &RemoteWorkLease) -> [u8; 32] {
        hmac_sha256(&self.key, &binding_bytes(lease))
    }
}

impl RemoteLeaseParties {
    pub const fn new(controller_id: ControllerId, worker_id: WorkerId) -> Self {
        Self {
            controller_id,
            worker_id,
        }
    }

    pub const fn controller_id(self) -> ControllerId {
        self.controller_id
    }

    pub const fn worker_id(self) -> WorkerId {
        self.worker_id
    }
}

impl RemoteWorkLease {
    pub fn issue(
        issuer: &WorkLeaseIssuer,
        spec: RemoteSandboxSpec,
        parties: RemoteLeaseParties,
        capability_lease: &CapabilityLease,
        presented: &[WorkerAttestation],
        now: SystemTime,
        cancel: &CancellationToken,
    ) -> Result<Self, SandboxError> {
        check_cancel(cancel)?;
        require_proc_lease(capability_lease)?;
        verify_required_attestations(&spec.required_attestations, presented)?;
        let now_ms = unix_ms(now)?;
        if now_ms >= spec.expires_at_unix_ms || now_ms >= spec.cancel_deadline_unix_ms {
            return Err(SandboxError::TimeoutInvalid);
        }
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(RuntimeId::new().as_uuid().as_bytes());
        let mut lease = Self {
            lease_id: WorkLeaseId::new(),
            controller_id: parties.controller_id,
            worker_id: parties.worker_id,
            capability_lease_id: capability_lease.lease_id(),
            task_digest: ArtifactId::from_bytes(&[]),
            spec,
            expires_at_unix_ms: 0,
            cancel_deadline_unix_ms: 0,
            nonce,
            signature: [0u8; 32],
        };
        lease.expires_at_unix_ms = lease.spec.expires_at_unix_ms;
        lease.cancel_deadline_unix_ms = lease.spec.cancel_deadline_unix_ms;
        lease.task_digest = task_digest(&lease);
        lease.signature = issuer.mac(&lease);
        Ok(lease)
    }

    pub fn verify(
        &self,
        issuer: &WorkLeaseIssuer,
        controller_id: ControllerId,
        worker_id: WorkerId,
        now: SystemTime,
        cancel: &CancellationToken,
    ) -> Result<(), SandboxError> {
        check_cancel(cancel)?;
        if self.controller_id != controller_id || self.worker_id != worker_id {
            return Err(SandboxError::LeaseInvalid);
        }
        if self.expires_at_unix_ms != self.spec.expires_at_unix_ms
            || self.cancel_deadline_unix_ms != self.spec.cancel_deadline_unix_ms
        {
            return Err(SandboxError::LeaseInvalid);
        }
        if self.task_digest != task_digest(self) {
            return Err(SandboxError::LeaseInvalid);
        }
        if !ct_eq(&self.signature, &issuer.mac(self)) {
            return Err(SandboxError::LeaseInvalid);
        }
        let now_ms = unix_ms(now)?;
        if now_ms >= self.expires_at_unix_ms || now_ms >= self.cancel_deadline_unix_ms {
            return Err(SandboxError::TimeoutInvalid);
        }
        Ok(())
    }

    pub const fn lease_id(&self) -> WorkLeaseId {
        self.lease_id
    }

    pub const fn controller_id(&self) -> ControllerId {
        self.controller_id
    }

    pub const fn worker_id(&self) -> WorkerId {
        self.worker_id
    }

    pub const fn capability_lease_id(&self) -> LeaseId {
        self.capability_lease_id
    }

    pub const fn task_digest(&self) -> ArtifactId {
        self.task_digest
    }

    pub const fn spec(&self) -> &RemoteSandboxSpec {
        &self.spec
    }

    pub const fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }

    pub const fn cancel_deadline_unix_ms(&self) -> u64 {
        self.cancel_deadline_unix_ms
    }

    pub fn canonical_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"schema\":\"");
        out.push_str(REMOTE_WORK_LEASE_SCHEMA);
        out.push_str("\",\"schema_version\":");
        out.push_str(&REMOTE_WORK_LEASE_SCHEMA_VERSION.to_string());
        out.push_str(",\"lease_id\":\"");
        out.push_str(&self.lease_id.0.to_string());
        out.push_str("\",\"controller_id\":\"");
        out.push_str(&self.controller_id.0.to_string());
        out.push_str("\",\"worker_id\":\"");
        out.push_str(&self.worker_id.0.to_string());
        out.push_str("\",\"capability_lease_id\":\"");
        out.push_str(&self.capability_lease_id.to_string());
        out.push_str("\",\"task_digest\":\"");
        out.push_str(&self.task_digest.to_string());
        out.push_str("\",\"sandbox\":");
        out.push_str(&self.spec.canonical_json());
        out.push_str(",\"expires_at_unix_ms\":");
        out.push_str(&self.expires_at_unix_ms.to_string());
        out.push_str(",\"cancel_deadline_unix_ms\":");
        out.push_str(&self.cancel_deadline_unix_ms.to_string());
        out.push_str(",\"nonce\":\"");
        push_hex(&mut out, &self.nonce);
        out.push_str("\",\"signature\":\"");
        push_hex(&mut out, &self.signature);
        out.push_str("\"}");
        out
    }
}

impl WorkerProfile {
    pub fn new(
        id: WorkerId,
        platform: WorkerPlatform,
        attestations: impl IntoIterator<Item = WorkerAttestation>,
    ) -> Result<Self, SandboxError> {
        if !platform.hosts_firecracker() {
            return Err(SandboxError::TierUnavailable);
        }
        let attestations: Vec<WorkerAttestation> = attestations.into_iter().collect();
        if attestations.is_empty() || attestations.len() > MAX_ATTESTATIONS {
            return Err(SandboxError::InvalidSpec);
        }
        if duplicate_attestation_class(&attestations) {
            return Err(SandboxError::InvalidSpec);
        }
        if !has_class(&attestations, WorkerAttestationClass::Platform)
            || !has_class(&attestations, WorkerAttestationClass::Firecracker)
            || !has_class(&attestations, WorkerAttestationClass::Jailer)
        {
            return Err(SandboxError::InvalidSpec);
        }
        Ok(Self {
            id,
            platform,
            attestations,
        })
    }

    pub const fn id(&self) -> WorkerId {
        self.id
    }

    pub const fn platform(&self) -> WorkerPlatform {
        self.platform
    }

    pub fn attestations(&self) -> &[WorkerAttestation] {
        &self.attestations
    }

    pub fn is_ready(&self) -> bool {
        self.platform.hosts_firecracker()
            && has_class(&self.attestations, WorkerAttestationClass::Platform)
            && has_class(&self.attestations, WorkerAttestationClass::Firecracker)
            && has_class(&self.attestations, WorkerAttestationClass::Jailer)
    }
}

impl RemoteBackend {
    pub fn new() -> Self {
        Self::with_controller(WorkLeaseIssuer::ephemeral(), ControllerId::new())
    }

    pub fn with_controller(issuer: WorkLeaseIssuer, controller_id: ControllerId) -> Self {
        let caps = SandboxCapabilities::new(
            SandboxTier::RemoteWorker,
            NetworkCapability::allowlist_and_proxy(),
            MountCapability::workspace_temp(),
            ResourceCapability::bounded(),
        )
        .expect("remote-worker is a known sandbox tier");
        Self {
            caps,
            issuer,
            controller_id,
            profile: None,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_profile(
        issuer: WorkLeaseIssuer,
        controller_id: ControllerId,
        profile: WorkerProfile,
    ) -> Result<Self, SandboxError> {
        if !profile.is_ready() {
            return Err(SandboxError::TierUnavailable);
        }
        let mut backend = Self::with_controller(issuer, controller_id);
        backend.profile = Some(profile);
        Ok(backend)
    }

    pub fn attach_profile(&mut self, profile: WorkerProfile) -> Result<(), SandboxError> {
        if !profile.is_ready() {
            return Err(SandboxError::TierUnavailable);
        }
        self.profile = Some(profile);
        Ok(())
    }

    pub fn controller_id(&self) -> ControllerId {
        self.controller_id
    }

    pub fn profile(&self) -> Option<&WorkerProfile> {
        self.profile.as_ref()
    }

    pub fn work_lease(&self, handle: &SandboxHandle) -> Result<RemoteWorkLease, SandboxError> {
        let sessions = self.lock_sessions()?;
        sessions
            .get(&handle.id())
            .filter(|session| session_matches(session, handle))
            .map(|session| session.work_lease.clone())
            .ok_or(SandboxError::UnknownHandle)
    }

    pub fn issue_work_lease(
        &self,
        spec: RemoteSandboxSpec,
        capability_lease: &CapabilityLease,
        now: SystemTime,
        cancel: &CancellationToken,
    ) -> Result<RemoteWorkLease, SandboxError> {
        check_cancel(cancel)?;
        let profile = self.profile.as_ref().ok_or(SandboxError::TierUnavailable)?;
        if !profile.is_ready() {
            return Err(SandboxError::TierUnavailable);
        }
        RemoteWorkLease::issue(
            &self.issuer,
            spec,
            RemoteLeaseParties::new(self.controller_id, profile.id),
            capability_lease,
            &profile.attestations,
            now,
            cancel,
        )
    }

    fn lock_sessions(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<SandboxId, PreparedSession>>, SandboxError> {
        self.sessions.lock().map_err(|_| SandboxError::HealthFailed)
    }
}

impl Default for RemoteBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxBackend for RemoteBackend {
    fn capabilities(&self) -> SandboxCapabilities {
        self.caps
    }

    fn health(&self, cancel: &CancellationToken) -> Result<BackendHealth, SandboxError> {
        check_cancel(cancel)?;
        match &self.profile {
            Some(profile) if profile.is_ready() => BackendHealth::available(Some(REMOTE_VERSION)),
            Some(_) => {
                BackendHealth::unavailable(HealthReason::FeatureMissing, Some(REMOTE_VERSION))
            }
            None => BackendHealth::unavailable(HealthReason::RuntimeMissing, Some(REMOTE_VERSION)),
        }
    }

    fn supports(&self, spec: &SandboxSpec) -> Result<(), SandboxError> {
        if spec.tier() != SandboxTier::RemoteWorker {
            return Err(SandboxError::TierUnavailable);
        }
        if spec.mounts().iter().any(|mount| mount.source().is_some()) {
            return Err(SandboxError::ForbiddenMount);
        }
        parse_rootfs_image(spec.image())?;
        supports_spec(&self.caps, spec)
    }

    fn prepare(
        &self,
        spec: &SandboxSpec,
        lease: &CapabilityLease,
        cancel: &CancellationToken,
    ) -> Result<SandboxHandle, SandboxError> {
        check_cancel(cancel)?;
        require_proc_lease(lease)?;
        self.supports(spec)?;
        let health = self.health(cancel)?;
        if !health.is_available() {
            return Err(SandboxError::TierUnavailable);
        }
        let profile = self.profile.as_ref().ok_or(SandboxError::TierUnavailable)?;
        let now = SystemTime::now();
        let expires_at = now
            .checked_add(spec.timeout())
            .ok_or(SandboxError::TimeoutInvalid)?;
        let remote_spec = RemoteSandboxSpec::from_sandbox_spec(
            spec,
            [],
            profile.attestations.iter().cloned(),
            expires_at,
            expires_at,
            now,
        )?;
        let work_lease = self.issue_work_lease(remote_spec, lease, now, cancel)?;
        let handle = SandboxHandle::new(SandboxTier::RemoteWorker, lease.lease_id())?;
        let mut sessions = self.lock_sessions()?;
        if sessions.len() >= MAX_LIVE_REMOTE_SANDBOXES {
            return Err(SandboxError::ResourceLimit);
        }
        sessions.insert(
            handle.id(),
            PreparedSession {
                handle,
                capability_lease_id: lease.lease_id(),
                work_lease,
            },
        );
        Ok(handle)
    }

    fn exec(
        &self,
        handle: &SandboxHandle,
        request: &SandboxExecRequest,
        lease: &CapabilityLease,
        cancel: &CancellationToken,
    ) -> Result<SandboxExecResult, SandboxError> {
        check_cancel(cancel)?;
        require_proc_lease(lease)?;
        if lease.lease_id() != handle.lease_id() {
            return Err(SandboxError::LeaseInvalid);
        }
        if handle.tier() != SandboxTier::RemoteWorker {
            return Err(SandboxError::TierUnavailable);
        }
        let work_lease = {
            let sessions = self.lock_sessions()?;
            let session = sessions
                .get(&handle.id())
                .filter(|session| session_matches(session, handle))
                .ok_or(SandboxError::UnknownHandle)?;
            if session.capability_lease_id != lease.lease_id() {
                return Err(SandboxError::LeaseInvalid);
            }
            session.work_lease.clone()
        };
        if request.timeout() > work_lease.spec.timeout {
            return Err(SandboxError::TimeoutInvalid);
        }
        if request.output_limit() > work_lease.spec.output_limit {
            return Err(SandboxError::OutputLimitInvalid);
        }
        let now = SystemTime::now();
        work_lease.verify(
            &self.issuer,
            self.controller_id,
            work_lease.worker_id,
            now,
            cancel,
        )?;
        // No Firecracker transport in this protocol task. Fail closed; not a pass.
        Err(SandboxError::HealthFailed)
    }

    fn destroy(
        &self,
        handle: &SandboxHandle,
        cancel: &CancellationToken,
    ) -> Result<(), SandboxError> {
        check_cancel(cancel)?;
        if handle.tier() != SandboxTier::RemoteWorker {
            return Err(SandboxError::UnknownHandle);
        }
        let mut sessions = self.lock_sessions()?;
        match sessions.remove(&handle.id()) {
            Some(session) if session_matches(&session, handle) => Ok(()),
            Some(session) => {
                sessions.insert(handle.id(), session);
                Err(SandboxError::UnknownHandle)
            }
            None => Err(SandboxError::UnknownHandle),
        }
    }
}

fn session_matches(session: &PreparedSession, handle: &SandboxHandle) -> bool {
    session.handle.id() == handle.id()
        && session.handle.tier() == handle.tier()
        && session.handle.lease_id() == handle.lease_id()
}

fn require_proc_lease(lease: &CapabilityLease) -> Result<(), SandboxError> {
    if lease.capability() != Capability::ProcExec {
        return Err(SandboxError::LeaseInvalid);
    }
    Ok(())
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), SandboxError> {
    if cancel.is_cancelled() {
        Err(SandboxError::Cancelled)
    } else {
        Ok(())
    }
}

fn parse_rootfs_image(image: Option<&str>) -> Result<ArtifactRef, SandboxError> {
    let raw = image.ok_or(SandboxError::Empty)?;
    if looks_like_host_path(raw) || is_docker_socket(raw) {
        return Err(SandboxError::ForbiddenMount);
    }
    let id = raw
        .parse::<ArtifactId>()
        .map_err(|_| SandboxError::InvalidSpec)?;
    let artifact = ArtifactRef::new(id, ROOTFS_MEDIA, 1, RedactionClass::Project);
    validate_artifact_ref(&artifact)?;
    Ok(artifact)
}

fn validate_artifact_ref(artifact: &ArtifactRef) -> Result<(), SandboxError> {
    if artifact.bytes == 0 {
        return Err(SandboxError::Empty);
    }
    validate_media_type(&artifact.media_type)?;
    if looks_like_host_path(&artifact.media_type) {
        return Err(SandboxError::ForbiddenMount);
    }
    Ok(())
}

fn validate_media_type(value: &str) -> Result<(), SandboxError> {
    if value != SNAPSHOT_MEDIA && value != ARTIFACT_MEDIA && value != ROOTFS_MEDIA {
        return Err(SandboxError::InvalidSpec);
    }
    Ok(())
}

fn validate_guest_target(path: &RepoPath) -> Result<(), SandboxError> {
    let raw = path.as_str();
    if is_docker_socket(raw) || is_forbidden_guest(raw) || looks_like_host_path(raw) {
        return Err(SandboxError::ForbiddenMount);
    }
    Ok(())
}

fn is_forbidden_guest(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    FORBIDDEN_GUEST_PREFIXES.iter().any(|prefix| {
        lower == *prefix
            || lower.starts_with(prefix) && lower.as_bytes().get(prefix.len()) == Some(&b'/')
    })
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
}

fn is_docker_socket(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let trimmed = lower.trim_end_matches('/');
    trimmed == "docker.sock" || trimmed.ends_with("/docker.sock")
}

fn has_class(items: &[WorkerAttestation], class: WorkerAttestationClass) -> bool {
    items.iter().any(|item| item.class == class)
}

fn duplicate_attestation_class(items: &[WorkerAttestation]) -> bool {
    for (i, item) in items.iter().enumerate() {
        if items[i + 1..].iter().any(|other| other.class == item.class) {
            return true;
        }
    }
    false
}

fn insert_unique_target(seen: &mut HashMap<String, ()>, target: &str) -> Result<(), SandboxError> {
    if seen.insert(target.to_owned(), ()).is_some() {
        Err(SandboxError::InvalidSpec)
    } else {
        Ok(())
    }
}

fn verify_required_attestations(
    required: &[WorkerAttestation],
    presented: &[WorkerAttestation],
) -> Result<(), SandboxError> {
    for need in required {
        let Some(got) = presented.iter().find(|item| item.class == need.class) else {
            return Err(SandboxError::LeaseInvalid);
        };
        if got.digest != need.digest {
            return Err(SandboxError::LeaseInvalid);
        }
    }
    Ok(())
}

fn unix_ms(time: SystemTime) -> Result<u64, SandboxError> {
    let elapsed = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SandboxError::TimeoutInvalid)?;
    u64::try_from(elapsed.as_millis()).map_err(|_| SandboxError::TimeoutInvalid)
}

fn millis_of(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn mount_mode_str(mode: MountMode) -> &'static str {
    match mode {
        MountMode::ReadOnly => "read_only",
        MountMode::ReadWrite => "read_write",
        MountMode::Temp => "temp",
    }
}

fn network_str(network: SandboxNetwork) -> &'static str {
    match network {
        SandboxNetwork::None => "none",
        SandboxNetwork::Allowlist => "allowlist",
        SandboxNetwork::Proxy => "proxy",
    }
}

fn push_artifact_json(out: &mut String, artifact: &ArtifactRef) {
    out.push('{');
    push_artifact_fields(out, artifact);
    out.push('}');
}

fn push_artifact_fields(out: &mut String, artifact: &ArtifactRef) {
    out.push_str("\"id\":\"");
    out.push_str(&artifact.id.to_string());
    out.push_str("\",\"media_type\":\"");
    push_json_str(out, &artifact.media_type);
    out.push_str("\",\"bytes\":");
    out.push_str(&artifact.bytes.to_string());
    out.push_str(",\"redaction\":\"");
    out.push_str(artifact.redaction.as_str());
    out.push('"');
}

fn push_json_str(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
}

fn push_hex(out: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
}

fn task_digest(lease: &RemoteWorkLease) -> ArtifactId {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(TASK_TAG);
    buf.push(0);
    buf.extend_from_slice(lease.controller_id.0.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.worker_id.0.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.capability_lease_id.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(&lease.spec.canonical_bytes());
    ArtifactId::from_bytes(&buf)
}

fn binding_bytes(lease: &RemoteWorkLease) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(BINDING_TAG);
    buf.push(0);
    buf.extend_from_slice(lease.lease_id.0.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.controller_id.0.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.worker_id.0.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.capability_lease_id.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.task_digest.as_digest());
    buf.push(0);
    buf.extend_from_slice(&lease.spec.canonical_bytes());
    buf.push(0);
    buf.extend_from_slice(&lease.expires_at_unix_ms.to_be_bytes());
    buf.extend_from_slice(&lease.cancel_deadline_unix_ms.to_be_bytes());
    buf.extend_from_slice(&lease.nonce);
    buf
}

fn hmac_sha256(key: &[u8; 32], msg: &[u8]) -> [u8; 32] {
    let mut ipad = [0x36u8; HMAC_BLOCK];
    let mut opad = [0x5cu8; HMAC_BLOCK];
    for (i, byte) in key.iter().copied().enumerate() {
        ipad[i] ^= byte;
        opad[i] ^= byte;
    }
    let mut inner = Vec::with_capacity(HMAC_BLOCK + msg.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner_hash = *ArtifactId::from_bytes(&inner).as_digest();
    let mut outer = Vec::with_capacity(HMAC_BLOCK + inner_hash.len());
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    *ArtifactId::from_bytes(&outer).as_digest()
}

fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut acc = 0u8;
    for i in 0..32 {
        acc |= a[i] ^ b[i];
    }
    acc == 0
}

impl fmt::Debug for RemoteSandboxSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteSandboxSpec")
            .field("image", &self.image.id.to_string())
            .field("inputs", &self.inputs.len())
            .field("mounts", &self.mounts.len())
            .field("cwd", &self.cwd)
            .field("network", &self.network)
            .field("cpu_millis", &self.cpu_millis)
            .field("memory_mb", &self.memory_mb)
            .field("pids", &self.pids)
            .field("timeout", &self.timeout)
            .field("output_limit", &self.output_limit)
            .field("required_attestations", &self.required_attestations)
            .field("expires_at_unix_ms", &self.expires_at_unix_ms)
            .field("cancel_deadline_unix_ms", &self.cancel_deadline_unix_ms)
            .field("secret_count", &self.secret_count)
            .finish()
    }
}

impl fmt::Debug for RemoteWorkLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteWorkLease")
            .field("lease_id", &self.lease_id)
            .field("controller_id", &self.controller_id)
            .field("worker_id", &self.worker_id)
            .field("capability_lease_id", &self.capability_lease_id)
            .field("task_digest", &self.task_digest.to_string())
            .field("spec", &self.spec)
            .field("expires_at_unix_ms", &self.expires_at_unix_ms)
            .field("cancel_deadline_unix_ms", &self.cancel_deadline_unix_ms)
            .field("signature", &"<mac>")
            .finish()
    }
}

impl fmt::Debug for WorkLeaseIssuer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkLeaseIssuer")
            .field("key", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use std::time::Instant;

    use capability_broker::{
        ActionRequest, ApprovalChoice, ApprovalResolution, ApprovalScopeId, CanonicalAction,
        CanonicalHostPath, FilesystemScope, LeaseIssuer, PolicyDocument, PolicySource, PolicyStack,
        PrincipalRef, ProcessScope, ResourceDescriptor, SecretHandle, evaluate, issue,
        request_approval,
    };
    use protocol::{ErrorCode, SessionId};

    use crate::backend::{IsolationStrength, SandboxManager};
    use crate::backends::container::ContainerBackend;
    use crate::backends::gvisor::GvisorBackend;
    use crate::backends::host_restricted::HostRestrictedBackend;
    use crate::{SandboxMount, SandboxSpec};

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-7c1e9b";
    const NOW_UNIX_MS: u64 = 1_700_000_000_000;
    const EXPIRES_UNIX_MS: u64 = 1_700_000_060_000;
    const CANCEL_UNIX_MS: u64 = 1_700_000_045_000;
    const GOLDEN_SPEC: &str = r#"{"schema":"rapidlm.remote_sandbox_spec","schema_version":1,"tier":"remote-worker","image":{"id":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","media_type":"application/vnd.rapidlm.rootfs","bytes":3,"redaction":"project"},"inputs":[{"id":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","media_type":"application/vnd.rapidlm.snapshot","bytes":12,"redaction":"project","target":"src","mode":"read_only"}],"mounts":[{"source":"temp","target":"tmp","mode":"temp"}],"cwd":"src","network":"none","cpu_millis":1000,"memory_mb":256,"pids":64,"timeout_ms":30000,"output_limit":1048576,"required_attestations":[{"class":"platform","digest":"sha256:d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"},{"class":"firecracker","digest":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}],"expires_at_unix_ms":1700000060000,"cancel_deadline_unix_ms":1700000045000,"secret_count":0}"#;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(NOW_UNIX_MS)
    }

    fn expires() -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(EXPIRES_UNIX_MS)
    }

    fn cancel_at() -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(CANCEL_UNIX_MS)
    }

    fn rootfs() -> ArtifactRef {
        ArtifactRef::new(
            ArtifactId::from_bytes(b"abc"),
            ROOTFS_MEDIA,
            3,
            RedactionClass::Project,
        )
    }

    fn snapshot() -> ArtifactRef {
        ArtifactRef::new(
            ArtifactId::from_bytes(b""),
            SNAPSHOT_MEDIA,
            12,
            RedactionClass::Project,
        )
    }

    fn platform_attestation() -> WorkerAttestation {
        WorkerAttestation::new(
            WorkerAttestationClass::Platform,
            ArtifactId::from_bytes(b"The quick brown fox jumps over the lazy dog"),
        )
    }

    fn firecracker_attestation() -> WorkerAttestation {
        WorkerAttestation::new(
            WorkerAttestationClass::Firecracker,
            ArtifactId::from_bytes(b"abc"),
        )
    }

    fn jailer_attestation() -> WorkerAttestation {
        WorkerAttestation::new(
            WorkerAttestationClass::Jailer,
            ArtifactId::from_bytes(b"jail"),
        )
    }

    fn golden_spec() -> RemoteSandboxSpec {
        RemoteSandboxSpec::builder()
            .image(rootfs())
            .input(
                RemoteInput::new(
                    snapshot(),
                    RepoPath::parse("src").expect("src"),
                    MountMode::ReadOnly,
                )
                .expect("input"),
            )
            .mount(RemoteMount::temp(RepoPath::parse("tmp").expect("tmp")).expect("temp"))
            .cwd(RepoPath::parse("src").expect("cwd"))
            .require_attestation(platform_attestation())
            .require_attestation(firecracker_attestation())
            .expires_at(expires())
            .cancel_deadline(cancel_at())
            .build(now())
            .expect("spec")
    }

    fn ready_profile() -> WorkerProfile {
        WorkerProfile::new(
            WorkerId::from_runtime(
                RuntimeId::from_str("018f3c8a-7e2b-7a10-8c4d-0123456789ab").expect("worker"),
            ),
            WorkerPlatform::LinuxKvm,
            [
                platform_attestation(),
                firecracker_attestation(),
                jailer_attestation(),
            ],
        )
        .expect("profile")
    }

    fn issuer() -> WorkLeaseIssuer {
        WorkLeaseIssuer::from_key([0x42; 32]).expect("issuer")
    }

    fn controller() -> ControllerId {
        ControllerId::from_runtime(
            RuntimeId::from_str("018f3c8a-7e2b-7a10-8c4d-0123456789cd").expect("controller"),
        )
    }

    fn parties(worker: WorkerId) -> RemoteLeaseParties {
        RemoteLeaseParties::new(controller(), worker)
    }

    fn principal() -> PrincipalRef {
        PrincipalRef::parse("agent").expect("principal")
    }

    fn parse_doc(src: &str, source: PolicySource) -> PolicyDocument {
        PolicyDocument::parse_toml(src, source, &CancellationToken::new()).expect("parse")
    }

    fn proc_stack() -> PolicyStack {
        PolicyStack::new([
            parse_doc(
                r#"
[[rules]]
id = "proc-allow"
effect = "allow"
subjects = ["*"]
capability = "proc.exec"
resource = { command_family = "test" }
"#,
                PolicySource::user("user-policy.toml").expect("user"),
            ),
            parse_doc(
                r#"
[[rules]]
id = "proc-ask"
effect = "ask"
subjects = ["*"]
capability = "proc.exec"
"#,
                PolicySource::trusted_project(".rapidlm/policy.toml").expect("project"),
            ),
        ])
        .expect("stack")
    }

    fn fs_stack() -> PolicyStack {
        PolicyStack::new([
            parse_doc(
                r#"
[[rules]]
id = "fs-allow"
effect = "allow"
subjects = ["*"]
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }
"#,
                PolicySource::user("user-policy.toml").expect("user"),
            ),
            parse_doc(
                r#"
[[rules]]
id = "fs-ask"
effect = "ask"
subjects = ["*"]
capability = "fs.read"
"#,
                PolicySource::trusted_project(".rapidlm/policy.toml").expect("project"),
            ),
        ])
        .expect("stack")
    }

    fn issue_capability(capability: Capability, resource: ResourceDescriptor) -> CapabilityLease {
        let policies = if capability == Capability::ProcExec {
            proc_stack()
        } else {
            fs_stack()
        };
        let actual = CanonicalAction::Resource {
            capability,
            resource: resource.clone(),
        };
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            capability,
            resource,
            actual,
            "sandbox",
        )
        .expect("request");
        let issued_at = Instant::now();
        let decision = evaluate(&policies, &request, &CancellationToken::new()).expect("evaluate");
        let approval = request_approval(&request, &decision, issued_at, &CancellationToken::new())
            .expect("approval");
        let approved = match approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                &request,
                issued_at,
                &CancellationToken::new(),
            )
            .expect("resolve")
        {
            ApprovalResolution::Approved(approved) => approved,
            ApprovalResolution::Denied => panic!("expected approved"),
        };
        issue(
            &LeaseIssuer::from_key([0x42; 32]).expect("issuer"),
            &approved,
            &policies,
            issued_at,
            &CancellationToken::new(),
        )
        .expect("issue")
    }

    fn proc_lease() -> CapabilityLease {
        issue_capability(
            Capability::ProcExec,
            ResourceDescriptor::Process(ProcessScope::new("test").expect("process")),
        )
    }

    fn fs_lease() -> CapabilityLease {
        issue_capability(
            Capability::FsRead,
            ResourceDescriptor::Filesystem(FilesystemScope::repo("src/main.rs").expect("fs")),
        )
    }

    fn remote_sandbox_spec() -> SandboxSpec {
        SandboxSpec::builder(SandboxTier::RemoteWorker)
            .cwd(RepoPath::parse("src").expect("cwd"))
            .image(rootfs().id.to_string())
            .mount(SandboxMount::temp(RepoPath::parse("tmp").expect("tmp")).expect("temp"))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("spec")
    }

    #[test]
    fn isolation_is_microvm_and_strong() {
        let backend = RemoteBackend::new();
        let caps = backend.capabilities();
        assert_eq!(caps.tier(), SandboxTier::RemoteWorker);
        assert_eq!(caps.isolation(), IsolationStrength::MicroVm);
        assert!(caps.isolation().is_strong_isolation());
        assert!(caps.isolation().doctor_warning().is_none());
        assert!(caps.network().allowlist_supported());
        assert!(caps.network().proxy_supported());
    }

    #[test]
    fn spec_includes_hashes_limits_policy_and_attestations() {
        let spec = golden_spec();
        let hashes: Vec<ArtifactId> = spec.input_hashes().collect();
        assert_eq!(hashes[0], rootfs().id);
        assert_eq!(hashes[1], snapshot().id);
        assert_eq!(spec.cpu_millis(), 1_000);
        assert_eq!(spec.memory_mb(), 256);
        assert_eq!(spec.pids(), 64);
        assert_eq!(spec.timeout(), Duration::from_secs(30));
        assert_eq!(spec.output_limit(), 1024 * 1024);
        assert_eq!(spec.network(), SandboxNetwork::None);
        assert_eq!(spec.mounts()[0].mode(), MountMode::Temp);
        assert!(matches!(spec.mounts()[0].source(), RemoteMountSource::Temp));
        assert_eq!(
            spec.required_attestations()[1].class(),
            WorkerAttestationClass::Firecracker
        );
        assert_eq!(spec.expires_at_unix_ms(), EXPIRES_UNIX_MS);
        assert_eq!(spec.cancel_deadline_unix_ms(), CANCEL_UNIX_MS);
        assert_eq!(spec.tier(), SandboxTier::RemoteWorker);
    }

    #[test]
    fn spec_canonical_json_golden() {
        let json = golden_spec().canonical_json();
        assert_eq!(json, GOLDEN_SPEC);
        assert!(!json.contains("host_path"));
        assert!(!json.contains("CanonicalHostPath"));
        assert!(!json.contains("/Users"));
        assert!(!json.contains("/home"));
        assert!(!json.contains("/etc"));
        assert!(!json.contains("docker.sock"));
        assert!(!json.contains(CANARY));
        assert!(json.contains("expires_at_unix_ms"));
        assert!(json.contains("cancel_deadline_unix_ms"));
    }

    #[test]
    fn missing_expiry_or_cancellation_is_rejected() {
        let built = RemoteSandboxSpec::builder()
            .image(rootfs())
            .cwd(RepoPath::parse("src").expect("cwd"))
            .require_attestation(platform_attestation())
            .require_attestation(firecracker_attestation())
            .cancel_deadline(cancel_at())
            .build(now());
        assert_eq!(built.expect_err("expiry"), SandboxError::TimeoutInvalid);
        let built = RemoteSandboxSpec::builder()
            .image(rootfs())
            .cwd(RepoPath::parse("src").expect("cwd"))
            .require_attestation(platform_attestation())
            .require_attestation(firecracker_attestation())
            .expires_at(expires())
            .build(now());
        assert_eq!(built.expect_err("cancel"), SandboxError::TimeoutInvalid);
    }

    #[test]
    fn host_path_authority_cannot_enter_the_protocol() {
        let host = CanonicalHostPath::from_resolved("/Users/canary-home").expect("host");
        let local = SandboxSpec::builder(SandboxTier::RemoteWorker)
            .cwd(RepoPath::parse("src").expect("cwd"))
            .image(rootfs().id.to_string())
            .mount(
                SandboxMount::bind(
                    host,
                    RepoPath::parse("src").expect("src"),
                    MountMode::ReadWrite,
                )
                .expect("bind"),
            )
            .build()
            .expect("local");
        let err = RemoteSandboxSpec::from_sandbox_spec(
            &local,
            [],
            [platform_attestation(), firecracker_attestation()],
            expires(),
            cancel_at(),
            now(),
        )
        .expect_err("host");
        assert_eq!(err, SandboxError::ForbiddenMount);
        assert_eq!(err.error_code(), Some(ErrorCode::PolicyDenied));
        assert!(!err.as_str().contains("/Users"));
        assert!(!err.as_str().contains(CANARY));

        let imaged = SandboxSpec::builder(SandboxTier::RemoteWorker)
            .cwd(RepoPath::parse("src").expect("cwd"))
            .image("/var/lib/firecracker/rootfs.ext4")
            .build()
            .expect("imaged");
        assert_eq!(
            RemoteSandboxSpec::from_sandbox_spec(
                &imaged,
                [],
                [platform_attestation(), firecracker_attestation()],
                expires(),
                cancel_at(),
                now(),
            )
            .expect_err("image path"),
            SandboxError::ForbiddenMount
        );

        assert_eq!(
            RemoteInput::new(
                snapshot(),
                RepoPath::parse("etc/passwd").expect("etc"),
                MountMode::ReadOnly,
            )
            .expect_err("etc"),
            SandboxError::ForbiddenMount
        );
        assert_eq!(
            RemoteMount::temp(RepoPath::parse("docker.sock").expect("sock")).expect_err("sock"),
            SandboxError::ForbiddenMount
        );
    }

    #[test]
    fn docker_socket_and_sensitive_targets_cannot_bypass_mount_policy() {
        let sock = CanonicalHostPath::from_resolved("/var/run/docker.sock").expect("sock");
        assert_eq!(
            SandboxMount::bind(
                sock,
                RepoPath::parse("docker.sock").expect("t"),
                MountMode::ReadWrite
            )
            .expect_err("bind"),
            SandboxError::ForbiddenMount
        );
        let backend = RemoteBackend::new();
        let lease = proc_lease();
        let live = CancellationToken::new();
        let spec = SandboxSpec::builder(SandboxTier::RemoteWorker)
            .cwd(RepoPath::parse("src").expect("cwd"))
            .image(rootfs().id.to_string())
            .mount(
                SandboxMount::bind(
                    CanonicalHostPath::from_resolved("/etc").expect("etc"),
                    RepoPath::parse("src").expect("src"),
                    MountMode::ReadOnly,
                )
                .expect("bind"),
            )
            .build()
            .expect("spec");
        assert_eq!(
            backend.prepare(&spec, &lease, &live).expect_err("etc"),
            SandboxError::ForbiddenMount
        );
    }

    #[test]
    fn resource_timeout_and_cancel_fail_closed() {
        let err = RemoteSandboxSpec::builder()
            .image(rootfs())
            .cwd(RepoPath::parse("src").expect("cwd"))
            .require_attestation(platform_attestation())
            .require_attestation(firecracker_attestation())
            .pids(0)
            .expires_at(expires())
            .cancel_deadline(cancel_at())
            .build(now())
            .expect_err("pids");
        assert_eq!(err, SandboxError::ResourceLimit);
        let err = RemoteSandboxSpec::builder()
            .image(rootfs())
            .cwd(RepoPath::parse("src").expect("cwd"))
            .require_attestation(platform_attestation())
            .require_attestation(firecracker_attestation())
            .timeout(Duration::from_secs(120))
            .expires_at(expires())
            .cancel_deadline(cancel_at())
            .build(now())
            .expect_err("timeout past expiry");
        assert_eq!(err, SandboxError::TimeoutInvalid);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let err = RemoteWorkLease::issue(
            &issuer(),
            golden_spec(),
            parties(ready_profile().id()),
            &proc_lease(),
            ready_profile().attestations(),
            now(),
            &cancelled,
        )
        .expect_err("cancel");
        assert_eq!(err, SandboxError::Cancelled);
        assert!(err.error_code().is_none());
    }

    #[test]
    fn work_lease_binds_ids_hashes_and_rejects_tamper_expiry_and_wrong_attestation() {
        let profile = ready_profile();
        let lease = RemoteWorkLease::issue(
            &issuer(),
            golden_spec(),
            parties(profile.id()),
            &proc_lease(),
            profile.attestations(),
            now(),
            &CancellationToken::new(),
        )
        .expect("issue");
        lease
            .verify(
                &issuer(),
                controller(),
                profile.id(),
                now(),
                &CancellationToken::new(),
            )
            .expect("verify");
        assert_eq!(lease.spec().image().id, rootfs().id);
        assert_eq!(lease.expires_at_unix_ms(), EXPIRES_UNIX_MS);
        assert_eq!(lease.cancel_deadline_unix_ms(), CANCEL_UNIX_MS);
        let json = lease.canonical_json();
        assert!(json.contains("\"schema\":\"rapidlm.remote_work_lease\""));
        assert!(!json.contains("host_path"));
        assert!(!json.contains(CANARY));
        assert!(!json.contains("token"));

        let mut tampered = lease.clone();
        tampered.spec.cpu_millis = 2_000;
        assert_eq!(
            tampered
                .verify(
                    &issuer(),
                    controller(),
                    profile.id(),
                    now(),
                    &CancellationToken::new(),
                )
                .expect_err("tamper"),
            SandboxError::LeaseInvalid
        );
        assert_eq!(
            lease
                .verify(
                    &issuer(),
                    controller(),
                    profile.id(),
                    UNIX_EPOCH + Duration::from_millis(EXPIRES_UNIX_MS),
                    &CancellationToken::new(),
                )
                .expect_err("expired"),
            SandboxError::TimeoutInvalid
        );
        assert_eq!(
            lease
                .verify(
                    &issuer(),
                    controller(),
                    WorkerId::new(),
                    now(),
                    &CancellationToken::new(),
                )
                .expect_err("worker"),
            SandboxError::LeaseInvalid
        );

        let wrong = [platform_attestation(), jailer_attestation()];
        assert_eq!(
            RemoteWorkLease::issue(
                &issuer(),
                golden_spec(),
                parties(profile.id()),
                &proc_lease(),
                &wrong,
                now(),
                &CancellationToken::new(),
            )
            .expect_err("missing firecracker"),
            SandboxError::LeaseInvalid
        );
        let mismatched = [
            platform_attestation(),
            WorkerAttestation::new(
                WorkerAttestationClass::Firecracker,
                ArtifactId::from_bytes(b"other"),
            ),
            jailer_attestation(),
        ];
        assert_eq!(
            RemoteWorkLease::issue(
                &issuer(),
                golden_spec(),
                parties(profile.id()),
                &proc_lease(),
                &mismatched,
                now(),
                &CancellationToken::new(),
            )
            .expect_err("digest"),
            SandboxError::LeaseInvalid
        );
        assert_eq!(
            RemoteWorkLease::issue(
                &issuer(),
                golden_spec(),
                parties(profile.id()),
                &fs_lease(),
                profile.attestations(),
                now(),
                &CancellationToken::new(),
            )
            .expect_err("fs"),
            SandboxError::LeaseInvalid
        );
        assert_eq!(
            SandboxError::LeaseInvalid.error_code(),
            Some(ErrorCode::PolicyLeaseInvalid)
        );
    }

    #[test]
    fn secrets_and_issuer_key_are_not_echoed() {
        let spec = RemoteSandboxSpec::builder()
            .image(rootfs())
            .cwd(RepoPath::parse("src").expect("cwd"))
            .require_attestation(platform_attestation())
            .require_attestation(firecracker_attestation())
            .expires_at(expires())
            .cancel_deadline(cancel_at())
            .secret_count(1)
            .build(now())
            .expect("spec");
        let debug = format!("{spec:?}");
        assert!(!debug.contains(CANARY));
        assert!(debug.contains("secret_count: 1"));
        assert!(!spec.canonical_json().contains(CANARY));
        let handle = SecretHandle::parse(CANARY).expect("handle");
        assert!(!format!("{:?}", issuer()).contains("0x42"));
        assert!(!format!("{handle:?}").is_empty());
        assert!(!SandboxError::LeaseInvalid.as_str().contains(CANARY));
    }

    #[test]
    fn absent_worker_is_unavailable_not_clean() {
        let backend = RemoteBackend::new();
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert!(!health.is_available());
        assert_eq!(health.reason(), Some(HealthReason::RuntimeMissing));
        assert_eq!(
            backend
                .prepare(
                    &remote_sandbox_spec(),
                    &proc_lease(),
                    &CancellationToken::new()
                )
                .expect_err("prepare"),
            SandboxError::TierUnavailable
        );
        assert_eq!(
            SandboxError::TierUnavailable.error_code(),
            Some(ErrorCode::SandboxTierUnavailable)
        );
    }

    #[test]
    fn required_remote_never_silently_uses_weaker_backend() {
        let mut mgr = SandboxManager::new();
        mgr.register(Box::new(HostRestrictedBackend::new()))
            .expect("host");
        mgr.register(Box::new(ContainerBackend::new()))
            .expect("container");
        mgr.register(Box::new(GvisorBackend::new()))
            .expect("gvisor");
        mgr.register(Box::new(RemoteBackend::new()))
            .expect("remote");
        let spec = remote_sandbox_spec();
        match mgr.select(&spec, &CancellationToken::new()) {
            Ok(selected) => panic!(
                "required remote must not select {:?}/{:?}",
                selected.capabilities().tier(),
                selected.capabilities().isolation()
            ),
            Err(err) => {
                assert_eq!(err, SandboxError::TierUnavailable);
                assert_eq!(err.error_code(), Some(ErrorCode::SandboxTierUnavailable));
            }
        }
    }

    #[test]
    fn attested_backend_prepares_lease_and_exec_is_not_a_clean_pass() {
        let backend =
            RemoteBackend::with_profile(issuer(), controller(), ready_profile()).expect("backend");
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert!(health.is_available());
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = backend
            .prepare(&remote_sandbox_spec(), &lease, &live)
            .expect("prepare");
        assert_eq!(handle.tier(), SandboxTier::RemoteWorker);
        let work = backend.work_lease(&handle).expect("lease");
        assert_eq!(work.controller_id(), controller());
        assert_eq!(work.worker_id(), ready_profile().id());
        assert_eq!(work.capability_lease_id(), lease.lease_id());
        assert!(
            !work
                .spec()
                .input_hashes()
                .next()
                .unwrap()
                .to_string()
                .is_empty()
        );
        assert!(work.spec().expires_at_unix_ms() > 0);
        assert!(work.spec().cancel_deadline_unix_ms() > 0);

        let request =
            SandboxExecRequest::new(["/bin/true"], Duration::from_secs(1), 4096).expect("req");
        assert_eq!(
            backend
                .exec(&handle, &request, &lease, &live)
                .expect_err("no transport"),
            SandboxError::HealthFailed
        );
        let wide =
            SandboxExecRequest::new(["/bin/true"], Duration::from_secs(31), 4096).expect("wide");
        assert_eq!(
            backend
                .exec(&handle, &wide, &lease, &live)
                .expect_err("timeout"),
            SandboxError::TimeoutInvalid
        );
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            backend
                .exec(&handle, &request, &lease, &cancelled)
                .expect_err("cancel"),
            SandboxError::Cancelled
        );
        assert_eq!(
            backend
                .prepare(&remote_sandbox_spec(), &fs_lease(), &live)
                .expect_err("fs"),
            SandboxError::LeaseInvalid
        );
        backend.destroy(&handle, &live).expect("destroy");
        assert_eq!(
            backend.work_lease(&handle).expect_err("gone"),
            SandboxError::UnknownHandle
        );
    }

    #[test]
    fn weaker_tier_specs_are_refused() {
        let backend =
            RemoteBackend::with_profile(issuer(), controller(), ready_profile()).expect("backend");
        let lease = proc_lease();
        let live = CancellationToken::new();
        let host = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(RepoPath::parse("src").expect("cwd"))
            .build()
            .expect("host");
        assert_eq!(
            backend.prepare(&host, &lease, &live).expect_err("host"),
            SandboxError::TierUnavailable
        );
        let container = SandboxSpec::builder(SandboxTier::Container)
            .cwd(RepoPath::parse("src").expect("cwd"))
            .build()
            .expect("container");
        assert_eq!(
            backend
                .prepare(&container, &lease, &live)
                .expect_err("container"),
            SandboxError::TierUnavailable
        );
    }

    #[test]
    fn attested_remote_is_selected_when_required() {
        let mut mgr = SandboxManager::new();
        mgr.register(Box::new(HostRestrictedBackend::new()))
            .expect("host");
        mgr.register(Box::new(
            RemoteBackend::with_profile(issuer(), controller(), ready_profile()).expect("remote"),
        ))
        .expect("register");
        let selected = mgr
            .select(&remote_sandbox_spec(), &CancellationToken::new())
            .expect("select");
        assert_eq!(selected.capabilities().tier(), SandboxTier::RemoteWorker);
        assert_eq!(
            selected.capabilities().isolation(),
            IsolationStrength::MicroVm
        );
    }

    #[test]
    fn zero_key_and_empty_attestations_fail_closed() {
        assert_eq!(
            WorkLeaseIssuer::from_key([0; 32]).expect_err("zero"),
            SandboxError::LeaseInvalid
        );
        assert_eq!(
            WorkerProfile::new(WorkerId::new(), WorkerPlatform::LinuxKvm, []).expect_err("empty"),
            SandboxError::InvalidSpec
        );
        assert_eq!(
            RemoteSandboxSpec::builder()
                .image(rootfs())
                .cwd(RepoPath::parse("src").expect("cwd"))
                .expires_at(expires())
                .cancel_deadline(cancel_at())
                .build(now())
                .expect_err("no attestations"),
            SandboxError::InvalidSpec
        );
    }
}
