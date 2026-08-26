#![forbid(unsafe_code)]
//! P11-026/028..030: session insights analyzers, endurance harness planner,
//! and the verified-success-per-token release benchmark (budget-parameterized).

/// One insight extracted from a session's observable events.
#[derive(Clone, Debug, PartialEq)]
pub struct Insight {
    pub kind: &'static str,
    pub detail: String,
}

/// Session insights over exported ledger records: tool usage, approvals,
/// goal completion, error pressure.
pub fn analyze(records: &[crate::EventSummary]) -> Vec<Insight> {
    let mut out = Vec::new();
    let tools = records
        .iter()
        .filter(|r| r.kind.starts_with("tool."))
        .count();
    if tools > 0 {
        out.push(Insight {
            kind: "tool_usage",
            detail: format!("{tools} tool events"),
        });
    }
    let denied = records.iter().filter(|r| r.kind == "tool.denied").count();
    if denied > 0 {
        out.push(Insight {
            kind: "denials",
            detail: format!("{denied} denied tool calls"),
        });
    }
    if records.iter().any(|r| r.kind == "goal.completed") {
        out.push(Insight {
            kind: "goal_completion",
            detail: "at least one goal completed".into(),
        });
    }
    let secrets = records
        .iter()
        .filter(|r| r.kind.contains("secret"))
        .count();
    if secrets > 0 {
        out.push(Insight {
            kind: "secret_touches",
            detail: format!("{secrets} secret-classified events"),
        });
    }
    out
}

/// Event summary fed to analyzers (bounded).
#[derive(Clone, Debug, PartialEq)]
pub struct EventSummary {
    pub kind: String,
}

impl EventSummary {
    pub fn new(kind: &str) -> Self {
        Self { kind: kind.to_owned() }
    }
}

// ---------------------------------------------------------------------------
// P11-028 endurance harness (deterministic, budget-parameterized).

/// Endurance tier in wall-clock hours for real deployments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnduranceTier {
    H1,
    H4,
    H12,
    H24,
}

impl EnduranceTier {
    pub const fn hours(self) -> u32 {
        match self {
            Self::H1 => 1,
            Self::H4 => 4,
            Self::H12 => 12,
            Self::H24 => 24,
        }
    }

    /// Deterministic scaled-down event budget: 360 events/hour compressed.
    pub const fn planned_events(self) -> u64 {
        self.hours() as u64 * 360
    }
}

/// Execute a compressed endurance run: drive `step` until the tier's event
/// budget is met. Real-duration wall-clock execution is an environmental
/// limitation; this driver keeps the contract time-budget aware.
pub fn run_endurance(
    tier: EnduranceTier,
    mut step: impl FnMut(u64) -> Result<(), ()>,
) -> u64 {
    let budget = tier.planned_events();
    for i in 1..=budget {
        if step(i).is_err() {
            return i.saturating_sub(1);
        }
    }
    budget
}

// ---------------------------------------------------------------------------
// P11-029 >=1000-tool-call release scenario + P11-030 benchmark.

/// Minimum tool calls required by the release scenario.
pub const RELEASE_TOOL_CALL_MINIMUM: u64 = 1000;

/// Drive the >=1000-tool-call release scenario; every call must succeed.
pub fn run_release_scenario(mut call_tool: impl FnMut(u64) -> bool) -> u64 {
    let mut completed = 0;
    for i in 1..=RELEASE_TOOL_CALL_MINIMUM {
        if !call_tool(i) {
            return completed;
        }
        completed = i;
    }
    completed
}

/// Verified-success-per-token metric over a run.
pub fn verified_success_per_token(completed_goals: u32, tokens_used: u64) -> f64 {
    if tokens_used == 0 {
        return 0.0;
    }
    completed_goals as f64 / tokens_used as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzer_surfaces_tools_denials_completion_and_secrets() {
        let records = vec![
            EventSummary::new("tool.completed"),
            EventSummary::new("tool.completed"),
            EventSummary::new("tool.denied"),
            EventSummary::new("goal.completed"),
            EventSummary::new("secret.used"),
        ];
        let insights = analyze(&records);
        let kinds: Vec<&str> = insights.iter().map(|i| i.kind).collect();
        assert!(kinds.contains(&"tool_usage"));
        assert!(kinds.contains(&"denials"));
        assert!(kinds.contains(&"goal_completion"));
        assert!(kinds.contains(&"secret_touches"));
    }

    #[test]
    fn endurance_tiers_plan_deterministic_budgets_and_stop_on_failure() {
        assert_eq!(EnduranceTier::H24.planned_events(), 24 * 360);
        let mut last = 0;
        let executed = run_endurance(EnduranceTier::H1, |i| {
            last = i;
            if i < 100 { Ok(()) } else { Err(()) }
        });
        assert_eq!(executed, 99);
        assert_eq!(last, 100);
        let full = run_endurance(EnduranceTier::H1, |_| Ok(()));
        assert_eq!(full, EnduranceTier::H1.planned_events());
    }

    #[test]
    fn release_scenario_requires_1000_successful_calls() {
        let ok = run_release_scenario(|_| true);
        assert_eq!(ok, RELEASE_TOOL_CALL_MINIMUM);
        let stopped_at_500 = run_release_scenario(|i| i < 500);
        assert_eq!(stopped_at_500, 499);
    }

    #[test]
    fn vspt_benchmark_math_is_exact() {
        assert!((verified_success_per_token(1, 500) - 0.002).abs() < 1e-9);
        assert_eq!(verified_success_per_token(1, 0), 0.0);
    }
}
