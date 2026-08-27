//! Sandbox backend trait, advertised capabilities, and no-downgrade selection.
//!
//! Isolation rank is derived from [`protocol::SandboxTier`]. A required tier
//! never selects a weaker backend, including when that backend claims `supports`.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use capability_broker::{
    CancellationToken, CanonicalHostPath, Capability, CapabilityLease, SecretHandle,
};
use protocol::{ErrorCode, LeaseId, RepoPath, RuntimeId, SandboxTier};

/// Maximum mounts accepted on one [`SandboxSpec`].
pub const MAX_MOUNTS: usize = 32;

/// Maximum env-allowlist names on one [`SandboxSpec`].
pub const MAX_ENV_NAMES: usize = 256;

/// Maximum UTF-8 bytes for one env-allowlist name or image reference.
pub const MAX_IDENT_BYTES: usize = 256;

/// Maximum argv tokens on [`SandboxExecRequest`].
pub const MAX_ARGV: usize = 256;

/// Maximum UTF-8 bytes for one argv token.
pub const MAX_ARG_BYTES: usize = 4096;

/// Maximum milliCPU units accepted on a spec.
pub const MAX_CPU_MILLIS: u32 = 256_000;

/// Maximum memory (MiB) accepted on a spec.
pub const MAX_MEMORY_MB: u32 = 65_536;

/// Maximum pids accepted on a spec.
pub const MAX_PIDS: u32 = 4_096;

/// Maximum child timeout accepted on a spec.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(3_600);

/// Maximum captured output bytes accepted on a spec.
pub const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;

/// Isolation class implied by a sandbox tier. Not independently configurable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum IsolationStrength {
    /// Process + path/network policy. Not a strong malicious-code boundary.
    ProcessPolicy,
    /// Rootless namespaces/cgroups/seccomp.
    Namespaces,
    /// gVisor-class syscall mediation.
    SyscallMediation,
    /// Firecracker-class remote microVM.
    MicroVm,
}

/// Network mode requested inside the sandbox. Default isolated mode is none.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SandboxNetwork {
    None,
    Allowlist,
    Proxy,
}

/// Mount class requested by [`SandboxMount`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MountMode {
    ReadOnly,
    ReadWrite,
    Temp,
}

/// Why [`BackendHealth`] reported unavailable. Distinct from a transport error.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum HealthReason {
    RuntimeMissing,
    PlatformUnsupported,
    FeatureMissing,
}

/// Structured child termination reported by [`SandboxBackend::exec`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SandboxExitReason {
    Exited,
    TimedOut,
    Cancelled,
    Oom,
    PolicyViolation,
}

/// Typed backend/manager failure. Display never echoes spec or secret fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SandboxError {
    Cancelled,
    TierUnavailable,
    UnsupportedNetwork,
    UnsupportedMount,
    ForbiddenMount,
    ResourceLimit,
    TimeoutInvalid,
    OutputLimitInvalid,
    TooManyMounts,
    TooManyEnvNames,
    TooManyArgs,
    TooLong,
    Empty,
    Nul,
    Control,
    InvalidEnvName,
    InvalidSpec,
    DuplicateBackend,
    UnknownHandle,
    LeaseInvalid,
    HealthFailed,
}

/// Network modes a backend can materialize. `None` is always implied.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct NetworkCapability {
    allowlist: bool,
    proxy: bool,
}

/// Mount modes a backend can materialize.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct MountCapability {
    read_only: bool,
    read_write: bool,
    temp: bool,
    max_mounts: u8,
}

/// Resource ceilings a backend will enforce. Specs above these are unsupported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceCapability {
    max_cpu_millis: u32,
    max_memory_mb: u32,
    max_pids: u32,
    max_timeout: Duration,
    max_output_bytes: u64,
}

/// Explicit backend advertisement. Isolation is derived from `tier`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SandboxCapabilities {
    tier: SandboxTier,
    isolation: IsolationStrength,
    network: NetworkCapability,
    mounts: MountCapability,
    resources: ResourceCapability,
}

/// Cooperative health probe. `available == false` is not a clean/pass result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendHealth {
    available: bool,
    reason: Option<HealthReason>,
    version: Option<String>,
}

/// Doctor row: capabilities plus any host-restricted limitation warning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendDoctorReport {
    tier: SandboxTier,
    isolation: IsolationStrength,
    health: BackendHealth,
    network: NetworkCapability,
    mounts: MountCapability,
    warning: Option<&'static str>,
}

/// One planned mount. Host source is already canonical; temp has no source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxMount {
    source: Option<CanonicalHostPath>,
    target: RepoPath,
    mode: MountMode,
}

/// Containment request. Limits are explicit; secrets are opaque handles only.
pub struct SandboxSpec {
    tier: SandboxTier,
    image: Option<String>,
    mounts: Vec<SandboxMount>,
    cwd: RepoPath,
    env_allowlist: Vec<String>,
    network: SandboxNetwork,
    cpu_millis: u32,
    memory_mb: u32,
    pids: u32,
    timeout: Duration,
    output_limit: u64,
    secrets: Vec<SecretHandle>,
}

/// Incremental spec constructor. [`SandboxSpecBuilder::build`] validates bounds.
pub struct SandboxSpecBuilder {
    tier: SandboxTier,
    image: Option<String>,
    mounts: Vec<SandboxMount>,
    cwd: Option<RepoPath>,
    env_allowlist: Vec<String>,
    network: SandboxNetwork,
    cpu_millis: u32,
    memory_mb: u32,
    pids: u32,
    timeout: Duration,
    output_limit: u64,
    secrets: Vec<SecretHandle>,
}

/// Opaque sandbox instance identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SandboxId(RuntimeId);

/// Lifecycle handle returned by [`SandboxBackend::prepare`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SandboxHandle {
    id: SandboxId,
    tier: SandboxTier,
    lease_id: LeaseId,
}

/// Argv-first exec inside a prepared sandbox. Time/output cannot exceed the spec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxExecRequest {
    argv: Vec<String>,
    timeout: Duration,
    output_limit: u64,
}

/// Structured exec completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxExecResult {
    exit: SandboxExit,
}

