//! Process-tree cancellation: group TERM, grace, then KILL.
//!
//! `terminate_tree` signals only the [`ProcessGroupId`] recorded at spawn.
//! It never accepts a raw PID from the caller. Process-group 0/1 are refused
//! because `kill(-1)` is a broadcast. Timeout and user-cancel stay distinct.

use std::error::Error;
use std::fmt;
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use capability_broker::CancellationToken;
use protocol::{ErrorCode, JobId};

use crate::spawn::{JobHandle, ProcessGroupId};

/// Maximum grace between the group TERM and the group KILL.
pub const MAX_GRACE: Duration = Duration::from_secs(60);

/// Default grace used by callers that do not pick a tighter bound.
pub const DEFAULT_GRACE: Duration = Duration::from_millis(200);

/// Poll stride while waiting for the leader to exit.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Bound on waiting after SIGKILL before [`CancelError::TreeStillAlive`].
const KILL_WAIT: Duration = Duration::from_secs(2);

/// Absolute `kill(1)` paths. Never PATH-search an untrusted executable.
#[cfg(unix)]
const KILL_PROGRAMS: &[&str] = &["/bin/kill", "/usr/bin/kill"];

/// Absolute `taskkill.exe` path for Windows tree termination.
#[cfg(windows)]
const TASKKILL_PROGRAM: &str = r"C:\Windows\System32\taskkill.exe";

/// Why the supervisor is tearing the tree down.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TerminationCause {
    UserCancel,
    Timeout,
}

/// Bounded wait after the first group signal, plus the terminal cause.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GracePeriod {
    duration: Duration,
    cause: TerminationCause,
}

/// One recorded group signal.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SignalKind {
    Term,
    Kill,
}

/// Recorded supervisor action. Only group signals are emitted today.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TerminateAction {
    GroupSignal(SignalKind),
}

/// Observed leader exit. Display never includes argv or output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ProcessExit {
    code: Option<i32>,
    signal: Option<i32>,
}

/// Distinguishes a natural exit from timeout and user-cancel.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TerminalStatus {
    Exited(ProcessExit),
    TimedOut { exit: ProcessExit, escalated: bool },
    Cancelled { exit: ProcessExit, escalated: bool },
}

/// Signals/actions plus the terminal status for one `terminate_tree` call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminateReport {
    job_id: JobId,
    pid: u32,
    process_group_id: ProcessGroupId,
    status: TerminalStatus,
    actions: Vec<TerminateAction>,
}

/// Typed cancellation failure. Display never echoes PIDs as attacker text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CancelError {
    GraceInvalid,
    InvalidProcessGroup,
    SignalFailed,
    WaitFailed,
    TreeStillAlive,
    UnsupportedPlatform,
}

impl GracePeriod {
    /// User/session cancel. `duration` may be zero (immediate escalate).
    pub fn user_cancel(duration: Duration) -> Result<Self, CancelError> {
        Self::new(duration, TerminationCause::UserCancel)
    }

    /// Supervisor timeout. `duration` may be zero (immediate escalate).
    pub fn timeout(duration: Duration) -> Result<Self, CancelError> {
        Self::new(duration, TerminationCause::Timeout)
    }

    pub fn duration(self) -> Duration {
        self.duration
    }

    pub fn cause(self) -> TerminationCause {
        self.cause
    }

    fn new(duration: Duration, cause: TerminationCause) -> Result<Self, CancelError> {
        if duration > MAX_GRACE {
            return Err(CancelError::GraceInvalid);
        }
        Ok(Self { duration, cause })
    }
}

impl ProcessExit {
    pub const fn code(self) -> Option<i32> {
        self.code
    }

    pub const fn signal(self) -> Option<i32> {
        self.signal
    }

    fn from_status(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            signal: exit_signal(status),
        }
    }
}

impl TerminalStatus {
    pub const fn is_timeout(self) -> bool {
        matches!(self, Self::TimedOut { .. })
    }

    pub const fn is_cancelled(self) -> bool {
        matches!(self, Self::Cancelled { .. })
    }

