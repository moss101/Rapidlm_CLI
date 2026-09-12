//! Orphan process reconciliation after supervisor/daemon restart.
//!
//! Startup compares each persisted [`ProcessIdentity`] against an independently
//! observed pid + process-group + start-time triple. PID alone never authorizes
//! a signal. Unknown ownership is a blocked warning, not a clean pass.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use capability_broker::CancellationToken;
use protocol::{ErrorCode, JobId};

use crate::jobs::{JobLifetime, JobRegistry, MAX_LIVE_JOBS, ProcessIdentity, RunningTombstone};

/// Observed kernel start may be slightly after the recorded stamp (etime rounding).
pub const START_AHEAD_SLACK_MS: u64 = 2_000;

/// Recorded stamp may lag the kernel start (spawn, then persist).
pub const MAX_SPAWN_RECORD_SKEW_MS: u64 = 5_000;

/// Bound on waiting after SIGTERM before escalating a reconciled tree.
pub const RECOVERY_GRACE: Duration = Duration::from_millis(200);

/// Bound on waiting after SIGKILL before reporting the tree still alive.
const KILL_WAIT: Duration = Duration::from_secs(2);

const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Hard ceiling for a `ps` observation payload. Used only by the
/// `cfg(unix)` observation paths, and gated with them.
#[cfg(unix)]
const MAX_PS_OUTPUT_BYTES: usize = 4 * 1024;

/// Absolute `kill(1)` paths. Never PATH-search an untrusted executable.
#[cfg(unix)]
const KILL_PROGRAMS: &[&str] = &["/bin/kill", "/usr/bin/kill"];

/// Absolute `ps(1)` paths used only to read pid/pgid/etime.
#[cfg(unix)]
const PS_PROGRAMS: &[&str] = &["/bin/ps", "/usr/bin/ps"];

/// Absolute `taskkill.exe` path for Windows tree termination.
#[cfg(windows)]
const TASKKILL_PROGRAM: &str = r"C:\Windows\System32\taskkill.exe";

/// A persisted running job presented to the reconciler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrphanJob {
    job_id: JobId,
    lifetime: JobLifetime,
    identity: ProcessIdentity,
}

/// Independently observed OS state for one recorded PID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessObservation {
    /// No process exists at the recorded PID.
    Absent,
    /// A process exists. Missing fields are unread, not wildcards.
    Present {
        pid: u32,
        process_group_id: Option<u32>,
        started_unix_ms: Option<u64>,
    },
    /// Liveness or identity could not be determined.
    Unreadable,
}

/// Provenance of a persisted child relative to the live process table.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Ownership {
    Dead,
    Owned,
    Reused,
    Unknown,
}

/// What to do when ownership is proven.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ReconcilePolicy {
    TerminateOwned,
    ReadoptOwned,
}

/// Why reconciliation refused to act on a live PID.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RecoveryWarning {
    UnknownOwnership,
    PidReuse,
}

/// Action taken (or refused) for one persisted identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ReconcileDecision {
    Orphaned,
    Terminated,
    Readopted,
    Blocked { warning: RecoveryWarning },
}

/// Per-job classification and decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconcileOutcome {
    job_id: JobId,
    lifetime: JobLifetime,
    recorded: ProcessIdentity,
    ownership: Ownership,
    decision: ReconcileDecision,
}

/// Startup reconciliation of every running tombstone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconcileReport {
    outcomes: Vec<ReconcileOutcome>,
}

/// Typed recovery failure. Display never echoes argv, script, or output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RecoveryError {
    Cancelled,
    InvalidIdentity,
    TooManyJobs,
    SignalFailed,
    TreeStillAlive,
    UnsupportedPlatform,
}

/// Read pid + group + start-time without trusting the recorded triple.
pub trait ProcessProbe {
    fn observe(
        &self,
        recorded: ProcessIdentity,
        cancel: &CancellationToken,
    ) -> Result<ProcessObservation, RecoveryError>;
}

/// Signal a process group only after ownership has been proven.
pub trait ProcessTreeKiller {
    fn terminate_owned(
        &self,
        identity: ProcessIdentity,
        cancel: &CancellationToken,
    ) -> Result<(), RecoveryError>;
}

/// Host `/proc` + `ps` identity reader.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostProcessProbe;

/// Host process-group TERM then KILL.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostProcessKiller;

impl OrphanJob {
    pub fn new(
        job_id: JobId,
        lifetime: JobLifetime,
        identity: ProcessIdentity,
    ) -> Result<Self, RecoveryError> {
        if identity.pid() < 2 || identity.process_group_id() < 2 {
            return Err(RecoveryError::InvalidIdentity);
        }
        Ok(Self {
            job_id,
            lifetime,
            identity,
        })
    }

    pub fn job_id(self) -> JobId {
        self.job_id
    }

    pub fn lifetime(self) -> JobLifetime {
        self.lifetime
    }

    pub fn identity(self) -> ProcessIdentity {
        self.identity
    }
}

impl From<&RunningTombstone> for OrphanJob {
    fn from(tombstone: &RunningTombstone) -> Self {
        Self {
            job_id: tombstone.job_id(),
            lifetime: tombstone.lifetime(),
            identity: tombstone.identity(),
        }
    }
}

