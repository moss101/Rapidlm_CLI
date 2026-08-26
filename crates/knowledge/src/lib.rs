#![forbid(unsafe_code)]
//! P11-020..025/027: feedback events, preference candidates, decay/conflict,
//! candidate ranking, experiment registry + held-out promotion gates.

/// Operator feedback on an outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedbackKind {
    Accept,
    Reject,
    Edit,
}

/// One feedback event over observable outcome data only.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedbackEvent {
    pub session_id: String,
    pub kind: FeedbackKind,
    /// Edited statement (Edit) or accepted/rejected statement text.
    pub statement: String,
}

impl FeedbackEvent {
    pub fn new(session_id: &str, kind: FeedbackKind, statement: &str) -> Self {
        Self {
            session_id: session_id.to_owned(),
            kind,
            statement: statement.to_owned(),
        }
    }
}

/// A learned preference candidate with lifecycle metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct PreferenceCandidate {
    pub proposition: String,
    pub scope: &'static str,
    pub confidence: f64,
    pub observations: u32,
    pub contradictions: u32,
}

const CONFIRM_GAIN: f64 = 0.1;
const CONTRADICT_LOSS: f64 = 0.2;

impl PreferenceCandidate {
    pub fn new(proposition: &str, scope: &'static str) -> Self {
        Self {
            proposition: proposition.to_owned(),
            scope,
            confidence: 0.3,
            observations: 0,
            contradictions: 0,
        }
    }

    /// Confirming observation raises confidence toward the 0.95 ceiling.
    pub fn confirm(&mut self) {
        self.observations += 1;
        self.confidence = (self.confidence + CONFIRM_GAIN).min(0.95);
    }

    /// Contradiction reduces confidence (never overwrites history).
    pub fn contradict(&mut self) {
        self.contradictions += 1;
        self.confidence = (self.confidence - CONTRADICT_LOSS).max(0.05);
    }

    /// Time-based exponential decay; recency keeps preferences honest.
    pub fn decay(&mut self, days_idle: u32) {
        let factor = 0.98f64.powi(days_idle as i32);
        self.confidence = (self.confidence * factor).max(0.05);
    }

    /// Extract a candidate from operator feedback. Rejects lower confidence.
    pub fn from_feedback(feedback: &FeedbackEvent) -> Option<Self> {
        if feedback.statement.trim().is_empty() || feedback.statement.len() > 256 {
            return None;
        }
        let mut candidate = Self::new(feedback.statement.trim(), "session");
        match feedback.kind {
            FeedbackKind::Accept => candidate.confirm(),
            FeedbackKind::Reject => candidate.contradict(),
            FeedbackKind::Edit => {
                // An edit is one confirmation of the edited form.
                candidate.confirm();
            }
        }
        Some(candidate)
    }
}

/// Preference store with contradiction handling across candidates.
#[derive(Default)]
pub struct PreferenceStore {
    candidates: Vec<PreferenceCandidate>,
}

impl PreferenceStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge a candidate: same proposition confirms/contradicts in place.
    pub fn merge(&mut self, candidate: PreferenceCandidate) -> &PreferenceCandidate {
        if let Some(existing) = self
            .candidates
            .iter_mut()
            .find(|c| c.proposition == candidate.proposition)
        {
            existing.confirm();
            for _ in 0..candidate.contradictions {
                existing.contradict();
            }
        } else {
            self.candidates.push(candidate);
        }
        self.candidates.last().unwrap()
    }

    pub fn all(&self) -> &[PreferenceCandidate] {
        &self.candidates
    }

    /// Apply daily decay to every stored preference.
    pub fn apply_decay(&mut self, days_idle: u32) {
        for c in &mut self.candidates {
            c.decay(days_idle);
        }
    }
}

/// Rank candidates: confidence desc, then fewer contradictions, then prop asc.
pub fn rank(candidates: &[PreferenceCandidate]) -> Vec<&PreferenceCandidate> {
    let mut sorted: Vec<&PreferenceCandidate> = candidates.iter().collect();
    sorted.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.contradictions.cmp(&b.contradictions))
            .then(a.proposition.cmp(&b.proposition))
    });
    sorted
}

/// Promotion decision from held-out gates. Hard gates run first.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PromotionDecision {
    Promoted { score: f64 },
    RejectedHardGate { gate: &'static str },
    RejectedRanking { score: f64 },
}

