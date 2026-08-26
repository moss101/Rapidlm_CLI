//! Delegation utility scoring and host-side enforcement (agent-harness §5).
//!
//! Two layers, kept separate and sharing one source of nesting configuration:
//!
//! - **P5-018 scoring** — a pure, deterministic `DelegationScore` and a
//!   `recommend` decision. Balance quality/verification reward against context
//!   duplication, merge-conflict risk, latency and spawn cost. This is a hint,
//!   never authority.
//! - **P5-019 host/runtime enforcement** — `enforce_delegation` applies the
//!   canonical `DelegationPolicy` (depth, budget, workspace-safety) *around* the
//!   scoring decision. It is the layer that actually permits spawn / keep-local
//!   for a candidate delegation, and is the single nesting-depth boundary.
//!
//! There is **one** canonical depth cap (`DELEGATION_MAX_DEPTH`), consumed here
//! by `DelegationPolicy::standard()`; the host must not define a second one.

/// Canonical maximum nesting depth. Single source for the whole stack.
pub const DELEGATION_MAX_DEPTH: u32 = 3;

/// Default score at or above which scoring recommends delegation.
pub const DELEGATION_DEFAULT_THRESHOLD: i32 = 60;

/// Default max spawn cost (µs dollars) the enforcement layer allows.
pub const DELEGATION_DEFAULT_MAX_SPAWN_COST: u64 = 2_000_000;

/// Default max merge-conflict risk (0..=100) the enforcement layer allows.
pub const DELEGATION_DEFAULT_MAX_CONFLICT_RISK: u8 = 80;

/// Input factors for one delegation evaluation. All weights are 0..=100.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegationInput {
    expected_quality_gain: u8,
    verification_independence: u8,
    context_duplication: u8,
    merge_conflict_risk: u8,
    latency_cost_ms: u64,
    spawn_cost_usd_micros: u64,
}

/// Canonical nesting/budget policy applied by the host enforcement layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegationPolicy {
    max_depth: u32,
    max_spawn_cost_usd_micros: u64,
    max_merge_conflict_risk: u8,
}

/// Bounded delegation score in 0..=100.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegationScore(u8);

/// Pure scoring decision (no depth/budget enforcement).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScoringRecommendation {
    Delegate,
    KeepLocal,
}

/// Host/runtime enforcement result for one candidate delegation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnforcementDecision {
    Spawn,
    KeepLocal,
    DepthExceeded,
    BudgetExceeded,
    Unsafe,
}

impl DelegationInput {
    pub fn new(
        expected_quality_gain: u8,
        verification_independence: u8,
        context_duplication: u8,
        merge_conflict_risk: u8,
        latency_cost_ms: u64,
        spawn_cost_usd_micros: u64,
    ) -> Self {
        Self {
            expected_quality_gain: clamp100(expected_quality_gain),
            verification_independence: clamp100(verification_independence),
            context_duplication: clamp100(context_duplication),
            merge_conflict_risk: clamp100(merge_conflict_risk),
            latency_cost_ms,
            spawn_cost_usd_micros,
        }
    }
}

impl DelegationPolicy {
    pub const fn standard() -> Self {
        Self {
            max_depth: DELEGATION_MAX_DEPTH,
            max_spawn_cost_usd_micros: DELEGATION_DEFAULT_MAX_SPAWN_COST,
            max_merge_conflict_risk: DELEGATION_DEFAULT_MAX_CONFLICT_RISK,
        }
    }

    pub fn with_max_depth(mut self, max_depth: u32) -> Self {
        if max_depth == 0 {
            self.max_depth = 1;
        } else {
            self.max_depth = max_depth;
        }
        self
    }

    pub const fn max_depth(self) -> u32 {
        self.max_depth
    }

    pub const fn max_spawn_cost_usd_micros(self) -> u64 {
        self.max_spawn_cost_usd_micros
    }

    pub const fn max_merge_conflict_risk(self) -> u8 {
        self.max_merge_conflict_risk
    }
}

impl DelegationScore {
    pub const fn value(self) -> u8 {
        self.0
    }
}

impl ScoringRecommendation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delegate => "delegate",
            Self::KeepLocal => "keep_local",
        }
    }
}

impl EnforcementDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::KeepLocal => "keep_local",
            Self::DepthExceeded => "depth_exceeded",
            Self::BudgetExceeded => "budget_exceeded",
            Self::Unsafe => "unsafe",
        }
    }
}

/// Compute the bounded utility score. Higher is better (0..=100).
///
/// Reward: 60% expected quality gain + 40% verification independence, so the
/// ideal case reaches 1.0. Penalty: 25% context duplication + 25% merge-conflict
/// risk, plus latency and spawn-cost terms that saturate at 20% each. The sum is
/// clamped to 0..=100, so a worst case never goes below 0. Deterministic and
/// NaN-free for all inputs.
pub fn evaluate_delegation(input: &DelegationInput) -> DelegationScore {
    let reward =
        0.60 * unit(input.expected_quality_gain) + 0.40 * unit(input.verification_independence);
    let penalty = 0.25 * unit(input.context_duplication) + 0.25 * unit(input.merge_conflict_risk);
    let latency = small_penalty(input.latency_cost_ms);
    let spawn = small_penalty(input.spawn_cost_usd_micros);
    let score = (reward - penalty - latency - spawn).clamp(0.0, 1.0);
    DelegationScore((score * 100.0).round() as u8)
}

/// Pure scoring recommendation. `threshold` is in 0..=100.
pub fn recommend(input: &DelegationInput, threshold: i32) -> ScoringRecommendation {
    if (evaluate_delegation(input).value() as i32) >= threshold {
        ScoringRecommendation::Delegate
    } else {
        ScoringRecommendation::KeepLocal
    }
}

