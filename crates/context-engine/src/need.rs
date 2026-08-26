//! Typed [`InformationNeed`] for Context Scout / compile inputs.
//!
//! Distinct from [`crate::ContextPacket`]: a need is the request; a packet is
//! the compiled evidence-bearing result. Invalid needs fail closed.

use std::fmt;

/// Maximum questions accepted on one need.
pub const MAX_NEED_QUESTIONS: usize = 32;

/// Maximum UTF-8 bytes per question.
pub const MAX_QUESTION_BYTES: usize = 512;

/// Maximum path/symbol anchors.
pub const MAX_ANCHORS: usize = 64;

/// Maximum scope path prefixes.
pub const MAX_SCOPE_PATHS: usize = 64;

/// Maximum UTF-8 bytes per path or symbol.
pub const MAX_REF_BYTES: usize = 1024;

/// Representative vs exhaustive enumeration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CompletenessRequirement {
    Representative,
    Exhaustive,
}

/// How absence claims must be evidenced.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum NegativeClaimPolicy {
    RequireCheckedScope,
    AllowUnchecked,
}

/// Path prefixes the need is allowed to search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeSet {
    paths: Vec<String>,
}

/// File/symbol anchor already known to the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodeRef {
    path: String,
    symbol: Option<String>,
}

/// Scout/compile request. Token budget is a hard cap, not a hint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InformationNeed {
    questions: Vec<String>,
    scope: ScopeSet,
    known_anchors: Vec<CodeRef>,
    completeness: CompletenessRequirement,
    need_callers: bool,
    need_tests: bool,
    need_types: bool,
    need_config: bool,
    negative_claim_policy: NegativeClaimPolicy,
    token_budget: u32,
}

/// Construction failures. Empty/oversized fields never become a need.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NeedError {
    EmptyQuestions,
    EmptyQuestion,
    TooManyQuestions,
    QuestionTooLarge,
    TooManyAnchors,
    TooManyScopePaths,
    InvalidRef,
    EmptyScopeForExhaustive,
    ZeroBudget,
}

impl ScopeSet {
    pub fn new(paths: Vec<String>) -> Result<Self, NeedError> {
        if paths.len() > MAX_SCOPE_PATHS {
            return Err(NeedError::TooManyScopePaths);
        }
        for path in &paths {
            validate_ref_field(path)?;
        }
        Ok(Self { paths })
    }

    pub fn empty() -> Self {
        Self { paths: Vec::new() }
    }

    pub fn paths(&self) -> &[String] {
        &self.paths
    }
}

impl CodeRef {
    pub fn new(path: impl Into<String>, symbol: Option<String>) -> Result<Self, NeedError> {
        let path = path.into();
        validate_ref_field(&path)?;
        if let Some(symbol) = symbol.as_ref() {
            validate_ref_field(symbol)?;
        }
        Ok(Self { path, symbol })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn symbol(&self) -> Option<&str> {
        self.symbol.as_deref()
    }
}

impl InformationNeed {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        questions: Vec<String>,
        scope: ScopeSet,
        known_anchors: Vec<CodeRef>,
        completeness: CompletenessRequirement,
        need_callers: bool,
        need_tests: bool,
        need_types: bool,
        need_config: bool,
        negative_claim_policy: NegativeClaimPolicy,
        token_budget: u32,
    ) -> Result<Self, NeedError> {
        if questions.is_empty() {
            return Err(NeedError::EmptyQuestions);
        }
        if questions.len() > MAX_NEED_QUESTIONS {
            return Err(NeedError::TooManyQuestions);
        }
        for question in &questions {
            if question.is_empty() {
                return Err(NeedError::EmptyQuestion);
            }
            if question.len() > MAX_QUESTION_BYTES {
                return Err(NeedError::QuestionTooLarge);
            }
        }
        if known_anchors.len() > MAX_ANCHORS {
            return Err(NeedError::TooManyAnchors);
        }
        if completeness == CompletenessRequirement::Exhaustive && scope.paths.is_empty() {
            return Err(NeedError::EmptyScopeForExhaustive);
        }
        if token_budget == 0 {
            return Err(NeedError::ZeroBudget);
        }
        Ok(Self {
            questions,
            scope,
            known_anchors,
            completeness,
            need_callers,
            need_tests,
            need_types,
            need_config,
            negative_claim_policy,
            token_budget,
        })
    }

    pub fn questions(&self) -> &[String] {
        &self.questions
    }

    pub fn scope(&self) -> &ScopeSet {
        &self.scope
    }

    pub fn known_anchors(&self) -> &[CodeRef] {
        &self.known_anchors
    }

    pub fn completeness(&self) -> CompletenessRequirement {
        self.completeness
    }

