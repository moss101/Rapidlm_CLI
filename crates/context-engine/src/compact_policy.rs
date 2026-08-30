//! Compaction policy: soft/hard token thresholds, strategy selection, and
//! fail-closed post-compaction verification.
//!
//! [`compact_packet`](crate::compact::compact_packet) answers *how* to
//! summarize a packet. This module answers *when* and *whether*: packets
//! below the soft threshold are passed through untouched (compaction spends
//! tokens too), packets between soft and hard are compacted opportunistically,
//! and packets at or over the hard threshold must compact or the turn fails
//! typed. After compaction the replacement is re-estimated and verified
//! against the hard threshold — a summary that does not shrink enough is a
//! `StillOverHard` failure, never a silently oversized context.
//!
//! Size estimates use the compiler's own packet token accounting plus a
//! documented `bytes / 4` heuristic for summary text; they are bounds for
//! admission decisions, not billing figures.

use std::fmt;

use crate::compact::{compact_packet, CompactError, CompactMethod, CompactedContext, PacketSummarizer};
use crate::compile::{ContextPacket, ContextSource, explain_packet};
use crate::repo_manifest::CancellationToken;

/// Fail closed when the compacted replacement still reaches this bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionPolicy {
    soft_tokens: u32,
    hard_tokens: u32,
    strategy: CompactionStrategy,
}

/// Which summarizer a policy-driven compaction may use.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CompactionStrategy {
    /// Prefer the model summarizer; fall back to deterministic text.
    #[default]
    ModelPreferred,
    /// Never call the model; deterministic summary only.
    DeterministicOnly,
}

/// Typed policy validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyValidationError {
    /// Soft and hard must both be positive.
    NonPositive,
    /// Soft must be strictly below hard so the bands are decidable.
    SoftNotBelowHard,
}

impl PolicyValidationError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NonPositive => "non_positive_threshold",
            Self::SoftNotBelowHard => "soft_not_below_hard",
        }
    }
}

impl fmt::Display for PolicyValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for PolicyValidationError {}

impl CompactionPolicy {
    /// Validate and construct. `soft_tokens` must be positive and strictly
    /// below `hard_tokens`.
    pub fn new(
        soft_tokens: u32,
        hard_tokens: u32,
        strategy: CompactionStrategy,
    ) -> Result<Self, PolicyValidationError> {
        if soft_tokens == 0 || hard_tokens == 0 {
            return Err(PolicyValidationError::NonPositive);
        }
        if soft_tokens >= hard_tokens {
            return Err(PolicyValidationError::SoftNotBelowHard);
        }
        Ok(Self {
            soft_tokens,
            hard_tokens,
            strategy,
        })
    }

    pub const fn soft_tokens(&self) -> u32 {
        self.soft_tokens
    }

    pub const fn hard_tokens(&self) -> u32 {
        self.hard_tokens
    }

    pub const fn strategy(&self) -> CompactionStrategy {
        self.strategy
    }
}

/// Threshold classification of a packet's token total.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThresholdDecision {
    /// Below soft: keep the packet as compiled.
    Under,
    /// Soft..hard: compaction is worthwhile but not mandatory.
    Soft,
    /// At or over hard: compaction must succeed or the turn fails.
    Hard,
}

/// Outcome of a policy-driven compaction run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionOutcome {
    /// Why compaction ran (or did not).
    pub decision: ThresholdDecision,
    /// The compacted replacement; `None` when the decision was `Under`.
    pub compacted: Option<CompactedContext>,
    /// Audit trail for telemetry: thresholds, sizes, method, verification.
    pub evidence: CompactionEvidence,
}

/// Machine-readable compaction record. Values are estimates, never secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionEvidence {
    pub decision: ThresholdDecision,
    pub strategy: CompactionStrategy,
    /// Method used when compaction ran; `Deterministic` when it did not.
    pub method: CompactMethod,
    pub before_tokens: u32,
    pub after_estimate_tokens: u32,
    pub soft_tokens: u32,
    pub hard_tokens: u32,
    /// True when the replacement verified under the hard threshold (or no
    /// compaction was needed).
    pub verified: bool,
}

