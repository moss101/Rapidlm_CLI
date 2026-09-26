//! Loops (SEAM-03, `/loop` and `rapid loop add|list|rm`): a prompt run every
//! interval as a background turn, for a limited lifetime.
//!
//! A loop is a cron row of kind `loop` in the project's ledger
//! (`event_ledger::cron`): it expires — seven days unless its creator says
//! otherwise — and at most `MAX_ACTIVE_LOOPS` are active at once. What fires
//! it is the scheduler's claim-lease poll, as for any cron row.

use std::path::PathBuf;

use crate::p9_commands::P9CommandError;

/// Wire identity of `rapid loop` output lines.
pub const LOOP_SCHEMA: &str = "rapidlm.loop.v1";

/// The longest a loop may be given (`--for`): thirty days.
pub const MAX_LOOP_LIFETIME_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Minute counts that divide an hour evenly, and hour counts that divide a
/// day: an interval a five-field schedule can express exactly.
const MINUTE_STEPS: &[u32] = &[1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30];
const HOUR_STEPS: &[u32] = &[1, 2, 3, 4, 6, 8, 12];

/// A loop interval (`5m`, `2h`, `1d`) as the five-field schedule that fires
/// on it exactly. An interval that does not divide an hour or a day evenly
/// is refused rather than approximated — `*/7` minutes fires at :56 and
/// again at :00.
pub fn interval_schedule(interval: &str) -> Result<String, String> {
    let (count, unit) = split_amount(interval)?;
    match unit {
        'm' if MINUTE_STEPS.contains(&count) => Ok(if count == 1 {
            "* * * * *".to_owned()
        } else {
            format!("*/{count} * * * *")
        }),
        'h' if HOUR_STEPS.contains(&count) => Ok(if count == 1 {
            "0 * * * *".to_owned()
        } else {
            format!("0 */{count} * * *")
        }),
        'd' if count == 1 => Ok("0 0 * * *".to_owned()),
        _ => Err(format!(
            "an interval of {interval} does not divide an hour or a day evenly; use minutes \
from {MINUTE_STEPS:?}, hours from {HOUR_STEPS:?}, or 1d"
        )),
    }
}

/// A lifetime (`--for 12h`, `3d`) in milliseconds, at most
/// [`MAX_LOOP_LIFETIME_MS`].
pub fn lifetime_ms(text: &str) -> Result<i64, String> {
    let (count, unit) = split_amount(text)?;
    let unit_ms: i64 = match unit {
        'm' => 60 * 1000,
        'h' => 60 * 60 * 1000,
        'd' => 24 * 60 * 60 * 1000,
        _ => {
            return Err(format!(
                "{text}: a lifetime is minutes, hours or days (30m, 12h, 3d)"
            ));
        }
    };
    let ms = i64::from(count).saturating_mul(unit_ms);
    if ms > MAX_LOOP_LIFETIME_MS {
        return Err(format!("{text}: a loop lives at most 30 days"));
    }
    Ok(ms)
}

/// `5m` → `(5, 'm')`: a positive whole count and a one-letter unit.
fn split_amount(text: &str) -> Result<(u32, char), String> {
    let unit = text
        .chars()
        .last()
        .ok_or_else(|| "an empty interval".to_owned())?;
    let digits = &text[..text.len() - unit.len_utf8()];
    let count: u32 = Some(digits)
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|digits| digits.parse().ok())
        .filter(|count| *count > 0)
        .ok_or_else(|| format!("{text}: expected a count and a unit, like 5m, 2h or 1d"))?;
    Ok((count, unit))
}

