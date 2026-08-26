//! Trigger/jitter/lease pure logic (P7-014..019).
//!
//! Pure, deterministic predicates + a bounded cron field parser. No wall-clock
//! or process I/O here — the runtime supplies time (unix seconds) and drives the
//! scheduler. `MAX_SEARCH_MINUTES` bounds the `next_after` search so a
//! `CronExpr` can never loop unbounded.

use std::error::Error;
use std::fmt;

/// Maximum UTF-8 bytes accepted in one cron expression.
pub const MAX_CRON_BYTES: usize = 256;
/// Maximum minutes searched forward to find the next cron fire.
pub const MAX_SEARCH_MINUTES: u64 = 60 * 24 * 366 * 2; // ~2 years
/// Maximum jitter bucket milliseconds.
pub const MAX_JITTER_MS: u64 = 60_000;

/// A unix-seconds instant. `0` is a valid epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct UnitTime(u64);

impl UnitTime {
    pub const fn new(secs: u64) -> Self {
        Self(secs)
    }
    pub const fn secs(self) -> u64 {
        self.0
    }
}

/// Bounded 5-field cron expression: minute hour day-of-month month day-of-week.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CronExpr {
    fields: [Field; 5],
    raw: String,
}

/// A parsed cron field (bounded set of allowed values).
#[derive(Clone, Debug, Eq, PartialEq)]
struct Field(BTreeSet<u64>);

impl Field {
    fn contains(&self, value: u64) -> bool {
        self.0.contains(&value)
    }
}

/// Deterministic jitter: a stable hash of a seed → offset in [0, bucket_ms).
pub fn deterministic_jitter(seed: u64, bucket_ms: u64) -> u64 {
    if bucket_ms == 0 {
        return 0;
    }
    let bucket_ms = bucket_ms.min(MAX_JITTER_MS);
    // SplitMix64-style deterministic mix; no RNG state.
    let mut z = seed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^= z >> 31;
    z % bucket_ms
}

/// Trigger kind. `Cron` uses a bounded expression; `Interval`/`OneShot` are
/// absolute-ish.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TriggerKind {
    Cron { expr: CronExpr },
    Interval { seconds: u64 },
    OneShot { at: UnitTime },
}

/// A bounded, durable trigger descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerSpec {
    id: String,
    kind: TriggerKind,
}

/// Durable cursor: last fire offset + a missed-one-shot mark.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TriggerCursor {
    last_fire: Option<u64>,
    missed: bool,
}

/// Decision after a scheduled fire point on the cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FireDecision {
    Fire,
    /// A one-shot whose time has passed but was never fired.
    MissedBackfill,
    Skip,
}

/// A job lease with generation + liveness heartbeats.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobLease {
    generation: u64,
    owner: String,
    issued_at: UnitTime,
    expires_at: UnitTime,
    heartbeat_at: UnitTime,
}

impl TriggerCursor {
    pub const fn new() -> Self {
        Self {
            last_fire: None,
            missed: false,
        }
    }
    pub const fn last_fire(&self) -> Option<u64> {
        self.last_fire
    }
    pub const fn missed(&self) -> bool {
        self.missed
    }
    /// Roll the cursor forward. A `MissedBackfill` marks the cursor so the
    /// runtime can emit a backfill decision rather than silently dropping it.
    pub fn advance(&mut self, decision: FireDecision, now: u64) {
        match decision {
            FireDecision::Fire => {
                self.last_fire = Some(now);
                self.missed = false;
            }
            FireDecision::MissedBackfill => {
                self.last_fire = Some(now);
                self.missed = true;
            }
            FireDecision::Skip => {}
        }
    }
}

