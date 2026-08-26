//! Goal budget tracking. Unset ceilings stay unset; paused time does not accrue.
//!
//! Usage is recorded only while the goal is [`GoalState::Active`]. Exhaustion is
//! a typed blocker. Model text cannot invent limits or override these checks.

use std::error::Error;
use std::fmt;

use protocol::ErrorCode;

use crate::agent::model::CancellationToken;
use crate::goal::state::{GoalBudget, GoalSnapshot, GoalState, GoalUsage};

/// Share of any configured ceiling that emits converge guidance.
pub const CONVERGENCE_NUMERATOR: u64 = 3;

/// Denominator for [`CONVERGENCE_NUMERATOR`] (75%).
pub const CONVERGENCE_DENOMINATOR: u64 = 4;

/// Configured budget axis. Unset axes are never compared.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum BudgetDimension {
    Turns,
    Tokens,
    ActiveMs,
    Cost,
}

/// Axes at or above 75% of an explicit ceiling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct ConvergenceHint {
    turns: bool,
    tokens: bool,
    active_ms: bool,
    cost: bool,
}

/// Result of a budget check. Exhaustion is a blocker, not a silent continue.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GoalBudgetOutcome {
    Continue {
        usage: GoalUsage,
        hint: Option<ConvergenceHint>,
    },
    Exhausted {
        dimension: BudgetDimension,
        usage: GoalUsage,
        hint: Option<ConvergenceHint>,
    },
}

/// Typed budget-guard failure. Display never echoes goal statement text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GoalBudgetError {
    Cancelled,
}

/// Tracks continuation turns, tokens, active wall-clock, and cost.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalBudgetGuard {
    budget: GoalBudget,
    usage: GoalUsage,
    state: GoalState,
}

impl BudgetDimension {
    pub const ALL: &'static [Self] = &[Self::Turns, Self::Tokens, Self::ActiveMs, Self::Cost];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Turns => "turns",
            Self::Tokens => "tokens",
            Self::ActiveMs => "active_ms",
            Self::Cost => "cost",
        }
    }
}

impl ConvergenceHint {
    pub const fn empty() -> Self {
        Self {
            turns: false,
            tokens: false,
            active_ms: false,
            cost: false,
        }
    }

    /// Hint only when at least one *configured* ceiling is ≥75% consumed.
    pub fn from_budget(budget: GoalBudget, usage: GoalUsage) -> Option<Self> {
        let hint = Self {
            turns: crossed_threshold(usage.turns(), budget.max_turns()),
            tokens: crossed_threshold(usage.tokens(), budget.max_tokens()),
            active_ms: crossed_threshold(usage.active_ms(), budget.max_active_ms()),
            cost: crossed_threshold(usage.cost(), budget.max_cost()),
        };
        if hint.is_empty() { None } else { Some(hint) }
    }

    pub const fn is_empty(self) -> bool {
        !self.turns && !self.tokens && !self.active_ms && !self.cost
    }

    pub const fn turns(self) -> bool {
        self.turns
    }

    pub const fn tokens(self) -> bool {
        self.tokens
    }

    pub const fn active_ms(self) -> bool {
        self.active_ms
    }

    pub const fn cost(self) -> bool {
        self.cost
    }

    pub const fn contains(self, dimension: BudgetDimension) -> bool {
        match dimension {
            BudgetDimension::Turns => self.turns,
            BudgetDimension::Tokens => self.tokens,
            BudgetDimension::ActiveMs => self.active_ms,
            BudgetDimension::Cost => self.cost,
        }
    }
}

impl GoalBudgetOutcome {
    pub const fn usage(self) -> GoalUsage {
        match self {
            Self::Continue { usage, .. } | Self::Exhausted { usage, .. } => usage,
        }
    }

    pub const fn hint(self) -> Option<ConvergenceHint> {
        match self {
            Self::Continue { hint, .. } | Self::Exhausted { hint, .. } => hint,
        }
    }

    pub const fn is_exhausted(self) -> bool {
        matches!(self, Self::Exhausted { .. })
    }

