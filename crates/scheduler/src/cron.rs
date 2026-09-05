//! Prompt cron facade over the durable [`CronStore`].
//!
//! The facade owns schedule semantics: `add` refuses to store a job whose
//! schedule does not parse, and `poll` re-validates every claimed row at
//! claim time. A row whose stored schedule no longer parses is quarantined
//! ("kept, not loaded") instead of failing the whole poll — a corrupted or
//! hand-edited row must never take down the fire loop or fire blind.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use capability_broker::CancellationToken;
use event_ledger::cron::{CronJob, CronStore, CronStoreError};
use process_supervisor::Schedule;

/// Wire identity of the facade's poll report.
pub const CRON_FACADE_SCHEMA: &str = "rapidlm.cron.facade.v1";

/// A `firing` row older than this is treated as orphaned by a crashed
/// poller. The tightest five-field cadence is one minute, so one minute of
/// lease never overlaps the next legitimate fire.
pub const FIRING_LEASE_TIMEOUT_MS: i64 = 60_000;

/// Upper bound on jobs claimed per poll so one pathological database cannot
/// make a single tick unbounded.
pub const MAX_POLL_BATCH: usize = 64;

/// Quarantine reason recorded when a stored schedule no longer parses.
pub const QUARANTINE_UNPARSEABLE_SCHEDULE: &str =
    "stored schedule no longer parses; kept, not loaded";

/// Consecutive prompt-*execution* failures (distinct from the unparseable-
/// schedule case above, which `poll()` itself already catches) before
/// [`PromptCron::report_execution`] auto-quarantines a job. A defensible
/// numeric default, not a contested product question — chosen the same way
/// this codebase's other per-turn ceilings are (e.g. `MAX_SUBAGENT_SPAWNS_
/// PER_TURN`): "three strikes" stops an unattended job from firing and
/// failing forever without quarantining on one transient blip.
pub const MAX_CONSECUTIVE_EXECUTION_FAILURES: u32 = 3;

/// Typed failures of the cron facade.
#[derive(Debug)]
pub enum CronError {
    Cancelled,
    Store(CronStoreError),
    Schedule(process_supervisor::ScheduleError),
}

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cron operation cancelled"),
            Self::Store(err) => write!(f, "cron store: {err}"),
            Self::Schedule(err) => {
                write!(f, "schedule rejected: {}", err.as_str())
            }
        }
    }
}

impl std::error::Error for CronError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::Schedule(err) => Some(err),
            Self::Cancelled => None,
        }
    }
}

impl From<CronStoreError> for CronError {
    fn from(value: CronStoreError) -> Self {
        Self::Store(value)
    }
}

impl From<process_supervisor::ScheduleError> for CronError {
    fn from(value: process_supervisor::ScheduleError) -> Self {
        Self::Schedule(value)
    }
}

/// One prompt the host should execute after a successful poll. The row is
/// already back in the `active` state with its next fire time recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct DuePrompt {
    pub id: String,
    pub prompt: String,
    pub session_id: Option<String>,
    pub fired_at_ms: i64,
}

/// Outcome of one [`PromptCron::poll`] tick.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PollReport {
    /// Prompts due for execution this tick.
    pub fired: Vec<DuePrompt>,
    /// Rows quarantined because their schedule no longer parses.
    pub quarantined: usize,
    /// Stale `firing` rows requeued by crash recovery.
    pub requeued: usize,
}

/// Outcome of one [`PromptCron::report_execution`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionReport {
    /// The job's consecutive-failure streak after this report.
    pub consecutive_failures: u32,
    /// Whether this report crossed [`MAX_CONSECUTIVE_EXECUTION_FAILURES`]
    /// and the job was just quarantined as a result.
    pub quarantined: bool,
}

/// Claim-lease prompt cron. Storage lives in [`CronStore`]; this type adds
/// schedule validation, fire-time derivation, and the poll loop.
#[derive(Clone, Debug)]
pub struct PromptCron {
    store: CronStore,
}