    pub const fn escalated(self) -> bool {
        match self {
            Self::Exited(_) => false,
            Self::TimedOut { escalated, .. } | Self::Cancelled { escalated, .. } => escalated,
        }
    }

    pub const fn exit(self) -> ProcessExit {
        match self {
            Self::Exited(exit) | Self::TimedOut { exit, .. } | Self::Cancelled { exit, .. } => exit,
        }
    }

    /// Timeout maps to [`ErrorCode::ProcessTimeout`]. User-cancel is not an
    /// [`ErrorCode`] (same convention as spawn cancel).
    pub const fn error_code(self) -> Option<ErrorCode> {
        match self {
            Self::TimedOut { .. } => Some(ErrorCode::ProcessTimeout),
            Self::Exited(_) | Self::Cancelled { .. } => None,
        }
    }

    fn from_cause(cause: TerminationCause, exit: ProcessExit, escalated: bool) -> Self {
        match cause {
            TerminationCause::Timeout => Self::TimedOut { exit, escalated },
            TerminationCause::UserCancel => Self::Cancelled { exit, escalated },
        }
    }
}

impl TerminateReport {
    pub fn job_id(&self) -> JobId {
        self.job_id
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn process_group_id(&self) -> ProcessGroupId {
        self.process_group_id
    }

    pub fn status(&self) -> TerminalStatus {
        self.status
    }

    pub fn actions(&self) -> &[TerminateAction] {
        &self.actions
    }

    fn new(job: &JobHandle, status: TerminalStatus, actions: Vec<TerminateAction>) -> Self {
        Self {
            job_id: job.job_id(),
            pid: job.pid(),
            process_group_id: job.process_group_id(),
            status,
            actions,
        }
    }
}

/// Terminate the supervised process group, then escalate after `grace`.
///
/// If the leader has already been reaped, no signal is sent (PID reuse).
pub fn terminate_tree(
    job: &mut JobHandle,
    grace: GracePeriod,
) -> Result<TerminateReport, CancelError> {
    validate_pgid_raw(job.process_group_id().as_u32())?;
    if let Some(status) = child_exit(job)? {
        return Ok(TerminateReport::new(
            job,
            TerminalStatus::Exited(ProcessExit::from_status(status)),
            Vec::new(),
        ));
    }

    let mut actions = Vec::with_capacity(2);
    signal_group(job.process_group_id(), SignalKind::Term)?;
    actions.push(TerminateAction::GroupSignal(SignalKind::Term));

    if let Some(status) = wait_leader(job, grace.duration())? {
        return Ok(TerminateReport::new(
            job,
            TerminalStatus::from_cause(grace.cause(), ProcessExit::from_status(status), false),
            actions,
        ));
    }

    signal_group(job.process_group_id(), SignalKind::Kill)?;
    actions.push(TerminateAction::GroupSignal(SignalKind::Kill));
    match wait_leader(job, KILL_WAIT)? {
        Some(status) => Ok(TerminateReport::new(
            job,
            TerminalStatus::from_cause(grace.cause(), ProcessExit::from_status(status), true),
            actions,
        )),
        None => Err(CancelError::TreeStillAlive),
    }
}

/// Wait for a natural exit, or terminate on user-cancel / spec timeout.
///
/// A cancelled token and an expired `JobHandle::timeout` produce different
/// [`TerminalStatus`] values. An already-dead leader is reported as `Exited`
/// and is not signaled.
pub fn await_exit(
    job: &mut JobHandle,
    cancel: &CancellationToken,
    grace: Duration,
) -> Result<TerminateReport, CancelError> {
    let _ = GracePeriod::user_cancel(grace)?;
    let deadline = job.timeout().map(|limit| job.started_at() + limit);

    loop {
        if let Some(status) = child_exit(job)? {
            return Ok(TerminateReport::new(
                job,
                TerminalStatus::Exited(ProcessExit::from_status(status)),
                Vec::new(),
            ));
        }
        if cancel.is_cancelled() {
            return terminate_tree(job, GracePeriod::user_cancel(grace)?);
        }
        if let Some(deadline) = deadline
            && Instant::now() >= deadline {
                return terminate_tree(job, GracePeriod::timeout(grace)?);
            }

        let slice = match deadline {
            Some(deadline) => POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            None => POLL_INTERVAL,
        };
        if !slice.is_zero() {
            thread::sleep(slice);
        }
    }
}

/// Bounded capture of one drained stream. `truncated` means the child wrote
/// more than `cap` bytes; the excess was read and discarded, never stored.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DrainedStream {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// [`await_exit`], but concurrently draining stdout/stderr on background
/// threads instead of leaving them unread until the child exits.
///
/// `await_exit` alone only polls [`Child::try_wait`] — it never reads the
/// child's pipes. A child that writes more than the OS pipe buffer (commonly
/// 16-64 KiB, varies by platform) before exiting blocks in `write(2)` once
/// that buffer fills, so it never reaches exit; the poll loop then reports a
/// timeout even though the child had already finished useful work. Draining
/// must run for the whole wait, not just until `cap` is reached: a reader
/// that stops once truncated would let the pipe fill again and reintroduce
/// the exact same deadlock for any output past the cap.
///
/// Must be called before anything else reads or takes the child's stdout/
/// stderr — this function takes both pipes itself.
///
/// [`Child::try_wait`]: std::process::Child::try_wait
pub fn await_exit_draining(
    job: &mut JobHandle,
    cancel: &CancellationToken,
    grace: Duration,
    cap: usize,
) -> Result<(TerminateReport, DrainedStream, DrainedStream), CancelError> {
    let stdout = job.child_mut().stdout.take();
    let stderr = job.child_mut().stderr.take();
    let stdout_reader = stdout.map(|pipe| thread::spawn(move || drain_capped(pipe, cap)));
    let stderr_reader = stderr.map(|pipe| thread::spawn(move || drain_capped(pipe, cap)));
    let report = await_exit(job, cancel, grace)?;
    let join = |reader: Option<thread::JoinHandle<DrainedStream>>| {
        reader.and_then(|handle| handle.join().ok()).unwrap_or_default()
    };
    Ok((report, join(stdout_reader), join(stderr_reader)))
}

/// Reads `pipe` to EOF, keeping only the first `cap` bytes. Never stops
/// early on overflow: the caller relies on this to keep draining so the
/// child is never blocked on a full pipe, no matter how much it writes.
fn drain_capped<R: Read>(mut pipe: R, cap: usize) -> DrainedStream {
    let mut buf = [0u8; 8192];
    let mut out = Vec::new();
    let mut truncated = false;
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(out.len());
                if room > 0 {
                    let take = room.min(n);
                    out.extend_from_slice(&buf[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
        }
    }
    DrainedStream {
        bytes: out,
        truncated,
    }
}

fn child_exit(job: &mut JobHandle) -> Result<Option<ExitStatus>, CancelError> {
    job.child_mut()
        .try_wait()
        .map_err(|_| CancelError::WaitFailed)
}

fn wait_leader(job: &mut JobHandle, budget: Duration) -> Result<Option<ExitStatus>, CancelError> {
    if budget.is_zero() {
        return child_exit(job);
    }
    let deadline = Instant::now() + budget;
    loop {
        if let Some(status) = child_exit(job)? {
            return Ok(Some(status));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

fn validate_pgid_raw(pgid: u32) -> Result<(), CancelError> {
    if pgid < 2 {
        Err(CancelError::InvalidProcessGroup)
    } else {
        Ok(())
    }
}

/// Send one group signal directly, bypassing `terminate_tree`'s
/// wait/escalate ceremony — for a caller that already knows it wants an
/// immediate, best-effort kill (e.g. `spawn`'s own stdin-write-failure
/// cleanup, which needs to reap a process that will never be handed back
/// as a usable `JobHandle`).
pub(crate) fn signal_group(pgid: ProcessGroupId, kind: SignalKind) -> Result<(), CancelError> {
    validate_pgid_raw(pgid.as_u32())?;
    platform_signal_group(pgid, kind)
}

#[cfg(unix)]
fn platform_signal_group(pgid: ProcessGroupId, kind: SignalKind) -> Result<(), CancelError> {
    let flag = match kind {
        SignalKind::Term => "-TERM",
        SignalKind::Kill => "-KILL",
    };
    // Negative pid is the process-group form of kill(1). The magnitude is
    // the recorded group id, never attacker text.
    let target = format!("-{}", pgid.as_u32());
    let program = kill_program()?;
    let status = Command::new(program)
        .args([flag, target.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .status()
        .map_err(|_| CancelError::SignalFailed)?;
    if status.success() || process_absent(status) {
        Ok(())
    } else {
        Err(CancelError::SignalFailed)
    }
}

#[cfg(windows)]
fn platform_signal_group(pgid: ProcessGroupId, kind: SignalKind) -> Result<(), CancelError> {
    // Spawn recorded the leader pid as the group id (CREATE_NEW_PROCESS_GROUP).
    // `/T` terminates descendants; `/F` is the hard-kill escalation.
    let pid = pgid.as_u32().to_string();
    let mut command = Command::new(TASKKILL_PROGRAM);
    command.args(["/PID", &pid, "/T"]);
    if matches!(kind, SignalKind::Kill) {
        command.arg("/F");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear();
    let status = command.status().map_err(|_| CancelError::SignalFailed)?;
    if status.success() || process_absent(status) {
        Ok(())
    } else {
        Err(CancelError::SignalFailed)
    }
}

#[cfg(not(any(unix, windows)))]
fn platform_signal_group(_pgid: ProcessGroupId, _kind: SignalKind) -> Result<(), CancelError> {
    Err(CancelError::UnsupportedPlatform)
}

#[cfg(unix)]
fn kill_program() -> Result<&'static str, CancelError> {
    KILL_PROGRAMS
        .iter()
        .copied()
        .find(|path| std::path::Path::new(path).is_file())
        .ok_or(CancelError::SignalFailed)
}

fn process_absent(status: ExitStatus) -> bool {
    // kill(1) uses 1 for ESRCH on BSD/GNU; taskkill uses 128 for not found.
    matches!(status.code(), Some(1) | Some(128))
}

#[cfg(unix)]
fn exit_signal(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: ExitStatus) -> Option<i32> {
    None
}

impl CancelError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GraceInvalid => "process grace period exceeds bound",
            Self::InvalidProcessGroup => "process group is not a supervised identity",
            Self::SignalFailed => "process group signal failed",
            Self::WaitFailed => "process wait failed",
            Self::TreeStillAlive => "process tree still alive after kill",
            Self::UnsupportedPlatform => "process tree cancel is unsupported on this platform",
        }
    }

    pub const fn error_code(self) -> Option<ErrorCode> {
        match self {
            Self::GraceInvalid | Self::InvalidProcessGroup => Some(ErrorCode::ToolInvalidArguments),
            Self::SignalFailed | Self::WaitFailed | Self::TreeStillAlive => {
                Some(ErrorCode::InternalUnexpected)
            }
            Self::UnsupportedPlatform => Some(ErrorCode::SandboxTierUnavailable),
        }
    }
}

impl fmt::Display for CancelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for CancelError {}

impl fmt::Display for TerminalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exited(exit) => write!(f, "exited code={:?} signal={:?}", exit.code, exit.signal),
            Self::TimedOut { exit, escalated } => write!(
                f,
                "timed out escalated={escalated} code={:?} signal={:?}",
                exit.code, exit.signal
            ),
            Self::Cancelled { exit, escalated } => write!(
                f,
                "cancelled escalated={escalated} code={:?} signal={:?}",
                exit.code, exit.signal
            ),
        }
    }
}

impl fmt::Display for TerminateReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "job={} pid={} pgid={} status={} actions={}",
            self.job_id,
            self.pid,
            self.process_group_id,
            self.status,
            self.actions.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use capability_broker::{
        evaluate, issue, request_approval, validate_use, ActionRequest, ApprovalChoice,
        ApprovalResolution, ApprovalScopeId, CanonicalAction, CapabilityLease, LeaseIssuer,
        LeaseUseGuard, LeaseValidator, PolicyDocument, PolicyRevision, PolicySource, PolicyStack,
        PrincipalRef,
    };
    use protocol::SessionId;