    /// Budget-exhausted blocker dimension, if this step must stop the goal.
    pub const fn blocker(self) -> Option<BudgetDimension> {
        match self {
            Self::Exhausted { dimension, .. } => Some(dimension),
            Self::Continue { .. } => None,
        }
    }

    pub const fn code(self) -> Option<ErrorCode> {
        match self {
            Self::Exhausted { .. } => Some(ErrorCode::GoalBudgetExhausted),
            Self::Continue { .. } => None,
        }
    }
}

impl GoalBudgetError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
        }
    }

    pub const fn code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
        }
    }
}

impl GoalBudgetGuard {
    pub const fn new(budget: GoalBudget, usage: GoalUsage, state: GoalState) -> Self {
        Self {
            budget,
            usage,
            state,
        }
    }

    pub fn from_snapshot(snapshot: &GoalSnapshot) -> Self {
        Self::new(snapshot.budget(), snapshot.usage(), snapshot.state())
    }

    pub const fn budget(&self) -> GoalBudget {
        self.budget
    }

    pub const fn usage(&self) -> GoalUsage {
        self.usage
    }

    pub const fn state(&self) -> GoalState {
        self.state
    }

    /// Replace ceilings. Absent fields stay unset; none are invented.
    pub fn set_budget(&mut self, budget: GoalBudget) {
        self.budget = budget;
    }

    pub fn set_state(&mut self, state: GoalState) {
        self.state = state;
    }

    /// Copy observed usage onto a snapshot. Ceilings stay on the snapshot budget.
    pub fn apply(&self, snapshot: GoalSnapshot) -> GoalSnapshot {
        snapshot.with_usage(self.usage)
    }

    /// Pre-turn check. Does not reserve or accrue a continuation turn.
    pub fn before_turn(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<GoalBudgetOutcome, GoalBudgetError> {
        check_cancel(cancel)?;
        Ok(self.evaluate())
    }

    /// Accrue known model tokens/cost only while the goal is active.
    pub fn after_model(
        &mut self,
        tokens: u64,
        cost: u64,
        cancel: &CancellationToken,
    ) -> Result<GoalBudgetOutcome, GoalBudgetError> {
        check_cancel(cancel)?;
        self.accrue(0, tokens, 0, cost);
        Ok(self.evaluate())
    }

    /// Accrue one continuation turn and supplied active wall-clock, if active.
    ///
    /// `active_ms` is ignored while paused or blocked so parked time cannot
    /// consume the wall-clock ceiling.
    pub fn after_turn(
        &mut self,
        active_ms: u64,
        cancel: &CancellationToken,
    ) -> Result<GoalBudgetOutcome, GoalBudgetError> {
        check_cancel(cancel)?;
        self.accrue(1, 0, active_ms, 0);
        Ok(self.evaluate())
    }

    fn accrue(&mut self, turns: u64, tokens: u64, active_ms: u64, cost: u64) {
        if self.state != GoalState::Active {
            return;
        }
        self.usage = GoalUsage::new(
            self.usage.turns().saturating_add(turns),
            self.usage.tokens().saturating_add(tokens),
            self.usage.active_ms().saturating_add(active_ms),
            self.usage.cost().saturating_add(cost),
        );
    }

    fn evaluate(&self) -> GoalBudgetOutcome {
        let hint = ConvergenceHint::from_budget(self.budget, self.usage);
        match first_exhausted(self.budget, self.usage) {
            Some(dimension) => GoalBudgetOutcome::Exhausted {
                dimension,
                usage: self.usage,
                hint,
            },
            None => GoalBudgetOutcome::Continue {
                usage: self.usage,
                hint,
            },
        }
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), GoalBudgetError> {
    if cancel.is_cancelled() {
        Err(GoalBudgetError::Cancelled)
    } else {
        Ok(())
    }
}

fn first_exhausted(budget: GoalBudget, usage: GoalUsage) -> Option<BudgetDimension> {
    for (used, limit, dimension) in dimension_values(budget, usage) {
        if let Some(limit) = limit {
            if used >= limit {
                return Some(dimension);
            }
        }
    }
    None
}

fn crossed_threshold(used: u64, limit: Option<u64>) -> bool {
    let Some(limit) = limit else {
        return false;
    };
    u128::from(used) * u128::from(CONVERGENCE_DENOMINATOR)
        >= u128::from(limit) * u128::from(CONVERGENCE_NUMERATOR)
}

fn dimension_values(
    budget: GoalBudget,
    usage: GoalUsage,
) -> [(u64, Option<u64>, BudgetDimension); 4] {
    [
        (usage.turns(), budget.max_turns(), BudgetDimension::Turns),
        (usage.tokens(), budget.max_tokens(), BudgetDimension::Tokens),
        (
            usage.active_ms(),
            budget.max_active_ms(),
            BudgetDimension::ActiveMs,
        ),
        (usage.cost(), budget.max_cost(), BudgetDimension::Cost),
    ]
}

impl fmt::Display for BudgetDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for GoalBudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "goal budget check cancelled",
        })
    }
}

