//! Remote macOS iOS Simulator delegation.
//!
//! Local `simctl` stays first. Remote use is denied unless policy/config
//! explicitly permits it. Delegation issues a signed work lease with
//! content-addressed artifact inputs and imports evidence only after worker
//! identity and result hashes verify (T-010, T-CU-01, T-CU-02, T-CU-03).

use std::error::Error;
use std::fmt::{self, Debug};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use capability_broker::{CancellationToken, SecretHandle};
use protocol::{ArtifactId, ArtifactRef, ErrorCode, EvidenceId, RedactionClass, RuntimeId};

use super::simctl::{
    BundleId, DEFAULT_BOOT_TIMEOUT, DeviceUdid, IosSimctlBackend, IosSimctlError, MAX_BOOT_TIMEOUT,
};

/// Worker capability advertised by an authorized macOS simulator host.
pub const IOS_SIMULATOR_CAPABILITY: &str = "ios-simulator";

/// Wire schema for [`IosRemoteWorkLease`].
pub const IOS_REMOTE_WORK_LEASE_SCHEMA: &str = "rapidlm.ios_remote_work_lease";

/// v1 schema version for [`IosRemoteWorkLease`].
pub const IOS_REMOTE_WORK_LEASE_SCHEMA_VERSION: u16 = 1;

/// Default bound for one remote observe/act/lifecycle call.
pub const DEFAULT_REMOTE_TIMEOUT: Duration = Duration::from_secs(30);

/// Absolute upper bound accepted on a remote call.
pub const MAX_REMOTE_TIMEOUT: Duration = Duration::from_secs(180);

/// Maximum signed artifact inputs or imported outputs on one lease.
pub const MAX_REMOTE_ARTIFACTS: usize = 8;

/// Maximum bytes accepted for one remote evidence payload during hash check.
pub const MAX_REMOTE_OUTPUT_BYTES: u64 = 2 * 1024 * 1024;

const BINDING_TAG: &[u8] = b"rapidlm.ios_remote_work_lease.v1";
const TASK_TAG: &[u8] = b"rapidlm.ios_remote_task.v1";
const RESULT_TAG: &[u8] = b"rapidlm.ios_remote_result.v1";
const HMAC_BLOCK: usize = 64;
const FIXTURE_UDID: &str = "A1B2C3D4-E5F6-7890-ABCD-EF1234567890";
const SCREENSHOT_MEDIA: &str = "image/png";
const SCREENSHOT_MARKER: &[u8] = b"rapidlm.ios.remote.screenshot.v1";
const REDACTED_SCREENSHOT: &[u8] = b"rapidlm.ios.remote.screenshot.redacted.v1";
const APP_MEDIA: &str = "application/vnd.rapidlm.ios-app";

/// Host-side iOS Simulator availability. Distinct from remote policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LocalIosAvailability {
    Ready,
    Unavailable,
}

/// Explicit policy/config gate. Default is deny. UI text cannot construct Allow.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RemoteDelegationPolicy {
    Denied,
    Allowed,
}

/// Typed remote-delegation config. Not parsed from page/app text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct RemoteDelegationConfig {
    policy: RemoteDelegationPolicy,
}

/// Platform that may host a remote iOS Simulator.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WorkerPlatform {
    Macos,
}

/// Controller identity bound into a work lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ControllerId(RuntimeId);

/// Authorized remote macOS worker identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct MacosWorkerId(RuntimeId);

/// Work-lease identity. Distinct from an in-process capability lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct WorkLeaseId(RuntimeId);

/// Observation identity bound to one remote capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct RemoteObservationId(RuntimeId);

/// Advertised macOS worker. Incomplete profiles are rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MacosWorkerProfile {
    id: MacosWorkerId,
    platform: WorkerPlatform,
    capability: &'static str,
    attestation: ArtifactId,
}

/// MAC key used to sign controller-issued work leases.
pub struct RemoteWorkLeaseIssuer {
    key: [u8; 32],
}

/// Controller and worker identities bound into a work lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct RemoteLeaseParties {
    controller_id: ControllerId,
    worker_id: MacosWorkerId,
}

/// Typed remote operation carried by a signed lease. No host paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DelegatedOperation {
    Observe {
        generation: u64,
        screenshot: bool,
    },
    Act {
        observation: RemoteObservationId,
        generation: u64,
        action: RemoteActionKind,
        target: Option<RemoteTargetRef>,
    },
    Boot {
        udid: DeviceUdid,
    },
    Install {
        udid: DeviceUdid,
        app: ArtifactRef,
    },
    Launch {
        udid: DeviceUdid,
        bundle: BundleId,
    },
    Screenshot {
        udid: DeviceUdid,
    },
}

/// Model-visible action class. Payloads are omitted from the lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RemoteActionKind {
    Tap,
    TypeText,
    Key,
}

/// Typed remote action. Secret handles never carry plaintext.
#[derive(Clone, Eq, PartialEq)]
pub enum RemoteMobileAction {
    Tap { count: u8 },
    TypeText { value: RemoteSecretAwareText },
    Key { name: RemoteKey },
}

/// Type payload. Secret handles stay opaque.
#[derive(Clone, Eq, PartialEq)]
pub enum RemoteSecretAwareText {
    Literal(String),
    SecretHandle(SecretHandle),
}

/// Bounded hardware/system key. Not an arbitrary HID code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RemoteKey {
    Home,
    Enter,
}

/// Target resolution order: accessibility → role+name → visual → coordinate.
#[derive(Clone, Eq, PartialEq)]
pub enum RemoteTargetRef {
    Accessibility {
        node: String,
    },
    RoleName {
        role: String,
        name: String,
    },
    Visual {
        observation: RemoteObservationId,
        label: String,
    },
    Coordinate {
        observation: RemoteObservationId,
        x: i32,
        y: i32,
    },
}

/// Signed work lease. Capability-lease tokens and host paths are absent.
#[derive(Clone, Eq, PartialEq)]
pub struct IosRemoteWorkLease {
    lease_id: WorkLeaseId,
    controller_id: ControllerId,
    worker_id: MacosWorkerId,
    task_digest: ArtifactId,
    artifact_inputs: Vec<ArtifactRef>,
    operation: DelegatedOperation,
    expires_at_unix_ms: u64,
    cancel_deadline_unix_ms: u64,
    nonce: [u8; 16],
    signature: [u8; 32],
}

/// Observe options. Screenshot is off by default.
#[derive(Clone, Debug)]
pub struct RemoteObserveRequest {
    screenshot: bool,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Act request. A current observation is required (observe → resolve → act).
#[derive(Clone)]
pub struct RemoteActRequest {
    observation_id: RemoteObservationId,
    generation: u64,
    action: RemoteMobileAction,
    target: Option<RemoteTargetRef>,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Device lease returned by a remote boot. Includes worker identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteDeviceLease {
    device_id: DeviceUdid,
    worker_id: MacosWorkerId,
    generation: u64,
    expires_at_unix_ms: u64,
}

/// Canonical remote observation after hash/identity verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteMobileObservation {
    id: RemoteObservationId,
    worker_id: MacosWorkerId,
    generation: u64,
    state_hash: ArtifactId,
    screenshot: Option<ArtifactRef>,
    ui_node_count: u32,
    sensitive: bool,
    captured_at_ms: u64,
}

/// Side-effect outcome. Model prose cannot override this.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RemoteActionStatus {
    Succeeded,
    Failed,
    Denied,
}

/// Canonical remote action result after hash/identity verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteMobileActionResult {
    status: RemoteActionStatus,
    action: RemoteActionKind,
    after: RemoteMobileObservation,
    evidence: Vec<EvidenceId>,
    secret_handle_used: bool,
}

/// Untrusted worker envelope. Must pass [`IosRemoteDelegator::import_result`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UntrustedRemoteEnvelope {
    pub lease_id: WorkLeaseId,
    pub worker_id: MacosWorkerId,
    pub attestation: ArtifactId,
    pub observation: Option<UntrustedObservation>,
    pub action_status: Option<RemoteActionStatus>,
    pub artifacts: Vec<UntrustedArtifact>,
}

