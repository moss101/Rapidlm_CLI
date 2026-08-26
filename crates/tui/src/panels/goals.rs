//! Goals/evidence panel (P6-022). A frontend projection of `AppState.goals`.
//!
//! [`GoalViewModel`] never mutates goal state; it only projects the goal rows
//! (statement, lifecycle, stop reason, budget/usage) already reduced by the
//! kernel/snapshot or `LocalUiEvent::SyncGoal`, and marks the selected goal.
//! Untrusted statement text is sanitized.

use kernel::GoalStopReason;
use protocol::GoalId;

use crate::sanitize::sanitize_untrusted;
use crate::state::{AppState, GoalLifecycle};

/// Maximum projected goal rows in one panel.
pub const MAX_GOALS: usize = crate::state::MAX_PROJECTED_GOALS;
/// Render height is clamped to this many rows.
pub const MAX_GOALS_ROWS: u16 = 256;
/// Preview truncation for a goal statement.
pub const MAX_GOAL_PREVIEW_CHARS: usize = 56;

/// One projected goal row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalRow {
    id: GoalId,
    statement: String,
    lifecycle: GoalLifecycle,
    stop_reason: Option<GoalStopReason>,
    max_turns: Option<u64>,
    max_tokens: Option<u64>,
    turns: u64,
    tokens: u64,
}

impl GoalRow {
    pub fn id(&self) -> GoalId {
        self.id
    }
    pub fn statement(&self) -> &str {
        &self.statement
    }
    pub const fn lifecycle(&self) -> GoalLifecycle {
        self.lifecycle
    }
    pub const fn stop_reason(&self) -> Option<GoalStopReason> {
        self.stop_reason
    }
    pub const fn max_turns(&self) -> Option<u64> {
        self.max_turns
    }
    pub const fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }
    pub const fn turns(&self) -> u64 {
        self.turns
    }
    pub const fn tokens(&self) -> u64 {
        self.tokens
    }
}

/// Bounded goals panel projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalViewModel {
    rows: Vec<GoalRow>,
    selected: Option<GoalId>,
}

impl GoalViewModel {
    pub fn from_state(state: &AppState) -> Self {
        let rows = state
            .goals()
            .values()
            .take(MAX_GOALS)
            .map(|goal| {
                let mut statement: String =
                    sanitize_untrusted(goal.statement().unwrap_or("")).into_owned();
                if statement.chars().count() > MAX_GOAL_PREVIEW_CHARS {
                    let cut: String = statement
                        .chars()
                        .take(MAX_GOAL_PREVIEW_CHARS.saturating_sub(1))
                        .collect();
                    statement = format!("{cut}…");
                }
                GoalRow {
                    id: goal.id(),
                    statement,
                    lifecycle: goal.lifecycle(),
                    stop_reason: goal.stop_reason(),
                    max_turns: goal.max_turns(),
                    max_tokens: goal.max_tokens(),
                    turns: goal.turns(),
                    tokens: goal.tokens(),
                }
            })
            .collect();
        Self {
            rows,
            selected: state.selected_goal(),
        }
    }

    pub fn rows(&self) -> &[GoalRow] {
        &self.rows
    }

    pub fn selected(&self) -> Option<GoalId> {
        self.selected
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{GoalProjection, LocalUiEvent, UiEvent, reduce};
    use std::str::FromStr;

    fn goal_id() -> GoalId {
        GoalId::from_str("018f3c8a-7e2b-7a10-8c4d-0123456789ab").expect("goal")
    }

    #[test]
    fn empty_panel_renders_placeholder() {
        let model = GoalViewModel::from_state(&crate::state::AppState::new());
        assert!(model.is_empty());
        assert!(model.rows().is_empty());
    }

    #[test]
    fn goal_row_is_sanitized_and_preview_truncated() {
        let long = "x".repeat(200);
        let projection = GoalProjection::new(
            goal_id(),
            Some(long),
            GoalLifecycle::Active,
            Some(10),
            Some(100_000),
            3,
            42,
        );
        let state = reduce(crate::state::AppState::new(), &UiEvent::Local(LocalUiEvent::SyncGoal(projection)));
        let model = GoalViewModel::from_state(&state);
        let row = &model.rows()[0];
        assert_eq!(row.id(), goal_id());
        assert!(row.statement().chars().count() <= MAX_GOAL_PREVIEW_CHARS);
        assert_eq!(row.lifecycle(), GoalLifecycle::Active);
        assert_eq!(row.max_turns(), Some(10));
        assert_eq!(row.turns(), 3);
        assert_eq!(row.tokens(), 42);
        assert_eq!(model.selected(), Some(goal_id()));
    }
}
