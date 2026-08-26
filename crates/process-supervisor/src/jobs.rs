//! Durable background job registry: specs, state, output refs, tombstones.
//!
//! [`JobRegistry`] journals `job.started` / `job.completed` to the Event Ledger
//! before acknowledging the transition. A running tombstone records process
//! identity (pid + group + start time) so reconnect does not need a live TUI.
//! Daemon-owned jobs ignore client/session lifetime; client-owned jobs do not.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Debug};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use capability_broker::CancellationToken;
use capability_broker::normalize::command::{
    MAX_ARG_BYTES, MAX_ARGV, MAX_ENV_NAME_BYTES, MAX_ENV_NAMES, MAX_PATH_BYTES,
    MAX_SHELL_SCRIPT_BYTES,
};
use event_ledger::event::{ActorRef, EventKind};
use event_ledger::ledger::{
    AppendOptions, CancellationToken as LedgerCancel, EventLedger, LedgerError, MAX_PAYLOAD_BYTES,
};
use protocol::{
    ArtifactId, ArtifactRef, ErrorCode, JobId, LeaseId, RedactionClass, SessionId, TraceId,
};
use serde::{Deserialize, Serialize};

use crate::output::{FinishedSpool, OutputRef};
use crate::spawn::{ExecSpec, Invocation, JobHandle};

/// On-disk record / tombstone schema written by this module.
pub const JOB_RECORD_SCHEMA: u16 = 1;

/// Maximum simultaneously non-terminal jobs in one registry.
pub const MAX_LIVE_JOBS: usize = 1024;

/// Maximum persisted job records loaded or retained by one registry.
pub const MAX_STORED_JOBS: usize = 8192;

/// Hard ceiling for a job record or tombstone file.
const MAX_RECORD_BYTES: usize = 64 * 1024;

const JOBS_DIR: &str = "jobs";
const RECORD_SUFFIX: &str = ".json";
const TOMBSTONE_SUFFIX: &str = ".running.json";

/// Who owns cancellation for a registered job.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobLifetime {
    Client,
    Daemon,
}

/// Durable job lifecycle (`architecture/process-supervisor-and-jobs.md`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Sleeping,
    Completed,
    Failed,
    Cancelled,
    Orphaned,
}

/// Argv-first or explicit shell invocation persisted without env/secret values.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedInvocation {
    Argv { argv: Vec<String> },
    Shell { shell: String, script: String },
}

/// Daemon-owned or client-owned job specification. Env values are not stored.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    job_id: JobId,
    session_id: SessionId,
    lifetime: JobLifetime,
    lease_id: LeaseId,
    invocation: PersistedInvocation,
    cwd: String,
    env_names: Vec<String>,
    stdin_len: u64,
    timeout_ms: Option<u64>,
    output_limit: u64,
    command_family: String,
}

/// OS identity recorded at start. Recovery must not trust PID alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pid: u32,
    process_group_id: u32,
    started_unix_ms: u64,
}

/// Artifact-only view of one captured stream. Excerpt bytes stay off the ledger.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutputPointer {
    artifact: Option<ArtifactRef>,
    truncated: bool,
    cursor: u64,
}

/// Finished stdout/stderr references stored on the job record.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobOutputs {
    stdout: OutputPointer,
    stderr: OutputPointer,
}

/// Terminal outcome written on `job.completed`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum JobOutcome {
    Completed {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    Failed {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    Cancelled {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
}

/// Attribution for a journaled job event. No argv/output/secret fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobEventMeta {
    actor: ActorRef,
    trace_id: TraceId,
    redaction: RedactionClass,
}

/// Reconnect-safe view of a persisted job. Payload bytes are never included.
#[derive(Clone, Eq, PartialEq)]
pub struct JobSnapshot {
    spec: JobSpec,
    state: JobState,
    identity: Option<ProcessIdentity>,
    outputs: Option<JobOutputs>,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    spec_digest: ArtifactId,
    started_seq: Option<u64>,
    completed_seq: Option<u64>,
}

/// Owned registry record. Same fields as [`JobSnapshot`].
pub type JobRecord = JobSnapshot;

/// Running-job tombstone used after TUI disconnect or supervisor restart.
#[derive(Clone, Eq, PartialEq)]
pub struct RunningTombstone {
    job_id: JobId,
    session_id: SessionId,
    lifetime: JobLifetime,
    lease_id: LeaseId,
    identity: ProcessIdentity,
    spec_digest: ArtifactId,
}

/// Jobs cancelled vs left running after a client/session lifetime end.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientDisconnectReport {
    cancelled: Vec<JobId>,
    still_running: Vec<JobId>,
}

/// Durable job index journaled through the session Event Ledger.
pub struct JobRegistry {
    root: PathBuf,
    ledger: EventLedger,
    jobs: BTreeMap<JobId, JobSnapshot>,
}

/// Typed registry failure. Display never echoes argv, script, or output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum JobError {
    Cancelled,
    NotFound,
    AlreadyExists,
    AlreadyTerminal,
    NotRunning,
    SessionNotFound,
    SessionMismatch,
    InvalidIdentity,
    InvalidSpec,
    TooManyJobs,
    PayloadTooLarge,
    NotCommitted,
    Corrupt,
    Ledger,
    Io,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedJob {
    schema: u16,
    spec: JobSpec,
    state: JobState,
    identity: Option<ProcessIdentity>,
    outputs: Option<JobOutputs>,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    spec_digest: String,
    started_seq: Option<u64>,
    completed_seq: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTombstone {
    schema: u16,
    job_id: String,
    session_id: String,
    lifetime: JobLifetime,
    lease_id: String,
    identity: ProcessIdentity,
    spec_digest: String,
}

#[derive(Serialize)]
struct StartedPayload<'a> {
    job_id: String,
    state: &'static str,
    lifetime: &'static str,
    lease_id: String,
    pid: u32,
    process_group_id: u32,
    spec_digest: &'a str,
}

#[derive(Serialize)]
struct CompletedPayload<'a> {
    job_id: String,
    state: &'static str,
    terminal: &'static str,
    lifetime: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_status: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    stdout: &'a OutputPointer,
    stderr: &'a OutputPointer,
}

impl JobLifetime {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Daemon => "daemon",
        }
    }

    pub const fn survives_client_disconnect(self) -> bool {
        matches!(self, Self::Daemon)
    }
}

impl JobState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Sleeping => "sleeping",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Orphaned => "orphaned",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Orphaned
        )
    }
}

impl JobOutcome {
    pub const fn state(self) -> JobState {
        match self {
            Self::Completed { .. } => JobState::Completed,
            Self::Failed { .. } => JobState::Failed,
            Self::Cancelled { .. } => JobState::Cancelled,
        }
    }

