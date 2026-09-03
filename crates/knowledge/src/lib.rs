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
        // `days_idle as i32` wraps negative past `i32::MAX`, which flips
        // `powi` from decaying to blowing up toward infinity — the opposite
        // of "recency keeps preferences honest". Clamp first so an
        // unrealistic (or underflow-derived) `days_idle` still decays.
        let factor = 0.98f64.powi(days_idle.min(i32::MAX as u32) as i32);
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

    /// Merge a candidate: same proposition confirms/contradicts in place,
    /// replaying whatever the incoming candidate itself represents (an
    /// Accept/Edit-sourced candidate carries one observation, a
    /// Reject-sourced one carries one contradiction — see
    /// `PreferenceCandidate::from_feedback`) rather than always confirming.
    pub fn merge(&mut self, candidate: PreferenceCandidate) -> &PreferenceCandidate {
        let index = match self
            .candidates
            .iter()
            .position(|c| c.proposition == candidate.proposition)
        {
            Some(index) => {
                let existing = &mut self.candidates[index];
                for _ in 0..candidate.observations {
                    existing.confirm();
                }
                for _ in 0..candidate.contradictions {
                    existing.contradict();
                }
                index
            }
            None => {
                self.candidates.push(candidate);
                self.candidates.len() - 1
            }
        };
        &self.candidates[index]
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
    fn decay_never_blows_up_past_i32_max_days_idle() {
        // `days_idle as i32` wraps negative past `i32::MAX`, flipping
        // `0.98f64.powi(negative)` from decaying to diverging toward
        // infinity — the clamp must keep it decaying regardless.
        let mut cand = PreferenceCandidate::new("p", "user");
        cand.decay(i32::MAX as u32 + 1);
        assert!(cand.confidence.is_finite());
        assert!((cand.confidence - 0.05).abs() < 1e-9);
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
    fn merge_returns_the_actual_merged_entry_even_when_not_last() {
        let mut store = PreferenceStore::new();
        store.merge(PreferenceCandidate::new("prop-a", "user"));
        store.merge(PreferenceCandidate::new("prop-b", "user"));
        // "prop-a" lives at index 0, not last — merging into it must still
        // return *its* entry, not whatever happens to be last in the vec.
        let merged = store.merge(PreferenceCandidate::new("prop-a", "user"));
        assert_eq!(merged.proposition, "prop-a");
    }

    #[test]
    fn merge_replays_a_rejection_as_a_contradiction_not_a_confirmation() {
        let mut store = PreferenceStore::new();
        store.merge(
            PreferenceCandidate::from_feedback(&FeedbackEvent::new(
                "s",
                FeedbackKind::Accept,
                "always run fmt",
            ))
            .unwrap(),
        );
        let before = store.all()[0].clone();
        assert_eq!(before.observations, 1);
        assert_eq!(before.contradictions, 0);

        let rejected = PreferenceCandidate::from_feedback(&FeedbackEvent::new(
            "s",
            FeedbackKind::Reject,
            "always run fmt",
        ))
        .unwrap();
        store.merge(rejected);
        let after = &store.all()[0];
        // A rejection must land as a contradiction, never an extra
        // confirming observation — merge must not always call `confirm()`
        // regardless of what the incoming candidate actually represents.
        assert_eq!(
            after.observations, before.observations,
            "a reject-sourced merge must not add a confirming observation"
        );
        assert_eq!(after.contradictions, before.contradictions + 1);
        assert!(
            after.confidence < before.confidence,
            "a rejection must lower confidence, not raise or roughly cancel it"
        );
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
