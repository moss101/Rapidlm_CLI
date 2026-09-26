//! Background jobs a dead host left running (SEAM-03, `job.orphan_reconciled`).
//!
//! A host (the TUI, the daemon, `rapid acp`) that dies without dropping a
//! session's job registry — killed, crashed — leaves that session's
//! `job.started` rows without a terminal record, and a Unix job still
//! running in its own process group with no supervisor and no timeout. When
//! a host opens such a session, it reconciles them: each non-terminal job
//! whose recorded host process is gone is examined through
//! `process_supervisor::reconcile_orphans` (the recorded pid, process group
//! and start time must all match before anything is signalled — a pid alone
//! never is), and gets one `job.orphan_reconciled` record saying what was
//! found and done. A job whose host is still running belongs to that host
//! and is left alone.

use event_ledger::event::{ActorRef, EventKind};
use kernel::InProcessKernelClient;
use process_supervisor::{JobLifetime, OrphanJob, ProcessIdentity, ReconcileDecision};
use protocol::TraceId;

/// One non-terminal job as the session's records describe it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OpenJob {
    pub job_id: protocol::JobId,
    /// The host process that started it (`None` when an older record did
    /// not say).
    pub host_pid: Option<u32>,
    /// Its own process, when it had one: (pid, process group, start ms).
    pub process: Option<(u32, u32, u64)>,
}

/// The session's jobs with a `job.started` and no terminal record, oldest
/// first, from its exported events (`(kind, payload)` pairs).
pub(crate) fn open_jobs(events: &[(String, serde_json::Value)]) -> Vec<OpenJob> {
    let mut open: Vec<OpenJob> = Vec::new();
    for (kind, payload) in events {
        let Some(job_id) = payload
            .get("job_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| raw.parse::<protocol::JobId>().ok())
        else {
            continue;
        };
        match kind.as_str() {
            "job.started" => {
                let u32_of = |value: &serde_json::Value, field: &str| {
                    value
                        .get(field)
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|raw| u32::try_from(raw).ok())
                };
                let process = payload.get("process").and_then(|process| {
                    Some((
                        u32_of(process, "pid")?,
                        u32_of(process, "process_group")?,
                        process
                            .get("started_unix_ms")
                            .and_then(serde_json::Value::as_u64)?,
                    ))
                });
                let host_pid = u32_of(payload, "host_pid").or_else(|| {
                    payload
                        .get("process")
                        .and_then(|process| u32_of(process, "host_pid"))
                });
                open.retain(|job| job.job_id != job_id);
                open.push(OpenJob {
                    job_id,
                    host_pid,
                    process,
                });
            }
            "job.completed" | "job.orphan_reconciled" => {
                open.retain(|job| job.job_id != job_id);
            }
            _ => {}
        }
    }
    open
}

/// What reconciliation found for one job, as its record says.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Reconciled {
    pub job_id: protocol::JobId,
    /// `terminated` (its process tree was still running and is stopped),
    /// `exited` (its process was gone), `lost` (it had no process of its
    /// own to look at), `blocked:<why>` (the process at its pid is not
    /// provably it; nothing was signalled), or `failed:<why>` (it was
    /// examined and stopping it did not finish — a signal may have been
    /// sent, as when its tree outlived the `KILL`).
    pub outcome: String,
}

/// Reconcile `jobs` whose host is gone (`host_alive` says whether a pid is a
/// running process; this host's own pid always counts as alive).
pub(crate) fn reconcile(
    jobs: &[OpenJob],
    host_alive: &dyn Fn(u32) -> bool,
    probe: &dyn process_supervisor::ProcessProbe,
    killer: &dyn process_supervisor::ProcessTreeKiller,
) -> Vec<Reconciled> {
    let me = std::process::id();
    let mut out = Vec::new();
    for job in jobs {
        // Owned by a host that is still running (this one, or another on
        // this machine): not an orphan.
        let host_gone = match job.host_pid {
            Some(pid) => pid != me && !host_alive(pid),
            None => false,
        };
        if !host_gone {
            continue;
        }
        let Some((pid, group, started)) = job.process else {
            out.push(Reconciled {
                job_id: job.job_id,
                outcome: "lost".to_owned(),
            });
            continue;
        };
        let outcome = ProcessIdentity::new(pid, group, started)
            .ok()
            .and_then(|identity| OrphanJob::new(job.job_id, JobLifetime::Client, identity).ok())
            .map(|orphan| {
                match process_supervisor::reconcile_orphans(
                    &[orphan],
                    &Probe(probe),
                    &Killer(killer),
                    &capability_broker::CancellationToken::new(),
                ) {
                    Ok(report) => match report.outcomes().first().map(|o| o.decision()) {
                        Some(ReconcileDecision::Terminated) => "terminated".to_owned(),
                        Some(ReconcileDecision::Orphaned) => "exited".to_owned(),
                        Some(ReconcileDecision::Readopted) => "readopted".to_owned(),
                        Some(ReconcileDecision::Blocked { warning }) => {
                            format!("blocked:{}", warning.as_str())
                        }
                        None => "exited".to_owned(),
                    },
                    Err(err) => format!("failed:{}", err.as_str().replace(' ', "_")),
                }
            })
            .unwrap_or_else(|| "blocked:invalid_identity".to_owned());
        out.push(Reconciled {
            job_id: job.job_id,
            outcome,
        });
    }
    out
}

