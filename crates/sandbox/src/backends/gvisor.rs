//! gVisor/runsc sandbox backend.
//!
//! `supports` verifies the runsc binary, version, required features, and that
//! a private least-mount rootfs can be applied. A required
//! [`SandboxTier::Gvisor`] job never silently uses the container backend
//! (T-009). Mount sources are canonicalized before use (T-003). Host-rootfs
//! `runsc do` is refused.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use capability_broker::{CancellationToken, CanonicalHostPath, Capability, CapabilityLease};
use process_signal::GroupSignal;
use protocol::{LeaseId, RepoPath, RuntimeId, SandboxTier};

use crate::backend::{
    BackendHealth, HealthReason, IsolationStrength, MountCapability, MountMode, NetworkCapability,
    ResourceCapability, ResourceUsage, SandboxBackend, SandboxCapabilities, SandboxError,
    SandboxExecRequest, SandboxExecResult, SandboxExit, SandboxExitReason, SandboxHandle,
    SandboxId, SandboxMount, SandboxNetwork, SandboxSpec, supports_spec,
};

/// Maximum prepared gVisor sandboxes retained by one backend.
pub const MAX_LIVE_GVISOR_SANDBOXES: usize = 64;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const CANCEL_STRIDE: u32 = 8;
const TERM_GRACE: Duration = Duration::from_millis(80);
const KILL_WAIT: Duration = Duration::from_secs(2);
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const SAMPLE_MISS_LIMIT: u32 = 5;

/// Fixed runsc paths. Never Docker, never `$PATH`.
const RUNSC_PROGRAMS: &[&str] = &["/usr/bin/runsc", "/usr/local/bin/runsc", "/bin/runsc"];
const TRUE_PROGRAMS: &[&str] = &["/usr/bin/true", "/bin/true"];
const TEST_PROGRAMS: &[&str] = &["/usr/bin/test", "/bin/test"];
const LS_PROGRAMS: &[&str] = &["/usr/bin/ls", "/bin/ls"];
const PS_PROGRAMS: &[&str] = &["/bin/ps", "/usr/bin/ps"];
const PGREP_PROGRAMS: &[&str] = &["/usr/bin/pgrep", "/bin/pgrep"];

const PLATFORMS: &[&str] = &["systrap", "ptrace"];

const PROXY_ENV_NAMES: &[&str] = &[
    "ALL_PROXY",
    "FTP_PROXY",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "SOCKS5_PROXY",
    "SOCKS_PROXY",
];

const SENSITIVE_PREFIXES: &[&str] = &[
    "/etc",
    "/dev",
    "/proc",
    "/sys",
    "/root",
    "/private/etc",
    "/private/dev",
    "/private/var/root",
    "/private/var/run/docker",
    "/var/run/docker",
    "/run/docker",
];

const DOCKER_SOCKET_NAMES: &[&str] = &[
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/private/var/run/docker.sock",
];

const HOST_SENSITIVE_PROBES: &[&str] = &[
    "/etc/passwd",
    "/etc",
    "/var/run/docker.sock",
    "/run/docker.sock",
];

/// Host rootfs pieces that may be read-only bind-mounted. Not `/etc`, `/home`.
const SYSTEM_RO_BINDS: &[&str] = &["/usr", "/bin", "/lib", "/lib64", "/sbin"];

/// Runtime that materializes [`SandboxTier::Gvisor`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GvisorRuntime {
    Runsc,
}

/// Network mode recorded on a prepared plan. Host network is never selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GvisorNetwork {
    Isolated,
    Allowlist,
    Proxy,
}

/// Detected runsc binary, version, and required features. Not a clean/pass
/// result unless [`Self::is_ready`] is true.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GvisorSupport {
    ready: bool,
    reason: Option<HealthReason>,
    program: Option<String>,
    version: Option<String>,
    features: GvisorFeatures,
}

/// Feature flags required before gVisor may be selected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GvisorFeatures {
    rootless: bool,
    network_none: bool,
    platform: Option<String>,
    least_mounts: bool,
}

/// Planned containment for one handle. Secrets stay as a count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GvisorPlan {
    isolation: IsolationStrength,
    network: GvisorNetwork,
    runtime: GvisorRuntime,
    program: String,
    args: Vec<String>,
    guest_cwd: String,
    host_cwd: CanonicalHostPath,
    binds: Vec<PlannedBind>,
    timeout: Duration,
    output_limit: u64,
    cpu_millis: u32,
    memory_mb: u32,
    pids: u32,
    secret_count: usize,
    rootless: bool,
    network_none: bool,
    platform: String,
    least_mounts: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlannedBind {
    source: Option<String>,
    guest: String,
    mode: MountMode,
}

/// gVisor/runsc backend. Isolation is syscall mediation.
pub struct GvisorBackend {
    caps: SandboxCapabilities,
    sessions: Mutex<HashMap<SandboxId, PreparedSession>>,
}

struct PreparedSession {
    handle: SandboxHandle,
    lease_id: LeaseId,
    plan: GvisorPlan,
}

struct ExecBundle {
    root: PathBuf,
    bundle: PathBuf,
    state: PathBuf,
    cid: String,
}

impl Drop for ExecBundle {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl GvisorRuntime {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Runsc => "runsc",
        }
    }
}

impl GvisorNetwork {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Isolated => "none",
            Self::Allowlist => "allowlist",
            Self::Proxy => "proxy",
        }
    }

    pub const fn from_network(network: SandboxNetwork) -> Self {
        match network {
            // This backend never declares `NetworkCapability::open_supported`
            // (see `NetworkCapability::allowlist_and_proxy` above), and
            // `prepare`'s own `self.supports(spec)?` already refuses an
            // `Open` request before this is ever reached — mapped to the
            // most restrictive state as a defensive fallback, not a real
            // code path.
            SandboxNetwork::None | SandboxNetwork::Open => Self::Isolated,
            SandboxNetwork::Allowlist => Self::Allowlist,
            SandboxNetwork::Proxy => Self::Proxy,
        }
    }

    pub const fn uses_host_network(self) -> bool {
        false
    }
}

impl GvisorFeatures {
    pub const fn empty() -> Self {
        Self {
            rootless: false,
            network_none: false,
            platform: None,
            least_mounts: false,
        }
    }

    pub const fn rootless(&self) -> bool {
        self.rootless
    }

    pub const fn network_none(&self) -> bool {
        self.network_none
    }

    pub fn platform(&self) -> Option<&str> {
        self.platform.as_deref()
    }

    pub const fn least_mounts(&self) -> bool {
        self.least_mounts
    }

    pub fn is_complete(&self) -> bool {
        self.rootless && self.network_none && self.platform.is_some() && self.least_mounts
    }
}

impl GvisorSupport {
    pub const fn is_ready(&self) -> bool {
        self.ready
    }

    pub const fn reason(&self) -> Option<HealthReason> {
        self.reason
    }

    pub fn program(&self) -> Option<&str> {
        self.program.as_deref()
    }

    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    pub const fn features(&self) -> &GvisorFeatures {
        &self.features
    }
}

impl GvisorPlan {
    /// Construct runsc argv/config from a typed spec. Does not require a live runtime.
    pub fn from_spec(spec: &SandboxSpec) -> Result<Self, SandboxError> {
        if spec.tier() != SandboxTier::Gvisor {
            return Err(SandboxError::TierUnavailable);
        }
        if spec.image().is_some() {
            return Err(SandboxError::InvalidSpec);
        }
        for name in spec.env_allowlist() {
            if is_proxy_env_name(name) {
                return Err(SandboxError::UnsupportedNetwork);
            }
        }
        for mount in spec.mounts() {
            validate_mount(mount)?;
        }
        let host_cwd = resolve_cwd(spec.cwd(), spec.mounts())?;
        let guest_cwd = guest_abs(spec.cwd())?;
        let network = GvisorNetwork::from_network(spec.network());
        let program = intended_runsc_program();
        let platform = "systrap".to_owned();
        let mut binds = Vec::new();
        let args = vec![
            "--rootless".to_owned(),
            "--network=none".to_owned(),
            "--ignore-cgroups".to_owned(),
            "--platform".to_owned(),
            platform.clone(),
        ];

        for host in SYSTEM_RO_BINDS {
            if !Path::new(host).exists() {
                continue;
            }
            if is_forbidden_host_source(host) || is_docker_socket(host) {
                continue;
            }
            binds.push(PlannedBind {
                source: Some((*host).to_owned()),
                guest: (*host).to_owned(),
                mode: MountMode::ReadOnly,
            });
        }

        for mount in spec.mounts() {
            let guest = guest_abs(mount.target())?;
            if is_forbidden_guest(&guest) {
                return Err(SandboxError::ForbiddenMount);
            }
            match mount.mode() {
                MountMode::Temp => binds.push(PlannedBind {
                    source: None,
                    guest,
                    mode: MountMode::Temp,
                }),
                MountMode::ReadOnly | MountMode::ReadWrite => {
                    let source = mount.source().ok_or(SandboxError::InvalidSpec)?;
                    if is_forbidden_host_source(source.as_str())
                        || is_docker_socket(source.as_str())
                    {
                        return Err(SandboxError::ForbiddenMount);
                    }
                    binds.push(PlannedBind {
                        source: Some(source.as_str().to_owned()),
                        guest,
                        mode: mount.mode(),
                    });
                }
            }
        }

        if args.iter().any(|part| is_docker_socket(part)) {
            return Err(SandboxError::ForbiddenMount);
        }
        if args.iter().any(|part| is_host_network_flag(part)) {
            return Err(SandboxError::UnsupportedNetwork);
        }
        if binds.iter().any(|bind| bind.source.as_deref() == Some("/")) {
            return Err(SandboxError::ForbiddenMount);
        }

        Ok(Self {
            isolation: IsolationStrength::SyscallMediation,
            network,
            runtime: GvisorRuntime::Runsc,
            program,
            args,
            guest_cwd,
            host_cwd,
            binds,
            timeout: spec.timeout(),
            output_limit: spec.output_limit(),
            cpu_millis: spec.cpu_millis(),
            memory_mb: spec.memory_mb(),
            pids: spec.pids(),
            secret_count: spec.secrets().len(),
            rootless: true,
            network_none: true,
            platform,
            least_mounts: true,
        })
    }

    pub const fn isolation(&self) -> IsolationStrength {
        self.isolation
    }

