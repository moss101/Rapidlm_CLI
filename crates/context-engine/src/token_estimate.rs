//! Provider-aware token estimates with a conservative fallback.
//!
//! Exact family tokenizers may be injected. Missing or unknown families use a
//! conservative heuristic. Cache keys are tokenizer family plus content hash.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use crate::ingest::content::ContentHash;
use crate::ingest::walk::DEFAULT_MAX_FILE_BYTES;
use crate::repo_manifest::CancellationToken;

/// Default wall-clock budget for one estimate.
pub const DEFAULT_ESTIMATE_TIMEOUT: Duration = Duration::from_secs(1);

/// Default UTF-8 byte cap for one estimate input.
pub const DEFAULT_MAX_ESTIMATE_BYTES: usize = DEFAULT_MAX_FILE_BYTES as usize;

/// Default distinct cache entries retained by one estimator.
pub const DEFAULT_MAX_CACHE_ENTRIES: usize = 4_096;

const CANCEL_STRIDE: usize = 16;

/// Tokenizer family used for provider-aware estimates and cache keys.
///
/// Families are encodings, not model IDs. Unknown is the conservative fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum TokenizerFamily {
    OpenaiCl100k,
    OpenaiO200k,
    AnthropicClaude,
    Llama,
    Unknown,
}

/// How an estimate was produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum EstimateConfidence {
    /// Unknown-family conservative heuristic.
    Low,
    /// Family-specific heuristic with no exact tokenizer registered.
    Medium,
    /// Exact tokenizer for the requested family.
    High,
}

/// Cache identity: tokenizer family and SHA-256 of the estimated bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct EstimateCacheKey {
    family: TokenizerFamily,
    content_hash: ContentHash,
}

/// Tokens and confidence for one text/family pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TokenEstimate {
    tokens: u32,
    confidence: EstimateConfidence,
}

/// Resource bounds for one [`TokenEstimator`]. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct TokenEstimateLimits {
    max_text_bytes: usize,
    max_cache_entries: usize,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Optional exact counter for one tokenizer family.
pub trait FamilyTokenizer {
    fn family(&self) -> TokenizerFamily;
    fn count(&self, text: &str, cancel: &CancellationToken) -> Result<u32, TokenEstimateError>;
}

/// Provider-aware estimator with a bounded family/hash cache.
pub struct TokenEstimator {
    limits: TokenEstimateLimits,
    cache: HashMap<EstimateCacheKey, TokenEstimate>,
    order: VecDeque<EstimateCacheKey>,
    tokenizers: HashMap<TokenizerFamily, Box<dyn FamilyTokenizer + Send + Sync>>,
}

/// Typed estimate failure. Display never echoes source text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenEstimateError {
    Cancelled,
    Timeout,
    TextTooLarge,
    TokenizerFailed,
}

impl TokenizerFamily {
    pub const ALL: &'static [Self] = &[
        Self::OpenaiCl100k,
        Self::OpenaiO200k,
        Self::AnthropicClaude,
        Self::Llama,
        Self::Unknown,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiCl100k => "cl100k",
            Self::OpenaiO200k => "o200k",
            Self::AnthropicClaude => "claude",
            Self::Llama => "llama",
            Self::Unknown => "unknown",
        }
    }

    /// Conservative units-per-token as `(numerator, denominator)` of `ceil(units * n / d)`.
    ///
    /// Smaller ratios yield more tokens. Unknown is the most conservative family.
    const fn unit_ratio(self) -> (u64, u64) {
        match self {
            Self::Unknown => (1, 2),
            Self::Llama => (2, 5),
            Self::AnthropicClaude => (1, 3),
            Self::OpenaiCl100k => (5, 16),
            Self::OpenaiO200k => (1, 4),
        }
    }
}

impl EstimateConfidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl EstimateCacheKey {
    pub fn new(family: TokenizerFamily, content_hash: ContentHash) -> Self {
        Self {
            family,
            content_hash,
        }
    }

    pub fn family(&self) -> TokenizerFamily {
        self.family
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn from_text(family: TokenizerFamily, text: &str) -> Self {
        Self::new(family, ContentHash::from_bytes(text.as_bytes()))
    }
}

impl TokenEstimate {
    pub fn new(tokens: u32, confidence: EstimateConfidence) -> Self {
        Self { tokens, confidence }
    }

