//! `rapid usage`: what a session's model steps consumed and cost, reduced
//! from the ledger's `model.completed` records — the record of what was
//! billed, never a second count kept beside it (SEAM-07 AC-01).
//!
//! A step's cost is what the provider reported, or unknown. Unknown is
//! never read as zero: a turn with any unknown step has an unknown cost, and
//! so does a total with any unknown turn — the known part is shown beside
//! it, named as partial. A ledger written before steps recorded their split
//! and cost reduces the same way: tokens as recorded, split and cost unknown.

use std::fmt::Write as _;

/// One `model.completed` record, as the reducer reads it.
#[derive(Clone, Debug)]
pub struct StepRecord {
    pub session: String,
    pub recorded_at: String,
    pub payload: serde_json::Value,
}

/// What a set of steps consumed. Every split is `None` when any step's is
/// unknown; the cost likewise, with the known part kept in `cost_known_*`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Usage {
    pub steps: u64,
    pub tokens: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    /// Steps whose token count was estimated (the provider reported none),
    /// or recorded before the ledger said.
    pub tokens_estimated_steps: u64,
    /// The provider-reported cost, when every step reported one.
    pub cost_usd_micros: Option<u64>,
    /// The sum of the steps that did report one.
    pub cost_known_usd_micros: u64,
    /// Steps whose cost is unknown.
    pub cost_unknown_steps: u64,
}

impl Usage {
    fn add(&mut self, step: &serde_json::Value) {
        let field = |name: &str| step.get(name).and_then(serde_json::Value::as_u64);
        let first = self.steps == 0;
        self.steps += 1;
        // A failed step's record carries no `tokens`: its split is its count.
        let tokens = field("tokens")
            .or_else(|| Some(field("input_tokens")?.saturating_add(field("output_tokens")?)));
        self.tokens = self.tokens.saturating_add(tokens.unwrap_or(0));
        let sum = |total: Option<u64>, next: Option<u64>| {
            if first {
                next
            } else {
                Some(total?.saturating_add(next?))
            }
        };
        self.input_tokens = sum(self.input_tokens, field("input_tokens"));
        self.output_tokens = sum(self.output_tokens, field("output_tokens"));
        self.cached_tokens = sum(self.cached_tokens, field("cached_tokens"));
        // Estimated when the record says so — and when it does not say at
        // all (a record from before the ledger carried the flag).
        if step
            .get("tokens_estimated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
        {
            self.tokens_estimated_steps += 1;
        }
        let reported = step
            .get("cost")
            .filter(|cost| cost.get("kind").and_then(serde_json::Value::as_str) == Some("reported"))
            .and_then(|cost| cost.get("usd_micros"))
            .and_then(serde_json::Value::as_u64);
        match reported {
            Some(micros) => {
                self.cost_known_usd_micros = self.cost_known_usd_micros.saturating_add(micros);
            }
            None => self.cost_unknown_steps += 1,
        }
        self.cost_usd_micros = (self.cost_unknown_steps == 0).then_some(self.cost_known_usd_micros);
    }

    fn merge(&mut self, other: &Usage) {
        if other.steps == 0 {
            return;
        }
        let first = self.steps == 0;
        let sum = |total: Option<u64>, next: Option<u64>| {
            if first {
                next
            } else {
                Some(total?.saturating_add(next?))
            }
        };
        self.steps += other.steps;
        self.tokens = self.tokens.saturating_add(other.tokens);
        self.input_tokens = sum(self.input_tokens, other.input_tokens);
        self.output_tokens = sum(self.output_tokens, other.output_tokens);
        self.cached_tokens = sum(self.cached_tokens, other.cached_tokens);
        self.tokens_estimated_steps += other.tokens_estimated_steps;
        self.cost_known_usd_micros = self
            .cost_known_usd_micros
            .saturating_add(other.cost_known_usd_micros);
        self.cost_unknown_steps += other.cost_unknown_steps;
        self.cost_usd_micros = (self.cost_unknown_steps == 0).then_some(self.cost_known_usd_micros);
    }

    /// `none` when there were no steps; `reported` when every step's cost
    /// and tokens were the provider's;
    /// `estimated` when some tokens were estimated; `unknown` when some
    /// cost was not reported.
    pub fn basis(&self) -> &'static str {
        if self.steps == 0 {
            "none"
        } else if self.cost_unknown_steps > 0 {
            "unknown"
        } else if self.tokens_estimated_steps > 0 {
            "estimated"
        } else {
            "reported"
        }
    }
}