/// Process-tree exit plus resource usage. Reason flags are explicit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SandboxExit {
    code: Option<i32>,
    signal: Option<i32>,
    reason: SandboxExitReason,
    oom: bool,
    timeout: bool,
    policy_violation: bool,
    usage: ResourceUsage,
}

/// Counters only. Never holds output bytes or secret material.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceUsage {
    cpu_ms: u64,
    memory_peak_mb: u64,
    pids_peak: u32,
    output_bytes: u64,
}

/// Host/container/gVisor/remote backend contract.
pub trait SandboxBackend: Send + Sync {
    fn capabilities(&self) -> SandboxCapabilities;

    fn health(&self, cancel: &CancellationToken) -> Result<BackendHealth, SandboxError>;

    /// Capability check only. Health/availability is [`Self::health`].
    fn supports(&self, spec: &SandboxSpec) -> Result<(), SandboxError> {
        supports_spec(&self.capabilities(), spec)
    }

    fn prepare(
        &self,
        spec: &SandboxSpec,
        lease: &CapabilityLease,
        cancel: &CancellationToken,
    ) -> Result<SandboxHandle, SandboxError>;

    fn exec(
        &self,
        handle: &SandboxHandle,
        request: &SandboxExecRequest,
        lease: &CapabilityLease,
        cancel: &CancellationToken,
    ) -> Result<SandboxExecResult, SandboxError>;

    fn destroy(
        &self,
        handle: &SandboxHandle,
        cancel: &CancellationToken,
    ) -> Result<(), SandboxError>;
}

/// Registered backends. Selection never returns a weaker isolation than required.
#[derive(Default)]
pub struct SandboxManager {
    backends: Vec<Box<dyn SandboxBackend>>,
}

/// Isolation rank for `tier`. Unknown future variants fail closed (`None`).
pub fn isolation_rank(tier: SandboxTier) -> Option<u8> {
    match tier {
        SandboxTier::HostRestricted => Some(0),
        SandboxTier::Container => Some(1),
        SandboxTier::Gvisor => Some(2),
        SandboxTier::RemoteWorker => Some(3),
        _ => None,
    }
}

/// Whether `candidate` is at least as strong as `required`.
pub fn meets_required_isolation(candidate: SandboxTier, required: SandboxTier) -> bool {
    match (isolation_rank(candidate), isolation_rank(required)) {
        (Some(got), Some(need)) => got >= need,
        _ => false,
    }
}

/// Capability match used by [`SandboxBackend::supports`] and the manager.
pub fn supports_spec(caps: &SandboxCapabilities, spec: &SandboxSpec) -> Result<(), SandboxError> {
    if !meets_required_isolation(caps.tier, spec.tier) {
        return Err(SandboxError::TierUnavailable);
    }
    if !caps.network.supports(spec.network) {
        return Err(SandboxError::UnsupportedNetwork);
    }
    if spec.mounts.len() > usize::from(caps.mounts.max_mounts) {
        return Err(SandboxError::TooManyMounts);
    }
    for mount in &spec.mounts {
        if !caps.mounts.supports(mount.mode) {
            return Err(SandboxError::UnsupportedMount);
        }
    }
    if spec.cpu_millis > caps.resources.max_cpu_millis
        || spec.memory_mb > caps.resources.max_memory_mb
        || spec.pids > caps.resources.max_pids
        || spec.timeout > caps.resources.max_timeout
        || spec.output_limit > caps.resources.max_output_bytes
    {
        return Err(SandboxError::ResourceLimit);
    }
    Ok(())
}

impl IsolationStrength {
    pub const fn of_tier(tier: SandboxTier) -> Option<Self> {
        match tier {
            SandboxTier::HostRestricted => Some(Self::ProcessPolicy),
            SandboxTier::Container => Some(Self::Namespaces),
            SandboxTier::Gvisor => Some(Self::SyscallMediation),
            SandboxTier::RemoteWorker => Some(Self::MicroVm),
            _ => None,
        }
    }

    pub const fn is_strong_isolation(self) -> bool {
        !matches!(self, Self::ProcessPolicy)
    }

    /// Host-restricted doctor warning. Stronger tiers have no default warning.
    pub const fn doctor_warning(self) -> Option<&'static str> {
        match self {
            Self::ProcessPolicy => Some("host-restricted is not a strong malicious-code boundary"),
            Self::Namespaces | Self::SyscallMediation | Self::MicroVm => None,
        }
    }
}

impl NetworkCapability {
    pub const fn none_only() -> Self {
        Self {
            allowlist: false,
            proxy: false,
        }
    }

    pub const fn allowlist() -> Self {
        Self {
            allowlist: true,
            proxy: false,
        }
    }

    pub const fn allowlist_and_proxy() -> Self {
        Self {
            allowlist: true,
            proxy: true,
        }
    }

    pub const fn supports(self, network: SandboxNetwork) -> bool {
        match network {
            SandboxNetwork::None => true,
            SandboxNetwork::Allowlist => self.allowlist,
            SandboxNetwork::Proxy => self.proxy,
        }
    }

    pub const fn allowlist_supported(self) -> bool {
        self.allowlist
    }

    pub const fn proxy_supported(self) -> bool {
        self.proxy
    }
}

impl MountCapability {
    pub const fn workspace_temp() -> Self {
        Self {
            read_only: true,
            read_write: true,
            temp: true,
            max_mounts: MAX_MOUNTS as u8,
        }
    }

    pub const fn read_only_temp() -> Self {
        Self {
            read_only: true,
            read_write: false,
            temp: true,
            max_mounts: MAX_MOUNTS as u8,
        }
    }

    pub const fn supports(self, mode: MountMode) -> bool {
        match mode {
            MountMode::ReadOnly => self.read_only,
            MountMode::ReadWrite => self.read_write,
            MountMode::Temp => self.temp,
        }
    }

    pub const fn read_only(self) -> bool {
        self.read_only
    }

    pub const fn read_write(self) -> bool {
        self.read_write
    }

    pub const fn temp(self) -> bool {
        self.temp
    }

    pub const fn max_mounts(self) -> u8 {
        self.max_mounts
    }
}

