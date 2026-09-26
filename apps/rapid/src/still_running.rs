//! What a resumed session still has running (SEAM-03, the resume summary).
//!
//! On `/resume`, a TUI started on an existing session, and `rapid exec
//! --continue`, the model's next turn gets one short block naming the
//! session's work that has not ended: background jobs (monitors among
//! them), subagents, loops. It is derived from the session's records and
//! the loop store each time it is asked for — nothing is stored — and a
//! session with nothing running gets no block at all.
//!
//! "Running" is judged honestly: a job or subagent counts only while the
//! host process that recorded it is alive (this process, or another such
//! as the daemon). A dead host's jobs are reconciled when the session is
//! opened (`job_recovery`), and a subagent whose host is gone is not
//! running anywhere, whatever its last record said.

use event_ledger::event::EventKind;
use kernel::InProcessKernelClient;

/// Rows the block names before it says how many more there are.
const MAX_ROWS: usize = 20;

/// Bytes of one row: a long command or task is cut, not the block.
const MAX_ROW_BYTES: usize = 160;

/// One unfinished thing, as the block names it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunningRow {
    pub line: String,
}

/// The block for `session`, or `None` when nothing in it is running.
pub(crate) fn running_block(
    client: &InProcessKernelClient,
    session: protocol::SessionId,
    ledger: Option<&std::path::Path>,
) -> Option<String> {
    let jobs = crate::job_recovery::job_events(client, session).unwrap_or_default();
    let agents = agent_events(client, session).unwrap_or_default();
    let loops = crate::loops::session_loop_rows(ledger, session);
    let host_alive = |pid: u32| pid == std::process::id() || process_signal::process_exists(pid);
    let mut rows = running_jobs(&jobs, &host_alive);
    rows.extend(running_agents(&agents, &host_alive));
    rows.extend(loops.into_iter().map(|row| RunningRow {
        line: format!("loop {}", row.line),
    }));
    render(&rows)
}

/// The session's `agent.*` records as `(kind, payload)` pairs.
fn agent_events(
    client: &InProcessKernelClient,
    session: protocol::SessionId,
) -> Option<Vec<(String, serde_json::Value)>> {
    let events = client.events_of_kind(session, "agent.").ok()?;
    Some(
        events
            .iter()
            .map(|event| (event.kind().as_str().to_owned(), event.payload().clone()))
            .collect(),
    )
}

