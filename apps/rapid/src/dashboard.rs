//! The agent dashboard (SEAM-15-1): a cross-session overview of the
//! project, reduced from its ledger each time it is asked for.
//!
//! Each row is a session with what the ledger says of it: where it came from
//! (its origin, the session it was forked from), its title, how many turns it
//! has had and how the last one ended, the subagents it recorded and their
//! recorded states, the background jobs it has open, the loops it owns, and
//! the sessions forked from it. Nothing is stored for the dashboard, and
//! nothing in it depends on the clock or on what is running now — a process's
//! liveness is not a record — so the same ledger always gives the same bytes:
//! a daemon that is killed and restarted reproduces its dashboard exactly.
//!
//! The daemon serves it as `dashboard.get`; the terminal UI renders the same
//! projection for `/dashboard`.

use std::collections::BTreeMap;
use std::path::Path;

use event_ledger::ledger::clean_inline_text;
use kernel::InProcessKernelClient;

/// Wire identity of the projection.
pub(crate) const SCHEMA: &str = "rapidlm.dashboard.v1";
/// Sessions shown, at most: the most recently active.
pub(crate) const MAX_SESSIONS: usize = 200;
/// Agents and loops shown per session, at most.
const MAX_PER_SESSION: usize = 20;
/// Agent and job records read per session, at most (the newest).
const MAX_RECORDS_READ: u32 = 400;
/// Characters of a prompt or a task as shown.
const MAX_TEXT_CHARS: usize = 100;
/// Sessions the text form lists.
const MAX_RENDERED_SESSIONS: usize = 30;
/// Sessions whose first record is read to find who forked whom, at most:
/// the most recently active.
const MAX_FORK_SCAN: usize = 1000;
/// What stands in for text the secret scanner reports.
const WITHHELD: &str = "(withheld: looks like a credential)";

/// `text` made inert and bounded — and withheld if it looks like a
/// credential: this projection lists every session's prompts without the
/// reader knowing a session id, which no other call does.
fn shown(text: &str, max: usize) -> Option<String> {
    let text = clean_inline_text(text, max)?;
    Some(if crate::memory_flush::holds_secret(&text) {
        WITHHELD.to_owned()
    } else {
        text
    })
}

/// The dashboard of the project whose ledger is at `ledger_path`.
pub(crate) fn build(
    client: &InProcessKernelClient,
    ledger_path: &Path,
) -> Result<serde_json::Value, String> {
    build_with(client, ledger_path, MAX_SESSIONS)
}