impl PromptCron {
    /// Open (or create) the facade over the ledger database at `path`.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, CronError> {
        Ok(Self {
            store: CronStore::open(path)?,
        })
    }

    /// Storage access for operators (`list`, `remove`, `quarantine`).
    pub fn store(&self) -> &CronStore {
        &self.store
    }

    /// Validate and store a new job. The schedule must parse and must have a
    /// fire time strictly after `now_ms`; nothing is persisted on failure.
    pub fn add(
        &self,
        prompt: &str,
        session_id: Option<&str>,
        schedule_text: &str,
        now_ms: i64,
        cancel: &CancellationToken,
    ) -> Result<CronJob, CronError> {
        if cancel.is_cancelled() {
            return Err(CronError::Cancelled);
        }
        let schedule = Schedule::parse(schedule_text)?;
        let first_fire = schedule.next_fire_after(unix_ms_to_system_time(now_ms), cancel)?;
        let next_fire_ms = system_time_to_unix_ms(first_fire);
        Ok(self
            .store
            .add(prompt, session_id, schedule_text, next_fire_ms, now_ms)?)
    }

    /// Remove a job by id. Returns `false` when the id is unknown.
    pub fn remove(&self, id: &str) -> Result<bool, CronError> {
        Ok(self.store.remove(id)?)
    }

    /// All jobs, quarantined rows included.
    pub fn list(&self) -> Result<Vec<CronJob>, CronError> {
        Ok(self.store.list()?)
    }

    /// Quarantine a job: kept, not loaded.
    pub fn quarantine(&self, id: &str, reason: &str, now_ms: i64) -> Result<(), CronError> {
        Ok(self.store.quarantine(id, reason, now_ms)?)
    }

    /// Record whether a fired job's prompt execution actually succeeded,
    /// and auto-quarantine after [`MAX_CONSECUTIVE_EXECUTION_FAILURES`] in a
    /// row — closing the gap `poll()`'s own module doc names: completing a
    /// job's lease only ever meant "the schedule re-parsed," never "the
    /// prompt's execution succeeded," so a job whose prompt failed on every
    /// real run kept firing forever with nothing to stop it. The caller
    /// (whoever actually ran the fired prompt, outside this crate) reports
    /// the real outcome here; this method owns the "how many failures
    /// before quarantine" policy, the same "detection vs. policy" split
    /// `poll()` already uses for the unparseable-schedule case.
    pub fn report_execution(
        &self,
        id: &str,
        succeeded: bool,
        now_ms: i64,
    ) -> Result<ExecutionReport, CronError> {
        let consecutive_failures = self.store.record_execution_result(id, succeeded, now_ms)?;
        if !succeeded && consecutive_failures >= MAX_CONSECUTIVE_EXECUTION_FAILURES {
            self.store.quarantine(
                id,
                &format!(
                    "quarantined after {consecutive_failures} consecutive execution failures"
                ),
                now_ms,
            )?;
            return Ok(ExecutionReport {
                consecutive_failures,
                quarantined: true,
            });
        }
        Ok(ExecutionReport {
            consecutive_failures,
            quarantined: false,
        })
    }

    /// Crash recovery: requeue stale `firing` rows. Returns the count.
    pub fn requeue_orphaned(&self, now_ms: i64) -> Result<usize, CronError> {
        Ok(self.store.requeue_orphaned(now_ms, FIRING_LEASE_TIMEOUT_MS)? as usize)
    }

    /// One fire-loop tick:
    ///
    /// 1. requeue orphaned `firing` rows (crash recovery),
    /// 2. atomically claim every due `active` row (claim lease),
    /// 3. per row: re-parse the stored schedule — quarantine on failure —
    ///    otherwise derive the next fire time and complete the lease,
    /// 4. return the prompts due for execution.
    ///
    /// If next-fire derivation is cancelled mid-tick the claimed row stays
    /// `firing` until [`Self::requeue_orphaned`] rescues it after the lease
    /// timeout; it is never lost.
    pub fn poll(
        &self,
        now_ms: i64,
        cancel: &CancellationToken,
        max_jobs: usize,
    ) -> Result<PollReport, CronError> {
        if cancel.is_cancelled() {
            return Err(CronError::Cancelled);
        }
        let max_jobs = max_jobs.min(MAX_POLL_BATCH);
        let requeued = self.requeue_orphaned(now_ms)?;
        let claimed = self.store.claim_due(now_ms, max_jobs)?;
        let mut fired = Vec::with_capacity(claimed.len());
        let mut quarantined = 0usize;
        for job in claimed {
            if cancel.is_cancelled() {
                return Err(CronError::Cancelled);
            }
            match Schedule::parse(&job.schedule) {
                Ok(schedule) => {
                    let next = schedule
                        .next_fire_after(unix_ms_to_system_time(now_ms), cancel)
                        .map_err(CronError::Schedule)?;
                    let next_ms = system_time_to_unix_ms(next);
                    self.store.complete(&job.id, next_ms, now_ms)?;
                    fired.push(DuePrompt {
                        id: job.id,
                        prompt: job.prompt,
                        session_id: job.session_id,
                        fired_at_ms: now_ms,
                    });
                }
                Err(_) => {
                    self.store
                        .quarantine(&job.id, QUARANTINE_UNPARSEABLE_SCHEDULE, now_ms)?;
                    quarantined += 1;
                }
            }
        }
        Ok(PollReport {
            fired,
            quarantined,
            requeued,
        })
    }
}

