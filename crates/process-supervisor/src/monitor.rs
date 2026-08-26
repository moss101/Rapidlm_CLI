//! Monitor spec + pure event predicates (P7-009..013).
//!
//! A [`MonitorSpec`] describes a condition a runtime watches for (exit code,
//! log regex, port readiness, file event). [`MonitorSpec::observe`] is a pure,
//! deterministic predicate over a [`MonitorObservation`] — no process, file, or
//! network I/O here, so it is unit-testable in isolation. The I/O side (watching
//! a log stream, polling a port, inotify) lives in the runtime that emits the
//! observations.

use std::error::Error;
use std::fmt;

/// Maximum UTF-8 bytes accepted in one regex pattern.
pub const MAX_PATTERN_BYTES: usize = 4 * 1024;
/// Maximum UTF-8 bytes accepted in one file path or log-line observation.
pub const MAX_OBS_BYTES: usize = 16 * 1024;
/// Maximum monitor specs retained in one registry view.
pub const MAX_MONITORS: usize = 256;

/// File-event kind a monitor can match on.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum FileEventKind {
    Created,
    Modified,
    Removed,
}

/// What a monitor watches for.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum MonitorKind {
    /// Process exited with a specific code.
    ExitCode { code: i32 },
    /// A log/producer line matches a bounded regex.
    LogRegex { pattern: String },
    /// A network port became ready (accept/connect).
    PortReady { port: u16 },
    /// A filesystem path observed a file event.
    FileEvent { path: String, event: FileEventKind },
}

/// A bounded, durable monitor condition. `id` is a stable registry key.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct MonitorSpec {
    id: String,
    kind: MonitorKind,
}

impl MonitorSpec {
    pub fn new(id: impl Into<String>, kind: MonitorKind) -> Result<Self, MonitorError> {
        let id = id.into();
        if !valid_text(&id, MAX_OBS_BYTES) {
            return Err(MonitorError::InvalidId);
        }
        match &kind {
            MonitorKind::LogRegex { pattern } if !valid_regex(pattern) => {
                return Err(MonitorError::InvalidPattern)
            }
            MonitorKind::FileEvent { path, .. } if !valid_text(path, MAX_OBS_BYTES) => {
                return Err(MonitorError::InvalidPath)
            }
            _ => {}
        }
        Ok(Self { id, kind })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn kind(&self) -> &MonitorKind {
        &self.kind
    }

    /// Pure predicate: does `observation` satisfy this monitor?
    pub fn observe(&self, observation: &MonitorObservation) -> MonitorVerdict {
        match (&self.kind, observation) {
            (
                MonitorKind::ExitCode { code: want },
                MonitorObservation::ExitCode { code: got },
            ) => verdict(*got == *want),
            (
                MonitorKind::LogRegex { pattern },
                MonitorObservation::LogLine { text },
            ) => {
                if !valid_text(text, MAX_OBS_BYTES) {
                    return MonitorVerdict::not_matched(MonitorMissReason::BoundExceeded);
                }
                match regex_match(pattern, text) {
                    Some(true) => MonitorVerdict::matched(),
                    Some(false) => MonitorVerdict::not_matched(MonitorMissReason::NoMatch),
                    None => MonitorVerdict::not_matched(MonitorMissReason::InvalidPattern),
                }
            }
            (
                MonitorKind::PortReady { port: want },
                MonitorObservation::PortReady { port: got, ready },
            ) => verdict(*ready && *got == *want),
            (
                MonitorKind::FileEvent { path: want, event },
                MonitorObservation::FileEvent { path: got, event: got_event },
            ) => verdict(got == want && *event == *got_event),
            _ => MonitorVerdict::not_matched(MonitorMissReason::KindMismatch),
        }
    }
}

/// One observed runtime event fed to a monitor.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum MonitorObservation {
    ExitCode { code: i32 },
    LogLine { text: String },
    PortReady { port: u16, ready: bool },
    FileEvent { path: String, event: FileEventKind },
}

impl MonitorObservation {
    pub fn log_line(text: impl Into<String>) -> Result<Self, MonitorError> {
        let text = text.into();
        if !valid_text(&text, MAX_OBS_BYTES) {
            return Err(MonitorError::BoundExceeded);
        }
        Ok(Self::LogLine { text })
    }
    pub fn file(path: impl Into<String>, event: FileEventKind) -> Result<Self, MonitorError> {
        let path = path.into();
        if !valid_text(&path, MAX_OBS_BYTES) {
            return Err(MonitorError::BoundExceeded);
        }
        Ok(Self::FileEvent { path, event })
    }
}

/// Why a monitor did not match.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum MonitorMissReason {
    NoMatch,
    KindMismatch,
    InvalidPattern,
    BoundExceeded,
}

/// Result of one predicate evaluation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct MonitorVerdict {
    matched: bool,
    reason: Option<MonitorMissReason>,
}

