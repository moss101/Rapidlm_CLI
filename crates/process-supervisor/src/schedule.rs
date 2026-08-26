//! Bounded five-field daemon schedule parser and next-fire calculator.
//!
//! This module parses expressions and computes the next fire instant. It never
//! spawns a process or mutates the job registry. Sub-minute cadence is rejected.

use std::error::Error;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use capability_broker::CancellationToken;
use protocol::ErrorCode;
use serde::{Deserialize, Serialize};

/// Wire schema for [`ScheduleSpec`].
pub const SCHEDULE_SCHEMA: u16 = 1;

/// Smallest allowed gap between consecutive fires.
pub const MIN_SCHEDULE_INTERVAL: Duration = Duration::from_secs(60);

/// Maximum UTF-8 bytes accepted by [`Schedule::parse`].
pub const MAX_SCHEDULE_BYTES: usize = 128;

/// Search window for [`Schedule::next_fire_after`].
pub const MAX_SEARCH_MINUTES: u32 = 366 * 24 * 60 * 5;

const CANCEL_STRIDE: u32 = 64;
const MIN_UNIX_SECS: i64 = 0;
const MAX_UNIX_SECS: i64 = 4_102_444_800; // 2100-01-01T00:00:00Z

/// Documented five-field subset plus timezone.
///
/// ```text
/// minute hour day-of-month month day-of-week [timezone]
/// ```
///
/// Field atoms: `*`, `n`, `n-m`, `a,b`, `*/k`, `n-m/k`, `n/k`. `k` must be ≥ 1.
/// Day-of-week `0` and `7` are Sunday. Month and weekday names are not accepted.
/// A sixth numeric/`*` field is treated as seconds and rejected as sub-minute.
#[derive(Clone, Eq, PartialEq)]
pub struct Schedule {
    minute: FieldBits,
    hour: FieldBits,
    dom: FieldBits,
    month: FieldBits,
    dow: FieldBits,
    tz: TimeZone,
}

/// Persistable daemon schedule. Catch-up is explicit; default is skip.
#[derive(Clone, Eq, PartialEq)]
pub struct ScheduleSpec {
    schedule: Schedule,
    catch_up: CatchUpPolicy,
}

/// Missed-fire policy. Only skip is implemented; burst catch-up is not implicit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatchUpPolicy {
    Skip,
}

/// Documented IANA subset used without a tzdb dependency.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TimeZone {
    Utc,
    AmericaNewYork,
    AmericaLosAngeles,
    EuropeLondon,
}

/// Cooperative time source so DST tests do not depend on wall time.
pub trait Clock {
    fn now(&self) -> SystemTime;
}

/// Host clock.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SystemClock;

/// Deterministic clock for DST/timezone tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FakeClock {
    now_unix_ms: u64,
}