    pub const fn network(&self) -> GvisorNetwork {
        self.network
    }

    pub const fn runtime(&self) -> GvisorRuntime {
        self.runtime
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(self.program.clone());
        argv.extend(self.args.iter().cloned());
        argv
    }

    pub fn guest_cwd(&self) -> &str {
        &self.guest_cwd
    }

    pub fn host_cwd(&self) -> &CanonicalHostPath {
        &self.host_cwd
    }

    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    pub const fn output_limit(&self) -> u64 {
        self.output_limit
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

    pub const fn secret_count(&self) -> usize {
        self.secret_count
    }

    pub const fn is_rootless(&self) -> bool {
        self.rootless
    }

    pub const fn has_network_none(&self) -> bool {
        self.network_none
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    pub const fn has_least_mounts(&self) -> bool {
        self.least_mounts
    }

    pub const fn is_strong_isolation(&self) -> bool {
        self.isolation.is_strong_isolation()
    }

    pub fn uses_docker_socket(&self) -> bool {
        self.argv().iter().any(|part| is_docker_socket(part))
            || self.program_is_docker()
            || self
                .binds
                .iter()
                .any(|bind| bind.source.as_deref().is_some_and(is_docker_socket))
    }

    pub fn bind_sources(&self) -> impl Iterator<Item = &str> {
        self.binds.iter().filter_map(|bind| bind.source.as_deref())
    }

    pub fn shares_host_network(&self) -> bool {
        !self.network_none
            || self.network.uses_host_network()
            || self.args.iter().any(|part| is_host_network_flag(part))
    }

    /// Host-rootfs `runsc do` is never a planned execution path (T-009).
    pub fn uses_host_rootfs(&self) -> bool {
        !self.least_mounts
            || self.args.iter().any(|part| part == "do")
            || self
                .binds
                .iter()
                .any(|bind| bind.source.as_deref() == Some("/"))
    }

    fn program_is_docker(&self) -> bool {
        let name = Path::new(&self.program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&self.program);
        name.eq_ignore_ascii_case("docker") || name.eq_ignore_ascii_case("dockerd")
    }
}

impl GvisorBackend {
    pub fn new() -> Self {
        let caps = SandboxCapabilities::new(
            SandboxTier::Gvisor,
            NetworkCapability::allowlist_and_proxy(),
            MountCapability::workspace_temp(),
            ResourceCapability::bounded(),
        )
        .expect("gvisor is a known sandbox tier");
        Self {
            caps,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Verifies runsc, its version, and required features. Unavailable is not
    /// a clean/pass and never selects a weaker backend.
    pub fn supports(&self, cancel: &CancellationToken) -> Result<GvisorSupport, SandboxError> {
        detect_support(cancel)
    }

    /// Prepared plan for `handle`, if this backend owns it.
    pub fn plan(&self, handle: &SandboxHandle) -> Result<GvisorPlan, SandboxError> {
        let sessions = self.lock_sessions()?;
        sessions
            .get(&handle.id())
            .filter(|session| session_matches(session, handle))
            .map(|session| session.plan.clone())
            .ok_or(SandboxError::UnknownHandle)
    }

    fn lock_sessions(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<SandboxId, PreparedSession>>, SandboxError> {
        self.sessions.lock().map_err(|_| SandboxError::HealthFailed)
    }
}

impl Default for GvisorBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxBackend for GvisorBackend {
    fn capabilities(&self) -> SandboxCapabilities {
        self.caps
    }

    fn health(&self, cancel: &CancellationToken) -> Result<BackendHealth, SandboxError> {
        let support = detect_support(cancel)?;
        if support.is_ready() {
            BackendHealth::available(support.version().or(Some("runsc")))
        } else {
            BackendHealth::unavailable(
                support.reason().unwrap_or(HealthReason::RuntimeMissing),
                support.version().or(Some("runsc")),
            )
        }
    }

    fn supports(&self, spec: &SandboxSpec) -> Result<(), SandboxError> {
        if spec.tier() != SandboxTier::Gvisor {
            return Err(SandboxError::TierUnavailable);
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
        require_proc_lease(lease)?;
        if spec.tier() != SandboxTier::Gvisor {
            return Err(SandboxError::TierUnavailable);
        }
        SandboxBackend::supports(self, spec)?;
        let mut plan = GvisorPlan::from_spec(spec)?;
        if plan.uses_docker_socket() {
            return Err(SandboxError::ForbiddenMount);
        }
        if plan.shares_host_network() {
            return Err(SandboxError::UnsupportedNetwork);
        }
        if plan.uses_host_rootfs() || !plan.has_least_mounts() {
            return Err(SandboxError::TierUnavailable);
        }
        check_cancel(cancel)?;
        let support = detect_support(cancel)?;
        if !support.is_ready() || !support.features().least_mounts() {
            return Err(SandboxError::TierUnavailable);
        }
        if let Some(program) = support.program() {
            plan.program = program.to_owned();
        }
        if let Some(platform) = support.features().platform() {
            apply_platform(&mut plan, platform);
        }
        let handle = SandboxHandle::new(SandboxTier::Gvisor, lease.lease_id())?;
        let mut sessions = self.lock_sessions()?;
        if sessions.len() >= MAX_LIVE_GVISOR_SANDBOXES {
            return Err(SandboxError::ResourceLimit);
        }
        sessions.insert(
            handle.id(),
            PreparedSession {
                handle,
                lease_id: lease.lease_id(),
                plan,
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
        if handle.tier() != SandboxTier::Gvisor {
            return Err(SandboxError::TierUnavailable);
        }
        let plan = {
            let sessions = self.lock_sessions()?;
            let session = sessions
                .get(&handle.id())
                .filter(|session| session_matches(session, handle))
                .ok_or(SandboxError::UnknownHandle)?;
            if session.lease_id != lease.lease_id() {
                return Err(SandboxError::LeaseInvalid);
            }
            session.plan.clone()
        };
        if request.timeout() > plan.timeout {
            return Err(SandboxError::TimeoutInvalid);
        }
        if request.output_limit() > plan.output_limit {
            return Err(SandboxError::OutputLimitInvalid);
        }
        if plan.uses_host_rootfs() || !plan.has_least_mounts() {
            return Err(SandboxError::TierUnavailable);
        }
        run_gvisor(&plan, request, handle.id(), cancel)
    }

    fn destroy(
        &self,
        handle: &SandboxHandle,
        cancel: &CancellationToken,
    ) -> Result<(), SandboxError> {
        check_cancel(cancel)?;
        if handle.tier() != SandboxTier::Gvisor {
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

fn detect_support(cancel: &CancellationToken) -> Result<GvisorSupport, SandboxError> {
    check_cancel(cancel)?;
    if !cfg!(target_os = "linux") {
        return Ok(GvisorSupport {
            ready: false,
            reason: Some(HealthReason::PlatformUnsupported),
            program: None,
            version: Some("runsc".to_owned()),
            features: GvisorFeatures::empty(),
        });
    }
    let Some(program) = first_existing(RUNSC_PROGRAMS) else {
        return Ok(GvisorSupport {
            ready: false,
            reason: Some(HealthReason::RuntimeMissing),
            program: None,
            version: Some("runsc".to_owned()),
            features: GvisorFeatures::empty(),
        });
    };
    check_cancel(cancel)?;
    let Some(version) = runsc_version(program, cancel)? else {
        return Ok(GvisorSupport {
            ready: false,
            reason: Some(HealthReason::FeatureMissing),
            program: Some(program.to_owned()),
            version: Some("runsc".to_owned()),
            features: parse_runsc_features(program, cancel)?,
        });
    };
    let advertised = parse_runsc_features(program, cancel)?;
    check_cancel(cancel)?;
    let Some(platform) = probe_least_mount_platform(program, cancel)? else {
        return Ok(GvisorSupport {
            ready: false,
            reason: Some(HealthReason::FeatureMissing),
            program: Some(program.to_owned()),
            version: Some(version),
            features: advertised,
        });
    };
    let features = GvisorFeatures {
        rootless: true,
        network_none: true,
        platform: Some(platform),
        least_mounts: true,
    };
    if !features.is_complete() {
        return Ok(GvisorSupport {
            ready: false,
            reason: Some(HealthReason::FeatureMissing),
            program: Some(program.to_owned()),
            version: Some(version),
            features,
        });
    }
    Ok(GvisorSupport {
        ready: true,
        reason: None,
        program: Some(program.to_owned()),
        version: Some(version),
        features,
    })
}

fn apply_platform(plan: &mut GvisorPlan, platform: &str) {
    plan.platform = platform.to_owned();
    let mut i = 0;
    while i + 1 < plan.args.len() {
        if plan.args[i] == "--platform" {
            plan.args[i + 1] = platform.to_owned();
            return;
        }
        i += 1;
    }
    plan.args.push("--platform".to_owned());
    plan.args.push(platform.to_owned());
}

fn intended_runsc_program() -> String {
    first_existing(RUNSC_PROGRAMS)
        .unwrap_or(RUNSC_PROGRAMS[0])
        .to_owned()
}

fn guest_abs(path: &RepoPath) -> Result<String, SandboxError> {
    let mut guest = String::from("/");
    guest.push_str(path.as_str());
    if guest.contains('\0') {
        return Err(SandboxError::Nul);
    }
    if is_docker_socket(&guest) || is_forbidden_guest(&guest) {
        return Err(SandboxError::ForbiddenMount);
    }
    Ok(guest)
}

fn validate_mount(mount: &SandboxMount) -> Result<(), SandboxError> {
    if is_docker_socket(mount.target().as_str()) || is_forbidden_guest_target(mount.target()) {
        return Err(SandboxError::ForbiddenMount);
    }
    match mount.mode() {
        MountMode::Temp => {
            if mount.source().is_some() {
                return Err(SandboxError::InvalidSpec);
            }
            Ok(())
        }
        MountMode::ReadOnly | MountMode::ReadWrite => {
            let source = mount.source().ok_or(SandboxError::InvalidSpec)?;
            if is_forbidden_host_source(source.as_str()) || is_docker_socket(source.as_str()) {
                return Err(SandboxError::ForbiddenMount);
            }
            let resolved = resolve_existing_dir(Path::new(source.as_str()))?;
            if is_forbidden_host_source(resolved.as_str()) || is_docker_socket(resolved.as_str()) {
                return Err(SandboxError::ForbiddenMount);
            }
            Ok(())
        }
    }
}

fn resolve_cwd(cwd: &RepoPath, mounts: &[SandboxMount]) -> Result<CanonicalHostPath, SandboxError> {
    let mut best: Option<(&SandboxMount, usize)> = None;
    for mount in mounts {
        if !matches!(mount.mode(), MountMode::ReadOnly | MountMode::ReadWrite) {
            continue;
        }
        if let Some(prefix_len) = target_covers(mount.target(), cwd)
            && best.is_none_or(|(_, len)| prefix_len > len)
        {
            best = Some((mount, prefix_len));
        }
    }
    let (mount, _) = best.ok_or(SandboxError::ForbiddenMount)?;
    let source = mount.source().ok_or(SandboxError::InvalidSpec)?;
    if is_forbidden_host_source(source.as_str()) || is_docker_socket(source.as_str()) {
        return Err(SandboxError::ForbiddenMount);
    }
    let resolved_source = resolve_existing_dir(Path::new(source.as_str()))?;
    if is_forbidden_host_source(resolved_source.as_str())
        || is_docker_socket(resolved_source.as_str())
    {
        return Err(SandboxError::ForbiddenMount);
    }
    let joined = join_host(&resolved_source, mount.target(), cwd)?;
    let resolved_cwd = resolve_existing_dir(Path::new(joined.as_str()))?;
    if is_forbidden_host_source(resolved_cwd.as_str()) || is_docker_socket(resolved_cwd.as_str()) {
        return Err(SandboxError::ForbiddenMount);
    }
    if !path_is_within(resolved_source.as_str(), resolved_cwd.as_str()) {
        return Err(SandboxError::ForbiddenMount);
    }
    Ok(resolved_cwd)
}

fn target_covers(mount_target: &RepoPath, cwd: &RepoPath) -> Option<usize> {
    let target = mount_target.as_str();
    let cwd = cwd.as_str();
    if cwd == target {
        return Some(target.len());
    }
    if cwd.starts_with(target) && cwd.as_bytes().get(target.len()) == Some(&b'/') {
        return Some(target.len());
    }
    None
}

fn join_host(
    source: &CanonicalHostPath,
    mount_target: &RepoPath,
    cwd: &RepoPath,
) -> Result<CanonicalHostPath, SandboxError> {
    if cwd.as_str() == mount_target.as_str() {
        return Ok(source.clone());
    }
    let prefix = mount_target.as_str();
    let remainder = cwd
        .as_str()
        .get(prefix.len() + 1..)
        .ok_or(SandboxError::ForbiddenMount)?;
    if remainder.is_empty() {
        return Ok(source.clone());
    }
    let mut joined = source.as_str().trim_end_matches('/').to_owned();
    joined.push('/');
    joined.push_str(remainder);
    CanonicalHostPath::from_resolved(&joined).map_err(|_| SandboxError::ForbiddenMount)
}

fn path_is_within(root: &str, candidate: &str) -> bool {
    let root = root.trim_end_matches('/');
    let candidate = candidate.trim_end_matches('/');
    candidate == root
        || candidate.starts_with(root) && candidate.as_bytes().get(root.len()) == Some(&b'/')
}

fn resolve_existing_dir(path: &Path) -> Result<CanonicalHostPath, SandboxError> {
    let requested = path.to_str().ok_or(SandboxError::ForbiddenMount)?;
    if is_forbidden_host_source(requested) || is_docker_socket(requested) {
        return Err(SandboxError::ForbiddenMount);
    }
    let canon = fs::canonicalize(path).map_err(|_| SandboxError::ForbiddenMount)?;
    let meta = fs::metadata(&canon).map_err(|_| SandboxError::ForbiddenMount)?;
    if !meta.is_dir() {
        return Err(SandboxError::ForbiddenMount);
    }
    let text = canon.to_str().ok_or(SandboxError::ForbiddenMount)?;
    if is_forbidden_host_source(text) || is_docker_socket(text) {
        return Err(SandboxError::ForbiddenMount);
    }
    CanonicalHostPath::from_resolved(text).map_err(|_| SandboxError::ForbiddenMount)
}

fn resolve_existing_file(path: &str) -> Result<CanonicalHostPath, SandboxError> {
    let requested =
        CanonicalHostPath::from_resolved(path).map_err(|_| SandboxError::ForbiddenMount)?;
    if is_forbidden_host_source(requested.as_str()) || is_docker_socket(requested.as_str()) {
        return Err(SandboxError::ForbiddenMount);
    }
    let canon = fs::canonicalize(requested.as_str()).map_err(|_| SandboxError::ForbiddenMount)?;
    if !canon.is_file() {
        return Err(SandboxError::ForbiddenMount);
    }
    let text = canon.to_str().ok_or(SandboxError::ForbiddenMount)?;
    let resolved =
        CanonicalHostPath::from_resolved(text).map_err(|_| SandboxError::ForbiddenMount)?;
    if is_forbidden_host_source(resolved.as_str()) || is_docker_socket(resolved.as_str()) {
        return Err(SandboxError::ForbiddenMount);
    }
    Ok(resolved)
}

fn is_forbidden_host_source(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let trimmed = lower.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return true;
    }
    if is_docker_socket(trimmed) {
        return true;
    }
    if sensitive_prefix_match(trimmed) {
        return true;
    }
    let parts: Vec<&str> = trimmed.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return true;
    }
    if matches!(parts[0], "etc" | "dev" | "proc" | "sys" | "root") {
        return true;
    }
    if parts.starts_with(&["var", "run", "docker"]) || parts.starts_with(&["run", "docker"]) {
        return true;
    }
    if parts.starts_with(&["private", "etc"])
        || parts.starts_with(&["private", "dev"])
        || parts.starts_with(&["private", "var", "root"])
        || parts.starts_with(&["private", "var", "run", "docker"])
    {
        return true;
    }
    if is_home_root(&parts) {
        return true;
    }
    if parts.iter().any(|part| {
        matches!(
            *part,
            ".ssh" | ".gnupg" | ".aws" | ".kube" | ".netrc" | "keychains"
        )
    }) {
        return true;
    }
    if parts.windows(2).any(|pair| pair == [".config", "gh"]) {
        return true;
    }
    false
}

fn is_forbidden_guest(path: &str) -> bool {
    is_docker_socket(path) || is_forbidden_host_source(path)
}

fn is_forbidden_guest_target(target: &RepoPath) -> bool {
    let raw = target.as_str();
    if is_docker_socket(raw) {
        return true;
    }
    let lower = raw.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "etc" | "dev" | "proc" | "sys" | "root" | "home" | "users"
    ) || lower.starts_with("etc/")
        || lower.starts_with("dev/")
        || lower.starts_with("proc/")
        || lower.starts_with("sys/")
        || lower.starts_with("root/")
        || lower.starts_with("home/")
        || lower.starts_with("users/")
}

fn sensitive_prefix_match(path: &str) -> bool {
    SENSITIVE_PREFIXES.iter().any(|prefix| {
        path == *prefix
            || path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/')
    })
}

fn is_home_root(parts: &[&str]) -> bool {
    matches!(parts, ["users"] | ["home"] | ["users", _] | ["home", _])
}

fn is_docker_socket(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let trimmed = lower.trim_end_matches('/');
    if trimmed == "docker.sock" || trimmed.ends_with("/docker.sock") {
        return true;
    }
    DOCKER_SOCKET_NAMES.contains(&trimmed)
}

fn is_proxy_env_name(name: &str) -> bool {
    PROXY_ENV_NAMES
        .iter()
        .any(|blocked| name.eq_ignore_ascii_case(blocked))
}

fn is_host_network_flag(part: &str) -> bool {
    let lower = part.to_ascii_lowercase();
    lower == "--network=host"
        || lower == "--net=host"
        || lower == "--share-net"
        || lower == "--network=host=true"
}

fn first_existing(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|path| Path::new(path).is_file())
}

fn cpu_limit_seconds(cpu_millis: u32) -> u64 {
    u64::from(cpu_millis.div_ceil(1_000)).max(1)
}

fn memory_limit_bytes(memory_mb: u32) -> u64 {
    u64::from(memory_mb)
        .saturating_mul(1024)
        .saturating_mul(1024)
}

fn runsc_version(
    program: &str,
    cancel: &CancellationToken,
) -> Result<Option<String>, SandboxError> {
    let Some(output) = run_bounded_output(program, &["--version"], cancel)? else {
        return Ok(None);
    };
    let text = if output.0.is_empty() {
        output.1
    } else {
        output.0
    };
    let line = match text.lines().next() {
        Some(line) => line,
        None => return Ok(None),
    };
    let ident = line
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    if ident.is_empty() || ident.len() > 64 {
        Ok(Some("runsc".to_owned()))
    } else {
        Ok(Some(ident))
    }
}

fn parse_runsc_features(
    program: &str,
    cancel: &CancellationToken,
) -> Result<GvisorFeatures, SandboxError> {
    let Some((stdout, stderr)) = run_bounded_output(program, &["--help"], cancel)? else {
        return Ok(GvisorFeatures::empty());
    };
    let mut text = stdout;
    text.push_str(&stderr);
    let lower = text.to_ascii_lowercase();
    Ok(GvisorFeatures {
        rootless: lower.contains("rootless"),
        network_none: lower.contains("network"),
        platform: lower.contains("platform").then(|| "systrap".to_owned()),
        least_mounts: false,
    })
}

fn run_bounded_output(
    program: &str,
    args: &[&str],
    cancel: &CancellationToken,
) -> Result<Option<(String, String)>, SandboxError> {
    check_cancel(cancel)?;
    let mut command = Command::new(program);
    command.args(args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.env_clear();
    isolate_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return Ok(None),
    };
    let stdout = match child.stdout.take() {
        Some(pipe) => pipe,
        None => {
            terminate_process_group(&mut child);
            return Ok(None);
        }
    };
    let stderr = match child.stderr.take() {
        Some(pipe) => pipe,
        None => {
            terminate_process_group(&mut child);
            return Ok(None);
        }
    };
    let stdout_thread = thread::spawn(move || read_capped(stdout, 64 * 1024));
    let stderr_thread = thread::spawn(move || read_capped(stderr, 64 * 1024));
    let started = Instant::now();
    let mut polls = 0u32;
    let status = loop {
        if polls.is_multiple_of(CANCEL_STRIDE) && cancel.is_cancelled() {
            terminate_process_group(&mut child);
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(SandboxError::Cancelled);
        }
        if started.elapsed() >= HEALTH_PROBE_TIMEOUT {
            terminate_process_group(&mut child);
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Ok(None);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(_) => {
                terminate_process_group(&mut child);
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Ok(None);
            }
        }
        polls = polls.saturating_add(1);
    };
    let stdout = stdout_thread
        .join()
        .map(|(buf, _)| String::from_utf8_lossy(&buf).into_owned())
        .unwrap_or_default();
    let stderr = stderr_thread
        .join()
        .map(|(buf, _)| String::from_utf8_lossy(&buf).into_owned())
        .unwrap_or_default();
    if status.success() {
        Ok(Some((stdout, stderr)))
    } else {
        Ok(None)
    }
}

fn probe_least_mount_platform(
    program: &str,
    cancel: &CancellationToken,
) -> Result<Option<String>, SandboxError> {
    let Some(true_bin) = first_existing(TRUE_PROGRAMS) else {
        return Ok(None);
    };
    for platform in PLATFORMS {
        check_cancel(cancel)?;
        if probe_private_rootfs(program, platform, true_bin, cancel)? {
            return Ok(Some((*platform).to_owned()));
        }
    }
    Ok(None)
}

fn probe_private_rootfs(
    program: &str,
    platform: &str,
    true_bin: &str,
    cancel: &CancellationToken,
) -> Result<bool, SandboxError> {
    check_cancel(cancel)?;
    let binds = system_ro_binds();
    if binds.is_empty() {
        return Ok(false);
    }
    if !run_probe_bundle(
        program,
        platform,
        &binds,
        &[true_bin],
        "/",
        1_000,
        256,
        32,
        cancel,
    )? {
        return Ok(false);
    }
    if host_sensitive_visible(program, platform, &binds, cancel)? {
        return Ok(false);
    }
    Ok(true)
}

fn host_sensitive_visible(
    program: &str,
    platform: &str,
    binds: &[PlannedBind],
    cancel: &CancellationToken,
) -> Result<bool, SandboxError> {
    if let Some(test_bin) = first_existing(TEST_PROGRAMS) {
        for path in HOST_SENSITIVE_PROBES {
            if run_probe_bundle(
                program,
                platform,
                binds,
                &[test_bin, "-e", path],
                "/",
                1_000,
                256,
                32,
                cancel,
            )? {
                return Ok(true);
            }
        }
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
            && run_probe_bundle(
                program,
                platform,
                binds,
                &[test_bin, "-e", &home],
                "/",
                1_000,
                256,
                32,
                cancel,
            )?
        {
            return Ok(true);
        }
        return Ok(false);
    }
    if let Some(ls) = first_existing(LS_PROGRAMS) {
        for path in HOST_SENSITIVE_PROBES {
            if run_probe_bundle(
                program,
                platform,
                binds,
                &[ls, path],
                "/",
                1_000,
                256,
                32,
                cancel,
            )? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    // Cannot verify isolation without a probe binary: fail closed.
    Ok(true)
}

fn system_ro_binds() -> Vec<PlannedBind> {
    let mut binds = Vec::new();
    for host in SYSTEM_RO_BINDS {
        if !Path::new(host).exists() {
            continue;
        }
        if is_forbidden_host_source(host) || is_docker_socket(host) {
            continue;
        }
        binds.push(PlannedBind {
            source: Some((*host).to_owned()),
            guest: (*host).to_owned(),
            mode: MountMode::ReadOnly,
        });
    }
    binds
}

#[allow(clippy::too_many_arguments)]
fn run_probe_bundle(
    program: &str,
    platform: &str,
    binds: &[PlannedBind],
    argv: &[&str],
    cwd: &str,
    cpu_millis: u32,
    memory_mb: u32,
    pids: u32,
    cancel: &CancellationToken,
) -> Result<bool, SandboxError> {
    check_cancel(cancel)?;
    let bundle = match create_bundle(binds, argv, cwd, cpu_millis, memory_mb, pids) {
        Ok(bundle) => bundle,
        Err(_) => return Ok(false),
    };
    let mut command = Command::new(program);
    command.args([
        "--rootless",
        "--network=none",
        "--ignore-cgroups",
        "--platform",
        platform,
    ]);
    if let Some(state) = bundle.state.to_str() {
        command.arg("--root");
        command.arg(state);
    } else {
        return Ok(false);
    }
    command.arg("run");
    if let Some(bundle_dir) = bundle.bundle.to_str() {
        command.arg("--bundle");
        command.arg(bundle_dir);
    } else {
        return Ok(false);
    }
    command.arg(&bundle.cid);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    command.env_clear();
    isolate_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            delete_container(program, &bundle.state, &bundle.cid, cancel);
            return Ok(false);
        }
    };
    let ok = wait_probe(&mut child, cancel)?;
    delete_container(program, &bundle.state, &bundle.cid, cancel);
    Ok(ok)
}

fn wait_probe(child: &mut Child, cancel: &CancellationToken) -> Result<bool, SandboxError> {
    let started = Instant::now();
    let mut polls = 0u32;
    loop {
        if polls.is_multiple_of(CANCEL_STRIDE) && cancel.is_cancelled() {
            terminate_process_group(child);
            return Err(SandboxError::Cancelled);
        }
        if started.elapsed() >= HEALTH_PROBE_TIMEOUT {
            terminate_process_group(child);
            return Ok(false);
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success()),
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(_) => {
                terminate_process_group(child);
                return Ok(false);
            }
        }
        polls = polls.saturating_add(1);
    }
}

fn run_gvisor(
    plan: &GvisorPlan,
    request: &SandboxExecRequest,
    sandbox_id: SandboxId,
    cancel: &CancellationToken,
) -> Result<SandboxExecResult, SandboxError> {
    check_cancel(cancel)?;
    if plan.uses_docker_socket() {
        return Err(SandboxError::ForbiddenMount);
    }
    if plan.shares_host_network() {
        return Err(SandboxError::UnsupportedNetwork);
    }
    if plan.uses_host_rootfs() || !plan.has_least_mounts() {
        return Err(SandboxError::TierUnavailable);
    }
    for bind in &plan.binds {
        revalidate_bind(bind)?;
    }
    let argv = request.argv();
    let program = resolve_existing_file(&argv[0])?;
    if !guest_path_is_visible(program.as_str(), plan) {
        return Err(SandboxError::ForbiddenMount);
    }
    let mut guest_argv = Vec::with_capacity(argv.len());
    guest_argv.push(program.as_str().to_owned());
    guest_argv.extend(argv.iter().skip(1).cloned());
    let guest_refs: Vec<&str> = guest_argv.iter().map(String::as_str).collect();
    let bundle = create_bundle(
        &plan.binds,
        &guest_refs,
        &plan.guest_cwd,
        plan.cpu_millis,
        plan.memory_mb,
        plan.pids,
    )?;
    let mut command = Command::new(&plan.program);
    command.arg("--rootless");
    command.arg("--network=none");
    command.arg("--ignore-cgroups");
    command.arg("--platform");
    command.arg(&plan.platform);
    let Some(state) = bundle.state.to_str() else {
        return Err(SandboxError::ForbiddenMount);
    };
    command.arg("--root");
    command.arg(state);
    command.arg("run");
    let Some(bundle_dir) = bundle.bundle.to_str() else {
        return Err(SandboxError::ForbiddenMount);
    };
    command.arg("--bundle");
    command.arg(bundle_dir);
    command.arg(&bundle.cid);
    command.env_clear();
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    isolate_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            delete_container(&plan.program, &bundle.state, &bundle.cid, cancel);
            return Err(if err.kind() == std::io::ErrorKind::NotFound {
                SandboxError::TierUnavailable
            } else {
                SandboxError::HealthFailed
            });
        }
    };
    let started = Instant::now();
    let outcome = wait_child(
        &mut child,
        request.timeout(),
        request.output_limit(),
        plan,
        cancel,
    );
    delete_container(&plan.program, &bundle.state, &bundle.cid, cancel);
    let _ = sandbox_id;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    fn usage_bytes(output: &[u8]) -> u64 {
        u64::try_from(output.len()).unwrap_or(u64::MAX)
    }
    match outcome {
        WaitOutcome::Finished {
            code,
            signal,
            output,
            usage,
        } => Ok(SandboxExecResult::new(
            SandboxExit::new(
                code,
                signal,
                SandboxExitReason::Exited,
                false,
                false,
                false,
                ResourceUsage::new(
                    elapsed_ms,
                    usage.memory_peak_mb,
                    usage.pids_peak,
                    usage_bytes(&output),
                ),
            ),
            output,
        )),
        WaitOutcome::TimedOut { output, usage } => Ok(SandboxExecResult::new(
            SandboxExit::new(
                None,
                None,
                SandboxExitReason::TimedOut,
                false,
                true,
                false,
                ResourceUsage::new(
                    elapsed_ms,
                    usage.memory_peak_mb,
                    usage.pids_peak,
                    usage_bytes(&output),
                ),
            ),
            output,
        )),
        WaitOutcome::Cancelled { output, usage } => Ok(SandboxExecResult::new(
            SandboxExit::new(
                None,
                None,
                SandboxExitReason::Cancelled,
                false,
                false,
                false,
                ResourceUsage::new(
                    elapsed_ms,
                    usage.memory_peak_mb,
                    usage.pids_peak,
                    usage_bytes(&output),
                ),
            ),
            output,
        )),
        WaitOutcome::Oom { output, usage } => Ok(SandboxExecResult::new(
            SandboxExit::new(
                None,
                None,
                SandboxExitReason::Oom,
                true,
                false,
                false,
                ResourceUsage::new(
                    elapsed_ms,
                    usage.memory_peak_mb,
                    usage.pids_peak,
                    usage_bytes(&output),
                ),
            ),
            output,
        )),
        WaitOutcome::PidsExceeded { output, usage } => Ok(SandboxExecResult::new(
            SandboxExit::new(
                None,
                None,
                SandboxExitReason::PolicyViolation,
                false,
                false,
                true,
                ResourceUsage::new(
                    elapsed_ms,
                    usage.memory_peak_mb,
                    usage.pids_peak,
                    usage_bytes(&output),
                ),
            ),
            output,
        )),
        WaitOutcome::Failed => Err(SandboxError::HealthFailed),
    }
}

fn guest_path_is_visible(host_path: &str, plan: &GvisorPlan) -> bool {
    plan.binds.iter().any(|bind| {
        let Some(source) = bind.source.as_deref() else {
            return false;
        };
        path_is_within(source, host_path)
            || (bind.guest != "/" && path_is_within(&bind.guest, host_path))
    })
}

fn revalidate_bind(bind: &PlannedBind) -> Result<(), SandboxError> {
    if is_docker_socket(&bind.guest) || is_forbidden_guest(&bind.guest) {
        return Err(SandboxError::ForbiddenMount);
    }
    match bind.mode {
        MountMode::Temp => {
            if bind.source.is_some() {
                return Err(SandboxError::InvalidSpec);
            }
            Ok(())
        }
        MountMode::ReadOnly | MountMode::ReadWrite => {
            let source = bind.source.as_deref().ok_or(SandboxError::InvalidSpec)?;
            if source == "/" || is_forbidden_host_source(source) || is_docker_socket(source) {
                return Err(SandboxError::ForbiddenMount);
            }
            let resolved = resolve_existing_dir(Path::new(source))?;
            if resolved.as_str() == "/"
                || is_forbidden_host_source(resolved.as_str())
                || is_docker_socket(resolved.as_str())
            {
                return Err(SandboxError::ForbiddenMount);
            }
            Ok(())
        }
    }
}

fn create_bundle(
    binds: &[PlannedBind],
    argv: &[&str],
    cwd: &str,
    cpu_millis: u32,
    memory_mb: u32,
    pids: u32,
) -> Result<ExecBundle, SandboxError> {
    if argv.is_empty() {
        return Err(SandboxError::Empty);
    }
    if cpu_millis == 0 || memory_mb == 0 || pids == 0 {
        return Err(SandboxError::ResourceLimit);
    }
    for bind in binds {
        revalidate_bind(bind)?;
    }
    let mut root = std::env::temp_dir();
    root.push(format!("rapidlm-gvisor-{}", RuntimeId::new()));
    fs::create_dir_all(&root).map_err(|_| SandboxError::ResourceLimit)?;
    let bundle_dir = root.join("bundle");
    let rootfs = bundle_dir.join("rootfs");
    let state = root.join("state");
    if fs::create_dir_all(&rootfs).is_err() || fs::create_dir_all(&state).is_err() {
        let _ = fs::remove_dir_all(&root);
        return Err(SandboxError::ResourceLimit);
    }
    if create_dir_in_rootfs(&rootfs, cwd).is_err() {
        let _ = fs::remove_dir_all(&root);
        return Err(SandboxError::ResourceLimit);
    }
    if create_dir_in_rootfs(&rootfs, "/tmp").is_err() {
        let _ = fs::remove_dir_all(&root);
        return Err(SandboxError::ResourceLimit);
    }
    for bind in binds {
        if create_dir_in_rootfs(&rootfs, &bind.guest).is_err() {
            let _ = fs::remove_dir_all(&root);
            return Err(SandboxError::ResourceLimit);
        }
    }
    let cid = format!("rlm{}", RuntimeId::new()).replace('-', "");
    let spec = match write_oci_spec(&rootfs, binds, argv, cwd, cpu_millis, memory_mb, pids) {
        Ok(spec) => spec,
        Err(err) => {
            let _ = fs::remove_dir_all(&root);
            return Err(err);
        }
    };
    if fs::write(bundle_dir.join("config.json"), spec).is_err() {
        let _ = fs::remove_dir_all(&root);
        return Err(SandboxError::ResourceLimit);
    }
    Ok(ExecBundle {
        root,
        bundle: bundle_dir,
        state,
        cid,
    })
}

fn create_dir_in_rootfs(rootfs: &Path, guest: &str) -> Result<(), SandboxError> {
    let trimmed = guest.trim_start_matches('/');
    let dest = if trimmed.is_empty() {
        rootfs.to_path_buf()
    } else {
        rootfs.join(trimmed)
    };
    fs::create_dir_all(&dest).map_err(|_| SandboxError::ResourceLimit)?;
    Ok(())
}

fn write_oci_spec(
    rootfs: &Path,
    binds: &[PlannedBind],
    argv: &[&str],
    cwd: &str,
    cpu_millis: u32,
    memory_mb: u32,
    pids: u32,
) -> Result<String, SandboxError> {
    let rootfs_str = rootfs.to_str().ok_or(SandboxError::ForbiddenMount)?;
    let cpu = cpu_limit_seconds(cpu_millis);
    let memory = memory_limit_bytes(memory_mb);
    if memory == 0 {
        return Err(SandboxError::ResourceLimit);
    }
    let mut args_json = String::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            args_json.push(',');
        }
        args_json.push('"');
        args_json.push_str(&json_escape(arg));
        args_json.push('"');
    }
    let mut mounts = String::from(
        r#"{"destination":"/proc","type":"proc","source":"proc"},{"destination":"/tmp","type":"tmpfs","source":"tmpfs","options":["nosuid","nodev","mode=1777"]}"#,
    );
    for bind in binds {
        mounts.push(',');
        match bind.mode {
            MountMode::Temp => {
                mounts.push_str(&format!(
                    r#"{{"destination":"{}","type":"tmpfs","source":"tmpfs","options":["nosuid","nodev","mode=1777"]}}"#,
                    json_escape(&bind.guest)
                ));
            }
            MountMode::ReadOnly | MountMode::ReadWrite => {
                let source = bind.source.as_deref().ok_or(SandboxError::InvalidSpec)?;
                let ro = matches!(bind.mode, MountMode::ReadOnly);
                let opts = if ro {
                    r#"["rbind","ro","nosuid","nodev"]"#
                } else {
                    r#"["rbind","rw","nosuid","nodev"]"#
                };
                mounts.push_str(&format!(
                    r#"{{"destination":"{}","type":"bind","source":"{}","options":{}}}"#,
                    json_escape(&bind.guest),
                    json_escape(source),
                    opts
                ));
            }
        }
    }
    Ok(format!(
        r#"{{"ociVersion":"1.0.2","process":{{"terminal":false,"user":{{"uid":0,"gid":0}},"args":[{args_json}],"env":["PATH=/usr/bin:/bin","HOME=/tmp"],"cwd":"{}","noNewPrivileges":true,"rlimits":[{{"type":"RLIMIT_CPU","hard":{cpu},"soft":{cpu}}},{{"type":"RLIMIT_NPROC","hard":{pids},"soft":{pids}}},{{"type":"RLIMIT_AS","hard":{memory},"soft":{memory}}}]}},"root":{{"path":"{}","readonly":false}},"hostname":"sandbox","mounts":[{mounts}],"linux":{{"namespaces":[{{"type":"pid"}},{{"type":"network"}},{{"type":"ipc"}},{{"type":"uts"}},{{"type":"mount"}}]}}}}"#,
        json_escape(cwd),
        json_escape(rootfs_str),
    ))
}

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let code = u32::from(c);
                out.push_str(&format!("\\u{code:04x}"));
            }
            c => out.push(c),
        }
    }
    out
}