impl MonitorVerdict {
    pub const fn matched() -> Self {
        Self {
            matched: true,
            reason: None,
        }
    }
    pub const fn not_matched(reason: MonitorMissReason) -> Self {
        Self {
            matched: false,
            reason: Some(reason),
        }
    }
    pub const fn matched_flag(&self) -> bool {
        self.matched
    }
    pub const fn reason(&self) -> Option<MonitorMissReason> {
        self.reason
    }
}

/// Typed monitor failure. Display never echoes pattern/path bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MonitorError {
    InvalidId,
    InvalidPattern,
    InvalidPath,
    BoundExceeded,
}

impl MonitorError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidId => "monitor id is invalid",
            Self::InvalidPattern => "monitor regex pattern is invalid",
            Self::InvalidPath => "monitor path is invalid",
            Self::BoundExceeded => "monitor observation exceeds a bound",
        }
    }
}

impl fmt::Display for MonitorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for MonitorError {}

fn verdict(matched: bool) -> MonitorVerdict {
    if matched {
        MonitorVerdict::matched()
    } else {
        MonitorVerdict::not_matched(MonitorMissReason::NoMatch)
    }
}

fn valid_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn valid_regex(pattern: &str) -> bool {
    valid_text(pattern, MAX_PATTERN_BYTES)
}

/// Deterministic bounded regex match. Returns `None` only for a control-char /
/// oversize input that bypassed construction validation (fail closed).
fn regex_match(pattern: &str, text: &str) -> Option<bool> {
    if !valid_regex(pattern) || !valid_text(text, MAX_OBS_BYTES) {
        return None;
    }
    // A tiny, deterministic substring matcher sufficient for the monitor seam;
    // it avoids a heavyweight regex dependency and is pure + bounded.
    Some(contains_substring(pattern, text))
}

fn contains_substring(needle: &str, haystack: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle = needle.as_bytes();
    let haystack = haystack.as_bytes();
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_monitor_matches_only_exact_code() {
        let spec = MonitorSpec::new("m1", MonitorKind::ExitCode { code: 0 }).expect("spec");
        assert!(spec.observe(&MonitorObservation::ExitCode { code: 0 }).matched_flag());
        assert!(!spec.observe(&MonitorObservation::ExitCode { code: 1 }).matched_flag());
    }

    #[test]
    fn log_regex_monitor_matches_substring_and_bounds() {
        let spec =
            MonitorSpec::new("m2", MonitorKind::LogRegex { pattern: "ready".to_owned() })
                .expect("spec");
        assert!(
            spec.observe(&MonitorObservation::log_line("server ready on :8080").expect("l"))
                .matched_flag()
        );
        assert!(
            !spec
                .observe(&MonitorObservation::log_line("server starting").expect("l"))
                .matched_flag()
        );
        // An oversize log line is rejected at construction (fail closed), so it
        // can never silently satisfy a monitor.
        let long = "x".repeat(MAX_OBS_BYTES + 1);
        assert!(matches!(
            MonitorObservation::log_line(long),
            Err(MonitorError::BoundExceeded)
        ));
    }

    #[test]
    fn port_readiness_requires_port_and_ready() {
        let spec = MonitorSpec::new("m3", MonitorKind::PortReady { port: 8443 }).expect("spec");
        assert!(
            spec.observe(&MonitorObservation::PortReady { port: 8443, ready: true })
                .matched_flag()
        );
        assert!(
            !spec
                .observe(&MonitorObservation::PortReady { port: 8443, ready: false })
                .matched_flag()
        );
        assert!(
            !spec
                .observe(&MonitorObservation::PortReady { port: 8080, ready: true })
                .matched_flag()
        );
    }

    #[test]
    fn file_event_monitor_matches_path_and_kind() {
        let spec = MonitorSpec::new(
            "m4",
            MonitorKind::FileEvent {
                path: "/tmp/out.json".to_owned(),
                event: FileEventKind::Created,
            },
        )
        .expect("spec");
        assert!(
            spec.observe(&MonitorObservation::file("/tmp/out.json", FileEventKind::Created).expect("f"))
                .matched_flag()
        );
        assert!(
            !spec
                .observe(&MonitorObservation::file("/tmp/out.json", FileEventKind::Modified).expect("f"))
                .matched_flag()
        );
        assert!(
            !spec
                .observe(&MonitorObservation::file("/other", FileEventKind::Created).expect("f"))
                .matched_flag()
        );
    }

    #[test]
    fn kind_mismatch_is_not_matched() {
        let spec = MonitorSpec::new("m5", MonitorKind::ExitCode { code: 0 }).expect("spec");
        let observation = MonitorObservation::log_line("ignored").expect("l");
        assert!(!spec.observe(&observation).matched_flag());
        assert_eq!(
            spec.observe(&observation).reason(),
            Some(MonitorMissReason::KindMismatch)
        );
    }

    #[test]
    fn invalid_pattern_and_bounds_fail_closed() {
        assert!(matches!(
            MonitorSpec::new("m6", MonitorKind::LogRegex { pattern: "a\nb".to_owned() }),
            Err(MonitorError::InvalidPattern)
        ));
        assert!(matches!(
            MonitorObservation::log_line("\u{0}b"),
            Err(MonitorError::BoundExceeded)
        ));
    }
}
