//! Packet compaction with a deterministic fallback if a summarizer fails.
//!
//! Protected locators (mandatory blocks) are always retained. Summary text is
//! machine-built from counts; a model summarizer is optional and fail-closed
//! into the deterministic path.

use std::error::Error;
use std::fmt;

use crate::compile::{ContextPacket, ContextSource, explain_packet};
use crate::repo_manifest::CancellationToken;

/// How the summary was produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CompactMethod {
    Deterministic,
    Model,
}

/// Compacted view of one [`ContextPacket`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactedContext {
    summary: String,
    method: CompactMethod,
    retained_locators: Vec<String>,
}

/// Optional model-assisted summarizer. Failures fall back to deterministic text.
pub trait PacketSummarizer {
    fn summarize(
        &self,
        packet: &ContextPacket,
        cancel: &CancellationToken,
    ) -> Result<String, CompactError>;
}

/// Typed compact failure. Display never echoes locators or block text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactError {
    Cancelled,
    InvalidPacket,
}

impl CompactedContext {
    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn method(&self) -> CompactMethod {
        self.method
    }

    pub fn retained_locators(&self) -> &[String] {
        &self.retained_locators
    }
}

impl CompactError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::InvalidPacket => "invalid_packet",
        }
    }
}

impl fmt::Display for CompactMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deterministic => f.write_str("deterministic"),
            Self::Model => f.write_str("model"),
        }
    }
}

impl fmt::Display for CompactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for CompactError {}

/// Compact `packet`. `summarizer` is used when present; any failure falls back.
pub fn compact_packet(
    packet: &ContextPacket,
    summarizer: Option<&dyn PacketSummarizer>,
    cancel: &CancellationToken,
) -> Result<CompactedContext, CompactError> {
    if cancel.is_cancelled() {
        return Err(CompactError::Cancelled);
    }
    let retained: Vec<String> = packet
        .blocks()
        .iter()
        .filter(|block| block.is_mandatory() || block.source() == ContextSource::System)
        .map(|block| block.locator().to_string())
        .collect();
    if packet.blocks().is_empty() && packet.dropped().is_empty() {
        return Err(CompactError::InvalidPacket);
    }
    let deterministic = deterministic_summary(packet);
    if let Some(summarizer) = summarizer {
        match summarizer.summarize(packet, cancel) {
            Ok(text) if !text.trim().is_empty() => {
                return Ok(CompactedContext {
                    summary: text,
                    method: CompactMethod::Model,
                    retained_locators: retained,
                });
            }
            Err(CompactError::Cancelled) => return Err(CompactError::Cancelled),
            Ok(_) | Err(_) => {}
        }
    }
    Ok(CompactedContext {
        summary: deterministic,
        method: CompactMethod::Deterministic,
        retained_locators: retained,
    })
}

fn deterministic_summary(packet: &ContextPacket) -> String {
    let explain = explain_packet(packet);
    format!(
        "included={} dropped={} tokens={} reserve={} pressure={} partition_cap={} duplicate={}",
        explain.included(),
        explain.dropped(),
        explain.included_tokens(),
        explain.reserved_output(),
        explain.drop_budget_pressure(),
        explain.drop_partition_cap(),
        explain.drop_duplicate()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::{CompileContext, CompileInput, compile};

    fn block(locator: &str, tokens: u32) -> CompileInput {
        CompileInput::new(locator, locator).tokens(tokens)
    }

    fn packet() -> ContextPacket {
        compile(
            &CompileContext::new(100, 20)
                .safety_margin(0)
                .user(block("task", 10))
                .retrieved(block("noise", 80).score(1)),
        )
        .expect("compile")
    }

    struct OkSummarizer;
    impl PacketSummarizer for OkSummarizer {
        fn summarize(
            &self,
            _packet: &ContextPacket,
            cancel: &CancellationToken,
        ) -> Result<String, CompactError> {
            if cancel.is_cancelled() {
                return Err(CompactError::Cancelled);
            }
            Ok("model summary".to_string())
        }
    }

    struct FailingSummarizer;
    impl PacketSummarizer for FailingSummarizer {
        fn summarize(
            &self,
            _packet: &ContextPacket,
            _cancel: &CancellationToken,
        ) -> Result<String, CompactError> {
            Err(CompactError::InvalidPacket)
        }
    }

    #[test]
    fn deterministic_fallback_when_summarizer_fails() {
        let packet = packet();
        let compacted =
            compact_packet(&packet, Some(&FailingSummarizer), &CancellationToken::new())
                .expect("compact");
        assert_eq!(compacted.method(), CompactMethod::Deterministic);
        assert!(compacted.summary().contains("included="));
        assert!(!compacted.summary().contains("noise"));
        assert!(compacted.retained_locators().contains(&"task".to_string()));
    }

    #[test]
    fn model_summarizer_is_used_when_it_returns_text() {
        let packet = packet();
        let compacted = compact_packet(&packet, Some(&OkSummarizer), &CancellationToken::new())
            .expect("compact");
        assert_eq!(compacted.method(), CompactMethod::Model);
        assert_eq!(compacted.summary(), "model summary");
    }

    #[test]
    fn explain_reports_drop_and_partition_metrics() {
        let packet = packet();
        let explain = explain_packet(&packet);
        assert!(explain.included() >= 1);
        assert!(explain.reserved_output() == 20);
        assert!(explain.dropped() >= 1);
        assert_eq!(
            explain.drop_partition_cap() + explain.drop_budget_pressure(),
            explain.dropped()
        );
        assert!(!format!("{explain:?}").contains("noise"));
    }

    #[test]
    fn cancelled_compact_fails_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            compact_packet(&packet(), None, &cancel),
            Err(CompactError::Cancelled)
        );
    }
}
