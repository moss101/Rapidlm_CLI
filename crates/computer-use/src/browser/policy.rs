//! Pure batching + settle/reobserve policy (P8-009, P8-010).
//!
//! Deterministic rules over a [`UiAction`] list: whether a sequence is a valid
//! batch (bounded, no page-change mixed with mutations), and the settle/reobserve
//! cost each action imposes. No I/O here — the runtime applies these rules.

use crate::browser::action::UiAction;

/// Maximum actions accepted in one batch.
pub const MAX_BATCH_ACTIONS: usize = 16;

/// Whether a list of [`UiAction`] is a legal batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BatchDecision {
    Ok,
    Empty,
    TooLarge,
    /// A `Navigate` (page change) cannot be batched with other mutations.
    MixedPageChange,
}

/// Settle + reobserve cost of one action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SettlePolicy {
    /// Millis to wait for the page to settle after the action.
    pub settle_ms: u64,
    /// Whether a fresh observation is required before the next action.
    pub reobserve: bool,
}

/// Assess whether `actions` is a legal batch.
pub fn batchable(actions: &[UiAction]) -> BatchDecision {
    if actions.is_empty() {
        return BatchDecision::Empty;
    }
    if actions.len() > MAX_BATCH_ACTIONS {
        return BatchDecision::TooLarge;
    }
    let has_page_change = actions
        .iter()
        .any(|a| matches!(a, UiAction::Navigate { .. }));
    if has_page_change && actions.len() > 1 {
        return BatchDecision::MixedPageChange;
    }
    BatchDecision::Ok
}

/// Settle/reobserve policy for `action`.
pub fn settle_policy(action: &UiAction) -> SettlePolicy {
    match action {
        UiAction::Navigate { .. } => SettlePolicy {
            settle_ms: 1000,
            reobserve: true,
        },
        UiAction::Click { .. } | UiAction::Type { .. } | UiAction::Key { .. } => SettlePolicy {
            settle_ms: 200,
            reobserve: true,
        },
        UiAction::Scroll { .. } => SettlePolicy {
            settle_ms: 50,
            reobserve: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::action::{KeyCode, MouseButton, TargetSelector};

    fn click() -> UiAction {
        UiAction::Click {
            target: TargetSelector::parse("target-1").expect("selector"),
            button: MouseButton::Left,
            count: 1,
        }
    }
    fn navigate() -> UiAction {
        UiAction::Navigate {
            url: "https://example.com".to_owned(),
        }
    }

    #[test]
    fn batch_rules_reject_empty_too_large_and_page_change_mix() {
        assert_eq!(batchable(&[]), BatchDecision::Empty);
        let many = vec![click(); MAX_BATCH_ACTIONS + 1];
        assert_eq!(batchable(&many), BatchDecision::TooLarge);
        assert_eq!(
            batchable(&[click(), navigate()]),
            BatchDecision::MixedPageChange
        );
        assert_eq!(batchable(&[click(), click()]), BatchDecision::Ok);
    }

    #[test]
    fn settle_policy_is_deterministic_and_bounded() {
        let nav = settle_policy(&navigate());
        assert_eq!(nav.settle_ms, 1000);
        assert!(nav.reobserve);
        let click = settle_policy(&click());
        assert_eq!(click.settle_ms, 200);
        assert!(click.reobserve);
        let key = settle_policy(&UiAction::Key {
            target: None,
            key: KeyCode::parse("Enter").expect("key"),
        });
        assert_eq!(key.settle_ms, 200);
        let scroll = settle_policy(&UiAction::Scroll {
            target: None,
            dx: 0,
            dy: 100,
        });
        assert_eq!(scroll.settle_ms, 50);
    }
}