/// [`build`] showing at most `max_sessions` sessions.
pub(crate) fn build_with(
    client: &InProcessKernelClient,
    ledger_path: &Path,
    max_sessions: usize,
) -> Result<serde_json::Value, String> {
    let mut summaries = client
        .list_sessions(&kernel::CancellationToken::new())
        .map_err(|err| format!("the sessions could not be listed: {err}"))?;
    let total = summaries.len();
    // The most recently active, when there are more than fit; then in the
    // order they began. Ties break on the id, so the order is total.
    summaries.sort_by(|a, b| {
        b.last_activity
            .cmp(&a.last_activity)
            .then_with(|| b.session_id.cmp(&a.session_id))
    });
    let all = summaries.clone();
    summaries.truncate(max_sessions);
    summaries.sort_by(|a, b| {
        a.first_seen
            .cmp(&b.first_seen)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });

    // Loops, by owning session.
    // A failure to read them is an error, not an empty list: two asks of
    // one ledger must not differ without saying why.
    let mut loops: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    let cron = scheduler::PromptCron::open(ledger_path)
        .map_err(|err| format!("the loops could not be read: {err}"))?;
    for job in cron
        .list()
        .map_err(|err| format!("the loops could not be read: {err}"))?
    {
        if job.kind != event_ledger::cron::CronJobKind::Loop {
            continue;
        }
        let Some(owner) = job.session_id.clone() else {
            continue;
        };
        loops.entry(owner).or_default().push(serde_json::json!({
            "id": clean_inline_text(&job.id, 64).unwrap_or_default(),
            "schedule": clean_inline_text(&job.schedule, 64).unwrap_or_default(),
            "prompt": shown(&job.prompt, MAX_TEXT_CHARS).unwrap_or_default(),
            "status": format!("{:?}", job.status).to_ascii_lowercase(),
            "expires_at_ms": job.expires_at_ms,
        }));
    }
    for rows in loops.values_mut() {
        rows.sort_by_key(|row| row["id"].as_str().unwrap_or_default().to_owned());
        rows.truncate(MAX_PER_SESSION);
    }

    // Who forked whom: read from the most recently active sessions, however
    // many of them the rows show — so a shown parent lists a child the cap
    // left out of the rows.
    let mut scanned = all.clone();
    scanned.truncate(MAX_FORK_SCAN);
    let mut forked_from: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for summary in &scanned {
        let Ok(id) = summary.session_id.parse::<protocol::SessionId>() else {
            continue;
        };
        let Ok(Some(first)) = client.edge_event_of_kind(id, "session.", false) else {
            continue;
        };
        if first.kind() == event_ledger::event::EventKind::SessionForked
            && let (Some(parent), Some(at)) = (
                first.payload()["parent_session_id"].as_str(),
                first.payload()["source_seq"].as_u64(),
            )
        {
            forked_from.insert(summary.session_id.clone(), (parent.to_owned(), at));
        }
    }
    let mut forks: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (child, (parent, _)) in &forked_from {
        forks.entry(parent.clone()).or_default().push(child.clone());
    }

    let sessions: Vec<serde_json::Value> = summaries
        .iter()
        .map(|summary| {
            let id = &summary.session_id;
            let parsed = id.parse::<protocol::SessionId>().ok();
            let last = parsed.and_then(|session| crate::resume_recap::last_turn(client, session));
            serde_json::json!({
                "id": id,
                "origin": summary.origin,
                "title": summary.title.as_deref().and_then(|t| shown(t, MAX_TEXT_CHARS)),
                "first_seen": summary.first_seen,
                "last_activity": summary.last_activity,
                "last_seq": summary.last_seq,
                "background": summary.background,
                "forked_from": forked_from.get(id).map(|(parent, at)| {
                    serde_json::json!({"session": parent, "at_seq": at})
                }),
                "forks": forks.get(id).cloned().unwrap_or_default(),
                "turns": last.as_ref().map_or(0, |last| last.turns),
                "last_turn": last.map(|last| serde_json::json!({
                    "asked": shown(&last.asked, MAX_TEXT_CHARS),
                    "end": last.end.as_str(),
                    "took_ms": last.took_ms,
                    "detail": last.detail.as_deref().and_then(|d| shown(d, MAX_TEXT_CHARS)),
                })),
                "agents": parsed.map(|session| agents_of(client, session)).unwrap_or_default(),
                "open_jobs": parsed.map_or(0, |session| open_jobs_of(client, session)),
                "loops": loops.get(id).cloned().unwrap_or_default(),
            })
        })
        .collect();
    Ok(serde_json::json!({
        "schema": SCHEMA,
        "total_sessions": total,
        "truncated": total > summaries.len(),
        "sessions": sessions,
        "actions": [{
            "id": "new_agent",
            "label": "New agent",
            "method": "sessions.create",
            "params": {},
        }],
    }))
}

/// The subagents `session` recorded, in the order they were spawned, each
/// with the state its records last gave it — recorded state, not whether any
/// process is still alive.
fn agents_of(
    client: &InProcessKernelClient,
    session: protocol::SessionId,
) -> Vec<serde_json::Value> {
    const KINDS: [&str; 4] = [
        "agent.spawned",
        "agent.state_changed",
        "agent.result",
        "agent.cancelled",
    ];
    let Ok(events) = client.recent_events_of_kinds(session, &KINDS, MAX_RECORDS_READ) else {
        return Vec::new();
    };
    // (id, role, task, state) in spawn order.
    let mut agents: Vec<(String, String, String, String)> = Vec::new();
    for event in &events {
        let payload = event.payload();
        let Some(id) = payload["agent_id"].as_str() else {
            continue;
        };
        let text = |key: &str| {
            payload[key]
                .as_str()
                .and_then(|text| shown(text, MAX_TEXT_CHARS))
                .unwrap_or_default()
        };
        match event.kind().as_str() {
            "agent.spawned" => {
                agents.retain(|(known, ..)| known != id);
                agents.push((
                    id.to_owned(),
                    text("role"),
                    text("current_operation"),
                    "running".to_owned(),
                ));
            }
            kind => {
                let state = match (kind, payload["state"].as_str()) {
                    ("agent.cancelled", _) => "cancelled".to_owned(),
                    (_, Some(state)) => clean_inline_text(state, 24).unwrap_or_default(),
                    ("agent.result", None) => "finished".to_owned(),
                    _ => continue,
                };
                if let Some(agent) = agents.iter_mut().find(|(known, ..)| known == id) {
                    agent.3 = state;
                }
            }
        }
    }
    let skip = agents.len().saturating_sub(MAX_PER_SESSION);
    agents
        .into_iter()
        .skip(skip)
        .map(|(id, role, task, state)| {
            serde_json::json!({"id": id, "role": role, "task": task, "state": state})
        })
        .collect()
}

