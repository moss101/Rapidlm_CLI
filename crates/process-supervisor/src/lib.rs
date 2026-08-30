#![forbid(unsafe_code)]

pub mod cancel;
pub mod jobs;
pub mod monitor;
pub mod output;
pub mod pty;
pub mod recovery;
pub mod schedule;
pub mod spawn;
pub mod trigger;

pub use cancel::{
    CancelError, DEFAULT_GRACE, DrainedStream, GracePeriod, MAX_GRACE, ProcessExit, SignalKind,
    TerminalStatus, TerminateAction, TerminateReport, TerminationCause, await_exit,
    await_exit_draining, terminate_tree,
};
pub use jobs::{
    ClientDisconnectReport, JOB_RECORD_SCHEMA, JobError, JobEventMeta, JobLifetime, JobOutcome,
    JobOutputs, JobRecord, JobRegistry, JobSnapshot, JobSpec, JobState, MAX_LIVE_JOBS,
    MAX_STORED_JOBS, OutputPointer, PersistedInvocation, ProcessIdentity, RunningTombstone,
};
pub use output::{
    FinishedSpool, MAX_INLINE_EXCERPT_BYTES, OutputCursor, OutputError, OutputLimits,
    OutputMetrics, OutputRef, OutputSpool, OutputStream,
};
pub use recovery::{
    HostProcessKiller, HostProcessProbe, MAX_SPAWN_RECORD_SKEW_MS, OrphanJob, Ownership,
    ProcessObservation, ProcessProbe, ProcessTreeKiller, RECOVERY_GRACE, ReconcileDecision,
    ReconcileOutcome, ReconcilePolicy, ReconcileReport, RecoveryError, RecoveryWarning,
    START_AHEAD_SLACK_MS, classify_identity, decide_action, reconcile_orphans, reconcile_registry,
    reconcile_registry_host,
};
pub use schedule::{
    CatchUpPolicy, Clock, FakeClock, MAX_SCHEDULE_BYTES, MAX_SEARCH_MINUTES, MIN_SCHEDULE_INTERVAL,
    SCHEDULE_SCHEMA, Schedule, ScheduleError, ScheduleSpec, SystemClock, TimeZone,
};
pub use spawn::{
    ExecBinding, ExecSpec, Invocation, JobHandle, MAX_STDIN_BYTES, ProcessGroupId,
    SHELL_COMMAND_FAMILY, SecretOrValue, SpawnError, StdinSpec, spawn,
};