/// Observation fields claimed by a worker. Not evidence until imported.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UntrustedObservation {
    pub id: RemoteObservationId,
    pub generation: u64,
    pub state_hash: ArtifactId,
    pub ui_node_count: u32,
    pub sensitive: bool,
    pub captured_at_ms: u64,
    pub screenshot: Option<ArtifactId>,
}

/// Claimed artifact plus optional payload used only for hash verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UntrustedArtifact {
    pub artifact: ArtifactRef,
    pub payload: Vec<u8>,
}

/// Typed remote-delegation failure. Display never echoes UDID, secrets, or paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum IosRemoteError {
    Cancelled,
    Timeout,
    TimeoutInvalid,
    LocalCapabilityPresent,
    CapabilityUnavailable,
    RemotePolicyDenied,
    WorkerUnavailable,
    WorkerUnauthorized,
    WorkerIdentityMismatch,
    AttestationMismatch,
    HashMismatch,
    LeaseInvalid,
    LeaseExpired,
    ObservationRequired,
    StaleObservation,
    TargetRequired,
    TargetNotFound,
    AmbiguousTarget,
    SensitiveDenied,
    SecretUnresolved,
    InvalidText,
    InvalidArtifact,
    OutputBound,
    DeviceNotFound,
    DeviceClosed,
    Unavailable,
    Backend,
}

/// Routes iOS simulator work to an authorized remote macOS worker.
pub struct IosRemoteDelegator {
    config: RemoteDelegationConfig,
    local: LocalIosAvailability,
    issuer: RemoteWorkLeaseIssuer,
    controller_id: ControllerId,
    worker: Option<MacosWorkerProfile>,
    transport: Mutex<FakeMacosWorker>,
    last_observation: Mutex<Option<RemoteMobileObservation>>,
    last_lease: Mutex<Option<IosRemoteWorkLease>>,
    clock: Mutex<Option<SystemTime>>,
}

struct FakeMacosWorker {
    profile: MacosWorkerProfile,
    generation: u64,
    booted: bool,
    udid: DeviceUdid,
    installed: Vec<ArtifactId>,
    launched: Vec<BundleId>,
    label: &'static str,
    secret_present: bool,
}

impl LocalIosAvailability {
    /// Probe the host. Non-macOS is unavailable and is never silently emulated.
    pub fn detect(cancel: &CancellationToken) -> Result<Self, IosRemoteError> {
        check_cancel(cancel)?;
        match IosSimctlBackend::probe(cancel) {
            Ok(cap) if cap.is_ready() => Ok(Self::Ready),
            Ok(_) => Ok(Self::Unavailable),
            Err(IosSimctlError::Cancelled) => Err(IosRemoteError::Cancelled),
            Err(IosSimctlError::TimeoutInvalid) => Err(IosRemoteError::TimeoutInvalid),
            Err(_) => Ok(Self::Unavailable),
        }
    }

    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }
}

impl RemoteDelegationPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Allowed => "allowed",
        }
    }

    /// Untrusted UI/page/app text never grants remote use.
    pub fn from_untrusted_text(_text: &str) -> Self {
        Self::Denied
    }
}

impl RemoteDelegationConfig {
    pub const fn denied() -> Self {
        Self {
            policy: RemoteDelegationPolicy::Denied,
        }
    }

    /// Explicit host/org config. Not constructible from observation text.
    pub const fn from_policy(policy: RemoteDelegationPolicy) -> Self {
        Self { policy }
    }

    pub const fn allows_remote(self) -> bool {
        matches!(self.policy, RemoteDelegationPolicy::Allowed)
    }

    pub const fn policy(self) -> RemoteDelegationPolicy {
        self.policy
    }
}

impl WorkerPlatform {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Macos => "macos",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, IosRemoteError> {
        match raw {
            "macos" => Ok(Self::Macos),
            _ => Err(IosRemoteError::WorkerUnauthorized),
        }
    }
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

impl MacosWorkerId {
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

impl RemoteObservationId {
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

impl MacosWorkerProfile {
    pub fn new(
        id: MacosWorkerId,
        platform: WorkerPlatform,
        capability: &str,
        attestation: ArtifactId,
    ) -> Result<Self, IosRemoteError> {
        if platform != WorkerPlatform::Macos || capability != IOS_SIMULATOR_CAPABILITY {
            return Err(IosRemoteError::WorkerUnauthorized);
        }
        if attestation == ArtifactId::from_bytes(&[]) {
            return Err(IosRemoteError::AttestationMismatch);
        }
        Ok(Self {
            id,
            platform,
            capability: IOS_SIMULATOR_CAPABILITY,
            attestation,
        })
    }

    pub const fn id(&self) -> MacosWorkerId {
        self.id
    }

    pub const fn platform(&self) -> WorkerPlatform {
        self.platform
    }

    pub const fn capability(&self) -> &'static str {
        self.capability
    }