/// How many background jobs of `session` have a start and no end.
fn open_jobs_of(client: &InProcessKernelClient, session: protocol::SessionId) -> usize {
    const KINDS: [&str; 3] = ["job.started", "job.completed", "job.orphan_reconciled"];
    let Ok(events) = client.recent_events_of_kinds(session, &KINDS, MAX_RECORDS_READ) else {
        return 0;
    };
    let pairs: Vec<(String, serde_json::Value)> = events
        .iter()
        .map(|event| (event.kind().as_str().to_owned(), event.payload().clone()))
        .collect();
    crate::job_recovery::open_jobs(&pairs).len()
}

/// The dashboard as lines for a terminal: the most recently active session
/// first, one line each, every field made inert and bounded.
pub(crate) fn render_lines(dashboard: &serde_json::Value) -> Vec<String> {
    let Some(sessions) = dashboard["sessions"].as_array() else {
        return vec!["dashboard: nothing to show".to_owned()];
    };
    let mut rows: Vec<&serde_json::Value> = sessions.iter().collect();
    rows.sort_by(|a, b| {
        b["last_activity"]
            .as_str()
            .cmp(&a["last_activity"].as_str())
            .then_with(|| b["id"].as_str().cmp(&a["id"].as_str()))
    });
    let shown = |value: &serde_json::Value, max: usize| {
        value
            .as_str()
            .and_then(|text| clean_inline_text(text, max))
            .unwrap_or_default()
    };
    let total = dashboard["total_sessions"].as_u64().unwrap_or_default();
    let more = rows.len().saturating_sub(MAX_RENDERED_SESSIONS);
    rows.truncate(MAX_RENDERED_SESSIONS);
    let mut lines = vec![format!(
        "dashboard — {} of {total} session(s), newest first{}",
        rows.len(),
        if dashboard["truncated"].as_bool() == Some(true) {
            " (older ones left out)"
        } else {
            ""
        }
    )];
    for row in rows {
        let id: String = shown(&row["id"], 64).chars().take(8).collect();
        let mut line = id;
        let origin = shown(&row["origin"], 16);
        if !origin.is_empty() {
            line.push(' ');
            line.push_str(&origin);
        }
        let title = shown(&row["title"], 40);
        if !title.is_empty() {
            line.push_str(&format!(" \"{title}\""));
        }
        if row["background"].as_bool() == Some(true) {
            line.push_str(" [background]");
        }
        if let Some(parent) = row["forked_from"]["session"].as_str() {
            let parent: String = shown(&serde_json::json!(parent), 64)
                .chars()
                .take(8)
                .collect();
            line.push_str(&format!(" (fork of {parent})"));
        }
        match row["last_turn"].as_object() {
            Some(last) => {
                let asked = shown(&last["asked"], 40);
                line.push_str(&format!(
                    " — {} turn(s), last {}: {asked}",
                    row["turns"].as_u64().unwrap_or_default(),
                    shown(&last["end"], 16)
                ));
            }
            None => line.push_str(" — no turns"),
        }
        let count = |key: &str| row[key].as_array().map_or(0, Vec::len);
        let mut extras = Vec::new();
        for (label, n) in [
            ("forks", count("forks")),
            ("agents", count("agents")),
            ("loops", count("loops")),
            (
                "jobs",
                usize::try_from(row["open_jobs"].as_u64().unwrap_or_default()).unwrap_or(0),
            ),
        ] {
            if n > 0 {
                extras.push(format!("{n} {label}"));
            }
        }
        if !extras.is_empty() {
            line.push_str(&format!(" [{}]", extras.join(", ")));
        }
        lines.push(line);
    }
    if more > 0 {
        lines.push(format!("… and {more} more session(s) not listed here"));
    }
    lines.push("new agent: start another session (sessions.create on the daemon)".to_owned());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::KernelClient as _;

    struct Project {
        dir: std::path::PathBuf,
        ledger: std::path::PathBuf,
        client: InProcessKernelClient,
        actor: event_ledger::event::ActorRef,
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn project(tag: &str) -> Project {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-dashboard-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let ledger = dir.join("sessions.sqlite");
        let client = InProcessKernelClient::open(&ledger).expect("ledger");
        Project {
            dir,
            ledger,
            client,
            actor: crate::interactive::human_actor().expect("actor"),
        }
    }

    impl Project {
        fn session(&self, origin: &str) -> protocol::SessionId {
            crate::approvals::client_call(
                self.client.create_session(
                    kernel::CreateSession::new(
                        protocol::ProjectId::new(),
                        self.actor.clone(),
                        protocol::TraceId::new(),
                    )
                    .with_origin(origin),
                ),
            )
            .expect("session")
            .id()
        }

        fn append(
            &self,
            session: protocol::SessionId,
            kind: event_ledger::event::EventKind,
            payload: serde_json::Value,
        ) {
            self.client
                .append_turn_progress(
                    session,
                    &self.actor,
                    protocol::TraceId::new(),
                    kind,
                    payload,
                )
                .expect("append");
            std::thread::sleep(std::time::Duration::from_millis(3));
        }

        /// One finished turn.
        fn turn(&self, session: protocol::SessionId, text: &str, outcome: kernel::TurnOutcome) {
            let tip = self.client.session_tip(session).expect("tip");
            let handle =
                crate::approvals::client_call(self.client.submit_turn(kernel::SubmitTurn::new(
                    session,
                    tip,
                    self.actor.clone(),
                    protocol::TraceId::new(),
                    text,
                )))
                .expect("turn");
            self.client
                .finish_turn(kernel::FinishTurn::new(
                    session,
                    handle.turn_id(),
                    self.actor.clone(),
                    protocol::TraceId::new(),
                    outcome,
                ))
                .expect("finish");
            std::thread::sleep(std::time::Duration::from_millis(3));
        }

        fn dashboard(&self) -> serde_json::Value {
            build(&self.client, &self.ledger).expect("dashboard")
        }
    }

    fn row(dashboard: &serde_json::Value, id: protocol::SessionId) -> &serde_json::Value {
        dashboard["sessions"]
            .as_array()
            .expect("sessions")
            .iter()
            .find(|row| row["id"] == id.to_string())
            .expect("the session's row")
    }

    #[test]
    fn every_row_is_what_the_ledger_says_of_its_session() {
        let project = project("rows");
        let main = project.session("interactive");
        project.turn(
            main,
            "\u{1b}[2Jfix the parser\nsecond line",
            kernel::TurnOutcome::Completed {
                text: Some("Fixed it.\nmore".to_owned()),
            },
        );
        project.turn(
            main,
            "now the lexer",
            kernel::TurnOutcome::Failed {
                reason: "model refused".to_owned(),
            },
        );
        let daemon = project.session("daemon");
        let background = project.session("loop");
        project.append(
            background,
            event_ledger::event::EventKind::AutomationTriggerReceived,
            serde_json::json!({"source": "loop"}),
        );
        // Agents: one running, one that finished, one cancelled.
        let (a1, a2, a3) = (
            protocol::AgentId::new().to_string(),
            protocol::AgentId::new().to_string(),
            protocol::AgentId::new().to_string(),
        );
        let agent = |id: &str, role: &str, task: &str| serde_json::json!({"agent_id": id, "role": role, "current_operation": task});
        project.append(
            main,
            event_ledger::event::EventKind::AgentSpawned,
            agent(&a1, "explore", "read the \u{1b}[2J parser"),
        );
        project.append(
            main,
            event_ledger::event::EventKind::AgentSpawned,
            agent(&a2, "write", "patch it"),
        );
        project.append(
            main,
            event_ledger::event::EventKind::AgentSpawned,
            agent(&a3, "explore", "look around"),
        );
        project.append(
            main,
            event_ledger::event::EventKind::AgentStateChanged,
            serde_json::json!({"agent_id": a2, "state": "succeeded"}),
        );
        project.append(
            main,
            event_ledger::event::EventKind::AgentCancelled,
            serde_json::json!({"agent_id": a3}),
        );
        // A job with a start and no end, and one with both.
        let open = protocol::JobId::new().to_string();
        let done = protocol::JobId::new().to_string();
        project.append(
            main,
            event_ledger::event::EventKind::JobStarted,
            serde_json::json!({"job_id": open, "host_pid": 1}),
        );
        project.append(
            main,
            event_ledger::event::EventKind::JobStarted,
            serde_json::json!({"job_id": done, "host_pid": 1}),
        );
        project.append(
            main,
            event_ledger::event::EventKind::JobCompleted,
            serde_json::json!({"job_id": done, "exit_status": 0}),
        );
        // A title, a fork, and two loops (one of another session).
        project.append(
            main,
            event_ledger::event::EventKind::SessionRenamed,
            serde_json::json!({"title": "Parser work", "source": "user"}),
        );
        let fork =
            crate::approvals::client_call(project.client.fork_session(kernel::ForkSession::new(
                main,
                project.client.session_tip(main).expect("tip"),
                project.actor.clone(),
                protocol::TraceId::new(),
            )))
            .expect("fork")
            .id();
        let cron = scheduler::PromptCron::open(&project.ledger).expect("cron");
        let owner = main.to_string();
        let added = cron
            .add_loop(
                "check the \u{1b}[2J build",
                Some(&owner),
                "0 * * * *",
                event_ledger::cron::DEFAULT_LOOP_LIFETIME_MS,
                1_700_000_000_000,
                &capability_broker::CancellationToken::new(),
            )
            .expect("loop");
        // A scheduled prompt that is not a loop is not a loop row.
        cron.add(
            "a plain cron prompt",
            Some(&owner),
            "0 * * * *",
            1_700_000_000_000,
            &capability_broker::CancellationToken::new(),
        )
        .expect("cron job");
        cron.add_loop(
            "another session's loop",
            Some(&daemon.to_string()),
            "0 * * * *",
            event_ledger::cron::DEFAULT_LOOP_LIFETIME_MS,
            1_700_000_000_000,
            &capability_broker::CancellationToken::new(),
        )
        .expect("loop");
        drop(cron);

        let dashboard = project.dashboard();
        assert_eq!(dashboard["schema"], SCHEMA);
        assert_eq!(dashboard["total_sessions"], 4);
        assert_eq!(dashboard["truncated"], false);
        assert_eq!(dashboard["actions"][0]["id"], "new_agent");
        assert_eq!(dashboard["actions"][0]["method"], "sessions.create");
        // In the order the sessions began.
        let order: Vec<&str> = dashboard["sessions"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|r| r["id"].as_str().expect("id"))
            .collect();
        let mut sorted_by_time: Vec<&str> = order.clone();
        sorted_by_time.sort_by_key(|id| {
            dashboard["sessions"]
                .as_array()
                .expect("rows")
                .iter()
                .find(|r| r["id"] == *id)
                .map(|r| r["first_seen"].as_str().unwrap_or_default().to_owned())
        });
        assert_eq!(order, sorted_by_time);
        // The main session's row.
        let main_row = row(&dashboard, main);
        assert_eq!(main_row["origin"], "interactive");
        assert_eq!(main_row["title"], "Parser work");
        assert_eq!(main_row["background"], false);
        assert_eq!(main_row["turns"], 2);
        assert_eq!(main_row["last_turn"]["asked"], "now the lexer");
        assert_eq!(main_row["last_turn"]["end"], "failed");
        assert_eq!(main_row["last_turn"]["detail"], "model refused");
        assert!(main_row["last_turn"]["took_ms"].is_i64());
        assert_eq!(main_row["forks"], serde_json::json!([fork.to_string()]));
        assert_eq!(main_row["forked_from"], serde_json::Value::Null);
        assert_eq!(main_row["open_jobs"], 1);
        let agents = main_row["agents"].as_array().expect("agents");
        let states: Vec<(&str, &str)> = agents
            .iter()
            .map(|a| {
                (
                    a["id"].as_str().unwrap_or(""),
                    a["state"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            states,
            [
                (a1.as_str(), "running"),
                (a2.as_str(), "succeeded"),
                (a3.as_str(), "cancelled")
            ]
        );
        assert_eq!(agents[0]["role"], "explore");
        assert_eq!(main_row["loops"].as_array().expect("loops").len(), 1);
        assert_eq!(main_row["loops"][0]["id"], added.id);
        assert_eq!(main_row["loops"][0]["prompt"], "check the [2J build");
        assert_eq!(main_row["loops"][0]["status"], "active");
        // The fork knows its parent; the loop's session is marked background.
        let fork_row = row(&dashboard, fork);
        assert_eq!(fork_row["forked_from"]["session"], main.to_string());
        assert!(fork_row["forked_from"]["at_seq"].is_u64());
        assert_eq!(row(&dashboard, background)["background"], true);
        // Another session's loop is its own, and a session without turns says so.
        assert_eq!(
            row(&dashboard, daemon)["loops"]
                .as_array()
                .expect("loops")
                .len(),
            1
        );
        assert_eq!(row(&dashboard, daemon)["turns"], 0);
        assert_eq!(
            row(&dashboard, daemon)["last_turn"],
            serde_json::Value::Null
        );
        // Nothing in it can carry an escape.
        let text = dashboard.to_string();
        assert!(
            !text.contains("\\u001b") && !text.contains('\u{1b}'),
            "{text}"
        );
        assert!(
            main_row["agents"][0]["task"]
                .as_str()
                .expect("task")
                .contains("[2J")
        );
    }

    #[test]
    fn the_same_ledger_gives_the_same_bytes_before_and_after_a_restart() {
        let project = project("bytes");
        let a = project.session("interactive");
        project.turn(
            a,
            "first ask",
            kernel::TurnOutcome::Completed {
                text: Some("done".to_owned()),
            },
        );
        let b = project.session("daemon");
        project.append(
            b,
            event_ledger::event::EventKind::AgentSpawned,
            serde_json::json!({"agent_id": protocol::AgentId::new().to_string(), "role": "explore", "current_operation": "look"}),
        );
        let cron = scheduler::PromptCron::open(&project.ledger).expect("cron");
        cron.add_loop(
            "a prompt",
            Some(&a.to_string()),
            "0 * * * *",
            event_ledger::cron::DEFAULT_LOOP_LIFETIME_MS,
            1_700_000_000_000,
            &capability_broker::CancellationToken::new(),
        )
        .expect("loop");
        drop(cron);
        let before = project.dashboard().to_string();
        assert_eq!(
            project.dashboard().to_string(),
            before,
            "asking twice changes nothing"
        );
        // Everything in the process is dropped and the ledger is opened anew.
        let ledger = project.ledger.clone();
        let reopened = InProcessKernelClient::open(&ledger).expect("reopen");
        let after = build(&reopened, &ledger).expect("dashboard").to_string();
        assert_eq!(after, before, "a restart reproduces it byte for byte");
        assert!(before.contains("first ask") && before.contains("a prompt"));
    }

    #[test]
    fn a_dashboard_shows_the_most_recently_active_sessions_and_says_so() {
        let project = project("cap");
        let ids: Vec<protocol::SessionId> = (0..5)
            .map(|n| {
                let id = project.session("interactive");
                project.turn(
                    id,
                    &format!("ask {n}"),
                    kernel::TurnOutcome::Completed { text: None },
                );
                id
            })
            .collect();
        // The oldest is active again, last.
        project.append(
            ids[0],
            event_ledger::event::EventKind::SessionRenamed,
            serde_json::json!({"title": "revived", "source": "user"}),
        );
        let dashboard = build_with(&project.client, &project.ledger, 3).expect("dashboard");
        assert_eq!(dashboard["total_sessions"], 5);
        assert_eq!(dashboard["truncated"], true);
        let shown: Vec<&str> = dashboard["sessions"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|r| r["id"].as_str().expect("id"))
            .collect();
        assert_eq!(shown.len(), 3);
        // The three most recently active: the revived one, and the last two.
        for id in [ids[0], ids[3], ids[4]] {
            assert!(shown.contains(&id.to_string().as_str()), "{shown:?}");
        }
        // Shown in the order they began.
        assert_eq!(shown[0], ids[0].to_string());
        let lines = render_lines(&dashboard);
        assert!(
            lines[0].contains("3 of 5") && lines[0].contains("older ones left out"),
            "{lines:?}"
        );
    }

    #[test]
    fn the_text_form_is_one_inert_line_a_session_newest_first() {
        let project = project("text");
        let old = project.session("interactive");
        project.turn(
            old,
            "\u{1b}[2Jan old ask",
            kernel::TurnOutcome::Completed { text: None },
        );
        project.append(
            old,
            event_ledger::event::EventKind::SessionRenamed,
            serde_json::json!({"title": "Old\u{1b}[31m work", "source": "user"}),
        );
        let new = project.session("daemon");
        project.append(
            new,
            event_ledger::event::EventKind::AgentSpawned,
            serde_json::json!({"agent_id": protocol::AgentId::new().to_string(), "role": "explore", "current_operation": "look"}),
        );
        let lines = render_lines(&project.dashboard());
        assert!(lines[0].contains("2 of 2"), "{lines:?}");
        assert!(
            lines[1].starts_with(&new.to_string()[..8]),
            "newest first: {lines:?}"
        );
        assert!(
            lines[1].contains("no turns") && lines[1].contains("1 agents"),
            "{lines:?}"
        );
        assert!(
            lines[2].contains("\"Old [31m work\"") && lines[2].contains("last completed"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|line| !line.contains('\u{1b}') && !line.contains('\n')),
            "{lines:?}"
        );
        assert!(lines.last().expect("a line").contains("new agent"));
        // A malformed dashboard is said, not a panic.
        assert_eq!(
            render_lines(&serde_json::json!({})),
            ["dashboard: nothing to show"]
        );
    }

    #[test]
    fn the_text_form_lists_a_bounded_number_of_sessions_and_says_how_many_it_left_out() {
        let sessions: Vec<serde_json::Value> = (0..45)
            .map(|n| {
                serde_json::json!({
                    "id": format!("01a08600-0000-7000-8000-{n:012}"),
                    "origin": "interactive",
                    "title": null,
                    "last_activity": format!("2026-09-30T10:{:02}:00.000Z", n % 60),
                    "last_turn": null, "turns": 0, "forks": [], "agents": [], "loops": [],
                    "open_jobs": 0, "background": false, "forked_from": null,
                })
            })
            .collect();
        let dashboard = serde_json::json!({
            "sessions": sessions, "total_sessions": 500, "truncated": true, "actions": [],
        });
        let lines = render_lines(&dashboard);
        // Header, the most recent thirty, the remainder, the action.
        assert_eq!(lines.len(), 1 + MAX_RENDERED_SESSIONS + 1 + 1, "{lines:?}");
        assert!(lines[0].contains("30 of 500"), "{lines:?}");
        assert!(lines[lines.len() - 2].contains("and 15 more"), "{lines:?}");
        // The remainder is not promised to be anywhere: the dashboard itself
        // holds only the most recent, and the line must not say otherwise.
        assert_eq!(
            lines[lines.len() - 2],
            "… and 15 more session(s) not listed here"
        );
        assert!(lines[1].contains(":00.000Z") || lines[1].starts_with("01a08600"));
    }

    #[test]
    fn a_prompt_that_looks_like_a_credential_is_withheld_from_every_field() {
        let project = project("secret");
        let id = project.session("interactive");
        let key = "AKIAJSIE6T5YJX3ZZZZZ";
        project.turn(
            id,
            &format!("deploy with {key}"),
            kernel::TurnOutcome::Failed {
                reason: format!("rejected {key}"),
            },
        );
        project.append(
            id,
            event_ledger::event::EventKind::SessionRenamed,
            serde_json::json!({"title": format!("use {key}"), "source": "user"}),
        );
        project.append(
            id,
            event_ledger::event::EventKind::AgentSpawned,
            serde_json::json!({
                "agent_id": protocol::AgentId::new().to_string(),
                "role": "explore",
                "current_operation": format!("try {key}"),
            }),
        );
        let owner = id.to_string();
        scheduler::PromptCron::open(&project.ledger)
            .expect("cron")
            .add_loop(
                &format!("poll with {key}"),
                Some(&owner),
                "0 * * * *",
                event_ledger::cron::DEFAULT_LOOP_LIFETIME_MS,
                1_700_000_000_000,
                &capability_broker::CancellationToken::new(),
            )
            .expect("loop");
        let dashboard = project.dashboard();
        let wire = dashboard.to_string();
        assert!(!wire.contains(key), "{wire}");
        assert!(!render_lines(&dashboard).join("\n").contains(key));
        let row = row(&dashboard, id);
        assert_eq!(row["last_turn"]["asked"], WITHHELD);
        assert_eq!(row["title"], WITHHELD);
        assert_eq!(row["agents"][0]["task"], WITHHELD);
        assert_eq!(row["loops"][0]["prompt"], WITHHELD);
        // The state is still said; only the text is held back.
        assert_eq!(row["last_turn"]["end"], "failed");
        assert_eq!(row["turns"], 1);
    }

    #[test]
    fn a_forked_child_outside_the_rows_is_still_listed_under_its_shown_parent() {
        let project = project("forks-cap");
        let parent = project.session("interactive");
        project.turn(
            parent,
            "the parent",
            kernel::TurnOutcome::Completed { text: None },
        );
        let child =
            crate::approvals::client_call(project.client.fork_session(kernel::ForkSession::new(
                parent,
                project.client.session_tip(parent).expect("tip"),
                project.actor.clone(),
                protocol::TraceId::new(),
            )))
            .expect("fork")
            .id();
        // Two newer sessions push the child out of a two-row window, and the
        // parent is made active again so it stays in.
        for n in 0..2 {
            let other = project.session("interactive");
            project.turn(
                other,
                &format!("other {n}"),
                kernel::TurnOutcome::Completed { text: None },
            );
        }
        project.append(
            parent,
            event_ledger::event::EventKind::SessionRenamed,
            serde_json::json!({"title": "parent", "source": "user"}),
        );
        let dashboard = build_with(&project.client, &project.ledger, 2).expect("dashboard");
        let ids: Vec<&str> = dashboard["sessions"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|r| r["id"].as_str().expect("id"))
            .collect();
        assert!(ids.contains(&parent.to_string().as_str()), "{ids:?}");
        assert!(!ids.contains(&child.to_string().as_str()), "{ids:?}");
        assert_eq!(
            row(&dashboard, parent)["forks"],
            serde_json::json!([child.to_string()])
        );
    }

    #[test]
    fn a_session_without_an_origin_has_no_trailing_space_and_the_remainder_is_not_promised() {
        let dashboard = serde_json::json!({
            "sessions": [{
                "id": "01a08600-0000-7000-8000-000000000001", "origin": null,
                "title": null, "last_activity": "2026-09-30T10:00:00.000Z",
                "last_turn": null, "turns": 0, "forks": [], "agents": [], "loops": [],
                "open_jobs": 0, "background": false, "forked_from": null,
            }],
            "total_sessions": 500, "truncated": true, "actions": [],
        });
        let lines = render_lines(&dashboard);
        assert_eq!(lines[1], "01a08600 — no turns", "{lines:?}");
    }

    #[test]
    fn loops_that_cannot_be_read_fail_the_dashboard_rather_than_showing_none() {
        let project = project("loops-fail");
        project.session("interactive");
        // A ledger path the scheduler cannot open: a directory.
        let err = build_with(&project.client, &project.dir, 10).expect_err("an error");
        assert!(err.contains("the loops could not be read"), "{err}");
    }

    #[test]
    fn an_empty_project_has_an_empty_dashboard() {
        let project = project("empty");
        let dashboard = project.dashboard();
        assert_eq!(dashboard["total_sessions"], 0);
        assert_eq!(dashboard["sessions"], serde_json::json!([]));
        assert_eq!(dashboard["actions"][0]["id"], "new_agent");
        assert_eq!(render_lines(&dashboard).len(), 2);
    }
}