impl TriggerSpec {
    pub fn new(id: impl Into<String>, kind: TriggerKind) -> Result<Self, TriggerError> {
        let id = id.into();
        if !valid_text(&id, MAX_CRON_BYTES) {
            return Err(TriggerError::InvalidId);
        }
        if let TriggerKind::Cron { expr } = &kind
            && expr.raw.len() > MAX_CRON_BYTES
        {
            return Err(TriggerError::InvalidExpression);
        }
        Ok(Self { id, kind })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub const fn kind(&self) -> &TriggerKind {
        &self.kind
    }
}

impl JobLease {
    pub fn new(
        generation: u64,
        owner: impl Into<String>,
        issued_at: UnitTime,
        lease_secs: u64,
    ) -> Self {
        let owner = owner.into();
        let expires_at = UnitTime::new(issued_at.secs().saturating_add(lease_secs.max(1)));
        Self {
            generation,
            owner,
            issued_at,
            expires_at,
            heartbeat_at: issued_at,
        }
    }
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub const fn issued_at(&self) -> UnitTime {
        self.issued_at
    }
    pub const fn expires_at(&self) -> UnitTime {
        self.expires_at
    }
    pub const fn heartbeat_at(&self) -> UnitTime {
        self.heartbeat_at
    }
    /// A lease is live if it is before expiry and heartbeat is within grace.
    pub fn is_live(&self, now: UnitTime, heartbeat_grace_secs: u64) -> bool {
        now.secs() < self.expires_at.secs()
            && now.secs().saturating_sub(self.heartbeat_at.secs())
                <= heartbeat_grace_secs
    }
    /// Renew the lease: bump expiry from `now` and refresh the heartbeat.
    pub fn renew(&mut self, now: UnitTime, lease_secs: u64) {
        self.expires_at = UnitTime::new(now.secs().saturating_add(lease_secs.max(1)));
        self.heartbeat_at = now;
    }
}

impl CronExpr {
    pub fn parse(raw: &str) -> Result<Self, TriggerError> {
        if !valid_text(raw, MAX_CRON_BYTES) {
            return Err(TriggerError::InvalidExpression);
        }
        let parts: Vec<&str> = raw.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(TriggerError::InvalidExpression);
        }
        let ranges = [
            (0u64, 59u64),
            (0, 23),
            (1, 31),
            (1, 12),
            (0, 6),
        ];
        let mut fields = Vec::with_capacity(5);
        for (i, part) in parts.iter().enumerate() {
            fields.push(parse_field(part, ranges[i])?);
        }
        let fields: [Field; 5] = fields.try_into().map_err(|_| TriggerError::InvalidExpression)?;
        Ok(Self {
            fields,
            raw: raw.to_string(),
        })
    }

    /// Next instant (>= `after`) matching this expression, or `None` if no match
    /// within `MAX_SEARCH_MINUTES`. Pure + bounded.
    pub fn next_after(&self, after: UnitTime) -> Option<UnitTime> {
        let start = after.secs();
        let mut t = start.saturating_add(60 - (start % 60));
        for _ in 0..MAX_SEARCH_MINUTES {
            if self.matches_minute(t) {
                return Some(UnitTime::new(t));
            }
            t = t.saturating_add(60);
        }
        None
    }

    pub fn expression(&self) -> &str {
        &self.raw
    }

    fn matches_minute(&self, t: u64) -> bool {
        let minute = (t / 60) % 60;
        let hour = (t / 3600) % 24;
        let day = (t / 86400) % 31 + 1;
        let month = (t / 2_592_000) % 12 + 1;
        let dow = (t / 86400 + 4) % 7; // epoch Thursday = 0 (sun)
        self.fields[0].contains(minute)
            && self.fields[1].contains(hour)
            && self.fields[2].contains(day)
            && self.fields[3].contains(month)
            && self.fields[4].contains(dow)
    }
}

impl TriggerKind {
    /// Evaluate a fire point against a one-shot / interval trigger.
    pub fn fires_at(&self, now: UnitTime, cursor: &TriggerCursor) -> FireDecision {
        match self {
            TriggerKind::OneShot { at } => {
                if cursor.last_fire().is_some() {
                    FireDecision::Skip
                } else if now.secs() >= at.secs() {
                    FireDecision::MissedBackfill
                } else {
                    FireDecision::Skip
                }
            }
            TriggerKind::Interval { seconds } => match cursor.last_fire() {
                Some(last) if now.secs().saturating_sub(last) >= *seconds => FireDecision::Fire,
                Some(_) => FireDecision::Skip,
                None => FireDecision::Fire,
            },
            TriggerKind::Cron { expr } => match expr.next_after(UnitTime::new(cursor.last_fire().unwrap_or(0))) {
                Some(next) if now.secs() >= next.secs() => FireDecision::Fire,
                _ => FireDecision::Skip,
            },
        }
    }
}

/// Typed trigger failure. Display never echoes the raw expression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerError {
    InvalidId,
    InvalidExpression,
}

impl TriggerError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidId => "trigger id is invalid",
            Self::InvalidExpression => "trigger/cron expression is invalid",
        }
    }
}

impl fmt::Display for TriggerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for TriggerError {}

use std::collections::BTreeSet;