    pub const fn exit_code(self) -> Option<i32> {
        match self {
            Self::Completed { exit_code, .. }
            | Self::Failed { exit_code, .. }
            | Self::Cancelled { exit_code, .. } => exit_code,
        }
    }

    pub const fn signal(self) -> Option<i32> {
        match self {
            Self::Completed { signal, .. }
            | Self::Failed { signal, .. }
            | Self::Cancelled { signal, .. } => signal,
        }
    }

    const fn journal_state(self) -> &'static str {
        "completed"
    }

    const fn terminal_name(self) -> &'static str {
        self.state().as_str()
    }
}

impl PersistedInvocation {
    pub fn from_exec(invocation: &Invocation) -> Self {
        match invocation {
            Invocation::Argv { argv } => Self::Argv { argv: argv.clone() },
            Invocation::Shell { shell, script } => Self::Shell {
                shell: shell.clone(),
                script: script.clone(),
            },
        }
    }

    pub fn argv_len(&self) -> usize {
        match self {
            Self::Argv { argv } => argv.len(),
            Self::Shell { .. } => 2,
        }
    }
}

impl JobSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        job_id: JobId,
        session_id: SessionId,
        lifetime: JobLifetime,
        lease_id: LeaseId,
        invocation: PersistedInvocation,
        cwd: impl Into<String>,
        env_names: impl IntoIterator<Item = impl Into<String>>,
        stdin_len: u64,
        timeout: Option<Duration>,
        output_limit: u64,
        command_family: impl Into<String>,
    ) -> Result<Self, JobError> {
        let spec = Self {
            job_id,
            session_id,
            lifetime,
            lease_id,
            invocation,
            cwd: cwd.into(),
            env_names: env_names.into_iter().map(Into::into).collect(),
            stdin_len,
            timeout_ms: timeout
                .map(|d| u64::try_from(d.as_millis()).map_err(|_| JobError::InvalidSpec))
                .transpose()?,
            output_limit,
            command_family: command_family.into(),
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Build a spec from a spawned handle. Binding session must match `session_id`.
    pub fn from_spawned(
        handle: &JobHandle,
        exec: &ExecSpec,
        session_id: SessionId,
        lifetime: JobLifetime,
    ) -> Result<Self, JobError> {
        let binding = exec.binding().ok_or(JobError::InvalidSpec)?;
        if binding.session_id() != session_id {
            return Err(JobError::SessionMismatch);
        }
        let family = binding.command_family().ok_or(JobError::InvalidSpec)?;
        let stdin_len = match exec.stdin() {
            crate::spawn::StdinSpec::Empty => 0,
            crate::spawn::StdinSpec::Bytes(bytes) => bytes.len() as u64,
        };
        Self::new(
            handle.job_id(),
            session_id,
            lifetime,
            handle.lease_id(),
            PersistedInvocation::from_exec(exec.invocation()),
            exec.cwd().as_str(),
            exec.env().keys().cloned(),
            stdin_len,
            exec.timeout(),
            exec.output_limit(),
            family,
        )
    }

    pub fn job_id(&self) -> JobId {
        self.job_id
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn lifetime(&self) -> JobLifetime {
        self.lifetime
    }

    pub fn lease_id(&self) -> LeaseId {
        self.lease_id
    }

    pub fn invocation(&self) -> &PersistedInvocation {
        &self.invocation
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn env_names(&self) -> &[String] {
        &self.env_names
    }

    pub fn stdin_len(&self) -> u64 {
        self.stdin_len
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout_ms.map(Duration::from_millis)
    }

    pub fn output_limit(&self) -> u64 {
        self.output_limit
    }

    pub fn command_family(&self) -> &str {
        &self.command_family
    }

    pub fn digest(&self) -> Result<ArtifactId, JobError> {
        let bytes = serde_json::to_vec(self).map_err(|_| JobError::InvalidSpec)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(JobError::PayloadTooLarge);
        }
        Ok(ArtifactId::from_bytes(&bytes))
    }

    fn validate(&self) -> Result<(), JobError> {
        if self.cwd.is_empty() || self.command_family.is_empty() {
            return Err(JobError::InvalidSpec);
        }
        if self.cwd.len() > MAX_PATH_BYTES || self.command_family.len() > MAX_ENV_NAME_BYTES {
            return Err(JobError::InvalidSpec);
        }
        if self.cwd.contains('\0') || self.command_family.contains('\0') {
            return Err(JobError::InvalidSpec);
        }
        if self.env_names.len() > MAX_ENV_NAMES {
            return Err(JobError::InvalidSpec);
        }
        for name in &self.env_names {
            if name.is_empty() || name.len() > MAX_ENV_NAME_BYTES || name.contains('\0') {
                return Err(JobError::InvalidSpec);
            }
        }
        match &self.invocation {
            PersistedInvocation::Argv { argv } => {
                if argv.is_empty() || argv.len() > MAX_ARGV {
                    return Err(JobError::InvalidSpec);
                }
                for (i, arg) in argv.iter().enumerate() {
                    let max = if i == 0 {
                        MAX_PATH_BYTES
                    } else {
                        MAX_ARG_BYTES
                    };
                    if arg.len() > max || arg.contains('\0') {
                        return Err(JobError::InvalidSpec);
                    }
                    if i == 0 && arg.is_empty() {
                        return Err(JobError::InvalidSpec);
                    }
                }
            }
            PersistedInvocation::Shell { shell, script } => {
                if shell.is_empty() || script.is_empty() {
                    return Err(JobError::InvalidSpec);
                }
                if shell.len() > MAX_PATH_BYTES || script.len() > MAX_SHELL_SCRIPT_BYTES {
                    return Err(JobError::InvalidSpec);
                }
                if shell.contains('\0') || script.contains('\0') {
                    return Err(JobError::InvalidSpec);
                }
            }
        }
        Ok(())
    }
}

impl ProcessIdentity {
    pub fn new(pid: u32, process_group_id: u32, started_unix_ms: u64) -> Result<Self, JobError> {
        if pid < 2 || process_group_id < 2 {
            return Err(JobError::InvalidIdentity);
        }
        Ok(Self {
            pid,
            process_group_id,
            started_unix_ms,
        })
    }

    pub fn from_handle(handle: &JobHandle) -> Result<Self, JobError> {
        Self::new(
            handle.pid(),
            handle.process_group_id().as_u32(),
            unix_now_ms()?,
        )
    }

    pub const fn pid(self) -> u32 {
        self.pid
    }

    pub const fn process_group_id(self) -> u32 {
        self.process_group_id
    }

    pub const fn started_unix_ms(self) -> u64 {
        self.started_unix_ms
    }
}

impl OutputPointer {
    pub fn new(artifact: Option<ArtifactRef>, truncated: bool, cursor: u64) -> Self {
        Self {
            artifact,
            truncated,
            cursor,
        }
    }

    pub fn from_output_ref(refer: &OutputRef) -> Self {
        Self {
            artifact: refer.artifact.clone(),
            truncated: refer.truncated,
            cursor: refer.cursor.offset,
        }
    }

    pub fn empty() -> Self {
        Self {
            artifact: None,
            truncated: false,
            cursor: 0,
        }
    }

    pub fn artifact(&self) -> Option<&ArtifactRef> {
        self.artifact.as_ref()
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn cursor(&self) -> u64 {
        self.cursor
    }
}

impl JobOutputs {
    pub fn new(stdout: OutputPointer, stderr: OutputPointer) -> Self {
        Self { stdout, stderr }
    }

    pub fn from_finished(finished: &FinishedSpool) -> Self {
        Self {
            stdout: OutputPointer::from_output_ref(&finished.stdout),
            stderr: OutputPointer::from_output_ref(&finished.stderr),
        }
    }

    pub fn empty() -> Self {
        Self {
            stdout: OutputPointer::empty(),
            stderr: OutputPointer::empty(),
        }
    }

    pub fn stdout(&self) -> &OutputPointer {
        &self.stdout
    }

    pub fn stderr(&self) -> &OutputPointer {
        &self.stderr
    }
}

impl JobEventMeta {
    pub fn new(actor: ActorRef, trace_id: TraceId, redaction: RedactionClass) -> Self {
        Self {
            actor,
            trace_id,
            redaction,
        }
    }

    pub fn actor(&self) -> &ActorRef {
        &self.actor
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub fn redaction(&self) -> RedactionClass {
        self.redaction
    }
}

impl JobSnapshot {
    pub fn spec(&self) -> &JobSpec {
        &self.spec
    }

    pub fn job_id(&self) -> JobId {
        self.spec.job_id
    }

    pub fn session_id(&self) -> SessionId {
        self.spec.session_id
    }

    pub fn lifetime(&self) -> JobLifetime {
        self.spec.lifetime
    }

    pub fn state(&self) -> JobState {
        self.state
    }

    pub fn identity(&self) -> Option<ProcessIdentity> {
        self.identity
    }

    pub fn outputs(&self) -> Option<&JobOutputs> {
        self.outputs.as_ref()
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub fn exit_signal(&self) -> Option<i32> {
        self.exit_signal
    }

    pub fn spec_digest(&self) -> ArtifactId {
        self.spec_digest
    }

    pub fn started_seq(&self) -> Option<u64> {
        self.started_seq
    }

    pub fn completed_seq(&self) -> Option<u64> {
        self.completed_seq
    }

    pub fn tombstone(&self) -> Option<RunningTombstone> {
        if self.state.is_terminal() {
            return None;
        }
        let identity = self.identity?;
        Some(RunningTombstone {
            job_id: self.spec.job_id,
            session_id: self.spec.session_id,
            lifetime: self.spec.lifetime,
            lease_id: self.spec.lease_id,
            identity,
            spec_digest: self.spec_digest,
        })
    }
}

impl RunningTombstone {
    pub fn job_id(&self) -> JobId {
        self.job_id
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn lifetime(&self) -> JobLifetime {
        self.lifetime
    }

    pub fn lease_id(&self) -> LeaseId {
        self.lease_id
    }

    pub fn identity(&self) -> ProcessIdentity {
        self.identity
    }

    pub fn spec_digest(&self) -> ArtifactId {
        self.spec_digest
    }
}

impl ClientDisconnectReport {
    pub fn cancelled(&self) -> &[JobId] {
        &self.cancelled
    }

    pub fn still_running(&self) -> &[JobId] {
        &self.still_running
    }
}

impl JobRegistry {
    /// Open (or create) a registry rooted at `root/jobs` and reload records.
    pub fn open(root: impl AsRef<Path>, ledger: EventLedger) -> Result<Self, JobError> {
        let jobs_dir = root.as_ref().join(JOBS_DIR);
        ensure_private_dir(&jobs_dir)?;
        let mut registry = Self {
            root: jobs_dir,
            ledger,
            jobs: BTreeMap::new(),
        };
        registry.reload()?;
        Ok(registry)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Persist a running job, write the tombstone, then journal `job.started`.
    pub fn start_job(
        &mut self,
        spec: JobSpec,
        identity: ProcessIdentity,
        meta: &JobEventMeta,
        cancel: &CancellationToken,
    ) -> Result<JobId, JobError> {
        check_cancel(cancel)?;
        spec.validate()?;
        if self.jobs.contains_key(&spec.job_id) {
            return Err(JobError::AlreadyExists);
        }
        if self.jobs.len() >= MAX_STORED_JOBS || self.live_count() >= MAX_LIVE_JOBS {
            return Err(JobError::TooManyJobs);
        }
        let spec_digest = spec.digest()?;
        let snapshot = JobSnapshot {
            spec,
            state: JobState::Running,
            identity: Some(identity),
            outputs: None,
            exit_code: None,
            exit_signal: None,
            spec_digest,
            started_seq: None,
            completed_seq: None,
        };
        self.write_tombstone(&snapshot)?;
        let journaled = match self.journal_started(&snapshot, meta, cancel) {
            Ok(seq) => seq,
            Err(err) => {
                let _ = fs::remove_file(self.tombstone_path(snapshot.job_id()));
                return Err(err);
            }
        };
        let mut stored = snapshot;
        stored.started_seq = Some(journaled);
        if let Err(err) = self.write_record(&stored) {
            let _ = fs::remove_file(self.tombstone_path(stored.job_id()));
            return Err(err);
        }
        let id = stored.job_id();
        self.jobs.insert(id, stored);
        Ok(id)
    }

    /// Journal `job.completed`, persist artifact refs, and drop the tombstone.
    pub fn complete_job(
        &mut self,
        job_id: JobId,
        outcome: JobOutcome,
        outputs: JobOutputs,
        meta: &JobEventMeta,
        cancel: &CancellationToken,
    ) -> Result<JobSnapshot, JobError> {
        check_cancel(cancel)?;
        let current = self.jobs.get(&job_id).ok_or(JobError::NotFound)?;
        if current.state.is_terminal() {
            return Err(JobError::AlreadyTerminal);
        }
        if current.state != JobState::Running && current.state != JobState::Sleeping {
            return Err(JobError::NotRunning);
        }
        let seq = self.journal_completed(current, outcome, &outputs, meta, cancel)?;
        let mut next = current.clone();
        next.state = outcome.state();
        next.outputs = Some(outputs);
        next.exit_code = outcome.exit_code();
        next.exit_signal = outcome.signal();
        next.completed_seq = Some(seq);
        self.write_record(&next)?;
        let _ = fs::remove_file(self.tombstone_path(job_id));
        self.jobs.insert(job_id, next.clone());
        Ok(next)
    }

    /// Reconnect-safe status. Does not require a live TUI or process handle.
    pub fn get(&self, job_id: JobId) -> Result<JobSnapshot, JobError> {
        self.jobs.get(&job_id).cloned().ok_or(JobError::NotFound)
    }

    /// Attach to a persisted job after TUI reconnect.
    pub fn attach(&self, job_id: JobId) -> Result<JobSnapshot, JobError> {
        self.get(job_id)
    }

    pub fn tombstone(&self, job_id: JobId) -> Result<RunningTombstone, JobError> {
        self.get(job_id)?.tombstone().ok_or(JobError::NotRunning)
    }

    pub fn running_tombstones(&self) -> Vec<RunningTombstone> {
        self.jobs
            .values()
            .filter_map(JobSnapshot::tombstone)
            .collect()
    }

    /// TUI/client disconnect: cancel client-owned jobs; leave daemon jobs running.
    pub fn on_client_disconnect(
        &mut self,
        session_id: SessionId,
        meta: &JobEventMeta,
        cancel: &CancellationToken,
    ) -> Result<ClientDisconnectReport, JobError> {
        self.end_client_lifetime(session_id, meta, cancel)
    }

    /// Session close uses the same client-lifetime rule as TUI disconnect.
    pub fn on_session_close(
        &mut self,
        session_id: SessionId,
        meta: &JobEventMeta,
        cancel: &CancellationToken,
    ) -> Result<ClientDisconnectReport, JobError> {
        self.end_client_lifetime(session_id, meta, cancel)
    }

    fn end_client_lifetime(
        &mut self,
        session_id: SessionId,
        meta: &JobEventMeta,
        cancel: &CancellationToken,
    ) -> Result<ClientDisconnectReport, JobError> {
        check_cancel(cancel)?;
        let mut cancelled = Vec::new();
        let mut still_running = Vec::new();
        let ids: Vec<JobId> = self
            .jobs
            .values()
            .filter(|job| job.session_id() == session_id && !job.state.is_terminal())
            .map(JobSnapshot::job_id)
            .collect();
        for id in ids {
            check_cancel(cancel)?;
            let job = self.jobs.get(&id).ok_or(JobError::NotFound)?;
            if job.lifetime().survives_client_disconnect() {
                still_running.push(id);
                continue;
            }
            self.complete_job(
                id,
                JobOutcome::Cancelled {
                    exit_code: None,
                    signal: None,
                },
                job.outputs().cloned().unwrap_or_else(JobOutputs::empty),
                meta,
                cancel,
            )?;
            cancelled.push(id);
        }
        Ok(ClientDisconnectReport {
            cancelled,
            still_running,
        })
    }

    fn live_count(&self) -> usize {
        self.jobs
            .values()
            .filter(|job| !job.state.is_terminal())
            .count()
    }

    fn reload(&mut self) -> Result<(), JobError> {
        self.jobs.clear();
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(JobError::from_io(err)),
        };
        for entry in entries {
            let entry = entry.map_err(JobError::from_io)?;
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name,
                None => return Err(JobError::Corrupt),
            };
            if !name.ends_with(RECORD_SUFFIX) || name.ends_with(TOMBSTONE_SUFFIX) {
                continue;
            }
            if self.jobs.len() >= MAX_STORED_JOBS {
                return Err(JobError::TooManyJobs);
            }
            let snapshot = self.load_record(&path)?;
            self.verify_started_binding(&snapshot)?;
            if let Some(tombstone) = self.load_tombstone_if_present(snapshot.job_id())? {
                if tombstone.lifetime != snapshot.lifetime()
                    || tombstone.spec_digest != snapshot.spec_digest
                    || tombstone.session_id != snapshot.session_id()
                    || tombstone.job_id != snapshot.job_id()
                {
                    return Err(JobError::Corrupt);
                }
            } else if !snapshot.state.is_terminal() {
                return Err(JobError::Corrupt);
            }
            self.jobs.insert(snapshot.job_id(), snapshot);
        }
        Ok(())
    }

    fn verify_started_binding(&self, snapshot: &JobSnapshot) -> Result<(), JobError> {
        let Some(seq) = snapshot.started_seq else {
            return if snapshot.state == JobState::Running {
                Err(JobError::Corrupt)
            } else {
                Ok(())
            };
        };
        let cancel = LedgerCancel::new();
        let event = self
            .ledger
            .get(snapshot.session_id(), seq, &cancel)
            .map_err(map_ledger)?;
        if event.kind() != EventKind::JobStarted {
            return Err(JobError::Corrupt);
        }
        let payload = event.payload();
        let job_id = payload
            .get("job_id")
            .and_then(|v| v.as_str())
            .ok_or(JobError::Corrupt)?;
        let lifetime = payload
            .get("lifetime")
            .and_then(|v| v.as_str())
            .ok_or(JobError::Corrupt)?;
        let digest = payload
            .get("spec_digest")
            .and_then(|v| v.as_str())
            .ok_or(JobError::Corrupt)?;
        if job_id != snapshot.job_id().to_string() {
            return Err(JobError::Corrupt);
        }
        if lifetime != snapshot.lifetime().as_str() {
            return Err(JobError::Corrupt);
        }
        if digest != snapshot.spec_digest.to_string() {
            return Err(JobError::Corrupt);
        }
        Ok(())
    }

    fn journal_started(
        &self,
        snapshot: &JobSnapshot,
        meta: &JobEventMeta,
        cancel: &CancellationToken,
    ) -> Result<u64, JobError> {
        check_cancel(cancel)?;
        let digest = snapshot.spec_digest.to_string();
        let identity = snapshot.identity.ok_or(JobError::InvalidIdentity)?;
        let payload = StartedPayload {
            job_id: snapshot.job_id().to_string(),
            state: "started",
            lifetime: snapshot.lifetime().as_str(),
            lease_id: snapshot.spec.lease_id.to_string(),
            pid: identity.pid,
            process_group_id: identity.process_group_id,
            spec_digest: &digest,
        };
        self.append(
            snapshot.session_id(),
            EventKind::JobStarted,
            &payload,
            meta,
            cancel,
        )
    }

    fn journal_completed(
        &self,
        snapshot: &JobSnapshot,
        outcome: JobOutcome,
        outputs: &JobOutputs,
        meta: &JobEventMeta,
        cancel: &CancellationToken,
    ) -> Result<u64, JobError> {
        check_cancel(cancel)?;
        let payload = CompletedPayload {
            job_id: snapshot.job_id().to_string(),
            state: outcome.journal_state(),
            terminal: outcome.terminal_name(),
            lifetime: snapshot.lifetime().as_str(),
            exit_status: outcome.exit_code(),
            signal: outcome.signal(),
            stdout: &outputs.stdout,
            stderr: &outputs.stderr,
        };
        self.append(
            snapshot.session_id(),
            EventKind::JobCompleted,
            &payload,
            meta,
            cancel,
        )
    }

    fn append<P: Serialize>(
        &self,
        session: SessionId,
        kind: EventKind,
        payload: &P,
        meta: &JobEventMeta,
        cancel: &CancellationToken,
    ) -> Result<u64, JobError> {
        check_cancel(cancel)?;
        let encoded = serde_json::to_vec(payload).map_err(|_| JobError::InvalidSpec)?;
        if encoded.len() > MAX_PAYLOAD_BYTES.min(MAX_RECORD_BYTES) {
            return Err(JobError::PayloadTooLarge);
        }
        let ledger_cancel = LedgerCancel::new();
        if cancel.is_cancelled() {
            ledger_cancel.cancel();
            return Err(JobError::Cancelled);
        }
        let options = AppendOptions {
            redaction: meta.redaction,
            trace_id: meta.trace_id,
            expected_seq: None,
        };
        let envelope = self
            .ledger
            .append(
                session,
                meta.actor.clone(),
                kind,
                payload,
                &options,
                &ledger_cancel,
            )
            .map_err(map_ledger)?;
        if cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        Ok(envelope.seq())
    }

    fn write_record(&self, snapshot: &JobSnapshot) -> Result<(), JobError> {
        let persisted = PersistedJob {
            schema: JOB_RECORD_SCHEMA,
            spec: snapshot.spec.clone(),
            state: snapshot.state,
            identity: snapshot.identity,
            outputs: snapshot.outputs.clone(),
            exit_code: snapshot.exit_code,
            exit_signal: snapshot.exit_signal,
            spec_digest: snapshot.spec_digest.to_string(),
            started_seq: snapshot.started_seq,
            completed_seq: snapshot.completed_seq,
        };
        write_private_json(&self.record_path(snapshot.job_id()), &persisted)
    }

    fn write_tombstone(&self, snapshot: &JobSnapshot) -> Result<(), JobError> {
        let identity = snapshot.identity.ok_or(JobError::InvalidIdentity)?;
        let persisted = PersistedTombstone {
            schema: JOB_RECORD_SCHEMA,
            job_id: snapshot.job_id().to_string(),
            session_id: snapshot.session_id().to_string(),
            lifetime: snapshot.lifetime(),
            lease_id: snapshot.spec.lease_id.to_string(),
            identity,
            spec_digest: snapshot.spec_digest.to_string(),
        };
        write_exclusive_json(&self.tombstone_path(snapshot.job_id()), &persisted)
    }

    fn load_record(&self, path: &Path) -> Result<JobSnapshot, JobError> {
        let persisted: PersistedJob = read_json(path)?;
        if persisted.schema != JOB_RECORD_SCHEMA {
            return Err(JobError::Corrupt);
        }
        persisted.spec.validate()?;
        let digest = persisted.spec.digest()?;
        if digest.to_string() != persisted.spec_digest {
            return Err(JobError::Corrupt);
        }
        Ok(JobSnapshot {
            spec: persisted.spec,
            state: persisted.state,
            identity: persisted.identity,
            outputs: persisted.outputs,
            exit_code: persisted.exit_code,
            exit_signal: persisted.exit_signal,
            spec_digest: digest,
            started_seq: persisted.started_seq,
            completed_seq: persisted.completed_seq,
        })
    }

    fn load_tombstone_if_present(
        &self,
        job_id: JobId,
    ) -> Result<Option<RunningTombstone>, JobError> {
        let path = self.tombstone_path(job_id);
        match read_json::<PersistedTombstone>(&path) {
            Ok(persisted) => {
                if persisted.schema != JOB_RECORD_SCHEMA {
                    return Err(JobError::Corrupt);
                }
                let parsed_id: JobId = persisted.job_id.parse().map_err(|_| JobError::Corrupt)?;
                let session_id: SessionId = persisted
                    .session_id
                    .parse()
                    .map_err(|_| JobError::Corrupt)?;
                let lease_id: LeaseId =
                    persisted.lease_id.parse().map_err(|_| JobError::Corrupt)?;
                if parsed_id != job_id {
                    return Err(JobError::Corrupt);
                }
                let spec_digest: ArtifactId = persisted
                    .spec_digest
                    .parse()
                    .map_err(|_| JobError::Corrupt)?;
                let identity = ProcessIdentity::new(
                    persisted.identity.pid,
                    persisted.identity.process_group_id,
                    persisted.identity.started_unix_ms,
                )?;
                Ok(Some(RunningTombstone {
                    job_id: parsed_id,
                    session_id,
                    lifetime: persisted.lifetime,
                    lease_id,
                    identity,
                    spec_digest,
                }))
            }
            Err(JobError::NotFound) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn record_path(&self, job_id: JobId) -> PathBuf {
        self.root.join(format!("{job_id}{RECORD_SUFFIX}"))
    }

    fn tombstone_path(&self, job_id: JobId) -> PathBuf {
        self.root.join(format!("{job_id}{TOMBSTONE_SUFFIX}"))
    }
}

impl JobError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "job registry cancelled",
            Self::NotFound => "job not found",
            Self::AlreadyExists => "job already registered",
            Self::AlreadyTerminal => "job is already terminal",
            Self::NotRunning => "job is not running",
            Self::SessionNotFound => "job session is not in the ledger",
            Self::SessionMismatch => "job session does not match the bound lease",
            Self::InvalidIdentity => "job process identity is invalid",
            Self::InvalidSpec => "job spec is invalid",
            Self::TooManyJobs => "job registry capacity exceeded",
            Self::PayloadTooLarge => "job record exceeds bound",
            Self::NotCommitted => "job event was not committed",
            Self::Corrupt => "job registry data is corrupt",
            Self::Ledger => "job ledger error",
            Self::Io => "job registry io error",
        }
    }

    pub const fn error_code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::NotFound | Self::InvalidSpec | Self::InvalidIdentity | Self::SessionMismatch => {
                Some(ErrorCode::ToolInvalidArguments)
            }
            Self::AlreadyExists | Self::AlreadyTerminal | Self::NotRunning => {
                Some(ErrorCode::SessionConflict)
            }
            Self::SessionNotFound => Some(ErrorCode::SessionNotFound),
            Self::TooManyJobs | Self::PayloadTooLarge => Some(ErrorCode::ToolInvalidArguments),
            Self::NotCommitted | Self::Ledger | Self::Io => Some(ErrorCode::InternalUnexpected),
            Self::Corrupt => Some(ErrorCode::StorageCorrupt),
        }
    }

    fn from_io(err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::NotFound {
            Self::NotFound
        } else {
            Self::Io
        }
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for JobError {}

impl Debug for PersistedInvocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Argv { argv } => f.debug_struct("Argv").field("len", &argv.len()).finish(),
            Self::Shell { .. } => f
                .debug_struct("Shell")
                .field("script", &"redacted")
                .finish(),
        }
    }
}

impl Debug for JobSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobSpec")
            .field("job_id", &self.job_id)
            .field("session_id", &self.session_id)
            .field("lifetime", &self.lifetime)
            .field("lease_id", &self.lease_id)
            .field("invocation", &self.invocation)
            .field("cwd_len", &self.cwd.len())
            .field("env_names", &self.env_names.len())
            .field("stdin_len", &self.stdin_len)
            .field("timeout_ms", &self.timeout_ms)
            .field("output_limit", &self.output_limit)
            .field("command_family", &self.command_family)
            .finish()
    }
}