/// Typed parse / next-fire failure. Display never echoes the input expression.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ScheduleError {
    Cancelled,
    Empty,
    TooLong,
    InvalidField,
    InvalidTimezone,
    SubMinute,
    CadenceTooFrequent,
    Clock,
    HorizonExceeded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FieldBits {
    bits: u64,
    lo: u8,
    hi: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Civil {
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
}

impl Schedule {
    /// Parse the documented five-field subset. Default timezone is UTC.
    pub fn parse(input: &str) -> Result<Self, ScheduleError> {
        if input.is_empty() {
            return Err(ScheduleError::Empty);
        }
        if input.len() > MAX_SCHEDULE_BYTES {
            return Err(ScheduleError::TooLong);
        }
        if input.bytes().any(|b| b == 0 || b.is_ascii_control()) {
            return Err(ScheduleError::InvalidField);
        }

        let tokens: Vec<&str> = input.split_ascii_whitespace().collect();
        match tokens.as_slice() {
            [minute, hour, dom, month, dow] => {
                Self::from_fields(minute, hour, dom, month, dow, TimeZone::Utc)
            }
            [minute, hour, dom, month, dow, tz] => {
                let zone = match TimeZone::parse(tz) {
                    Ok(zone) => zone,
                    Err(ScheduleError::InvalidTimezone) if looks_like_seconds_field(tz) => {
                        return Err(ScheduleError::SubMinute);
                    }
                    Err(err) => return Err(err),
                };
                Self::from_fields(minute, hour, dom, month, dow, zone)
            }
            [] => Err(ScheduleError::Empty),
            _ if tokens.len() >= 6 && looks_like_seconds_field(tokens[0]) => {
                Err(ScheduleError::SubMinute)
            }
            _ => Err(ScheduleError::InvalidField),
        }
    }

    /// Parse five fields and force a timezone. Extra timezone tokens are rejected.
    pub fn parse_in_timezone(input: &str, tz: TimeZone) -> Result<Self, ScheduleError> {
        let parsed = Self::parse(input)?;
        if input.split_ascii_whitespace().count() == 6 {
            return Err(ScheduleError::InvalidField);
        }
        Ok(Self { tz, ..parsed })
    }

    pub fn timezone(&self) -> TimeZone {
        self.tz
    }

    /// Tightest consecutive-fire gap. Five-field expressions are at least one minute.
    pub fn min_interval(&self) -> Duration {
        MIN_SCHEDULE_INTERVAL
    }

    /// Next fire strictly after `clock.now()`.
    pub fn next_fire(
        &self,
        clock: &impl Clock,
        cancel: &CancellationToken,
    ) -> Result<SystemTime, ScheduleError> {
        self.next_fire_after(clock.now(), cancel)
    }

    /// Next matching instant strictly after `after`. Missed fires are skipped.
    pub fn next_fire_after(
        &self,
        after: SystemTime,
        cancel: &CancellationToken,
    ) -> Result<SystemTime, ScheduleError> {
        let after_secs = system_time_unix_secs(after)?;
        let local = self.tz.utc_to_local(after_secs)?;
        let mut civil = increment_minute(local)?;
        let mut scanned = 0u32;
        let mut previous: Option<i64> = None;

        while scanned < MAX_SEARCH_MINUTES {
            if scanned.is_multiple_of(CANCEL_STRIDE) && cancel.is_cancelled() {
                return Err(ScheduleError::Cancelled);
            }
            if self.matches(civil)
                && let Some(utc) = self.tz.local_to_utc(civil)?
            {
                if utc > after_secs {
                    if let Some(prev) = previous
                        && utc.saturating_sub(prev) < MIN_SCHEDULE_INTERVAL.as_secs() as i64
                    {
                        return Err(ScheduleError::CadenceTooFrequent);
                    }
                    return unix_secs_to_system_time(utc);
                }
                previous = Some(utc);
            }
            civil = increment_minute(civil)?;
            scanned = scanned.saturating_add(1);
        }
        Err(ScheduleError::HorizonExceeded)
    }

    fn from_fields(
        minute: &str,
        hour: &str,
        dom: &str,
        month: &str,
        dow: &str,
        tz: TimeZone,
    ) -> Result<Self, ScheduleError> {
        let minute = parse_field(minute, 0, 59, false)?;
        let hour = parse_field(hour, 0, 23, false)?;
        let dom = parse_field(dom, 1, 31, false)?;
        let month = parse_field(month, 1, 12, false)?;
        let dow = parse_field(dow, 0, 7, true)?;
        let schedule = Self {
            minute,
            hour,
            dom,
            month,
            dow,
            tz,
        };
        if schedule.min_interval() < MIN_SCHEDULE_INTERVAL {
            return Err(ScheduleError::CadenceTooFrequent);
        }
        Ok(schedule)
    }

    fn matches(&self, civil: Civil) -> bool {
        if !self.month.contains(civil.month) {
            return false;
        }
        if !self.hour.contains(civil.hour) || !self.minute.contains(civil.minute) {
            return false;
        }
        let weekday = weekday_of(civil.year, civil.month, civil.day);
        let dom_ok = self.dom.contains(civil.day);
        let dow_ok = self.dow.contains(weekday);
        if !self.dom.is_star() && !self.dow.is_star() {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        }
    }
}

impl ScheduleSpec {
    pub fn parse(input: &str) -> Result<Self, ScheduleError> {
        Ok(Self {
            schedule: Schedule::parse(input)?,
            catch_up: CatchUpPolicy::Skip,
        })
    }

    pub fn new(schedule: Schedule, catch_up: CatchUpPolicy) -> Self {
        Self { schedule, catch_up }
    }

    pub fn schedule(&self) -> &Schedule {
        &self.schedule
    }

    pub fn catch_up(&self) -> CatchUpPolicy {
        self.catch_up
    }

    pub fn timezone(&self) -> TimeZone {
        self.schedule.timezone()
    }

    pub fn min_interval(&self) -> Duration {
        self.schedule.min_interval()
    }

    pub fn next_fire(
        &self,
        clock: &impl Clock,
        cancel: &CancellationToken,
    ) -> Result<SystemTime, ScheduleError> {
        self.schedule.next_fire(clock, cancel)
    }

    pub fn next_fire_after(
        &self,
        after: SystemTime,
        cancel: &CancellationToken,
    ) -> Result<SystemTime, ScheduleError> {
        self.schedule.next_fire_after(after, cancel)
    }
}

impl TimeZone {
    pub fn parse(name: &str) -> Result<Self, ScheduleError> {
        match name {
            "UTC" | "Etc/UTC" => Ok(Self::Utc),
            "America/New_York" => Ok(Self::AmericaNewYork),
            "America/Los_Angeles" => Ok(Self::AmericaLosAngeles),
            "Europe/London" => Ok(Self::EuropeLondon),
            _ => Err(ScheduleError::InvalidTimezone),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Utc => "UTC",
            Self::AmericaNewYork => "America/New_York",
            Self::AmericaLosAngeles => "America/Los_Angeles",
            Self::EuropeLondon => "Europe/London",
        }
    }

    fn std_offset_secs(self) -> i32 {
        match self {
            Self::Utc | Self::EuropeLondon => 0,
            Self::AmericaNewYork => -5 * 3600,
            Self::AmericaLosAngeles => -8 * 3600,
        }
    }

    fn dst_offset_secs(self) -> i32 {
        match self {
            Self::Utc => 0,
            Self::EuropeLondon => 3600,
            Self::AmericaNewYork => -4 * 3600,
            Self::AmericaLosAngeles => -7 * 3600,
        }
    }

    fn dst_range_utc(self, year: i32) -> Result<Option<(i64, i64)>, ScheduleError> {
        match self {
            Self::Utc => Ok(None),
            Self::AmericaNewYork | Self::AmericaLosAngeles => {
                let start_day = nth_weekday(year, 3, 0, 2)?;
                let end_day = nth_weekday(year, 11, 0, 1)?;
                // 02:00 standard → UTC; 02:00 daylight → UTC.
                let start = civil_to_unix(Civil {
                    year,
                    month: 3,
                    day: start_day,
                    hour: 2,
                    minute: 0,
                })? - i64::from(self.std_offset_secs());
                let end = civil_to_unix(Civil {
                    year,
                    month: 11,
                    day: end_day,
                    hour: 2,
                    minute: 0,
                })? - i64::from(self.dst_offset_secs());
                Ok(Some((start, end)))
            }
            Self::EuropeLondon => {
                let start_day = last_weekday(year, 3, 0)?;
                let end_day = last_weekday(year, 10, 0)?;
                let start = civil_to_unix(Civil {
                    year,
                    month: 3,
                    day: start_day,
                    hour: 1,
                    minute: 0,
                })?;
                let end = civil_to_unix(Civil {
                    year,
                    month: 10,
                    day: end_day,
                    hour: 1,
                    minute: 0,
                })?;
                Ok(Some((start, end)))
            }
        }
    }

    fn offset_at(self, utc_secs: i64) -> Result<i32, ScheduleError> {
        let year = unix_to_civil(utc_secs)?.year;
        Ok(match self.dst_range_utc(year)? {
            Some((start, end)) if utc_secs >= start && utc_secs < end => self.dst_offset_secs(),
            _ => self.std_offset_secs(),
        })
    }

    fn utc_to_local(self, utc_secs: i64) -> Result<Civil, ScheduleError> {
        let local = utc_secs
            .checked_add(i64::from(self.offset_at(utc_secs)?))
            .ok_or(ScheduleError::Clock)?;
        unix_to_civil(local)
    }

    fn local_to_utc(self, civil: Civil) -> Result<Option<i64>, ScheduleError> {
        let as_utc = civil_to_unix(civil)?;
        let mut found: Option<i64> = None;
        for offset in [self.std_offset_secs(), self.dst_offset_secs()] {
            let candidate = as_utc
                .checked_sub(i64::from(offset))
                .ok_or(ScheduleError::Clock)?;
            if !(MIN_UNIX_SECS..MAX_UNIX_SECS).contains(&candidate) {
                continue;
            }
            if self.offset_at(candidate)? == offset {
                found = Some(match found {
                    Some(prev) if prev < candidate => prev,
                    _ => candidate,
                });
            }
        }
        Ok(found)
    }
}

impl FakeClock {
    pub const fn at_unix_ms(now_unix_ms: u64) -> Self {
        Self { now_unix_ms }
    }

    pub const fn at_unix_secs(secs: u64) -> Self {
        Self {
            now_unix_ms: secs.saturating_mul(1000),
        }
    }

    pub fn set_unix_ms(&mut self, now_unix_ms: u64) {
        self.now_unix_ms = now_unix_ms;
    }

    pub fn advance(&mut self, duration: Duration) {
        let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        self.now_unix_ms = self.now_unix_ms.saturating_add(ms);
    }

    pub fn unix_ms(self) -> u64 {
        self.now_unix_ms
    }
}

impl Clock for FakeClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.now_unix_ms)
    }
}

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

