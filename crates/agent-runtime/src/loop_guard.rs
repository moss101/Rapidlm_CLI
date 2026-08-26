//! Agent turn loop-robustness guards (agent-harness §6).
//!
//! Detects model loops so a turn cannot spin forever repeating the same tool
//! call or the same output. Guards are pure, window-bounded, and deterministic;
//! they fail the turn (never silently continue) once a loop is detected.

use std::collections::HashMap;
use std::collections::vec_deque::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};

use protocol::ArtifactId;

/// Default window of recent tool calls retained for loop detection.
pub const DEFAULT_REPEATED_TOOL_CALL_WINDOW: usize = 4;

/// Default number of identical tool calls that trips the loop detector.
pub const DEFAULT_REPEATED_TOOL_CALL_THRESHOLD: usize = 3;

/// Default window of recent model messages retained for loop detection.
pub const DEFAULT_REPEATED_MESSAGE_WINDOW: usize = 4;

/// Default number of identical model messages that trips the loop detector.
pub const DEFAULT_REPEATED_MESSAGE_THRESHOLD: usize = 3;

/// A canonical signature of one tool call: tool name plus a hash of arguments.
///
/// The arguments are folded into the hash, never stored verbatim, so a loop
/// signature cannot leak a payload into logs/telemetry.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ToolCallSignature {
    tool: String,
    arguments_hash: u64,
}

impl ToolCallSignature {
    fn new(tool: &str, arguments: &str) -> Self {
        Self {
            tool: tool.to_owned(),
            arguments_hash: hash_str(arguments),
        }
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn arguments_hash(&self) -> u64 {
        self.arguments_hash
    }
}

/// Detects a model that repeatedly issues the same exact tool call.
#[derive(Clone, Debug)]
pub struct ToolCallLoopDetector {
    window: usize,
    threshold: usize,
    history: VecDeque<ToolCallSignature>,
}

impl ToolCallLoopDetector {
    pub fn new() -> Self {
        Self::with(
            DEFAULT_REPEATED_TOOL_CALL_WINDOW,
            DEFAULT_REPEATED_TOOL_CALL_THRESHOLD,
        )
    }

    /// Bounded detector. A zero window or threshold is rejected as a no-op
    /// configuration (a turn would loop forever), so callers must pass ≥ 1.
    pub fn with(window: usize, threshold: usize) -> Self {
        assert!(
            window >= 1 && threshold >= 1,
            "loop detector needs a positive window/threshold"
        );
        Self {
            window,
            threshold,
            history: VecDeque::with_capacity(window),
        }
    }

    /// Record one tool call. Bounded to the window; a call older than the
    /// window no longer contributes to a loop decision.
    pub fn observe(&mut self, tool: &str, arguments: &str) {
        self.history
            .push_back(ToolCallSignature::new(tool, arguments));
        while self.history.len() > self.window {
            self.history.pop_front();
        }
    }

    /// Whether the same exact call has been issued at least `threshold` times
    /// within the retained window.
    pub fn is_looping(&self) -> bool {
        let mut counts: HashMap<&ToolCallSignature, usize> = HashMap::new();
        for signature in &self.history {
            *counts.entry(signature).or_insert(0) += 1;
        }
        counts.values().any(|count| *count >= self.threshold)
    }

    pub fn len(&self) -> usize {
        self.history.len()
    }

    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    pub fn window(&self) -> usize {
        self.window
    }

    pub fn threshold(&self) -> usize {
        self.threshold
    }
}

impl Default for ToolCallLoopDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// A canonical signature of one model message (assistant output/stream body).
///
/// The body is content-hashed (SHA-256 via `protocol::ArtifactId`), never stored
/// verbatim, so a loop signature cannot leak a payload into logs/telemetry.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct MessageSignature {
    body_hash: ArtifactId,
}

impl MessageSignature {
    pub fn new(body: &str) -> Self {
        Self {
            body_hash: ArtifactId::from_bytes(body.as_bytes()),
        }
    }

    pub const fn body_hash(&self) -> ArtifactId {
        self.body_hash
    }
}

/// Detects a model that repeats the same assistant message in a bounded window.
#[derive(Clone, Debug)]
pub struct MessageLoopDetector {
    window: usize,
    threshold: usize,
    history: VecDeque<MessageSignature>,
}

impl MessageLoopDetector {
    pub fn new() -> Self {
        Self::with(
            DEFAULT_REPEATED_MESSAGE_WINDOW,
            DEFAULT_REPEATED_MESSAGE_THRESHOLD,
        )
    }

    pub fn with(window: usize, threshold: usize) -> Self {
        assert!(
            window >= 1 && threshold >= 1,
            "loop detector needs a positive window/threshold"
        );
        Self {
            window,
            threshold,
            history: VecDeque::with_capacity(window),
        }
    }