impl ResourceCapability {
    pub const fn bounded() -> Self {
        Self {
            max_cpu_millis: MAX_CPU_MILLIS,
            max_memory_mb: MAX_MEMORY_MB,
            max_pids: MAX_PIDS,
            max_timeout: MAX_TIMEOUT,
            max_output_bytes: MAX_OUTPUT_BYTES,
        }
    }

    pub const fn new(
        max_cpu_millis: u32,
        max_memory_mb: u32,
        max_pids: u32,
        max_timeout: Duration,
        max_output_bytes: u64,
    ) -> Self {
        Self {
            max_cpu_millis,
            max_memory_mb,
            max_pids,
            max_timeout,
            max_output_bytes,
        }
    }

    pub const fn max_cpu_millis(self) -> u32 {
        self.max_cpu_millis
    }

    pub const fn max_memory_mb(self) -> u32 {
        self.max_memory_mb
    }

    pub const fn max_pids(self) -> u32 {
        self.max_pids
    }

    pub const fn max_timeout(self) -> Duration {
        self.max_timeout
    }

    pub const fn max_output_bytes(self) -> u64 {
        self.max_output_bytes
    }
}

impl SandboxCapabilities {
    pub fn new(
        tier: SandboxTier,
        network: NetworkCapability,
        mounts: MountCapability,
        resources: ResourceCapability,
    ) -> Result<Self, SandboxError> {
        let isolation = IsolationStrength::of_tier(tier).ok_or(SandboxError::TierUnavailable)?;
        Ok(Self {
            tier,
            isolation,
            network,
            mounts,
            resources,
        })
    }

    pub const fn tier(self) -> SandboxTier {
        self.tier
    }

    pub const fn isolation(self) -> IsolationStrength {
        self.isolation
    }

    pub const fn network(self) -> NetworkCapability {
        self.network
    }

    pub const fn mounts(self) -> MountCapability {
        self.mounts
    }

    pub const fn resources(self) -> ResourceCapability {
        self.resources
    }
}

impl BackendHealth {
    pub fn available(version: Option<&str>) -> Result<Self, SandboxError> {
        let version = match version {
            Some(raw) => Some(bounded_ident(raw)?),
            None => None,
        };
        Ok(Self {
            available: true,
            reason: None,
            version,
        })
    }

    pub fn unavailable(reason: HealthReason, version: Option<&str>) -> Result<Self, SandboxError> {
        let version = match version {
            Some(raw) => Some(bounded_ident(raw)?),
            None => None,
        };
        Ok(Self {
            available: false,
            reason: Some(reason),
            version,
        })
    }

    pub const fn is_available(&self) -> bool {
        self.available
    }

    pub const fn reason(&self) -> Option<HealthReason> {
        self.reason
    }

    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }
}

impl BackendDoctorReport {
    pub const fn tier(&self) -> SandboxTier {
        self.tier
    }

    pub const fn isolation(&self) -> IsolationStrength {
        self.isolation
    }

    pub const fn health(&self) -> &BackendHealth {
        &self.health
    }

    pub const fn network(&self) -> NetworkCapability {
        self.network
    }

    pub const fn mounts(&self) -> MountCapability {
        self.mounts
    }

    pub const fn warning(&self) -> Option<&'static str> {
        self.warning
    }
}

impl SandboxMount {
    pub fn bind(
        source: CanonicalHostPath,
        target: RepoPath,
        mode: MountMode,
    ) -> Result<Self, SandboxError> {
        if matches!(mode, MountMode::Temp) {
            return Err(SandboxError::InvalidSpec);
        }
        if is_docker_socket(source.as_str()) || is_docker_socket(target.as_str()) {
            return Err(SandboxError::ForbiddenMount);
        }
        Ok(Self {
            source: Some(source),
            target,
            mode,
        })
    }

    pub fn temp(target: RepoPath) -> Result<Self, SandboxError> {
        if is_docker_socket(target.as_str()) {
            return Err(SandboxError::ForbiddenMount);
        }
        Ok(Self {
            source: None,
            target,
            mode: MountMode::Temp,
        })
    }

    pub fn source(&self) -> Option<&CanonicalHostPath> {
        self.source.as_ref()
    }

    pub fn target(&self) -> &RepoPath {
        &self.target
    }

    pub const fn mode(&self) -> MountMode {
        self.mode
    }
}

impl SandboxSpec {
    pub fn builder(tier: SandboxTier) -> SandboxSpecBuilder {
        SandboxSpecBuilder {
            tier,
            image: None,
            mounts: Vec::new(),
            cwd: None,
            env_allowlist: Vec::new(),
            network: SandboxNetwork::None,
            cpu_millis: 1_000,
            memory_mb: 256,
            pids: 64,
            timeout: Duration::from_secs(30),
            output_limit: 1024 * 1024,
            secrets: Vec::new(),
        }
    }

    pub const fn tier(&self) -> SandboxTier {
        self.tier
    }

    pub fn image(&self) -> Option<&str> {
        self.image.as_deref()
    }

    pub fn mounts(&self) -> &[SandboxMount] {
        &self.mounts
    }

    pub fn cwd(&self) -> &RepoPath {
        &self.cwd
    }

    pub fn env_allowlist(&self) -> &[String] {
        &self.env_allowlist
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

    pub fn secrets(&self) -> &[SecretHandle] {
        &self.secrets
    }
}

impl SandboxSpecBuilder {
    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.image = Some(image.into());
        self
    }

    pub fn mount(mut self, mount: SandboxMount) -> Self {
        self.mounts.push(mount);
        self
    }

    pub fn cwd(mut self, cwd: RepoPath) -> Self {
        self.cwd = Some(cwd);
        self
    }