impl ScheduleError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "schedule calculation cancelled",
            Self::Empty => "schedule expression is empty",
            Self::TooLong => "schedule expression exceeds bound",
            Self::InvalidField => "schedule field is invalid",
            Self::InvalidTimezone => "schedule timezone is not in the documented set",
            Self::SubMinute => "schedule cadence below one minute is rejected",
            Self::CadenceTooFrequent => "schedule fires more often than the configured minimum",
            Self::Clock => "schedule clock is invalid",
            Self::HorizonExceeded => "no fire within the search horizon",
        }
    }

    pub const fn error_code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::Clock => Some(ErrorCode::InternalUnexpected),
            Self::Empty
            | Self::TooLong
            | Self::InvalidField
            | Self::InvalidTimezone
            | Self::SubMinute
            | Self::CadenceTooFrequent
            | Self::HorizonExceeded => Some(ErrorCode::ToolInvalidArguments),
        }
    }
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ScheduleError {}

impl fmt::Debug for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Schedule")
            .field("minute_count", &self.minute.count())
            .field("hour_count", &self.hour.count())
            .field("dom_count", &self.dom.count())
            .field("month_count", &self.month.count())
            .field("dow_count", &self.dow.count())
            .field("timezone", &self.tz.as_str())
            .finish()
    }
}

