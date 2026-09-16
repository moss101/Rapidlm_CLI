//! Rootless Linux container sandbox backend.
//!
//! Builds bubblewrap argv/config from a typed [`SandboxSpec`]: user/mount/pid/
//! ipc/uts/cgroup namespaces, a seccomp filter, minimal binds, and an explicit
//! network mode. Never opens or bind-mounts a privileged Docker socket.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use capability_broker::{CancellationToken, CanonicalHostPath, Capability, CapabilityLease};
use process_signal::{isolate_process_group, terminate_process_group_default};

use crate::backends::process_sample::{first_existing, sample_process_group};
use protocol::{LeaseId, RepoPath, SandboxTier};

use crate::backend::{
    BackendHealth, HealthReason, IsolationStrength, MountCapability, MountMode, NetworkCapability,
    ResourceCapability, ResourceUsage, SandboxBackend, SandboxCapabilities, SandboxError,
    SandboxExecRequest, SandboxExecResult, SandboxExit, SandboxExitReason, SandboxHandle,
    SandboxId, SandboxMount, SandboxNetwork, SandboxSpec,
};

/// Maximum prepared container sandboxes retained by one backend.
pub const MAX_LIVE_CONTAINER_SANDBOXES: usize = 64;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const CANCEL_STRIDE: u32 = 8;
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Fixed bubblewrap paths. Never Docker, never `$PATH`.
const BWRAP_PROGRAMS: &[&str] = &["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"];
const POSIX_SH: &[&str] = &["/bin/sh", "/usr/bin/sh"];
const TRUE_PROGRAMS: &[&str] = &["/usr/bin/true", "/bin/true"];

/// Host rootfs pieces that may be read-only bind-mounted. Not `/etc`, `/home`, `/Users`.
const SYSTEM_RO_BINDS: &[&str] = &["/usr", "/bin", "/lib", "/lib64", "/sbin"];

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

/// Apply rlimits then exec already-validated argv. Integers are positional.
const APPLY_RLIMITS: &str = r#"ulimit -t "$1" || exit 125
ulimit -u "$2" || exit 125
ulimit -v "$3" || true
shift 3
exec "$@""#;

/// Rootless runtime that materializes [`SandboxTier::Container`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ContainerRuntime {
    Bubblewrap,
}

/// Network mode recorded on a prepared plan. All modes unshare the net ns.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ContainerNetwork {
    Isolated,
    Allowlist,
    Proxy,
}

/// Planned containment for one handle. Secrets stay as a count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerPlan {
    isolation: IsolationStrength,
    network: ContainerNetwork,
    runtime: ContainerRuntime,
    program: String,
    args: Vec<String>,
    guest_cwd: String,
    binds: Vec<PlannedBind>,
    timeout: Duration,
    output_limit: u64,
    cpu_millis: u32,
    memory_mb: u32,
    pids: u32,
    secret_count: usize,
    seccomp: bool,
    user_namespace: bool,
    mount_namespace: bool,
    pid_namespace: bool,
    cgroup_namespace: bool,
    unshare_net: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlannedBind {
    source: Option<String>,
    guest: String,
    mode: MountMode,
}

/// Rootless container backend. Isolation is namespaces/cgroups/seccomp.
pub struct ContainerBackend {
    caps: SandboxCapabilities,
    sessions: Mutex<HashMap<SandboxId, PreparedSession>>,
}

struct PreparedSession {
    handle: SandboxHandle,
    lease_id: LeaseId,
    plan: ContainerPlan,
}

impl ContainerRuntime {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bubblewrap => "bwrap",
        }
    }
}

impl ContainerNetwork {
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

    pub const fn unshares_host_network(self) -> bool {
        true
    }
}

