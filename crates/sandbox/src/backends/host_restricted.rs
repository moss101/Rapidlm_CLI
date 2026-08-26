//! Host-restricted sandbox backend.
//!
//! Runs argv-first children through process-supervisor constraints (dedicated
//! cwd, allowlisted env, timeout, output cap, process group, rlimits) plus
//! brokered workspace/path and network helpers. Isolation is process-policy
//! only and is never a strong malicious-code boundary.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use capability_broker::{CancellationToken, CanonicalHostPath, Capability, CapabilityLease};
use protocol::{LeaseId, RepoPath, SandboxTier};

use crate::backend::{
    BackendHealth, IsolationStrength, MountCapability, MountMode, NetworkCapability,
    ResourceCapability, ResourceUsage, SandboxBackend, SandboxCapabilities, SandboxError,
    SandboxExecRequest, SandboxExecResult, SandboxExit, SandboxExitReason, SandboxHandle,
    SandboxId, SandboxMount, SandboxNetwork, SandboxSpec,
};

/// Maximum prepared host-restricted sandboxes retained by one backend.
pub const MAX_LIVE_HOST_SANDBOXES: usize = 64;

/// Health/doctor identity. Not a container/runtime version.
const HOST_VERSION: &str = "host-restricted";

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const CANCEL_STRIDE: u32 = 8;
const TERM_GRACE: Duration = Duration::from_millis(80);
const KILL_WAIT: Duration = Duration::from_secs(2);

/// Env names that would inherit or install a host proxy. Always denied.
const PROXY_ENV_NAMES: &[&str] = &[
    "ALL_PROXY",
    "FTP_PROXY",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "SOCKS5_PROXY",
    "SOCKS_PROXY",
];

/// Lexical and resolved prefixes that must never be brokered as a host source.
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

const KILL_PROGRAMS: &[&str] = &["/bin/kill", "/usr/bin/kill"];
const POSIX_SH: &[&str] = &["/bin/sh", "/usr/bin/sh"];
const PS_PROGRAMS: &[&str] = &["/bin/ps", "/usr/bin/ps"];
const PGREP_PROGRAMS: &[&str] = &["/usr/bin/pgrep", "/bin/pgrep"];

/// Fixed helper: set RLIMIT_CPU then exec the already-validated argv.
/// Integers and program argv are positional; nothing is interpolated.
const APPLY_CPU_RLIMIT: &str = r#"ulimit -t "$1" || exit 125
shift
exec "$@""#;

/// Network helper attached to a prepared host-restricted sandbox.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum HostNetworkHelper {
    /// Default isolated mode: no host network grant and no proxy env.
    Isolated,
    /// Brokered allowlist helper. Not a kernel/netns boundary.
    Allowlist,
}

/// Planned containment for one prepared handle. Secrets stay as a count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostRestrictedPlan {
    isolation: IsolationStrength,
    network: HostNetworkHelper,
    cwd_host: CanonicalHostPath,
    mounts: Vec<SandboxMount>,
    env_allowlist: Vec<String>,
    timeout: Duration,
    output_limit: u64,
    cpu_millis: u32,
    memory_mb: u32,
    pids: u32,
    secret_count: usize,
}

/// Process-policy backend. Never advertises container/gVisor/microVM isolation.
pub struct HostRestrictedBackend {
    caps: SandboxCapabilities,
    sessions: Mutex<HashMap<SandboxId, PreparedSession>>,
}

struct PreparedSession {
    handle: SandboxHandle,
    lease_id: LeaseId,
    plan: HostRestrictedPlan,
}

impl HostNetworkHelper {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Isolated => "none",
            Self::Allowlist => "allowlist",
        }
    }

    pub const fn from_network(network: SandboxNetwork) -> Result<Self, SandboxError> {
        match network {
            SandboxNetwork::None => Ok(Self::Isolated),
            SandboxNetwork::Allowlist => Ok(Self::Allowlist),
            SandboxNetwork::Proxy => Err(SandboxError::UnsupportedNetwork),
        }
    }
}

impl HostRestrictedPlan {
    pub const fn isolation(&self) -> IsolationStrength {
        self.isolation
    }

    pub const fn network(&self) -> HostNetworkHelper {
        self.network
    }

    pub fn cwd_host(&self) -> &CanonicalHostPath {
        &self.cwd_host
    }

    pub fn mounts(&self) -> &[SandboxMount] {
        &self.mounts
    }

    pub fn env_allowlist(&self) -> &[String] {
        &self.env_allowlist
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

    pub const fn is_strong_isolation(&self) -> bool {
        self.isolation.is_strong_isolation()
    }

    pub const fn doctor_warning(&self) -> Option<&'static str> {
        self.isolation.doctor_warning()
    }
}