/// Typed policy-run failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactPolicyError {
    Cancelled,
    InvalidPacket,
    /// The compacted replacement still reaches the hard threshold.
    StillOverHard {
        estimate: u32,
        hard: u32,
    },
}

impl CompactPolicyError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::InvalidPacket => "invalid_packet",
            Self::StillOverHard { .. } => "still_over_hard",
        }
    }
}

impl fmt::Display for CompactPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StillOverHard { estimate, hard } => {
                write!(f, "compacted context still exceeds the hard threshold (estimate {estimate} >= {hard} tokens)")
            }
            other => f.write_str(other.as_str()),
        }
    }
}

impl std::error::Error for CompactPolicyError {}

/// Classify a packet token total against the policy bands.
pub fn classify(tokens: u32, policy: &CompactionPolicy) -> ThresholdDecision {
    if tokens >= policy.hard_tokens {
        ThresholdDecision::Hard
    } else if tokens >= policy.soft_tokens {
        ThresholdDecision::Soft
    } else {
        ThresholdDecision::Under
    }
}

/// Run a policy-driven compaction over `packet`.
///
/// `Under` short-circuits: no summarizer call, no replacement. `Soft` and
/// `Hard` compact per the strategy, then verify the replacement estimates
/// under the hard threshold; `DeterministicOnly` never invokes `summarizer`.
pub fn compact_with_policy(
    packet: &crate::compile::ContextPacket,
    policy: &CompactionPolicy,
    summarizer: Option<&dyn PacketSummarizer>,
    cancel: &CancellationToken,
) -> Result<CompactionOutcome, CompactPolicyError> {
    if cancel.is_cancelled() {
        return Err(CompactPolicyError::Cancelled);
    }
    let before = estimate_packet_tokens(packet);
    let decision = classify(before, policy);
    if decision == ThresholdDecision::Under {
        return Ok(CompactionOutcome {
            decision,
            compacted: None,
            evidence: CompactionEvidence {
                decision,
                strategy: policy.strategy(),
                method: CompactMethod::Deterministic,
                before_tokens: before,
                after_estimate_tokens: before,
                soft_tokens: policy.soft_tokens(),
                hard_tokens: policy.hard_tokens(),
                verified: true,
            },
        });
    }
    let effective_summarizer = match policy.strategy() {
        CompactionStrategy::ModelPreferred => summarizer,
        CompactionStrategy::DeterministicOnly => None,
    };
    let compacted = compact_packet(packet, effective_summarizer, cancel)
        .map_err(|err| match err {
            CompactError::Cancelled => CompactPolicyError::Cancelled,
            CompactError::InvalidPacket => CompactPolicyError::InvalidPacket,
        })?;
    let after = estimate_compacted_tokens(packet, &compacted);
    if after >= policy.hard_tokens() {
        return Err(CompactPolicyError::StillOverHard {
            estimate: after,
            hard: policy.hard_tokens(),
        });
    }
    Ok(CompactionOutcome {
        decision,
        compacted: Some(compacted.clone()),
        evidence: CompactionEvidence {
            decision,
            strategy: policy.strategy(),
            method: compacted.method(),
            before_tokens: before,
            after_estimate_tokens: after,
            soft_tokens: policy.soft_tokens(),
            hard_tokens: policy.hard_tokens(),
            verified: true,
        },
    })
}

/// Packet-side estimate from the compiler's own token accounting.
fn estimate_packet_tokens(packet: &crate::compile::ContextPacket) -> u32 {
    explain_packet(packet).included_tokens()
}