fn delete_container(program: &str, state: &Path, cid: &str, cancel: &CancellationToken) {
    if cancel.is_cancelled() {
        let _ = state;
        return;
    }
    let Some(state) = state.to_str() else {
        return;
    };
    let mut command = Command::new(program);
    command.args(["--root", state, "delete", "--force", cid]);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    command.env_clear();
    isolate_process_group(&mut command);
    if let Ok(mut child) = command.spawn() {
        let started = Instant::now();
        loop {
            if started.elapsed() >= Duration::from_secs(2) {
                terminate_process_group(&mut child);
                break;
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(_) => {
                    terminate_process_group(&mut child);
                    break;
                }
            }
        }
    }
}

enum WaitOutcome {
    Finished {
        code: Option<i32>,
        signal: Option<i32>,
        output: Vec<u8>,
        usage: GroupUsage,
    },
    TimedOut {
        output: Vec<u8>,
        usage: GroupUsage,
    },
    Cancelled {
        output: Vec<u8>,
        usage: GroupUsage,
    },
    Oom {
        output: Vec<u8>,
        usage: GroupUsage,
    },
    PidsExceeded {
        output: Vec<u8>,
        usage: GroupUsage,
    },
    Failed,
}

#[derive(Clone, Copy)]
struct GroupUsage {
    memory_peak_mb: u64,
    pids_peak: u32,
}