/// One turn's usage.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TurnUsage {
    pub session: String,
    pub turn: String,
    pub started_at: String,
    #[serde(flatten)]
    pub usage: Usage,
}

/// Every turn, in the order its first step was recorded, and the total.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub turns: Vec<TurnUsage>,
    pub total: Usage,
}

pub const USAGE_SCHEMA: &str = "rapidlm.usage/v1";

/// Reduce `records` (any order within a session is kept as given) per turn.
/// A record with no `turn_id` is its own turn, named `-`.
pub fn reduce(records: &[StepRecord]) -> Report {
    let mut turns: Vec<TurnUsage> = Vec::new();
    for record in records {
        let turn = record
            .payload
            .get("turn_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-")
            .to_owned();
        let at = match turns
            .iter()
            .position(|row| row.session == record.session && row.turn == turn && turn != "-")
        {
            Some(at) => at,
            None => {
                turns.push(TurnUsage {
                    session: record.session.clone(),
                    turn,
                    started_at: record.recorded_at.clone(),
                    usage: Usage::default(),
                });
                turns.len() - 1
            }
        };
        turns[at].usage.add(&record.payload);
    }
    let mut total = Usage::default();
    for turn in &turns {
        total.merge(&turn.usage);
    }
    Report {
        schema: USAGE_SCHEMA,
        turns,
        total,
    }
}

fn opt(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

/// USD from micros, six places: `0.001234`.
fn usd(micros: u64) -> String {
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}

fn cost(usage: &Usage) -> String {
    // No steps cost nothing — the one zero that is known.
    if usage.steps == 0 {
        return usd(0);
    }
    match usage.cost_usd_micros {
        Some(micros) => usd(micros),
        None if usage.cost_known_usd_micros > 0 => {
            format!("unknown(>={})", usd(usage.cost_known_usd_micros))
        }
        None => "unknown".to_owned(),
    }
}

/// Tab-separated: a header, a row per turn, and a `total` row.
pub fn tsv(report: &Report) -> String {
    let mut out = String::from(
        "session\tturn\tstarted_at\tsteps\ttokens\tinput_tokens\toutput_tokens\tcached_tokens\tcost_usd\tbasis\n",
    );
    let mut row = |session: &str, turn: &str, at: &str, usage: &Usage| {
        let opt = |value: Option<u64>| {
            if usage.steps == 0 {
                "0".to_owned()
            } else {
                opt(value)
            }
        };
        let _ = writeln!(
            out,
            "{session}\t{turn}\t{at}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            usage.steps,
            usage.tokens,
            opt(usage.input_tokens),
            opt(usage.output_tokens),
            opt(usage.cached_tokens),
            cost(usage),
            usage.basis()
        );
    };
    for turn in &report.turns {
        row(&turn.session, &turn.turn, &turn.started_at, &turn.usage);
    }
    row("total", "-", "-", &report.total);
    out
}

/// One line: the total.
pub fn quiet(report: &Report) -> String {
    let total = &report.total;
    format!(
        "turns={} steps={} tokens={} cost_usd={} basis={}\n",
        report.turns.len(),
        total.steps,
        total.tokens,
        cost(total),
        total.basis()
    )
}

/// `--since`: an RFC 3339 UTC time or a date, as the ledger's own sortable
/// prefix (`YYYY-MM-DDTHH:MM:SS`).
pub fn since_prefix(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .is_some_and(|part| part.iter().all(u8::is_ascii_digit))
    };
    let date = raw.len() >= 10
        && digits(0..4)
        && bytes[4] == b'-'
        && digits(5..7)
        && bytes[7] == b'-'
        && digits(8..10);
    if !date {
        return None;
    }
    if raw.len() == 10 {
        return Some(format!("{raw}T00:00:00"));
    }
    let time = raw.len() >= 19
        && bytes[10] == b'T'
        && digits(11..13)
        && bytes[13] == b':'
        && digits(14..16)
        && bytes[16] == b':'
        && digits(17..19);
    let zone = &raw[19.min(raw.len())..];
    let utc = zone == "Z"
        || zone == "+00:00"
        || zone
            .strip_prefix('.')
            .and_then(|rest| rest.strip_suffix('Z'))
            .is_some_and(|frac| !frac.is_empty() && frac.bytes().all(|b| b.is_ascii_digit()));
    (time && utc).then(|| raw[..19].to_owned())
}