    pub fn env_allowlist(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.env_allowlist = names.into_iter().map(Into::into).collect();
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

    pub fn secret(mut self, secret: SecretHandle) -> Self {
        self.secrets.push(secret);
        self
    }

    pub fn build(self) -> Result<SandboxSpec, SandboxError> {
        if isolation_rank(self.tier).is_none() {
            return Err(SandboxError::TierUnavailable);
        }
        let cwd = self.cwd.ok_or(SandboxError::Empty)?;
        if self.mounts.len() > MAX_MOUNTS {
            return Err(SandboxError::TooManyMounts);
        }
        if self.env_allowlist.len() > MAX_ENV_NAMES {
            return Err(SandboxError::TooManyEnvNames);
        }
        for name in &self.env_allowlist {
            validate_env_name(name)?;
        }
        if let Some(image) = &self.image {
            let _ = bounded_ident(image)?;
        }
        if self.cpu_millis == 0 || self.cpu_millis > MAX_CPU_MILLIS {
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
        Ok(SandboxSpec {
            tier: self.tier,
            image: self.image,
            mounts: self.mounts,
            cwd,
            env_allowlist: self.env_allowlist,
            network: self.network,
            cpu_millis: self.cpu_millis,
            memory_mb: self.memory_mb,
            pids: self.pids,
            timeout: self.timeout,
            output_limit: self.output_limit,
            secrets: self.secrets,
        })
    }
}

impl Default for SandboxId {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxId {
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

impl SandboxHandle {
    pub fn new(tier: SandboxTier, lease_id: LeaseId) -> Result<Self, SandboxError> {
        if isolation_rank(tier).is_none() {
            return Err(SandboxError::TierUnavailable);
        }
        Ok(Self {
            id: SandboxId::new(),
            tier,
            lease_id,
        })
    }

    pub const fn id(self) -> SandboxId {
        self.id
    }

    pub const fn tier(self) -> SandboxTier {
        self.tier
    }

    pub const fn lease_id(self) -> LeaseId {
        self.lease_id
    }
}

impl SandboxExecRequest {
    pub fn new(
        argv: impl IntoIterator<Item = impl Into<String>>,
        timeout: Duration,
        output_limit: u64,
    ) -> Result<Self, SandboxError> {
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        if argv.is_empty() || argv.iter().any(|arg| arg.is_empty()) {
            return Err(SandboxError::Empty);
        }
        if argv.len() > MAX_ARGV {
            return Err(SandboxError::TooManyArgs);
        }
        for arg in &argv {
            if arg.len() > MAX_ARG_BYTES {
                return Err(SandboxError::TooLong);
            }
            if arg.contains('\0') {
                return Err(SandboxError::Nul);
            }
            if arg.chars().any(char::is_control) {
                return Err(SandboxError::Control);
            }
        }
        if timeout.is_zero() || timeout > MAX_TIMEOUT {
            return Err(SandboxError::TimeoutInvalid);
        }
        if output_limit == 0 || output_limit > MAX_OUTPUT_BYTES {
            return Err(SandboxError::OutputLimitInvalid);
        }
        Ok(Self {
            argv,
            timeout,
            output_limit,
        })
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    pub const fn output_limit(&self) -> u64 {
        self.output_limit
    }

    fn within_spec(&self, spec: &SandboxSpec) -> Result<(), SandboxError> {
        if self.timeout > spec.timeout {
            return Err(SandboxError::TimeoutInvalid);
        }
        if self.output_limit > spec.output_limit {
            return Err(SandboxError::OutputLimitInvalid);
        }
        Ok(())
    }
}

impl SandboxExecResult {
    pub const fn new(exit: SandboxExit) -> Self {
        Self { exit }
    }

    pub const fn exit(&self) -> SandboxExit {
        self.exit
    }
}

impl SandboxExit {
    pub const fn new(
        code: Option<i32>,
        signal: Option<i32>,
        reason: SandboxExitReason,
        oom: bool,
        timeout: bool,
        policy_violation: bool,
        usage: ResourceUsage,
    ) -> Self {
        Self {
            code,
            signal,
            reason,
            oom,
            timeout,
            policy_violation,
            usage,
        }
    }

    pub const fn code(self) -> Option<i32> {
        self.code
    }

    pub const fn signal(self) -> Option<i32> {
        self.signal
    }

    pub const fn reason(self) -> SandboxExitReason {
        self.reason
    }

    pub const fn oom(self) -> bool {
        self.oom
    }

    pub const fn timed_out(self) -> bool {
        self.timeout
    }

    pub const fn policy_violation(self) -> bool {
        self.policy_violation
    }

    pub const fn usage(self) -> ResourceUsage {
        self.usage
    }
}

impl ResourceUsage {
    pub const fn new(cpu_ms: u64, memory_peak_mb: u64, pids_peak: u32, output_bytes: u64) -> Self {
        Self {
            cpu_ms,
            memory_peak_mb,
            pids_peak,
            output_bytes,
        }
    }

    pub const fn cpu_ms(self) -> u64 {
        self.cpu_ms
    }

    pub const fn memory_peak_mb(self) -> u64 {
        self.memory_peak_mb
    }

    pub const fn pids_peak(self) -> u32 {
        self.pids_peak
    }

    pub const fn output_bytes(self) -> u64 {
        self.output_bytes
    }
}

impl SandboxManager {
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
        }
    }

    pub fn register(&mut self, backend: Box<dyn SandboxBackend>) -> Result<(), SandboxError> {
        let caps = backend.capabilities();
        let isolation =
            IsolationStrength::of_tier(caps.tier).ok_or(SandboxError::TierUnavailable)?;
        if caps.isolation != isolation {
            return Err(SandboxError::InvalidSpec);
        }
        if self
            .backends
            .iter()
            .any(|existing| existing.capabilities().tier == caps.tier)
        {
            return Err(SandboxError::DuplicateBackend);
        }
        self.backends.push(backend);
        self.backends
            .sort_by_key(|backend| isolation_rank(backend.capabilities().tier));
        Ok(())
    }

    pub fn doctor(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<BackendDoctorReport>, SandboxError> {
        check_cancel(cancel)?;
        let mut reports = Vec::with_capacity(self.backends.len());
        for backend in &self.backends {
            check_cancel(cancel)?;
            let caps = backend.capabilities();
            let health = backend.health(cancel)?;
            reports.push(BackendDoctorReport {
                tier: caps.tier,
                isolation: caps.isolation,
                health,
                network: caps.network,
                mounts: caps.mounts,
                warning: caps.isolation.doctor_warning(),
            });
        }
        Ok(reports)
    }

    /// Lowest isolation that is still `>= spec.tier`, healthy, and supporting.
    pub fn select(
        &self,
        spec: &SandboxSpec,
        cancel: &CancellationToken,
    ) -> Result<&dyn SandboxBackend, SandboxError> {
        check_cancel(cancel)?;
        if isolation_rank(spec.tier).is_none() {
            return Err(SandboxError::TierUnavailable);
        }

        let mut best: Option<&dyn SandboxBackend> = None;
        let mut best_rank = u8::MAX;
        let mut saw_eligible = false;
        let mut last_support_err = SandboxError::TierUnavailable;

        for backend in &self.backends {
            check_cancel(cancel)?;
            let caps = backend.capabilities();
            if !meets_required_isolation(caps.tier, spec.tier) {
                continue;
            }
            saw_eligible = true;
            match backend.health(cancel) {
                Ok(health) if health.is_available() => {}
                Ok(_) => {
                    last_support_err = SandboxError::TierUnavailable;
                    continue;
                }
                Err(SandboxError::Cancelled) => return Err(SandboxError::Cancelled),
                Err(_) => {
                    last_support_err = SandboxError::HealthFailed;
                    continue;
                }
            }
            if let Err(err) = backend.supports(spec) {
                last_support_err = err;
                continue;
            }
            let rank = isolation_rank(caps.tier).ok_or(SandboxError::TierUnavailable)?;
            if rank < best_rank {
                best_rank = rank;
                best = Some(backend.as_ref());
            }
        }

        match best {
            Some(backend) => Ok(backend),
            None if saw_eligible => Err(last_support_err.into_unavailable_if_profile()),
            None => Err(SandboxError::TierUnavailable),
        }
    }

    pub fn prepare(
        &self,
        spec: &SandboxSpec,
        lease: &CapabilityLease,
        cancel: &CancellationToken,
    ) -> Result<SandboxHandle, SandboxError> {
        check_cancel(cancel)?;
        require_proc_lease(lease)?;
        let backend = self.select(spec, cancel)?;
        backend.prepare(spec, lease, cancel)
    }

    pub fn exec(
        &self,
        spec: &SandboxSpec,
        handle: &SandboxHandle,
        request: &SandboxExecRequest,
        lease: &CapabilityLease,
        cancel: &CancellationToken,
    ) -> Result<SandboxExecResult, SandboxError> {
        check_cancel(cancel)?;
        require_proc_lease(lease)?;
        if lease.lease_id() != handle.lease_id {
            return Err(SandboxError::LeaseInvalid);
        }
        if handle.tier != spec.tier && !meets_required_isolation(handle.tier, spec.tier) {
            return Err(SandboxError::TierUnavailable);
        }
        request.within_spec(spec)?;
        let backend = self.backend_for(handle.tier)?;
        backend.exec(handle, request, lease, cancel)
    }

    pub fn destroy(
        &self,
        handle: &SandboxHandle,
        cancel: &CancellationToken,
    ) -> Result<(), SandboxError> {
        check_cancel(cancel)?;
        let backend = self.backend_for(handle.tier)?;
        backend.destroy(handle, cancel)
    }

    fn backend_for(&self, tier: SandboxTier) -> Result<&dyn SandboxBackend, SandboxError> {
        self.backends
            .iter()
            .find(|backend| backend.capabilities().tier == tier)
            .map(|backend| backend.as_ref())
            .ok_or(SandboxError::UnknownHandle)
    }
}

impl SandboxError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "sandbox operation cancelled",
            Self::TierUnavailable => "required sandbox tier is unavailable",
            Self::UnsupportedNetwork => "sandbox backend cannot provide the requested network mode",
            Self::UnsupportedMount => "sandbox backend cannot provide the requested mount mode",
            Self::ForbiddenMount => "sandbox mount is forbidden",
            Self::ResourceLimit => "sandbox resource limit is invalid or unsupported",
            Self::TimeoutInvalid => "sandbox timeout is invalid",
            Self::OutputLimitInvalid => "sandbox output limit is invalid",
            Self::TooManyMounts => "sandbox mount list exceeds bound",
            Self::TooManyEnvNames => "sandbox env allowlist exceeds bound",
            Self::TooManyArgs => "sandbox argv exceeds bound",
            Self::TooLong => "sandbox field exceeds bound",
            Self::Empty => "sandbox field is empty",
            Self::Nul => "sandbox field contains NUL",
            Self::Control => "sandbox field contains a control character",
            Self::InvalidEnvName => "sandbox env allowlist name is invalid",
            Self::InvalidSpec => "sandbox spec is invalid",
            Self::DuplicateBackend => "sandbox backend tier is already registered",
            Self::UnknownHandle => "sandbox handle is not owned by this manager",
            Self::LeaseInvalid => "sandbox lease is not valid for this operation",
            Self::HealthFailed => "sandbox backend health probe failed",
        }
    }

    pub const fn error_code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::TierUnavailable
            | Self::UnsupportedNetwork
            | Self::UnsupportedMount
            | Self::HealthFailed => Some(ErrorCode::SandboxTierUnavailable),
            Self::ForbiddenMount => Some(ErrorCode::PolicyDenied),
            Self::LeaseInvalid => Some(ErrorCode::PolicyLeaseInvalid),
            Self::TimeoutInvalid => Some(ErrorCode::ProcessTimeout),
            Self::UnknownHandle => Some(ErrorCode::InternalUnexpected),
            _ => Some(ErrorCode::ToolInvalidArguments),
        }
    }

    fn into_unavailable_if_profile(self) -> Self {
        match self {
            Self::UnsupportedNetwork
            | Self::UnsupportedMount
            | Self::ResourceLimit
            | Self::HealthFailed
            | Self::TierUnavailable => Self::TierUnavailable,
            other => other,
        }
    }
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for SandboxError {}