    pub fn need_callers(&self) -> bool {
        self.need_callers
    }

    pub fn need_tests(&self) -> bool {
        self.need_tests
    }

    pub fn need_types(&self) -> bool {
        self.need_types
    }

    pub fn need_config(&self) -> bool {
        self.need_config
    }

    pub fn negative_claim_policy(&self) -> NegativeClaimPolicy {
        self.negative_claim_policy
    }

    pub fn token_budget(&self) -> u32 {
        self.token_budget
    }

    /// Exhaustive needs require checked negative findings.
    pub fn requires_checked_negatives(&self) -> bool {
        self.completeness == CompletenessRequirement::Exhaustive
            || self.negative_claim_policy == NegativeClaimPolicy::RequireCheckedScope
    }
}

fn validate_ref_field(value: &str) -> Result<(), NeedError> {
    if value.is_empty() || value.len() > MAX_REF_BYTES {
        return Err(NeedError::InvalidRef);
    }
    if value.bytes().any(|b| b == 0) {
        return Err(NeedError::InvalidRef);
    }
    Ok(())
}

impl fmt::Display for NeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyQuestions => f.write_str("information need has no questions"),
            Self::EmptyQuestion => f.write_str("information need question is empty"),
            Self::TooManyQuestions => f.write_str("too many information-need questions"),
            Self::QuestionTooLarge => f.write_str("information-need question exceeds bound"),
            Self::TooManyAnchors => f.write_str("too many information-need anchors"),
            Self::TooManyScopePaths => f.write_str("too many information-need scope paths"),
            Self::InvalidRef => f.write_str("invalid information-need path or symbol"),
            Self::EmptyScopeForExhaustive => {
                f.write_str("exhaustive need requires a non-empty scope")
            }
            Self::ZeroBudget => f.write_str("information-need token budget is zero"),
        }
    }
}

impl std::error::Error for NeedError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn representative(questions: Vec<&str>) -> InformationNeed {
        InformationNeed::new(
            questions.into_iter().map(str::to_owned).collect(),
            ScopeSet::empty(),
            Vec::new(),
            CompletenessRequirement::Representative,
            false,
            false,
            false,
            false,
            NegativeClaimPolicy::AllowUnchecked,
            2_048,
        )
        .expect("need")
    }

    #[test]
    fn valid_need_round_trips_fields() {
        let need = InformationNeed::new(
            vec!["where is EventLedger::append defined?".into()],
            ScopeSet::new(vec!["crates/event-ledger".into()]).expect("scope"),
            vec![
                CodeRef::new("crates/event-ledger/src/ledger.rs", Some("append".into()))
                    .expect("anchor"),
            ],
            CompletenessRequirement::Exhaustive,
            true,
            true,
            true,
            false,
            NegativeClaimPolicy::RequireCheckedScope,
            4_096,
        )
        .expect("need");
        assert_eq!(need.questions().len(), 1);
        assert_eq!(need.scope().paths(), &["crates/event-ledger".to_string()]);
        assert_eq!(need.completeness(), CompletenessRequirement::Exhaustive);
        assert!(need.need_callers());
        assert!(need.requires_checked_negatives());
        assert_eq!(need.token_budget(), 4_096);
    }

    #[test]
    fn empty_questions_and_zero_budget_fail_closed() {
        let err = InformationNeed::new(
            Vec::new(),
            ScopeSet::empty(),
            Vec::new(),
            CompletenessRequirement::Representative,
            false,
            false,
            false,
            false,
            NegativeClaimPolicy::AllowUnchecked,
            100,
        )
        .expect_err("empty");
        assert_eq!(err, NeedError::EmptyQuestions);

        let err = InformationNeed::new(
            vec!["q".into()],
            ScopeSet::empty(),
            Vec::new(),
            CompletenessRequirement::Representative,
            false,
            false,
            false,
            false,
            NegativeClaimPolicy::AllowUnchecked,
            0,
        )
        .expect_err("zero");
        assert_eq!(err, NeedError::ZeroBudget);
    }

    #[test]
    fn exhaustive_without_scope_is_rejected() {
        let err = InformationNeed::new(
            vec!["find every caller of append".into()],
            ScopeSet::empty(),
            Vec::new(),
            CompletenessRequirement::Exhaustive,
            true,
            false,
            false,
            false,
            NegativeClaimPolicy::RequireCheckedScope,
            1_000,
        )
        .expect_err("scope required");
        assert_eq!(err, NeedError::EmptyScopeForExhaustive);
    }

    #[test]
    fn representative_need_does_not_require_checked_negatives() {
        let need = representative(vec!["summarize ledger append"]);
        assert!(!need.requires_checked_negatives());
        assert_eq!(need.completeness(), CompletenessRequirement::Representative);
    }
}