/// Jobs with a `job.started` and no terminal record whose host is alive.
/// A record that names no host (an older one) is listed as last recorded.
fn running_jobs(
    events: &[(String, serde_json::Value)],
    host_alive: &dyn Fn(u32) -> bool,
) -> Vec<RunningRow> {
    let text = |payload: &serde_json::Value, field: &str| {
        payload
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    crate::job_recovery::open_jobs(events)
        .into_iter()
        .filter(|job| job.host_pid.is_none_or(host_alive))
        .map(|job| {
            let id = job.job_id.to_string();
            let started = events.iter().rev().find(|(kind, payload)| {
                kind == "job.started" && text(payload, "job_id").as_deref() == Some(&id)
            });
            let handle = started
                .and_then(|(_, payload)| text(payload, "handle"))
                .unwrap_or_else(|| id.clone());
            let command = started
                .and_then(|(_, payload)| text(payload, "command"))
                .unwrap_or_default();
            let unknown = if job.host_pid.is_none() {
                " (last recorded running; its host is not recorded)"
            } else {
                ""
            };
            RunningRow {
                line: format!("job {handle} running: {command}{unknown}"),
            }
        })
        .collect()
}

/// Subagents spawned and not ended whose recorded host is alive. One with
/// no recorded host is left out: it ran inside an earlier host's turn.
fn running_agents(
    events: &[(String, serde_json::Value)],
    host_alive: &dyn Fn(u32) -> bool,
) -> Vec<RunningRow> {
    // (id, role, task, host) in spawn order.
    let mut open: Vec<(String, String, String, Option<u32>)> = Vec::new();
    for (kind, payload) in events {
        let Some(id) = payload.get("agent_id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let state = payload
            .get("state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let ended = kind == EventKind::AgentResult.as_str()
            || kind == EventKind::AgentCancelled.as_str()
            || matches!(state, "succeeded" | "failed" | "cancelled");
        if ended {
            open.retain(|(open_id, ..)| open_id != id);
        } else if kind == EventKind::AgentSpawned.as_str() {
            let field = |name: &str| {
                payload
                    .get(name)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let host = payload
                .get("host_pid")
                .and_then(serde_json::Value::as_u64)
                .and_then(|raw| u32::try_from(raw).ok());
            open.retain(|(open_id, ..)| open_id != id);
            open.push((
                id.to_owned(),
                field("role"),
                field("current_operation"),
                host,
            ));
        }
    }
    open.into_iter()
        .filter(|(.., host)| host.is_some_and(host_alive))
        .map(|(id, role, task, _)| RunningRow {
            line: format!("subagent {id} ({role}) running: {task}"),
        })
        .collect()
}

/// The block's text: a header, then one line a row, each cut to
/// [`MAX_ROW_BYTES`], at most [`MAX_ROWS`] of them.
fn render(rows: &[RunningRow]) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let mut block = String::from(
        "Still running in this session (derived from its records at resume; job_status and \
job_output read a job, /loop lists loops):\n",
    );
    for row in rows.iter().take(MAX_ROWS) {
        let clean = row.line.replace(['\n', '\r'], " ");
        block.push_str("- ");
        block.push_str(cut(&clean, MAX_ROW_BYTES));
        block.push('\n');
    }
    if rows.len() > MAX_ROWS {
        block.push_str(&format!("- … {} more\n", rows.len() - MAX_ROWS));
    }
    Some(block)
}

/// `text` cut at `max` bytes, on a character boundary.
fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOB: &str = "019c0000-0000-7000-8000-0000000000a1";
    const OTHER: &str = "019c0000-0000-7000-8000-0000000000a2";

    fn started(job: &str, host: u32, command: &str) -> (String, serde_json::Value) {
        (
            "job.started".to_owned(),
            serde_json::json!({"job_id": job, "state": "started", "host_pid": host,
                "handle": "job-1", "command": command}),
        )
    }

    #[test]
    fn a_job_counts_while_its_host_lives_and_until_it_ends() {
        let alive = |pid: u32| pid == 10;
        let events = vec![started(JOB, 10, "cargo watch"), started(OTHER, 11, "make")];
        let rows = running_jobs(&events, &alive);
        assert_eq!(rows.len(), 1, "a dead host's job is not running: {rows:?}");
        assert_eq!(rows[0].line, "job job-1 running: cargo watch");
        let mut ended = events.clone();
        ended.push((
            "job.completed".to_owned(),
            serde_json::json!({"job_id": JOB, "state": "completed"}),
        ));
        assert!(running_jobs(&ended, &alive).is_empty());
    }

    #[test]
    fn a_subagent_counts_only_with_a_live_recorded_host() {
        let alive = |pid: u32| pid == 10;
        let spawned = |id: &str, host: Option<u32>| {
            let mut payload = serde_json::json!({"agent_id": id, "role": "explore",
                "state": "running", "current_operation": "map the parser"});
            if let Some(host) = host {
                payload["host_pid"] = host.into();
            }
            ("agent.spawned".to_owned(), payload)
        };
        let events = vec![
            spawned("a1", Some(10)),
            spawned("a2", None),
            spawned("a3", Some(11)),
        ];
        let rows = running_agents(&events, &alive);
        assert_eq!(
            rows,
            vec![RunningRow {
                line: "subagent a1 (explore) running: map the parser".to_owned()
            }]
        );
        let mut ended = events;
        ended.push((
            "agent.state_changed".to_owned(),
            serde_json::json!({"agent_id": "a1", "state": "failed"}),
        ));
        assert!(running_agents(&ended, &alive).is_empty());
    }

    #[test]
    fn nothing_running_is_no_block_and_a_long_list_is_bounded() {
        assert_eq!(render(&[]), None);
        let rows: Vec<RunningRow> = (0..30)
            .map(|n| RunningRow {
                line: format!("job job-{n} running: {}\nsecond line", "é".repeat(200)),
            })
            .collect();
        let block = render(&rows).expect("a block");
        assert!(
            block.len() <= crate::host::MAX_RUNNING_BLOCK_BYTES,
            "{}",
            block.len()
        );
        assert!(block.ends_with("- … 10 more\n"), "{block}");
        assert!(!block.contains("second line\n"), "one line a row");
    }
}
