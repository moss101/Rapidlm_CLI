//! Host reminder feeds injected into the model context as one bounded,
//! system-labeled block.
//!
//! A roster (parsed from a project TOML artifact) declares named feeds of
//! reminders. Per turn the host nominates which feeds are active — typically
//! the feeds whose required capability the host has granted — and
//! [`ActiveReminders::admit`] runs the fail-closed admission pass: at most
//! two feeds, a hard byte budget, every drop recorded with a reason. Nothing
//! is silently truncated; a reminder that does not fit is reported, not
//! included.
//!
//! Each reminder carries a [`ReminderFloor`]: the minimum reasoning effort
//! the host should use while it is active. The roster stores the semantics;
//! the composition root maps them onto its provider vocabulary.

use std::fmt;

/// Wire identity of the roster artifact format and the rendered block.
pub const REMINDERS_SCHEMA: &str = "rapidlm.reminders.v1";

/// Feeds a roster may declare.
pub const MAX_ROSTER_FEEDS: usize = 8;
/// Feeds a single turn may activate.
pub const MAX_ACTIVE_FEEDS: usize = 2;
/// Reminders one feed may carry.
pub const MAX_REMINDERS_PER_FEED: usize = 8;
/// UTF-8 bytes allowed in one reminder text.
pub const MAX_REMINDER_TEXT_BYTES: usize = 1024;
/// Total byte budget for admitted reminder text in one turn.
pub const MAX_ACTIVE_REMINDER_BYTES: usize = 4096;
/// Bounded id/name length shared by feed names, reminder ids, and
/// capability references.
pub const MAX_NAME_BYTES: usize = 64;

/// Minimum reasoning effort a reminder asks the host to use.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub enum ReminderFloor {
    Baseline,
    Low,
    Medium,
    High,
    Max,
}

pub const REMINDER_FLOOR_NAMES: &[&str] = &["baseline", "low", "medium", "high", "max"];

impl ReminderFloor {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    /// Case-insensitive parse over the fixed name table.
    pub fn parse(raw: &str) -> Option<Self> {
        REMINDER_FLOOR_NAMES
            .iter()
            .copied()
            .position(|name| name.eq_ignore_ascii_case(raw))
            .map(|index| Self::ALL[index])
    }

    const ALL: [Self; 5] = [
        Self::Baseline,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Max,
    ];

    /// The stronger of two floors.
    pub fn max_of(self, other: Self) -> Self {
        if self >= other { self } else { other }
    }
}

impl fmt::Display for ReminderFloor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One reminder inside a feed.
#[derive(Clone, Debug, PartialEq)]
pub struct Reminder {
    pub id: String,
    pub text: String,
    pub floor: ReminderFloor,
}

/// A named feed of reminders, optionally gated on a capability: the host
/// should only nominate feeds whose required capability it has granted.
#[derive(Clone, Debug, PartialEq)]
pub struct ReminderFeed {
    pub name: String,
    /// Capability that must be granted for the host to nominate this feed.
    pub requires: Option<String>,
    pub reminders: Vec<Reminder>,
}

/// Parsed roster artifact. Feed and reminder order is file order; it is the
/// admission priority.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReminderRoster {
    feeds: Vec<ReminderFeed>,
}

/// Typed failures for roster parsing.
#[derive(Debug)]
pub enum ReminderError {
    Parse { reason: String },
    SchemaMismatch { found: String },
    UnknownField { field: String },
    NameInvalid,
    NameTooLong { limit: usize },
    TextTooLarge { limit: usize, observed: usize },
    UnknownFloor { name: String },
    RosterTooManyFeeds { limit: usize },
    FeedTooManyReminders { limit: usize },
}

impl fmt::Display for ReminderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse { reason } => write!(f, "reminders roster is invalid: {reason}"),
            Self::SchemaMismatch { found } => write!(
                f,
                "reminders roster schema '{found}' is unsupported; expected '{REMINDERS_SCHEMA}'"
            ),
            Self::UnknownField { field } => {
                write!(f, "reminders roster has unknown field '{field}'")
            }
            Self::NameInvalid => write!(
                f,
                "feed and reminder names must be {MAX_NAME_BYTES} bytes of lowercase letters, digits, and single interior dashes"
            ),
            Self::NameTooLong { limit } => write!(f, "name exceeds {limit} bytes"),
            Self::TextTooLarge { limit, observed } => write!(
                f,
                "reminder text is {observed} bytes; limit is {limit} bytes"
            ),
            Self::UnknownFloor { name } => write!(
                f,
                "unknown reasoning floor '{name}'; expected one of {}",
                REMINDER_FLOOR_NAMES.join("|")
            ),
            Self::RosterTooManyFeeds { limit } => {
                write!(f, "roster exceeds {limit} feeds")
            }
            Self::FeedTooManyReminders { limit } => {
                write!(f, "feed exceeds {limit} reminders")
            }
        }
    }
}