/// Replacement-side estimate: the *real* mandatory/system block tokens from
/// the original packet — `compact_packet` never touches these, it only ever
/// replaces the non-mandatory blocks with `compacted.summary()` — plus
/// `bytes / 4` over the summary text. Deliberately conservative and
/// deterministic, and critically measures what actually survives into the
/// rebuilt packet: `compacted.retained_locators()` holds short locator
/// *labels* (`"system/prompt"`, a handful of bytes each), not the mandatory
/// content they name, so summing those instead of the real blocks' own
/// `estimated_tokens()` made this check pass almost regardless of the real
/// post-compaction size — see `newtask.md`'s note on this fix.
fn estimate_compacted_tokens(packet: &ContextPacket, compacted: &CompactedContext) -> u32 {
    let mandatory_tokens: u32 = packet
        .blocks()
        .iter()
        .filter(|block| block.is_mandatory() || block.source() == ContextSource::System)
        .map(|block| block.estimated_tokens())
        .fold(0u32, u32::saturating_add);
    let summary_tokens = (compacted.summary().len() as u32).div_ceil(4);
    mandatory_tokens.saturating_add(summary_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::{CompileContext, CompileInput, compile};

    fn block(locator: &str, tokens: u32) -> CompileInput {
        CompileInput::new(locator, locator).tokens(tokens)
    }

    fn packet() -> crate::compile::ContextPacket {
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
            _packet: &crate::compile::ContextPacket,
            _cancel: &CancellationToken,
        ) -> Result<String, CompactError> {
            Ok("concise model summary".to_owned())
        }
    }

    struct PanickySummarizer;
    impl PacketSummarizer for PanickySummarizer {
        fn summarize(
            &self,
            _packet: &crate::compile::ContextPacket,
            _cancel: &CancellationToken,
        ) -> Result<String, CompactError> {
            panic!("deterministic strategy must not call the summarizer")
        }
    }

    #[test]
    fn policy_validates_band_ordering() {
        assert_eq!(
            CompactionPolicy::new(0, 10, CompactionStrategy::ModelPreferred),
            Err(PolicyValidationError::NonPositive)
        );
        assert_eq!(
            CompactionPolicy::new(10, 0, CompactionStrategy::ModelPreferred),
            Err(PolicyValidationError::NonPositive)
        );
        assert_eq!(
            CompactionPolicy::new(10, 10, CompactionStrategy::ModelPreferred),
            Err(PolicyValidationError::SoftNotBelowHard)
        );
        assert!(CompactionPolicy::new(10, 11, CompactionStrategy::ModelPreferred).is_ok());
    }

    #[test]
    fn classification_matches_band_boundaries() {
        let policy = CompactionPolicy::new(50, 100, CompactionStrategy::ModelPreferred).expect("policy");
        assert_eq!(classify(49, &policy), ThresholdDecision::Under);
        assert_eq!(classify(50, &policy), ThresholdDecision::Soft);
        assert_eq!(classify(99, &policy), ThresholdDecision::Soft);
        assert_eq!(classify(100, &policy), ThresholdDecision::Hard);
        assert_eq!(classify(u32::MAX, &policy), ThresholdDecision::Hard);
    }

    #[test]
    fn under_soft_threshold_skips_compaction_entirely() {
        let packet = compile(
            &CompileContext::new(1_000_000, 20)
                .safety_margin(0)
                .user(block("task", 10)),
        )
        .expect("compile");
        let policy = CompactionPolicy::new(500, 900, CompactionStrategy::ModelPreferred).expect("policy");
        let outcome =
            compact_with_policy(&packet, &policy, Some(&OkSummarizer), &CancellationToken::new())
                .expect("outcome");
        assert_eq!(outcome.decision, ThresholdDecision::Under);
        assert!(outcome.compacted.is_none());
        assert!(outcome.evidence.verified);
        assert_eq!(outcome.evidence.before_tokens, outcome.evidence.after_estimate_tokens);
    }

    #[test]
    fn soft_decision_prefers_model_summary_and_verifies() {
        let policy = CompactionPolicy::new(10, 10_000, CompactionStrategy::ModelPreferred).expect("policy");
        let outcome =
            compact_with_policy(&packet(), &policy, Some(&OkSummarizer), &CancellationToken::new())
                .expect("outcome");
        assert_eq!(outcome.decision, ThresholdDecision::Soft);
        let compacted = outcome.compacted.expect("compacted");
        assert_eq!(compacted.method(), CompactMethod::Model);
        assert_eq!(outcome.evidence.method, CompactMethod::Model);
        assert!(outcome.evidence.after_estimate_tokens < outcome.evidence.hard_tokens);
    }

    #[test]
    fn deterministic_strategy_never_calls_the_summarizer() {
        let policy = CompactionPolicy::new(10, 10_000, CompactionStrategy::DeterministicOnly).expect("policy");
        let outcome = compact_with_policy(
            &packet(),
            &policy,
            Some(&PanickySummarizer),
            &CancellationToken::new(),
        )
        .expect("outcome");
        assert_eq!(outcome.evidence.method, CompactMethod::Deterministic);
    }

    #[test]
    fn replacement_over_hard_fails_closed() {
        // hard = 1: any replacement must fail verification.
        let policy = CompactionPolicy::new(1, 1, CompactionStrategy::DeterministicOnly);
        assert_eq!(
            policy,
            Err(PolicyValidationError::SoftNotBelowHard),
            "degenerate bands are rejected at construction"
        );
        let policy = CompactionPolicy::new(1, 2, CompactionStrategy::DeterministicOnly).expect("policy");
        let err = compact_with_policy(
            &packet(),
            &policy,
            None,
            &CancellationToken::new(),
        )
        .expect_err("replacement cannot fit in 2 tokens");
        match err {
            CompactPolicyError::StillOverHard { estimate, hard } => {
                assert!(estimate >= hard);
                assert_eq!(hard, 2);
            }
            other => panic!("expected StillOverHard, got {other:?}"),
        }
        assert_eq!(err.as_str(), "still_over_hard");
        assert!(err.to_string().contains("hard threshold"));
    }

    #[test]
    fn hard_check_measures_real_mandatory_content_not_locator_labels() {
        // `retained_locators()` holds short locator *labels* (e.g. the
        // 4-byte string "task"), not the mandatory content they name — this
        // packet's mandatory block alone is 45 real tokens, but its locator
        // label estimates to ~1 token. A hard threshold of 50 means
        // compaction (which only ever replaces the *optional* "noise" block
        // with a summary, never the mandatory one) can never bring the real
        // post-compaction size under 50: mandatory content alone is already
        // most of the way there. The old locator-label-based estimate would
        // have summed to only a handful of tokens (nowhere near 50) and
        // wrongly reported `verified: true`.
        let policy = CompactionPolicy::new(1, 50, CompactionStrategy::DeterministicOnly).expect("policy");
        let packet = compile(
            &CompileContext::new(100, 20)
                .safety_margin(0)
                .user(block("task", 45))
                .retrieved(block("noise", 30).score(1)),
        )
        .expect("compile");
        let err = compact_with_policy(&packet, &policy, None, &CancellationToken::new())
            .expect_err("mandatory content alone is too close to the hard cap");
        match err {
            CompactPolicyError::StillOverHard { estimate, hard } => {
                assert_eq!(hard, 50);
                assert!(
                    estimate >= 45,
                    "estimate must reflect the real 45-token mandatory block, got {estimate}"
                );
            }
            other => panic!("expected StillOverHard, got {other:?}"),
        }
    }

    #[test]
    fn cancelled_input_fails_typed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let policy = CompactionPolicy::new(10, 10_000, CompactionStrategy::ModelPreferred).expect("policy");
        assert_eq!(
            compact_with_policy(&packet(), &policy, Some(&OkSummarizer), &cancel),
            Err(CompactPolicyError::Cancelled)
        );
    }
}
