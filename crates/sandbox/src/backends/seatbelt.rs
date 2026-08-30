//! macOS Seatbelt (`sandbox-exec`) sandbox backend.
//!
//! Wraps the same `sandbox-exec -f <profile>` invocation `apps/rapid`'s
//! existing async job-based `shell_exec(sandbox: true)` path already uses
//! (`apps/rapid/src/exec_tools.rs::seatbelt_profile`/`find_sandbox_exec`),
//! but through `SandboxManager`'s ordinary synchronous `SandboxBackend`
//! contract instead of a special-cased job. The two paths are deliberately
//! left unmerged for now (see `newtask.md` §1.1 item #2 and
//! `apps/rapid/src/sandbox_exec.rs`'s own doc comment on the same split) —
//! wiring `apps/rapid` to prefer this backend over the existing job path is
//! separate follow-up work, not attempted here.
//!
//! Lands at [`SandboxTier::HostRestricted`] (so `isolation()` reports
//! [`IsolationStrength::ProcessPolicy`]) even though a real Seatbelt profile
//! is a stronger property than that tier's other backend
//! ([`crate::HostRestrictedBackend`], process-group + rlimits only, no real
//! filesystem boundary). `IsolationStrength`'s four variants have no clean
//! slot for "single-process syscall/file mediation but not a container" —
//! this is the taxonomy gap `newtask.md` flags, deliberately not resolved
//! by widening `protocol::SandboxTier` speculatively for one backend.
//!
//! Only `SandboxNetwork::None` is accepted (`Allowlist`/`Proxy` are
//! refused as unsupported): the profile below adds a real `(deny
//! network*)` rule for that case — verified empirically against the real
//! `sandbox-exec` binary that a later `(allow default)` does not undo an
//! earlier `(deny network*)` — a genuine guarantee neither the existing
//! job-based Seatbelt path nor `HostRestrictedBackend`'s process-policy-only
//! isolation makes today.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use capability_broker::{CancellationToken, CanonicalHostPath, Capability, CapabilityLease};
use protocol::{LeaseId, SandboxTier};

use crate::backend::{
    BackendHealth, HealthReason, MountCapability, MountMode, NetworkCapability, ResourceCapability,
    ResourceUsage, SandboxBackend, SandboxCapabilities, SandboxError, SandboxExecRequest,
    SandboxExecResult, SandboxExit, SandboxExitReason, SandboxHandle, SandboxId, SandboxNetwork,
    SandboxSpec,
};
use crate::backends::host_restricted::{
    is_forbidden_host_source, pid_rss_kb, resolve_cwd, resolve_existing_dir,
};

/// Maximum prepared Seatbelt sandboxes retained by one backend.
pub const MAX_LIVE_SEATBELT_SANDBOXES: usize = 64;

const SEATBELT_VERSION: &str = "seatbelt";
const POLL_INTERVAL: Duration = Duration::from_millis(10);

const POSIX_SH: &[&str] = &["/bin/sh", "/usr/bin/sh"];
/// Fixed helper: set RLIMIT_CPU then exec the already-validated argv.
/// Integers and program argv are positional; nothing is interpolated.
/// Same technique `host_restricted.rs::APPLY_CPU_RLIMIT` uses — duplicated
/// rather than shared since it's a fixed, trivial three-line script, unlike
/// the mount/path-validation logic this module already reuses from there.
const APPLY_CPU_RLIMIT: &str = r#"ulimit -t "$1" || exit 125
shift
exec "$@""#;

fn sandbox_exec_binary() -> Option<&'static str> {
    ["/usr/bin/sandbox-exec", "/bin/sandbox-exec"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
}