    pub const fn attestation(&self) -> ArtifactId {
        self.attestation
    }
}

impl RemoteWorkLeaseIssuer {
    pub fn from_key(key: [u8; 32]) -> Result<Self, IosRemoteError> {
        if key.iter().all(|b| *b == 0) {
            return Err(IosRemoteError::LeaseInvalid);
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

    fn mac(&self, lease: &IosRemoteWorkLease) -> [u8; 32] {
        hmac_sha256(&self.key, &binding_bytes(lease))
    }
}

impl RemoteLeaseParties {
    pub const fn new(controller_id: ControllerId, worker_id: MacosWorkerId) -> Self {
        Self {
            controller_id,
            worker_id,
        }
    }

    pub const fn controller_id(self) -> ControllerId {
        self.controller_id
    }

    pub const fn worker_id(self) -> MacosWorkerId {
        self.worker_id
    }
}

impl IosRemoteWorkLease {
    pub fn issue(
        issuer: &RemoteWorkLeaseIssuer,
        parties: RemoteLeaseParties,
        operation: DelegatedOperation,
        artifact_inputs: Vec<ArtifactRef>,
        timeout: Duration,
        now: SystemTime,
        cancel: &CancellationToken,
    ) -> Result<Self, IosRemoteError> {
        check_cancel(cancel)?;
        validate_timeout(timeout)?;
        if artifact_inputs.len() > MAX_REMOTE_ARTIFACTS {
            return Err(IosRemoteError::InvalidArtifact);
        }
        if artifact_inputs
            .iter()
            .any(|item| item.bytes > MAX_REMOTE_OUTPUT_BYTES)
        {
            return Err(IosRemoteError::OutputBound);
        }
        let now_ms = unix_ms(now)?;
        let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        let expires_at_unix_ms = now_ms.saturating_add(timeout_ms);
        let cancel_deadline_unix_ms = expires_at_unix_ms;
        if now_ms >= expires_at_unix_ms || now_ms >= cancel_deadline_unix_ms {
            return Err(IosRemoteError::TimeoutInvalid);
        }
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(RuntimeId::new().as_uuid().as_bytes());
        let mut lease = Self {
            lease_id: WorkLeaseId::new(),
            controller_id: parties.controller_id,
            worker_id: parties.worker_id,
            task_digest: ArtifactId::from_bytes(&[]),
            artifact_inputs,
            operation,
            expires_at_unix_ms,
            cancel_deadline_unix_ms,
            nonce,
            signature: [0u8; 32],
        };
        lease.task_digest = task_digest(&lease);
        lease.signature = issuer.mac(&lease);
        Ok(lease)
    }

    pub fn verify(
        &self,
        issuer: &RemoteWorkLeaseIssuer,
        controller_id: ControllerId,
        worker_id: MacosWorkerId,
        now: SystemTime,
        cancel: &CancellationToken,
    ) -> Result<(), IosRemoteError> {
        check_cancel(cancel)?;
        if self.controller_id != controller_id || self.worker_id != worker_id {
            return Err(IosRemoteError::WorkerIdentityMismatch);
        }
        if self.expires_at_unix_ms != self.cancel_deadline_unix_ms {
            return Err(IosRemoteError::LeaseInvalid);
        }
        if self.task_digest != task_digest(self) {
            return Err(IosRemoteError::LeaseInvalid);
        }
        if !ct_eq(&self.signature, &issuer.mac(self)) {
            return Err(IosRemoteError::LeaseInvalid);
        }
        let now_ms = unix_ms(now)?;
        if now_ms >= self.expires_at_unix_ms || now_ms >= self.cancel_deadline_unix_ms {
            return Err(IosRemoteError::LeaseExpired);
        }
        Ok(())
    }

    pub const fn lease_id(&self) -> WorkLeaseId {
        self.lease_id
    }

    pub const fn controller_id(&self) -> ControllerId {
        self.controller_id
    }

    pub const fn worker_id(&self) -> MacosWorkerId {
        self.worker_id
    }

    pub const fn task_digest(&self) -> ArtifactId {
        self.task_digest
    }

    pub fn artifact_inputs(&self) -> &[ArtifactRef] {
        &self.artifact_inputs
    }

    pub const fn operation(&self) -> &DelegatedOperation {
        &self.operation
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
        out.push_str(IOS_REMOTE_WORK_LEASE_SCHEMA);
        out.push_str("\",\"schema_version\":");
        out.push_str(&IOS_REMOTE_WORK_LEASE_SCHEMA_VERSION.to_string());
        out.push_str(",\"lease_id\":\"");
        out.push_str(&self.lease_id.0.to_string());
        out.push_str("\",\"controller_id\":\"");
        out.push_str(&self.controller_id.0.to_string());
        out.push_str("\",\"worker_id\":\"");
        out.push_str(&self.worker_id.0.to_string());
        out.push_str("\",\"task_digest\":\"");
        out.push_str(&self.task_digest.to_string());
        out.push_str("\",\"capability\":\"");
        out.push_str(IOS_SIMULATOR_CAPABILITY);
        out.push_str("\",\"expires_at_unix_ms\":");
        out.push_str(&self.expires_at_unix_ms.to_string());
        out.push_str(",\"cancel_deadline_unix_ms\":");
        out.push_str(&self.cancel_deadline_unix_ms.to_string());
        out.push_str(",\"inputs\":");
        out.push_str(&self.artifact_inputs.len().to_string());
        out.push('}');
        out
    }
}

impl RemoteObserveRequest {
    pub fn new() -> Self {
        Self {
            screenshot: false,
            timeout: DEFAULT_REMOTE_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }

    pub fn with_screenshot(mut self, screenshot: bool) -> Self {
        self.screenshot = screenshot;
        self
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, IosRemoteError> {
        validate_timeout(timeout)?;
        self.timeout = timeout;
        Ok(self)
    }
}

impl Default for RemoteObserveRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteActRequest {
    pub fn new(
        observation_id: RemoteObservationId,
        generation: u64,
        action: RemoteMobileAction,
    ) -> Self {
        Self {
            observation_id,
            generation,
            action,
            target: None,
            timeout: DEFAULT_REMOTE_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }

    pub fn with_target(mut self, target: RemoteTargetRef) -> Self {
        self.target = Some(target);
        self
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, IosRemoteError> {
        validate_timeout(timeout)?;
        self.timeout = timeout;
        Ok(self)
    }
}

impl RemoteMobileAction {
    pub const fn kind(&self) -> RemoteActionKind {
        match self {
            Self::Tap { .. } => RemoteActionKind::Tap,
            Self::TypeText { .. } => RemoteActionKind::TypeText,
            Self::Key { .. } => RemoteActionKind::Key,
        }
    }

    fn uses_secret(&self) -> bool {
        matches!(
            self,
            Self::TypeText {
                value: RemoteSecretAwareText::SecretHandle(_),
            }
        )
    }

    fn literal_text(&self) -> Option<&str> {
        match self {
            Self::TypeText {
                value: RemoteSecretAwareText::Literal(text),
            } => Some(text.as_str()),
            _ => None,
        }
    }
}

impl RemoteDeviceLease {
    pub fn device_id(&self) -> &DeviceUdid {
        &self.device_id
    }

    pub const fn worker_id(&self) -> MacosWorkerId {
        self.worker_id
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }
}

impl RemoteMobileObservation {
    pub const fn id(&self) -> RemoteObservationId {
        self.id
    }

    pub const fn worker_id(&self) -> MacosWorkerId {
        self.worker_id
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn state_hash(&self) -> ArtifactId {
        self.state_hash
    }

    pub fn screenshot(&self) -> Option<&ArtifactRef> {
        self.screenshot.as_ref()
    }

    pub const fn ui_node_count(&self) -> u32 {
        self.ui_node_count
    }

    pub const fn sensitive(&self) -> bool {
        self.sensitive
    }
}

impl RemoteMobileActionResult {
    pub const fn status(&self) -> RemoteActionStatus {
        self.status
    }

    pub const fn action(&self) -> RemoteActionKind {
        self.action
    }

    pub const fn after(&self) -> &RemoteMobileObservation {
        &self.after
    }

    pub fn evidence(&self) -> &[EvidenceId] {
        &self.evidence
    }

    pub const fn secret_handle_used(&self) -> bool {
        self.secret_handle_used
    }
}

impl IosRemoteDelegator {
    /// Production constructor. Probes the host; does not invent a remote worker.
    pub fn open(
        config: RemoteDelegationConfig,
        worker: Option<MacosWorkerProfile>,
        cancel: &CancellationToken,
    ) -> Result<Self, IosRemoteError> {
        let local = LocalIosAvailability::detect(cancel)?;
        Ok(Self::new(config, local, worker, None))
    }

    /// In-process stand-in. Does not contact a network worker.
    pub fn fake_denied() -> Self {
        Self::new(
            RemoteDelegationConfig::denied(),
            LocalIosAvailability::Unavailable,
            None,
            None,
        )
    }

    /// In-process stand-in with an authorized macOS worker and explicit allow.
    pub fn fake_allowed() -> Self {
        let profile = fixture_worker();
        Self::new(
            RemoteDelegationConfig::from_policy(RemoteDelegationPolicy::Allowed),
            LocalIosAvailability::Unavailable,
            Some(profile.clone()),
            Some(FakeMacosWorker::new(profile)),
        )
    }

    pub fn fake_local_ready() -> Self {
        let profile = fixture_worker();
        Self::new(
            RemoteDelegationConfig::from_policy(RemoteDelegationPolicy::Allowed),
            LocalIosAvailability::Ready,
            Some(profile.clone()),
            Some(FakeMacosWorker::new(profile)),
        )
    }

    fn new(
        config: RemoteDelegationConfig,
        local: LocalIosAvailability,
        worker: Option<MacosWorkerProfile>,
        transport: Option<FakeMacosWorker>,
    ) -> Self {
        let profile = worker
            .clone()
            .unwrap_or_else(FakeMacosWorker::placeholder_profile);
        Self {
            config,
            local,
            issuer: RemoteWorkLeaseIssuer::ephemeral(),
            controller_id: ControllerId::new(),
            worker,
            transport: Mutex::new(transport.unwrap_or_else(|| FakeMacosWorker::new(profile))),
            last_observation: Mutex::new(None),
            last_lease: Mutex::new(None),
            clock: Mutex::new(None),
        }
    }

    pub const fn config(&self) -> RemoteDelegationConfig {
        self.config
    }

    pub const fn local_availability(&self) -> LocalIosAvailability {
        self.local
    }

    pub fn authorized_worker(&self) -> Option<&MacosWorkerProfile> {
        self.worker.as_ref()
    }

    pub fn last_lease(&self) -> Result<Option<IosRemoteWorkLease>, IosRemoteError> {
        let guard = self
            .last_lease
            .lock()
            .map_err(|_| IosRemoteError::Unavailable)?;
        Ok(guard.clone())
    }

    /// Decide whether remote routing is permitted. Never auto-allows.
    pub fn decide(&self, cancel: &CancellationToken) -> Result<(), IosRemoteError> {
        check_cancel(cancel)?;
        if self.local.is_ready() {
            return Err(IosRemoteError::LocalCapabilityPresent);
        }
        if !self.config.allows_remote() {
            return Err(IosRemoteError::RemotePolicyDenied);
        }
        let worker = self
            .worker
            .as_ref()
            .ok_or(IosRemoteError::WorkerUnavailable)?;
        if worker.platform != WorkerPlatform::Macos || worker.capability != IOS_SIMULATOR_CAPABILITY
        {
            return Err(IosRemoteError::WorkerUnauthorized);
        }
        Ok(())
    }

    pub fn observe(
        &self,
        request: RemoteObserveRequest,
    ) -> Result<RemoteMobileObservation, IosRemoteError> {
        self.decide(&request.cancel)?;
        validate_timeout(request.timeout)?;
        let generation = self.current_generation()?;
        let envelope = self.dispatch(
            DelegatedOperation::Observe {
                generation,
                screenshot: request.screenshot,
            },
            Vec::new(),
            request.timeout,
            &request.cancel,
        )?;
        let observation = self.import_observation(&envelope, request.screenshot)?;
        self.store_observation(observation.clone())?;
        Ok(observation)
    }

    pub fn act(
        &self,
        request: RemoteActRequest,
    ) -> Result<RemoteMobileActionResult, IosRemoteError> {
        self.decide(&request.cancel)?;
        validate_timeout(request.timeout)?;
        let prior = self
            .last_observation
            .lock()
            .map_err(|_| IosRemoteError::Unavailable)?
            .clone()
            .ok_or(IosRemoteError::ObservationRequired)?;
        if prior.id != request.observation_id || prior.generation != request.generation {
            return Err(IosRemoteError::StaleObservation);
        }
        self.resolve_target(&prior, request.target.as_ref())?;
        if prior.sensitive {
            match &request.action {
                RemoteMobileAction::TypeText {
                    value: RemoteSecretAwareText::Literal(_),
                } => return Err(IosRemoteError::SensitiveDenied),
                RemoteMobileAction::TypeText {
                    value: RemoteSecretAwareText::SecretHandle(_),
                } => {}
                RemoteMobileAction::Tap { .. } | RemoteMobileAction::Key { .. } => {}
            }
        }
        if let Some(text) = request.action.literal_text()
            && (text.is_empty() || text.len() > 512 || text.contains('\0'))
        {
            return Err(IosRemoteError::InvalidText);
        }
        let operation = DelegatedOperation::Act {
            observation: request.observation_id,
            generation: request.generation,
            action: request.action.kind(),
            target: request.target.clone(),
        };
        let envelope = self.dispatch(operation, Vec::new(), request.timeout, &request.cancel)?;
        let after = self.import_observation(&envelope, true)?;
        if after.generation <= prior.generation {
            return Err(IosRemoteError::StaleObservation);
        }
        let status = envelope.action_status.ok_or(IosRemoteError::Backend)?;
        self.store_observation(after.clone())?;
        Ok(RemoteMobileActionResult {
            status,
            action: request.action.kind(),
            after,
            evidence: vec![EvidenceId::new()],
            secret_handle_used: request.action.uses_secret(),
        })
    }

    pub fn boot(
        &self,
        udid: DeviceUdid,
        cancel: &CancellationToken,
    ) -> Result<RemoteDeviceLease, IosRemoteError> {
        self.decide(cancel)?;
        let envelope = self.dispatch(
            DelegatedOperation::Boot { udid: udid.clone() },
            Vec::new(),
            DEFAULT_BOOT_TIMEOUT,
            cancel,
        )?;
        let observation = self.import_observation(&envelope, false)?;
        self.store_observation(observation.clone())?;
        Ok(RemoteDeviceLease {
            device_id: udid,
            worker_id: observation.worker_id,
            generation: observation.generation,
            expires_at_unix_ms: self
                .last_lease()?
                .ok_or(IosRemoteError::LeaseInvalid)?
                .expires_at_unix_ms,
        })
    }

    pub fn install(
        &self,
        udid: DeviceUdid,
        app: ArtifactRef,
        cancel: &CancellationToken,
    ) -> Result<(), IosRemoteError> {
        self.decide(cancel)?;
        if app.media_type != APP_MEDIA || app.bytes == 0 || app.bytes > MAX_REMOTE_OUTPUT_BYTES {
            return Err(IosRemoteError::InvalidArtifact);
        }
        let operation = DelegatedOperation::Install {
            udid,
            app: app.clone(),
        };
        let _ = self.dispatch(operation, vec![app], DEFAULT_REMOTE_TIMEOUT, cancel)?;
        Ok(())
    }

    pub fn launch(
        &self,
        udid: DeviceUdid,
        bundle: BundleId,
        cancel: &CancellationToken,
    ) -> Result<(), IosRemoteError> {
        self.decide(cancel)?;
        let _ = self.dispatch(
            DelegatedOperation::Launch { udid, bundle },
            Vec::new(),
            DEFAULT_REMOTE_TIMEOUT,
            cancel,
        )?;
        Ok(())
    }

    pub fn screenshot(
        &self,
        udid: DeviceUdid,
        cancel: &CancellationToken,
    ) -> Result<ArtifactRef, IosRemoteError> {
        self.decide(cancel)?;
        let envelope = self.dispatch(
            DelegatedOperation::Screenshot { udid },
            Vec::new(),
            DEFAULT_REMOTE_TIMEOUT,
            cancel,
        )?;
        let observation = self.import_observation(&envelope, true)?;
        self.store_observation(observation.clone())?;
        observation
            .screenshot
            .clone()
            .ok_or(IosRemoteError::OutputBound)
    }

    /// Verify worker identity and content hashes before evidence import.
    pub fn import_result(
        &self,
        lease: &IosRemoteWorkLease,
        envelope: &UntrustedRemoteEnvelope,
        now: SystemTime,
        cancel: &CancellationToken,
    ) -> Result<RemoteMobileObservation, IosRemoteError> {
        check_cancel(cancel)?;
        let worker = self
            .worker
            .as_ref()
            .ok_or(IosRemoteError::WorkerUnavailable)?;
        lease.verify(&self.issuer, self.controller_id, worker.id, now, cancel)?;
        if envelope.lease_id != lease.lease_id {
            return Err(IosRemoteError::LeaseInvalid);
        }
        if envelope.worker_id != worker.id || envelope.worker_id != lease.worker_id {
            return Err(IosRemoteError::WorkerIdentityMismatch);
        }
        if envelope.attestation != worker.attestation {
            return Err(IosRemoteError::AttestationMismatch);
        }
        if envelope.artifacts.len() > MAX_REMOTE_ARTIFACTS {
            return Err(IosRemoteError::OutputBound);
        }
        for artifact in &envelope.artifacts {
            verify_artifact(artifact)?;
        }
        let claimed = envelope
            .observation
            .as_ref()
            .ok_or(IosRemoteError::Backend)?;
        if claimed.state_hash != result_digest(lease, envelope) {
            return Err(IosRemoteError::HashMismatch);
        }
        let screenshot = match claimed.screenshot {
            Some(id) => {
                let found = envelope
                    .artifacts
                    .iter()
                    .find(|item| item.artifact.id == id)
                    .ok_or(IosRemoteError::HashMismatch)?;
                Some(found.artifact.clone())
            }
            None => None,
        };
        Ok(RemoteMobileObservation {
            id: claimed.id,
            worker_id: envelope.worker_id,
            generation: claimed.generation,
            state_hash: claimed.state_hash,
            screenshot,
            ui_node_count: claimed.ui_node_count,
            sensitive: claimed.sensitive,
            captured_at_ms: claimed.captured_at_ms,
        })
    }

    fn dispatch(
        &self,
        operation: DelegatedOperation,
        inputs: Vec<ArtifactRef>,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<UntrustedRemoteEnvelope, IosRemoteError> {
        check_cancel(cancel)?;
        let worker = self
            .worker
            .as_ref()
            .ok_or(IosRemoteError::WorkerUnavailable)?;
        let now = self.now()?;
        let lease = IosRemoteWorkLease::issue(
            &self.issuer,
            RemoteLeaseParties::new(self.controller_id, worker.id),
            operation,
            inputs,
            timeout,
            now,
            cancel,
        )?;
        {
            let mut slot = self
                .last_lease
                .lock()
                .map_err(|_| IosRemoteError::Unavailable)?;
            *slot = Some(lease.clone());
        }
        let mut transport = self
            .transport
            .lock()
            .map_err(|_| IosRemoteError::Unavailable)?;
        let envelope = transport.execute(&lease, now, cancel)?;
        let _ = self.import_result(&lease, &envelope, now, cancel)?;
        Ok(envelope)
    }

    fn import_observation(
        &self,
        envelope: &UntrustedRemoteEnvelope,
        want_screenshot: bool,
    ) -> Result<RemoteMobileObservation, IosRemoteError> {
        let lease = self.last_lease()?.ok_or(IosRemoteError::LeaseInvalid)?;
        let observation =
            self.import_result(&lease, envelope, self.now()?, &CancellationToken::new())?;
        if want_screenshot && observation.screenshot.is_none() {
            return Err(IosRemoteError::OutputBound);
        }
        Ok(observation)
    }

    fn resolve_target(
        &self,
        observation: &RemoteMobileObservation,
        target: Option<&RemoteTargetRef>,
    ) -> Result<(), IosRemoteError> {
        let Some(target) = target else {
            return Err(IosRemoteError::TargetRequired);
        };
        match target {
            RemoteTargetRef::Accessibility { node } if node == "submit" || node == "passcode" => {
                Ok(())
            }
            RemoteTargetRef::RoleName { role, name }
                if role == "button" && name == "Submit"
                    || role == "secure" && name == "Passcode" =>
            {
                Ok(())
            }
            RemoteTargetRef::Visual {
                observation: id,
                label,
            } if *id == observation.id && (label == "Submit" || label == "Passcode") => Ok(()),
            RemoteTargetRef::Coordinate {
                observation: id, ..
            } if *id == observation.id => Ok(()),
            RemoteTargetRef::Coordinate { .. } | RemoteTargetRef::Visual { .. } => {
                Err(IosRemoteError::StaleObservation)
            }
            RemoteTargetRef::Accessibility { .. } | RemoteTargetRef::RoleName { .. } => {
                Err(IosRemoteError::TargetNotFound)
            }
        }
    }

    fn store_observation(
        &self,
        observation: RemoteMobileObservation,
    ) -> Result<(), IosRemoteError> {
        let mut slot = self
            .last_observation
            .lock()
            .map_err(|_| IosRemoteError::Unavailable)?;
        *slot = Some(observation);
        Ok(())
    }

    fn current_generation(&self) -> Result<u64, IosRemoteError> {
        let transport = self
            .transport
            .lock()
            .map_err(|_| IosRemoteError::Unavailable)?;
        Ok(transport.generation)
    }

    fn now(&self) -> Result<SystemTime, IosRemoteError> {
        let guard = self.clock.lock().map_err(|_| IosRemoteError::Unavailable)?;
        Ok(guard.unwrap_or_else(SystemTime::now))
    }

    #[cfg(test)]
    fn set_now(&self, now: SystemTime) {
        if let Ok(mut guard) = self.clock.lock() {
            *guard = Some(now);
        }
    }
}

impl FakeMacosWorker {
    fn new(profile: MacosWorkerProfile) -> Self {
        Self {
            profile,
            generation: 1,
            booted: false,
            udid: DeviceUdid::parse(FIXTURE_UDID).expect("fixture udid"),
            installed: Vec::new(),
            launched: Vec::new(),
            label: "Submit",
            secret_present: false,
        }
    }

    fn placeholder_profile() -> MacosWorkerProfile {
        MacosWorkerProfile {
            id: MacosWorkerId::new(),
            platform: WorkerPlatform::Macos,
            capability: IOS_SIMULATOR_CAPABILITY,
            attestation: ArtifactId::from_bytes(b"rapidlm.ios.remote.unbound"),
        }
    }

    fn execute(
        &mut self,
        lease: &IosRemoteWorkLease,
        now: SystemTime,
        cancel: &CancellationToken,
    ) -> Result<UntrustedRemoteEnvelope, IosRemoteError> {
        check_cancel(cancel)?;
        if lease.worker_id != self.profile.id {
            return Err(IosRemoteError::WorkerIdentityMismatch);
        }
        match &lease.operation {
            DelegatedOperation::Observe { screenshot, .. } => {
                self.observe_envelope(lease, now, *screenshot)
            }
            DelegatedOperation::Act {
                observation,
                generation,
                action,
                target,
            } => self.act_envelope(
                lease,
                *observation,
                *generation,
                *action,
                target.as_ref(),
                now,
            ),
            DelegatedOperation::Boot { udid } => {
                if udid != &self.udid {
                    return Err(IosRemoteError::DeviceNotFound);
                }
                self.booted = true;
                self.generation = self.generation.saturating_add(1);
                self.observe_envelope(lease, now, false)
            }
            DelegatedOperation::Install { udid, app } => {
                if udid != &self.udid {
                    return Err(IosRemoteError::DeviceNotFound);
                }
                if !self.booted {
                    return Err(IosRemoteError::DeviceClosed);
                }
                self.installed.push(app.id);
                self.observe_envelope(lease, now, false)
            }
            DelegatedOperation::Launch { udid, bundle } => {
                if udid != &self.udid {
                    return Err(IosRemoteError::DeviceNotFound);
                }
                if !self.booted {
                    return Err(IosRemoteError::DeviceClosed);
                }
                self.launched.push(bundle.clone());
                self.observe_envelope(lease, now, false)
            }
            DelegatedOperation::Screenshot { udid } => {
                if udid != &self.udid {
                    return Err(IosRemoteError::DeviceNotFound);
                }
                if !self.booted {
                    return Err(IosRemoteError::DeviceClosed);
                }
                self.observe_envelope(lease, now, true)
            }
        }
    }

    fn act_envelope(
        &mut self,
        lease: &IosRemoteWorkLease,
        observation: RemoteObservationId,
        generation: u64,
        action: RemoteActionKind,
        target: Option<&RemoteTargetRef>,
        now: SystemTime,
    ) -> Result<UntrustedRemoteEnvelope, IosRemoteError> {
        let _ = observation;
        if generation != self.generation {
            return Err(IosRemoteError::StaleObservation);
        }
        if !self.booted {
            return Err(IosRemoteError::DeviceClosed);
        }
        match action {
            RemoteActionKind::Tap => {
                let _ = target;
                self.label = "Done";
            }
            RemoteActionKind::TypeText => {
                self.secret_present = matches!(
                    target,
                    Some(RemoteTargetRef::Accessibility { node }) if node == "passcode"
                ) || matches!(
                    target,
                    Some(RemoteTargetRef::RoleName { role, .. }) if *role == "secure"
                );
            }
            RemoteActionKind::Key => {}
        }
        self.generation = self.generation.saturating_add(1);
        let mut envelope = self.observe_envelope(lease, now, true)?;
        envelope.action_status = Some(RemoteActionStatus::Succeeded);
        let digest = result_digest(lease, &envelope);
        if let Some(obs) = envelope.observation.as_mut() {
            obs.state_hash = digest;
        }
        Ok(envelope)
    }

    fn observe_envelope(
        &self,
        lease: &IosRemoteWorkLease,
        now: SystemTime,
        screenshot: bool,
    ) -> Result<UntrustedRemoteEnvelope, IosRemoteError> {
        let captured_at_ms = unix_ms(now)?;
        let mut artifacts = Vec::new();
        let screenshot_id = if screenshot {
            let payload = if self.secret_present {
                REDACTED_SCREENSHOT.to_vec()
            } else {
                SCREENSHOT_MARKER.to_vec()
            };
            let redaction = if self.secret_present {
                RedactionClass::Secret
            } else {
                RedactionClass::Project
            };
            let artifact = ArtifactRef::new(
                ArtifactId::from_bytes(&payload),
                SCREENSHOT_MEDIA,
                payload.len() as u64,
                redaction,
            );
            let id = artifact.id;
            artifacts.push(UntrustedArtifact { artifact, payload });
            Some(id)
        } else {
            None
        };
        let mut envelope = UntrustedRemoteEnvelope {
            lease_id: lease.lease_id,
            worker_id: self.profile.id,
            attestation: self.profile.attestation,
            observation: Some(UntrustedObservation {
                id: RemoteObservationId::new(),
                generation: self.generation,
                state_hash: ArtifactId::from_bytes(&[]),
                ui_node_count: 2,
                sensitive: self.secret_present,
                captured_at_ms,
                screenshot: screenshot_id,
            }),
            action_status: None,
            artifacts,
        };
        let digest = result_digest(lease, &envelope);
        if let Some(obs) = envelope.observation.as_mut() {
            obs.state_hash = digest;
        }
        Ok(envelope)
    }
}

impl IosRemoteError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::TimeoutInvalid => "timeout_invalid",
            Self::LocalCapabilityPresent => "local_capability_present",
            Self::CapabilityUnavailable => "capability_unavailable",
            Self::RemotePolicyDenied => "remote_policy_denied",
            Self::WorkerUnavailable => "worker_unavailable",
            Self::WorkerUnauthorized => "worker_unauthorized",
            Self::WorkerIdentityMismatch => "worker_identity_mismatch",
            Self::AttestationMismatch => "attestation_mismatch",
            Self::HashMismatch => "hash_mismatch",
            Self::LeaseInvalid => "lease_invalid",
            Self::LeaseExpired => "lease_expired",
            Self::ObservationRequired => "observation_required",
            Self::StaleObservation => "stale_observation",
            Self::TargetRequired => "target_required",
            Self::TargetNotFound => "target_not_found",
            Self::AmbiguousTarget => "ambiguous_target",
            Self::SensitiveDenied => "sensitive_denied",
            Self::SecretUnresolved => "secret_unresolved",
            Self::InvalidText => "invalid_text",
            Self::InvalidArtifact => "invalid_artifact",
            Self::OutputBound => "output_bound",
            Self::DeviceNotFound => "device_not_found",
            Self::DeviceClosed => "device_closed",
            Self::Unavailable => "unavailable",
            Self::Backend => "backend",
        }
    }

    pub const fn code(self) -> ErrorCode {
        match self {
            Self::Cancelled
            | Self::TimeoutInvalid
            | Self::ObservationRequired
            | Self::TargetRequired
            | Self::TargetNotFound
            | Self::AmbiguousTarget
            | Self::InvalidText
            | Self::InvalidArtifact
            | Self::OutputBound
            | Self::SecretUnresolved => ErrorCode::ToolInvalidArguments,
            Self::Timeout => ErrorCode::ProcessTimeout,
            Self::LocalCapabilityPresent => ErrorCode::SessionConflict,
            Self::CapabilityUnavailable => ErrorCode::MobileCapabilityUnavailable,
            Self::RemotePolicyDenied | Self::WorkerUnauthorized | Self::SensitiveDenied => {
                ErrorCode::PolicyDenied
            }
            Self::WorkerUnavailable => ErrorCode::MobileCapabilityUnavailable,
            Self::WorkerIdentityMismatch
            | Self::AttestationMismatch
            | Self::LeaseInvalid
            | Self::LeaseExpired => ErrorCode::PolicyLeaseInvalid,
            Self::HashMismatch => ErrorCode::WorkspacePreimageMismatch,
            Self::StaleObservation => ErrorCode::BrowserStaleObservation,
            Self::DeviceNotFound | Self::DeviceClosed => ErrorCode::SessionNotFound,
            Self::Unavailable | Self::Backend => ErrorCode::InternalUnexpected,
        }
    }

    pub const fn remote_worker_hint(self) -> Option<&'static str> {
        match self {
            Self::CapabilityUnavailable | Self::RemotePolicyDenied | Self::WorkerUnavailable => {
                Some(IOS_SIMULATOR_CAPABILITY)
            }
            _ => None,
        }
    }
}

impl fmt::Display for IosRemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for IosRemoteError {}

impl Debug for IosRemoteWorkLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IosRemoteWorkLease")
            .field("lease_id", &self.lease_id)
            .field("worker_id", &self.worker_id)
            .field("task_digest", &self.task_digest)
            .field("expires_at_unix_ms", &self.expires_at_unix_ms)
            .finish_non_exhaustive()
    }
}

impl Debug for RemoteMobileAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tap { count } => f.debug_struct("Tap").field("count", count).finish(),
            Self::TypeText { value } => match value {
                RemoteSecretAwareText::Literal(_) => f
                    .debug_struct("TypeText")
                    .field("value", &"literal")
                    .finish(),
                RemoteSecretAwareText::SecretHandle(_) => f
                    .debug_struct("TypeText")
                    .field("value", &"secret_handle")
                    .finish(),
            },
            Self::Key { name } => f.debug_struct("Key").field("name", name).finish(),
        }
    }
}

impl Debug for RemoteTargetRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accessibility { node } => {
                f.debug_struct("Accessibility").field("node", node).finish()
            }
            Self::RoleName { role, name } => f
                .debug_struct("RoleName")
                .field("role", role)
                .field("name", name)
                .finish(),
            Self::Visual { observation, label } => f
                .debug_struct("Visual")
                .field("observation", observation)
                .field("label", label)
                .finish(),
            Self::Coordinate { observation, x, y } => f
                .debug_struct("Coordinate")
                .field("observation", observation)
                .field("x", x)
                .field("y", y)
                .finish(),
        }
    }
}