impl GroupUsage {
    const fn empty() -> Self {
        Self {
            memory_peak_mb: 0,
            pids_peak: 1,
        }
    }
}

fn wait_child(
    child: &mut Child,
    timeout: Duration,
    output_limit: u64,
    plan: &GvisorPlan,
    cancel: &CancellationToken,
) -> WaitOutcome {
    let cap = usize::try_from(output_limit).unwrap_or(usize::MAX);
    let stdout = match child.stdout.take() {
        Some(pipe) => pipe,
        None => {
            terminate_process_group(child);
            return WaitOutcome::Failed;
        }
    };
    let stderr = match child.stderr.take() {
        Some(pipe) => pipe,
        None => {
            terminate_process_group(child);
            return WaitOutcome::Failed;
        }
    };
    let stdout_thread = thread::spawn(move || read_capped(stdout, cap));
    let stderr_thread = thread::spawn(move || read_capped(stderr, cap));
    let started = Instant::now();
    let mut polls = 0u32;
    let mut usage = GroupUsage::empty();
    let mut sample_misses = 0u32;
    let pgid = child.id();
    let status = loop {
        if polls.is_multiple_of(CANCEL_STRIDE) && cancel.is_cancelled() {
            terminate_process_group(child);
            let output = join_output(stdout_thread, stderr_thread);
            return WaitOutcome::Cancelled { output, usage };
        }
        if started.elapsed() >= timeout {
            terminate_process_group(child);
            let output = join_output(stdout_thread, stderr_thread);
            return WaitOutcome::TimedOut { output, usage };
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                match sample_process_group(pgid) {
                    Some(sample) => {
                        sample_misses = 0;
                        usage.pids_peak = usage.pids_peak.max(sample.0);
                        usage.memory_peak_mb = usage.memory_peak_mb.max(sample.1);
                        if sample.1 > u64::from(plan.memory_mb) {
                            terminate_process_group(child);
                            let output = join_output(stdout_thread, stderr_thread);
                            return WaitOutcome::Oom { output, usage };
                        }
                        if sample.0 > plan.pids {
                            terminate_process_group(child);
                            let output = join_output(stdout_thread, stderr_thread);
                            return WaitOutcome::PidsExceeded { output, usage };
                        }
                    }
                    None => {
                        sample_misses = sample_misses.saturating_add(1);
                        if sample_misses >= SAMPLE_MISS_LIMIT {
                            terminate_process_group(child);
                            let _ = stdout_thread.join();
                            let _ = stderr_thread.join();
                            return WaitOutcome::Failed;
                        }
                    }
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(_) => {
                terminate_process_group(child);
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return WaitOutcome::Failed;
            }
        }
        polls = polls.saturating_add(1);
    };
    let output = join_output(stdout_thread, stderr_thread);
    WaitOutcome::Finished {
        code: status.code(),
        signal: exit_signal(&status),
        output,
        usage,
    }
}

/// Combined, already-capped stdout+stderr bytes: stdout first, then stderr —
/// the two are read concurrently on separate pipes/threads, so there is no
/// real chronological interleaving to preserve.
fn join_output(
    stdout: thread::JoinHandle<(Vec<u8>, bool)>,
    stderr: thread::JoinHandle<(Vec<u8>, bool)>,
) -> Vec<u8> {
    let mut out = stdout.join().map(|(buf, _)| buf).unwrap_or_default();
    let mut err = stderr.join().map(|(buf, _)| buf).unwrap_or_default();
    out.append(&mut err);
    out
}

fn read_capped(mut pipe: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        match pipe.read(&mut tmp) {
            Ok(0) => return (buf, false),
            Ok(n) => {
                if buf.len().saturating_add(n) > cap {
                    let keep = cap.saturating_sub(buf.len());
                    buf.extend_from_slice(&tmp[..keep]);
                    let mut drain = [0u8; 8192];
                    while let Ok(read) = pipe.read(&mut drain) {
                        if read == 0 {
                            break;
                        }
                    }
                    return (buf, true);
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(_) => return (buf, false),
        }
    }
}

fn isolate_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
}

fn terminate_process_group(child: &mut Child) {
    let pid = child.id();
    if pid >= 2 {
        let _ = signal_group(pid, GroupSignal::Term);
        let grace_deadline = Instant::now() + TERM_GRACE;
        while Instant::now() < grace_deadline {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(_) => break,
            }
        }
        let _ = signal_group(pid, GroupSignal::Kill);
        let kill_deadline = Instant::now() + KILL_WAIT;
        while Instant::now() < kill_deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(_) => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// One group signal, by `kill(2)` — never `kill(1)`, whose procps-ng parser
/// made `-<pgid>` the broadcast (see `process_signal`). An absent group is
/// `Ok`; anything else is the backend's health failure.
fn signal_group(pgid: u32, kind: GroupSignal) -> Result<(), SandboxError> {
    process_signal::signal_process_group(pgid, kind).map_err(|_| SandboxError::HealthFailed)
}

fn sample_process_group(pgid: u32) -> Option<(u32, u64)> {
    if pgid < 2 {
        return None;
    }
    let pids = group_pids(pgid)?;
    if pids.is_empty() {
        return None;
    }
    let count = u32::try_from(pids.len()).unwrap_or(u32::MAX);
    let mut rss_kb = 0u64;
    for pid in &pids {
        rss_kb = rss_kb.saturating_add(pid_rss_kb(*pid)?);
    }
    let memory_mb = rss_kb.div_ceil(1024);
    Some((count, memory_mb))
}

fn group_pids(pgid: u32) -> Option<Vec<u32>> {
    if let Some(pids) = pgrep_group(pgid) {
        return Some(pids);
    }
    ps_group(pgid)
}

fn pgrep_group(pgid: u32) -> Option<Vec<u32>> {
    let program = first_existing(PGREP_PROGRAMS)?;
    let output = Command::new(program)
        .args(["-g", &pgid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .output()
        .ok()?;
    let mut pids = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Ok(pid) = line.trim().parse::<u32>()
            && pid >= 2
        {
            pids.push(pid);
        }
    }
    if pids.is_empty() { None } else { Some(pids) }
}

fn ps_group(pgid: u32) -> Option<Vec<u32>> {
    let program = first_existing(PS_PROGRAMS)?;
    let output = Command::new(program)
        .args(["-ax", "-o", "pid=,pgid="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut pids = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut cols = line.split_whitespace();
        let Some(pid) = cols.next().and_then(|c| c.parse::<u32>().ok()) else {
            continue;
        };
        let Some(group) = cols.next().and_then(|c| c.parse::<u32>().ok()) else {
            continue;
        };
        if group == pgid && pid >= 2 {
            pids.push(pid);
        }
    }
    if pids.is_empty() { None } else { Some(pids) }
}

fn pid_rss_kb(pid: u32) -> Option<u64> {
    let program = first_existing(PS_PROGRAMS)?;
    let output = Command::new(program)
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .and_then(|col| col.parse::<u64>().ok())
}

fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
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
    if lease.is_expired(Instant::now()) || lease.remaining_uses() == 0 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use capability_broker::{
        ActionRequest, ApprovalChoice, ApprovalResolution, ApprovalScopeId, CanonicalAction,
        FilesystemScope, LeaseIssuer, PolicyDocument, PolicySource, PolicyStack, PrincipalRef,
        ProcessScope, ResourceDescriptor, SecretHandle, evaluate, issue, request_approval,
    };
    use protocol::{ErrorCode, SessionId};

    use crate::backend::SandboxManager;
    use crate::backends::container::ContainerBackend;
    use crate::backends::host_restricted::HostRestrictedBackend;

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-7c1e9b";

    struct TempWorkspace {
        path: PathBuf,
        host: CanonicalHostPath,
    }

    impl TempWorkspace {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("rapidlm-gvisor-sbx-{}", protocol::RuntimeId::new()));
            fs::create_dir_all(&path).expect("temp workspace");
            let canon = fs::canonicalize(&path).expect("canonicalize");
            let host =
                CanonicalHostPath::from_resolved(canon.to_str().expect("utf8")).expect("host");
            Self { path, host }
        }

        fn mount(&self, target: &str, mode: MountMode) -> SandboxMount {
            SandboxMount::bind(
                self.host.clone(),
                RepoPath::parse(target).expect("target"),
                mode,
            )
            .expect("mount")
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn cwd() -> RepoPath {
        RepoPath::parse("src").expect("cwd")
    }

    fn gvisor_spec(ws: &TempWorkspace) -> SandboxSpec {
        SandboxSpec::builder(SandboxTier::Gvisor)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .build()
            .expect("spec")
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

    fn host_tool(candidates: &[&'static str]) -> Option<&'static str> {
        candidates
            .iter()
            .copied()
            .find(|path| Path::new(path).is_file())
    }

    fn live_runtime(backend: &GvisorBackend) -> Option<GvisorSupport> {
        let support = backend
            .supports(&CancellationToken::new())
            .expect("supports probe");
        if support.is_ready() {
            Some(support)
        } else {
            None
        }
    }

    #[test]
    fn isolation_is_syscall_mediation_and_strong() {
        let backend = GvisorBackend::new();
        let caps = backend.capabilities();
        assert_eq!(caps.tier(), SandboxTier::Gvisor);
        assert_eq!(caps.isolation(), IsolationStrength::SyscallMediation);
        assert!(caps.isolation().is_strong_isolation());
        assert!(caps.isolation().doctor_warning().is_none());
        assert!(caps.network().allowlist_supported());
        assert!(caps.network().proxy_supported());
    }

    #[test]
    fn plan_constructs_runsc_argv_without_docker_or_host_net() {
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::Gvisor)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .mount(SandboxMount::temp(RepoPath::parse("tmp").expect("tmp")).expect("temp"))
            .network(SandboxNetwork::None)
            .secret(SecretHandle::parse(CANARY).expect("secret"))
            .build()
            .expect("spec");
        let plan = GvisorPlan::from_spec(&spec).expect("plan");
        let argv = plan.argv();
        assert_eq!(plan.runtime(), GvisorRuntime::Runsc);
        assert_eq!(plan.runtime().as_str(), "runsc");
        assert_eq!(plan.network(), GvisorNetwork::Isolated);
        assert_eq!(plan.network().as_str(), "none");
        assert!(plan.is_rootless());
        assert!(plan.has_network_none());
        assert!(plan.has_least_mounts());
        assert!(!plan.uses_host_rootfs());
        assert!(!plan.shares_host_network());
        assert!(!plan.uses_docker_socket());
        assert!(plan.is_strong_isolation());
        assert!(argv.iter().any(|part| part == "--rootless"));
        assert!(argv.iter().any(|part| part == "--network=none"));
        assert!(argv.iter().any(|part| part == "--platform"));
        assert!(!argv.iter().any(|part| part == "do"));
        assert!(!argv.iter().any(|part| is_host_network_flag(part)));
        assert!(!argv.iter().any(|part| part.contains("docker.sock")));
        assert!(!argv.iter().any(|part| part == "--privileged"));
        assert!(!argv.iter().any(|part| part.contains(CANARY)));
        assert_eq!(plan.secret_count(), 1);
        assert!(plan.cpu_millis() > 0);
        assert!(plan.memory_mb() > 0);
        assert!(plan.pids() > 0);
        let debug = format!("{plan:?}");
        assert!(!debug.contains(CANARY));
        assert!(!debug.contains("PLAINTEXT"));
        assert!(!plan.program().ends_with("docker"));
        assert!(plan.program().ends_with("runsc"));
        assert!(plan.bind_sources().all(|src| src != "/"));
        assert!(plan.bind_sources().all(|src| !is_docker_socket(src)));
        assert!(
            plan.bind_sources()
                .all(|src| !is_forbidden_host_source(src) || SYSTEM_RO_BINDS.contains(&src))
        );
    }

    #[test]
    fn supports_reports_runsc_version_features_or_skip_reason() {
        let backend = GvisorBackend::new();
        let live = CancellationToken::new();
        let support = backend.supports(&live).expect("supports");
        let health = backend.health(&live).expect("health");
        if support.is_ready() {
            assert!(health.is_available());
            assert!(health.reason().is_none());
            assert!(support.reason().is_none());
            assert!(support.program().is_some());
            assert!(support.version().is_some());
            assert!(support.features().is_complete());
            assert!(support.features().rootless());
            assert!(support.features().network_none());
            assert!(support.features().least_mounts());
            assert!(support.features().platform().is_some());
            return;
        }
        assert!(!health.is_available(), "unavailable is not a clean/pass");
        let reason = support.reason().expect("skip capability reason");
        assert_eq!(health.reason(), Some(reason));
        assert!(
            matches!(
                reason,
                HealthReason::PlatformUnsupported
                    | HealthReason::RuntimeMissing
                    | HealthReason::FeatureMissing
            ),
            "{reason:?}"
        );
        if !cfg!(target_os = "linux") {
            assert_eq!(reason, HealthReason::PlatformUnsupported);
        }
        assert!(!support.features().is_complete());
        assert!(!support.features().least_mounts());
    }

    #[test]
    fn required_gvisor_never_silently_uses_container_backend() {
        let mut mgr = SandboxManager::new();
        mgr.register(Box::new(HostRestrictedBackend::new()))
            .expect("host");
        mgr.register(Box::new(ContainerBackend::new()))
            .expect("container");
        mgr.register(Box::new(GvisorBackend::new()))
            .expect("gvisor");
        let ws = TempWorkspace::new();
        let spec = gvisor_spec(&ws);
        let live = CancellationToken::new();
        if live_runtime(&GvisorBackend::new()).is_some() {
            let selected = mgr.select(&spec, &live).expect("select");
            assert_eq!(selected.capabilities().tier(), SandboxTier::Gvisor);
            assert_eq!(
                selected.capabilities().isolation(),
                IsolationStrength::SyscallMediation
            );
            return;
        }
        match mgr.select(&spec, &live) {
            Ok(selected) => panic!(
                "required gvisor must not select {:?}/{:?}",
                selected.capabilities().tier(),
                selected.capabilities().isolation()
            ),
            Err(err) => {
                assert_eq!(err, SandboxError::TierUnavailable);
                assert_eq!(err.error_code(), Some(ErrorCode::SandboxTierUnavailable));
                assert!(!err.as_str().contains(CANARY));
            }
        }
    }

    #[test]
    fn prepare_refuses_weaker_tiers_images_and_wrong_lease() {
        let backend = GvisorBackend::new();
        let ws = TempWorkspace::new();
        let live = CancellationToken::new();
        let lease = proc_lease();
        let host = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .build()
            .expect("host");
        assert_eq!(
            backend.prepare(&host, &lease, &live).expect_err("host"),
            SandboxError::TierUnavailable
        );
        let container = SandboxSpec::builder(SandboxTier::Container)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .build()
            .expect("container");
        assert_eq!(
            backend
                .prepare(&container, &lease, &live)
                .expect_err("container"),
            SandboxError::TierUnavailable
        );
        let imaged = SandboxSpec::builder(SandboxTier::Gvisor)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .image("gvisor:latest")
            .build()
            .expect("image");
        assert_eq!(
            backend.prepare(&imaged, &lease, &live).expect_err("image"),
            SandboxError::InvalidSpec
        );
        assert_eq!(
            backend
                .prepare(&gvisor_spec(&ws), &fs_lease(), &live)
                .expect_err("fs"),
            SandboxError::LeaseInvalid
        );
        assert_eq!(
            SandboxError::LeaseInvalid.error_code(),
            Some(ErrorCode::PolicyLeaseInvalid)
        );
    }

    #[test]
    fn docker_socket_home_and_etc_cannot_bypass_path_policy() {
        let backend = GvisorBackend::new();
        let lease = proc_lease();
        let live = CancellationToken::new();
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
        let home = CanonicalHostPath::from_resolved("/Users/canary-home").expect("home");
        let spec = SandboxSpec::builder(SandboxTier::Gvisor)
            .cwd(cwd())
            .mount(SandboxMount::bind(home, cwd(), MountMode::ReadWrite).expect("bind"))
            .build()
            .expect("spec");
        assert_eq!(
            backend.prepare(&spec, &lease, &live).expect_err("home"),
            SandboxError::ForbiddenMount
        );
        let etc = CanonicalHostPath::from_resolved("/etc").expect("etc");
        let spec = SandboxSpec::builder(SandboxTier::Gvisor)
            .cwd(cwd())
            .mount(SandboxMount::bind(etc, cwd(), MountMode::ReadOnly).expect("bind"))
            .build()
            .expect("etc spec");
        assert_eq!(
            backend.prepare(&spec, &lease, &live).expect_err("etc"),
            SandboxError::ForbiddenMount
        );
        assert!(
            !SandboxError::ForbiddenMount
                .as_str()
                .contains("docker.sock")
        );
        assert!(
            !SandboxError::ForbiddenMount
                .as_str()
                .contains("canary-home")
        );
        assert!(!SandboxError::ForbiddenMount.as_str().contains(CANARY));
        assert_eq!(
            SandboxError::ForbiddenMount.error_code(),
            Some(ErrorCode::PolicyDenied)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_sensitive_path_is_forbidden() {
        let backend = GvisorBackend::new();
        let lease = proc_lease();
        let live = CancellationToken::new();

        let etc_ws = TempWorkspace::new();
        let via_etc = etc_ws.path.join("via");
        std::os::unix::fs::symlink("/etc", &via_etc).expect("symlink etc");
        assert!(via_etc.exists(), "/etc must exist through symlink");
        let etc_host =
            CanonicalHostPath::from_resolved(via_etc.to_str().expect("utf8")).expect("etc host");
        assert!(
            !is_forbidden_host_source(etc_host.as_str()),
            "lexical path must look like a temp workspace, not /etc"
        );
        let etc_spec = SandboxSpec::builder(SandboxTier::Gvisor)
            .cwd(cwd())
            .mount(SandboxMount::bind(etc_host, cwd(), MountMode::ReadWrite).expect("bind"))
            .build()
            .expect("etc spec");
        assert_eq!(
            backend
                .prepare(&etc_spec, &lease, &live)
                .expect_err("etc symlink"),
            SandboxError::ForbiddenMount
        );

        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
            && Path::new(&home).is_dir()
        {
            let home_ws = TempWorkspace::new();
            let via_home = home_ws.path.join("via");
            std::os::unix::fs::symlink(&home, &via_home).expect("symlink home");
            let home_host = CanonicalHostPath::from_resolved(via_home.to_str().expect("utf8"))
                .expect("home host");
            let home_spec = SandboxSpec::builder(SandboxTier::Gvisor)
                .cwd(cwd())
                .mount(SandboxMount::bind(home_host, cwd(), MountMode::ReadWrite).expect("bind"))
                .build()
                .expect("home spec");
            assert_eq!(
                backend
                    .prepare(&home_spec, &lease, &live)
                    .expect_err("home symlink"),
                SandboxError::ForbiddenMount
            );
        }

        let cwd_ws = TempWorkspace::new();
        std::os::unix::fs::symlink("/etc", cwd_ws.path.join("escape")).expect("cwd symlink");
        let cwd_spec = SandboxSpec::builder(SandboxTier::Gvisor)
            .cwd(RepoPath::parse("src/escape").expect("cwd"))
            .mount(cwd_ws.mount("src", MountMode::ReadWrite))
            .build()
            .expect("cwd spec");
        assert_eq!(
            GvisorPlan::from_spec(&cwd_spec).expect_err("cwd escape"),
            SandboxError::ForbiddenMount
        );
    }

    #[test]
    fn fixture_runs_when_runsc_present_otherwise_skip_reason() {
        let backend = GvisorBackend::new();
        let ws = TempWorkspace::new();
        let spec = gvisor_spec(&ws);
        let plan = GvisorPlan::from_spec(&spec).expect("plan");
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            assert!(
                plan.bind_sources().all(|src| src != home.as_str()
                    && !src.starts_with(&format!("{}/", home.trim_end_matches('/')))),
                "host home must not be a bind source"
            );
        }
        assert!(!plan.uses_docker_socket());
        assert!(!plan.shares_host_network());
        assert!(!plan.uses_host_rootfs());
        assert!(plan.has_least_mounts());

        let live = CancellationToken::new();
        let Some(support) = live_runtime(&backend) else {
            let health = backend.health(&live).expect("health");
            assert!(!health.is_available());
            let reason = health.reason().expect("skip capability reason");
            assert!(
                matches!(
                    reason,
                    HealthReason::PlatformUnsupported
                        | HealthReason::RuntimeMissing
                        | HealthReason::FeatureMissing
                ),
                "{reason:?}"
            );
            assert_eq!(
                backend
                    .prepare(&spec, &proc_lease(), &live)
                    .expect_err("no runtime"),
                SandboxError::TierUnavailable
            );
            return;
        };
        assert!(support.features().is_complete());
        assert!(support.features().least_mounts());
        let lease = proc_lease();
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");
        assert_eq!(handle.tier(), SandboxTier::Gvisor);
        let true_bin = host_tool(&["/usr/bin/true", "/bin/true"]).expect("true");
        let request =
            SandboxExecRequest::new([true_bin], Duration::from_secs(5), 4096).expect("request");
        let result = backend
            .exec(&handle, &request, &lease, &live)
            .expect("fixture");
        assert_eq!(result.exit().reason(), SandboxExitReason::Exited);
        assert_eq!(result.exit().code(), Some(0));
        assert!(!result.exit().timed_out());
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn docker_sock_home_and_etc_are_unreachable_or_health_fails() {
        let backend = GvisorBackend::new();
        let ws = TempWorkspace::new();
        let spec = gvisor_spec(&ws);
        let plan = GvisorPlan::from_spec(&spec).expect("plan");
        assert!(plan.bind_sources().all(|src| !is_docker_socket(src)));
        assert!(plan.bind_sources().all(|src| src != "/etc"
            && !src.starts_with("/etc/")
            && src != "/private/etc"
            && !src.starts_with("/private/etc/")));
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            assert!(plan.bind_sources().all(|src| src != home.as_str()
                && !src.starts_with(&format!("{}/", home.trim_end_matches('/')))));
        }

        let live = CancellationToken::new();
        let Some(_) = live_runtime(&backend) else {
            let health = backend.health(&live).expect("health");
            assert!(!health.is_available(), "unavailable is not a clean/pass");
            assert!(matches!(
                health.reason(),
                Some(
                    HealthReason::PlatformUnsupported
                        | HealthReason::RuntimeMissing
                        | HealthReason::FeatureMissing
                )
            ));
            assert_eq!(
                backend
                    .prepare(&spec, &proc_lease(), &live)
                    .expect_err("no runtime"),
                SandboxError::TierUnavailable
            );
            return;
        };

        let lease = proc_lease();
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");
        let ls = host_tool(&["/bin/ls", "/usr/bin/ls"]).expect("ls");
        for path in ["/etc", "/etc/passwd", "/var/run/docker.sock"] {
            let request =
                SandboxExecRequest::new([ls, path], Duration::from_secs(2), 4096).expect("req");
            let result = backend
                .exec(&handle, &request, &lease, &live)
                .expect("exec");
            assert_ne!(
                result.exit().code(),
                Some(0),
                "{path} must be unreachable in the guest"
            );
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
        let request =
            SandboxExecRequest::new([ls, &home], Duration::from_secs(2), 4096).expect("home");
        let result = backend
            .exec(&handle, &request, &lease, &live)
            .expect("home exec");
        assert_ne!(
            result.exit().code(),
            Some(0),
            "host home must be unreachable"
        );
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn resource_limits_are_applied_in_oci_or_exec_fails_closed() {
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::Gvisor)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .cpu_millis(2_000)
            .memory_mb(128)
            .pids(16)
            .build()
            .expect("spec");
        let plan = GvisorPlan::from_spec(&spec).expect("plan");
        assert_eq!(plan.cpu_millis(), 2_000);
        assert_eq!(plan.memory_mb(), 128);
        assert_eq!(plan.pids(), 16);
        let spec_json = write_oci_spec(
            Path::new("/tmp/rapidlm-gvisor-test-rootfs"),
            &plan.binds,
            &["/bin/true"],
            plan.guest_cwd(),
            plan.cpu_millis(),
            plan.memory_mb(),
            plan.pids(),
        )
        .expect("oci");
        assert!(spec_json.contains("RLIMIT_CPU"));
        assert!(spec_json.contains("RLIMIT_NPROC"));
        assert!(spec_json.contains("RLIMIT_AS"));
        assert!(spec_json.contains("\"hard\":2"));
        assert!(spec_json.contains("\"hard\":16"));
        assert!(!spec_json.contains("\"source\":\"/\""));
        assert!(!spec_json.contains("docker.sock"));

        let backend = GvisorBackend::new();
        let live = CancellationToken::new();
        if live_runtime(&backend).is_none() {
            assert_eq!(
                backend
                    .prepare(&spec, &proc_lease(), &live)
                    .expect_err("no runtime"),
                SandboxError::TierUnavailable
            );
        }
    }

    #[test]
    fn network_deny_does_not_use_host_network() {
        let ws = TempWorkspace::new();
        for network in [
            SandboxNetwork::None,
            SandboxNetwork::Allowlist,
            SandboxNetwork::Proxy,
        ] {
            let spec = SandboxSpec::builder(SandboxTier::Gvisor)
                .cwd(cwd())
                .mount(ws.mount("src", MountMode::ReadWrite))
                .network(network)
                .build()
                .expect("spec");
            let plan = GvisorPlan::from_spec(&spec).expect("plan");
            assert!(plan.has_network_none());
            assert!(!plan.shares_host_network());
            assert!(plan.argv().iter().any(|part| part == "--network=none"));
            assert!(!plan.argv().iter().any(|part| is_host_network_flag(part)));
        }

        let backend = GvisorBackend::new();
        let listener = TcpListener::bind("127.0.0.1:0").expect("local endpoint");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let hits = Arc::new(AtomicU32::new(0));
        let hits_thread = hits.clone();
        thread::spawn(move || {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(4) {
                match listener.accept() {
                    Ok(_) => {
                        hits_thread.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        TcpStream::connect_timeout(&addr, Duration::from_secs(1)).expect("endpoint live");
        let deadline = Instant::now() + Duration::from_secs(1);
        while hits.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "host probe must hit endpoint"
        );

        let live = CancellationToken::new();
        let Some(_) = live_runtime(&backend) else {
            let health = backend.health(&live).expect("health");
            assert!(!health.is_available());
            thread::sleep(Duration::from_millis(50));
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "plan isolation must not open the host endpoint"
            );
            return;
        };

        let lease = proc_lease();
        let handle = backend
            .prepare(&gvisor_spec(&ws), &lease, &live)
            .expect("prepare");
        let sh = host_tool(&["/bin/sh", "/usr/bin/sh"]).expect("sh");
        let script = format!(
            "exec 3<>/dev/tcp/{}/{} || true; python3 -c 'import socket; socket.create_connection((\"{}\", {}), 1)' 2>/dev/null || true; nc -w 1 {} {} 2>/dev/null || true",
            addr.ip(),
            addr.port(),
            addr.ip(),
            addr.port(),
            addr.ip(),
            addr.port()
        );
        let request = SandboxExecRequest::new([sh, "-c", &script], Duration::from_secs(2), 4096)
            .expect("req");
        let result = backend
            .exec(&handle, &request, &lease, &live)
            .expect("exec");
        let _ = result;
        thread::sleep(Duration::from_millis(80));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "gvisor with network none must not reach the local endpoint"
        );
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn timeout_and_cancellation_are_explicit() {
        let backend = GvisorBackend::new();
        let live = CancellationToken::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            backend.supports(&cancel).expect_err("supports"),
            SandboxError::Cancelled
        );
        assert_eq!(
            backend.health(&cancel).expect_err("health"),
            SandboxError::Cancelled
        );
        assert_eq!(SandboxError::Cancelled.error_code(), None);

        let ws = TempWorkspace::new();
        let spec = gvisor_spec(&ws);
        let lease = proc_lease();
        let Some(_) = live_runtime(&backend) else {
            let err = backend
                .prepare(&spec, &lease, &live)
                .expect_err("prepare without runtime");
            assert_eq!(err, SandboxError::TierUnavailable);
            return;
        };
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");
        let sleep = host_tool(&["/bin/sleep", "/usr/bin/sleep"]).expect("sleep");
        let timed =
            SandboxExecRequest::new([sleep, "5"], Duration::from_millis(80), 1024).expect("timed");
        let result = backend
            .exec(&handle, &timed, &lease, &live)
            .expect("timeout");
        assert_eq!(result.exit().reason(), SandboxExitReason::TimedOut);
        assert!(result.exit().timed_out());

        let mid = CancellationToken::new();
        let sleeper =
            SandboxExecRequest::new([sleep, "5"], Duration::from_secs(2), 1024).expect("sleep");
        let result = thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(30));
                mid.cancel();
            });
            backend
                .exec(&handle, &sleeper, &lease, &mid)
                .expect("mid-cancel")
        });
        assert_eq!(result.exit().reason(), SandboxExitReason::Cancelled);
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn exec_cannot_widen_timeout_or_output_when_prepared() {
        let backend = GvisorBackend::new();
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::Gvisor)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .timeout(Duration::from_secs(2))
            .output_limit(1024)
            .build()
            .expect("spec");
        let lease = proc_lease();
        let live = CancellationToken::new();
        let Some(_) = live_runtime(&backend) else {
            assert_eq!(
                backend
                    .prepare(&spec, &lease, &live)
                    .expect_err("no runtime"),
                SandboxError::TierUnavailable
            );
            let heavy = SandboxSpec::builder(SandboxTier::Gvisor)
                .cwd(cwd())
                .mount(ws.mount("src", MountMode::ReadWrite))
                .memory_mb(u32::MAX)
                .build();
            assert_eq!(heavy.expect_err("heavy"), SandboxError::ResourceLimit);
            return;
        };
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");
        let true_bin = host_tool(&["/usr/bin/true", "/bin/true"]).expect("true");
        let wide = SandboxExecRequest::new([true_bin], Duration::from_secs(5), 1024).expect("wide");
        assert_eq!(
            backend
                .exec(&handle, &wide, &lease, &live)
                .expect_err("timeout"),
            SandboxError::TimeoutInvalid
        );
        let wide_out =
            SandboxExecRequest::new([true_bin], Duration::from_secs(1), 4096).expect("out");
        assert_eq!(
            backend
                .exec(&handle, &wide_out, &lease, &live)
                .expect_err("output"),
            SandboxError::OutputLimitInvalid
        );
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn doctor_lists_gvisor_without_host_warning() {
        let mut mgr = SandboxManager::new();
        mgr.register(Box::new(GvisorBackend::new()))
            .expect("register");
        let reports = mgr.doctor(&CancellationToken::new()).expect("doctor");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].tier(), SandboxTier::Gvisor);
        assert_eq!(reports[0].isolation(), IsolationStrength::SyscallMediation);
        assert!(reports[0].warning().is_none());
        assert!(!format!("{:?}", reports[0]).contains(CANARY));
        if !cfg!(target_os = "linux") {
            assert!(!reports[0].health().is_available());
            assert_eq!(
                reports[0].health().reason(),
                Some(HealthReason::PlatformUnsupported)
            );
        }
    }

    #[test]
    fn cwd_outside_declared_mount_is_rejected() {
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::Gvisor)
            .cwd(RepoPath::parse("docs").expect("docs"))
            .mount(ws.mount("src", MountMode::ReadWrite))
            .build()
            .expect("spec");
        assert_eq!(
            GvisorPlan::from_spec(&spec).expect_err("cwd"),
            SandboxError::ForbiddenMount
        );
    }

    #[test]
    fn cancelled_prepare_is_not_a_clean_pass() {
        let backend = GvisorBackend::new();
        let ws = TempWorkspace::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            backend
                .prepare(&gvisor_spec(&ws), &proc_lease(), &cancel)
                .expect_err("prepare"),
            SandboxError::Cancelled
        );
    }

    #[test]
    fn trait_supports_rejects_non_gvisor_specs() {
        let backend = GvisorBackend::new();
        let ws = TempWorkspace::new();
        let container = SandboxSpec::builder(SandboxTier::Container)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .build()
            .expect("container");
        assert_eq!(
            SandboxBackend::supports(&backend, &container).expect_err("container"),
            SandboxError::TierUnavailable
        );
        let ok = gvisor_spec(&ws);
        SandboxBackend::supports(&backend, &ok).expect("gvisor spec");
    }
}