impl Debug for OutputPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputPointer")
            .field("artifact", &self.artifact.as_ref().map(|refer| refer.id))
            .field("truncated", &self.truncated)
            .field("cursor", &self.cursor)
            .finish()
    }
}

impl Debug for JobOutputs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobOutputs")
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .finish()
    }
}

impl Debug for JobSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobSnapshot")
            .field("job_id", &self.job_id())
            .field("state", &self.state)
            .field("lifetime", &self.lifetime())
            .field("identity", &self.identity)
            .field("outputs", &self.outputs)
            .field("exit_code", &self.exit_code)
            .field("spec_digest", &self.spec_digest)
            .finish()
    }
}

impl Debug for RunningTombstone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunningTombstone")
            .field("job_id", &self.job_id)
            .field("lifetime", &self.lifetime)
            .field("pid", &self.identity.pid)
            .field("process_group_id", &self.identity.process_group_id)
            .field("spec_digest", &self.spec_digest)
            .finish()
    }
}

impl Debug for JobRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobRegistry")
            .field("jobs", &self.jobs.len())
            .field("live", &self.live_count())
            .finish()
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), JobError> {
    if cancel.is_cancelled() {
        Err(JobError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_ledger(err: LedgerError) -> JobError {
    match err {
        LedgerError::Cancelled => JobError::Cancelled,
        LedgerError::SessionNotFound { .. } => JobError::SessionNotFound,
        LedgerError::PayloadBound { .. } => JobError::PayloadTooLarge,
        LedgerError::NotCommitted => JobError::NotCommitted,
        LedgerError::Corrupt(_) => JobError::Corrupt,
        _ => JobError::Ledger,
    }
}

fn unix_now_ms() -> Result<u64, JobError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|_| JobError::Io)
}

fn ensure_private_dir(path: &Path) -> Result<(), JobError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => validate_private_dir(&meta)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => create_private_dir(path)?,
        Err(err) => return Err(JobError::from_io(err)),
    }
    let meta = fs::symlink_metadata(path).map_err(JobError::from_io)?;
    validate_private_dir(&meta)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = meta.permissions();
        perms.set_mode(0o700);
        fs::set_permissions(path, perms).map_err(JobError::from_io)?;
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<(), JobError> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(path).map_err(JobError::from_io)?;
            validate_private_dir(&meta)
        }
        Err(err) => Err(JobError::from_io(err)),
    }
}