pub const USAGE_HELP: &str = "\
usage: rapid usage [<session-id>] [--project] [--since <time>] [--output tsv|json] [--quiet]

What model steps consumed and cost, per turn, reduced from this project's
ledger (the `model.completed` record each step leaves). With no session id,
the session with the most recent activity; --project reads every session.

  <session-id>        One recorded session (see `rapid sessions list`)
  --project           Every session in this project's ledger
  --since <time>      Only steps recorded at or after <time>: an RFC 3339
                      UTC time (2026-09-27T12:00:00Z) or a date (2026-09-27)
  --output tsv|json   A tab-separated table (the default: a header, a row
                      per turn, a `total` row) or one JSON document
  --quiet             Only the total, on one line
  -h, --help          Print this help

A cost is what the provider reported, or `unknown` — never zero for a step
that reported none. A turn or total with any unknown step shows `unknown`,
with the reported part as `unknown(>=<usd>)`. `basis` says which: `none` (no steps), `reported`,
`estimated` (some token counts were estimated because the provider reported
none) or `unknown` (some cost was not reported). Steps recorded before the
ledger carried the split read as estimated with an unknown cost.

Exit code:
  0   the report was printed (an empty one included)
  2   bad arguments, or an unknown session
";

#[cfg(test)]
mod tests {
    use super::*;

    fn step(session: &str, turn: &str, payload: serde_json::Value) -> StepRecord {
        let mut payload = payload;
        payload["turn_id"] = turn.into();
        StepRecord {
            session: session.to_owned(),
            recorded_at: "2026-09-27T10:00:00Z".to_owned(),
            payload,
        }
    }

    fn reported(tokens: u64, input: u64, output: u64, micros: u64) -> serde_json::Value {
        serde_json::json!({
            "tokens": tokens, "input_tokens": input, "output_tokens": output,
            "cached_tokens": 0, "tokens_estimated": false,
            "cost": {"kind": "reported", "usd_micros": micros},
        })
    }

    #[test]
    fn totals_are_the_sum_of_the_records() {
        let records = vec![
            step("s", "t1", reported(30, 20, 10, 150)),
            step("s", "t1", reported(12, 8, 4, 50)),
            step("s", "t2", reported(7, 5, 2, 25)),
        ];
        let report = reduce(&records);
        assert_eq!(report.turns.len(), 2);
        assert_eq!(report.turns[0].usage.steps, 2);
        assert_eq!(report.turns[0].usage.tokens, 42);
        assert_eq!(report.turns[0].usage.cost_usd_micros, Some(200));
        assert_eq!(report.total.tokens, 49);
        assert_eq!(report.total.input_tokens, Some(33));
        assert_eq!(report.total.output_tokens, Some(16));
        assert_eq!(report.total.cost_usd_micros, Some(225));
        assert_eq!(report.total.basis(), "reported");
        let table = tsv(&report);
        assert!(
            table.ends_with("total\t-\t-\t3\t49\t33\t16\t0\t0.000225\treported\n"),
            "{table}"
        );
    }

    #[test]
    fn an_unknown_cost_is_never_zero_and_estimates_are_flagged() {
        let mut unknown = reported(10, 6, 4, 0);
        unknown["cost"] = serde_json::json!({"kind": "unknown"});
        let mut estimated = reported(9, 0, 0, 40);
        estimated["tokens_estimated"] = true.into();
        estimated["input_tokens"] = serde_json::Value::Null;
        let report = reduce(&[
            step("s", "t1", reported(5, 3, 2, 60)),
            step("s", "t1", unknown),
            step("s", "t2", estimated),
        ]);
        let first = &report.turns[0].usage;
        assert_eq!(first.cost_usd_micros, None);
        assert_eq!(first.cost_known_usd_micros, 60);
        assert_eq!(first.basis(), "unknown");
        assert_eq!(report.turns[1].usage.basis(), "estimated");
        assert_eq!(report.turns[1].usage.input_tokens, None);
        assert_eq!(report.total.cost_usd_micros, None);
        assert_eq!(report.total.input_tokens, None, "a part unknown");
        assert!(quiet(&report).contains("cost_usd=unknown(>=0.000100) basis=unknown"));
    }

