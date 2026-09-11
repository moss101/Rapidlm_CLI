//! P11-018/019: bounded trajectory collection with privacy classification.

/// Privacy class assigned to each collected event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivacyClass {
    Public,
    Internal,
    Secret,
}

/// Maximum collected events per trajectory.
pub const MAX_TRAJECTORY_EVENTS: usize = 512;
/// Maximum bytes retained per collected text field.
pub const MAX_TEXT_BYTES: usize = 2 * 1024;

/// Classify an event kind into its privacy class. Secret-classified content
/// is dropped, never stored — even redacted.
pub fn classify(kind: &str) -> PrivacyClass {
    match kind {
        "secret.used" | "credential.accessed" => PrivacyClass::Secret,
        "model.stream_delta" | "model.completed" | "tool.requested" | "tool.completed"
        | "turn.started" | "turn.completed" | "goal.created" | "goal.completed" => {
            PrivacyClass::Internal
        }
        _ => PrivacyClass::Internal,
    }
}

/// One bounded collected observation.
#[derive(Clone, Debug, PartialEq)]
pub struct TrajectoryEvent {
    pub seq: u64,
    pub kind: String,
    pub privacy: PrivacyClass,
    pub text_excerpt: String,
}

/// Bounded collector: drops secrets outright, truncates text, caps length.
#[derive(Default)]
pub struct TrajectoryCollector {
    events: Vec<TrajectoryEvent>,
    dropped_secrets: u64,
}

impl TrajectoryCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one observable event. Returns false when the record was
    /// rejected (secret content or capacity).
    pub fn record(&mut self, seq: u64, kind: &str, text: &str) -> bool {
        let privacy = classify(kind);
        if privacy == PrivacyClass::Secret {
            self.dropped_secrets += 1;
            return false;
        }
        if self.events.len() >= MAX_TRAJECTORY_EVENTS {
            return false;
        }
        let mut excerpt = text.to_owned();
        if excerpt.len() > MAX_TEXT_BYTES {
            truncate_to_char_boundary(&mut excerpt, MAX_TEXT_BYTES);
        }
        self.events.push(TrajectoryEvent {
            seq,
            kind: kind.to_owned(),
            privacy,
            text_excerpt: excerpt,
        });
        true
    }

    pub fn events(&self) -> &[TrajectoryEvent] {
        &self.events
    }

    pub fn dropped_secrets(&self) -> u64 {
        self.dropped_secrets
    }
}

/// Truncates `s` to at most `max` bytes without panicking on a multi-byte
/// character straddling the cut. `String::truncate` panics unless `max` is a
/// char boundary; a byte-length check alone (`s.len() > max`) does not make
/// a raw `truncate(max)` call safe for arbitrary UTF-8 — `text` is caller-
/// observed content, not a fixed literal.
fn truncate_to_char_boundary(s: &mut String, max: usize) {
    let mut cut = max.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_dropped_never_stored_and_counter_increments() {
        let mut c = TrajectoryCollector::new();
        assert!(
            !c.record(1, "secret.used", "token=abc"),
            "secrets never stored"
        );
        assert!(c.record(2, "tool.completed", "wrote file"));
        assert_eq!(c.events().len(), 1);
        assert_eq!(c.dropped_secrets(), 1);
        assert_eq!(classify("credential.accessed"), PrivacyClass::Secret);
    }

    #[test]
    fn collection_is_bounded_and_text_truncated() {
        let mut c = TrajectoryCollector::new();
        for i in 0..MAX_TRAJECTORY_EVENTS + 5 {
            let ok = c.record(i as u64, "model.completed", &"x".repeat(50));
            if i >= MAX_TRAJECTORY_EVENTS {
                assert!(!ok, "capacity enforced at {i}");
                break;
            }
        }
        assert_eq!(c.events().len(), MAX_TRAJECTORY_EVENTS);
        let long = "y".repeat(MAX_TEXT_BYTES * 3);
        let mut c2 = TrajectoryCollector::new();
        c2.record(0, "model.completed", &long);
        assert_eq!(c2.events()[0].text_excerpt.len(), MAX_TEXT_BYTES);
    }

    #[test]
    fn record_does_not_panic_when_a_multibyte_char_straddles_the_cap() {
        // MAX_TEXT_BYTES - 1 ASCII bytes then one 4-byte char lands that
        // char across the cut, since it starts one byte before the cap.
        let text = format!("{}{}", "a".repeat(MAX_TEXT_BYTES - 1), '\u{1D518}');
        assert!(!text.is_char_boundary(MAX_TEXT_BYTES));
        let mut c = TrajectoryCollector::new();
        assert!(c.record(0, "model.completed", &text));
        assert!(c.events()[0].text_excerpt.len() <= MAX_TEXT_BYTES);
    }
}
