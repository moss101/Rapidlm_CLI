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
    let count: u32 = digits
        .parse()
        .ok()
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
            let is_loop = cron
                .list()
                .map_err(|err| P9CommandError::Agent(format!("{err}")))?
                .iter()
                .any(|job| job.id == *id && job.kind == event_ledger::cron::CronJobKind::Loop);
            if is_loop
                && cron
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

fn print_loops(loops: &[event_ledger::cron::CronJob]) {
    println!("schema={LOOP_SCHEMA} count={}", loops.len());
    for job in loops {
        println!(
            "id={} status={} schedule={} next_fire_at_ms={} expires_at_ms={} prompt={}",
            job.id,
            job.status.as_str(),
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
        for refused in ["7m", "45m", "5h", "2d", "0m", "m", "", "5s", "5 m", "-5m"] {
            assert!(interval_schedule(refused).is_err(), "{refused}");
        }
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