/// Adapters from trait objects to the generic reconciler.
struct Probe<'a>(&'a dyn process_supervisor::ProcessProbe);
impl process_supervisor::ProcessProbe for Probe<'_> {
    fn observe(
        &self,
        recorded: ProcessIdentity,
        cancel: &capability_broker::CancellationToken,
    ) -> Result<process_supervisor::ProcessObservation, process_supervisor::RecoveryError> {
        self.0.observe(recorded, cancel)
    }
}
struct Killer<'a>(&'a dyn process_supervisor::ProcessTreeKiller);
impl process_supervisor::ProcessTreeKiller for Killer<'_> {
    fn terminate_owned(
        &self,
        identity: ProcessIdentity,
        cancel: &capability_broker::CancellationToken,
    ) -> Result<(), process_supervisor::RecoveryError> {
        self.0.terminate_owned(identity, cancel)
    }
}

/// Every `job.*` record of `session`, oldest first, in one read however
/// long the session is — the bounded export refuses a session past 10 000
/// events, and the long-lived sessions are the ones that run background
/// jobs. `None` when they cannot be read: a partial read could miss a
/// `job.completed` and make a finished job look open.
pub(crate) fn job_events(
    client: &InProcessKernelClient,
    session: protocol::SessionId,
) -> Option<Vec<(String, serde_json::Value)>> {
    let events = client.events_of_kind(session, "job.").ok()?;
    Some(
        events
            .iter()
            .map(|event| (event.kind().as_str().to_owned(), event.payload().clone()))
            .collect(),
    )
}

/// Open `session` for this host: reconcile the jobs a dead host left, one
/// `job.orphan_reconciled` record each. Unix only — elsewhere no job has a
/// process group of its own and a host's liveness cannot be read, so
/// nothing is judged. Best effort: a session whose records cannot all be
/// read, or a record that does not land, leaves the rows as they were.
pub(crate) fn reconcile_session(
    client: &InProcessKernelClient,
    session: protocol::SessionId,
    actor: &ActorRef,
) -> Vec<Reconciled> {
    if !cfg!(unix) {
        return Vec::new();
    }
    let Some(events) = job_events(client, session) else {
        return Vec::new();
    };
    let jobs = open_jobs(&events);
    if jobs.is_empty() {
        return Vec::new();
    }
    let reconciled = reconcile(
        &jobs,
        &process_signal::process_exists,
        &process_supervisor::HostProcessProbe,
        &process_supervisor::HostProcessKiller,
    );
    // What was recorded: a record that did not land is not reported as one
    // (a caller counts them against the session's tip).
    reconciled
        .into_iter()
        .filter(|record| {
            client
                .append_turn_progress(
                    session,
                    actor,
                    TraceId::new(),
                    EventKind::JobOrphanReconciled,
                    serde_json::json!({
                        "job_id": record.job_id.to_string(),
                        "state": "orphan_reconciled",
                        "outcome": record.outcome,
                    }),
                )
                .is_ok()
        })
        .collect()
}