impl ReconcilePolicy {
    /// Client jobs are torn down after a crash. Daemon jobs re-adopt when the
    /// persisted identity still names the live process (reattach token).
    pub const fn for_lifetime(lifetime: JobLifetime) -> Self {
        match lifetime {
            JobLifetime::Client => Self::TerminateOwned,
            JobLifetime::Daemon => Self::ReadoptOwned,
        }
    }
}

impl Ownership {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dead => "dead",
            Self::Owned => "owned",
            Self::Reused => "reused",
            Self::Unknown => "unknown",
        }
    }
}

impl RecoveryWarning {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownOwnership => "unknown_ownership",
            Self::PidReuse => "pid_reuse",
        }
    }
}

impl ReconcileDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Orphaned => "orphaned",
            Self::Terminated => "terminated",
            Self::Readopted => "readopted",
            Self::Blocked { warning } => match warning {
                RecoveryWarning::UnknownOwnership => "blocked_unknown_ownership",
                RecoveryWarning::PidReuse => "blocked_pid_reuse",
            },
        }
    }

    pub const fn is_blocked(self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    pub const fn warning(self) -> Option<RecoveryWarning> {
        match self {
            Self::Blocked { warning } => Some(warning),
            Self::Orphaned | Self::Terminated | Self::Readopted => None,
        }
    }
}

impl ReconcileOutcome {
    pub fn job_id(self) -> JobId {
        self.job_id
    }

    pub fn lifetime(self) -> JobLifetime {
        self.lifetime
    }

    pub fn recorded(self) -> ProcessIdentity {
        self.recorded
    }

    pub fn ownership(self) -> Ownership {
        self.ownership
    }

    pub fn decision(self) -> ReconcileDecision {
        self.decision
    }
}

impl ReconcileReport {
    pub fn outcomes(&self) -> &[ReconcileOutcome] {
        &self.outcomes
    }

    pub fn is_empty(&self) -> bool {
        self.outcomes.is_empty()
    }

    pub fn blocked(&self) -> impl Iterator<Item = &ReconcileOutcome> {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.decision.is_blocked())
    }

    pub fn killed(&self) -> impl Iterator<Item = ProcessIdentity> + '_ {
        self.outcomes.iter().filter_map(|outcome| {
            matches!(outcome.decision, ReconcileDecision::Terminated).then_some(outcome.recorded)
        })
    }
}

/// Classify one observation. Missing group or start-time is unknown, not owned.
pub fn classify_identity(recorded: ProcessIdentity, observed: ProcessObservation) -> Ownership {
    if recorded.pid() < 2 || recorded.process_group_id() < 2 {
        return Ownership::Unknown;
    }
    match observed {
        ProcessObservation::Absent => Ownership::Dead,
        ProcessObservation::Unreadable => Ownership::Unknown,
        ProcessObservation::Present {
            pid,
            process_group_id,
            started_unix_ms,
        } => {
            if pid != recorded.pid() {
                return Ownership::Unknown;
            }
            let Some(pgid) = process_group_id else {
                return Ownership::Unknown;
            };
            let Some(started) = started_unix_ms else {
                return Ownership::Unknown;
            };
            if pgid < 2 {
                return Ownership::Unknown;
            }
            if pgid != recorded.process_group_id() {
                return Ownership::Reused;
            }
            if start_times_compatible(recorded.started_unix_ms(), started) {
                Ownership::Owned
            } else {
                Ownership::Reused
            }
        }
    }
}

/// Map ownership and policy to a decision. Reuse and unknown never terminate.
pub fn decide_action(ownership: Ownership, policy: ReconcilePolicy) -> ReconcileDecision {
    match ownership {
        Ownership::Dead => ReconcileDecision::Orphaned,
        Ownership::Owned => match policy {
            ReconcilePolicy::TerminateOwned => ReconcileDecision::Terminated,
            ReconcilePolicy::ReadoptOwned => ReconcileDecision::Readopted,
        },
        Ownership::Reused => ReconcileDecision::Blocked {
            warning: RecoveryWarning::PidReuse,
        },
        Ownership::Unknown => ReconcileDecision::Blocked {
            warning: RecoveryWarning::UnknownOwnership,
        },
    }
}