    #[test]
    fn a_ledger_from_before_the_split_reduces_with_cost_unknown() {
        // What `model.completed` carried before SEAM-07: tokens, no split,
        // no cost, no estimate flag.
        let report = reduce(&[
            step(
                "s",
                "t1",
                serde_json::json!({"tokens": 40, "request_id": "r1"}),
            ),
            step(
                "s",
                "t1",
                serde_json::json!({"tokens": 2, "request_id": "r2"}),
            ),
        ]);
        let usage = &report.total;
        assert_eq!(usage.tokens, 42);
        assert_eq!(usage.cost_usd_micros, None, "unknown, never zero");
        assert_eq!(usage.cost_known_usd_micros, 0);
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.tokens_estimated_steps, 2);
        assert!(
            tsv(&report).contains("\tunknown\tunknown\n"),
            "{}",
            tsv(&report)
        );
    }

    #[test]
    fn the_tui_and_rapid_usage_total_the_same_records_alike() {
        // One rule, two readers: the `/usage` tab's fold and `rapid usage`.
        let mut unknown = reported(10, 6, 4, 0);
        unknown["cost"] = serde_json::json!({"kind": "unknown"});
        let payloads = [
            reported(30, 20, 10, 150),
            unknown,
            serde_json::json!({"tokens": 7}),
            serde_json::json!({"tokens": null, "input_tokens": 2, "output_tokens": 1,
                "tokens_estimated": false, "cost": {"kind": "reported", "usd_micros": 5}}),
        ];
        let records: Vec<StepRecord> = payloads
            .iter()
            .enumerate()
            .map(|(at, payload)| step("s", &format!("t{at}"), payload.clone()))
            .collect();
        let report = reduce(&records);
        let mut tui = tui::state::SessionUsage::default();
        for record in &records {
            tui.add_step(&record.payload);
        }
        let total = &report.total;
        assert_eq!(
            (
                tui.steps,
                tui.tokens,
                tui.input_tokens,
                tui.output_tokens,
                tui.cached_tokens
            ),
            (
                total.steps,
                total.tokens,
                total.input_tokens,
                total.output_tokens,
                total.cached_tokens
            )
        );
        assert_eq!(tui.cost_usd_micros(), total.cost_usd_micros);
        assert_eq!(tui.cost_known_usd_micros, total.cost_known_usd_micros);
        assert_eq!(tui.tokens_estimated_steps, total.tokens_estimated_steps);
        assert_eq!(tui.turns, report.turns.len() as u64);
    }

    #[test]
    fn no_steps_is_a_known_zero() {
        let report = reduce(&[]);
        assert_eq!(report.total.basis(), "none");
        assert!(tsv(&report).ends_with("total\t-\t-\t0\t0\t0\t0\t0\t0.000000\tnone\n"));
        assert_eq!(
            quiet(&report),
            "turns=0 steps=0 tokens=0 cost_usd=0.000000 basis=none\n"
        );
    }

    #[test]
    fn since_accepts_a_utc_time_or_a_date_only() {
        assert_eq!(
            since_prefix("2026-09-27").as_deref(),
            Some("2026-09-27T00:00:00")
        );
        assert_eq!(
            since_prefix("2026-09-27T12:30:00Z").as_deref(),
            Some("2026-09-27T12:30:00")
        );
        assert_eq!(
            since_prefix("2026-09-27T12:30:00.5Z").as_deref(),
            Some("2026-09-27T12:30:00")
        );
        for bad in [
            "yesterday",
            "2026-9-27",
            "2026-09-27T12:30:00+02:00",
            "2026-09-27T12:30",
        ] {
            assert_eq!(since_prefix(bad), None, "{bad}");
        }
    }
}