/// `session`'s job registry in a host serving many sessions (the daemon,
/// `rapid acp`): on its first use in this host, the jobs a dead host left
/// in it are reconciled before the registry is handed out.
pub(crate) fn open_session_jobs(
    jobs: &crate::exec_tools::SessionJobs,
    client: &InProcessKernelClient,
    session: protocol::SessionId,
    actor: &ActorRef,
) -> crate::exec_tools::JobRegistry {
    let (registry, first_use) = jobs.open(session);
    if first_use {
        reconcile_session(client, session, actor);
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use process_supervisor::{ProcessObservation, RecoveryError};
    use std::cell::RefCell;

    fn job(host_pid: Option<u32>, process: Option<(u32, u32, u64)>) -> OpenJob {
        OpenJob {
            job_id: protocol::JobId::new(),
            host_pid,
            process,
        }
    }

    #[test]
    fn open_jobs_are_the_started_ones_without_a_terminal_record() {
        let (a, b, c) = (
            protocol::JobId::new(),
            protocol::JobId::new(),
            protocol::JobId::new(),
        );
        let events = vec![
            (
                "job.started".to_owned(),
                serde_json::json!({"job_id": a.to_string(), "host_pid": 7,
                    "process": {"pid": 40, "process_group": 40, "started_unix_ms": 9}}),
            ),
            (
                "job.started".to_owned(),
                serde_json::json!({"job_id": b.to_string(), "host_pid": 7}),
            ),
            (
                "job.completed".to_owned(),
                serde_json::json!({"job_id": a.to_string()}),
            ),
            (
                "job.started".to_owned(),
                serde_json::json!({"job_id": c.to_string()}),
            ),
            (
                "job.orphan_reconciled".to_owned(),
                serde_json::json!({"job_id": c.to_string()}),
            ),
        ];
        assert_eq!(
            open_jobs(&events),
            vec![OpenJob {
                job_id: b,
                host_pid: Some(7),
                process: None,
            }]
        );
    }

    struct Scripted(ProcessObservation);
    impl process_supervisor::ProcessProbe for Scripted {
        fn observe(
            &self,
            _recorded: ProcessIdentity,
            _cancel: &capability_broker::CancellationToken,
        ) -> Result<ProcessObservation, RecoveryError> {
            Ok(self.0)
        }
    }
    #[derive(Default)]
    struct Kills(RefCell<Vec<u32>>);
    impl process_supervisor::ProcessTreeKiller for Kills {
        fn terminate_owned(
            &self,
            identity: ProcessIdentity,
            _cancel: &capability_broker::CancellationToken,
        ) -> Result<(), RecoveryError> {
            self.0.borrow_mut().push(identity.process_group_id());
            Ok(())
        }
    }

    #[test]
    fn only_a_dead_hosts_jobs_are_judged_and_only_a_proven_process_is_stopped() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64;
        let owned = ProcessObservation::Present {
            pid: 4242,
            process_group_id: Some(4242),
            started_unix_ms: Some(now),
        };
        let dead_host = |_| false;
        let me = std::process::id();
        // (the job, what the probe sees, the outcome, a kill expected)
        let cases: Vec<(OpenJob, ProcessObservation, Option<&str>, bool)> = vec![
            (
                job(Some(99_999), Some((4242, 4242, now))),
                owned,
                Some("terminated"),
                true,
            ),
            (
                job(Some(99_999), Some((4242, 4242, now))),
                ProcessObservation::Absent,
                Some("exited"),
                false,
            ),
            // The pid now names another process (another start time).
            (
                job(Some(99_999), Some((4242, 4242, now))),
                ProcessObservation::Present {
                    pid: 4242,
                    process_group_id: Some(4242),
                    started_unix_ms: Some(now + 60_000),
                },
                Some("blocked:"),
                false,
            ),
            (job(Some(99_999), None), owned, Some("lost"), false),
            // This host's own job, and one whose host did not say: untouched.
            (job(Some(me), Some((4242, 4242, now))), owned, None, false),
            (job(None, Some((4242, 4242, now))), owned, None, false),
        ];
        for (index, (open, observed, expected, killed)) in cases.into_iter().enumerate() {
            let kills = Kills::default();
            let result = reconcile(&[open], &dead_host, &Scripted(observed), &kills);
            match expected {
                None => assert!(result.is_empty(), "case {index}: {result:?}"),
                Some(outcome) => {
                    assert_eq!(result.len(), 1, "case {index}");
                    assert!(
                        result[0].outcome.starts_with(outcome),
                        "case {index}: {result:?}"
                    );
                }
            }
            assert_eq!(!kills.0.borrow().is_empty(), killed, "case {index}");
        }
        // Stopping it was attempted and did not finish: not `blocked`, which
        // promises nothing was signalled.
        struct Survives;
        impl process_supervisor::ProcessTreeKiller for Survives {
            fn terminate_owned(
                &self,
                _: ProcessIdentity,
                _: &capability_broker::CancellationToken,
            ) -> Result<(), RecoveryError> {
                Err(RecoveryError::TreeStillAlive)
            }
        }
        let survived = reconcile(
            &[job(Some(99_999), Some((4242, 4242, now)))],
            &dead_host,
            &Scripted(owned),
            &Survives,
        );
        assert_eq!(survived.len(), 1);
        assert!(survived[0].outcome.starts_with("failed:"), "{survived:?}");
        // A host that is still running keeps its jobs.
        let alive = reconcile(
            &[job(Some(99_999), Some((4242, 4242, now)))],
            &|_| true,
            &Scripted(owned),
            &Kills::default(),
        );
        assert!(alive.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_dead_hosts_running_job_is_found_and_stopped() {
        // A real process in its own group, recorded as a dead host's job.
        let mut command = std::process::Command::new(test_fixtures::tool_str("sleep"));
        command.arg("30");
        process_signal::isolate_process_group(&mut command);
        let mut child = command.spawn().expect("spawn");
        let pid = child.id();
        // A real orphan's parent is init, which reaps it the moment it dies;
        // this test is the parent here, so it reaps concurrently the same way.
        let reaper = std::thread::spawn(move || child.wait().expect("reaped"));
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64;
        let dead_host = {
            let mut gone = std::process::Command::new(test_fixtures::tool_str("true"))
                .spawn()
                .expect("spawn");
            let pid = gone.id();
            gone.wait().expect("wait");
            pid
        };
        let result = reconcile(
            &[job(Some(dead_host), Some((pid, pid, started)))],
            &process_signal::process_exists,
            &process_supervisor::HostProcessProbe,
            &process_supervisor::HostProcessKiller,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].outcome, "terminated", "{result:?}");
        let status = reaper.join().expect("reaper");
        assert!(!status.success(), "stopped, not finished: {status:?}");
    }
}