    pub fn observe(&mut self, body: &str) {
        self.observe_hash(ArtifactId::from_bytes(body.as_bytes()));
    }

    /// Observe a pre-computed canonical message hash. Whitespace/chunking are
    /// NOT normalized, so only verbatim-equal messages count as repeats; a
    /// legitimate one-off repetition does not trip the detector.
    pub fn observe_hash(&mut self, body_hash: ArtifactId) {
        self.history.push_back(MessageSignature { body_hash });
        while self.history.len() > self.window {
            self.history.pop_front();
        }
    }

    pub fn is_looping(&self) -> bool {
        let mut counts: HashMap<&MessageSignature, usize> = HashMap::new();
        for signature in &self.history {
            *counts.entry(signature).or_insert(0) += 1;
        }
        counts.values().any(|count| *count >= self.threshold)
    }

    pub fn len(&self) -> usize {
        self.history.len()
    }

    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    /// Reset at the agent-attempt / goal boundary.
    pub fn reset(&mut self) {
        self.history.clear();
    }
}

impl Default for MessageLoopDetector {
    fn default() -> Self {
        Self::new()
    }
}

fn hash_str(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_loop_is_detected_after_threshold() {
        let mut detector = ToolCallLoopDetector::new();
        // Two identical calls are not a loop yet.
        detector.observe("repo.search", "{\"query\":\"x\"}");
        detector.observe("repo.search", "{\"query\":\"x\"}");
        assert!(!detector.is_looping());
        // Third identical call trips the detector.
        detector.observe("repo.search", "{\"query\":\"x\"}");
        assert!(detector.is_looping());
        assert_eq!(detector.len(), 3);
    }

    #[test]
    fn different_calls_are_not_a_loop() {
        let mut detector = ToolCallLoopDetector::new();
        detector.observe("repo.search", "{\"query\":\"a\"}");
        detector.observe("repo.search", "{\"query\":\"b\"}");
        detector.observe("repo.read", "{\"path\":\"a.rs\"}");
        assert!(!detector.is_looping());
    }

    #[test]
    fn window_bounds_history_so_a_loop_can_eventually_clear() {
        let mut detector = ToolCallLoopDetector::with(3, 2);
        detector.observe("tool", "{\"a\":1}");
        detector.observe("tool", "{\"a\":1}");
        assert!(detector.is_looping());
        // Fill the window with distinct calls so the repetition ages out.
        detector.observe("b", "{}");
        detector.observe("c", "{}");
        assert_eq!(detector.len(), 3);
        assert!(!detector.is_looping());
    }

    #[test]
    fn message_loop_detector_reports_identical_bodies() {
        let mut detector = MessageLoopDetector::new();
        detector.observe("let me try again");
        detector.observe("let me try again");
        detector.observe("let me try again");
        assert!(detector.is_looping());
        // Two distinct messages push the repeated body out of the window (4).
        detector.observe("a different message");
        detector.observe("another message");
        assert!(!detector.is_looping());
    }

    #[test]
    fn signatures_do_not_embed_payload_text() {
        let sig = ToolCallSignature::new("tool", "secret-value-hunter2");
        assert!(!format!("{sig:?}").contains("hunter2"));
        assert_eq!(sig.tool(), "tool");
        assert_ne!(sig.arguments_hash(), hash_str("other"));
    }

    #[test]
    fn message_detector_observe_hash_and_reset_boundary() {
        let mut detector = MessageLoopDetector::new();
        detector.observe_hash(ArtifactId::from_bytes(b"still working"));
        detector.observe_hash(ArtifactId::from_bytes(b"still working"));
        assert!(!detector.is_looping());
        detector.observe_hash(ArtifactId::from_bytes(b"still working"));
        assert!(detector.is_looping());
        detector.reset();
        assert!(!detector.is_looping());
        assert!(detector.is_empty());
    }

    #[test]
    fn alternating_messages_do_not_trip_the_detector() {
        let mut detector = MessageLoopDetector::new();
        detector.observe_hash(ArtifactId::from_bytes(b"checking A"));
        detector.observe_hash(ArtifactId::from_bytes(b"checking B"));
        detector.observe_hash(ArtifactId::from_bytes(b"checking A"));
        assert!(
            !detector.is_looping(),
            "no message repeats >= threshold within the window"
        );
    }

    #[test]
    fn message_signature_does_not_embed_raw_body() {
        let sig = MessageSignature::new("SECRET body won't be stored");
        let rendered = format!("{sig:?}");
        assert!(!rendered.contains("SECRET body"));
        assert_ne!(sig.body_hash(), ArtifactId::from_bytes(b"other"));
    }
}