impl fmt::Debug for ScheduleSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScheduleSpec")
            .field("schedule", &self.schedule)
            .field("catch_up", &self.catch_up)
            .finish()
    }
}

impl Serialize for TimeZone {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TimeZone {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        TimeZone::parse(&name).map_err(serde::de::Error::custom)
    }
}

impl Serialize for ScheduleSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ScheduleSpec", 4)?;
        state.serialize_field("schema", &SCHEDULE_SCHEMA)?;
        state.serialize_field("expr", &spec_expr(&self.schedule))?;
        state.serialize_field("timezone", &self.schedule.tz.as_str())?;
        state.serialize_field("catch_up", &self.catch_up)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ScheduleSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            schema: u16,
            expr: String,
            timezone: String,
            catch_up: CatchUpPolicy,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.schema != SCHEDULE_SCHEMA {
            return Err(serde::de::Error::custom("unsupported schedule schema"));
        }
        if wire.catch_up != CatchUpPolicy::Skip {
            return Err(serde::de::Error::custom("unsupported catch-up policy"));
        }
        let tz = TimeZone::parse(&wire.timezone).map_err(serde::de::Error::custom)?;
        let schedule =
            Schedule::parse_in_timezone(&wire.expr, tz).map_err(serde::de::Error::custom)?;
        Ok(ScheduleSpec {
            schedule,
            catch_up: wire.catch_up,
        })
    }
}

impl FieldBits {
    fn empty(lo: u8, hi: u8) -> Self {
        Self { bits: 0, lo, hi }
    }

    fn contains(self, value: u8) -> bool {
        value >= self.lo && value <= self.hi && (self.bits & (1u64 << value)) != 0
    }

    fn insert(&mut self, value: u8) -> Result<(), ScheduleError> {
        if value < self.lo || value > self.hi {
            return Err(ScheduleError::InvalidField);
        }
        self.bits |= 1u64 << value;
        Ok(())
    }

    fn fill_step(&mut self, start: u8, end: u8, step: u8) -> Result<(), ScheduleError> {
        if step == 0 {
            return Err(ScheduleError::SubMinute);
        }
        if start < self.lo || end > self.hi || start > end {
            return Err(ScheduleError::InvalidField);
        }
        let mut value = start;
        loop {
            self.insert(value)?;
            let next = value.checked_add(step).ok_or(ScheduleError::InvalidField)?;
            if next > end {
                break;
            }
            value = next;
        }
        Ok(())
    }

    fn is_star(self) -> bool {
        self.count() == u32::from(self.hi - self.lo + 1)
    }

    fn count(self) -> u32 {
        self.bits.count_ones()
    }
}

fn parse_field(raw: &str, lo: u8, hi: u8, sunday_seven: bool) -> Result<FieldBits, ScheduleError> {
    if raw.is_empty() {
        return Err(ScheduleError::InvalidField);
    }
    let mut bits = FieldBits::empty(lo, if sunday_seven { 6 } else { hi });
    let mut atoms = 0u8;
    for atom in raw.split(',') {
        atoms = atoms.checked_add(1).ok_or(ScheduleError::InvalidField)?;
        if atoms > 32 {
            return Err(ScheduleError::InvalidField);
        }
        parse_atom(&mut bits, atom, lo, hi, sunday_seven)?;
    }
    if bits.bits == 0 {
        return Err(ScheduleError::InvalidField);
    }
    Ok(bits)
}

fn parse_atom(
    bits: &mut FieldBits,
    atom: &str,
    lo: u8,
    hi: u8,
    sunday_seven: bool,
) -> Result<(), ScheduleError> {
    if atom.is_empty() {
        return Err(ScheduleError::InvalidField);
    }
    let (range, step) = match atom.split_once('/') {
        Some((range, step)) => (range, Some(parse_step(step)?)),
        None => (atom, None),
    };
    let (start, end) = if range == "*" {
        (lo, hi)
    } else if let Some((a, b)) = range.split_once('-') {
        (
            parse_number(a, lo, hi, sunday_seven)?,
            parse_number(b, lo, hi, sunday_seven)?,
        )
    } else if step.is_some() {
        (parse_number(range, lo, hi, sunday_seven)?, hi)
    } else {
        let value = parse_number(range, lo, hi, sunday_seven)?;
        (value, value)
    };
    let step = step.unwrap_or(1);
    if sunday_seven {
        if start > end {
            return Err(ScheduleError::InvalidField);
        }
        if step == 0 {
            return Err(ScheduleError::SubMinute);
        }
        let mut value = start;
        loop {
            bits.insert(normalize_dow(value, true))?;
            let next = value.checked_add(step).ok_or(ScheduleError::InvalidField)?;
            if next > end {
                break;
            }
            value = next;
        }
        return Ok(());
    }
    bits.fill_step(start, end, step)
}