fn parse_field(raw: &str, range: (u64, u64)) -> Result<Field, TriggerError> {
    let mut allowed = BTreeSet::new();
    for token in raw.split(',') {
        if token == "*" {
            for v in range.0..=range.1 {
                allowed.insert(v);
            }
            continue;
        }
        if let Some((lo, hi)) = token.split_once('-') {
            let lo: u64 = lo.parse().map_err(|_| TriggerError::InvalidExpression)?;
            let hi: u64 = hi.parse().map_err(|_| TriggerError::InvalidExpression)?;
            if lo > hi || hi > range.1 {
                return Err(TriggerError::InvalidExpression);
            }
            for v in lo.max(range.0)..=hi {
                allowed.insert(v);
            }
            continue;
        }
        if let Some(step_raw) = token.strip_prefix("*/") {
            let step: u64 = step_raw.parse().map_err(|_| TriggerError::InvalidExpression)?;
            if step == 0 {
                return Err(TriggerError::InvalidExpression);
            }
            let mut v = range.0;
            while v <= range.1 {
                allowed.insert(v);
                v += step;
            }
            continue;
        }
        let v: u64 = token.parse().map_err(|_| TriggerError::InvalidExpression)?;
        if v < range.0 || v > range.1 {
            return Err(TriggerError::InvalidExpression);
        }
        allowed.insert(v);
    }
    if allowed.is_empty() {
        return Err(TriggerError::InvalidExpression);
    }
    Ok(Field(allowed))
}

fn valid_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_next_after_is_deterministic_and_bounded() {
        let expr = CronExpr::parse("*/15 * * * *").expect("cron"); // every 15 min
        let t0 = UnitTime::new(0);
        let next = expr.next_after(t0).expect("next");
        assert_eq!(next.secs() % 900, 0);
        let next2 = expr.next_after(next).expect("next2");
        assert_eq!(next2.secs(), next.secs() + 900);
    }

    #[test]
    fn cron_invalid_expression_fails_closed() {
        assert!(CronExpr::parse("61 * * * *").is_err());
        assert!(CronExpr::parse("* * * *").is_err());
        assert!(CronExpr::parse("a b c d e").is_err());
        assert!(CronExpr::parse("* * * * * *").is_err());
    }

    #[test]
    fn deterministic_jitter_is_stable_and_bounded() {
        let a = deterministic_jitter(7, 1000);
        let b = deterministic_jitter(7, 1000);
        assert_eq!(a, b, "jitter is deterministic for a seed");
        assert!(a < 1000);
        // Different seeds may differ (not asserted equal).
        assert_ne!(deterministic_jitter(8, 1000), a);
        assert_eq!(deterministic_jitter(7, 0), 0, "zero bucket yields zero jitter");
    }

    #[test]
    fn lease_generation_heartbeat_and_renew() {
        let mut lease = JobLease::new(1, "worker-a", UnitTime::new(100), 30);
        assert_eq!(lease.generation(), 1);
        // Fresh lease: recent heartbeat (issue time) and not expired.
        assert!(lease.is_live(UnitTime::new(110), 30));
        // Expired (expires at 130) → not live.
        assert!(!lease.is_live(UnitTime::new(135), 30), "expired");
        // Stale heartbeat while not expired (heartbeat at 100, now 125, grace 5).
        assert!(!lease.is_live(UnitTime::new(125), 5), "stale heartbeat");
        // Renew extends expiry + refreshes heartbeat.
        lease.renew(UnitTime::new(130), 30);
        assert!(lease.is_live(UnitTime::new(155), 30), "renewed and recent");
        // A distinct owner has its own, independent lease/generation.
        let other = JobLease::new(2, "worker-b", UnitTime::new(200), 30);
        assert_eq!(other.generation(), 2);
    }

    #[test]
    fn one_shot_missed_backfill_then_skip() {
        let spec = TriggerSpec::new(
            "t1",
            TriggerKind::OneShot {
                at: UnitTime::new(500),
            },
        )
        .expect("spec");
        let mut cursor = TriggerCursor::new();
        // Time passed the one-shot with no fire → missed backfill.
        assert_eq!(
            spec.kind().fires_at(UnitTime::new(600), &cursor),
            FireDecision::MissedBackfill
        );
        cursor.advance(FireDecision::MissedBackfill, 600);
        assert!(cursor.missed());
        // After firing, it skips forever.
        assert_eq!(
            spec.kind().fires_at(UnitTime::new(700), &cursor),
            FireDecision::Skip
        );
    }

    #[test]
    fn interval_fires_on_elapsed_and_skips_otherwise() {
        let spec = TriggerSpec::new("t2", TriggerKind::Interval { seconds: 60 }).expect("spec");
        let mut cursor = TriggerCursor::new();
        assert_eq!(spec.kind().fires_at(UnitTime::new(0), &cursor), FireDecision::Fire);
        cursor.advance(FireDecision::Fire, 0);
        assert_eq!(spec.kind().fires_at(UnitTime::new(30), &cursor), FireDecision::Skip);
        assert_eq!(spec.kind().fires_at(UnitTime::new(61), &cursor), FireDecision::Fire);
    }
}