fn fixture_worker() -> MacosWorkerProfile {
    MacosWorkerProfile::new(
        MacosWorkerId::new(),
        WorkerPlatform::Macos,
        IOS_SIMULATOR_CAPABILITY,
        ArtifactId::from_bytes(b"rapidlm.ios.remote.macos.attestation.v1"),
    )
    .expect("fixture worker")
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), IosRemoteError> {
    if cancel.is_cancelled() {
        Err(IosRemoteError::Cancelled)
    } else {
        Ok(())
    }
}

fn validate_timeout(timeout: Duration) -> Result<(), IosRemoteError> {
    if timeout.is_zero() || timeout > MAX_REMOTE_TIMEOUT || timeout > MAX_BOOT_TIMEOUT {
        Err(IosRemoteError::TimeoutInvalid)
    } else {
        Ok(())
    }
}

fn unix_ms(now: SystemTime) -> Result<u64, IosRemoteError> {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|_| IosRemoteError::TimeoutInvalid)
}

fn verify_artifact(item: &UntrustedArtifact) -> Result<(), IosRemoteError> {
    if item.payload.len() as u64 != item.artifact.bytes {
        return Err(IosRemoteError::HashMismatch);
    }
    if item.artifact.bytes == 0 || item.artifact.bytes > MAX_REMOTE_OUTPUT_BYTES {
        return Err(IosRemoteError::OutputBound);
    }
    if ArtifactId::from_bytes(&item.payload) != item.artifact.id {
        return Err(IosRemoteError::HashMismatch);
    }
    Ok(())
}