fn parse_step(raw: &str) -> Result<u8, ScheduleError> {
    let step = parse_u8(raw)?;
    if step == 0 {
        return Err(ScheduleError::SubMinute);
    }
    Ok(step)
}

fn parse_number(raw: &str, lo: u8, hi: u8, sunday_seven: bool) -> Result<u8, ScheduleError> {
    let value = parse_u8(raw)?;
    if sunday_seven && value == 7 {
        return Ok(7);
    }
    if value < lo || value > hi {
        return Err(ScheduleError::InvalidField);
    }
    Ok(value)
}

fn parse_u8(raw: &str) -> Result<u8, ScheduleError> {
    if raw.is_empty() || raw.len() > 3 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ScheduleError::InvalidField);
    }
    raw.parse().map_err(|_| ScheduleError::InvalidField)
}

fn normalize_dow(value: u8, sunday_seven: bool) -> u8 {
    if sunday_seven && value == 7 { 0 } else { value }
}

fn looks_like_seconds_field(token: &str) -> bool {
    parse_field(token, 0, 59, false).is_ok()
}

fn spec_expr(schedule: &Schedule) -> String {
    format!(
        "{} {} {} {} {}",
        field_star_or_list(schedule.minute),
        field_star_or_list(schedule.hour),
        field_star_or_list(schedule.dom),
        field_star_or_list(schedule.month),
        field_star_or_list(schedule.dow),
    )
}

fn field_star_or_list(field: FieldBits) -> String {
    if field.is_star() {
        return "*".to_owned();
    }
    let mut parts = Vec::new();
    for value in field.lo..=field.hi {
        if field.contains(value) {
            parts.push(value.to_string());
        }
    }
    parts.join(",")
}

fn increment_minute(mut civil: Civil) -> Result<Civil, ScheduleError> {
    if civil.minute < 59 {
        civil.minute += 1;
        return Ok(civil);
    }
    civil.minute = 0;
    if civil.hour < 23 {
        civil.hour += 1;
        return Ok(civil);
    }
    civil.hour = 0;
    let dim = days_in_month(civil.year, civil.month)?;
    if civil.day < dim {
        civil.day += 1;
        return Ok(civil);
    }
    civil.day = 1;
    if civil.month < 12 {
        civil.month += 1;
        return Ok(civil);
    }
    civil.month = 1;
    civil.year = civil.year.checked_add(1).ok_or(ScheduleError::Clock)?;
    if civil.year > 2099 {
        return Err(ScheduleError::HorizonExceeded);
    }
    Ok(civil)
}

fn days_in_month(year: i32, month: u8) -> Result<u8, ScheduleError> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Ok(31),
        4 | 6 | 9 | 11 => Ok(30),
        2 => Ok(if is_leap(year) { 29 } else { 28 }),
        _ => Err(ScheduleError::Clock),
    }
}

fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn weekday_of(year: i32, month: u8, day: u8) -> u8 {
    let days = days_from_civil(year, month, day);
    (((days + 4) % 7 + 7) % 7) as u8
}

fn nth_weekday(year: i32, month: u8, weekday: u8, n: u8) -> Result<u8, ScheduleError> {
    if n == 0 {
        return Err(ScheduleError::Clock);
    }
    let first = weekday_of(year, month, 1);
    let day = 1 + (weekday + 7 - first) % 7 + (n - 1) * 7;
    let dim = days_in_month(year, month)?;
    if day == 0 || day > dim {
        return Err(ScheduleError::Clock);
    }
    Ok(day)
}

fn last_weekday(year: i32, month: u8, weekday: u8) -> Result<u8, ScheduleError> {
    let dim = days_in_month(year, month)?;
    let last = weekday_of(year, month, dim);
    let back = (last + 7 - weekday) % 7;
    dim.checked_sub(back).ok_or(ScheduleError::Clock)
}

fn civil_to_unix(civil: Civil) -> Result<i64, ScheduleError> {
    if civil.year < 1970 || civil.year > 2099 {
        return Err(ScheduleError::Clock);
    }
    if civil.month == 0 || civil.day == 0 || civil.hour > 23 || civil.minute > 59 {
        return Err(ScheduleError::Clock);
    }
    if civil.day > days_in_month(civil.year, civil.month)? {
        return Err(ScheduleError::Clock);
    }
    let days = days_from_civil(civil.year, civil.month, civil.day);
    days.checked_mul(86_400)
        .and_then(|d| d.checked_add(i64::from(civil.hour) * 3_600))
        .and_then(|d| d.checked_add(i64::from(civil.minute) * 60))
        .ok_or(ScheduleError::Clock)
}