impl fmt::Debug for SandboxSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxSpec")
            .field("tier", &self.tier)
            .field("image", &self.image)
            .field("mounts", &self.mounts)
            .field("cwd", &self.cwd)
            .field("env_allowlist", &self.env_allowlist)
            .field("network", &self.network)
            .field("cpu_millis", &self.cpu_millis)
            .field("memory_mb", &self.memory_mb)
            .field("pids", &self.pids)
            .field("timeout", &self.timeout)
            .field("output_limit", &self.output_limit)
            .field("secrets", &self.secrets.len())
            .finish()
    }
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

fn bounded_ident(value: &str) -> Result<String, SandboxError> {
    if value.is_empty() {
        return Err(SandboxError::Empty);
    }
    if value.len() > MAX_IDENT_BYTES {
        return Err(SandboxError::TooLong);
    }
    if value.contains('\0') {
        return Err(SandboxError::Nul);
    }
    if value.chars().any(char::is_control) {
        return Err(SandboxError::Control);
    }
    Ok(value.to_owned())
}

fn validate_env_name(name: &str) -> Result<(), SandboxError> {
    if name.is_empty() {
        return Err(SandboxError::Empty);
    }
    if name.len() > MAX_IDENT_BYTES {
        return Err(SandboxError::TooLong);
    }
    if name.contains('\0') {
        return Err(SandboxError::Nul);
    }
    if name.chars().any(char::is_control) {
        return Err(SandboxError::Control);
    }
    let bytes = name.as_bytes();
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(SandboxError::InvalidEnvName);
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        return Err(SandboxError::InvalidEnvName);
    }
    Ok(())
}