/// `rapid loop add <interval> <prompt…> [--for <lifetime>] [--session <id>]`,
/// `rapid loop list`, `rapid loop rm <id>` — each with `--db <path>` for a
/// ledger other than the current project's.
pub fn run_loop(args: &[String]) -> Result<i32, P9CommandError> {
    let mut db: Option<PathBuf> = None;
    let mut lifetime: Option<&str> = None;
    let mut session: Option<&str> = None;
    let mut rest: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                db = Some(PathBuf::from(args.get(i).ok_or(P9CommandError::Usage)?));
            }
            "--for" => {
                i += 1;
                lifetime = Some(args.get(i).ok_or(P9CommandError::Usage)?);
            }
            "--session" => {
                i += 1;
                session = Some(args.get(i).ok_or(P9CommandError::Usage)?);
            }
            other => rest.push(other),
        }
        i += 1;
    }
    let (mode, operands) = rest.split_first().ok_or(P9CommandError::Usage)?;
    let db_path = db.unwrap_or_else(crate::interactive::current_project_ledger_path);
    // Listing must not create the store it lists (as `rapid cron list`).
    if *mode == "list" && !db_path.exists() {
        print_loops(&[]);
        return Ok(0);
    }
    let cron = scheduler::PromptCron::open(&db_path)
        .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
    let refused = |err: String| P9CommandError::Agent(err);
    match *mode {
        "add" => {
            let (interval, prompt) = operands.split_first().ok_or(P9CommandError::Usage)?;
            let prompt = prompt.join(" ");
            if prompt.trim().is_empty() {
                return Err(P9CommandError::Usage);
            }
            // A loop a session owns is fired only by that session's host
            // (`claim_due_for_session`); until the session poller exists,
            // nothing would ever fire it — refused rather than stored dead.
            if session.is_some() {
                return Err(refused(
                    "--session is not available yet: a session's loops are fired by its own \
host, which cannot fire them yet; omit it and `rapid cron poll` fires the loop"
                        .to_owned(),
                ));
            }
            let schedule = interval_schedule(interval).map_err(refused)?;
            let lifetime = match lifetime {
                Some(text) => lifetime_ms(text).map_err(refused)?,
                None => event_ledger::cron::DEFAULT_LOOP_LIFETIME_MS,
            };
            let job = cron
                .add_loop(
                    &prompt,
                    session,
                    &schedule,
                    lifetime,
                    now_ms(),
                    &capability_broker::CancellationToken::new(),
                )
                .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
            println!(
                "schema={LOOP_SCHEMA} id={} every={interval} schedule={} next_fire_at_ms={} \
expires_at_ms={}",
                job.id,
                job.schedule,
                job.next_fire_at_ms,
                job.expires_at_ms.unwrap_or_default()
            );
            Ok(0)
        }
        "list" => {
            let loops: Vec<_> = cron
                .list()
                .map_err(|err| P9CommandError::Agent(format!("{err}")))?
                .into_iter()
                .filter(|job| job.kind == event_ledger::cron::CronJobKind::Loop)
                .collect();
            print_loops(&loops);
            Ok(0)
        }
        "rm" | "remove" => {
            let id = operands.first().ok_or(P9CommandError::Usage)?;
            // A loop's id only: `rapid cron remove` is for cron jobs.
            let kind = cron
                .list()
                .map_err(|err| P9CommandError::Agent(format!("{err}")))?
                .iter()
                .find(|job| job.id == *id)
                .map(|job| job.kind);
            if let Some(refusal) = rm_refusal(id, kind) {
                println!("{refusal}");
                return Ok(crate::headless::jsonl::JsonlExitCode::Usage.as_i32());
            }
            if cron
                .remove(id)
                .map_err(|err| P9CommandError::Agent(format!("{err}")))?
            {
                println!("removed id={id}");
                Ok(0)
            } else {
                println!("not found id={id}");
                Ok(crate::headless::jsonl::JsonlExitCode::Usage.as_i32())
            }
        }
        _ => Err(P9CommandError::Usage),
    }
}

/// Most bytes of a loop's answer a notification carries (the TUI refuses a
/// displayed field past `MAX_DISPLAY_TEXT_BYTES`, and a notice is a line,
/// not a report).
pub const MAX_NOTIFICATION_TEXT_BYTES: usize = 1024;

/// Run one fired loop: its prompt as the only turn of a fresh session of
/// its own — no history, so its context is bounded by the prompt alone, and
/// nothing of it reaches the session that owns the loop — through `run`
/// (given the new session's id), finished as the kernel records every turn.
pub(crate) fn fire_loop(
    client: &kernel::InProcessKernelClient,
    actor: &event_ledger::event::ActorRef,
    prompt: &str,
    run: &dyn Fn(protocol::SessionId) -> kernel::TurnOutcome,
) -> kernel::TurnOutcome {
    use kernel::KernelClient as _;
    let created =
        match crate::approvals::client_call(client.create_session(kernel::CreateSession::new(
            protocol::ProjectId::new(),
            actor.clone(),
            protocol::TraceId::new(),
        ))) {
            Ok(created) => created,
            Err(err) => {
                return kernel::TurnOutcome::Failed {
                    reason: format!("the loop's session could not be created: {err}"),
                };
            }
        };
    let handle = match crate::approvals::client_call(client.submit_turn(kernel::SubmitTurn::new(
        created.id(),
        created.seq(),
        actor.clone(),
        protocol::TraceId::new(),
        prompt,
    ))) {
        Ok(handle) => handle,
        Err(err) => {
            return kernel::TurnOutcome::Failed {
                reason: format!("the loop's turn could not be submitted: {err}"),
            };
        }
    };
    let outcome = run(created.id());
    let _ = client.finish_turn(kernel::FinishTurn::new(
        created.id(),
        handle.turn_id(),
        actor.clone(),
        protocol::TraceId::new(),
        outcome.clone(),
    ));
    outcome
}