fn unix_to_civil(secs: i64) -> Result<Civil, ScheduleError> {
    if !(MIN_UNIX_SECS..MAX_UNIX_SECS).contains(&secs) {
        return Err(ScheduleError::Clock);
    }
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    let hour = (rem / 3_600) as u8;
    let minute = ((rem % 3_600) / 60) as u8;
    let (year, month, day) = civil_from_days(days)?;
    Ok(Civil {
        year,
        month,
        day,
        hour,
        minute,
    })
}

fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    // yoe is in 0..400 for the Howard Hinnant era split.
    let yoe = (y - era * 400) as u32;
    let mp = if month > 2 {
        u32::from(month) - 3
    } else {
        u32::from(month) + 9
    };
    let doy = (153 * mp + 2) / 5 + u32::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era) * 146_097 + i64::from(doe) - 719_468
}

fn civil_from_days(days: i64) -> Result<(i32, u8, u8), ScheduleError> {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = u64::try_from(z - era * 146_097).map_err(|_| ScheduleError::Clock)?;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = i64::try_from(yoe).map_err(|_| ScheduleError::Clock)? + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let year = i32::try_from(y).map_err(|_| ScheduleError::Clock)?;
    let month = u8::try_from(m).map_err(|_| ScheduleError::Clock)?;
    let day = u8::try_from(d).map_err(|_| ScheduleError::Clock)?;
    Ok((year, month, day))
}

fn system_time_unix_secs(time: SystemTime) -> Result<i64, ScheduleError> {
    let dur = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ScheduleError::Clock)?;
    i64::try_from(dur.as_secs()).map_err(|_| ScheduleError::Clock)
}