/// Reconcile persisted running identities. Signals only proven-owned trees.
pub fn reconcile_orphans<P, K>(
    jobs: &[OrphanJob],
    probe: &P,
    killer: &K,
    cancel: &CancellationToken,
) -> Result<ReconcileReport, RecoveryError>
where
    P: ProcessProbe,
    K: ProcessTreeKiller,
{
    check_cancel(cancel)?;
    if jobs.len() > MAX_LIVE_JOBS {
        return Err(RecoveryError::TooManyJobs);
    }
    for job in jobs {
        if job.identity.pid() < 2 || job.identity.process_group_id() < 2 {
            return Err(RecoveryError::InvalidIdentity);
        }
    }

    let mut classified = Vec::with_capacity(jobs.len());
    for job in jobs {
        check_cancel(cancel)?;
        let observed = probe.observe(job.identity, cancel)?;
        let ownership = classify_identity(job.identity, observed);
        classified.push((job, ownership));
    }

    let ownerships = downgrade_pid_collisions(&classified);
    let mut outcomes = Vec::with_capacity(jobs.len());
    for (job, ownership) in jobs.iter().zip(ownerships) {
        check_cancel(cancel)?;
        let policy = ReconcilePolicy::for_lifetime(job.lifetime);
        let mut decision = decide_action(ownership, policy);
        if decision == ReconcileDecision::Terminated {
            // Re-check immediately before the signal (PID-reuse TOCTOU).
            let again = classify_identity(job.identity, probe.observe(job.identity, cancel)?);
            if again != Ownership::Owned {
                let warning = if again == Ownership::Reused {
                    RecoveryWarning::PidReuse
                } else {
                    RecoveryWarning::UnknownOwnership
                };
                decision = ReconcileDecision::Blocked { warning };
            } else {
                killer.terminate_owned(job.identity, cancel)?;
            }
        }
        outcomes.push(ReconcileOutcome {
            job_id: job.job_id,
            lifetime: job.lifetime,
            recorded: job.identity,
            ownership,
            decision,
        });
    }
    Ok(ReconcileReport { outcomes })
}

/// Startup entry: every running tombstone in `registry`.
pub fn reconcile_registry<P, K>(
    registry: &JobRegistry,
    probe: &P,
    killer: &K,
    cancel: &CancellationToken,
) -> Result<ReconcileReport, RecoveryError>
where
    P: ProcessProbe,
    K: ProcessTreeKiller,
{
    let tombstones = registry.running_tombstones();
    let jobs: Vec<OrphanJob> = tombstones.iter().map(OrphanJob::from).collect();
    reconcile_orphans(&jobs, probe, killer, cancel)
}

/// Startup entry using host process inspection and group signals.
pub fn reconcile_registry_host(
    registry: &JobRegistry,
    cancel: &CancellationToken,
) -> Result<ReconcileReport, RecoveryError> {
    reconcile_registry(registry, &HostProcessProbe, &HostProcessKiller, cancel)
}

fn start_times_compatible(recorded_unix_ms: u64, observed_unix_ms: u64) -> bool {
    if observed_unix_ms > recorded_unix_ms.saturating_add(START_AHEAD_SLACK_MS) {
        return false;
    }
    recorded_unix_ms.saturating_sub(observed_unix_ms) <= MAX_SPAWN_RECORD_SKEW_MS
}

fn downgrade_pid_collisions(classified: &[(&OrphanJob, Ownership)]) -> Vec<Ownership> {
    let mut owned_by_pid: BTreeMap<u32, u32> = BTreeMap::new();
    for (job, ownership) in classified {
        if *ownership == Ownership::Owned {
            *owned_by_pid.entry(job.identity.pid()).or_insert(0) += 1;
        }
    }
    classified
        .iter()
        .map(|(job, ownership)| {
            if *ownership == Ownership::Owned
                && owned_by_pid.get(&job.identity.pid()).copied().unwrap_or(0) > 1
            {
                Ownership::Unknown
            } else {
                *ownership
            }
        })
        .collect()
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), RecoveryError> {
    if cancel.is_cancelled() {
        Err(RecoveryError::Cancelled)
    } else {
        Ok(())
    }
}

impl ProcessProbe for HostProcessProbe {
    fn observe(
        &self,
        recorded: ProcessIdentity,
        cancel: &CancellationToken,
    ) -> Result<ProcessObservation, RecoveryError> {
        check_cancel(cancel)?;
        if recorded.pid() < 2 {
            return Err(RecoveryError::InvalidIdentity);
        }
        let observed = host_observe(recorded.pid())?;
        check_cancel(cancel)?;
        Ok(observed)
    }
}

impl ProcessTreeKiller for HostProcessKiller {
    fn terminate_owned(
        &self,
        identity: ProcessIdentity,
        cancel: &CancellationToken,
    ) -> Result<(), RecoveryError> {
        check_cancel(cancel)?;
        if identity.pid() < 2 || identity.process_group_id() < 2 {
            return Err(RecoveryError::InvalidIdentity);
        }
        host_terminate_group(identity, cancel)
    }
}

fn host_observe(pid: u32) -> Result<ProcessObservation, RecoveryError> {
    if pid < 2 {
        return Err(RecoveryError::InvalidIdentity);
    }
    #[cfg(unix)]
    {
        if linux_proc_available() {
            return Ok(observe_linux_proc(pid));
        }
        Ok(observe_unix_ps(pid))
    }
    #[cfg(windows)]
    {
        let _ = pid;
        Ok(ProcessObservation::Unreadable)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Ok(ProcessObservation::Unreadable)
    }
}

#[cfg(unix)]
fn linux_proc_available() -> bool {
    std::path::Path::new("/proc/self/stat").is_file()
}

#[cfg(unix)]
fn observe_linux_proc(pid: u32) -> ProcessObservation {
    let path = format!("/proc/{pid}/stat");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return ProcessObservation::Absent;
        }
        Err(_) => return ProcessObservation::Unreadable,
    };
    if bytes.len() > MAX_PS_OUTPUT_BYTES {
        return ProcessObservation::Unreadable;
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return ProcessObservation::Unreadable;
    };
    let Some((pgid, start_ticks)) = parse_linux_stat(text) else {
        return ProcessObservation::Unreadable;
    };
    let Some(started_unix_ms) = linux_start_unix_ms(start_ticks) else {
        return ProcessObservation::Unreadable;
    };
    ProcessObservation::Present {
        pid,
        process_group_id: Some(pgid),
        started_unix_ms: Some(started_unix_ms),
    }
}