/// Tell the session that owns loop `loop_id` how a fire went: one
/// `notification.recorded`, its text the answer (or the failure) bounded to
/// [`MAX_NOTIFICATION_TEXT_BYTES`] — a notice, never a turn of the
/// conversation.
pub(crate) fn record_loop_notification(
    client: &kernel::InProcessKernelClient,
    session: protocol::SessionId,
    actor: &event_ledger::event::ActorRef,
    loop_id: &str,
    outcome: &kernel::TurnOutcome,
) {
    let (result, text) = match outcome {
        kernel::TurnOutcome::Completed { text } => (
            "completed",
            text.clone().unwrap_or_else(|| "(no answer)".to_owned()),
        ),
        kernel::TurnOutcome::Failed { reason } => ("failed", reason.clone()),
        kernel::TurnOutcome::Interrupted => ("interrupted", "interrupted".to_owned()),
        kernel::TurnOutcome::Waiting => (
            "waiting",
            "stopped on a question or approval; a loop runs unattended".to_owned(),
        ),
    };
    let mut text = text.trim().to_owned();
    if text.len() > MAX_NOTIFICATION_TEXT_BYTES {
        let mut cut = MAX_NOTIFICATION_TEXT_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('…');
    }
    let _ = client.append_turn_progress(
        session,
        actor,
        protocol::TraceId::new(),
        event_ledger::event::EventKind::NotificationRecorded,
        serde_json::json!({
            "source": format!("loop {loop_id}"),
            "text": text,
            "loop_id": loop_id,
            "outcome": result,
        }),
    );
}

/// Fire the loops `session` owns that are due at `now_ms`: each through
/// [`fire_loop`] with `run`, its outcome recorded as a notification on
/// `session` and reported to the store (which quarantines a loop after
/// repeated failures). Returns how many fired.
pub(crate) fn fire_due_loops(
    cron: &scheduler::PromptCron,
    client: &kernel::InProcessKernelClient,
    session: protocol::SessionId,
    actor: &event_ledger::event::ActorRef,
    now_ms: i64,
    run: &dyn Fn(&str, protocol::SessionId) -> kernel::TurnOutcome,
) -> usize {
    let Ok(report) = cron.poll_session_loops(
        &session.to_string(),
        now_ms,
        &capability_broker::CancellationToken::new(),
        scheduler::MAX_POLL_BATCH,
    ) else {
        return 0;
    };
    for due in &report.fired {
        let outcome = fire_loop(client, actor, &due.prompt, &|id| run(&due.prompt, id));
        record_loop_notification(client, session, actor, &due.id, &outcome);
        let succeeded = matches!(outcome, kernel::TurnOutcome::Completed { .. });
        let _ = cron.report_execution(&due.id, succeeded, now_ms);
    }
    report.fired.len()
}

/// Why `rapid loop rm <id>` removes nothing: the id names no row, or a
/// cron job rather than a loop. `None` for a loop.
fn rm_refusal(id: &str, kind: Option<event_ledger::cron::CronJobKind>) -> Option<String> {
    match kind {
        Some(event_ledger::cron::CronJobKind::Loop) => None,
        Some(event_ledger::cron::CronJobKind::Cron) => Some(format!(
            "not a loop id={id} (a cron job is `rapid cron remove`'s)"
        )),
        None => Some(format!("not found id={id}")),
    }
}

fn print_loops(loops: &[event_ledger::cron::CronJob]) {
    println!("schema={LOOP_SCHEMA} count={}", loops.len());
    let now = now_ms();
    for job in loops {
        // Past its lifetime it will not fire again; the next poll or add
        // removes it.
        let status = if job.expires_at_ms.is_some_and(|expires| expires <= now) {
            "expired"
        } else {
            job.status.as_str()
        };
        println!(
            "id={} status={} schedule={} next_fire_at_ms={} expires_at_ms={} prompt={}",
            job.id,
            status,
            job.schedule,
            job.next_fire_at_ms,
            job.expires_at_ms.unwrap_or_default(),
            elide(&job.prompt),
        );
    }
}