fn unix_secs_to_system_time(secs: i64) -> Result<SystemTime, ScheduleError> {
    let secs = u64::try_from(secs).map_err(|_| ScheduleError::Clock)?;
    Ok(UNIX_EPOCH + Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-7c1e9b";

    // 2026-03-08 06:59:00Z / 07:00:00Z — US spring-forward in America/New_York.
    const NY_SPRING_BEFORE: u64 = 1_772_953_140;
    const NY_SPRING_AFTER: u64 = 1_772_953_200;
    // 2026-11-01 05:30:00Z first 01:30 EDT; 06:30:00Z second 01:30 EST.
    const NY_FALL_FIRST_0130: u64 = 1_793_511_000;
    const NY_FALL_SECOND_0130: u64 = 1_793_514_600;
    const NY_FALL_0200_EST: u64 = 1_793_516_400;
    // 2026-03-29 00:59:00Z / 01:00:00Z — Europe/London spring-forward.
    const LDN_SPRING_BEFORE: u64 = 1_774_745_940;
    const LDN_SPRING_AFTER: u64 = 1_774_746_000;

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn unix(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn fire_secs(schedule: &Schedule, after: u64) -> u64 {
        let next = schedule
            .next_fire_after(unix(after), &live())
            .expect("next fire");
        next.duration_since(UNIX_EPOCH).expect("epoch").as_secs()
    }

    #[test]
    fn parse_five_field_defaults_to_utc() {
        let schedule = Schedule::parse("0 12 * * *").expect("parse");
        assert_eq!(schedule.timezone(), TimeZone::Utc);
        assert_eq!(schedule.min_interval(), MIN_SCHEDULE_INTERVAL);
    }

    #[test]
    fn parse_accepts_documented_timezone() {
        let schedule = Schedule::parse("30 2 * * * America/New_York").expect("parse");
        assert_eq!(schedule.timezone(), TimeZone::AmericaNewYork);
        assert_eq!(
            Schedule::parse("0 0 * * 0 Europe/London")
                .expect("london")
                .timezone(),
            TimeZone::EuropeLondon
        );
        assert_eq!(
            Schedule::parse("0 * * * * America/Los_Angeles")
                .expect("la")
                .timezone(),
            TimeZone::AmericaLosAngeles
        );
    }

    #[test]
    fn parse_rejects_sub_minute_and_six_field_seconds() {
        assert_eq!(
            Schedule::parse("*/30 * * * * *"),
            Err(ScheduleError::SubMinute)
        );
        assert_eq!(
            Schedule::parse("0 0 0 * * *"),
            Err(ScheduleError::SubMinute)
        );
        assert_eq!(
            Schedule::parse("* * * * * *"),
            Err(ScheduleError::SubMinute)
        );
        assert_eq!(
            Schedule::parse("*/0 * * * *"),
            Err(ScheduleError::SubMinute)
        );
        assert_eq!(
            Schedule::parse("0-59/0 * * * *"),
            Err(ScheduleError::SubMinute)
        );
        assert_eq!(
            Schedule::parse("@every 30s"),
            Err(ScheduleError::InvalidField)
        );
        assert_eq!(
            Schedule::parse("0/0 * * * *"),
            Err(ScheduleError::SubMinute)
        );
    }

    #[test]
    fn parse_rejects_unknown_timezone_and_casefold_bypass() {
        assert_eq!(
            Schedule::parse("0 * * * * America/Chicago"),
            Err(ScheduleError::InvalidTimezone)
        );
        assert_eq!(
            Schedule::parse("0 * * * * america/new_york"),
            Err(ScheduleError::InvalidTimezone)
        );
        assert_eq!(
            Schedule::parse("0 * * * * utc"),
            Err(ScheduleError::InvalidTimezone)
        );
    }

    #[test]
    fn parse_rejects_empty_overlong_control_and_names() {
        assert_eq!(Schedule::parse(""), Err(ScheduleError::Empty));
        assert_eq!(
            Schedule::parse(&"0 ".repeat(80)),
            Err(ScheduleError::TooLong)
        );
        assert_eq!(
            Schedule::parse("0 12 * * *\nUTC"),
            Err(ScheduleError::InvalidField)
        );
        assert_eq!(
            Schedule::parse("0 12 * JAN *"),
            Err(ScheduleError::InvalidField)
        );
        assert_eq!(
            Schedule::parse("0 12 * * MON"),
            Err(ScheduleError::InvalidField)
        );
        assert_eq!(
            Schedule::parse("60 * * * *"),
            Err(ScheduleError::InvalidField)
        );
        assert_eq!(
            Schedule::parse("0 24 * * *"),
            Err(ScheduleError::InvalidField)
        );
    }

    #[test]
    fn parse_does_not_echo_canary_in_errors_or_debug() {
        let err = Schedule::parse(&format!("0 12 * * * {CANARY}")).expect_err("tz");
        let shown = format!("{err} {err:?} {}", err.as_str());
        assert!(!shown.contains(CANARY));
        assert_eq!(err.error_code(), Some(ErrorCode::ToolInvalidArguments));
        let schedule = Schedule::parse("0 12 * * *").expect("ok");
        assert!(!format!("{schedule:?}").contains(CANARY));
    }

    #[test]
    fn next_fire_every_minute_respects_minimum_interval() {
        let schedule = Schedule::parse("* * * * *").expect("parse");
        // 1_700_000_000 is 20s past a minute; next fire is the following :00.
        let first = fire_secs(&schedule, 1_700_000_000);
        let second = fire_secs(&schedule, first);
        let third = fire_secs(&schedule, second);
        assert_eq!(first, 1_700_000_040);
        assert_eq!(second - first, 60);
        assert_eq!(third - second, 60);
        assert!(second - first >= MIN_SCHEDULE_INTERVAL.as_secs());
    }

    #[test]
    fn next_fire_hourly_utc_is_deterministic() {
        let schedule = Schedule::parse("15 * * * *").expect("parse");
        // 2025-01-01 00:00:00Z
        assert_eq!(fire_secs(&schedule, 1_735_689_600), 1_735_689_600 + 15 * 60);
        assert_eq!(
            fire_secs(&schedule, 1_735_689_600 + 15 * 60),
            1_735_689_600 + 3600 + 15 * 60
        );
    }

    #[test]
    fn next_fire_uses_fake_clock() {
        let schedule = Schedule::parse("0 0 * * *").expect("parse");
        let clock = FakeClock::at_unix_secs(1_735_689_601);
        let next = schedule.next_fire(&clock, &live()).expect("next");
        assert_eq!(
            next.duration_since(UNIX_EPOCH).expect("epoch").as_secs(),
            1_735_689_600 + 86_400
        );
    }

    #[test]
    fn dst_spring_forward_skips_missing_new_york_hour() {
        let every = Schedule::parse("* * * * * America/New_York").expect("every");
        let clock = FakeClock::at_unix_secs(NY_SPRING_BEFORE);
        let next = every.next_fire(&clock, &live()).expect("next");
        assert_eq!(
            next.duration_since(UNIX_EPOCH).expect("epoch").as_secs(),
            NY_SPRING_AFTER
        );

        let two_thirty = Schedule::parse("30 2 * * * America/New_York").expect("2:30");
        // 02:30 does not exist on 2026-03-08; next is 2026-03-09 02:30 EDT = 06:30Z.
        let after_gap = fire_secs(&two_thirty, NY_SPRING_BEFORE);
        assert_eq!(after_gap, 1_773_037_800);
        assert!(after_gap - NY_SPRING_BEFORE >= MIN_SCHEDULE_INTERVAL.as_secs());
    }

    #[test]
    fn dst_fall_back_fires_once_in_repeated_new_york_hour() {
        let one_thirty = Schedule::parse("30 1 * * * America/New_York").expect("1:30");
        let first = fire_secs(&one_thirty, NY_FALL_FIRST_0130 - 60);
        assert_eq!(first, NY_FALL_FIRST_0130);
        let second = fire_secs(&one_thirty, first);
        assert_ne!(second, NY_FALL_SECOND_0130);
        assert!(second >= NY_FALL_0200_EST);
        assert!(second - first >= MIN_SCHEDULE_INTERVAL.as_secs());

        let during_repeat = fire_secs(&one_thirty, NY_FALL_SECOND_0130 - 30);
        assert_ne!(during_repeat, NY_FALL_SECOND_0130);
        assert!(during_repeat > NY_FALL_SECOND_0130);
    }

    #[test]
    fn dst_london_spring_forward_is_deterministic() {
        let every = Schedule::parse("* * * * * Europe/London").expect("every");
        let clock = FakeClock::at_unix_secs(LDN_SPRING_BEFORE);
        let next = every.next_fire(&clock, &live()).expect("next");
        assert_eq!(
            next.duration_since(UNIX_EPOCH).expect("epoch").as_secs(),
            LDN_SPRING_AFTER
        );
    }

    #[test]
    fn sunday_seven_and_dom_or_dow() {
        let sunday = Schedule::parse("0 0 * * 7").expect("sun7");
        let week = Schedule::parse("0 0 * * 1-7").expect("all dow");
        assert_eq!(week.min_interval(), MIN_SCHEDULE_INTERVAL);
        // 2025-01-05 is Sunday 00:00Z.
        assert_eq!(
            fire_secs(&sunday, 1_735_689_600),
            1_735_689_600 + 4 * 86_400
        );
        assert_eq!(fire_secs(&week, 1_735_689_600), 1_735_689_600 + 86_400);

        let either = Schedule::parse("0 0 1 * 1").expect("dom or monday");
        // 2025-01-01 is Wednesday; next is 2025-01-01 00:00 if after is before,
        // then Monday 2025-01-06 because both DOM=1 and DOW=Monday match.
        let first = fire_secs(&either, 1_735_689_600 - 1);
        assert_eq!(first, 1_735_689_600);
        let second = fire_secs(&either, first);
        assert_eq!(second, 1_735_689_600 + 5 * 86_400);
    }

    #[test]
    fn lists_ranges_and_steps() {
        let schedule = Schedule::parse("0,30 9-17/2 * 1-3 1-5").expect("parse");
        assert_eq!(schedule.min_interval(), MIN_SCHEDULE_INTERVAL);
        let first = fire_secs(&schedule, 1_735_689_600);
        let second = fire_secs(&schedule, first);
        assert!(second - first >= 30 * 60);
    }

    #[test]
    fn impossible_date_exhausts_horizon() {
        let schedule = Schedule::parse("0 0 31 2 *").expect("feb 31");
        let err = schedule
            .next_fire_after(unix(1_735_689_600), &live())
            .expect_err("never");
        assert_eq!(err, ScheduleError::HorizonExceeded);
    }

    #[test]
    fn cancellation_fails_closed() {
        let schedule = Schedule::parse("0 0 31 2 *").expect("feb 31");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            schedule.next_fire_after(unix(1_735_689_600), &cancel),
            Err(ScheduleError::Cancelled)
        );
    }

    #[test]
    fn civil_conversion_matches_known_unix_instants() {
        assert_eq!(
            civil_to_unix(Civil {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
            })
            .expect("epoch"),
            0
        );
        assert_eq!(
            civil_to_unix(Civil {
                year: 2000,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
            })
            .expect("y2k"),
            946_684_800
        );
        assert_eq!(
            civil_to_unix(Civil {
                year: 2026,
                month: 3,
                day: 8,
                hour: 7,
                minute: 0,
            })
            .expect("spring"),
            i64::try_from(NY_SPRING_AFTER).expect("fit")
        );
        let civil = unix_to_civil(i64::try_from(NY_SPRING_AFTER).expect("fit")).expect("back");
        assert_eq!(
            civil,
            Civil {
                year: 2026,
                month: 3,
                day: 8,
                hour: 7,
                minute: 0,
            }
        );
    }

    #[test]
    fn schedule_spec_defaults_to_skip_and_does_not_execute() {
        let spec = ScheduleSpec::parse("0 * * * * UTC").expect("spec");
        assert_eq!(spec.catch_up(), CatchUpPolicy::Skip);
        assert_eq!(spec.min_interval(), MIN_SCHEDULE_INTERVAL);
        let clock = FakeClock::at_unix_secs(1_735_689_601);
        let next = spec.next_fire(&clock, &live()).expect("next");
        assert!(next.duration_since(clock.now()).expect("delta") >= MIN_SCHEDULE_INTERVAL);
        let json = serde_json::to_string(&spec).expect("json");
        assert!(json.contains("\"catch_up\":\"skip\""));
        assert!(json.contains("\"timezone\":\"UTC\""));
        let decoded: ScheduleSpec = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded.catch_up(), CatchUpPolicy::Skip);
        assert_eq!(decoded.timezone(), TimeZone::Utc);
    }

    #[test]
    fn pre_epoch_clock_fails_closed() {
        let schedule = Schedule::parse("* * * * *").expect("parse");
        let before = UNIX_EPOCH.checked_sub(Duration::from_secs(1)).expect("sub");
        assert_eq!(
            schedule.next_fire_after(before, &live()),
            Err(ScheduleError::Clock)
        );
    }
}