fn unix_ms_to_system_time(ms: i64) -> SystemTime {
    if ms <= 0 {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH + Duration::from_millis(ms as u64)
}

fn system_time_to_unix_ms(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn open_cron() -> (PromptCron, Self) {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-cron-facade-{}-{seq}.sqlite",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            let cron = PromptCron::open(&path).expect("open prompt cron");
            (cron, Self { path })
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(sidecar(&self.path, "-wal"));
            let _ = std::fs::remove_file(sidecar(&self.path, "-shm"));
        }
    }

    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut owned = path.as_os_str().to_owned();
        owned.push(suffix);
        PathBuf::from(owned)
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    /// A fixed "now" far from epoch edge cases: 2023-11-14T22:13:20Z.
    const NOW_MS: i64 = 1_700_000_000_000;

    #[test]
    fn add_rejects_unparseable_schedule_and_stores_nothing() {
        let (cron, _db) = TempDb::open_cron();
        let err = cron
            .add("hello", None, "not a schedule", NOW_MS, &live())
            .expect_err("bad schedule");
        assert!(matches!(err, CronError::Schedule(_)));
        assert!(cron.list().expect("list").is_empty());
    }

    #[test]
    fn add_derives_the_first_fire_time_from_the_schedule() {
        let (cron, _db) = TempDb::open_cron();
        let job = cron
            .add("hello", None, "*/5 * * * *", NOW_MS, &live())
            .expect("add");
        assert!(job.next_fire_at_ms > NOW_MS);
        // First fire lands on a 5-minute boundary after `now`.
        let five_min: i64 = 5 * 60 * 1000;
        assert_eq!(job.next_fire_at_ms % five_min, 0);
        // Stored schedule text is preserved verbatim for audit.
        assert_eq!(job.schedule, "*/5 * * * *");
    }

    #[test]
    fn poll_before_due_fires_nothing_and_keeps_the_row_active() {
        let (cron, _db) = TempDb::open_cron();
        cron.add("hello", None, "*/5 * * * *", NOW_MS, &live())
            .expect("add");
        let report = cron.poll(NOW_MS, &live(), 10).expect("poll");
        assert!(report.fired.is_empty());
        assert_eq!(report.quarantined, 0);
        let job = &cron.list().expect("list")[0];
        assert_eq!(job.status, event_ledger::cron::CronJobStatus::Active);
    }

    #[test]
    fn poll_after_due_fires_the_prompt_and_reschedules() {
        let (cron, _db) = TempDb::open_cron();
        let job = cron
            .add("hello", Some("session-1"), "*/1 * * * *", NOW_MS, &live())
            .expect("add");
        let due_ms = job.next_fire_at_ms;
        let report = cron.poll(due_ms, &live(), 10).expect("poll");
        assert_eq!(report.fired.len(), 1);
        assert_eq!(report.fired[0].prompt, "hello");
        assert_eq!(report.fired[0].session_id.as_deref(), Some("session-1"));
        assert_eq!(report.fired[0].id, job.id);
        // The row is back to active with the next per-minute boundary.
        let done = cron.list().expect("list").remove(0);
        assert_eq!(done.status, event_ledger::cron::CronJobStatus::Active);
        assert!(done.next_fire_at_ms > due_ms);
        assert_eq!(done.next_fire_at_ms - due_ms, 60_000);
        // An immediate second poll does not double-fire.
        let again = cron.poll(due_ms, &live(), 10).expect("poll again");
        assert!(again.fired.is_empty());
    }

    #[test]
    fn poll_quarantines_a_row_whose_schedule_stopped_parsing() {
        let (cron, _db) = TempDb::open_cron();
        // A row written directly through the store bypasses facade
        // validation: the model for hand-edited or schema-skewed rows.
        cron.store()
            .add("hello", None, "* * * garbage", NOW_MS, NOW_MS)
            .expect("raw add");
        let report = cron.poll(NOW_MS, &live(), 10).expect("poll");
        assert!(report.fired.is_empty());
        assert_eq!(report.quarantined, 1);
        let job = &cron.list().expect("list")[0];
        assert_eq!(job.status, event_ledger::cron::CronJobStatus::Quarantined);
        assert_eq!(
            job.quarantine_reason.as_deref(),
            Some(QUARANTINE_UNPARSEABLE_SCHEDULE)
        );
        // Quarantined rows stay quarantined on later polls: kept, not loaded.
        let later = cron.poll(NOW_MS + 1, &live(), 10).expect("poll later");
        assert!(later.fired.is_empty());
    }

    #[test]
    fn report_execution_auto_quarantines_after_max_consecutive_failures() {
        let (cron, _db) = TempDb::open_cron();
        let job = cron
            .add("run checks", None, "*/5 * * * *", NOW_MS, &live())
            .expect("add");

        for n in 1..MAX_CONSECUTIVE_EXECUTION_FAILURES {
            let report = cron
                .report_execution(&job.id, false, NOW_MS + i64::from(n))
                .expect("report");
            assert_eq!(report.consecutive_failures, n);
            assert!(
                !report.quarantined,
                "must not quarantine before the threshold is reached (failure {n})"
            );
            let live_job = cron.store().get(&job.id).expect("get");
            assert_eq!(live_job.status, event_ledger::cron::CronJobStatus::Active);
        }

        let final_report = cron
            .report_execution(
                &job.id,
                false,
                NOW_MS + i64::from(MAX_CONSECUTIVE_EXECUTION_FAILURES),
            )
            .expect("report");
        assert_eq!(
            final_report.consecutive_failures,
            MAX_CONSECUTIVE_EXECUTION_FAILURES
        );
        assert!(final_report.quarantined, "the threshold-crossing report must quarantine");
        let quarantined_job = cron.store().get(&job.id).expect("get");
        assert_eq!(
            quarantined_job.status,
            event_ledger::cron::CronJobStatus::Quarantined
        );
        assert!(
            quarantined_job
                .quarantine_reason
                .as_deref()
                .unwrap_or_default()
                .contains("consecutive execution failures"),
            "{:?}",
            quarantined_job.quarantine_reason
        );
    }

    #[test]
    fn report_execution_success_resets_the_streak_and_never_quarantines() {
        let (cron, _db) = TempDb::open_cron();
        let job = cron
            .add("run checks", None, "*/5 * * * *", NOW_MS, &live())
            .expect("add");

        for n in 0..MAX_CONSECUTIVE_EXECUTION_FAILURES - 1 {
            cron.report_execution(&job.id, false, NOW_MS + i64::from(n))
                .expect("report failure");
        }
        // A success right before the threshold resets the streak entirely —
        // the job must never be quarantined by this sequence.
        let reset = cron
            .report_execution(&job.id, true, NOW_MS + 100)
            .expect("report success");
        assert_eq!(reset.consecutive_failures, 0);
        assert!(!reset.quarantined);
        let live_job = cron.store().get(&job.id).expect("get");
        assert_eq!(live_job.status, event_ledger::cron::CronJobStatus::Active);
        assert_eq!(live_job.consecutive_failures, 0);
    }

    #[test]
    fn requeue_orphaned_recovers_a_row_stranded_mid_poll() {
        let (cron, _db) = TempDb::open_cron();
        let job = cron
            .add("hello", None, "*/1 * * * *", NOW_MS, &live())
            .expect("add");
        // Simulate a crash between claim and complete: claim through the
        // raw store at the job's first fire time and never complete.
        let claimed = cron.store().claim_due(job.next_fire_at_ms, 10).expect("claim");
        assert_eq!(claimed.len(), 1);
        // A tick before the lease timeout does not steal the live lease.
        let early = cron
            .requeue_orphaned(job.next_fire_at_ms + 1_000)
            .expect("early sweep");
        assert_eq!(early, 0);
        // After the timeout the row is active again and can fire.
        let swept = cron
            .requeue_orphaned(job.next_fire_at_ms + FIRING_LEASE_TIMEOUT_MS + 1_000)
            .expect("sweep");
        assert_eq!(swept, 1);
        let report = cron
            .poll(job.next_fire_at_ms + FIRING_LEASE_TIMEOUT_MS + 60_000, &live(), 10)
            .expect("poll after recovery");
        assert_eq!(report.fired.len(), 1);
    }

    #[test]
    fn cancel_stops_poll_before_any_claim() {
        let (cron, _db) = TempDb::open_cron();
        cron.store()
            .add("hello", None, "*/1 * * * *", NOW_MS, NOW_MS - 1)
            .expect("raw add due");
        let token = CancellationToken::new();
        token.cancel();
        let err = cron.poll(NOW_MS, &token, 10).expect_err("cancelled");
        assert!(matches!(err, CronError::Cancelled));
        // Nothing was claimed; the row is still active and due.
        let job = &cron.list().expect("list")[0];
        assert_eq!(job.status, event_ledger::cron::CronJobStatus::Active);
    }
}