#[cfg(unix)]
fn parse_linux_stat(text: &str) -> Option<(u32, u64)> {
    let close = text.rfind(')')?;
    let mut fields = text[close + 1..].split_whitespace();
    let _state = fields.next()?;
    let _ppid = fields.next()?;
    let pgid = fields.next()?.parse::<u32>().ok()?;
    for _ in 0..16 {
        fields.next()?;
    }
    let start_ticks = fields.next()?.parse::<u64>().ok()?;
    Some((pgid, start_ticks))
}

#[cfg(unix)]
fn linux_start_unix_ms(start_ticks: u64) -> Option<u64> {
    let bytes = std::fs::read("/proc/stat").ok()?;
    if bytes.len() > 64 * 1024 {
        return None;
    }
    let text = std::str::from_utf8(&bytes).ok()?;
    let mut btime = None;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("btime ") else {
            continue;
        };
        btime = rest.split_whitespace().next()?.parse::<u64>().ok();
        break;
    }
    let btime = btime?;
    let ticks_per_sec = 100u64;
    let start_secs = start_ticks / ticks_per_sec;
    let start_ms = (start_ticks % ticks_per_sec) * 10;
    Some(
        btime
            .saturating_mul(1000)
            .saturating_add(start_secs.saturating_mul(1000) + start_ms),
    )
}

#[cfg(unix)]
fn observe_unix_ps(pid: u32) -> ProcessObservation {
    let program = match ps_program() {
        Some(program) => program,
        None => return ProcessObservation::Unreadable,
    };
    let output = Command::new(program)
        .args([
            "-o",
            "pid=",
            "-o",
            "pgid=",
            "-o",
            "etime=",
            "-p",
            &pid.to_string(),
        ])
        .stdin(Stdio::null())
        .env_clear()
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .output();
    let output = match output {
        Ok(output) => output,
        Err(_) => return ProcessObservation::Unreadable,
    };
    if output.stdout.len() + output.stderr.len() > MAX_PS_OUTPUT_BYTES {
        return ProcessObservation::Unreadable;
    }
    if !output.status.success() {
        return if process_absent(output.status) {
            ProcessObservation::Absent
        } else {
            ProcessObservation::Unreadable
        };
    }
    parse_ps_table(pid, &output.stdout).unwrap_or(ProcessObservation::Unreadable)
}

#[cfg(unix)]
fn parse_ps_table(expected_pid: u32, stdout: &[u8]) -> Option<ProcessObservation> {
    let text = std::str::from_utf8(stdout).ok()?;
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let pgid = parts.next()?.parse::<u32>().ok()?;
    let etime = parts.next()?;
    if parts.next().is_some() || pid != expected_pid {
        return None;
    }
    let elapsed_secs = parse_etime(etime)?;
    let now = unix_now_ms()?;
    let started = now.saturating_sub(elapsed_secs.saturating_mul(1000));
    Some(ProcessObservation::Present {
        pid,
        process_group_id: Some(pgid),
        started_unix_ms: Some(started),
    })
}