impl std::error::Error for ReminderError {}

/// Canonical name alphabet shared by feed names, reminder ids, and the
/// roster schema's identifiers: lowercase letters, digits, single interior
/// dashes. Invalid raw names are never echoed.
fn valid_name(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_NAME_BYTES
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes.first() != Some(&b'-')
        && bytes.last() != Some(&b'-')
        && !bytes.windows(2).any(|pair| pair == b"--")
}

impl ReminderRoster {
    /// Parse a roster from TOML. Closed schema:
    ///
    /// ```toml
    /// schema = "rapidlm.reminders.v1"
    /// [[feed]]
    /// name = "ops"
    /// requires = "net"            # optional capability gate
    /// [[feed.reminder]]
    /// id = "budget-guard"
    /// text = "stay under the time budget"
    /// floor = "medium"            # optional, default baseline
    /// ```
    pub fn parse(toml_str: &str) -> Result<Self, ReminderError> {
        let value: toml::Value = toml::from_str(toml_str).map_err(|err| ReminderError::Parse {
            reason: err.to_string(),
        })?;
        let table = value.as_table().ok_or_else(|| ReminderError::Parse {
            reason: "top level must be a table".to_string(),
        })?;
        for key in table.keys() {
            if key != "schema" && key != "feed" {
                return Err(ReminderError::UnknownField { field: key.clone() });
            }
        }
        let schema = table
            .get("schema")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| ReminderError::Parse {
                reason: "missing schema marker".to_string(),
            })?;
        if schema != REMINDERS_SCHEMA {
            return Err(ReminderError::SchemaMismatch {
                found: schema.to_string(),
            });
        }
        let feeds = table.get("feed").ok_or_else(|| ReminderError::Parse {
            reason: "missing [[feed]] array".to_string(),
        })?;
        let feeds = feeds.as_array().ok_or_else(|| ReminderError::Parse {
            reason: "[[feed]] must be an array of tables".to_string(),
        })?;
        if feeds.len() > MAX_ROSTER_FEEDS {
            return Err(ReminderError::RosterTooManyFeeds {
                limit: MAX_ROSTER_FEEDS,
            });
        }
        let mut parsed = Vec::with_capacity(feeds.len());
        for feed in feeds {
            let feed = feed.as_table().ok_or_else(|| ReminderError::Parse {
                reason: "each [[feed]] entry must be a table".to_string(),
            })?;
            for key in feed.keys() {
                if !matches!(key.as_str(), "name" | "requires" | "reminder") {
                    return Err(ReminderError::UnknownField {
                        field: format!("feed.{key}"),
                    });
                }
            }
            let name = feed
                .get("name")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| ReminderError::Parse {
                    reason: "feed name must be a string".to_string(),
                })?;
            if !valid_name(name) {
                return Err(ReminderError::NameInvalid);
            }
            let requires = match feed.get("requires") {
                None => None,
                Some(requires) => {
                    let requires = requires.as_str().ok_or_else(|| ReminderError::Parse {
                        reason: "feed requires must be a string".to_string(),
                    })?;
                    if !valid_name(requires) {
                        return Err(ReminderError::NameInvalid);
                    }
                    Some(requires.to_string())
                }
            };
            let mut reminders = Vec::new();
            match feed.get("reminder") {
                None => {}
                Some(entries) => {
                    let entries = entries.as_array().ok_or_else(|| ReminderError::Parse {
                        reason: "feed reminder must be an array of tables".to_string(),
                    })?;
                    if entries.len() > MAX_REMINDERS_PER_FEED {
                        return Err(ReminderError::FeedTooManyReminders {
                            limit: MAX_REMINDERS_PER_FEED,
                        });
                    }
                    for entry in entries {
                        let entry = entry.as_table().ok_or_else(|| ReminderError::Parse {
                            reason: "each reminder must be a table".to_string(),
                        })?;
                        for key in entry.keys() {
                            if !matches!(key.as_str(), "id" | "text" | "floor") {
                                return Err(ReminderError::UnknownField {
                                    field: format!("reminder.{key}"),
                                });
                            }
                        }
                        let id =
                            entry
                                .get("id")
                                .and_then(toml::Value::as_str)
                                .ok_or_else(|| ReminderError::Parse {
                                    reason: "reminder id must be a string".to_string(),
                                })?;
                        if !valid_name(id) {
                            return Err(ReminderError::NameInvalid);
                        }
                        let text =
                            entry
                                .get("text")
                                .and_then(toml::Value::as_str)
                                .ok_or_else(|| ReminderError::Parse {
                                    reason: "reminder text must be a string".to_string(),
                                })?;
                        if text.len() > MAX_REMINDER_TEXT_BYTES {
                            return Err(ReminderError::TextTooLarge {
                                limit: MAX_REMINDER_TEXT_BYTES,
                                observed: text.len(),
                            });
                        }
                        let floor = match entry.get("floor") {
                            None => ReminderFloor::Baseline,
                            Some(floor) => {
                                let floor = floor.as_str().ok_or_else(|| ReminderError::Parse {
                                    reason: "reminder floor must be a string".to_string(),
                                })?;
                                ReminderFloor::parse(floor).ok_or_else(|| {
                                    ReminderError::UnknownFloor {
                                        name: floor.to_string(),
                                    }
                                })?
                            }
                        };
                        reminders.push(Reminder {
                            id: id.to_string(),
                            text: text.to_string(),
                            floor,
                        });
                    }
                }
            }
            parsed.push(ReminderFeed {
                name: name.to_string(),
                requires,
                reminders,
            });
        }
        Ok(Self { feeds: parsed })
    }

    pub fn feeds(&self) -> &[ReminderFeed] {
        &self.feeds
    }

    /// Look up a feed by name.
    pub fn feed(&self, name: &str) -> Option<&ReminderFeed> {
        self.feeds.iter().find(|feed| feed.name == name)
    }
}