fn first_existing(candidates: &[&'static str]) -> Option<&'static str> {
    candidates.iter().copied().find(|path| Path::new(path).is_file())
}

fn cpu_limit_seconds(cpu_millis: u32) -> u64 {
    u64::from(cpu_millis.div_ceil(1_000)).max(1)
}

/// Prepared, immutable plan for one handle. `profile_path` is a real file on
/// disk (written in `prepare`, removed in `destroy`) since `sandbox-exec`
/// only accepts a profile by path, not on stdin.
#[derive(Clone, Debug)]
struct SeatbeltPlan {
    sandbox_exec: PathBuf,
    profile_path: PathBuf,
    cwd_host: CanonicalHostPath,
    timeout: Duration,
    output_limit: u64,
    cpu_millis: u32,
    memory_mb: u32,
}

struct PreparedSession {
    handle: SandboxHandle,
    lease_id: LeaseId,
    plan: SeatbeltPlan,
}

fn session_matches(session: &PreparedSession, handle: &SandboxHandle) -> bool {
    session.handle == *handle
}

/// Native macOS Seatbelt backend. Deterministic; no vendor engine.
pub struct SeatbeltBackend {
    caps: SandboxCapabilities,
    sessions: Mutex<HashMap<SandboxId, PreparedSession>>,
}

impl SeatbeltBackend {
    pub fn new() -> Self {
        let caps = SandboxCapabilities::new(
            SandboxTier::HostRestricted,
            NetworkCapability::none_only(),
            MountCapability::workspace_temp(),
            ResourceCapability::bounded(),
        )
        .expect("host-restricted is a known sandbox tier");
        Self {
            caps,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn lock_sessions(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<SandboxId, PreparedSession>>, SandboxError> {
        self.sessions.lock().map_err(|_| SandboxError::HealthFailed)
    }
}

impl Default for SeatbeltBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxBackend for SeatbeltBackend {
    fn capabilities(&self) -> SandboxCapabilities {
        self.caps
    }

    fn health(&self, cancel: &CancellationToken) -> Result<BackendHealth, SandboxError> {
        check_cancel(cancel)?;
        if !cfg!(target_os = "macos") {
            return BackendHealth::unavailable(HealthReason::PlatformUnsupported, None);
        }
        match sandbox_exec_binary() {
            Some(_) => BackendHealth::available(Some(SEATBELT_VERSION)),
            None => BackendHealth::unavailable(HealthReason::RuntimeMissing, None),
        }
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
        if !matches!(spec.network(), SandboxNetwork::None) {
            return Err(SandboxError::UnsupportedNetwork);
        }
        let sandbox_exec = sandbox_exec_binary().ok_or(SandboxError::HealthFailed)?;
        let mut write_roots = Vec::new();
        for mount in spec.mounts() {
            if mount.mode() != MountMode::ReadWrite {
                continue;
            }
            let source = mount.source().ok_or(SandboxError::InvalidSpec)?;
            if is_forbidden_host_source(source.as_str()) {
                return Err(SandboxError::ForbiddenMount);
            }
            let resolved = resolve_existing_dir(Path::new(source.as_str()))?;
            if is_forbidden_host_source(resolved.as_str()) {
                return Err(SandboxError::ForbiddenMount);
            }
            write_roots.push(resolved);
        }
        let cwd_host = resolve_cwd(spec.cwd(), spec.mounts())?;
        check_cancel(cancel)?;
        let handle = SandboxHandle::new(SandboxTier::HostRestricted, lease.lease_id())?;
        let profile = render_profile(&write_roots);
        let profile_path = std::env::temp_dir()
            .join(format!("rapidlm-seatbelt-{}.sb", handle.id().as_runtime()));
        fs::write(&profile_path, profile.as_bytes()).map_err(|_| SandboxError::HealthFailed)?;
        let plan = SeatbeltPlan {
            sandbox_exec: PathBuf::from(sandbox_exec),
            profile_path,
            cwd_host,
            timeout: spec.timeout(),
            output_limit: spec.output_limit(),
            cpu_millis: spec.cpu_millis(),
            memory_mb: spec.memory_mb(),
        };
        let mut sessions = self.lock_sessions()?;
        if sessions.len() >= MAX_LIVE_SEATBELT_SANDBOXES {
            let _ = fs::remove_file(&plan.profile_path);
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
        run_seatbelt(&plan, request, cancel)
    }

    fn destroy(&self, handle: &SandboxHandle, cancel: &CancellationToken) -> Result<(), SandboxError> {
        check_cancel(cancel)?;
        if handle.tier() != SandboxTier::HostRestricted {
            return Err(SandboxError::UnknownHandle);
        }
        let mut sessions = self.lock_sessions()?;
        match sessions.remove(&handle.id()) {
            Some(session) if session_matches(&session, handle) => {
                let _ = fs::remove_file(&session.plan.profile_path);
                Ok(())
            }
            Some(session) => {
                sessions.insert(handle.id(), session);
                Err(SandboxError::UnknownHandle)
            }
            None => Err(SandboxError::UnknownHandle),
        }
    }
}

/// `(version 1) (deny file-write*)` plus one `(allow file-write* (subpath
/// ...))` per resolved read-write mount, `/dev/` and `/private/tmp/` always
/// allowed for ordinary scratch/pipe use (matching the existing job-based
/// path's own profile), a `(deny network*)` (only reached when `prepare`
/// has already confirmed the request was `SandboxNetwork::None`), and
/// `(allow default)` last for everything else — same shape and rule order
/// as `apps/rapid/src/exec_tools.rs::seatbelt_profile`, empirically
/// verified (outside this crate, against the real `sandbox-exec` binary)
/// that a trailing `(allow default)` does not undo an earlier `(deny
/// network*)` or narrow `(allow file-write* (subpath ...))`.
fn render_profile(write_roots: &[CanonicalHostPath]) -> String {
    let mut profile = String::from("(version 1)\n(deny file-write*)\n");
    for root in write_roots {
        profile.push_str(&format!(
            "(allow file-write* (subpath {:?}))\n",
            root.as_str()
        ));
    }
    profile.push_str("(allow file-write* (subpath \"/dev/\") (subpath \"/private/tmp/\"))\n");
    profile.push_str("(deny network*)\n");
    profile.push_str("(allow default)\n");
    profile
}

fn run_seatbelt(
    plan: &SeatbeltPlan,
    request: &SandboxExecRequest,
    cancel: &CancellationToken,
) -> Result<SandboxExecResult, SandboxError> {
    check_cancel(cancel)?;
    let argv = request.argv();
    // CPU ceiling: same `sh -c 'ulimit -t ...; exec ...'` wrapper technique
    // `host_restricted.rs` uses, applied to the whole `sandbox-exec`
    // invocation — `exec` replaces the process image (and the rlimit
    // survives it) while keeping the `current_dir` set below, since `exec`
    // never changes the calling process's cwd.
    let sh = first_existing(POSIX_SH).ok_or(SandboxError::ResourceLimit)?;
    let secs = cpu_limit_seconds(plan.cpu_millis).to_string();
    let mut command = Command::new(sh);
    command.arg("-c");
    command.arg(APPLY_CPU_RLIMIT);
    command.arg("seatbelt");
    command.arg(secs);
    command.arg(&plan.sandbox_exec);
    command.arg("-f");
    command.arg(&plan.profile_path);
    command.args(argv);
    command.current_dir(plan.cwd_host.as_str());
    command.env_clear();
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
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
        plan.memory_mb,
        cancel,
    );
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    fn usage(elapsed_ms: u64, output: &[u8]) -> ResourceUsage {
        ResourceUsage::new(
            elapsed_ms,
            0,
            1,
            u64::try_from(output.len()).unwrap_or(u64::MAX),
        )
    }
    match outcome {
        WaitOutcome::Finished { code, signal, output } => Ok(SandboxExecResult::new(
            SandboxExit::new(
                code,
                signal,
                SandboxExitReason::Exited,
                false,
                false,
                false,
                usage(elapsed_ms, &output),
            ),
            output,
        )),
        WaitOutcome::TimedOut { output } => Ok(SandboxExecResult::new(
            SandboxExit::new(
                None,
                None,
                SandboxExitReason::TimedOut,
                false,
                true,
                false,
                usage(elapsed_ms, &output),
            ),
            output,
        )),
        WaitOutcome::Cancelled { output } => Ok(SandboxExecResult::new(
            SandboxExit::new(
                None,
                None,
                SandboxExitReason::Cancelled,
                false,
                false,
                false,
                usage(elapsed_ms, &output),
            ),
            output,
        )),
        WaitOutcome::Oom { output } => Ok(SandboxExecResult::new(
            SandboxExit::new(
                None,
                None,
                SandboxExitReason::Oom,
                true,
                false,
                false,
                usage(elapsed_ms, &output),
            ),
            output,
        )),
    }
}

enum WaitOutcome {
    Finished {
        code: Option<i32>,
        signal: Option<i32>,
        output: Vec<u8>,
    },
    TimedOut {
        output: Vec<u8>,
    },
    Cancelled {
        output: Vec<u8>,
    },
    Oom {
        output: Vec<u8>,
    },
}

fn wait_child(
    child: &mut Child,
    timeout: Duration,
    output_limit: u64,
    memory_mb: u32,
    cancel: &CancellationToken,
) -> WaitOutcome {
    let cap = usize::try_from(output_limit).unwrap_or(usize::MAX);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = stdout.map(|pipe| thread::spawn(move || read_capped(pipe, cap)));
    let stderr_reader = stderr.map(|pipe| thread::spawn(move || read_capped(pipe, cap)));
    let deadline = Instant::now() + timeout;
    let pid = child.id();
    enum Stop {
        Timeout,
        Cancelled,
        Oom,
    }
    let outcome = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break Ok(status);
        }
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            break Err(Stop::Cancelled);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break Err(Stop::Timeout);
        }
        // Single-pid RSS check: unlike `host_restricted.rs`'s process-group
        // model, `sh -c '...; exec sandbox-exec ...'` execs straight through
        // to the final target (the whole chain shares one pid, `exec` never
        // changes it), so there is no group to sum across — reusing
        // `pid_rss_kb` directly is exact here, not an approximation.
        if let Some(rss_kb) = pid_rss_kb(pid)
            && rss_kb.div_ceil(1024) > u64::from(memory_mb)
        {
            let _ = child.kill();
            let _ = child.wait();
            break Err(Stop::Oom);
        }
        thread::sleep(POLL_INTERVAL);
    };
    let output = join_output(stdout_reader, stderr_reader);
    match outcome {
        Ok(status) => WaitOutcome::Finished {
            code: status.code(),
            signal: exit_signal(&status),
            output,
        },
        Err(Stop::Cancelled) => WaitOutcome::Cancelled { output },
        Err(Stop::Timeout) => WaitOutcome::TimedOut { output },
        Err(Stop::Oom) => WaitOutcome::Oom { output },
    }
}

fn join_output(
    stdout: Option<thread::JoinHandle<Vec<u8>>>,
    stderr: Option<thread::JoinHandle<Vec<u8>>>,
) -> Vec<u8> {
    let mut out = stdout.and_then(|h| h.join().ok()).unwrap_or_default();
    let mut err = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    out.append(&mut err);
    out
}

fn read_capped(mut pipe: impl Read, cap: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        match pipe.read(&mut tmp) {
            Ok(0) | Err(_) => return buf,
            Ok(n) => {
                let room = cap.saturating_sub(buf.len());
                let take = n.min(room);
                buf.extend_from_slice(&tmp[..take]);
                if buf.len() >= cap {
                    return buf;
                }
            }
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
    use std::time::Instant;

    use capability_broker::{
        ActionRequest, ApprovalChoice, ApprovalResolution, ApprovalScopeId, CanonicalAction,
        FilesystemScope, LeaseIssuer, PolicyDocument, PolicySource, PolicyStack, PrincipalRef,
        ProcessScope, ResourceDescriptor, evaluate, issue, request_approval,
    };
    use crate::backend::SandboxMount;
    use protocol::{RepoPath, SessionId};

    fn seatbelt_available() -> bool {
        cfg!(target_os = "macos") && sandbox_exec_binary().is_some()
    }

    struct TempWorkspace {
        path: PathBuf,
        host: CanonicalHostPath,
    }

    impl TempWorkspace {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("rapidlm-seatbelt-sbx-{}", protocol::RuntimeId::new()));
            fs::create_dir_all(&path).expect("temp workspace");
            let canon = fs::canonicalize(&path).expect("canonicalize");
            let host =
                CanonicalHostPath::from_resolved(canon.to_str().expect("utf8")).expect("host");
            Self { path, host }
        }

        fn mount(&self, target: &str, mode: MountMode) -> SandboxMount {
            SandboxMount::bind(self.host.clone(), RepoPath::parse(target).expect("target"), mode)
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

    fn spec(ws: &TempWorkspace) -> SandboxSpec {
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
        LeaseIssuer::from_key([0x43; 32]).expect("issuer")
    }

    fn issue_lease(capability: Capability, resource: ResourceDescriptor) -> CapabilityLease {
        let policies = if capability == Capability::ProcExec {
            proc_stack()
        } else {
            fs_stack()
        };
        let actual = CanonicalAction::Resource { capability, resource: resource.clone() };
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
        issue(&issuer(), &approved, &policies, now, &CancellationToken::new()).expect("issue")
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
    fn capabilities_land_at_host_restricted_tier_and_reject_wider_network() {
        let backend = SeatbeltBackend::new();
        let caps = backend.capabilities();
        assert_eq!(caps.tier(), SandboxTier::HostRestricted);
        assert!(!caps.network().allowlist_supported());
        assert!(!caps.network().proxy_supported());
    }

    #[test]
    fn health_reports_unavailable_off_macos_or_without_the_binary() {
        let backend = SeatbeltBackend::new();
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert_eq!(health.is_available(), seatbelt_available());
    }

    #[test]
    fn cancelled_health_is_not_a_clean_pass() {
        let backend = SeatbeltBackend::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            backend.health(&cancel).expect_err("cancelled"),
            SandboxError::Cancelled
        );
    }

    #[test]
    fn prepare_requires_a_proc_exec_lease() {
        if !seatbelt_available() {
            return;
        }
        let backend = SeatbeltBackend::new();
        let ws = TempWorkspace::new();
        let live = CancellationToken::new();
        assert_eq!(
            backend
                .prepare(&spec(&ws), &fs_lease(), &live)
                .expect_err("fs lease"),
            SandboxError::LeaseInvalid
        );
    }

    #[test]
    fn prepare_rejects_allowlist_and_proxy_network() {
        if !seatbelt_available() {
            return;
        }
        let backend = SeatbeltBackend::new();
        let ws = TempWorkspace::new();
        let with_network = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .network(SandboxNetwork::Allowlist)
            .build()
            .expect("spec");
        assert_eq!(
            backend
                .prepare(&with_network, &proc_lease(), &CancellationToken::new())
                .expect_err("network"),
            SandboxError::UnsupportedNetwork
        );
    }

    #[test]
    fn home_and_sensitive_mounts_are_rejected() {
        if !seatbelt_available() {
            return;
        }
        let backend = SeatbeltBackend::new();
        let home = CanonicalHostPath::from_resolved("/private/etc").expect("host");
        let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(SandboxMount::bind(home, cwd(), MountMode::ReadWrite).expect("mount"))
            .build()
            .expect("spec");
        assert_eq!(
            backend
                .prepare(&spec, &proc_lease(), &CancellationToken::new())
                .expect_err("forbidden"),
            SandboxError::ForbiddenMount
        );
    }

    #[cfg(unix)]
    #[test]
    fn prepare_exec_destroy_confines_writes_to_the_mounted_root() {
        if !seatbelt_available() {
            return;
        }
        let backend = SeatbeltBackend::new();
        let ws = TempWorkspace::new();
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = backend.prepare(&spec(&ws), &lease, &live).expect("prepare");

        // A write inside the mounted root succeeds.
        let inside = SandboxExecRequest::new(
            ["/bin/sh", "-c", "echo hi > inside.txt"],
            Duration::from_secs(5),
            4096,
        )
        .expect("request");
        let result = backend.exec(&handle, &inside, &lease, &live).expect("exec");
        assert_eq!(result.exit().code(), Some(0));
        // `cwd == mount target` resolves `cwd_host` to the mount's source
        // root directly (see `join_host`'s equal-path branch) — there is no
        // nested `src/` directory on the host side, matching the same
        // pattern `apps/rapid/src/sandbox_exec.rs::build_spec` already uses.
        assert!(ws.path.join("inside.txt").exists());

        // A write to a real path outside the mounted root is denied by the
        // profile itself, not by this process's own policy layer.
        let outside_target = std::env::temp_dir()
            .join(format!("rapidlm-seatbelt-outside-{}.txt", protocol::RuntimeId::new()));
        let outside = SandboxExecRequest::new(
            [
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                format!("echo hi > {}", outside_target.display()),
            ],
            Duration::from_secs(5),
            4096,
        )
        .expect("request");
        let result = backend.exec(&handle, &outside, &lease, &live).expect("exec");
        assert_ne!(result.exit().code(), Some(0));
        assert!(!outside_target.exists());

        backend.destroy(&handle, &live).expect("destroy");
        assert_eq!(
            backend.exec(&handle, &inside, &lease, &live).expect_err("gone"),
            SandboxError::UnknownHandle
        );
    }

    #[cfg(unix)]
    #[test]
    fn network_is_genuinely_denied_not_just_unrequested() {
        if !seatbelt_available() {
            return;
        }
        let backend = SeatbeltBackend::new();
        let ws = TempWorkspace::new();
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = backend.prepare(&spec(&ws), &lease, &live).expect("prepare");
        let request = SandboxExecRequest::new(
            ["/usr/bin/curl", "-s", "-m", "3", "-o", "/dev/null", "https://example.com"],
            Duration::from_secs(10),
            4096,
        )
        .expect("request");
        let result = backend.exec(&handle, &request, &lease, &live).expect("exec");
        assert_ne!(result.exit().code(), Some(0), "curl must fail with network denied");
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn timeout_and_cancellation_are_explicit_terminal_statuses() {
        if !seatbelt_available() {
            return;
        }
        let backend = SeatbeltBackend::new();
        let ws = TempWorkspace::new();
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = backend.prepare(&spec(&ws), &lease, &live).expect("prepare");

        let timed = SandboxExecRequest::new(["/bin/sleep", "5"], Duration::from_millis(150), 1024)
            .expect("timed");
        let result = backend.exec(&handle, &timed, &lease, &live).expect("timeout");
        assert_eq!(result.exit().reason(), SandboxExitReason::TimedOut);
        assert!(result.exit().timed_out());

        let cancel = CancellationToken::new();
        cancel.cancel();
        let request = SandboxExecRequest::new(["/bin/sleep", "5"], Duration::from_secs(2), 1024)
            .expect("req");
        assert_eq!(
            backend.exec(&handle, &request, &lease, &cancel).expect_err("pre-cancel"),
            SandboxError::Cancelled
        );

        backend.destroy(&handle, &live).expect("destroy");
    }

    #[cfg(unix)]
    #[test]
    fn cpu_ceiling_kills_a_command_that_exceeds_it_before_the_wall_clock_timeout() {
        if !seatbelt_available() {
            return;
        }
        let backend = SeatbeltBackend::new();
        let ws = TempWorkspace::new();
        // Explicit, low cpu_millis, matching apps/rapid/src/sandbox_exec.rs's
        // own regression test: pure shell-builtin arithmetic (no subprocess
        // per iteration — RLIMIT_CPU only counts the process it's set on,
        // not descendants it forks and waits on, so a `date`-spawning loop
        // would never trip it regardless of wall-clock duration). This
        // iteration count is measured directly on real hardware
        // (`time /bin/sh -c '...'`) at ~4 real/CPU seconds, comfortably past
        // the 1-second ceiling this test sets.
        let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .cpu_millis(1_000)
            .build()
            .expect("spec");
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");
        let request = SandboxExecRequest::new(
            ["/bin/sh", "-c", "i=0; while [ $i -lt 1500000 ]; do i=$((i+1)); done"],
            Duration::from_secs(30),
            1024,
        )
        .expect("request");
        let result = backend.exec(&handle, &request, &lease, &live).expect("exec");
        assert_ne!(result.exit().code(), Some(0), "the CPU ceiling should kill it first");
        assert!(!result.exit().timed_out(), "killed by the CPU limit, not the wall-clock timeout");
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn memory_ceiling_kills_a_command_that_exceeds_it_before_the_wall_clock_timeout() {
        if !seatbelt_available() {
            return;
        }
        let backend = SeatbeltBackend::new();
        let ws = TempWorkspace::new();
        // `/usr/bin/python3` (a stable, standard macOS system path) allocates
        // and holds a real 200 MB `bytearray` — a `dd`/`yes`-style stream
        // through a small buffer would never show up in RSS the way a held
        // allocation does, which is exactly the "measure the real thing, not
        // a proxy for it" lesson this session's own CPU-ceiling test
        // (`cpu_ceiling_kills_a_command...`) already learned the hard way.
        // 64 MB ceiling, comfortably below the 200 MB allocation.
        let spec = SandboxSpec::builder(SandboxTier::HostRestricted)
            .cwd(cwd())
            .mount(ws.mount("src", MountMode::ReadWrite))
            .memory_mb(64)
            .build()
            .expect("spec");
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = backend.prepare(&spec, &lease, &live).expect("prepare");
        let request = SandboxExecRequest::new(
            [
                "/usr/bin/python3",
                "-c",
                "import time; b = bytearray(200 * 1024 * 1024); time.sleep(30)",
            ],
            Duration::from_secs(30),
            1024,
        )
        .expect("request");
        let result = backend.exec(&handle, &request, &lease, &live).expect("exec");
        assert!(result.exit().oom(), "the memory ceiling should have killed it");
        assert!(!result.exit().timed_out(), "killed by the memory limit, not the wall-clock timeout");
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn exec_cannot_widen_timeout_or_output_beyond_the_spec() {
        if !seatbelt_available() {
            return;
        }
        let backend = SeatbeltBackend::new();
        let ws = TempWorkspace::new();
        let lease = proc_lease();
        let live = CancellationToken::new();
        let handle = backend.prepare(&spec(&ws), &lease, &live).expect("prepare");

        let wide_timeout =
            SandboxExecRequest::new(["/bin/echo", "hi"], Duration::from_secs(3_600), 1024)
                .expect("req");
        assert_eq!(
            backend.exec(&handle, &wide_timeout, &lease, &live).expect_err("timeout"),
            SandboxError::TimeoutInvalid
        );

        let wide_output =
            SandboxExecRequest::new(["/bin/echo", "hi"], Duration::from_secs(1), 64 * 1024 * 1024)
                .expect("req");
        assert_eq!(
            backend.exec(&handle, &wide_output, &lease, &live).expect_err("output"),
            SandboxError::OutputLimitInvalid
        );
        backend.destroy(&handle, &live).expect("destroy");
    }

    #[test]
    fn exec_rejects_a_lease_retargeted_to_a_different_handle() {
        if !seatbelt_available() {
            return;
        }
        let backend = SeatbeltBackend::new();
        let ws = TempWorkspace::new();
        let lease = proc_lease();
        let other = proc_lease();
        let live = CancellationToken::new();
        let handle = backend.prepare(&spec(&ws), &lease, &live).expect("prepare");
        let request = SandboxExecRequest::new(["/bin/echo", "hi"], Duration::from_secs(1), 1024)
            .expect("req");
        assert_eq!(
            backend.exec(&handle, &request, &other, &live).expect_err("retarget"),
            SandboxError::LeaseInvalid
        );
        backend.destroy(&handle, &live).expect("destroy");
    }
}