impl HostRestrictedBackend {
    pub fn new() -> Self {
        let caps = SandboxCapabilities::new(
            SandboxTier::HostRestricted,
            NetworkCapability::allowlist(),
            MountCapability::workspace_temp(),
            ResourceCapability::bounded(),
        )
        .expect("host-restricted is a known sandbox tier");
        Self {
            caps,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Prepared plan for `handle`, if this backend owns it.
    pub fn plan(&self, handle: &SandboxHandle) -> Result<HostRestrictedPlan, SandboxError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SandboxError::HealthFailed)?;
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

impl Default for HostRestrictedBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxBackend for HostRestrictedBackend {
    fn capabilities(&self) -> SandboxCapabilities {
        self.caps
    }

    fn health(&self, cancel: &CancellationToken) -> Result<BackendHealth, SandboxError> {
        check_cancel(cancel)?;
        BackendHealth::available(Some(HOST_VERSION))
    }

    fn prepare(
        &self,
        spec: &SandboxSpec,
        lease: &CapabilityLease,
        cancel: &CancellationToken,
    ) -> Result<SandboxHandle, SandboxError> {
        check_cancel(cancel)?;
        require_proc_lease(lease)?;
        if spec.tier() != SandboxTier::HostRestricted {
            return Err(SandboxError::TierUnavailable);
        }
        self.supports(spec)?;
        if spec.image().is_some() {
            return Err(SandboxError::InvalidSpec);
        }
        let plan = plan_spec(spec)?;
        check_cancel(cancel)?;
        let handle = SandboxHandle::new(SandboxTier::HostRestricted, lease.lease_id())?;
        let mut sessions = self.lock_sessions()?;
        if sessions.len() >= MAX_LIVE_HOST_SANDBOXES {
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
        if handle.tier() != SandboxTier::HostRestricted {
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
        run_supervised(&plan, request, cancel)
    }

    fn destroy(
        &self,
        handle: &SandboxHandle,
        cancel: &CancellationToken,
    ) -> Result<(), SandboxError> {
        check_cancel(cancel)?;
        if handle.tier() != SandboxTier::HostRestricted {
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

fn plan_spec(spec: &SandboxSpec) -> Result<HostRestrictedPlan, SandboxError> {
    let network = HostNetworkHelper::from_network(spec.network())?;
    for name in spec.env_allowlist() {
        if is_proxy_env_name(name) {
            return Err(SandboxError::UnsupportedNetwork);
        }
    }
    for mount in spec.mounts() {
        validate_mount(mount)?;
    }
    let cwd_host = resolve_cwd(spec.cwd(), spec.mounts())?;
    require_cpu_rlimit(spec.cpu_millis())?;
    require_group_accounting()?;
    Ok(HostRestrictedPlan {
        isolation: IsolationStrength::ProcessPolicy,
        network,
        cwd_host,
        mounts: spec.mounts().to_vec(),
        env_allowlist: spec.env_allowlist().to_vec(),
        timeout: spec.timeout(),
        output_limit: spec.output_limit(),
        cpu_millis: spec.cpu_millis(),
        memory_mb: spec.memory_mb(),
        pids: spec.pids(),
        secret_count: spec.secrets().len(),
    })
}

fn validate_mount(mount: &SandboxMount) -> Result<(), SandboxError> {
    if is_forbidden_repo_target(mount.target().as_str()) {
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
            if is_forbidden_host_source(source.as_str()) {
                return Err(SandboxError::ForbiddenMount);
            }
            let resolved = resolve_existing_dir(Path::new(source.as_str()))?;
            if is_forbidden_host_source(resolved.as_str()) {
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
        if let Some(prefix_len) = target_covers(mount.target(), cwd) {
            if best.is_none_or(|(_, len)| prefix_len > len) {
                best = Some((mount, prefix_len));
            }
        }
    }
    let (mount, _) = best.ok_or(SandboxError::ForbiddenMount)?;
    let source = mount.source().ok_or(SandboxError::InvalidSpec)?;
    if is_forbidden_host_source(source.as_str()) {
        return Err(SandboxError::ForbiddenMount);
    }
    let resolved_source = resolve_existing_dir(Path::new(source.as_str()))?;
    if is_forbidden_host_source(resolved_source.as_str()) {
        return Err(SandboxError::ForbiddenMount);
    }
    let joined = join_host(&resolved_source, mount.target(), cwd)?;
    let resolved_cwd = resolve_existing_dir(Path::new(joined.as_str()))?;
    if is_forbidden_host_source(resolved_cwd.as_str()) {
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

fn resolve_existing_dir(path: &Path) -> Result<CanonicalHostPath, SandboxError> {
    let canon = fs::canonicalize(path).map_err(|_| SandboxError::ForbiddenMount)?;
    let meta = fs::metadata(&canon).map_err(|_| SandboxError::ForbiddenMount)?;
    if !meta.is_dir() {
        return Err(SandboxError::ForbiddenMount);
    }
    let text = canon.to_str().ok_or(SandboxError::ForbiddenMount)?;
    CanonicalHostPath::from_resolved(text).map_err(|_| SandboxError::ForbiddenMount)
}

fn resolve_existing_file(path: &str) -> Result<CanonicalHostPath, SandboxError> {
    let requested =
        CanonicalHostPath::from_resolved(path).map_err(|_| SandboxError::ForbiddenMount)?;
    if is_forbidden_host_source(requested.as_str()) {
        return Err(SandboxError::ForbiddenMount);
    }
    let canon = fs::canonicalize(requested.as_str()).map_err(|_| SandboxError::ForbiddenMount)?;
    if !canon.is_file() {
        return Err(SandboxError::ForbiddenMount);
    }
    let text = canon.to_str().ok_or(SandboxError::ForbiddenMount)?;
    let resolved =
        CanonicalHostPath::from_resolved(text).map_err(|_| SandboxError::ForbiddenMount)?;
    if is_forbidden_host_source(resolved.as_str()) {
        return Err(SandboxError::ForbiddenMount);
    }
    Ok(resolved)
}

fn assert_cwd_stable(plan: &HostRestrictedPlan) -> Result<(), SandboxError> {
    let now = resolve_existing_dir(Path::new(plan.cwd_host.as_str()))?;
    if now.as_str() != plan.cwd_host.as_str() || is_forbidden_host_source(now.as_str()) {
        return Err(SandboxError::ForbiddenMount);
    }
    Ok(())
}

fn path_is_within(parent: &str, child: &str) -> bool {
    let parent = parent.trim_end_matches('/');
    let child = child.trim_end_matches('/');
    child == parent
        || child.starts_with(parent) && child.as_bytes().get(parent.len()) == Some(&b'/')
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
    if parts
        .windows(2)
        .any(|pair| pair == ["library", "keychains"] || pair == ["com.apple.launchd", "listeners"])
    {
        return true;
    }
    false
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

fn is_forbidden_repo_target(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    is_docker_socket(&lower)
}

fn is_docker_socket(path: &str) -> bool {
    let trimmed = path.trim_end_matches('/');
    trimmed == "docker.sock" || trimmed.ends_with("/docker.sock")
}

fn is_proxy_env_name(name: &str) -> bool {
    PROXY_ENV_NAMES
        .iter()
        .any(|blocked| name.eq_ignore_ascii_case(blocked))
}

fn cpu_limit_seconds(cpu_millis: u32) -> u64 {
    u64::from(cpu_millis.div_ceil(1_000)).max(1)
}

fn first_existing(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|path| Path::new(path).is_file())
}

fn require_cpu_rlimit(cpu_millis: u32) -> Result<(), SandboxError> {
    let sh = first_existing(POSIX_SH).ok_or(SandboxError::ResourceLimit)?;
    let secs = cpu_limit_seconds(cpu_millis).to_string();
    let status = Command::new(sh)
        .args(["-c", "ulimit -t \"$1\"", "host-restricted", &secs])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .status()
        .map_err(|_| SandboxError::ResourceLimit)?;
    if status.success() {
        Ok(())
    } else {
        Err(SandboxError::ResourceLimit)
    }
}

fn require_group_accounting() -> Result<(), SandboxError> {
    if first_existing(PS_PROGRAMS).is_some() || first_existing(PGREP_PROGRAMS).is_some() {
        Ok(())
    } else {
        Err(SandboxError::ResourceLimit)
    }
}

fn run_supervised(
    plan: &HostRestrictedPlan,
    request: &SandboxExecRequest,
    cancel: &CancellationToken,
) -> Result<SandboxExecResult, SandboxError> {
    check_cancel(cancel)?;
    assert_cwd_stable(plan)?;
    let argv = request.argv();
    let program = resolve_existing_file(&argv[0])?;
    let mut command = spawn_command(plan, program.as_str(), &argv[1..])?;
    command.current_dir(plan.cwd_host.as_str());
    command.env_clear();
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    isolate_process_group(&mut command);
    let mut child = command.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            SandboxError::ForbiddenMount
        } else {
            SandboxError::HealthFailed
        }
    })?;
    let started = Instant::now();
    let outcome = wait_child(
        &mut child,
        request.timeout(),
        request.output_limit(),
        plan,
        cancel,
    );
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match outcome {
        WaitOutcome::Finished {
            code,
            signal,
            output_bytes,
            usage,
        } => Ok(SandboxExecResult::new(SandboxExit::new(
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
                output_bytes,
            ),
        ))),
        WaitOutcome::TimedOut {
            output_bytes,
            usage,
        } => Ok(SandboxExecResult::new(SandboxExit::new(
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
                output_bytes,
            ),
        ))),
        WaitOutcome::Cancelled {
            output_bytes,
            usage,
        } => Ok(SandboxExecResult::new(SandboxExit::new(
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
                output_bytes,
            ),
        ))),
        WaitOutcome::Oom {
            output_bytes,
            usage,
        } => Ok(SandboxExecResult::new(SandboxExit::new(
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
                output_bytes,
            ),
        ))),
        WaitOutcome::PidsExceeded {
            output_bytes,
            usage,
        } => Ok(SandboxExecResult::new(SandboxExit::new(
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
                output_bytes,
            ),
        ))),
        WaitOutcome::Failed => Err(SandboxError::HealthFailed),
    }
}

fn spawn_command(
    plan: &HostRestrictedPlan,
    program: &str,
    args: &[String],
) -> Result<Command, SandboxError> {
    let sh = first_existing(POSIX_SH).ok_or(SandboxError::ResourceLimit)?;
    let secs = cpu_limit_seconds(plan.cpu_millis).to_string();
    let mut command = Command::new(sh);
    command.arg("-c");
    command.arg(APPLY_CPU_RLIMIT);
    command.arg("host-restricted");
    command.arg(secs);
    command.arg(program);
    command.args(args);
    Ok(command)
}

enum WaitOutcome {
    Finished {
        code: Option<i32>,
        signal: Option<i32>,
        output_bytes: u64,
        usage: GroupUsage,
    },
    TimedOut {
        output_bytes: u64,
        usage: GroupUsage,
    },
    Cancelled {
        output_bytes: u64,
        usage: GroupUsage,
    },
    Oom {
        output_bytes: u64,
        usage: GroupUsage,
    },
    PidsExceeded {
        output_bytes: u64,
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
    plan: &HostRestrictedPlan,
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
    let pgid = child.id();
    let status = loop {
        if polls.is_multiple_of(CANCEL_STRIDE) && cancel.is_cancelled() {
            terminate_process_group(child);
            let output_bytes = join_output(stdout_thread, stderr_thread);
            return WaitOutcome::Cancelled {
                output_bytes,
                usage,
            };
        }
        if started.elapsed() >= timeout {
            terminate_process_group(child);
            let output_bytes = join_output(stdout_thread, stderr_thread);
            return WaitOutcome::TimedOut {
                output_bytes,
                usage,
            };
        }
        if let Some(sample) = sample_process_group(pgid) {
            usage.pids_peak = usage.pids_peak.max(sample.0);
            usage.memory_peak_mb = usage.memory_peak_mb.max(sample.1);
            if sample.1 > u64::from(plan.memory_mb) {
                terminate_process_group(child);
                let output_bytes = join_output(stdout_thread, stderr_thread);
                return WaitOutcome::Oom {
                    output_bytes,
                    usage,
                };
            }
            if sample.0 > plan.pids {
                terminate_process_group(child);
                let output_bytes = join_output(stdout_thread, stderr_thread);
                return WaitOutcome::PidsExceeded {
                    output_bytes,
                    usage,
                };
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(_) => {
                terminate_process_group(child);
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return WaitOutcome::Failed;
            }
        }
        polls = polls.saturating_add(1);
    };
    let output_bytes = join_output(stdout_thread, stderr_thread);
    WaitOutcome::Finished {
        code: status.code(),
        signal: exit_signal(&status),
        output_bytes,
        usage,
    }
}

fn join_output(
    stdout: thread::JoinHandle<(Vec<u8>, bool)>,
    stderr: thread::JoinHandle<(Vec<u8>, bool)>,
) -> u64 {
    let stdout_len = stdout.join().map(|(buf, _)| buf.len()).unwrap_or(0);
    let stderr_len = stderr.join().map(|(buf, _)| buf.len()).unwrap_or(0);
    u64::try_from(stdout_len.saturating_add(stderr_len)).unwrap_or(u64::MAX)
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

#[derive(Clone, Copy)]
enum GroupSignal {
    Term,
    Kill,
}

fn signal_group(pgid: u32, kind: GroupSignal) -> Result<(), SandboxError> {
    if pgid < 2 {
        return Err(SandboxError::HealthFailed);
    }
    platform_signal_group(pgid, kind)
}

#[cfg(unix)]
fn platform_signal_group(pgid: u32, kind: GroupSignal) -> Result<(), SandboxError> {
    let flag = match kind {
        GroupSignal::Term => "-TERM",
        GroupSignal::Kill => "-KILL",
    };
    let target = format!("-{pgid}");
    let program = first_existing(KILL_PROGRAMS).ok_or(SandboxError::HealthFailed)?;
    let status = Command::new(program)
        .args([flag, target.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .status()
        .map_err(|_| SandboxError::HealthFailed)?;
    if status.success() || matches!(status.code(), Some(1) | Some(128)) {
        Ok(())
    } else {
        Err(SandboxError::HealthFailed)
    }
}

#[cfg(windows)]
fn platform_signal_group(pgid: u32, kind: GroupSignal) -> Result<(), SandboxError> {
    const TASKKILL: &str = r"C:\Windows\System32\taskkill.exe";
    let pid = pgid.to_string();
    let mut command = Command::new(TASKKILL);
    command.args(["/PID", &pid, "/T"]);
    if matches!(kind, GroupSignal::Kill) {
        command.arg("/F");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear();
    let status = command.status().map_err(|_| SandboxError::HealthFailed)?;
    if status.success() || matches!(status.code(), Some(1) | Some(128)) {
        Ok(())
    } else {
        Err(SandboxError::HealthFailed)
    }
}

#[cfg(not(any(unix, windows)))]
fn platform_signal_group(_pgid: u32, _kind: GroupSignal) -> Result<(), SandboxError> {
    Err(SandboxError::HealthFailed)
}

/// `(pids, memory_mb)` for the dedicated process group. `None` if unreadable.
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
        rss_kb = rss_kb.saturating_add(pid_rss_kb(*pid).unwrap_or(0));
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
        if let Ok(pid) = line.trim().parse::<u32>() {
            if pid >= 2 {
                pids.push(pid);
            }
        }
    }
    if pids.is_empty() {
        None
    } else {
        Some(pids)
    }
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
    if pids.is_empty() {
        None
    } else {
        Some(pids)
    }
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
    use std::path::PathBuf;
    use std::time::Instant;

    use capability_broker::{
        evaluate, issue, request_approval, ActionRequest, ApprovalChoice, ApprovalResolution,
        ApprovalScopeId, CanonicalAction, FilesystemScope, LeaseIssuer, PolicyDocument,
        PolicySource, PolicyStack, PrincipalRef, ProcessScope, ResourceDescriptor, SecretHandle,
    };
    use protocol::{ErrorCode, SessionId};

    use crate::backend::SandboxManager;

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-7c1e9b";

    fn host_tool(candidates: &[&'static str]) -> &'static str {
        for path in candidates {
            if Path::new(path).is_file() {
                return path;
            }
        }
        panic!("no host tool among {candidates:?}");
    }

    fn true_bin() -> &'static str {
        host_tool(&["/usr/bin/true", "/bin/true", "/bin/echo"])
    }

    fn sleep_bin() -> &'static str {
        host_tool(&["/bin/sleep", "/usr/bin/sleep"])
    }

    fn sh_bin() -> &'static str {
        host_tool(&["/bin/sh", "/usr/bin/sh"])
    }

    struct TempWorkspace {
        path: PathBuf,
        host: CanonicalHostPath,
    }

    impl TempWorkspace {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("rapidlm-host-sbx-{}", protocol::RuntimeId::new()));
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

    fn host_spec(ws: &TempWorkspace) -> SandboxSpec {
        SandboxSpec::builder(SandboxTier::HostRestricted)
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

    #[cfg(unix)]
    fn pid_alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    fn wait_pid_file(path: &Path, budget: Duration) -> u32 {
        let deadline = Instant::now() + budget;
        loop {
            if let Ok(text) = fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    if pid >= 2 {
                        return pid;
                    }
                }
            }
            if Instant::now() >= deadline {
                panic!("pid file {} was not written", path.display());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn isolation_is_process_policy_and_not_strong() {
        let backend = HostRestrictedBackend::new();
        let caps = backend.capabilities();
        assert_eq!(caps.tier(), SandboxTier::HostRestricted);
        assert_eq!(caps.isolation(), IsolationStrength::ProcessPolicy);
        assert!(!caps.isolation().is_strong_isolation());
        assert_eq!(
            caps.isolation().doctor_warning(),
            Some("host-restricted is not a strong malicious-code boundary")
        );
        assert!(caps.network().allowlist_supported());
        assert!(!caps.network().proxy_supported());
    }

    #[test]
    fn doctor_and_health_warn_that_isolation_is_not_strong() {
        let mut mgr = SandboxManager::new();
        mgr.register(Box::new(HostRestrictedBackend::new()))
            .expect("register");
        let live = CancellationToken::new();
        let reports = mgr.doctor(&live).expect("doctor");
        assert_eq!(reports.len(), 1);
        let row = &reports[0];
        assert_eq!(row.tier(), SandboxTier::HostRestricted);
        assert_eq!(row.isolation(), IsolationStrength::ProcessPolicy);
        assert!(row.health().is_available());
        assert_eq!(row.health().version(), Some(HOST_VERSION));
        assert!(row.warning().unwrap().contains("not a strong"));
        assert!(!row.warning().unwrap().contains(CANARY));
    }

    #[test]
    fn cannot_be_selected_when_spec_requires_container_or_gvisor() {
        let mut mgr = SandboxManager::new();
        mgr.register(Box::new(HostRestrictedBackend::new()))
            .expect("register");
        let live = CancellationToken::new();
        let ws = TempWorkspace::new();
        let container = SandboxSpec::builder(SandboxTier::Container)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .build()
            .expect("container");
        let err = match mgr.select(&container, &live) {
            Ok(_) => panic!("container must not select host-restricted"),
            Err(err) => err,
        };
        assert_eq!(err, SandboxError::TierUnavailable);
        assert_eq!(err.error_code(), Some(ErrorCode::SandboxTierUnavailable));

        let gvisor = SandboxSpec::builder(SandboxTier::Gvisor)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .build()
            .expect("gvisor");
        let err = match mgr.select(&gvisor, &live) {
            Ok(_) => panic!("gvisor must not select host-restricted"),
            Err(err) => err,
        };
        assert_eq!(err, SandboxError::TierUnavailable);
    }

    #[test]
    fn prepare_refuses_stronger_tier_even_when_called_directly() {
        let backend = HostRestrictedBackend::new();
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::Container)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .build()
            .expect("spec");
        let err = backend
            .prepare(&spec, &proc_lease(), &CancellationToken::new())
            .expect_err("direct");
        assert_eq!(err, SandboxError::TierUnavailable);
        assert!(!err.as_str().contains(CANARY));
    }

    #[test]
    fn proxy_network_and_proxy_env_are_rejected() {
        let backend = HostRestrictedBackend::new();
        let ws = TempWorkspace::new();
        let lease = proc_lease();
        let live = CancellationToken::new();
        let proxy = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .network(SandboxNetwork::Proxy)
            .build()
            .expect("proxy");
        assert_eq!(
            backend.supports(&proxy).expect_err("proxy"),
            SandboxError::UnsupportedNetwork
        );

        let env = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .env_allowlist(["PATH", "http_proxy"])
            .build()
            .expect("env");
        assert_eq!(
            backend.prepare(&env, &lease, &live).expect_err("proxy env"),
            SandboxError::UnsupportedNetwork
        );
    }

    #[test]
    fn home_and_docker_mounts_cannot_bypass_path_policy() {
        let backend = HostRestrictedBackend::new();
        let lease = proc_lease();
        let live = CancellationToken::new();
        let home = CanonicalHostPath::from_resolved("/Users/canary-home").expect("home");
        let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(SandboxMount::bind(home, cwd(), MountMode::ReadWrite).expect("bind"))
            .build()
            .expect("spec");
        assert_eq!(
            backend.prepare(&spec, &lease, &live).expect_err("home"),
            SandboxError::ForbiddenMount
        );

        let ssh = CanonicalHostPath::from_resolved("/Users/canary-home/.ssh").expect("ssh");
        let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(SandboxMount::bind(ssh, cwd(), MountMode::ReadOnly).expect("ssh"))
            .build()
            .expect("spec");
        assert_eq!(
            backend.prepare(&spec, &lease, &live).expect_err("ssh"),
            SandboxError::ForbiddenMount
        );
        assert_eq!(
            SandboxError::ForbiddenMount.error_code(),
            Some(ErrorCode::PolicyDenied)
        );
        assert!(!SandboxError::ForbiddenMount
            .as_str()
            .contains("canary-home"));
    }

    #[test]
    fn resolved_sensitive_prefixes_are_denied() {
        let backend = HostRestrictedBackend::new();
        let lease = proc_lease();
        let live = CancellationToken::new();
        for path in [
            "/etc",
            "/private/etc",
            "/private/var/root",
            "/root",
            "/Users/canary-home",
        ] {
            let host = CanonicalHostPath::from_resolved(path).expect("path");
            let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
                .cwd(cwd())
                .mount(SandboxMount::bind(host, cwd(), MountMode::ReadOnly).expect("bind"))
                .build()
                .expect("spec");
            assert_eq!(
                backend.prepare(&spec, &lease, &live).expect_err(path),
                SandboxError::ForbiddenMount,
                "{path}"
            );
        }
        assert!(is_forbidden_host_source("/private/etc/passwd"));
        assert!(is_forbidden_host_source("/private/var/root/.ssh"));
        assert!(is_forbidden_host_source("/root"));
        assert!(!SandboxError::ForbiddenMount.as_str().contains(CANARY));
    }

    #[cfg(unix)]
    #[test]
    fn intermediate_symlink_into_private_etc_or_home_is_forbidden() {
        let backend = HostRestrictedBackend::new();
        let lease = proc_lease();
        let live = CancellationToken::new();

        let etc_ws = TempWorkspace::new();
        let via_etc = etc_ws.path.join("via");
        std::os::unix::fs::symlink("/private", &via_etc).expect("symlink private");
        let etc_escape = via_etc.join("etc");
        assert!(etc_escape.is_dir(), "macOS /private/etc must exist");
        let etc_host =
            CanonicalHostPath::from_resolved(etc_escape.to_str().expect("utf8")).expect("etc host");
        assert!(
            !is_forbidden_host_source(etc_host.as_str()),
            "lexical path must look like a temp workspace, not /etc"
        );
        let etc_spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(SandboxMount::bind(etc_host, cwd(), MountMode::ReadWrite).expect("bind"))
            .build()
            .expect("etc spec");
        assert_eq!(
            backend
                .prepare(&etc_spec, &lease, &live)
                .expect_err("private/etc"),
            SandboxError::ForbiddenMount
        );

        let home = std::env::var("HOME").expect("HOME");
        let home_name = Path::new(&home)
            .file_name()
            .and_then(|name| name.to_str())
            .expect("home name");
        let home_ws = TempWorkspace::new();
        let via_users = home_ws.path.join("via");
        std::os::unix::fs::symlink("/Users", &via_users).expect("symlink users");
        let home_escape = via_users.join(home_name);
        assert!(home_escape.is_dir(), "HOME must exist through symlink");
        let home_host = CanonicalHostPath::from_resolved(home_escape.to_str().expect("utf8"))
            .expect("home host");
        let home_spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(SandboxMount::bind(home_host, cwd(), MountMode::ReadWrite).expect("bind"))
            .build()
            .expect("home spec");
        assert_eq!(
            backend
                .prepare(&home_spec, &lease, &live)
                .expect_err("home"),
            SandboxError::ForbiddenMount
        );

        let cwd_ws = TempWorkspace::new();
        std::os::unix::fs::symlink("/private", cwd_ws.path.join("escape")).expect("cwd symlink");
        let cwd_spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(RepoPath::parse("src/escape/etc").expect("cwd"))
            .mount(cwd_ws.mount("src", MountMode::ReadWrite))
            .build()
            .expect("cwd spec");
        assert_eq!(
            backend
                .prepare(&cwd_spec, &lease, &live)
                .expect_err("cwd escape"),
            SandboxError::ForbiddenMount
        );
    }

    #[test]
    fn cwd_outside_declared_mount_is_rejected() {
        let backend = HostRestrictedBackend::new();
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(RepoPath::parse("docs").expect("docs"))
            .mount(ws.mount("src", MountMode::ReadWrite))
            .build()
            .expect("spec");
        let err = backend
            .prepare(&spec, &proc_lease(), &CancellationToken::new())
            .expect_err("cwd");
        assert_eq!(err, SandboxError::ForbiddenMount);
    }

    #[test]
    fn image_spec_is_rejected_for_host_restricted() {
        let backend = HostRestrictedBackend::new();
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .image("alpine:latest")
            .build()
            .expect("spec");
        let err = backend
            .prepare(&spec, &proc_lease(), &CancellationToken::new())
            .expect_err("image");
        assert_eq!(err, SandboxError::InvalidSpec);
    }

    #[test]
    fn prepare_exec_destroy_enforces_lease_and_labels_plan() {
        let backend = HostRestrictedBackend::new();
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .network(SandboxNetwork::Allowlist)
            .secret(SecretHandle::parse(CANARY).expect("secret"))
            .build()
            .expect("spec");
        let live = CancellationToken::new();
        assert_eq!(
            backend
                .prepare(&spec, &fs_lease(), &live)
                .expect_err("fs lease"),
            SandboxError::LeaseInvalid
        );

        let lease = proc_lease();
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");
        let plan = backend.plan(&handle).expect("plan");
        assert_eq!(plan.isolation(), IsolationStrength::ProcessPolicy);
        assert!(!plan.is_strong_isolation());
        assert_eq!(plan.network(), HostNetworkHelper::Allowlist);
        assert_eq!(plan.secret_count(), 1);
        assert!(plan.doctor_warning().unwrap().contains("not a strong"));
        let debug = format!("{plan:?}");
        assert!(!debug.contains(CANARY));
        assert!(!debug.contains("PLAINTEXT"));

        let other = proc_lease();
        let request =
            SandboxExecRequest::new([true_bin()], Duration::from_secs(2), 4096).expect("request");
        assert_eq!(
            backend
                .exec(&handle, &request, &other, &live)
                .expect_err("retarget"),
            SandboxError::LeaseInvalid
        );
        let result = backend
            .exec(&handle, &request, &lease, &live)
            .expect("exec");
        assert_eq!(result.exit().reason(), SandboxExitReason::Exited);
        assert_eq!(result.exit().code(), Some(0));
        assert!(!result.exit().timed_out());
        assert!(!result.exit().policy_violation());
        backend.destroy(&handle, &live).expect("destroy");
        assert_eq!(
            backend.plan(&handle).expect_err("gone"),
            SandboxError::UnknownHandle
        );
    }

    #[test]
    fn timeout_and_cancellation_are_explicit_terminal_statuses() {
        let backend = HostRestrictedBackend::new();
        let ws = TempWorkspace::new();
        let spec = host_spec(&ws);
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");

        let timed = SandboxExecRequest::new([sleep_bin(), "5"], Duration::from_millis(80), 1024)
            .expect("timed");
        let result = backend
            .exec(&handle, &timed, &lease, &live)
            .expect("timeout");
        assert_eq!(result.exit().reason(), SandboxExitReason::TimedOut);
        assert!(result.exit().timed_out());
        assert_ne!(result.exit().reason(), SandboxExitReason::Exited);

        let cancel = CancellationToken::new();
        cancel.cancel();
        let request =
            SandboxExecRequest::new([sleep_bin(), "5"], Duration::from_secs(2), 1024).expect("req");
        assert_eq!(
            backend
                .exec(&handle, &request, &lease, &cancel)
                .expect_err("pre-cancel"),
            SandboxError::Cancelled
        );
        assert_eq!(SandboxError::Cancelled.error_code(), None);

        let mid = CancellationToken::new();
        let sleeper = SandboxExecRequest::new([sleep_bin(), "5"], Duration::from_secs(2), 1024)
            .expect("sleep");
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
        assert!(!result.exit().timed_out());
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[cfg(unix)]
    #[test]
    fn grandchild_does_not_survive_timeout_or_cancel() {
        let backend = HostRestrictedBackend::new();
        let ws = TempWorkspace::new();
        let spec = host_spec(&ws);
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");
        let sh = sh_bin();
        let sleep = sleep_bin();
        let script = format!(
            "trap \"\" TERM; {sh} -c 'trap \"\" TERM; while :; do {sleep} 10; done' & echo $! > grandchild.pid; while :; do {sleep} 10; done"
        );
        let request =
            SandboxExecRequest::new([sh, "-c", &script], Duration::from_millis(400), 4096)
                .expect("request");
        let pid_path = ws.path.join("grandchild.pid");
        let result = thread::scope(|scope| {
            scope.spawn(|| {
                let grandchild = wait_pid_file(&pid_path, Duration::from_secs(2));
                assert!(
                    grandchild >= 2 && pid_alive(grandchild),
                    "fixture grandchild must start"
                );
            });
            backend
                .exec(&handle, &request, &lease, &live)
                .expect("timeout")
        });
        assert_eq!(result.exit().reason(), SandboxExitReason::TimedOut);
        let grandchild = fs::read_to_string(&pid_path)
            .expect("pid file")
            .trim()
            .parse::<u32>()
            .expect("pid");
        let deadline = Instant::now() + Duration::from_secs(2);
        while pid_alive(grandchild) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !pid_alive(grandchild),
            "grandchild {grandchild} survived group timeout"
        );
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn exec_cannot_widen_timeout_or_output_and_resource_limits_are_bounded() {
        let backend = HostRestrictedBackend::new();
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .timeout(Duration::from_secs(2))
            .output_limit(1024)
            .memory_mb(64)
            .build()
            .expect("spec");
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");
        let wide =
            SandboxExecRequest::new([true_bin()], Duration::from_secs(5), 1024).expect("wide");
        assert_eq!(
            backend
                .exec(&handle, &wide, &lease, &live)
                .expect_err("timeout"),
            SandboxError::TimeoutInvalid
        );
        let wide_out =
            SandboxExecRequest::new([true_bin()], Duration::from_secs(1), 4096).expect("out");
        assert_eq!(
            backend
                .exec(&handle, &wide_out, &lease, &live)
                .expect_err("output"),
            SandboxError::OutputLimitInvalid
        );

        let heavy = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .memory_mb(u32::MAX)
            .build();
        assert_eq!(heavy.expect_err("heavy"), SandboxError::ResourceLimit);
    }

    #[cfg(unix)]
    #[test]
    fn advertised_pids_bound_is_enforced() {
        let backend = HostRestrictedBackend::new();
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .pids(1)
            .timeout(Duration::from_secs(2))
            .build()
            .expect("spec");
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");
        let plan = backend.plan(&handle).expect("plan");
        assert_eq!(plan.pids(), 1);
        assert_eq!(plan.cpu_millis(), 1_000);
        assert_eq!(plan.memory_mb(), 256);
        let sh = sh_bin();
        let sleep = sleep_bin();
        let script = format!("{sleep} 30 & {sleep} 30 & wait");
        let request = SandboxExecRequest::new([sh, "-c", &script], Duration::from_secs(2), 4096)
            .expect("request");
        let result = backend
            .exec(&handle, &request, &lease, &live)
            .expect("exec");
        assert_eq!(result.exit().reason(), SandboxExitReason::PolicyViolation);
        assert!(result.exit().policy_violation());
        assert!(!result.exit().timed_out());
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn relative_executable_is_rejected() {
        let backend = HostRestrictedBackend::new();
        let ws = TempWorkspace::new();
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = backend
            .prepare(&host_spec(&ws), &lease, &live)
            .expect("prepare");
        let request = SandboxExecRequest::new(["true"], Duration::from_secs(1), 1024).expect("rel");
        assert_eq!(
            backend
                .exec(&handle, &request, &lease, &live)
                .expect_err("relative"),
            SandboxError::ForbiddenMount
        );
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn cancelled_health_is_not_a_clean_pass() {
        let backend = HostRestrictedBackend::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            backend.health(&cancel).expect_err("health"),
            SandboxError::Cancelled
        );
    }
}