/// One reminder that passed admission.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedReminder {
    pub feed: String,
    pub id: String,
    pub text: String,
    pub floor: ReminderFloor,
    pub bytes: usize,
}

/// One nomination or reminder that failed admission, with the reason. The
/// reason text is bounded and free of reminder content.
#[derive(Clone, Debug, PartialEq)]
pub struct DroppedReminder {
    pub feed: String,
    pub id: Option<String>,
    pub reason: String,
}

/// Admission outcome for one turn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActiveReminders {
    admitted: Vec<AdmittedReminder>,
    dropped: Vec<DroppedReminder>,
    feeds_admitted: usize,
}

impl ActiveReminders {
    /// Admit reminders for the feeds the host nominated. Deterministic:
    /// request order selects feeds, roster order selects reminders, and the
    /// first reminders in that order win the byte budget. Every failure is
    /// recorded in `dropped` — admission never fails the turn, but it never
    /// hides a drop either.
    pub fn admit(roster: &ReminderRoster, nominated: &[String]) -> Self {
        let mut active = Self::default();
        let mut seen: Vec<&str> = Vec::new();
        for name in nominated {
            // A duplicated nomination is a host bug: drop the repeat.
            if seen.contains(&name.as_str()) {
                active.dropped.push(DroppedReminder {
                    feed: name.clone(),
                    id: None,
                    reason: "duplicate feed nomination".to_string(),
                });
                continue;
            }
            seen.push(name);
            if active.feed_count() >= MAX_ACTIVE_FEEDS {
                active.dropped.push(DroppedReminder {
                    feed: name.clone(),
                    id: None,
                    reason: format!("active feed limit is {MAX_ACTIVE_FEEDS}"),
                });
                continue;
            }
            let Some(feed) = roster.feed(name) else {
                active.dropped.push(DroppedReminder {
                    feed: name.clone(),
                    id: None,
                    reason: "unknown feed".to_string(),
                });
                continue;
            };
            active.feeds_admitted += 1;
            active.admit_feed(feed);
        }
        active
    }

    fn feed_count(&self) -> usize {
        self.feeds_admitted
    }

    fn admit_feed(&mut self, feed: &ReminderFeed) {
        for reminder in &feed.reminders {
            let bytes = reminder.text.len();
            let used: usize = self.admitted.iter().map(|r| r.bytes).sum();
            if used + bytes > MAX_ACTIVE_REMINDER_BYTES {
                self.dropped.push(DroppedReminder {
                    feed: feed.name.clone(),
                    id: Some(reminder.id.clone()),
                    reason: format!("byte budget exceeded (limit {MAX_ACTIVE_REMINDER_BYTES})"),
                });
                continue;
            }
            self.admitted.push(AdmittedReminder {
                feed: feed.name.clone(),
                id: reminder.id.clone(),
                text: reminder.text.clone(),
                floor: reminder.floor,
                bytes,
            });
        }
    }