impl ContainerPlan {
    /// Construct argv/config from a typed spec. Does not require a live runtime.
    pub fn from_spec(spec: &SandboxSpec) -> Result<Self, SandboxError> {
        if spec.tier() != SandboxTier::Container {
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
        require_cwd_covered(spec.cwd(), spec.mounts())?;

        let network = ContainerNetwork::from_network(spec.network());
        let program = intended_bwrap_program();
        let guest_cwd = guest_abs(spec.cwd())?;
        let mut binds = Vec::new();
        let mut args = Vec::new();

        args.extend(
            [
                "--die-with-parent",
                "--new-session",
                "--unshare-user",
                "--unshare-pid",
                "--unshare-uts",
                "--unshare-ipc",
                "--unshare-cgroup",
                "--unshare-net",
                "--hostname",
                "sandbox",
                "--seccomp",
                "0",
                "--clearenv",
                "--tmpfs",
                "/",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
                "--dir",
                "/etc",
                "--dir",
                "/var",
                "--dir",
                "/run",
            ]
            .map(str::to_owned),
        );

        for host in SYSTEM_RO_BINDS {
            if !Path::new(host).exists() {
                continue;
            }
            if is_forbidden_host_source(host) || is_docker_socket(host) {
                continue;
            }
            args.push("--ro-bind".to_owned());
            args.push((*host).to_owned());
            args.push((*host).to_owned());
            binds.push(PlannedBind {
                source: Some((*host).to_owned()),
                guest: (*host).to_owned(),
                mode: MountMode::ReadOnly,
            });
        }

        for mount in spec.mounts() {
            let guest = guest_abs(mount.target())?;
            match mount.mode() {
                MountMode::Temp => {
                    args.push("--tmpfs".to_owned());
                    args.push(guest.clone());
                    binds.push(PlannedBind {
                        source: None,
                        guest,
                        mode: MountMode::Temp,
                    });
                }
                MountMode::ReadOnly | MountMode::ReadWrite => {
                    let source = mount.source().ok_or(SandboxError::InvalidSpec)?;
                    if is_forbidden_host_source(source.as_str())
                        || is_docker_socket(source.as_str())
                    {
                        return Err(SandboxError::ForbiddenMount);
                    }
                    let flag = match mount.mode() {
                        MountMode::ReadOnly => "--ro-bind",
                        MountMode::ReadWrite => "--bind",
                        MountMode::Temp => unreachable!("temp handled above"),
                    };
                    args.push(flag.to_owned());
                    args.push(source.as_str().to_owned());
                    args.push(guest.clone());
                    binds.push(PlannedBind {
                        source: Some(source.as_str().to_owned()),
                        guest,
                        mode: mount.mode(),
                    });
                }
            }
        }

        args.push("--chdir".to_owned());
        args.push(guest_cwd.clone());
        args.push("--setenv".to_owned());
        args.push("PATH".to_owned());
        args.push("/usr/bin:/bin".to_owned());
        args.push("--setenv".to_owned());
        args.push("HOME".to_owned());
        args.push("/tmp".to_owned());

        if args.iter().any(|part| is_docker_socket(part)) {
            return Err(SandboxError::ForbiddenMount);
        }

        Ok(Self {
            isolation: IsolationStrength::Namespaces,
            network,
            runtime: ContainerRuntime::Bubblewrap,
            program,
            args,
            guest_cwd,
            binds,
            timeout: spec.timeout(),
            output_limit: spec.output_limit(),
            cpu_millis: spec.cpu_millis(),
            memory_mb: spec.memory_mb(),
            pids: spec.pids(),
            secret_count: spec.secrets().len(),
            seccomp: true,
            user_namespace: true,
            mount_namespace: true,
            pid_namespace: true,
            cgroup_namespace: true,
            unshare_net: network.unshares_host_network(),
        })
    }

    pub const fn isolation(&self) -> IsolationStrength {
        self.isolation
    }

    pub const fn network(&self) -> ContainerNetwork {
        self.network
    }

    pub const fn runtime(&self) -> ContainerRuntime {
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

    pub const fn has_seccomp(&self) -> bool {
        self.seccomp
    }

    pub const fn has_user_namespace(&self) -> bool {
        self.user_namespace
    }

    pub const fn has_mount_namespace(&self) -> bool {
        self.mount_namespace
    }

    pub const fn has_pid_namespace(&self) -> bool {
        self.pid_namespace
    }

    pub const fn has_cgroup_namespace(&self) -> bool {
        self.cgroup_namespace
    }

    pub const fn unshares_host_network(&self) -> bool {
        self.unshare_net
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
        !self.unshare_net
            || self.args.iter().any(|part| {
                part == "--share-net" || part == "--network=host" || part == "--net=host"
            })
    }

    fn program_is_docker(&self) -> bool {
        let name = Path::new(&self.program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&self.program);
        name.eq_ignore_ascii_case("docker") || name.eq_ignore_ascii_case("dockerd")
    }
}

impl ContainerBackend {
    pub fn new() -> Self {
        let caps = SandboxCapabilities::new(
            SandboxTier::Container,
            NetworkCapability::allowlist_and_proxy(),
            MountCapability::workspace_temp(),
            ResourceCapability::bounded(),
        )
        .expect("container is a known sandbox tier");
        Self {
            caps,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Prepared plan for `handle`, if this backend owns it.
    pub fn plan(&self, handle: &SandboxHandle) -> Result<ContainerPlan, SandboxError> {
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

impl Default for ContainerBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxBackend for ContainerBackend {
    fn capabilities(&self) -> SandboxCapabilities {
        self.caps
    }

    fn health(&self, cancel: &CancellationToken) -> Result<BackendHealth, SandboxError> {
        check_cancel(cancel)?;
        if !cfg!(target_os = "linux") {
            return BackendHealth::unavailable(HealthReason::PlatformUnsupported, Some("bwrap"));
        }
        let Some(program) = first_existing(BWRAP_PROGRAMS) else {
            return BackendHealth::unavailable(HealthReason::RuntimeMissing, Some("bwrap"));
        };
        check_cancel(cancel)?;
        let version = bwrap_version(program);
        if !probe_rootless_namespaces(program, cancel)? {
            return BackendHealth::unavailable(HealthReason::FeatureMissing, version.as_deref());
        }
        check_cancel(cancel)?;
        if !probe_seccomp(program, cancel)? {
            return BackendHealth::unavailable(HealthReason::FeatureMissing, version.as_deref());
        }
        BackendHealth::available(version.as_deref().or(Some("bwrap")))
    }

    fn prepare(
        &self,
        spec: &SandboxSpec,
        lease: &CapabilityLease,
        cancel: &CancellationToken,
    ) -> Result<SandboxHandle, SandboxError> {
        check_cancel(cancel)?;
        require_proc_lease(lease)?;
        if spec.tier() != SandboxTier::Container {
            return Err(SandboxError::TierUnavailable);
        }
        self.supports(spec)?;
        let plan = ContainerPlan::from_spec(spec)?;
        if plan.uses_docker_socket() {
            return Err(SandboxError::ForbiddenMount);
        }
        if plan.shares_host_network() {
            return Err(SandboxError::UnsupportedNetwork);
        }
        check_cancel(cancel)?;
        let health = self.health(cancel)?;
        if !health.is_available() {
            return Err(SandboxError::TierUnavailable);
        }
        let handle = SandboxHandle::new(SandboxTier::Container, lease.lease_id())?;
        let mut sessions = self.lock_sessions()?;
        if sessions.len() >= MAX_LIVE_CONTAINER_SANDBOXES {
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
        if handle.tier() != SandboxTier::Container {
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
        run_container(&plan, request, cancel)
    }

    fn destroy(
        &self,
        handle: &SandboxHandle,
        cancel: &CancellationToken,
    ) -> Result<(), SandboxError> {
        check_cancel(cancel)?;
        if handle.tier() != SandboxTier::Container {
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

fn intended_bwrap_program() -> String {
    first_existing(BWRAP_PROGRAMS)
        .unwrap_or(BWRAP_PROGRAMS[0])
        .to_owned()
}

fn guest_abs(path: &RepoPath) -> Result<String, SandboxError> {
    let mut guest = String::from("/");
    guest.push_str(path.as_str());
    if guest.contains('\0') {
        return Err(SandboxError::Nul);
    }
    if is_docker_socket(&guest) {
        return Err(SandboxError::ForbiddenMount);
    }
    Ok(guest)
}

fn validate_mount(mount: &SandboxMount) -> Result<(), SandboxError> {
    if is_docker_socket(mount.target().as_str()) {
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

fn require_cwd_covered(cwd: &RepoPath, mounts: &[SandboxMount]) -> Result<(), SandboxError> {
    let mut best: Option<usize> = None;
    for mount in mounts {
        if let Some(prefix_len) = target_covers(mount.target(), cwd)
            && best.is_none_or(|len| prefix_len > len)
        {
            best = Some(prefix_len);
        }
    }
    best.ok_or(SandboxError::ForbiddenMount).map(|_| ())
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

fn resolve_existing_dir(path: &Path) -> Result<CanonicalHostPath, SandboxError> {
    let requested = path.to_str().ok_or(SandboxError::ForbiddenMount)?;
    if is_forbidden_host_source(requested) || is_docker_socket(requested) {
        return Err(SandboxError::ForbiddenMount);
    }
    let canon =
        protocol::host_path::canonicalize(path).map_err(|_| SandboxError::ForbiddenMount)?;
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
    let canon = protocol::host_path::canonicalize(requested.as_str())
        .map_err(|_| SandboxError::ForbiddenMount)?;
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
    DOCKER_SOCKET_NAMES
        .iter()
        .any(|socket| trimmed == *socket || trimmed.ends_with(&socket.to_string()))
}

fn is_proxy_env_name(name: &str) -> bool {
    PROXY_ENV_NAMES
        .iter()
        .any(|blocked| name.eq_ignore_ascii_case(blocked))
}

fn cpu_limit_seconds(cpu_millis: u32) -> u64 {
    u64::from(cpu_millis.div_ceil(1_000)).max(1)
}

fn memory_limit_kb(memory_mb: u32) -> u64 {
    u64::from(memory_mb).saturating_mul(1024).max(1024)
}

fn bwrap_version(program: &str) -> Option<String> {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let ident = line
        .lines()
        .next()?
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    if ident.is_empty() || ident.len() > 64 {
        Some("bwrap".to_owned())
    } else {
        Some(ident)
    }
}

fn probe_rootless_namespaces(
    program: &str,
    cancel: &CancellationToken,
) -> Result<bool, SandboxError> {
    check_cancel(cancel)?;
    let true_bin = match first_existing(TRUE_PROGRAMS) {
        Some(path) => path,
        None => return Ok(false),
    };
    let mut command = Command::new(program);
    command.args([
        "--die-with-parent",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-uts",
        "--unshare-ipc",
        "--unshare-cgroup",
        "--unshare-net",
        true_bin,
    ]);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    command.env_clear();
    isolate_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return Ok(false),
    };
    wait_probe(&mut child, cancel)
}

fn probe_seccomp(program: &str, cancel: &CancellationToken) -> Result<bool, SandboxError> {
    check_cancel(cancel)?;
    let Some(filter) = seccomp_filter_bytes() else {
        return Ok(false);
    };
    let true_bin = match first_existing(TRUE_PROGRAMS) {
        Some(path) => path,
        None => return Ok(false),
    };
    let mut command = Command::new(program);
    command.args([
        "--die-with-parent",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-net",
        "--seccomp",
        "0",
        true_bin,
    ]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    command.env_clear();
    isolate_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return Ok(false),
    };
    match child.stdin.take() {
        Some(mut stdin) => {
            if stdin.write_all(&filter).is_err() {
                terminate_process_group_default(&mut child);
                return Ok(false);
            }
        }
        None => {
            terminate_process_group_default(&mut child);
            return Ok(false);
        }
    }
    wait_probe(&mut child, cancel)
}

fn wait_probe(child: &mut Child, cancel: &CancellationToken) -> Result<bool, SandboxError> {
    let started = Instant::now();
    let mut polls = 0u32;
    loop {
        if polls.is_multiple_of(CANCEL_STRIDE) && cancel.is_cancelled() {
            terminate_process_group_default(child);
            return Err(SandboxError::Cancelled);
        }
        if started.elapsed() >= HEALTH_PROBE_TIMEOUT {
            terminate_process_group_default(child);
            return Ok(false);
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success()),
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(_) => {
                terminate_process_group_default(child);
                return Ok(false);
            }
        }
        polls = polls.saturating_add(1);
    }
}

fn run_container(
    plan: &ContainerPlan,
    request: &SandboxExecRequest,
    cancel: &CancellationToken,
) -> Result<SandboxExecResult, SandboxError> {
    check_cancel(cancel)?;
    if plan.uses_docker_socket() {
        return Err(SandboxError::ForbiddenMount);
    }
    if plan.shares_host_network() {
        return Err(SandboxError::UnsupportedNetwork);
    }
    let argv = request.argv();
    let program = resolve_existing_file(&argv[0])?;
    let sh = first_existing(POSIX_SH).ok_or(SandboxError::ResourceLimit)?;
    let filter = seccomp_filter_bytes().ok_or(SandboxError::HealthFailed)?;
    let mut command = Command::new(sh);
    command.arg("-c");
    command.arg(APPLY_RLIMITS);
    command.arg("container");
    command.arg(cpu_limit_seconds(plan.cpu_millis).to_string());
    command.arg(plan.pids.to_string());
    command.arg(memory_limit_kb(plan.memory_mb).to_string());
    command.arg(&plan.program);
    command.args(&plan.args);
    command.arg("--");
    command.arg(program.as_str());
    command.args(&argv[1..]);
    command.env_clear();
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    isolate_process_group(&mut command);
    let mut child = command.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            SandboxError::TierUnavailable
        } else {
            SandboxError::HealthFailed
        }
    })?;
    match child.stdin.take() {
        Some(mut stdin) => {
            if stdin.write_all(&filter).is_err() {
                terminate_process_group_default(&mut child);
                return Err(SandboxError::HealthFailed);
            }
        }
        None => {
            terminate_process_group_default(&mut child);
            return Err(SandboxError::HealthFailed);
        }
    }
    let started = Instant::now();
    let outcome = wait_child(
        &mut child,
        request.timeout(),
        request.output_limit(),
        plan,
        cancel,
    );
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
    plan: &ContainerPlan,
    cancel: &CancellationToken,
) -> WaitOutcome {
    let cap = usize::try_from(output_limit).unwrap_or(usize::MAX);
    let stdout = match child.stdout.take() {
        Some(pipe) => pipe,
        None => {
            terminate_process_group_default(child);
            return WaitOutcome::Failed;
        }
    };
    let stderr = match child.stderr.take() {
        Some(pipe) => pipe,
        None => {
            terminate_process_group_default(child);
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
            terminate_process_group_default(child);
            let output = join_output(stdout_thread, stderr_thread);
            return WaitOutcome::Cancelled { output, usage };
        }
        if started.elapsed() >= timeout {
            terminate_process_group_default(child);
            let output = join_output(stdout_thread, stderr_thread);
            return WaitOutcome::TimedOut { output, usage };
        }
        if let Some(sample) = sample_process_group(pgid) {
            usage.pids_peak = usage.pids_peak.max(sample.0);
            usage.memory_peak_mb = usage.memory_peak_mb.max(sample.1);
            if sample.1 > u64::from(plan.memory_mb) {
                terminate_process_group_default(child);
                let output = join_output(stdout_thread, stderr_thread);
                return WaitOutcome::Oom { output, usage };
            }
            if sample.0 > plan.pids {
                terminate_process_group_default(child);
                let output = join_output(stdout_thread, stderr_thread);
                return WaitOutcome::PidsExceeded { output, usage };
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(_) => {
                terminate_process_group_default(child);
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

/// Default-allow filter that kills sandbox-escape syscalls. Arch-specific.
fn seccomp_filter_bytes() -> Option<Vec<u8>> {
    const BPF_LD_W_ABS: u16 = 0x20;
    const BPF_JMP_JEQ_K: u16 = 0x15;
    const BPF_RET_K: u16 = 0x06;
    const OFF_NR: u32 = 0;
    const OFF_ARCH: u32 = 4;
    const RET_KILL: u32 = 0x8000_0000;
    const RET_ALLOW: u32 = 0x7fff_0000;

    let (arch, blocked) = seccomp_arch_blocked()?;
    let mut filter: Vec<(u16, u8, u8, u32)> = Vec::with_capacity(blocked.len() * 2 + 4);
    filter.push((BPF_LD_W_ABS, 0, 0, OFF_ARCH));
    filter.push((BPF_JMP_JEQ_K, 1, 0, arch));
    filter.push((BPF_RET_K, 0, 0, RET_KILL));
    filter.push((BPF_LD_W_ABS, 0, 0, OFF_NR));
    for nr in blocked {
        filter.push((BPF_JMP_JEQ_K, 0, 1, *nr));
        filter.push((BPF_RET_K, 0, 0, RET_KILL));
    }
    filter.push((BPF_RET_K, 0, 0, RET_ALLOW));

    let mut bytes = Vec::with_capacity(filter.len() * 8);
    for (code, jt, jf, k) in filter {
        bytes.extend_from_slice(&code.to_ne_bytes());
        bytes.push(jt);
        bytes.push(jf);
        bytes.extend_from_slice(&k.to_ne_bytes());
    }
    Some(bytes)
}

fn seccomp_arch_blocked() -> Option<(u32, &'static [u32])> {
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH_AARCH64: u32 = 0xC000_00B7;
    #[cfg(target_arch = "x86_64")]
    {
        const BLOCKED: &[u32] = &[
            101, 165, 166, 155, 169, 175, 176, 246, 321, 298, 167, 168, 170, 171, 163, 308, 272,
            250, 248, 249, 173, 172, 135, 323, 304, 303, 103, 313, 320, 174, 212, 180, 178, 177,
            134, 300,
        ];
        Some((AUDIT_ARCH_X86_64, BLOCKED))
    }
    #[cfg(target_arch = "aarch64")]
    {
        const BLOCKED: &[u32] = &[
            117, 40, 39, 41, 142, 105, 106, 104, 280, 241, 224, 225, 161, 162, 89, 268, 97, 219,
            217, 218, 92, 282, 265, 264, 116, 273, 294, 262,
        ];
        Some((AUDIT_ARCH_AARCH64, BLOCKED))
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use capability_broker::{
        ActionRequest, ApprovalChoice, ApprovalResolution, ApprovalScopeId, CanonicalAction,
        FilesystemScope, LeaseIssuer, PolicyDocument, PolicySource, PolicyStack, PrincipalRef,
        ProcessScope, ResourceDescriptor, SecretHandle, evaluate, issue, request_approval,
    };
    use protocol::{ErrorCode, SessionId};

    use crate::backend::SandboxManager;
    use crate::backends::host_restricted::HostRestrictedBackend;

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-7c1e9b";

    struct TempWorkspace {
        path: PathBuf,
        host: CanonicalHostPath,
    }

    impl TempWorkspace {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("rapidlm-ctr-sbx-{}", protocol::RuntimeId::new()));
            fs::create_dir_all(&path).expect("temp workspace");
            let canon = protocol::host_path::canonicalize(&path).expect("canonicalize");
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

    fn container_spec(ws: &TempWorkspace) -> SandboxSpec {
        SandboxSpec::builder(SandboxTier::Container)
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

    fn live_runtime(backend: &ContainerBackend) -> Option<BackendHealth> {
        let health = backend
            .health(&CancellationToken::new())
            .expect("health probe");
        if health.is_available() {
            Some(health)
        } else {
            None
        }
    }

    #[test]
    fn isolation_is_namespaces_and_strong() {
        let backend = ContainerBackend::new();
        let caps = backend.capabilities();
        assert_eq!(caps.tier(), SandboxTier::Container);
        assert_eq!(caps.isolation(), IsolationStrength::Namespaces);
        assert!(caps.isolation().is_strong_isolation());
        assert!(caps.isolation().doctor_warning().is_none());
        assert!(caps.network().allowlist_supported());
        assert!(caps.network().proxy_supported());
    }

    #[test]
    fn plan_constructs_argv_from_typed_spec_without_docker() {
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::Container)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .mount(SandboxMount::temp(RepoPath::parse("tmp").expect("tmp")).expect("temp"))
            .network(SandboxNetwork::None)
            .secret(SecretHandle::parse(CANARY).expect("secret"))
            .build()
            .expect("spec");
        let plan = ContainerPlan::from_spec(&spec).expect("plan");
        let argv = plan.argv();
        assert_eq!(plan.runtime(), ContainerRuntime::Bubblewrap);
        assert_eq!(plan.network(), ContainerNetwork::Isolated);
        assert_eq!(plan.network().as_str(), "none");
        assert!(plan.has_user_namespace());
        assert!(plan.has_mount_namespace());
        assert!(plan.has_pid_namespace());
        assert!(plan.has_cgroup_namespace());
        assert!(plan.has_seccomp());
        assert!(plan.unshares_host_network());
        assert!(!plan.shares_host_network());
        assert!(!plan.uses_docker_socket());
        assert!(argv.iter().any(|part| part == "--unshare-user"));
        assert!(argv.iter().any(|part| part == "--unshare-pid"));
        assert!(argv.iter().any(|part| part == "--unshare-cgroup"));
        assert!(argv.iter().any(|part| part == "--unshare-net"));
        assert!(argv.iter().any(|part| part == "--seccomp"));
        assert!(argv.iter().any(|part| part == "--bind"));
        assert!(argv.iter().any(|part| part == "--tmpfs"));
        assert!(!argv.iter().any(|part| part.contains("docker.sock")));
        assert!(!argv.iter().any(|part| part == "--privileged"));
        assert!(!argv.iter().any(|part| part == "--share-net"));
        assert!(!argv.iter().any(|part| part.contains(CANARY)));
        assert_eq!(plan.secret_count(), 1);
        let debug = format!("{plan:?}");
        assert!(!debug.contains(CANARY));
        assert!(!debug.contains("PLAINTEXT"));
        assert!(!plan.program().ends_with("docker"));
    }

    #[test]
    fn docker_socket_on_host_is_never_used_or_mounted() {
        let backend = ContainerBackend::new();
        let live = CancellationToken::new();
        let health = backend.health(&live).expect("health");
        for socket in DOCKER_SOCKET_NAMES {
            if Path::new(socket).exists() {
                assert!(
                    !health.is_available()
                        || health
                            .version()
                            .is_none_or(|v| !v.to_ascii_lowercase().contains("docker")),
                    "host docker.sock must not make container health a clean/pass"
                );
            }
        }
        let ws = TempWorkspace::new();
        let plan = ContainerPlan::from_spec(&container_spec(&ws)).expect("plan");
        assert!(!plan.uses_docker_socket());
        assert!(plan.bind_sources().all(|src| !is_docker_socket(src)));

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
        assert!(
            !SandboxError::ForbiddenMount
                .as_str()
                .contains("docker.sock")
        );
        assert_eq!(
            SandboxError::ForbiddenMount.error_code(),
            Some(ErrorCode::PolicyDenied)
        );
    }

    #[test]
    fn health_is_unavailable_not_clean_when_host_cannot_rootless() {
        let backend = ContainerBackend::new();
        let health = backend.health(&CancellationToken::new()).expect("health");
        if live_runtime(&backend).is_some() {
            assert!(health.is_available());
            assert!(health.reason().is_none());
            return;
        }
        assert!(!health.is_available(), "unavailable is not a clean/pass");
        let reason = health.reason().expect("reason");
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
    }

    #[test]
    fn manager_does_not_downgrade_to_host_when_container_is_unhealthy() {
        let mut mgr = SandboxManager::new();
        mgr.register(Box::new(HostRestrictedBackend::new()))
            .expect("host");
        mgr.register(Box::new(ContainerBackend::new()))
            .expect("container");
        let ws = TempWorkspace::new();
        let spec = container_spec(&ws);
        let live = CancellationToken::new();
        if live_runtime(&ContainerBackend::new()).is_some() {
            let selected = mgr.select(&spec, &live).expect("select");
            assert_eq!(selected.capabilities().tier(), SandboxTier::Container);
            return;
        }
        match mgr.select(&spec, &live) {
            Ok(selected) => {
                panic!(
                    "container spec must not select {:?}/{:?}",
                    selected.capabilities().tier(),
                    selected.capabilities().isolation()
                )
            }
            Err(err) => {
                assert_eq!(err, SandboxError::TierUnavailable);
                assert_eq!(err.error_code(), Some(ErrorCode::SandboxTierUnavailable));
            }
        }
    }

    #[test]
    fn prepare_refuses_weaker_tier_and_images() {
        let backend = ContainerBackend::new();
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
        let imaged = SandboxSpec::builder(SandboxTier::Container)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .image("alpine:latest")
            .build()
            .expect("image");
        assert_eq!(
            backend.prepare(&imaged, &lease, &live).expect_err("image"),
            SandboxError::InvalidSpec
        );
    }

    #[test]
    fn proxy_env_and_wrong_lease_are_rejected() {
        let backend = ContainerBackend::new();
        let ws = TempWorkspace::new();
        let live = CancellationToken::new();
        let env = SandboxSpec::builder(SandboxTier::Container)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .env_allowlist(["PATH", "HTTPS_PROXY"])
            .build()
            .expect("env");
        assert_eq!(
            backend
                .prepare(&env, &proc_lease(), &live)
                .expect_err("proxy env"),
            SandboxError::UnsupportedNetwork
        );
        assert_eq!(
            backend
                .prepare(&container_spec(&ws), &fs_lease(), &live)
                .expect_err("fs"),
            SandboxError::LeaseInvalid
        );
        assert_eq!(
            SandboxError::LeaseInvalid.error_code(),
            Some(ErrorCode::PolicyLeaseInvalid)
        );
        assert!(!SandboxError::LeaseInvalid.as_str().contains(CANARY));
    }

    #[test]
    fn home_mount_cannot_bypass_path_policy() {
        let backend = ContainerBackend::new();
        let lease = proc_lease();
        let live = CancellationToken::new();
        let home = CanonicalHostPath::from_resolved("/Users/canary-home").expect("home");
        let spec = SandboxSpec::builder(SandboxTier::Container)
            .cwd(cwd())
            .mount(SandboxMount::bind(home, cwd(), MountMode::ReadWrite).expect("bind"))
            .build()
            .expect("spec");
        assert_eq!(
            backend.prepare(&spec, &lease, &live).expect_err("home"),
            SandboxError::ForbiddenMount
        );
        let ssh = CanonicalHostPath::from_resolved("/Users/canary-home/.ssh").expect("ssh");
        let spec = SandboxSpec::builder(SandboxTier::Container)
            .cwd(cwd())
            .mount(SandboxMount::bind(ssh, cwd(), MountMode::ReadOnly).expect("ssh"))
            .build()
            .expect("spec");
        assert_eq!(
            backend.prepare(&spec, &lease, &live).expect_err("ssh"),
            SandboxError::ForbiddenMount
        );
        assert!(
            !SandboxError::ForbiddenMount
                .as_str()
                .contains("canary-home")
        );
        assert!(!SandboxError::ForbiddenMount.as_str().contains(CANARY));
    }

    #[test]
    fn fixture_cannot_read_host_home_outside_declared_mount() {
        let backend = ContainerBackend::new();
        let ws = TempWorkspace::new();
        let spec = container_spec(&ws);
        let plan = ContainerPlan::from_spec(&spec).expect("plan");
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            assert!(
                plan.bind_sources().all(|src| src != home.as_str()
                    && !src.starts_with(&format!("{}/", home.trim_end_matches('/')))),
                "host home must not be a bind source"
            );
        }
        assert!(plan.bind_sources().all(|src| {
            !is_home_root(
                &src.to_ascii_lowercase()
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>(),
            )
        }));

        let live = CancellationToken::new();
        let Some(_) = live_runtime(&backend) else {
            let health = backend.health(&live).expect("health");
            assert!(!health.is_available());
            return;
        };
        let lease = proc_lease();
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
        let ls = host_tool(&["/bin/ls", "/usr/bin/ls"]).expect("ls");
        let request =
            SandboxExecRequest::new([ls, &home], Duration::from_secs(2), 4096).expect("request");
        let result = backend
            .exec(&handle, &request, &lease, &live)
            .expect("exec");
        assert_ne!(result.exit().code(), Some(0), "home must not be readable");
        assert!(!result.exit().timed_out());
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn network_deny_verified_by_local_test_endpoint() {
        let backend = ContainerBackend::new();
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::Container)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .network(SandboxNetwork::None)
            .build()
            .expect("spec");
        let plan = ContainerPlan::from_spec(&spec).expect("plan");
        assert_eq!(plan.network(), ContainerNetwork::Isolated);
        assert!(plan.unshares_host_network());
        assert!(!plan.shares_host_network());

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
            assert!(matches!(
                health.reason(),
                Some(
                    HealthReason::PlatformUnsupported
                        | HealthReason::RuntimeMissing
                        | HealthReason::FeatureMissing
                )
            ));
            thread::sleep(Duration::from_millis(50));
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "plan isolation must not open the host endpoint"
            );
            return;
        };

        let lease = proc_lease();
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");
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
            "sandbox with network none must not reach the local endpoint"
        );
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn timeout_and_cancellation_are_explicit_when_runtime_is_live() {
        let backend = ContainerBackend::new();
        let live = CancellationToken::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            backend.health(&cancel).expect_err("health"),
            SandboxError::Cancelled
        );
        assert_eq!(SandboxError::Cancelled.error_code(), None);

        let ws = TempWorkspace::new();
        let spec = container_spec(&ws);
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
        let backend = ContainerBackend::new();
        let ws = TempWorkspace::new();
        let spec = SandboxSpec::builder(SandboxTier::Container)
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
            let heavy = SandboxSpec::builder(SandboxTier::Container)
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
    fn doctor_lists_container_without_host_warning() {
        let mut mgr = SandboxManager::new();
        mgr.register(Box::new(ContainerBackend::new()))
            .expect("register");
        let reports = mgr.doctor(&CancellationToken::new()).expect("doctor");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].tier(), SandboxTier::Container);
        assert_eq!(reports[0].isolation(), IsolationStrength::Namespaces);
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
        let spec = SandboxSpec::builder(SandboxTier::Container)
            .cwd(RepoPath::parse("docs").expect("docs"))
            .mount(ws.mount("src", MountMode::ReadWrite))
            .build()
            .expect("spec");
        assert_eq!(
            ContainerPlan::from_spec(&spec).expect_err("cwd"),
            SandboxError::ForbiddenMount
        );
    }

    #[test]
    fn allowlist_and_proxy_modes_still_unshare_net() {
        let ws = TempWorkspace::new();
        for network in [SandboxNetwork::Allowlist, SandboxNetwork::Proxy] {
            let spec = SandboxSpec::builder(SandboxTier::Container)
                .cwd(cwd())
                .mount(ws.mount("src", MountMode::ReadWrite))
                .network(network)
                .build()
                .expect("spec");
            let plan = ContainerPlan::from_spec(&spec).expect("plan");
            assert!(plan.unshares_host_network());
            assert!(!plan.shares_host_network());
            assert!(plan.argv().iter().any(|part| part == "--unshare-net"));
        }
    }

    #[test]
    fn cancelled_prepare_is_not_a_clean_pass() {
        let backend = ContainerBackend::new();
        let ws = TempWorkspace::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            backend
                .prepare(&container_spec(&ws), &proc_lease(), &cancel)
                .expect_err("prepare"),
            SandboxError::Cancelled
        );
    }
}