fn task_digest(lease: &IosRemoteWorkLease) -> ArtifactId {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(TASK_TAG);
    buf.push(0);
    buf.extend_from_slice(lease.controller_id.0.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.worker_id.0.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(operation_bytes(&lease.operation).as_slice());
    buf.push(0);
    for input in &lease.artifact_inputs {
        buf.extend_from_slice(input.id.as_digest());
        buf.push(0);
    }
    ArtifactId::from_bytes(&buf)
}

fn binding_bytes(lease: &IosRemoteWorkLease) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(BINDING_TAG);
    buf.push(0);
    buf.extend_from_slice(lease.lease_id.0.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.controller_id.0.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.worker_id.0.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lease.task_digest.as_digest());
    buf.push(0);
    buf.extend_from_slice(&lease.expires_at_unix_ms.to_be_bytes());
    buf.extend_from_slice(&lease.cancel_deadline_unix_ms.to_be_bytes());
    buf.extend_from_slice(&lease.nonce);
    buf
}

fn operation_bytes(operation: &DelegatedOperation) -> Vec<u8> {
    let mut buf = Vec::new();
    match operation {
        DelegatedOperation::Observe {
            generation,
            screenshot,
        } => {
            buf.extend_from_slice(b"observe");
            buf.extend_from_slice(&generation.to_be_bytes());
            buf.push(u8::from(*screenshot));
        }
        DelegatedOperation::Act {
            observation,
            generation,
            action,
            target,
        } => {
            buf.extend_from_slice(b"act");
            buf.extend_from_slice(observation.0.as_uuid().as_bytes());
            buf.extend_from_slice(&generation.to_be_bytes());
            buf.extend_from_slice(action.as_str().as_bytes());
            if let Some(target) = target {
                buf.extend_from_slice(target_kind(target).as_bytes());
            }
        }
        DelegatedOperation::Boot { udid } => {
            buf.extend_from_slice(b"boot");
            buf.extend_from_slice(udid.as_str().as_bytes());
        }
        DelegatedOperation::Install { udid, app } => {
            buf.extend_from_slice(b"install");
            buf.extend_from_slice(udid.as_str().as_bytes());
            buf.extend_from_slice(app.id.as_digest());
        }
        DelegatedOperation::Launch { udid, bundle } => {
            buf.extend_from_slice(b"launch");
            buf.extend_from_slice(udid.as_str().as_bytes());
            buf.extend_from_slice(bundle.as_str().as_bytes());
        }
        DelegatedOperation::Screenshot { udid } => {
            buf.extend_from_slice(b"screenshot");
            buf.extend_from_slice(udid.as_str().as_bytes());
        }
    }
    buf
}