fn elide(prompt: &str) -> String {
    const MAX_ECHO_CHARS: usize = 80;
    if prompt.chars().count() <= MAX_ECHO_CHARS {
        return prompt.to_owned();
    }
    let head: String = prompt.chars().take(MAX_ECHO_CHARS).collect();
    format!("{head}…")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interval_becomes_the_schedule_that_fires_on_it_exactly() {
        assert_eq!(interval_schedule("1m").as_deref(), Ok("* * * * *"));
        assert_eq!(interval_schedule("5m").as_deref(), Ok("*/5 * * * *"));
        assert_eq!(interval_schedule("30m").as_deref(), Ok("*/30 * * * *"));
        assert_eq!(interval_schedule("1h").as_deref(), Ok("0 * * * *"));
        assert_eq!(interval_schedule("6h").as_deref(), Ok("0 */6 * * *"));
        assert_eq!(interval_schedule("1d").as_deref(), Ok("0 0 * * *"));
        for refused in [
            "7m", "45m", "5h", "2d", "0m", "m", "", "5s", "5 m", "-5m", "+5m",
        ] {
            assert!(interval_schedule(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn rm_says_why_it_removed_nothing() {
        use event_ledger::cron::CronJobKind;
        assert_eq!(rm_refusal("x", Some(CronJobKind::Loop)), None);
        assert!(
            rm_refusal("x", Some(CronJobKind::Cron)).is_some_and(|m| m.starts_with("not a loop"))
        );
        assert!(rm_refusal("x", None).is_some_and(|m| m.starts_with("not found")));
    }

    #[test]
    fn a_lifetime_is_bounded() {
        assert_eq!(lifetime_ms("30m"), Ok(30 * 60 * 1000));
        assert_eq!(lifetime_ms("12h"), Ok(12 * 60 * 60 * 1000));
        assert_eq!(lifetime_ms("30d"), Ok(MAX_LOOP_LIFETIME_MS));
        assert!(lifetime_ms("31d").is_err());
        assert!(lifetime_ms("0d").is_err());
        assert!(lifetime_ms("3w").is_err());
    }

    #[test]
    fn rapid_loop_adds_lists_and_removes_only_loops() {
        let db = std::env::temp_dir().join(format!(
            "rapidlm-loops-{}-{}.sqlite",
            std::process::id(),
            now_ms()
        ));
        let db_arg = db.to_string_lossy().into_owned();
        let run = |args: &[&str]| {
            let mut all: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
            all.extend(["--db".to_owned(), db_arg.clone()]);
            run_loop(&all)
        };
        assert_eq!(run(&["list"]).expect("list"), 0);
        assert!(!db.exists(), "listing does not create the store");
        assert_eq!(
            run(&["add", "5m", "check", "the", "build", "--for", "2h"]).expect("add"),
            0
        );
        assert!(matches!(
            run(&["add", "7m", "x"]),
            Err(P9CommandError::Agent(_))
        ));
        // Nothing would fire a session's loop yet: refused, not stored.
        assert!(matches!(
            run(&["add", "5m", "x", "--session", "s-1"]),
            Err(P9CommandError::Agent(_))
        ));
        assert_ne!(run(&["rm", "cron-0000000000000000"]).expect("rm"), 0);
        let cron = scheduler::PromptCron::open(&db).expect("store");
        let loops = cron.list().expect("list");
        assert_eq!(loops.len(), 1);
        let job = &loops[0];
        assert_eq!(job.kind, event_ledger::cron::CronJobKind::Loop);
        assert_eq!(job.prompt, "check the build");
        assert_eq!(job.schedule, "*/5 * * * *");
        assert_eq!(
            job.expires_at_ms.map(|expires| expires - job.created_at_ms),
            Some(2 * 60 * 60 * 1000)
        );
        // A cron job is not a loop's to remove.
        let other = cron
            .add(
                "tidy",
                None,
                "0 9 * * *",
                now_ms(),
                &capability_broker::CancellationToken::new(),
            )
            .expect("cron job");
        assert_ne!(run(&["rm", &other.id]).expect("rm"), 0);
        assert_eq!(run(&["rm", &job.id]).expect("rm"), 0);
        assert_eq!(cron.list().expect("list").len(), 1, "the cron job stays");
        let _ = std::fs::remove_file(&db);
    }
}