pub struct ExperimentRegistry;

impl ExperimentRegistry {
    const MIN_HELDOUT_SCORE: f64 = 0.8;
    const MIN_RANK_SCORE: f64 = 0.6;

    /// Held-out promotion gate: hard correctness/security gates run BEFORE
    /// weighted ranking. `heldout_score` is measured on held-out scenarios;
    /// `security_clean` and `correctness_clean` are hard booleans.
    pub fn promote(
        heldout_score: f64,
        rank_score: f64,
        security_clean: bool,
        correctness_clean: bool,
    ) -> PromotionDecision {
        if !security_clean {
            return PromotionDecision::RejectedHardGate { gate: "security" };
        }
        if !correctness_clean {
            return PromotionDecision::RejectedHardGate { gate: "correctness" };
        }
        if heldout_score < Self::MIN_HELDOUT_SCORE {
            return PromotionDecision::RejectedRanking {
                score: heldout_score,
            };
        }
        if rank_score < Self::MIN_RANK_SCORE {
            return PromotionDecision::RejectedRanking { score: rank_score };
        }
        PromotionDecision::Promoted {
            score: heldout_score,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_maps_to_candidates_and_contradiction_reduces_confidence() {
        let accept = FeedbackEvent::new("s1", FeedbackKind::Accept, "prefer rustfmt two-space");
        let mut cand = PreferenceCandidate::from_feedback(&accept).expect("candidate");
        let start = cand.confidence;
        cand.confirm();
        assert!(cand.confidence > start);
        cand.contradict();
        cand.contradict();
        assert!(
            cand.confidence < start,
            "contradictions reduce confidence, never overwrite history"
        );
        // Reject feedback starts below baseline.
        let rejected =
            PreferenceCandidate::from_feedback(&FeedbackEvent::new("s1", FeedbackKind::Reject, "x"))
                .unwrap();
        assert!((rejected.confidence - 0.1).abs() < 1e-9);
        // Empty statements are not learnable.
        assert!(PreferenceCandidate::from_feedback(&FeedbackEvent::new("s", FeedbackKind::Accept, "")).is_none());
    }

    #[test]
    fn store_merges_by_proposition_and_decay_lowers_stale_preferences() {
        let mut store = PreferenceStore::new();
        store.merge(PreferenceCandidate::from_feedback(
            &FeedbackEvent::new("s", FeedbackKind::Accept, "always run fmt"),
        ).unwrap());
        let before = store.all()[0].confidence;
        store.apply_decay(30);
        assert!(store.all()[0].confidence < before, "idle preferences decay");
        // Same proposition merges (confirm), distinct propositions append.
        store.merge(PreferenceCandidate::from_feedback(
            &FeedbackEvent::new("s", FeedbackKind::Accept, "always run fmt"),
        ).unwrap());
        assert_eq!(store.all().len(), 1);
        store.merge(PreferenceCandidate::from_feedback(
            &FeedbackEvent::new("s", FeedbackKind::Accept, "never force push"),
        ).unwrap());
        assert_eq!(store.all().len(), 2);
    }

    #[test]
    fn ranking_orders_confidence_desc_then_fewer_contradictions() {
        let mut high = PreferenceCandidate::new("a", "user");
        high.confirm();
        high.confirm();
        let mut low = PreferenceCandidate::new("b", "user");
        low.contradict();
        let candidates = vec![low, high, PreferenceCandidate::new("c", "user")];
        let ranked = rank(&candidates);
        assert_eq!(ranked[0].proposition, "a");
        assert_eq!(ranked.last().unwrap().proposition, "b");
    }

    #[test]
    fn promotion_runs_hard_gates_before_ranking() {
        assert_eq!(
            ExperimentRegistry::promote(0.9, 0.9, false, true),
            PromotionDecision::RejectedHardGate { gate: "security" }
        );
        assert_eq!(
            ExperimentRegistry::promote(0.9, 0.9, true, false),
            PromotionDecision::RejectedHardGate { gate: "correctness" }
        );
        assert_eq!(
            ExperimentRegistry::promote(0.5, 0.9, true, true),
            PromotionDecision::RejectedRanking { score: 0.5 }
        );
        assert_eq!(
            ExperimentRegistry::promote(0.9, 0.7, true, true),
            PromotionDecision::Promoted { score: 0.9 }
        );
    }
}