    pub fn tokens(&self) -> u32 {
        self.tokens
    }

    pub fn confidence(&self) -> EstimateConfidence {
        self.confidence
    }
}

impl TokenEstimateLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_text_bytes(mut self, value: usize) -> Self {
        self.max_text_bytes = value;
        self
    }

    pub fn max_cache_entries(mut self, value: usize) -> Self {
        self.max_cache_entries = value;
        self
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }

    pub fn max_text_bytes_value(&self) -> usize {
        self.max_text_bytes
    }

    pub fn max_cache_entries_value(&self) -> usize {
        self.max_cache_entries
    }

    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl Default for TokenEstimateLimits {
    fn default() -> Self {
        Self {
            max_text_bytes: DEFAULT_MAX_ESTIMATE_BYTES,
            max_cache_entries: DEFAULT_MAX_CACHE_ENTRIES,
            timeout: DEFAULT_ESTIMATE_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }
}

impl TokenEstimator {
    pub fn new() -> Self {
        Self::with_limits(TokenEstimateLimits::new())
    }

    pub fn with_limits(limits: TokenEstimateLimits) -> Self {
        Self {
            limits,
            cache: HashMap::new(),
            order: VecDeque::new(),
            tokenizers: HashMap::new(),
        }
    }

    pub fn limits(&self) -> &TokenEstimateLimits {
        &self.limits
    }

    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    pub fn contains(&self, key: &EstimateCacheKey) -> bool {
        self.cache.contains_key(key)
    }

    pub fn cached(&self, key: &EstimateCacheKey) -> Option<TokenEstimate> {
        self.cache.get(key).copied()
    }

    /// Register or replace the exact tokenizer for its family.
    pub fn register_tokenizer(&mut self, tokenizer: Box<dyn FamilyTokenizer + Send + Sync>) {
        self.tokenizers.insert(tokenizer.family(), tokenizer);
    }

    /// Estimate tokens for `text` under `model_family`.
    ///
    /// Cache key is tokenizer family plus content hash. Missing exact tokenizers
    /// use a conservative heuristic. Non-empty text never yields zero tokens.
    pub fn estimate(
        &mut self,
        model_family: TokenizerFamily,
        text: &str,
    ) -> Result<TokenEstimate, TokenEstimateError> {
        let started = Instant::now();
        self.check_ready(started)?;
        if text.len() > self.limits.max_text_bytes {
            return Err(TokenEstimateError::TextTooLarge);
        }

        let key = EstimateCacheKey::from_text(model_family, text);
        if let Some(hit) = self.cache.get(&key).copied() {
            return Ok(hit);
        }

        let estimate = self.compute(model_family, text, started)?;
        self.insert_cache(key, estimate);
        Ok(estimate)
    }

    fn compute(
        &self,
        family: TokenizerFamily,
        text: &str,
        started: Instant,
    ) -> Result<TokenEstimate, TokenEstimateError> {
        if let Some(tokenizer) = self.tokenizers.get(&family) {
            let tokens = tokenizer.count(text, &self.limits.cancel)?;
            self.check_ready(started)?;
            return Ok(TokenEstimate::new(
                clamp_tokens(text, tokens),
                EstimateConfidence::High,
            ));
        }

        let units = weighted_units(text, &self.limits.cancel, started, self.limits.timeout)?;
        let tokens = heuristic_tokens(family, units);
        let confidence = if family == TokenizerFamily::Unknown {
            EstimateConfidence::Low
        } else {
            EstimateConfidence::Medium
        };
        Ok(TokenEstimate::new(clamp_tokens(text, tokens), confidence))
    }

    fn insert_cache(&mut self, key: EstimateCacheKey, estimate: TokenEstimate) {
        if self.limits.max_cache_entries == 0 || self.cache.contains_key(&key) {
            return;
        }
        while self.cache.len() >= self.limits.max_cache_entries {
            if let Some(old) = self.order.pop_front() {
                self.cache.remove(&old);
            } else {
                break;
            }
        }
        if self.cache.len() >= self.limits.max_cache_entries {
            return;
        }
        self.cache.insert(key, estimate);
        self.order.push_back(key);
    }

    fn check_ready(&self, started: Instant) -> Result<(), TokenEstimateError> {
        if self.limits.cancel.is_cancelled() {
            return Err(TokenEstimateError::Cancelled);
        }
        if self.limits.timeout.is_zero() || started.elapsed() > self.limits.timeout {
            return Err(TokenEstimateError::Timeout);
        }
        Ok(())
    }
}

impl Default for TokenEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenEstimateError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::TextTooLarge => "text too large",
            Self::TokenizerFailed => "tokenizer failed",
        }
    }
}