impl Error for GoalBudgetError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use protocol::GoalId;

    use crate::goal::state::{GoalActor, GoalCommand, GoalSpec, GoalStateMachine, GoalStopReason};

    const GOAL_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn goal_id() -> GoalId {
        GOAL_ID.parse().expect("goal id")
    }

    fn spec(budget: GoalBudget) -> GoalSpec {
        GoalSpec::new(goal_id(), "ship auth", Vec::new(), budget, Vec::new()).expect("spec")
    }

    fn snapshot(budget: GoalBudget) -> GoalSnapshot {
        let mut machine = GoalStateMachine::new();
        machine
            .apply(GoalCommand::Create(spec(budget)), &GoalActor::Human)
            .expect("create");
        machine.snapshot().expect("snapshot").clone()
    }

    fn paused(budget: GoalBudget) -> GoalSnapshot {
        let mut machine = GoalStateMachine::from_snapshot(snapshot(budget));
        machine
            .apply(
                GoalCommand::Pause {
                    goal_id: goal_id(),
                    process_recovered: false,
                },
                &GoalActor::Human,
            )
            .expect("pause");
        machine.snapshot().expect("snapshot").clone()
    }

    fn blocked(budget: GoalBudget) -> GoalSnapshot {
        let mut machine = GoalStateMachine::from_snapshot(snapshot(budget));
        machine
            .apply(
                GoalCommand::Block {
                    goal_id: goal_id(),
                    budget_exhausted: false,
                },
                &GoalActor::Human,
            )
            .expect("block");
        machine.snapshot().expect("snapshot").clone()
    }

    #[test]
    fn unset_budget_is_not_invented_and_never_hints() {
        let budget = GoalBudget::default();
        assert_eq!(budget.max_turns(), None);
        assert_eq!(budget.max_tokens(), None);
        assert_eq!(budget.max_active_ms(), None);
        assert_eq!(budget.max_cost(), None);

        let mut guard = GoalBudgetGuard::from_snapshot(&snapshot(budget));
        for _ in 0..8 {
            let outcome = guard.after_turn(1_000, &live()).expect("turn");
            assert!(!outcome.is_exhausted(), "unset ceilings cannot exhaust");
            assert_eq!(outcome.hint(), None);
            assert_eq!(outcome.blocker(), None);
            assert_eq!(outcome.code(), None);
        }
        let outcome = guard.after_model(50_000, 99, &live()).expect("model");
        assert_eq!(guard.usage().turns(), 8);
        assert_eq!(guard.usage().tokens(), 50_000);
        assert_eq!(guard.usage().active_ms(), 8_000);
        assert_eq!(guard.usage().cost(), 99);
        assert_eq!(outcome.hint(), None);
        assert!(!outcome.is_exhausted());
    }

    #[test]
    fn paused_and_blocked_elapsed_time_does_not_accrue() {
        let budget = GoalBudget::new(Some(10), Some(100), Some(1_000), Some(50));
        let mut paused_guard = GoalBudgetGuard::from_snapshot(&paused(budget));
        let paused_out = paused_guard.after_turn(9_000, &live()).expect("paused");
        assert_eq!(paused_guard.usage(), GoalUsage::default());
        assert!(!paused_out.is_exhausted());
        assert_eq!(paused_out.hint(), None);
        paused_guard
            .after_model(80, 40, &live())
            .expect("paused model");
        assert_eq!(paused_guard.usage(), GoalUsage::default());

        let mut blocked_guard = GoalBudgetGuard::from_snapshot(&blocked(budget));
        blocked_guard.after_turn(9_000, &live()).expect("blocked");
        blocked_guard
            .after_model(80, 40, &live())
            .expect("blocked model");
        assert_eq!(blocked_guard.usage(), GoalUsage::default());
        assert_eq!(blocked_guard.state(), GoalState::Blocked);
    }

    #[test]
    fn active_usage_accrues_and_writes_back() {
        let snap = snapshot(GoalBudget::new(Some(4), None, Some(10_000), None));
        let mut guard = GoalBudgetGuard::from_snapshot(&snap);
        guard.after_model(12, 0, &live()).expect("model");
        let outcome = guard.after_turn(250, &live()).expect("turn");
        assert_eq!(outcome.usage().turns(), 1);
        assert_eq!(outcome.usage().tokens(), 12);
        assert_eq!(outcome.usage().active_ms(), 250);
        let written = guard.apply(snap);
        assert_eq!(written.usage().turns(), 1);
        assert_eq!(written.usage().tokens(), 12);
        assert_eq!(written.budget().max_turns(), Some(4));
        assert_eq!(written.state(), GoalState::Active);
    }

    #[test]
    fn seventy_five_percent_returns_convergence_hint() {
        let mut guard = GoalBudgetGuard::from_snapshot(&snapshot(GoalBudget::new(
            Some(4),
            Some(100),
            Some(1_000),
            Some(40),
        )));
        let below = guard.after_model(74, 0, &live()).expect("below");
        assert_eq!(below.hint(), None);
        assert!(!below.is_exhausted());

        let hinted = guard.after_model(1, 0, &live()).expect("hint");
        let hint = hinted.hint().expect("convergence hint");
        assert!(hint.tokens());
        assert!(!hint.turns());
        assert!(!hint.active_ms());
        assert!(!hint.cost());
        assert!(hint.contains(BudgetDimension::Tokens));
        assert!(!hinted.is_exhausted());

        let turns = guard.after_turn(750, &live()).expect("turn");
        let hint = turns.hint().expect("multi hint");
        assert!(hint.tokens());
        assert!(hint.active_ms());
        assert!(!hint.turns());
        assert!(!hint.cost());
    }

    #[test]
    fn before_turn_after_model_after_turn_return_exhausted_blocker() {
        let mut turns = GoalBudgetGuard::new(
            GoalBudget::new(Some(2), None, None, None),
            GoalUsage::new(2, 0, 0, 0),
            GoalState::Active,
        );
        let blocked = turns.before_turn(&live()).expect("before");
        assert_eq!(blocked.blocker(), Some(BudgetDimension::Turns));
        assert_eq!(blocked.code(), Some(ErrorCode::GoalBudgetExhausted));
        assert!(
            blocked
                .hint()
                .expect("hint")
                .contains(BudgetDimension::Turns)
        );

        let mut tokens =
            GoalBudgetGuard::from_snapshot(&snapshot(GoalBudget::new(None, Some(10), None, None)));
        let blocked = tokens.after_model(10, 0, &live()).expect("tokens");
        assert_eq!(blocked.blocker(), Some(BudgetDimension::Tokens));

        let mut cost =
            GoalBudgetGuard::from_snapshot(&snapshot(GoalBudget::new(None, None, None, Some(5))));
        let blocked = cost.after_model(0, 5, &live()).expect("cost");
        assert_eq!(blocked.blocker(), Some(BudgetDimension::Cost));

        let mut time =
            GoalBudgetGuard::from_snapshot(&snapshot(GoalBudget::new(None, None, Some(100), None)));
        let blocked = time.after_turn(100, &live()).expect("time");
        assert_eq!(blocked.blocker(), Some(BudgetDimension::ActiveMs));
        assert_eq!(blocked.usage().turns(), 1);
        assert_eq!(blocked.usage().active_ms(), 100);
    }

    #[test]
    fn zero_ceiling_is_exhausted_not_unlimited() {
        let mut guard =
            GoalBudgetGuard::from_snapshot(&snapshot(GoalBudget::new(Some(0), None, None, None)));
        let outcome = guard.before_turn(&live()).expect("zero");
        assert_eq!(outcome.blocker(), Some(BudgetDimension::Turns));
        assert!(outcome.hint().expect("hint").turns());
    }

    #[test]
    fn resume_starts_accrual_pause_stops_it() {
        let budget = GoalBudget::new(None, None, Some(10_000), None);
        let mut guard = GoalBudgetGuard::from_snapshot(&snapshot(budget));
        guard.after_turn(40, &live()).expect("active");
        assert_eq!(guard.usage().active_ms(), 40);
        assert_eq!(guard.usage().turns(), 1);

        guard.set_state(GoalState::Paused);
        guard.after_turn(9_000, &live()).expect("paused");
        assert_eq!(guard.usage().active_ms(), 40);
        assert_eq!(guard.usage().turns(), 1);

        guard.set_state(GoalState::Active);
        guard.after_turn(10, &live()).expect("resumed");
        assert_eq!(guard.usage().active_ms(), 50);
        assert_eq!(guard.usage().turns(), 2);
    }

    #[test]
    fn set_budget_does_not_fill_missing_ceilings() {
        let mut guard = GoalBudgetGuard::from_snapshot(&snapshot(GoalBudget::default()));
        guard.set_budget(GoalBudget::new(Some(3), None, None, None));
        assert_eq!(guard.budget().max_turns(), Some(3));
        assert_eq!(guard.budget().max_tokens(), None);
        assert_eq!(guard.budget().max_active_ms(), None);
        assert_eq!(guard.budget().max_cost(), None);
        guard.after_turn(0, &live()).expect("t1");
        guard.after_turn(0, &live()).expect("t2");
        let last = guard.after_turn(0, &live()).expect("t3");
        assert_eq!(last.blocker(), Some(BudgetDimension::Turns));
    }

    #[test]
    fn cancelled_checks_do_not_accrue() {
        let cancel = live();
        cancel.cancel();
        let mut guard = GoalBudgetGuard::from_snapshot(&snapshot(GoalBudget::new(
            Some(1),
            Some(1),
            Some(1),
            Some(1),
        )));
        assert_eq!(
            guard.before_turn(&cancel).expect_err("before"),
            GoalBudgetError::Cancelled
        );
        assert_eq!(
            guard.after_model(9, 9, &cancel).expect_err("model"),
            GoalBudgetError::Cancelled
        );
        assert_eq!(
            guard.after_turn(9, &cancel).expect_err("turn"),
            GoalBudgetError::Cancelled
        );
        assert_eq!(guard.usage(), GoalUsage::default());
        assert_eq!(GoalBudgetError::Cancelled.code(), None);
        assert_eq!(
            GoalBudgetError::Cancelled.to_string(),
            "goal budget check cancelled"
        );
    }

    #[test]
    fn process_recovered_pause_does_not_accrue() {
        let mut machine =
            GoalStateMachine::from_snapshot(snapshot(GoalBudget::new(None, None, Some(50), None)));
        machine
            .apply(
                GoalCommand::Pause {
                    goal_id: goal_id(),
                    process_recovered: true,
                },
                &GoalActor::System,
            )
            .expect("pause");
        let snap = machine.snapshot().expect("snapshot");
        assert_eq!(snap.stop_reason(), Some(GoalStopReason::ProcessRecovered));
        let mut guard = GoalBudgetGuard::from_snapshot(snap);
        guard.after_turn(50, &live()).expect("recovered");
        assert_eq!(guard.usage().active_ms(), 0);
        assert_eq!(guard.usage().turns(), 0);
    }

    #[test]
    fn dimension_wire_forms_are_stable() {
        assert_eq!(BudgetDimension::Turns.as_str(), "turns");
        assert_eq!(BudgetDimension::Tokens.as_str(), "tokens");
        assert_eq!(BudgetDimension::ActiveMs.as_str(), "active_ms");
        assert_eq!(BudgetDimension::Cost.as_str(), "cost");
        assert_eq!(GoalId::from_str(GOAL_ID).expect("id").to_string(), GOAL_ID);
    }
}