fn validate_private_dir(meta: &fs::Metadata) -> Result<(), JobError> {
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Err(JobError::Io);
    }
    Ok(())
}

fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<(), JobError> {
    let bytes = encode_json(value)?;
    let tmp = path.with_extension("json.tmp");
    let _ = fs::remove_file(&tmp);
    {
        let mut file = open_private_file(&tmp, true)?;
        file.write_all(&bytes).map_err(JobError::from_io)?;
        file.sync_all().map_err(JobError::from_io)?;
    }
    fs::rename(&tmp, path).map_err(JobError::from_io)
}

fn write_exclusive_json<T: Serialize>(path: &Path, value: &T) -> Result<(), JobError> {
    let bytes = encode_json(value)?;
    let mut file = open_private_file(path, false)?;
    file.write_all(&bytes).map_err(JobError::from_io)?;
    file.sync_all().map_err(JobError::from_io)?;
    Ok(())
}

fn open_private_file(path: &Path, overwrite_tmp: bool) -> Result<File, JobError> {
    let mut opts = OpenOptions::new();
    opts.write(true);
    if overwrite_tmp {
        opts.create(true).truncate(true);
    } else {
        opts.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(file) => {
            let meta = fs::symlink_metadata(path).map_err(JobError::from_io)?;
            if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                drop(file);
                let _ = fs::remove_file(path);
                return Err(JobError::Io);
            }
            Ok(file)
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(JobError::AlreadyExists),
        Err(err) => Err(JobError::from_io(err)),
    }
}

fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, JobError> {
    let bytes = serde_json::to_vec(value).map_err(|_| JobError::InvalidSpec)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(JobError::PayloadTooLarge);
    }
    Ok(bytes)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, JobError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(JobError::NotFound),
        Err(err) => return Err(JobError::from_io(err)),
    };
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(JobError::Corrupt);
    }
    if meta.len() > MAX_RECORD_BYTES as u64 {
        return Err(JobError::PayloadTooLarge);
    }
    let bytes = fs::read(path).map_err(JobError::from_io)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(JobError::PayloadTooLarge);
    }
    serde_json::from_slice(&bytes).map_err(|_| JobError::Corrupt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use event_ledger::event::ActorKind;
    use protocol::{EventId, ProjectId};

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-7c1e9b";

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempReg {
        root: PathBuf,
        ledger: EventLedger,
        session: SessionId,
        registry: JobRegistry,
    }

    impl TempReg {
        fn create() -> Self {
            let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("rapidlm-job-registry-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("root");
            let ledger = EventLedger::open(root.join("ledger.sqlite")).expect("ledger");
            let session = SessionId::new();
            ledger
                .create_session(session, ProjectId::new(), &LedgerCancel::new())
                .expect("session");
            let registry = JobRegistry::open(&root, ledger.clone()).expect("registry");
            Self {
                root,
                ledger,
                session,
                registry,
            }
        }

        fn reopen(&self) -> JobRegistry {
            JobRegistry::open(&self.root, self.ledger.clone()).expect("reopen")
        }
    }

    impl Drop for TempReg {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn meta() -> JobEventMeta {
        JobEventMeta::new(
            ActorRef::new(ActorKind::System, &EventId::new().to_string()).expect("actor"),
            TraceId::new(),
            RedactionClass::Project,
        )
    }

    fn identity() -> ProcessIdentity {
        ProcessIdentity::new(42, 42, 1_700_000_000_000).expect("identity")
    }

    fn spec(session: SessionId, lifetime: JobLifetime) -> JobSpec {
        spec_with_argv(session, lifetime, &["/bin/echo", "ok"])
    }

    fn spec_with_argv(session: SessionId, lifetime: JobLifetime, argv: &[&str]) -> JobSpec {
        JobSpec::new(
            JobId::new(),
            session,
            lifetime,
            LeaseId::new(),
            PersistedInvocation::Argv {
                argv: argv.iter().map(|s| (*s).to_owned()).collect(),
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

    fn artifact(tag: &str) -> ArtifactRef {
        ArtifactRef {
            id: ArtifactId::from_bytes(tag.as_bytes()),
            media_type: "application/octet-stream".to_owned(),
            bytes: tag.len() as u64,
            redaction: RedactionClass::Sensitive,
        }
    }

    fn outputs_with_canary() -> JobOutputs {
        JobOutputs::new(
            OutputPointer::new(Some(artifact(CANARY)), false, 32),
            OutputPointer::empty(),
        )
    }

    #[test]
    fn start_journals_started_and_writes_running_tombstone() {
        let mut tmp = TempReg::create();
        let job = spec(tmp.session, JobLifetime::Daemon);
        let id = tmp
            .registry
            .start_job(job.clone(), identity(), &meta(), &live())
            .expect("start");
        assert_eq!(id, job.job_id());
        let snap = tmp.registry.get(id).expect("get");
        assert_eq!(snap.state(), JobState::Running);
        assert_eq!(snap.lifetime(), JobLifetime::Daemon);
        assert!(snap.tombstone().is_some());
        assert!(tmp.registry.tombstone_path(id).is_file());

        let event = tmp
            .ledger
            .get(
                tmp.session,
                snap.started_seq().expect("seq"),
                &LedgerCancel::new(),
            )
            .expect("started event");
        assert_eq!(event.kind(), EventKind::JobStarted);
        assert_eq!(
            event.payload().get("job_id").and_then(|v| v.as_str()),
            Some(id.to_string().as_str())
        );
        assert_eq!(
            event.payload().get("state").and_then(|v| v.as_str()),
            Some("started")
        );
        assert_eq!(
            event.payload().get("lifetime").and_then(|v| v.as_str()),
            Some("daemon")
        );
        assert!(
            !event.payload().to_string().contains(CANARY),
            "started payload must not carry process output"
        );
    }

    #[test]
    fn complete_journals_completed_and_stores_artifact_refs() {
        let mut tmp = TempReg::create();
        let job = spec(tmp.session, JobLifetime::Daemon);
        let id = tmp
            .registry
            .start_job(job, identity(), &meta(), &live())
            .expect("start");
        let out = outputs_with_canary();
        let done = tmp
            .registry
            .complete_job(
                id,
                JobOutcome::Completed {
                    exit_code: Some(0),
                    signal: None,
                },
                out.clone(),
                &meta(),
                &live(),
            )
            .expect("complete");
        assert_eq!(done.state(), JobState::Completed);
        assert_eq!(done.exit_code(), Some(0));
        assert_eq!(
            done.outputs()
                .expect("outputs")
                .stdout()
                .artifact()
                .map(|a| a.id),
            out.stdout().artifact().map(|a| a.id)
        );
        assert!(tmp.registry.tombstone(id).is_err());
        assert!(!tmp.registry.tombstone_path(id).is_file());

        let event = tmp
            .ledger
            .get(
                tmp.session,
                done.completed_seq().expect("seq"),
                &LedgerCancel::new(),
            )
            .expect("completed event");
        assert_eq!(event.kind(), EventKind::JobCompleted);
        assert_eq!(
            event.payload().get("exit_status").and_then(|v| v.as_i64()),
            Some(0)
        );
        assert!(
            !event.payload().to_string().contains(CANARY),
            "completed payload must store artifact ids, not excerpts"
        );
    }

    #[test]
    fn reopen_is_reconnect_safe_for_running_and_completed() {
        let mut tmp = TempReg::create();
        let running = spec(tmp.session, JobLifetime::Daemon);
        let finished = spec(tmp.session, JobLifetime::Client);
        let running_id = tmp
            .registry
            .start_job(running, identity(), &meta(), &live())
            .expect("running");
        let finished_id = tmp
            .registry
            .start_job(finished, identity(), &meta(), &live())
            .expect("finished");
        tmp.registry
            .complete_job(
                finished_id,
                JobOutcome::Failed {
                    exit_code: Some(2),
                    signal: None,
                },
                JobOutputs::empty(),
                &meta(),
                &live(),
            )
            .expect("complete");

        let reopened = tmp.reopen();
        let live_job = reopened.attach(running_id).expect("attach running");
        assert_eq!(live_job.state(), JobState::Running);
        assert_eq!(live_job.lifetime(), JobLifetime::Daemon);
        assert_eq!(live_job.identity().expect("id").pid(), 42);
        assert!(reopened.tombstone(running_id).is_ok());

        let done = reopened.get(finished_id).expect("completed");
        assert_eq!(done.state(), JobState::Failed);
        assert_eq!(done.exit_code(), Some(2));
        assert!(done.tombstone().is_none());
    }

    #[test]
    fn tui_disconnect_does_not_cancel_daemon_owned_job() {
        let mut tmp = TempReg::create();
        let daemon = spec(tmp.session, JobLifetime::Daemon);
        let client = spec(tmp.session, JobLifetime::Client);
        let daemon_id = tmp
            .registry
            .start_job(daemon, identity(), &meta(), &live())
            .expect("daemon");
        let client_id = tmp
            .registry
            .start_job(client, identity(), &meta(), &live())
            .expect("client");

        let report = tmp
            .registry
            .on_client_disconnect(tmp.session, &meta(), &live())
            .expect("disconnect");
        assert_eq!(report.cancelled(), &[client_id]);
        assert_eq!(report.still_running(), &[daemon_id]);
        assert_eq!(
            tmp.registry.get(daemon_id).expect("daemon").state(),
            JobState::Running
        );
        assert_eq!(
            tmp.registry.get(client_id).expect("client").state(),
            JobState::Cancelled
        );

        let reopened = tmp.reopen();
        assert_eq!(
            reopened.get(daemon_id).expect("reopen daemon").state(),
            JobState::Running
        );
        assert_eq!(
            reopened.get(client_id).expect("reopen client").state(),
            JobState::Cancelled
        );
    }

    #[test]
    fn client_job_follows_session_close() {
        let mut tmp = TempReg::create();
        let other = SessionId::new();
        tmp.ledger
            .create_session(other, ProjectId::new(), &LedgerCancel::new())
            .expect("other session");
        let local = spec(tmp.session, JobLifetime::Client);
        let foreign = spec(other, JobLifetime::Client);
        let local_id = tmp
            .registry
            .start_job(local, identity(), &meta(), &live())
            .expect("local");
        let foreign_id = tmp
            .registry
            .start_job(foreign, identity(), &meta(), &live())
            .expect("foreign");

        let report = tmp
            .registry
            .on_session_close(tmp.session, &meta(), &live())
            .expect("close");
        assert_eq!(report.cancelled(), &[local_id]);
        assert!(report.still_running().is_empty());
        assert_eq!(
            tmp.registry.get(local_id).expect("local").state(),
            JobState::Cancelled
        );
        assert_eq!(
            tmp.registry.get(foreign_id).expect("foreign").state(),
            JobState::Running
        );
    }

    #[test]
    fn cancelled_start_does_not_journal_or_leave_tombstone() {
        let mut tmp = TempReg::create();
        let job = spec(tmp.session, JobLifetime::Daemon);
        let id = job.job_id();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tmp
            .registry
            .start_job(job, identity(), &meta(), &cancel)
            .expect_err("cancelled");
        assert_eq!(err, JobError::Cancelled);
        assert_eq!(err.error_code(), None);
        assert!(tmp.registry.get(id).is_err());
        assert!(!tmp.registry.tombstone_path(id).is_file());
        assert_eq!(
            tmp.ledger
                .last_seq(tmp.session, &LedgerCancel::new())
                .expect("seq"),
            0
        );
    }

    #[test]
    fn unknown_session_fails_closed_without_tombstone() {
        let mut tmp = TempReg::create();
        let missing = SessionId::new();
        let job = spec(missing, JobLifetime::Daemon);
        let id = job.job_id();
        let err = tmp
            .registry
            .start_job(job, identity(), &meta(), &live())
            .expect_err("missing session");
        assert_eq!(err, JobError::SessionNotFound);
        assert_eq!(err.error_code(), Some(ErrorCode::SessionNotFound));
        assert!(!tmp.registry.tombstone_path(id).is_file());
    }

    #[test]
    fn broadcast_process_identity_is_rejected() {
        assert_eq!(
            ProcessIdentity::new(0, 9, 1).expect_err("pid 0"),
            JobError::InvalidIdentity
        );
        assert_eq!(
            ProcessIdentity::new(9, 1, 1).expect_err("pgid 1"),
            JobError::InvalidIdentity
        );
    }

    #[test]
    fn lifetime_tamper_on_disk_fails_closed() {
        let mut tmp = TempReg::create();
        let job = spec(tmp.session, JobLifetime::Client);
        let id = tmp
            .registry
            .start_job(job, identity(), &meta(), &live())
            .expect("start");
        let path = tmp.registry.record_path(id);
        let raw = fs::read_to_string(&path).expect("record");
        let tampered = raw.replace("\"client\"", "\"daemon\"");
        assert_ne!(raw, tampered);
        fs::write(&path, tampered).expect("tamper");
        let err = JobRegistry::open(&tmp.root, tmp.ledger.clone()).expect_err("tamper");
        assert_eq!(err, JobError::Corrupt);
        assert_eq!(err.error_code(), Some(ErrorCode::StorageCorrupt));
    }

    #[test]
    fn duplicate_start_and_double_complete_fail_closed() {
        let mut tmp = TempReg::create();
        let job = spec(tmp.session, JobLifetime::Daemon);
        let again = job.clone();
        let id = tmp
            .registry
            .start_job(job, identity(), &meta(), &live())
            .expect("start");
        let err = tmp
            .registry
            .start_job(again, identity(), &meta(), &live())
            .expect_err("dup");
        assert_eq!(err, JobError::AlreadyExists);
        tmp.registry
            .complete_job(
                id,
                JobOutcome::Completed {
                    exit_code: Some(0),
                    signal: None,
                },
                JobOutputs::empty(),
                &meta(),
                &live(),
            )
            .expect("complete");
        let err = tmp
            .registry
            .complete_job(
                id,
                JobOutcome::Completed {
                    exit_code: Some(0),
                    signal: None,
                },
                JobOutputs::empty(),
                &meta(),
                &live(),
            )
            .expect_err("again");
        assert_eq!(err, JobError::AlreadyTerminal);
    }

    #[test]
    fn debug_and_errors_do_not_echo_canary() {
        let mut tmp = TempReg::create();
        let job = spec_with_argv(tmp.session, JobLifetime::Daemon, &["/bin/echo", CANARY]);
        let id = tmp
            .registry
            .start_job(job, identity(), &meta(), &live())
            .expect("start");
        tmp.registry
            .complete_job(
                id,
                JobOutcome::Completed {
                    exit_code: Some(0),
                    signal: None,
                },
                outputs_with_canary(),
                &meta(),
                &live(),
            )
            .expect("complete");
        let snap = tmp.registry.get(id).expect("get");
        let shown = format!(
            "{:?} {:?} {:?} {:?} {:?}",
            tmp.registry,
            snap,
            snap.spec(),
            snap.outputs(),
            JobError::Corrupt
        );
        assert!(!shown.contains(CANARY), "{shown}");
        assert!(!JobError::Corrupt.to_string().contains(CANARY));
    }

    #[test]
    fn empty_spec_and_oversized_argv_fail_closed() {
        let err = JobSpec::new(
            JobId::new(),
            SessionId::new(),
            JobLifetime::Client,
            LeaseId::new(),
            PersistedInvocation::Argv { argv: Vec::new() },
            "/tmp",
            None::<String>,
            0,
            None,
            1,
            "test",
        )
        .expect_err("empty argv");
        assert_eq!(err, JobError::InvalidSpec);
        let huge = "x".repeat(MAX_ARG_BYTES + 1);
        let err = JobSpec::new(
            JobId::new(),
            SessionId::new(),
            JobLifetime::Daemon,
            LeaseId::new(),
            PersistedInvocation::Argv {
                argv: vec!["/bin/echo".to_owned(), huge],
            },
            "/tmp",
            None::<String>,
            0,
            None,
            1,
            "test",
        )
        .expect_err("huge arg");
        assert_eq!(err, JobError::InvalidSpec);
    }
}