    use crate::spawn::{spawn, ExecBinding, ExecSpec, SecretOrValue, StdinSpec};

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-7c1e9b";

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

[[rules]]
id = "proc-shell-allow"
effect = "allow"
subjects = ["*"]
capability = "proc.exec"
resource = { command_family = "shell" }
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

    fn issuer() -> LeaseIssuer {
        LeaseIssuer::from_key([0x11; 32]).expect("issuer")
    }

    fn approve_issue(
        request: &ActionRequest,
        policies: &PolicyStack,
        now: Instant,
    ) -> CapabilityLease {
        let decision = evaluate(policies, request, &CancellationToken::new()).expect("evaluate");
        let approval =
            request_approval(request, &decision, now, &CancellationToken::new()).expect("approval");
        let approved = match approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                request,
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
            policies,
            now,
            &CancellationToken::new(),
        )
        .expect("issue")
    }

    fn lease_guard(spec: &ExecSpec) -> LeaseUseGuard {
        let binding = spec.binding().expect("bound spec");
        let command = spec_command(spec);
        let actual = CanonicalAction::Command(command);
        let request = ActionRequest::new(
            binding.principal().clone(),
            binding.session_id(),
            binding.capability(),
            binding.resource().clone(),
            actual.clone(),
            "exec",
        )
        .expect("request");
        let now = Instant::now();
        let lease = approve_issue(&request, &proc_stack(), now);
        let validator = LeaseValidator::new(issuer(), PolicyRevision::of_stack(&proc_stack()));
        validate_use(&validator, &lease, &actual, now, &CancellationToken::new()).expect("guard")
    }

    fn spec_command(spec: &ExecSpec) -> capability_broker::CanonicalCommand {
        use capability_broker::{normalize_exec, ExecIntent, Resolver};

        struct FrozenPathResolver;
        impl Resolver for FrozenPathResolver {
            fn resolve_cwd(
                &self,
                requested: &str,
            ) -> Result<
                capability_broker::CanonicalHostPath,
                capability_broker::CommandNormalizeError,
            > {
                capability_broker::CanonicalHostPath::from_resolved(requested)
            }

            fn resolve_executable(
                &self,
                requested: &str,
                _cwd: &capability_broker::CanonicalHostPath,
            ) -> Result<
                capability_broker::CanonicalHostPath,
                capability_broker::CommandNormalizeError,
            > {
                capability_broker::CanonicalHostPath::from_resolved(requested)
                    .map_err(|_| capability_broker::CommandNormalizeError::UnresolvedExecutable)
            }
        }

        let env_names = spec.env().keys().cloned();
        let intent = match spec.invocation() {
            crate::spawn::Invocation::Argv { argv } => {
                ExecIntent::argv(argv.clone(), spec.cwd().as_str().to_owned(), env_names)
            }
            crate::spawn::Invocation::Shell { shell, script } => ExecIntent::shell(
                shell.clone(),
                script.clone(),
                spec.cwd().as_str().to_owned(),
                env_names,
            ),
        };
        normalize_exec(&intent, &FrozenPathResolver, spec.cancel()).expect("canon")
    }

    fn temp_cwd() -> capability_broker::CanonicalHostPath {
        let tmp = std::env::temp_dir().canonicalize().expect("temp");
        let rendered = tmp.to_str().expect("utf8 temp").replace('\\', "/");
        capability_broker::CanonicalHostPath::from_resolved(&rendered).expect("cwd")
    }

    fn require_bin(path: &str) -> String {
        assert!(
            Path::new(path).is_file(),
            "missing test fixture binary {path}"
        );
        path.to_owned()
    }

    fn test_binding() -> ExecBinding {
        ExecBinding::proc_exec(principal(), SessionId::new(), "test").expect("binding")
    }

    fn argv_spec(argv: &[&str], timeout: Option<Duration>) -> ExecSpec {
        ExecSpec::argv(
            argv.iter().copied(),
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Empty,
            timeout,
            4096,
            CancellationToken::new(),
        )
        .expect("spec")
        .bind(test_binding())
        .expect("bind")
    }

    fn spawn_argv(argv: &[&str], timeout: Option<Duration>) -> JobHandle {
        let spec = argv_spec(argv, timeout);
        spawn(spec.clone(), lease_guard(&spec)).expect("spawn")
    }

    fn unique_pid_file(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rlm-cancel-{tag}-{}-{nanos}.pid",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        path
    }

    fn wait_pid_file(path: &Path, budget: Duration) -> u32 {
        let deadline = Instant::now() + budget;
        loop {
            if let Ok(text) = fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse::<u32>()
                    && pid >= 2 {
                        return pid;
                    }
            if Instant::now() >= deadline {
                panic!("pid file {} was not written", path.display());
            }
            thread::sleep(Duration::from_millis(10));
        }
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
    fn query_pgid(pid: u32) -> u32 {
        let output = Command::new("/bin/ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()
            .expect("ps");
        assert!(output.status.success(), "ps failed");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("pgid")
    }

    #[test]
    fn grace_above_max_fails_closed() {
        let err = GracePeriod::timeout(MAX_GRACE + Duration::from_millis(1)).expect_err("bound");
        assert_eq!(err, CancelError::GraceInvalid);
        assert_eq!(err.error_code(), Some(ErrorCode::ToolInvalidArguments));
        assert!(!err.to_string().contains(CANARY));
    }

    #[test]
    fn timeout_and_user_cancel_statuses_are_distinct() {
        let exit = ProcessExit {
            code: None,
            signal: Some(15),
        };
        let timeout = TerminalStatus::TimedOut {
            exit,
            escalated: false,
        };
        let cancel = TerminalStatus::Cancelled {
            exit,
            escalated: false,
        };
        assert_ne!(timeout, cancel);
        assert!(timeout.is_timeout());
        assert!(!timeout.is_cancelled());
        assert!(cancel.is_cancelled());
        assert!(!cancel.is_timeout());
        assert_eq!(timeout.error_code(), Some(ErrorCode::ProcessTimeout));
        assert_eq!(cancel.error_code(), None);
        let timeout_text = timeout.to_string();
        let cancel_text = cancel.to_string();
        assert!(timeout_text.contains("timed out"));
        assert!(cancel_text.contains("cancelled"));
        assert!(!timeout_text.contains(CANARY));
        assert!(!cancel_text.contains(CANARY));
    }

    #[test]
    fn broadcast_process_groups_are_rejected() {
        assert_eq!(validate_pgid_raw(0), Err(CancelError::InvalidProcessGroup));
        assert_eq!(validate_pgid_raw(1), Err(CancelError::InvalidProcessGroup));
        assert!(validate_pgid_raw(2).is_ok());
    }

    #[test]
    fn cancel_error_display_does_not_echo_canary() {
        for err in [
            CancelError::GraceInvalid,
            CancelError::InvalidProcessGroup,
            CancelError::SignalFailed,
            CancelError::WaitFailed,
            CancelError::TreeStillAlive,
            CancelError::UnsupportedPlatform,
        ] {
            let text = format!("{err:?} {err}");
            assert!(!text.contains(CANARY));
            assert!(!text.contains("/etc/passwd"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn grandchild_fixture_is_terminated_with_group_semantics() {
        let sh = require_bin("/bin/sh");
        let sleep = require_bin("/bin/sleep");
        let pid_file = unique_pid_file("grand");
        let pid_path = pid_file.to_str().expect("utf8 pid path").replace('\\', "/");
        let script = format!("{sleep} 30 & echo $! > {pid_path}; exec {sleep} 30");
        let mut handle = spawn_argv(&[&sh, "-c", &script], None);
        let grandchild = wait_pid_file(&pid_file, Duration::from_secs(2));
        assert_eq!(query_pgid(handle.pid()), handle.process_group_id().as_u32());
        assert_eq!(query_pgid(grandchild), handle.process_group_id().as_u32());
        assert_ne!(grandchild, handle.pid());
        assert!(pid_alive(grandchild));

        let report = terminate_tree(
            &mut handle,
            GracePeriod::user_cancel(Duration::from_millis(150)).expect("grace"),
        )
        .expect("terminate");
        assert!(report.status().is_cancelled());
        assert!(!report.status().is_timeout());
        assert_eq!(report.status().error_code(), None);
        assert!(!report.actions().is_empty());
        assert!(!pid_alive(handle.pid()));
        assert!(
            !pid_alive(grandchild),
            "grandchild {} survived group cancel",
            grandchild
        );
        let _ = fs::remove_file(&pid_file);
    }

    #[cfg(unix)]
    #[test]
    fn term_ignored_escalates_after_grace() {
        let sh = require_bin("/bin/sh");
        let sleep = require_bin("/bin/sleep");
        let pid_file = unique_pid_file("trap");
        let pid_path = pid_file.to_str().expect("utf8 pid path").replace('\\', "/");
        let script = format!(
            "trap \"\" TERM; {sh} -c 'trap \"\" TERM; while :; do {sleep} 10; done' & echo $! > {pid_path}; while :; do {sleep} 10; done"
        );
        let mut handle = spawn_argv(&[&sh, "-c", &script], None);
        let grandchild = wait_pid_file(&pid_file, Duration::from_secs(2));
        assert_eq!(query_pgid(grandchild), handle.process_group_id().as_u32());

        let report = terminate_tree(
            &mut handle,
            GracePeriod::timeout(Duration::from_millis(80)).expect("grace"),
        )
        .expect("terminate");
        assert!(report.status().is_timeout());
        assert!(!report.status().is_cancelled());
        assert_eq!(
            report.status().error_code(),
            Some(ErrorCode::ProcessTimeout)
        );
        assert!(report.status().escalated());
        assert_eq!(
            report.actions(),
            &[
                TerminateAction::GroupSignal(SignalKind::Term),
                TerminateAction::GroupSignal(SignalKind::Kill),
            ]
        );
        assert!(!pid_alive(handle.pid()));
        assert!(!pid_alive(grandchild));
        let _ = fs::remove_file(&pid_file);
    }

    #[cfg(unix)]
    #[test]
    fn await_exit_distinguishes_timeout_from_user_cancel() {
        let sleep = require_bin("/bin/sleep");
        let mut timed = spawn_argv(&[&sleep, "30"], Some(Duration::from_millis(80)));
        let timeout_report = await_exit(
            &mut timed,
            &CancellationToken::new(),
            Duration::from_millis(150),
        )
        .expect("timeout");
        assert!(timeout_report.status().is_timeout());
        assert!(!timeout_report.status().is_cancelled());
        assert_eq!(
            timeout_report.status().error_code(),
            Some(ErrorCode::ProcessTimeout)
        );

        let cancel = CancellationToken::new();
        let mut cancelled = spawn_argv(&[&sleep, "30"], None);
        cancel.cancel();
        let cancel_report =
            await_exit(&mut cancelled, &cancel, Duration::from_millis(150)).expect("cancel");
        assert!(cancel_report.status().is_cancelled());
        assert!(!cancel_report.status().is_timeout());
        assert_eq!(cancel_report.status().error_code(), None);
        assert_ne!(timeout_report.status(), cancel_report.status());
    }

    #[cfg(unix)]
    #[test]
    fn unrelated_process_is_not_signaled() {
        let sleep = require_bin("/bin/sleep");
        let mut outsider = Command::new(&sleep)
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("outsider");
        let outsider_pid = outsider.id();
        let mut handle = spawn_argv(&[&sleep, "30"], None);
        assert_ne!(query_pgid(outsider_pid), handle.process_group_id().as_u32());

        terminate_tree(
            &mut handle,
            GracePeriod::user_cancel(Duration::from_millis(150)).expect("grace"),
        )
        .expect("terminate");
        assert!(
            pid_alive(outsider_pid),
            "cancel signaled a process outside the supervised group"
        );
        assert!(!pid_alive(handle.pid()));
        let _ = outsider.kill();
        let _ = outsider.wait();
    }

    #[cfg(unix)]
    #[test]
    fn already_reaped_leader_is_not_signaled_again() {
        let echo = require_bin("/bin/echo");
        let mut handle = spawn_argv(&[&echo, "ok"], None);
        let first = await_exit(&mut handle, &CancellationToken::new(), DEFAULT_GRACE)
            .expect("natural exit");
        assert!(matches!(first.status(), TerminalStatus::Exited(_)));
        assert!(first.actions().is_empty());

        let second = terminate_tree(
            &mut handle,
            GracePeriod::timeout(Duration::from_millis(50)).expect("grace"),
        )
        .expect("second");
        assert!(matches!(second.status(), TerminalStatus::Exited(_)));
        assert!(
            second.actions().is_empty(),
            "re-signaling a reaped group risks PID reuse"
        );
    }

    /// dd's output comfortably exceeds every common OS pipe buffer size
    /// (typically 16-64 KiB), so a reader that never drains it blocks the
    /// child in `write(2)` well before it can exit on its own.
    #[cfg(unix)]
    fn big_output_script() -> Vec<String> {
        let sh = require_bin("/bin/sh");
        let dd = require_bin("/bin/dd");
        vec![
            sh,
            "-c".to_owned(),
            format!("{dd} if=/dev/zero bs=1024 count=200 2>/dev/null"),
        ]
    }

    #[cfg(unix)]
    #[test]
    fn plain_await_exit_deadlocks_on_output_past_the_pipe_buffer() {
        let script = big_output_script();
        let argv: Vec<&str> = script.iter().map(String::as_str).collect();
        let mut handle = spawn_argv(&argv, Some(Duration::from_millis(300)));
        let report = await_exit(&mut handle, &CancellationToken::new(), DEFAULT_GRACE)
            .expect("await");
        assert!(
            report.status().is_timeout(),
            "a child blocked writing to an undrained pipe must be misreported as timed out \
             by a wait loop that never reads its stdout: status was {:?}",
            report.status()
        );
    }

    #[cfg(unix)]
    #[test]
    fn await_exit_draining_reads_output_past_the_pipe_buffer_without_deadlock() {
        let script = big_output_script();
        let argv: Vec<&str> = script.iter().map(String::as_str).collect();
        let mut handle = spawn_argv(&argv, Some(Duration::from_secs(5)));
        let (report, stdout, stderr) = await_exit_draining(
            &mut handle,
            &CancellationToken::new(),
            DEFAULT_GRACE,
            1024 * 1024,
        )
        .expect("await draining");
        assert!(
            !report.status().is_timeout(),
            "draining concurrently with the wait must let the child actually exit: status was {:?}",
            report.status()
        );
        assert!(matches!(report.status(), TerminalStatus::Exited(_)));
        assert_eq!(stdout.bytes.len(), 200 * 1024);
        assert!(stdout.bytes.iter().all(|b| *b == 0));
        assert!(!stdout.truncated);
        assert!(stderr.bytes.is_empty());
        assert!(!stderr.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn await_exit_draining_truncates_at_cap_but_still_drains_to_avoid_deadlock() {
        let script = big_output_script();
        let argv: Vec<&str> = script.iter().map(String::as_str).collect();
        let mut handle = spawn_argv(&argv, Some(Duration::from_secs(5)));
        let (report, stdout, _stderr) =
            await_exit_draining(&mut handle, &CancellationToken::new(), DEFAULT_GRACE, 64)
                .expect("await draining");
        assert!(
            !report.status().is_timeout(),
            "truncating at a small cap must not stop the drain thread from reading to EOF, \
             or the child would block on the pipe exactly as before: status was {:?}",
            report.status()
        );
        assert_eq!(stdout.bytes.len(), 64);
        assert!(stdout.truncated);
    }
}
