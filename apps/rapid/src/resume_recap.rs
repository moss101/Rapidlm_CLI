//! Where a resumed session stood (SEAM-12-2, the resume recap).
//!
//! When the TUI opens an existing session — `rapid resume`, `/resume`, a
//! fork — the user is told where it left off: the last turn (what was
//! asked, how it ended, how long it took, what it finished with) and the
//! goal's criteria that are still unmet. It is derived from the session's
//! `turn.*` records and the persisted goal each time — nothing is stored,
//! so it cannot go stale — and a session with no turns has none.
//!
//! The ledger is not trusted to be clean: every line taken from it is made
//! inert and bounded (`clean_session_title`) before it is shown.

use event_ledger::ledger::clean_session_title;
use kernel::InProcessKernelClient;

/// Unmet criteria named before the line says how many more there are.
const MAX_UNMET: usize = 5;

/// Milliseconds since the Unix epoch of a ledger timestamp
/// (`YYYY-MM-DDTHH:MM:SS[.fff]Z`), or `None` for anything else.
pub(crate) fn epoch_millis(stamp: &str) -> Option<i64> {
    let stamp = stamp.strip_suffix('Z')?;
    let (date, time) = stamp.split_once('T')?;
    let mut date = date.split('-');
    let (year, month, day) = (
        date.next()?.parse::<i64>().ok()?,
        date.next()?.parse::<i64>().ok()?,
        date.next()?.parse::<i64>().ok()?,
    );
    if date.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (clock, fraction) = time.split_once('.').unwrap_or((time, "0"));
    let mut clock = clock.split(':');
    let (hour, minute, second) = (
        clock.next()?.parse::<i64>().ok()?,
        clock.next()?.parse::<i64>().ok()?,
        clock.next()?.parse::<i64>().ok()?,
    );
    if clock.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis: i64 = format!("{fraction:0<3}")[..3].parse().ok()?;
    // Days from the civil calendar (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + millis)
}

/// `45s`, `2m 13s`, `1h 05m`.
pub(crate) fn human_duration(millis: i64) -> String {
    let seconds = millis.max(0) / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60)
    }
}

/// The active goal's criteria that are not satisfied, as `(id, text)`.
/// Empty when there is no goal, no criteria, or the goal cannot be read.
pub(crate) fn unmet_criteria(ledger_path: &std::path::Path) -> Vec<(String, String)> {
    let Some(goal_path) = ledger_path
        .parent()
        .map(|dir| dir.join(crate::goal_host::GOAL_FILE))
    else {
        return Vec::new();
    };
    let Ok(Some(host)) = crate::goal_host::GoalHost::load(&goal_path) else {
        return Vec::new();
    };
    let (Some(snapshot), Some(verdicts)) = (
        host.snapshot(),
        host.validate(&agent_runtime::CancellationToken::new()),
    ) else {
        return Vec::new();
    };
    snapshot
        .completion_criteria()
        .iter()
        .filter(|criterion| {
            !verdicts
                .verdicts()
                .iter()
                .any(|v| v.criterion_id() == criterion.id() && v.satisfied())
        })
        .map(|criterion| (criterion.id().to_owned(), criterion.text().to_owned()))
        .collect()
}

/// The recap for `session`, or `None` when it has had no turn.
pub(crate) fn recap(
    client: &InProcessKernelClient,
    session: protocol::SessionId,
    unmet: &[(String, String)],
) -> Option<String> {
    // Three bounded reads, however long the session: how many turns, the
    // newest one's start, and the newest turn record of any kind — which
    // is that turn's end if it has one (turns do not overlap).
    let turns = client.count_of_kind(session, "turn.started").ok()?;
    if turns == 0 {
        return None;
    }
    let started = client
        .edge_event_of_kind(session, "turn.started", true)
        .ok()??;
    let turn_id = started.payload()["turn_id"].as_str()?.to_owned();
    let newest = client.edge_event_of_kind(session, "turn.", true).ok()??;
    let ended = (newest.payload()["turn_id"].as_str() == Some(turn_id.as_str())
        && matches!(
            newest.kind(),
            event_ledger::event::EventKind::TurnCompleted
                | event_ledger::event::EventKind::TurnFailed
                | event_ledger::event::EventKind::TurnInterrupted
        ))
    .then_some(&newest);
    let started = &started;
    let first_line = |value: &serde_json::Value| {
        value
            .as_str()
            .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
            .and_then(clean_session_title)
    };
    let asked = first_line(&started.payload()["text"]).unwrap_or_else(|| "(no text)".to_owned());
    let took = ended.and_then(|end| {
        Some(human_duration(
            epoch_millis(end.recorded_at().as_str())?
                - epoch_millis(started.recorded_at().as_str())?,
        ))
    });
    let (how, tail) = match ended.map(|end| (end.kind(), end.payload())) {
        Some((event_ledger::event::EventKind::TurnCompleted, payload)) => (
            "completed",
            first_line(&payload["text"]).map(|text| format!("it ended with: {text}")),
        ),
        Some((event_ledger::event::EventKind::TurnFailed, payload)) => (
            "failed",
            first_line(&payload["reason"]).map(|text| format!("why: {text}")),
        ),
        Some((_, payload)) => (
            "was interrupted",
            first_line(&payload["reason"]).map(|text| format!("reason: {text}")),
        ),
        None => ("did not finish — the session ended mid-turn", None),
    };
    let mut out = format!(
        "resumed session — {turns} turn{}; the last one: {asked}\\n",
        if turns == 1 { "" } else { "s" }
    );
    match took {
        Some(took) => out.push_str(&format!("  it {how} after {took}\\n")),
        None => out.push_str(&format!("  it {how}\\n")),
    }
    if let Some(tail) = tail {
        out.push_str(&format!("  {tail}\\n"));
    }
    if !unmet.is_empty() {
        let shown: Vec<String> = unmet
            .iter()
            .take(MAX_UNMET)
            .map(|(id, text)| {
                let text = clean_session_title(text).unwrap_or_default();
                let id = clean_session_title(id).unwrap_or_default();
                format!("{id}: {text}")
            })
            .collect();
        out.push_str(&format!(
            "  goal criteria not yet met ({}): {}{}\\n",
            unmet.len(),
            shown.join("; "),
            if unmet.len() > MAX_UNMET {
                format!("; and {} more", unmet.len() - MAX_UNMET)
            } else {
                String::new()
            }
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ledger_timestamp_is_read_to_the_millisecond() {
        assert_eq!(epoch_millis("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(epoch_millis("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(epoch_millis("1970-01-02T00:00:00.5Z"), Some(86_400_500));
        // A leap day, and a date far from the epoch.
        assert_eq!(epoch_millis("2000-03-01T00:00:00Z"), Some(951_868_800_000));
        assert_eq!(
            epoch_millis("2026-09-29T02:44:45.123Z"),
            Some(1_790_649_885_123)
        );
        for bad in [
            "",
            "2026-09-29",
            "2026-09-29T02:44:45",
            "2026-13-01T00:00:00Z",
            "2026-09-29T25:00:00Z",
            "2026-09-29T00:00:00.xZ",
            "not a time Z",
        ] {
            assert_eq!(epoch_millis(bad), None, "{bad}");
        }
    }

    #[test]
    fn durations_read_as_a_person_would_say_them() {
        assert_eq!(human_duration(-5), "0s");
        assert_eq!(human_duration(45_900), "45s");
        assert_eq!(human_duration(133_000), "2m 13s");
        assert_eq!(human_duration(125_000), "2m 05s");
        assert_eq!(human_duration(3_900_000), "1h 05m");
    }
}