impl fmt::Display for TokenizerFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for EstimateConfidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for TokenEstimateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for TokenEstimateError {}

fn clamp_tokens(text: &str, tokens: u32) -> u32 {
    if text.is_empty() { 0 } else { tokens.max(1) }
}

fn weighted_units(
    text: &str,
    cancel: &CancellationToken,
    started: Instant,
    timeout: Duration,
) -> Result<u64, TokenEstimateError> {
    let mut units = 0u64;
    for (step, ch) in text.chars().enumerate() {
        if step == 0 || step.is_multiple_of(CANCEL_STRIDE) {
            if cancel.is_cancelled() {
                return Err(TokenEstimateError::Cancelled);
            }
            if timeout.is_zero() || started.elapsed() > timeout {
                return Err(TokenEstimateError::Timeout);
            }
        }
        units = units.saturating_add(unit_weight(ch));
    }
    Ok(units)
}

fn unit_weight(ch: char) -> u64 {
    if ch.is_ascii() { 1 } else { 2 }
}

fn heuristic_tokens(family: TokenizerFamily, units: u64) -> u32 {
    let (num, den) = family.unit_ratio();
    let prod = units.saturating_mul(num);
    let tokens = prod.div_ceil(den);
    u32::try_from(tokens).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedTokenizer {
        family: TokenizerFamily,
        tokens: u32,
    }

    impl FamilyTokenizer for FixedTokenizer {
        fn family(&self) -> TokenizerFamily {
            self.family
        }

        fn count(
            &self,
            _text: &str,
            cancel: &CancellationToken,
        ) -> Result<u32, TokenEstimateError> {
            if cancel.is_cancelled() {
                return Err(TokenEstimateError::Cancelled);
            }
            Ok(self.tokens)
        }
    }

    struct ZeroTokenizer;

    impl FamilyTokenizer for ZeroTokenizer {
        fn family(&self) -> TokenizerFamily {
            TokenizerFamily::OpenaiCl100k
        }

        fn count(
            &self,
            _text: &str,
            _cancel: &CancellationToken,
        ) -> Result<u32, TokenEstimateError> {
            Ok(0)
        }
    }

    struct FailingTokenizer;

    impl FamilyTokenizer for FailingTokenizer {
        fn family(&self) -> TokenizerFamily {
            TokenizerFamily::Llama
        }

        fn count(
            &self,
            _text: &str,
            _cancel: &CancellationToken,
        ) -> Result<u32, TokenEstimateError> {
            Err(TokenEstimateError::TokenizerFailed)
        }
    }

    #[test]
    fn empty_text_is_zero_tokens() {
        let mut estimator = TokenEstimator::new();
        let estimate = estimator
            .estimate(TokenizerFamily::Unknown, "")
            .expect("empty");
        assert_eq!(estimate.tokens(), 0);
        assert_eq!(estimate.confidence(), EstimateConfidence::Low);
    }

    #[test]
    fn fallback_never_returns_zero_for_non_empty_text() {
        let mut estimator = TokenEstimator::new();
        for family in TokenizerFamily::ALL {
            for text in ["a", " ", "\n", "你好", "fn main() {}"] {
                let estimate = estimator.estimate(*family, text).expect("estimate");
                assert!(
                    estimate.tokens() >= 1,
                    "family {family} text {text:?} was zero"
                );
            }
        }
    }

    #[test]
    fn cache_key_includes_tokenizer_family_and_content_hash() {
        let text = "fn compile() {}";
        let cl100k = EstimateCacheKey::from_text(TokenizerFamily::OpenaiCl100k, text);
        let o200k = EstimateCacheKey::from_text(TokenizerFamily::OpenaiO200k, text);
        let other = EstimateCacheKey::from_text(TokenizerFamily::OpenaiCl100k, "other");

        assert_eq!(cl100k.family(), TokenizerFamily::OpenaiCl100k);
        assert_eq!(
            cl100k.content_hash(),
            ContentHash::from_bytes(text.as_bytes())
        );
        assert_ne!(cl100k, o200k);
        assert_ne!(cl100k, other);
        assert_eq!(
            cl100k,
            EstimateCacheKey::new(TokenizerFamily::OpenaiCl100k, cl100k.content_hash())
        );
    }

    #[test]
    fn same_text_different_families_are_distinct_cache_entries() {
        let text = "let x = 1;\n".repeat(32);
        let mut estimator = TokenEstimator::new();
        let first = estimator
            .estimate(TokenizerFamily::Unknown, &text)
            .expect("unknown");
        let second = estimator
            .estimate(TokenizerFamily::OpenaiO200k, &text)
            .expect("o200k");

        assert_ne!(first.tokens(), second.tokens());
        assert_eq!(estimator.cache_len(), 2);
        assert!(estimator.contains(&EstimateCacheKey::from_text(
            TokenizerFamily::Unknown,
            &text
        )));
        assert!(estimator.contains(&EstimateCacheKey::from_text(
            TokenizerFamily::OpenaiO200k,
            &text
        )));
    }

    #[test]
    fn repeated_estimate_is_a_cache_hit() {
        let text = "cached body";
        let mut estimator = TokenEstimator::new();
        let first = estimator
            .estimate(TokenizerFamily::AnthropicClaude, text)
            .expect("first");
        assert_eq!(estimator.cache_len(), 1);
        let second = estimator
            .estimate(TokenizerFamily::AnthropicClaude, text)
            .expect("hit");
        assert_eq!(first, second);
        assert_eq!(estimator.cache_len(), 1);
        assert_eq!(
            estimator.cached(&EstimateCacheKey::from_text(
                TokenizerFamily::AnthropicClaude,
                text
            )),
            Some(first)
        );
    }

    #[test]
    fn unknown_family_is_the_most_conservative_heuristic() {
        let text = "a".repeat(100);
        let mut estimator = TokenEstimator::new();
        let unknown = estimator
            .estimate(TokenizerFamily::Unknown, &text)
            .expect("unknown")
            .tokens();
        for family in [
            TokenizerFamily::Llama,
            TokenizerFamily::AnthropicClaude,
            TokenizerFamily::OpenaiCl100k,
            TokenizerFamily::OpenaiO200k,
        ] {
            let tokens = estimator.estimate(family, &text).expect("family").tokens();
            assert!(unknown >= tokens, "unknown {unknown} < {family} {tokens}");
        }
    }

    #[test]
    fn provider_aware_families_differ_on_the_same_text() {
        let text = "a".repeat(80);
        let mut estimator = TokenEstimator::new();
        let mut seen = Vec::new();
        for family in TokenizerFamily::ALL {
            let estimate = estimator.estimate(*family, &text).expect("family");
            if *family == TokenizerFamily::Unknown {
                assert_eq!(estimate.confidence(), EstimateConfidence::Low);
            } else {
                assert_eq!(estimate.confidence(), EstimateConfidence::Medium);
            }
            seen.push((*family, estimate.tokens()));
        }
        let unique: std::collections::BTreeSet<u32> = seen.iter().map(|(_, n)| *n).collect();
        assert_eq!(unique.len(), TokenizerFamily::ALL.len(), "{seen:?}");
    }

    #[test]
    fn registered_tokenizer_is_high_confidence_and_family_scoped() {
        let text = "exact path";
        let mut estimator = TokenEstimator::new();
        estimator.register_tokenizer(Box::new(FixedTokenizer {
            family: TokenizerFamily::OpenaiCl100k,
            tokens: 42,
        }));

        let exact = estimator
            .estimate(TokenizerFamily::OpenaiCl100k, text)
            .expect("exact");
        assert_eq!(exact.tokens(), 42);
        assert_eq!(exact.confidence(), EstimateConfidence::High);

        let heuristic = estimator
            .estimate(TokenizerFamily::OpenaiO200k, text)
            .expect("heuristic");
        assert_ne!(heuristic.tokens(), 42);
        assert_eq!(heuristic.confidence(), EstimateConfidence::Medium);
        assert_eq!(estimator.cache_len(), 2);
    }

    #[test]
    fn exact_zero_for_non_empty_is_clamped() {
        let mut estimator = TokenEstimator::new();
        estimator.register_tokenizer(Box::new(ZeroTokenizer));
        let estimate = estimator
            .estimate(TokenizerFamily::OpenaiCl100k, "x")
            .expect("clamp");
        assert_eq!(estimate.tokens(), 1);
        assert_eq!(estimate.confidence(), EstimateConfidence::High);
    }

    #[test]
    fn tokenizer_failure_is_not_silently_replaced() {
        let mut estimator = TokenEstimator::new();
        estimator.register_tokenizer(Box::new(FailingTokenizer));
        assert_eq!(
            estimator.estimate(TokenizerFamily::Llama, "body"),
            Err(TokenEstimateError::TokenizerFailed)
        );
        assert_eq!(estimator.cache_len(), 0);
    }

    #[test]
    fn cancelled_and_zero_timeout_fail_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut cancelled =
            TokenEstimator::with_limits(TokenEstimateLimits::new().cancellation(cancel));
        assert_eq!(
            cancelled.estimate(TokenizerFamily::Unknown, "x"),
            Err(TokenEstimateError::Cancelled)
        );

        let mut timed_out =
            TokenEstimator::with_limits(TokenEstimateLimits::new().timeout(Duration::ZERO));
        assert_eq!(
            timed_out.estimate(TokenizerFamily::Unknown, "x"),
            Err(TokenEstimateError::Timeout)
        );
    }

    #[test]
    fn oversized_text_is_rejected() {
        let mut estimator =
            TokenEstimator::with_limits(TokenEstimateLimits::new().max_text_bytes(3));
        assert_eq!(
            estimator.estimate(TokenizerFamily::Unknown, "abcd"),
            Err(TokenEstimateError::TextTooLarge)
        );
        assert_eq!(estimator.cache_len(), 0);
    }

    #[test]
    fn cache_evicts_oldest_when_full() {
        let mut estimator =
            TokenEstimator::with_limits(TokenEstimateLimits::new().max_cache_entries(1));
        estimator
            .estimate(TokenizerFamily::Unknown, "one")
            .expect("one");
        let first = EstimateCacheKey::from_text(TokenizerFamily::Unknown, "one");
        assert!(estimator.contains(&first));

        estimator
            .estimate(TokenizerFamily::Unknown, "two")
            .expect("two");
        assert_eq!(estimator.cache_len(), 1);
        assert!(!estimator.contains(&first));
        assert!(estimator.contains(&EstimateCacheKey::from_text(
            TokenizerFamily::Unknown,
            "two"
        )));
    }

    #[test]
    fn disabled_cache_still_estimates() {
        let mut estimator =
            TokenEstimator::with_limits(TokenEstimateLimits::new().max_cache_entries(0));
        let estimate = estimator
            .estimate(TokenizerFamily::Unknown, "abc")
            .expect("estimate");
        assert!(estimate.tokens() >= 1);
        assert_eq!(estimator.cache_len(), 0);
    }

    #[test]
    fn non_ascii_units_are_heavier_than_ascii() {
        let mut estimator = TokenEstimator::new();
        let ascii = estimator
            .estimate(TokenizerFamily::Unknown, &"a".repeat(20))
            .expect("ascii")
            .tokens();
        let cjk = estimator
            .estimate(TokenizerFamily::Unknown, &"你".repeat(20))
            .expect("cjk")
            .tokens();
        assert!(cjk > ascii, "cjk {cjk} should exceed ascii {ascii}");
    }

    #[test]
    fn replacing_tokenizer_is_family_scoped() {
        let mut estimator = TokenEstimator::new();
        estimator.register_tokenizer(Box::new(FixedTokenizer {
            family: TokenizerFamily::Llama,
            tokens: 7,
        }));
        estimator.register_tokenizer(Box::new(FixedTokenizer {
            family: TokenizerFamily::Llama,
            tokens: 11,
        }));
        let estimate = estimator
            .estimate(TokenizerFamily::Llama, "replace me")
            .expect("replaced");
        assert_eq!(estimate.tokens(), 11);
        assert_eq!(estimate.confidence(), EstimateConfidence::High);
    }

    #[test]
    fn error_display_is_safe() {
        assert_eq!(
            TokenEstimateError::TokenizerFailed.to_string(),
            "tokenizer failed"
        );
        assert!(!TokenEstimateError::TextTooLarge.to_string().contains('/'));
        assert!(!format!("{:?}", TokenEstimateError::Cancelled).contains("secret"));
    }
}