#[cfg(unix)]
fn parse_etime(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (days, rest) = match raw.split_once('-') {
        Some((days, rest)) => (days.parse::<u64>().ok()?, rest),
        None => (0, raw),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let secs = match parts.as_slice() {
        [ss] => ss.parse::<u64>().ok()?,
        [mm, ss] => mm
            .parse::<u64>()
            .ok()?
            .checked_mul(60)?
            .checked_add(ss.parse::<u64>().ok()?)?,
        [hh, mm, ss] => hh
            .parse::<u64>()
            .ok()?
            .checked_mul(3600)?
            .checked_add(mm.parse::<u64>().ok()?.checked_mul(60)?)?
            .checked_add(ss.parse::<u64>().ok()?)?,
        _ => return None,
    };
    days.checked_mul(86_400)?.checked_add(secs)
}

// Used only by the `cfg(unix)` liveness check; gated with it.
#[cfg(unix)]
fn unix_now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

fn host_terminate_group(
    identity: ProcessIdentity,
    cancel: &CancellationToken,
) -> Result<(), RecoveryError> {
    check_cancel(cancel)?;
    signal_group(identity.process_group_id(), Signal::Term)?;
    if wait_until_absent(identity.pid(), RECOVERY_GRACE, cancel)? {
        return Ok(());
    }
    check_cancel(cancel)?;
    signal_group(identity.process_group_id(), Signal::Kill)?;
    if wait_until_absent(identity.pid(), KILL_WAIT, cancel)? {
        Ok(())
    } else {
        Err(RecoveryError::TreeStillAlive)
    }
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

fn signal_group(pgid: u32, kind: Signal) -> Result<(), RecoveryError> {
    if pgid < 2 {
        return Err(RecoveryError::InvalidIdentity);
    }
    platform_signal_group(pgid, kind)
}

#[cfg(unix)]
fn platform_signal_group(pgid: u32, kind: Signal) -> Result<(), RecoveryError> {
    let flag = match kind {
        Signal::Term => "-TERM",
        Signal::Kill => "-KILL",
    };
    let target = format!("-{pgid}");
    let program = kill_program()?;
    let status = Command::new(program)
        .args([flag, target.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .status()
        .map_err(|_| RecoveryError::SignalFailed)?;
    if status.success() || process_absent(status) {
        Ok(())
    } else {
        Err(RecoveryError::SignalFailed)
    }
}

#[cfg(windows)]
fn platform_signal_group(pgid: u32, kind: Signal) -> Result<(), RecoveryError> {
    let pid = pgid.to_string();
    let mut command = Command::new(TASKKILL_PROGRAM);
    command.args(["/PID", &pid, "/T"]);
    if matches!(kind, Signal::Kill) {
        command.arg("/F");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear();
    let status = command.status().map_err(|_| RecoveryError::SignalFailed)?;
    if status.success() || process_absent(status) {
        Ok(())
    } else {
        Err(RecoveryError::SignalFailed)
    }
}

#[cfg(not(any(unix, windows)))]
fn platform_signal_group(_pgid: u32, _kind: Signal) -> Result<(), RecoveryError> {
    Err(RecoveryError::UnsupportedPlatform)
}

fn wait_until_absent(
    pid: u32,
    budget: Duration,
    cancel: &CancellationToken,
) -> Result<bool, RecoveryError> {
    let deadline = Instant::now() + budget;
    loop {
        check_cancel(cancel)?;
        if !pid_alive(pid) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn pid_alive(pid: u32) -> bool {
    if pid < 2 {
        return false;
    }
    #[cfg(unix)]
    {
        let Ok(program) = kill_program() else {
            return false;
        };
        Command::new(program)
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_clear()
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn process_absent(status: ExitStatus) -> bool {
    matches!(status.code(), Some(1) | Some(128))
}

#[cfg(unix)]
fn kill_program() -> Result<&'static str, RecoveryError> {
    KILL_PROGRAMS
        .iter()
        .copied()
        .find(|path| std::path::Path::new(path).is_file())
        .ok_or(RecoveryError::SignalFailed)
}

#[cfg(unix)]
fn ps_program() -> Option<&'static str> {
    PS_PROGRAMS
        .iter()
        .copied()
        .find(|path| std::path::Path::new(path).is_file())
}

impl RecoveryError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "process recovery cancelled",
            Self::InvalidIdentity => "process identity is not a supervised identity",
            Self::TooManyJobs => "process recovery exceeds the live job bound",
            Self::SignalFailed => "process group signal failed",
            Self::TreeStillAlive => "process tree still alive after kill",
            Self::UnsupportedPlatform => "process recovery is unsupported on this platform",
        }
    }

    pub const fn error_code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::InvalidIdentity | Self::TooManyJobs => Some(ErrorCode::ToolInvalidArguments),
            Self::SignalFailed | Self::TreeStillAlive => Some(ErrorCode::InternalUnexpected),
            Self::UnsupportedPlatform => Some(ErrorCode::SandboxTierUnavailable),
        }
    }
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for RecoveryError {}

impl fmt::Display for Ownership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ReconcileDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ReconcileOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "job={} ownership={} decision={}",
            self.job_id, self.ownership, self.decision
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use event_ledger::event::{ActorKind, ActorRef};
    use event_ledger::ledger::{CancellationToken as LedgerCancel, EventLedger};
    use protocol::{EventId, LeaseId, ProjectId, RedactionClass, SessionId, TraceId};

    use crate::jobs::{JobEventMeta, JobSpec, JobState, PersistedInvocation};

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-7c1e9b";

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    struct ScriptedProbe {
        by_pid: BTreeMap<u32, ProcessObservation>,
    }

    impl ScriptedProbe {
        fn new(pairs: impl IntoIterator<Item = (u32, ProcessObservation)>) -> Self {
            Self {
                by_pid: pairs.into_iter().collect(),
            }
        }
    }

    impl ProcessProbe for ScriptedProbe {
        fn observe(
            &self,
            recorded: ProcessIdentity,
            cancel: &CancellationToken,
        ) -> Result<ProcessObservation, RecoveryError> {
            check_cancel(cancel)?;
            Ok(self
                .by_pid
                .get(&recorded.pid())
                .copied()
                .unwrap_or(ProcessObservation::Unreadable))
        }
    }

    #[derive(Default)]
    struct RecordingKiller {
        killed: RefCell<Vec<ProcessIdentity>>,
    }

    impl ProcessTreeKiller for RecordingKiller {
        fn terminate_owned(
            &self,
            identity: ProcessIdentity,
            cancel: &CancellationToken,
        ) -> Result<(), RecoveryError> {
            check_cancel(cancel)?;
            if identity.pid() < 2 || identity.process_group_id() < 2 {
                return Err(RecoveryError::InvalidIdentity);
            }
            self.killed.borrow_mut().push(identity);
            Ok(())
        }
    }

    impl RecordingKiller {
        fn killed(&self) -> Vec<ProcessIdentity> {
            self.killed.borrow().clone()
        }
    }

    struct FlipToReuseProbe {
        recorded: ProcessIdentity,
        observes: RefCell<u32>,
    }

    impl ProcessProbe for FlipToReuseProbe {
        fn observe(
            &self,
            recorded: ProcessIdentity,
            cancel: &CancellationToken,
        ) -> Result<ProcessObservation, RecoveryError> {
            check_cancel(cancel)?;
            let mut count = self.observes.borrow_mut();
            *count += 1;
            if *count == 1 {
                Ok(present(self.recorded))
            } else {
                Ok(ProcessObservation::Present {
                    pid: recorded.pid(),
                    process_group_id: Some(recorded.process_group_id()),
                    started_unix_ms: Some(recorded.started_unix_ms().saturating_add(60_000)),
                })
            }
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn identity(pid: u32, pgid: u32, started: u64) -> ProcessIdentity {
        ProcessIdentity::new(pid, pgid, started).expect("identity")
    }

    fn present(id: ProcessIdentity) -> ProcessObservation {
        ProcessObservation::Present {
            pid: id.pid(),
            process_group_id: Some(id.process_group_id()),
            started_unix_ms: Some(id.started_unix_ms()),
        }
    }

    fn job(lifetime: JobLifetime, id: ProcessIdentity) -> OrphanJob {
        OrphanJob::new(JobId::new(), lifetime, id).expect("job")
    }

    fn reconcile(
        jobs: &[OrphanJob],
        probe: &ScriptedProbe,
        killer: &RecordingKiller,
    ) -> ReconcileReport {
        reconcile_orphans(jobs, probe, killer, &live()).expect("reconcile")
    }

    struct TempReg {
        root: PathBuf,
        session: SessionId,
        registry: JobRegistry,
    }

    impl TempReg {
        fn create() -> Self {
            let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "rapidlm-orphan-recovery-{}-{seq}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("root");
            let ledger = EventLedger::open(root.join("ledger.sqlite")).expect("ledger");
            let session = SessionId::new();
            ledger
                .create_session(session, ProjectId::new(), &LedgerCancel::new())
                .expect("session");
            let registry = JobRegistry::open(&root, ledger).expect("registry");
            Self {
                root,
                session,
                registry,
            }
        }
    }

    impl Drop for TempReg {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn meta() -> JobEventMeta {
        JobEventMeta::new(
            ActorRef::new(ActorKind::System, &EventId::new().to_string()).expect("actor"),
            TraceId::new(),
            RedactionClass::Project,
        )
    }

    fn spec(session: SessionId, lifetime: JobLifetime) -> JobSpec {
        JobSpec::new(
            JobId::new(),
            session,
            lifetime,
            LeaseId::new(),
            PersistedInvocation::Argv {
                argv: vec!["/bin/sleep".to_owned(), "30".to_owned()],
            },
            "/tmp",
            ["PATH"],
            0,
            Some(Duration::from_secs(30)),
            4096,
            "test",
        )
        .expect("spec")
    }

    #[test]
    fn pid_alone_is_never_owned() {
        let recorded = identity(4242, 4242, 1_700_000_000_000);
        assert_eq!(
            classify_identity(
                recorded,
                ProcessObservation::Present {
                    pid: 4242,
                    process_group_id: None,
                    started_unix_ms: None,
                }
            ),
            Ownership::Unknown
        );
        assert_eq!(
            classify_identity(
                recorded,
                ProcessObservation::Present {
                    pid: 4242,
                    process_group_id: Some(4242),
                    started_unix_ms: None,
                }
            ),
            Ownership::Unknown
        );
        assert_eq!(
            decide_action(Ownership::Unknown, ReconcilePolicy::TerminateOwned),
            ReconcileDecision::Blocked {
                warning: RecoveryWarning::UnknownOwnership,
            }
        );
    }

    #[test]
    fn matching_pid_group_and_start_is_owned() {
        let recorded = identity(4242, 4242, 1_700_000_000_000);
        assert_eq!(
            classify_identity(recorded, present(recorded)),
            Ownership::Owned
        );
    }

    #[test]
    fn pid_reuse_fixture_is_not_killed_as_rapidlm_job() {
        let recorded = identity(4242, 4242, 1_700_000_000_000);
        let reused = ProcessObservation::Present {
            pid: 4242,
            process_group_id: Some(4242),
            started_unix_ms: Some(1_700_000_060_000),
        };
        assert_eq!(classify_identity(recorded, reused), Ownership::Reused);

        let job = job(JobLifetime::Client, recorded);
        let probe = ScriptedProbe::new([(4242, reused)]);
        let killer = RecordingKiller::default();
        let report = reconcile(&[job], &probe, &killer);

        assert!(killer.killed().is_empty(), "PID reuse must not be signaled");
        assert_eq!(report.outcomes().len(), 1);
        assert_eq!(report.outcomes()[0].ownership(), Ownership::Reused);
        assert_eq!(
            report.outcomes()[0].decision(),
            ReconcileDecision::Blocked {
                warning: RecoveryWarning::PidReuse,
            }
        );
        assert_eq!(
            report.blocked().count(),
            1,
            "reuse is a warning/blocked state"
        );
    }

    #[test]
    fn unknown_ownership_is_blocked_and_not_killed() {
        let recorded = identity(77, 77, 1_700_000_000_000);
        let job = job(JobLifetime::Daemon, recorded);
        let probe = ScriptedProbe::new([(77, ProcessObservation::Unreadable)]);
        let killer = RecordingKiller::default();
        let report = reconcile(&[job], &probe, &killer);

        assert!(killer.killed().is_empty());
        assert_eq!(report.outcomes()[0].ownership(), Ownership::Unknown);
        assert_eq!(
            report.outcomes()[0].decision(),
            ReconcileDecision::Blocked {
                warning: RecoveryWarning::UnknownOwnership,
            }
        );
    }

    #[test]
    fn pgid_mismatch_is_reuse_not_owned() {
        let recorded = identity(50, 50, 1_700_000_000_000);
        let observed = ProcessObservation::Present {
            pid: 50,
            process_group_id: Some(99),
            started_unix_ms: Some(1_700_000_000_000),
        };
        assert_eq!(classify_identity(recorded, observed), Ownership::Reused);
        let job = job(JobLifetime::Client, recorded);
        let probe = ScriptedProbe::new([(50, observed)]);
        let killer = RecordingKiller::default();
        let report = reconcile(&[job], &probe, &killer);
        assert!(killer.killed().is_empty());
        assert_eq!(
            report.outcomes()[0].decision().warning(),
            Some(RecoveryWarning::PidReuse)
        );
    }

    #[test]
    fn dead_identity_is_orphaned_without_a_signal() {
        let recorded = identity(88, 88, 1_700_000_000_000);
        let job = job(JobLifetime::Client, recorded);
        let probe = ScriptedProbe::new([(88, ProcessObservation::Absent)]);
        let killer = RecordingKiller::default();
        let report = reconcile(&[job], &probe, &killer);
        assert!(killer.killed().is_empty());
        assert_eq!(report.outcomes()[0].ownership(), Ownership::Dead);
        assert_eq!(report.outcomes()[0].decision(), ReconcileDecision::Orphaned);
    }

    #[test]
    fn client_owned_is_terminated_daemon_owned_is_readopted() {
        let client_id = identity(11, 11, 1_700_000_000_000);
        let daemon_id = identity(12, 12, 1_700_000_000_000);
        let client = job(JobLifetime::Client, client_id);
        let daemon = job(JobLifetime::Daemon, daemon_id);
        let probe = ScriptedProbe::new([(11, present(client_id)), (12, present(daemon_id))]);
        let killer = RecordingKiller::default();
        let report = reconcile(&[client, daemon], &probe, &killer);
        assert_eq!(killer.killed(), vec![client_id]);
        assert_eq!(
            report.outcomes()[0].decision(),
            ReconcileDecision::Terminated
        );
        assert_eq!(
            report.outcomes()[1].decision(),
            ReconcileDecision::Readopted
        );
    }

    #[test]
    fn second_probe_reuse_aborts_terminate() {
        let recorded = identity(33, 33, 1_700_000_000_000);
        let job = job(JobLifetime::Client, recorded);
        let probe = FlipToReuseProbe {
            recorded,
            observes: RefCell::new(0),
        };
        let killer = RecordingKiller::default();
        let report = reconcile_orphans(&[job], &probe, &killer, &live()).expect("reconcile");
        assert!(killer.killed().is_empty());
        assert_eq!(
            report.outcomes()[0].decision(),
            ReconcileDecision::Blocked {
                warning: RecoveryWarning::PidReuse,
            }
        );
    }

    #[test]
    fn duplicate_owned_pid_fails_closed() {
        let id = identity(44, 44, 1_700_000_000_000);
        let a = job(JobLifetime::Client, id);
        let b = job(JobLifetime::Client, id);
        let probe = ScriptedProbe::new([(44, present(id))]);
        let killer = RecordingKiller::default();
        let report = reconcile(&[a, b], &probe, &killer);
        assert!(killer.killed().is_empty());
        assert!(
            report
                .outcomes()
                .iter()
                .all(|o| o.ownership() == Ownership::Unknown
                    && o.decision()
                        == ReconcileDecision::Blocked {
                            warning: RecoveryWarning::UnknownOwnership,
                        })
        );
    }

    #[test]
    fn cancelled_reconcile_does_not_signal() {
        let recorded = identity(55, 55, 1_700_000_000_000);
        let job = job(JobLifetime::Client, recorded);
        let probe = ScriptedProbe::new([(55, present(recorded))]);
        let killer = RecordingKiller::default();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = reconcile_orphans(&[job], &probe, &killer, &cancel).expect_err("cancelled");
        assert_eq!(err, RecoveryError::Cancelled);
        assert_eq!(err.error_code(), None);
        assert!(killer.killed().is_empty());
    }

    #[test]
    fn broadcast_identity_is_rejected() {
        assert_eq!(
            ProcessIdentity::new(1, 9, 1).expect_err("pid 1"),
            crate::jobs::JobError::InvalidIdentity
        );
        let recorded = identity(9, 9, 1);
        let err = OrphanJob::new(JobId::new(), JobLifetime::Client, recorded);
        // ProcessIdentity already rejected pid/pgid < 2; recovery also
        // refuses a present observation that names pgid 0/1 as owned.
        assert_eq!(
            classify_identity(
                recorded,
                ProcessObservation::Present {
                    pid: 9,
                    process_group_id: Some(1),
                    started_unix_ms: Some(1),
                }
            ),
            Ownership::Unknown
        );
        let _ = err;
    }

    #[test]
    fn registry_startup_classifies_running_tombstones() {
        let mut tmp = TempReg::create();
        let reuse_id = identity(4242, 4242, 1_700_000_000_000);
        let dead_id = identity(4243, 4243, 1_700_000_000_000);
        let reuse_spec = spec(tmp.session, JobLifetime::Client);
        let dead_spec = spec(tmp.session, JobLifetime::Daemon);
        let reuse_job = reuse_spec.job_id();
        let dead_job = dead_spec.job_id();
        tmp.registry
            .start_job(reuse_spec, reuse_id, &meta(), &live())
            .expect("start reuse");
        tmp.registry
            .start_job(dead_spec, dead_id, &meta(), &live())
            .expect("start dead");

        let probe = ScriptedProbe::new([
            (
                4242,
                ProcessObservation::Present {
                    pid: 4242,
                    process_group_id: Some(4242),
                    started_unix_ms: Some(1_800_000_000_000),
                },
            ),
            (4243, ProcessObservation::Absent),
        ]);
        let killer = RecordingKiller::default();
        let report = reconcile_registry(&tmp.registry, &probe, &killer, &live()).expect("startup");
        assert!(killer.killed().is_empty());
        let by_id: BTreeMap<_, _> = report
            .outcomes()
            .iter()
            .map(|o| (o.job_id(), o.decision()))
            .collect();
        assert_eq!(
            by_id.get(&reuse_job).copied(),
            Some(ReconcileDecision::Blocked {
                warning: RecoveryWarning::PidReuse,
            })
        );
        assert_eq!(
            by_id.get(&dead_job).copied(),
            Some(ReconcileDecision::Orphaned)
        );
        assert_eq!(
            tmp.registry.get(reuse_job).expect("still running").state(),
            JobState::Running
        );
    }

    #[test]
    fn errors_and_debug_do_not_echo_canary() {
        let recorded = identity(66, 66, 1_700_000_000_000);
        let job = OrphanJob::new(JobId::new(), JobLifetime::Daemon, recorded).expect("job");
        let probe = ScriptedProbe::new([(66, ProcessObservation::Unreadable)]);
        let killer = RecordingKiller::default();
        let report = reconcile(&[job], &probe, &killer);
        let shown = format!(
            "{:?} {:?} {:?} {:?} {} {}",
            report,
            RecoveryError::SignalFailed,
            job,
            report.outcomes()[0],
            RecoveryError::InvalidIdentity,
            report.outcomes()[0]
        );
        assert!(!shown.contains(CANARY), "{shown}");
        for err in [
            RecoveryError::Cancelled,
            RecoveryError::InvalidIdentity,
            RecoveryError::TooManyJobs,
            RecoveryError::SignalFailed,
            RecoveryError::TreeStillAlive,
            RecoveryError::UnsupportedPlatform,
        ] {
            assert!(!err.to_string().contains(CANARY));
            assert!(!format!("{err:?}").contains("/etc/passwd"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn live_pid_reuse_fixture_is_not_killed() {
        let sleep = "/bin/sleep";
        assert!(std::path::Path::new(sleep).is_file());
        let mut child = Command::new(sleep)
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("outsider");
        let pid = child.id();
        let pgid = query_pgid(pid).unwrap_or(pid);
        let recorded = ProcessIdentity::new(pid, pgid.max(2), 1).expect("recorded");
        let job = job(JobLifetime::Client, recorded);
        let cancel = live();
        let report = reconcile_orphans(&[job], &HostProcessProbe, &HostProcessKiller, &cancel)
            .expect("host reconcile");
        assert_eq!(report.outcomes().len(), 1);
        assert_ne!(
            report.outcomes()[0].decision(),
            ReconcileDecision::Terminated
        );
        assert!(
            matches!(
                report.outcomes()[0].decision(),
                ReconcileDecision::Blocked { .. }
            ),
            "live reuse/unknown must be blocked, got {:?}",
            report.outcomes()[0].decision()
        );
        assert!(
            pid_alive(pid),
            "PID reuse fixture {pid} was killed as a RapidLM job"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    fn query_pgid(pid: u32) -> Option<u32> {
        let program = ps_program()?;
        let output = Command::new(program)
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        std::str::from_utf8(&output.stdout)
            .ok()?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }

    #[cfg(unix)]
    #[test]
    fn etime_and_linux_stat_parsers_are_bounded() {
        assert_eq!(parse_etime("05"), Some(5));
        assert_eq!(parse_etime("01:02"), Some(62));
        assert_eq!(parse_etime("01:02:03"), Some(3723));
        assert_eq!(parse_etime("2-01:00:00"), Some(176_400));
        assert!(parse_etime(CANARY).is_none());
        let stat = "4242 (canary-secret-PLAINTEXT-do-not-leak-7c1e9b) S 1 4242 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 12345 0";
        let parsed = parse_linux_stat(stat).expect("stat");
        assert_eq!(parsed.0, 4242);
        assert_eq!(parsed.1, 12345);
    }
}