    pub fn admitted(&self) -> &[AdmittedReminder] {
        &self.admitted
    }

    pub fn dropped(&self) -> &[DroppedReminder] {
        &self.dropped
    }

    pub fn is_empty(&self) -> bool {
        self.admitted.is_empty()
    }

    /// The strongest floor across admitted reminders; baseline when empty.
    pub fn effort_floor(&self) -> ReminderFloor {
        self.admitted
            .iter()
            .fold(ReminderFloor::Baseline, |acc, r| acc.max_of(r.floor))
    }

    /// Render the deterministic context block, or `None` when nothing is
    /// admitted (empty turns add no block to the packet).
    pub fn render(&self) -> Option<String> {
        if self.admitted.is_empty() {
            return None;
        }
        let feeds: Vec<&str> = {
            let mut names: Vec<&str> = Vec::new();
            for r in &self.admitted {
                if !names.contains(&r.feed.as_str()) {
                    names.push(&r.feed);
                }
            }
            names
        };
        let mut out = format!(
            "reminders schema={REMINDERS_SCHEMA} feeds={}\n",
            feeds.join(",")
        );
        for r in &self.admitted {
            out.push_str(&format!(
                "[{}/{} floor={}] {}\n",
                r.feed,
                r.id,
                r.floor.as_str(),
                r.text
            ));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster_toml() -> String {
        format!(
            "schema = \"{REMINDERS_SCHEMA}\"\n\
             [[feed]]\n\
             name = \"ops\"\n\
             [[feed.reminder]]\n\
             id = \"budget-guard\"\n\
             text = \"stay under the time budget\"\n\
             floor = \"medium\"\n\
             [[feed]]\n\
             name = \"safety\"\n\
             requires = \"exec\"\n\
             [[feed.reminder]]\n\
             id = \"no-force\"\n\
             text = \"never force-push\"\n\
             floor = \"high\"\n"
        )
    }

    #[test]
    fn parse_round_trips_feeds_reminders_and_floors() {
        let roster = ReminderRoster::parse(&roster_toml()).expect("parse");
        assert_eq!(roster.feeds().len(), 2);
        let ops = roster.feed("ops").expect("ops feed");
        assert!(ops.requires.is_none());
        assert_eq!(ops.reminders.len(), 1);
        assert_eq!(ops.reminders[0].floor, ReminderFloor::Medium);
        let safety = roster.feed("safety").expect("safety feed");
        assert_eq!(safety.requires.as_deref(), Some("exec"));
        assert_eq!(safety.reminders[0].floor, ReminderFloor::High);
    }

    #[test]
    fn parse_rejects_wrong_schema_unknown_fields_and_bad_names() {
        let err =
            ReminderRoster::parse("schema = \"rapidlm.reminders.v0\"\n[[feed]]\nname = \"a\"\n")
                .expect_err("schema");
        assert!(matches!(err, ReminderError::SchemaMismatch { .. }));
        let bad_field = {
            // Injected before any [[feed]] header so the key is top-level.
            let full = roster_toml();
            match full.split_once("[[feed]]") {
                Some((head, tail)) => format!("{head}surprise = 1\n[[feed]]{tail}"),
                None => panic!("feed marker missing"),
            }
        };
        let err = ReminderRoster::parse(&bad_field).expect_err("unknown field");
        assert!(err.to_string().contains("unknown field 'surprise'"));
        let bad_name = roster_toml().replace("name = \"ops\"", "name = \"Ops!\"");
        let err = ReminderRoster::parse(&bad_name).expect_err("name");
        assert!(matches!(err, ReminderError::NameInvalid));
        let bad_floor = roster_toml().replace("floor = \"medium\"", "floor = \"ludicrous\"");
        let err = ReminderRoster::parse(&bad_floor).expect_err("floor");
        assert!(err.to_string().contains("unknown reasoning floor"));
    }

    #[test]
    fn floor_parse_is_case_insensitive_and_max_of_takes_the_stronger() {
        assert_eq!(ReminderFloor::parse("HIGH"), Some(ReminderFloor::High));
        assert_eq!(
            ReminderFloor::parse("baseline"),
            Some(ReminderFloor::Baseline)
        );
        assert_eq!(ReminderFloor::parse("nope"), None);
        assert_eq!(
            ReminderFloor::Low.max_of(ReminderFloor::High),
            ReminderFloor::High
        );
        assert_eq!(
            ReminderFloor::Max.max_of(ReminderFloor::Low),
            ReminderFloor::Max
        );
    }

    #[test]
    fn admit_selects_nominated_feeds_in_request_order() {
        let roster = ReminderRoster::parse(&roster_toml()).expect("parse");
        let active = ActiveReminders::admit(&roster, &["safety".to_string(), "ops".to_string()]);
        assert_eq!(active.admitted().len(), 2);
        assert_eq!(active.admitted()[0].feed, "safety");
        assert_eq!(active.admitted()[1].feed, "ops");
        // Floors fold to the strongest.
        assert_eq!(active.effort_floor(), ReminderFloor::High);
        assert!(active.dropped().is_empty());
    }

    #[test]
    fn admit_drops_unknown_and_over_limit_feeds_with_reasons() {
        let roster = ReminderRoster::parse(&roster_toml()).expect("parse");
        let nominated = vec!["ghost".to_string(), "ops".to_string(), "ops".to_string()];
        let active = ActiveReminders::admit(&roster, &nominated);
        assert_eq!(active.feed_count(), 1);
        let reasons: Vec<&str> = active.dropped().iter().map(|d| d.reason.as_str()).collect();
        assert!(reasons.contains(&"unknown feed"));
        assert!(reasons.contains(&"duplicate feed nomination"));
        // A third valid feed exceeds the per-turn limit.
        let three_feeds = "schema = \"rapidlm.reminders.v1\"\n\
             [[feed]]\nname = \"a\"\n\
             [[feed]]\nname = \"b\"\n\
             [[feed]]\nname = \"c\"\n";
        let roster = ReminderRoster::parse(three_feeds).expect("parse three");
        let nominated = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let active = ActiveReminders::admit(&roster, &nominated);
        assert_eq!(active.feed_count(), MAX_ACTIVE_FEEDS);
        assert!(
            active
                .dropped()
                .iter()
                .any(|d| d.feed == "c" && d.reason.contains("active feed limit"))
        );
    }

    #[test]
    fn admit_enforces_the_byte_budget_and_records_drops() {
        // One reminder is capped at 1024 bytes, so exceeding the 4096 budget
        // needs several: four fit, the fifth does not.
        let big = "x".repeat(1_000);
        let mut toml = format!("schema = \"{REMINDERS_SCHEMA}\"\n[[feed]]\nname = \"wide\"\n");
        for index in 1..=5 {
            toml.push_str(&format!(
                "[[feed.reminder]]\nid = \"r{index}\"\ntext = \"{big}\"\n"
            ));
        }
        let roster = ReminderRoster::parse(&toml).expect("parse");
        let active = ActiveReminders::admit(&roster, &["wide".to_string()]);
        assert_eq!(active.admitted().len(), 4);
        assert_eq!(active.admitted()[0].id, "r1");
        assert_eq!(active.dropped().len(), 1);
        assert_eq!(active.dropped()[0].id.as_deref(), Some("r5"));
        assert!(active.dropped()[0].reason.contains("byte budget exceeded"));
    }

    #[test]
    fn render_is_deterministic_and_empty_admission_renders_nothing() {
        let roster = ReminderRoster::parse(&roster_toml()).expect("parse");
        let active = ActiveReminders::admit(&roster, &["ops".to_string(), "safety".to_string()]);
        let rendered = active.render().expect("render");
        let expected = format!(
            "reminders schema={REMINDERS_SCHEMA} feeds=ops,safety\n\
             [ops/budget-guard floor=medium] stay under the time budget\n\
             [safety/no-force floor=high] never force-push\n"
        );
        assert_eq!(rendered, expected);
        assert_eq!(rendered, active.render().expect("render again"));
        let empty = ActiveReminders::default();
        assert!(empty.render().is_none());
        assert_eq!(empty.effort_floor(), ReminderFloor::Baseline);
    }

    #[test]
    fn oversized_text_is_rejected_at_parse_time() {
        let long = "y".repeat(MAX_REMINDER_TEXT_BYTES + 1);
        let toml = format!(
            "schema = \"{REMINDERS_SCHEMA}\"\n\
             [[feed]]\n\
             name = \"wide\"\n\
             [[feed.reminder]]\n\
             id = \"big\"\n\
             text = \"{long}\"\n"
        );
        let err = ReminderRoster::parse(&toml).expect_err("too large");
        assert!(matches!(err, ReminderError::TextTooLarge { .. }));
    }
}