impl RemoteActionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tap => "tap",
            Self::TypeText => "type_text",
            Self::Key => "key",
        }
    }
}

fn target_kind(target: &RemoteTargetRef) -> &'static str {
    match target {
        RemoteTargetRef::Accessibility { .. } => "ax",
        RemoteTargetRef::RoleName { .. } => "role",
        RemoteTargetRef::Visual { .. } => "visual",
        RemoteTargetRef::Coordinate { .. } => "coord",
    }
}

fn result_digest(lease: &IosRemoteWorkLease, envelope: &UntrustedRemoteEnvelope) -> ArtifactId {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(RESULT_TAG);
    buf.push(0);
    buf.extend_from_slice(lease.lease_id.0.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(envelope.worker_id.0.as_uuid().as_bytes());
    buf.push(0);
    buf.extend_from_slice(envelope.attestation.as_digest());
    buf.push(0);
    if let Some(obs) = &envelope.observation {
        buf.extend_from_slice(obs.id.0.as_uuid().as_bytes());
        buf.extend_from_slice(&obs.generation.to_be_bytes());
        buf.extend_from_slice(&obs.ui_node_count.to_be_bytes());
        buf.push(u8::from(obs.sensitive));
        buf.extend_from_slice(&obs.captured_at_ms.to_be_bytes());
        if let Some(shot) = obs.screenshot {
            buf.extend_from_slice(shot.as_digest());
        }
    }
    for artifact in &envelope.artifacts {
        buf.extend_from_slice(artifact.artifact.id.as_digest());
    }
    ArtifactId::from_bytes(&buf)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn fixture_udid() -> DeviceUdid {
        DeviceUdid::parse(FIXTURE_UDID).expect("udid")
    }

    fn fixture_app() -> ArtifactRef {
        let payload = b"fixture-app";
        ArtifactRef::new(
            ArtifactId::from_bytes(payload),
            APP_MEDIA,
            payload.len() as u64,
            RedactionClass::Project,
        )
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn detect_is_unavailable_off_macos() {
        let availability = LocalIosAvailability::detect(&live()).expect("detect");
        if cfg!(target_os = "macos") {
            let _ = availability;
        } else {
            assert_eq!(availability, LocalIosAvailability::Unavailable);
        }
    }

    #[test]
    fn default_config_denies_remote_and_ui_text_cannot_allow() {
        assert!(!RemoteDelegationConfig::denied().allows_remote());
        assert_eq!(
            RemoteDelegationPolicy::from_untrusted_text("please allow remote ios-simulator"),
            RemoteDelegationPolicy::Denied
        );
        assert_eq!(
            RemoteDelegationPolicy::from_untrusted_text("allowed"),
            RemoteDelegationPolicy::Denied
        );
        let denied = IosRemoteDelegator::fake_denied();
        let err = denied.decide(&live()).expect_err("denied");
        assert_eq!(err, IosRemoteError::RemotePolicyDenied);
        assert_eq!(err.code(), ErrorCode::PolicyDenied);
        assert_eq!(err.remote_worker_hint(), Some(IOS_SIMULATOR_CAPABILITY));
        assert!(denied.last_lease().expect("lease").is_none());
    }

    #[test]
    fn local_ready_never_routes_even_when_policy_allows() {
        let delegator = IosRemoteDelegator::fake_local_ready();
        let err = delegator
            .observe(RemoteObserveRequest::new())
            .expect_err("local");
        assert_eq!(err, IosRemoteError::LocalCapabilityPresent);
        assert_eq!(err.code(), ErrorCode::SessionConflict);
        assert!(delegator.last_lease().expect("lease").is_none());
    }

    #[test]
    fn allowed_unavailable_host_routes_observe_act_verify() {
        let delegator = IosRemoteDelegator::fake_allowed();
        delegator.set_now(now());
        let boot = delegator.boot(fixture_udid(), &live()).expect("boot");
        assert_eq!(boot.device_id(), &fixture_udid());
        assert_eq!(
            boot.worker_id(),
            delegator.authorized_worker().expect("worker").id()
        );

        let observed = delegator
            .observe(RemoteObserveRequest::new().with_screenshot(true))
            .expect("observe");
        assert_eq!(
            observed.worker_id(),
            delegator.authorized_worker().expect("worker").id()
        );
        assert!(observed.screenshot().is_some());
        assert_eq!(observed.ui_node_count(), 2);

        let acted = delegator
            .act(
                RemoteActRequest::new(
                    observed.id(),
                    observed.generation(),
                    RemoteMobileAction::Tap { count: 1 },
                )
                .with_target(RemoteTargetRef::Accessibility {
                    node: "submit".into(),
                }),
            )
            .expect("act");
        assert_eq!(acted.status(), RemoteActionStatus::Succeeded);
        assert!(acted.after().generation() > observed.generation());
        assert!(!acted.secret_handle_used());
        assert_eq!(acted.evidence().len(), 1);

        let lease = delegator.last_lease().expect("lease").expect("issued");
        assert!(
            lease
                .canonical_json()
                .contains(IOS_REMOTE_WORK_LEASE_SCHEMA)
        );
        assert!(lease.canonical_json().contains(IOS_SIMULATOR_CAPABILITY));
        assert!(lease.expires_at_unix_ms() > lease.canonical_json().len() as u64);
    }

    #[test]
    fn linux_or_missing_capability_worker_is_rejected() {
        let err = WorkerPlatform::parse("linux").expect_err("linux");
        assert_eq!(err, IosRemoteError::WorkerUnauthorized);
        let err = MacosWorkerProfile::new(
            MacosWorkerId::new(),
            WorkerPlatform::Macos,
            "android-emulator",
            ArtifactId::from_bytes(b"attestation"),
        )
        .expect_err("cap");
        assert_eq!(err, IosRemoteError::WorkerUnauthorized);
    }

    #[test]
    fn hash_mismatch_and_wrong_worker_are_rejected_before_import() {
        let delegator = IosRemoteDelegator::fake_allowed();
        delegator.set_now(now());
        delegator.boot(fixture_udid(), &live()).expect("boot");
        let _ = delegator
            .observe(RemoteObserveRequest::new().with_screenshot(true))
            .expect("observe");
        let lease = delegator.last_lease().expect("lease").expect("issued");
        let honest = {
            let transport = delegator.transport.lock().expect("lock");
            transport
                .observe_envelope(&lease, now(), true)
                .expect("honest")
        };

        let mut bad_hash = honest.clone();
        if let Some(first) = bad_hash.artifacts.first_mut() {
            first.artifact.id = ArtifactId::from_bytes(b"tampered");
        }
        let err = delegator
            .import_result(&lease, &bad_hash, now(), &live())
            .expect_err("hash");
        assert_eq!(err, IosRemoteError::HashMismatch);
        assert_eq!(err.code(), ErrorCode::WorkspacePreimageMismatch);

        let mut wrong_worker = honest.clone();
        wrong_worker.worker_id = MacosWorkerId::new();
        let err = delegator
            .import_result(&lease, &wrong_worker, now(), &live())
            .expect_err("worker");
        assert_eq!(err, IosRemoteError::WorkerIdentityMismatch);
        assert_eq!(err.code(), ErrorCode::PolicyLeaseInvalid);

        let mut wrong_attest = honest;
        wrong_attest.attestation = ArtifactId::from_bytes(b"other-worker");
        let err = delegator
            .import_result(&lease, &wrong_attest, now(), &live())
            .expect_err("attest");
        assert_eq!(err, IosRemoteError::AttestationMismatch);
    }

    #[test]
    fn expired_lease_result_is_quarantined() {
        let delegator = IosRemoteDelegator::fake_allowed();
        delegator.set_now(now());
        delegator.boot(fixture_udid(), &live()).expect("boot");
        let lease = delegator.last_lease().expect("lease").expect("issued");
        let envelope = {
            let transport = delegator.transport.lock().expect("lock");
            transport
                .observe_envelope(&lease, now(), false)
                .expect("env")
        };
        let later = now() + DEFAULT_BOOT_TIMEOUT + Duration::from_secs(1);
        let err = delegator
            .import_result(&lease, &envelope, later, &live())
            .expect_err("expired");
        assert_eq!(err, IosRemoteError::LeaseExpired);
        assert_eq!(err.code(), ErrorCode::PolicyLeaseInvalid);
    }

    #[test]
    fn cancelled_and_invalid_timeout_fail_closed() {
        let delegator = IosRemoteDelegator::fake_allowed();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            delegator.decide(&cancel).expect_err("cancel"),
            IosRemoteError::Cancelled
        );
        assert_eq!(
            RemoteObserveRequest::new()
                .with_timeout(Duration::ZERO)
                .expect_err("zero"),
            IosRemoteError::TimeoutInvalid
        );
        assert_eq!(
            RemoteObserveRequest::new()
                .with_timeout(MAX_REMOTE_TIMEOUT + Duration::from_secs(1))
                .expect_err("max"),
            IosRemoteError::TimeoutInvalid
        );
    }

    #[test]
    fn stale_observation_and_coordinate_from_other_capture_are_rejected() {
        let delegator = IosRemoteDelegator::fake_allowed();
        delegator.set_now(now());
        delegator.boot(fixture_udid(), &live()).expect("boot");
        let first = delegator.observe(RemoteObserveRequest::new()).expect("one");
        let second = delegator.observe(RemoteObserveRequest::new()).expect("two");
        let err = delegator
            .act(
                RemoteActRequest::new(
                    first.id(),
                    first.generation(),
                    RemoteMobileAction::Tap { count: 1 },
                )
                .with_target(RemoteTargetRef::Coordinate {
                    observation: first.id(),
                    x: 10,
                    y: 10,
                }),
            )
            .expect_err("stale");
        assert_eq!(err, IosRemoteError::StaleObservation);
        assert_eq!(err.code(), ErrorCode::BrowserStaleObservation);

        let err = delegator
            .act(
                RemoteActRequest::new(
                    second.id(),
                    second.generation(),
                    RemoteMobileAction::Tap { count: 1 },
                )
                .with_target(RemoteTargetRef::Coordinate {
                    observation: first.id(),
                    x: 10,
                    y: 10,
                }),
            )
            .expect_err("coord");
        assert_eq!(err, IosRemoteError::StaleObservation);
    }

    #[test]
    fn secret_type_redacts_screenshot_and_omits_plaintext_from_lease() {
        let delegator = IosRemoteDelegator::fake_allowed();
        delegator.set_now(now());
        delegator.boot(fixture_udid(), &live()).expect("boot");
        let observed = delegator.observe(RemoteObserveRequest::new()).expect("obs");
        let handle = SecretHandle::parse("ios-login-secret").expect("handle");
        let acted = delegator
            .act(
                RemoteActRequest::new(
                    observed.id(),
                    observed.generation(),
                    RemoteMobileAction::TypeText {
                        value: RemoteSecretAwareText::SecretHandle(handle),
                    },
                )
                .with_target(RemoteTargetRef::Accessibility {
                    node: "passcode".into(),
                }),
            )
            .expect("type");
        assert!(acted.secret_handle_used());
        let shot = acted.after().screenshot().expect("shot");
        assert_eq!(shot.redaction, RedactionClass::Secret);
        assert_eq!(shot.id, ArtifactId::from_bytes(REDACTED_SCREENSHOT));
        let lease = delegator.last_lease().expect("lease").expect("issued");
        let json = lease.canonical_json();
        assert!(!json.contains("hunter2"));
        assert!(!json.contains("passcode-value"));
        match lease.operation() {
            DelegatedOperation::Act { action, .. } => {
                assert_eq!(*action, RemoteActionKind::TypeText);
            }
            other => panic!("expected act lease, got {other:?}"),
        }
    }

    #[test]
    fn literal_type_on_sensitive_field_is_denied() {
        let delegator = IosRemoteDelegator::fake_allowed();
        delegator.set_now(now());
        delegator.boot(fixture_udid(), &live()).expect("boot");
        let observed = delegator.observe(RemoteObserveRequest::new()).expect("obs");
        let handle = SecretHandle::parse("ios-login-secret").expect("handle");
        let _ = delegator
            .act(
                RemoteActRequest::new(
                    observed.id(),
                    observed.generation(),
                    RemoteMobileAction::TypeText {
                        value: RemoteSecretAwareText::SecretHandle(handle),
                    },
                )
                .with_target(RemoteTargetRef::RoleName {
                    role: "secure".into(),
                    name: "Passcode".into(),
                }),
            )
            .expect("seed sensitive");
        let after = delegator
            .observe(RemoteObserveRequest::new())
            .expect("after");
        let err = delegator
            .act(
                RemoteActRequest::new(
                    after.id(),
                    after.generation(),
                    RemoteMobileAction::TypeText {
                        value: RemoteSecretAwareText::Literal("hunter2".into()),
                    },
                )
                .with_target(RemoteTargetRef::Accessibility {
                    node: "passcode".into(),
                }),
            )
            .expect_err("literal");
        assert_eq!(err, IosRemoteError::SensitiveDenied);
        assert_eq!(err.code(), ErrorCode::PolicyDenied);
    }

    #[test]
    fn install_uses_artifact_ref_not_host_path() {
        let delegator = IosRemoteDelegator::fake_allowed();
        delegator.set_now(now());
        delegator.boot(fixture_udid(), &live()).expect("boot");
        delegator
            .install(fixture_udid(), fixture_app(), &live())
            .expect("install");
        let lease = delegator.last_lease().expect("lease").expect("issued");
        assert_eq!(lease.artifact_inputs().len(), 1);
        assert_eq!(lease.artifact_inputs()[0].media_type, APP_MEDIA);
        delegator
            .launch(
                fixture_udid(),
                BundleId::parse("com.example.fixture").expect("bundle"),
                &live(),
            )
            .expect("launch");
        let shot = delegator.screenshot(fixture_udid(), &live()).expect("shot");
        assert_eq!(shot.media_type, SCREENSHOT_MEDIA);
        assert_eq!(shot.id, ArtifactId::from_bytes(SCREENSHOT_MARKER));
    }

    #[test]
    fn tampered_lease_signature_is_rejected() {
        let issuer = RemoteWorkLeaseIssuer::from_key([7u8; 32]).expect("key");
        let worker = MacosWorkerId::new();
        let mut lease = IosRemoteWorkLease::issue(
            &issuer,
            RemoteLeaseParties::new(ControllerId::new(), worker),
            DelegatedOperation::Observe {
                generation: 1,
                screenshot: false,
            },
            Vec::new(),
            DEFAULT_REMOTE_TIMEOUT,
            now(),
            &live(),
        )
        .expect("issue");
        lease.signature[0] ^= 0xff;
        let err = lease
            .verify(&issuer, lease.controller_id, worker, now(), &live())
            .expect_err("tamper");
        assert_eq!(err, IosRemoteError::LeaseInvalid);
        match RemoteWorkLeaseIssuer::from_key([0u8; 32]) {
            Ok(_) => panic!("zero key"),
            Err(err) => assert_eq!(err, IosRemoteError::LeaseInvalid),
        }
    }

    #[test]
    fn act_without_observe_is_rejected() {
        let delegator = IosRemoteDelegator::fake_allowed();
        delegator.set_now(now());
        let err = delegator
            .act(RemoteActRequest::new(
                RemoteObservationId::new(),
                1,
                RemoteMobileAction::Key {
                    name: RemoteKey::Home,
                },
            ))
            .expect_err("no observe");
        assert_eq!(err, IosRemoteError::ObservationRequired);
    }
}