/// Host/runtime enforcement around `recommend`.
///
/// Order: depth bound → budget bound → workspace-safety (conflict-risk) bound →
/// scoring recommendation. `scope_so_far` is the current nesting depth, so a
/// candidate is refused outright at the canonical cap regardless of its score,
/// and no second depth limit is consulted anywhere.
pub fn enforce_delegation(
    input: &DelegationInput,
    scope_so_far: u32,
    policy: &DelegationPolicy,
) -> EnforcementDecision {
    if scope_so_far >= policy.max_depth {
        return EnforcementDecision::DepthExceeded;
    }
    if input.spawn_cost_usd_micros > policy.max_spawn_cost_usd_micros {
        return EnforcementDecision::BudgetExceeded;
    }
    if input.merge_conflict_risk > policy.max_merge_conflict_risk {
        return EnforcementDecision::Unsafe;
    }
    match recommend(input, DELEGATION_DEFAULT_THRESHOLD) {
        ScoringRecommendation::Delegate => EnforcementDecision::Spawn,
        ScoringRecommendation::KeepLocal => EnforcementDecision::KeepLocal,
    }
}

fn unit(value: u8) -> f64 {
    f64::from(value.clamp(0, 100)) / 100.0
}

/// Saturating penalty for latency/spawn, up to 0.20 of the score.
fn small_penalty(raw: u64) -> f64 {
    const CAP: f64 = 0.20;
    if raw == 0 {
        return 0.0;
    }
    (unit_axis(raw) * CAP).min(CAP)
}

fn unit_axis(raw: u64) -> f64 {
    if raw == 0 {
        return 0.0;
    }
    (raw as f64 / 1_000_000.0).min(1.0)
}

fn clamp100(value: u8) -> u8 {
    if value > 100 { 100 } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn high_value() -> DelegationInput {
        DelegationInput::new(90, 80, 20, 10, 500, 50_000)
    }

    fn low_value() -> DelegationInput {
        DelegationInput::new(10, 5, 90, 80, 400_000, 900_000)
    }

    #[test]
    fn high_value_delegation_scores_above_default_threshold() {
        let input = high_value();
        let score = evaluate_delegation(&input);
        assert!((score.value() as i32) >= DELEGATION_DEFAULT_THRESHOLD);
        assert_eq!(
            recommend(&input, DELEGATION_DEFAULT_THRESHOLD),
            ScoringRecommendation::Delegate
        );
    }

    #[test]
    fn low_value_delegation_scores_below_threshold() {
        let input = low_value();
        let score = evaluate_delegation(&input);
        assert!((score.value() as i32) < DELEGATION_DEFAULT_THRESHOLD);
        assert_eq!(
            recommend(&input, DELEGATION_DEFAULT_THRESHOLD),
            ScoringRecommendation::KeepLocal
        );
    }

    #[test]
    fn score_is_bounded_from_below_and_above_and_deterministic() {
        let worst = DelegationInput::new(0, 0, 100, 100, 9_000_000, 9_000_000);
        assert_eq!(evaluate_delegation(&worst).value(), 0);
        let best = DelegationInput::new(100, 100, 0, 0, 0, 0);
        assert_eq!(evaluate_delegation(&best).value(), 100);
        assert_eq!(
            evaluate_delegation(&high_value()),
            evaluate_delegation(&high_value())
        );
    }

    #[test]
    fn host_enforcement_uses_the_canonical_depth_cap_and_cannot_be_bypassed() {
        let input = high_value();
        let policy = DelegationPolicy::standard();
        // At the canonical cap the host refuses outright, even though scoring
        // recommends Delegate — a model cannot bypass the limit by re-requesting
        // a spawn path through the scoring layer.
        assert_eq!(
            enforce_delegation(&input, policy.max_depth, &policy),
            EnforcementDecision::DepthExceeded
        );
        assert_eq!(
            enforce_delegation(&input, DELEGATION_MAX_DEPTH, &policy),
            EnforcementDecision::DepthExceeded
        );
        // Inside the cap a high-value, affordable, safe candidate spawns.
        assert_eq!(
            enforce_delegation(&input, 0, &policy),
            EnforcementDecision::Spawn
        );
    }

    #[test]
    fn host_enforcement_respects_budget_and_workspace_safety() {
        let policy = DelegationPolicy::standard();
        let expensive = DelegationInput::new(90, 80, 10, 10, 500, 3_000_000);
        assert_eq!(
            enforce_delegation(&expensive, 0, &policy),
            EnforcementDecision::BudgetExceeded
        );
        let risky = DelegationInput::new(90, 80, 10, 95, 500, 50_000);
        assert_eq!(
            enforce_delegation(&risky, 0, &policy),
            EnforcementDecision::Unsafe
        );
    }

    #[test]
    fn one_depth_source_exists_and_is_used_by_the_standard_policy() {
        let policy = DelegationPolicy::standard();
        assert_eq!(policy.max_depth(), DELEGATION_MAX_DEPTH);
        // A lower cap still uses the same canonical field, not a second constant.
        let narrow = DelegationPolicy::standard().with_max_depth(1);
        assert_eq!(narrow.max_depth(), 1);
        assert_eq!(
            enforce_delegation(&high_value(), 1, &narrow),
            EnforcementDecision::DepthExceeded
        );
    }

    #[test]
    fn context_duplication_and_conflict_reduce_score() {
        let cheap = DelegationInput::new(80, 70, 10, 10, 300, 30_000);
        let expensive = DelegationInput::new(80, 70, 90, 90, 300, 30_000);
        assert!(evaluate_delegation(&expensive).value() < evaluate_delegation(&cheap).value());
    }
}