fn is_docker_socket(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let trimmed = lower.trim_end_matches('/');
    trimmed == "docker.sock" || trimmed.ends_with("/docker.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    use capability_broker::{
        evaluate, issue, request_approval, ActionRequest, ApprovalChoice, ApprovalResolution,
        ApprovalScopeId, CanonicalAction, Capability, FilesystemScope, LeaseIssuer, PolicyDocument,
        PolicySource, PolicyStack, PrincipalRef, ProcessScope, ResourceDescriptor,
    };
    use protocol::SessionId;

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-7c1e9b";

    struct FakeBackend {
        caps: SandboxCapabilities,
        available: bool,
        health_error: bool,
        force_supports: Option<Result<(), SandboxError>>,
        prepared: Mutex<Vec<SandboxHandle>>,
    }

    impl FakeBackend {
        fn new(tier: SandboxTier) -> Self {
            Self {
                caps: SandboxCapabilities::new(
                    tier,
                    NetworkCapability::none_only(),
                    MountCapability::workspace_temp(),
                    ResourceCapability::bounded(),
                )
                .expect("caps"),
                available: true,
                health_error: false,
                force_supports: None,
                prepared: Mutex::new(Vec::new()),
            }
        }

        fn with_network(mut self, network: NetworkCapability) -> Self {
            self.caps.network = network;
            self
        }

        fn with_resources(mut self, resources: ResourceCapability) -> Self {
            self.caps.resources = resources;
            self
        }

        fn unavailable(mut self) -> Self {
            self.available = false;
            self
        }

        fn health_error(mut self) -> Self {
            self.health_error = true;
            self
        }

        fn always_supports(mut self) -> Self {
            self.force_supports = Some(Ok(()));
            self
        }
    }

    impl SandboxBackend for FakeBackend {
        fn capabilities(&self) -> SandboxCapabilities {
            self.caps
        }

        fn health(&self, cancel: &CancellationToken) -> Result<BackendHealth, SandboxError> {
            check_cancel(cancel)?;
            if self.health_error {
                return Err(SandboxError::HealthFailed);
            }
            if self.available {
                BackendHealth::available(Some("fake-1"))
            } else {
                BackendHealth::unavailable(HealthReason::RuntimeMissing, Some("fake-1"))
            }
        }

        fn supports(&self, spec: &SandboxSpec) -> Result<(), SandboxError> {
            if let Some(forced) = self.force_supports {
                return forced;
            }
            supports_spec(&self.caps, spec)
        }

        fn prepare(
            &self,
            spec: &SandboxSpec,
            lease: &CapabilityLease,
            cancel: &CancellationToken,
        ) -> Result<SandboxHandle, SandboxError> {
            check_cancel(cancel)?;
            self.supports(spec)?;
            let handle = SandboxHandle::new(self.caps.tier, lease.lease_id()).expect("handle");
            self.prepared.lock().expect("prepared").push(handle);
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
            if lease.lease_id() != handle.lease_id() {
                return Err(SandboxError::LeaseInvalid);
            }
            if request.timeout() > MAX_TIMEOUT {
                return Err(SandboxError::TimeoutInvalid);
            }
            Ok(SandboxExecResult::new(SandboxExit::new(
                Some(0),
                None,
                SandboxExitReason::Exited,
                false,
                false,
                false,
                ResourceUsage::new(1, 1, 1, 0),
            )))
        }

        fn destroy(
            &self,
            handle: &SandboxHandle,
            cancel: &CancellationToken,
        ) -> Result<(), SandboxError> {
            check_cancel(cancel)?;
            let mut prepared = self.prepared.lock().expect("prepared");
            prepared.retain(|item| item.id() != handle.id());
            Ok(())
        }
    }

    fn cwd() -> RepoPath {
        RepoPath::parse("src").expect("cwd")
    }

    fn spec(tier: SandboxTier) -> SandboxSpec {
        SandboxSpec::builder(tier).cwd(cwd()).build().expect("spec")
    }

    fn select_err(
        mgr: &SandboxManager,
        spec: &SandboxSpec,
        cancel: &CancellationToken,
    ) -> SandboxError {
        match mgr.select(spec, cancel) {
            Ok(_) => panic!("expected select to fail closed"),
            Err(err) => err,
        }
    }

    fn manager(backends: impl IntoIterator<Item = FakeBackend>) -> SandboxManager {
        let mut manager = SandboxManager::new();
        for backend in backends {
            manager.register(Box::new(backend)).expect("register");
        }
        manager
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

    fn issuer() -> LeaseIssuer {
        LeaseIssuer::from_key([0x42; 32]).expect("issuer")
    }

    fn issue_lease(capability: Capability, resource: ResourceDescriptor) -> CapabilityLease {
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
        let now = Instant::now();
        let decision = evaluate(&policies, &request, &CancellationToken::new()).expect("evaluate");
        let approval = request_approval(&request, &decision, now, &CancellationToken::new())
            .expect("approval");
        let approved = match approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                &request,
                now,
                &CancellationToken::new(),
            )
            .expect("resolve")
        {
            ApprovalResolution::Approved(approved) => approved,
            ApprovalResolution::Denied => panic!("expected approved"),
        };
        issue(
            &issuer(),
            &approved,
            &policies,
            now,
            &CancellationToken::new(),
        )
        .expect("issue")
    }

    fn proc_lease() -> CapabilityLease {
        issue_lease(
            Capability::ProcExec,
            ResourceDescriptor::Process(ProcessScope::new("test").expect("process")),
        )
    }

    fn fs_lease() -> CapabilityLease {
        issue_lease(
            Capability::FsRead,
            ResourceDescriptor::Filesystem(FilesystemScope::repo("src/main.rs").expect("fs")),
        )
    }

    #[test]
    fn every_known_tier_has_derived_isolation() {
        for tier in SandboxTier::ALL {
            let rank = isolation_rank(*tier).expect("rank");
            let isolation = IsolationStrength::of_tier(*tier).expect("isolation");
            assert_eq!(isolation, IsolationStrength::of_tier(*tier).unwrap());
            assert!(meets_required_isolation(*tier, *tier));
            let _ = rank;
        }
        assert!(IsolationStrength::ProcessPolicy.doctor_warning().is_some());
        assert!(IsolationStrength::Namespaces.doctor_warning().is_none());
        assert!(!IsolationStrength::ProcessPolicy.is_strong_isolation());
        assert!(IsolationStrength::SyscallMediation.is_strong_isolation());
    }

    #[test]
    fn manager_selects_lowest_tier_meeting_required_floor() {
        let mgr = manager([
            FakeBackend::new(SandboxTier::HostRestricted),
            FakeBackend::new(SandboxTier::Container),
            FakeBackend::new(SandboxTier::Gvisor),
        ]);
        let selected = mgr
            .select(&spec(SandboxTier::Container), &CancellationToken::new())
            .expect("select");
        assert_eq!(selected.capabilities().tier, SandboxTier::Container);
        assert_eq!(
            selected.capabilities().isolation(),
            IsolationStrength::Namespaces
        );
    }

    #[test]
    fn required_gvisor_does_not_downgrade_to_host_or_container() {
        let mgr = manager([
            FakeBackend::new(SandboxTier::HostRestricted).always_supports(),
            FakeBackend::new(SandboxTier::Container).always_supports(),
        ]);
        let err = select_err(&mgr, &spec(SandboxTier::Gvisor), &CancellationToken::new());
        assert_eq!(err, SandboxError::TierUnavailable);
        assert_eq!(err.error_code(), Some(ErrorCode::SandboxTierUnavailable));
        assert!(!err.as_str().contains(CANARY));
    }

    #[test]
    fn unavailable_required_tier_does_not_use_weaker_healthy_backend() {
        let mgr = manager([
            FakeBackend::new(SandboxTier::Container),
            FakeBackend::new(SandboxTier::Gvisor).unavailable(),
        ]);
        let err = select_err(&mgr, &spec(SandboxTier::Gvisor), &CancellationToken::new());
        assert_eq!(err, SandboxError::TierUnavailable);
        assert_eq!(err.error_code(), Some(ErrorCode::SandboxTierUnavailable));
    }

    #[test]
    fn stronger_available_tier_may_satisfy_weaker_requirement() {
        let mgr = manager([
            FakeBackend::new(SandboxTier::Container).unavailable(),
            FakeBackend::new(SandboxTier::Gvisor),
        ]);
        let selected = mgr
            .select(&spec(SandboxTier::Container), &CancellationToken::new())
            .expect("upgrade");
        assert_eq!(selected.capabilities().tier, SandboxTier::Gvisor);
    }

    #[test]
    fn host_backend_cannot_bypass_isolation_via_supports() {
        let mgr = manager([FakeBackend::new(SandboxTier::HostRestricted).always_supports()]);
        let err = select_err(
            &mgr,
            &spec(SandboxTier::Container),
            &CancellationToken::new(),
        );
        assert_eq!(err, SandboxError::TierUnavailable);
        assert_eq!(err.error_code(), Some(ErrorCode::SandboxTierUnavailable));
    }

    #[test]
    fn network_and_mount_capabilities_are_advertised_and_enforced() {
        let host = FakeBackend::new(SandboxTier::HostRestricted);
        let caps = host.capabilities();
        assert!(!caps.network().allowlist_supported());
        assert!(!caps.network().proxy_supported());
        assert!(caps.mounts().read_only());
        assert!(caps.mounts().read_write());
        assert!(caps.mounts().temp());

        let mgr = manager([host.with_network(NetworkCapability::none_only())]);
        let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .network(SandboxNetwork::Allowlist)
            .build()
            .expect("spec");
        let err = select_err(&mgr, &spec, &CancellationToken::new());
        assert_eq!(err, SandboxError::TierUnavailable);
    }

    #[test]
    fn docker_socket_mount_is_rejected() {
        let source = CanonicalHostPath::from_resolved("/var/run/docker.sock").expect("path");
        let err = SandboxMount::bind(
            source,
            RepoPath::parse("docker.sock").expect("target"),
            MountMode::ReadWrite,
        )
        .expect_err("socket");
        assert_eq!(err, SandboxError::ForbiddenMount);
        assert_eq!(err.error_code(), Some(ErrorCode::PolicyDenied));
        assert!(!err.as_str().contains("docker.sock"));
    }

    #[test]
    fn resource_limits_and_timeouts_are_explicit_and_bounded() {
        assert_eq!(
            SandboxSpec::builder(SandboxTier::Container)
                .cwd(cwd())
                .timeout(Duration::ZERO)
                .build()
                .expect_err("zero timeout"),
            SandboxError::TimeoutInvalid
        );
        assert_eq!(
            SandboxSpec::builder(SandboxTier::Container)
                .cwd(cwd())
                .memory_mb(0)
                .build()
                .expect_err("zero memory"),
            SandboxError::ResourceLimit
        );
        assert_eq!(
            SandboxSpec::builder(SandboxTier::Container)
                .cwd(cwd())
                .output_limit(0)
                .build()
                .expect_err("zero output"),
            SandboxError::OutputLimitInvalid
        );

        let tight = ResourceCapability::new(500, 64, 8, Duration::from_secs(5), 4096);
        let mgr = manager([FakeBackend::new(SandboxTier::Container).with_resources(tight)]);
        let heavy = SandboxSpec::builder(SandboxTier::Container)
            .cwd(cwd())
            .memory_mb(256)
            .build()
            .expect("heavy");
        let err = select_err(&mgr, &heavy, &CancellationToken::new());
        assert_eq!(err, SandboxError::TierUnavailable);
    }

    #[test]
    fn cancellation_is_checked_on_select_prepare_exec_and_destroy() {
        let mgr = manager([FakeBackend::new(SandboxTier::Container)]);
        let spec = spec(SandboxTier::Container);
        let lease = proc_lease();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(select_err(&mgr, &spec, &cancel), SandboxError::Cancelled);
        assert_eq!(
            mgr.prepare(&spec, &lease, &cancel).expect_err("prepare"),
            SandboxError::Cancelled
        );
        let handle = SandboxHandle::new(SandboxTier::Container, lease.lease_id()).expect("handle");
        let request =
            SandboxExecRequest::new(["/bin/true"], Duration::from_secs(1), 1024).expect("request");
        assert_eq!(
            mgr.exec(&spec, &handle, &request, &lease, &cancel)
                .expect_err("exec"),
            SandboxError::Cancelled
        );
        assert_eq!(
            mgr.destroy(&handle, &cancel).expect_err("destroy"),
            SandboxError::Cancelled
        );
        assert_eq!(SandboxError::Cancelled.error_code(), None);
    }

    #[test]
    fn prepare_and_exec_require_proc_lease_and_matching_handle() {
        let mgr = manager([FakeBackend::new(SandboxTier::Container)]);
        let spec = spec(SandboxTier::Container);
        let live = CancellationToken::new();
        let fs = fs_lease();
        assert_eq!(
            mgr.prepare(&spec, &fs, &live).expect_err("fs lease"),
            SandboxError::LeaseInvalid
        );
        assert_eq!(
            SandboxError::LeaseInvalid.error_code(),
            Some(ErrorCode::PolicyLeaseInvalid)
        );

        let lease = proc_lease();
        let handle = mgr.prepare(&spec, &lease, &live).expect("prepare");
        assert_eq!(handle.tier(), SandboxTier::Container);
        let request = SandboxExecRequest::new(["/bin/true"], Duration::from_millis(50), 512)
            .expect("request");
        let other = proc_lease();
        assert_eq!(
            mgr.exec(&spec, &handle, &request, &other, &live)
                .expect_err("retarget"),
            SandboxError::LeaseInvalid
        );
        let result = mgr
            .exec(&spec, &handle, &request, &lease, &live)
            .expect("exec");
        assert_eq!(result.exit().reason(), SandboxExitReason::Exited);
        mgr.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn exec_cannot_widen_timeout_or_output_limit() {
        let mgr = manager([FakeBackend::new(SandboxTier::Container)]);
        let spec = SandboxSpec::builder(SandboxTier::Container)
            .cwd(cwd())
            .timeout(Duration::from_secs(2))
            .output_limit(1024)
            .build()
            .expect("spec");
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = mgr.prepare(&spec, &lease, &live).expect("prepare");
        let wide =
            SandboxExecRequest::new(["/bin/true"], Duration::from_secs(5), 1024).expect("wide");
        assert_eq!(
            mgr.exec(&spec, &handle, &wide, &lease, &live)
                .expect_err("widen timeout"),
            SandboxError::TimeoutInvalid
        );
        let wide_out =
            SandboxExecRequest::new(["/bin/true"], Duration::from_secs(1), 4096).expect("wide out");
        assert_eq!(
            mgr.exec(&spec, &handle, &wide_out, &lease, &live)
                .expect_err("widen output"),
            SandboxError::OutputLimitInvalid
        );
    }

    #[test]
    fn secret_handles_are_not_echoed_in_debug_or_errors() {
        let secret = SecretHandle::parse(CANARY).expect("handle");
        let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .secret(secret)
            .build()
            .expect("spec");
        let debug = format!("{spec:?}");
        assert!(!debug.contains(CANARY));
        assert!(debug.contains("secrets: 1"));
        assert!(!SandboxError::TierUnavailable.as_str().contains(CANARY));
        assert!(!SandboxError::LeaseInvalid.as_str().contains(CANARY));
    }

    #[test]
    fn doctor_warns_that_host_restricted_is_not_strong_isolation() {
        let mgr = manager([
            FakeBackend::new(SandboxTier::HostRestricted),
            FakeBackend::new(SandboxTier::Container),
        ]);
        let reports = mgr.doctor(&CancellationToken::new()).expect("doctor");
        let host = reports
            .iter()
            .find(|row| row.tier() == SandboxTier::HostRestricted)
            .expect("host");
        assert_eq!(host.isolation(), IsolationStrength::ProcessPolicy);
        assert!(host.warning().unwrap().contains("not a strong"));
        assert!(host.health().is_available());
        let container = reports
            .iter()
            .find(|row| row.tier() == SandboxTier::Container)
            .expect("container");
        assert!(container.warning().is_none());
        assert!(container.isolation().is_strong_isolation());
    }

    #[test]
    fn health_error_on_required_tier_is_not_a_clean_pass() {
        let mgr = manager([FakeBackend::new(SandboxTier::Gvisor).health_error()]);
        let err = select_err(&mgr, &spec(SandboxTier::Gvisor), &CancellationToken::new());
        assert_eq!(err, SandboxError::TierUnavailable);
        assert_eq!(err.error_code(), Some(ErrorCode::SandboxTierUnavailable));
    }

    #[test]
    fn duplicate_tier_registration_is_rejected() {
        let mut mgr = SandboxManager::new();
        mgr.register(Box::new(FakeBackend::new(SandboxTier::Container)))
            .expect("first");
        let err = mgr
            .register(Box::new(FakeBackend::new(SandboxTier::Container)))
            .expect_err("dup");
        assert_eq!(err, SandboxError::DuplicateBackend);
    }
}
