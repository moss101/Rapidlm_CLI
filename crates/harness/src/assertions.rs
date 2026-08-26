//! P11-008..013: assertion families evaluated against real ledger event
//! records (kind + payload JSON), one typed engine for every family.

use serde_json::Value;

/// Ledger-shaped record the engine evaluates. Produced by kernel export.
#[derive(Clone, Debug, PartialEq)]
pub struct LedgerRecord {
    pub kind: String,
    pub payload: Value,
}

impl LedgerRecord {
    pub fn new(kind: &str, payload: Value) -> Self {
        Self {
            kind: kind.to_owned(),
            payload,
        }
    }
}

/// Assertion families required by the eval contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssertionFamily {
    Graph,
    Files,
    Policy,
    Process,
    ContextEvidence,
    BrowserComputer,
}

impl AssertionFamily {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Graph => "graph",
            Self::Files => "files",
            Self::Policy => "policy",
            Self::Process => "process",
            Self::ContextEvidence => "context_evidence",
            Self::BrowserComputer => "browser_computer",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "graph" => Self::Graph,
            "files" | "workspace" => Self::Files,
            "policy" | "capability" => Self::Policy,
            "process" | "resource" => Self::Process,
            "context_evidence" | "context" | "evidence" => Self::ContextEvidence,
            "browser_computer" | "browser" | "computer" => Self::BrowserComputer,
            _ => return None,
        })
    }

    /// The ledger event kinds this family asserts over.
    fn matches_kind(self, kind: &str) -> bool {
        match self {
            Self::Graph => {
                kind == "goal.created" || kind == "goal.completed" || kind == "agent.spawned"
            }
            Self::Files => {
                kind == "workspace.patch_staged"
                    || kind == "workspace.transaction_committed"
                    || kind == "file.pre_write"
            }
            Self::Policy => {
                kind == "approval.requested" || kind == "approval.resolved" || kind == "tool.denied"
            }
            Self::Process => kind.starts_with("job."),
            Self::ContextEvidence => kind.starts_with("context.") || kind.starts_with("evidence."),
            Self::BrowserComputer => kind.starts_with("computer."),
        }
    }
}

/// A single assertion: family + expected event count (0 = must be absent).
#[derive(Clone, Debug, PartialEq)]
pub struct Assertion {
    pub family: AssertionFamily,
    /// Optional substring the payloads of matching events must contain.
    pub contains: Option<String>,
    pub min_count: u64,
}

/// Verdict for one assertion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Passed,
    Failed { observed: u64 },
}

/// P11-008..013 assertion engine over exported ledger records.
pub struct AssertionEngine;

impl AssertionEngine {
    /// Count matching records for a family (optionally requiring a payload
    /// substring), then compare against `assertion.min_count`.
    pub fn evaluate(records: &[LedgerRecord], assertion: &Assertion) -> Verdict {
        let mut observed = 0u64;
        for record in records {
            if !assertion.family.matches_kind(&record.kind) {
                continue;
            }
            if let Some(needle) = &assertion.contains {
                let text = record.payload.to_string();
                if !text.contains(needle.as_str()) {
                    continue;
                }
            }
            observed += 1;
        }
        // min_count == 0 asserts ABSENCE; otherwise observed must reach it.
        let passed = if assertion.min_count == 0 {
            observed == 0
        } else {
            observed >= assertion.min_count
        };
        if passed {
            Verdict::Passed
        } else {
            Verdict::Failed { observed }
        }
    }

    /// Evaluate all assertions; every verdict is returned so reports show
    /// partial failures rather than collapsing to one boolean.
    pub fn evaluate_all(
        records: &[LedgerRecord],
        assertions: &[Assertion],
    ) -> Vec<(Assertion, Verdict)> {
        assertions
            .iter()
            .map(|a| (a.clone(), Self::evaluate(records, a)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seeded() -> Vec<LedgerRecord> {
        vec![
            LedgerRecord::new("session.created", json!({})),
            LedgerRecord::new("goal.created", json!({"title": "ship"})),
            LedgerRecord::new("goal.completed", json!({"title": "ship"})),
            LedgerRecord::new(
                "workspace.transaction_committed",
                json!({"path": "src/lib.rs"}),
            ),
            LedgerRecord::new("approval.requested", json!({"tool": "fs.write"})),
            LedgerRecord::new("approval.resolved", json!({"choice": "approve"})),
            LedgerRecord::new("job.completed", json!({"exit": 0})),
            LedgerRecord::new("context.compiled", json!({"tokens": 900})),
            LedgerRecord::new("evidence.validated", json!({"claim": "c1"})),
            LedgerRecord::new(
                "computer.action_completed",
                json!({"target": "submit", "fresh": true}),
            ),
        ]
    }

    #[test]
    fn graph_family_counts_goal_lifecycle_events() {
        let verdict = AssertionEngine::evaluate(
            &seeded(),
            &Assertion {
                family: AssertionFamily::Graph,
                contains: None,
                min_count: 2,
            },
        );
        assert_eq!(verdict, Verdict::Passed);
        let failing = AssertionEngine::evaluate(
            &seeded(),
            &Assertion {
                family: AssertionFamily::Graph,
                contains: None,
                min_count: 3,
            },
        );
        assert_eq!(failing, Verdict::Failed { observed: 2 });
    }

    #[test]
    fn files_policy_process_context_browser_families_match_their_kinds() {
        let records = seeded();
        for (family, min) in [
            (AssertionFamily::Files, 1),
            (AssertionFamily::Policy, 2),
            (AssertionFamily::Process, 1),
            (AssertionFamily::ContextEvidence, 2),
            (AssertionFamily::BrowserComputer, 1),
        ] {
            let verdict = AssertionEngine::evaluate(
                &records,
                &Assertion {
                    family,
                    contains: None,
                    min_count: min,
                },
            );
            assert_eq!(verdict, Verdict::Passed, "{family:?} should pass");
        }
        // Absence assertions: zero browser events must pass with min_count 0.
        let empty: Vec<LedgerRecord> =
            vec![LedgerRecord::new("session.created", json!({}))];
        assert_eq!(
            AssertionEngine::evaluate(
                &empty,
                &Assertion {
                    family: AssertionFamily::BrowserComputer,
                    contains: None,
                    min_count: 0,
                }
            ),
            Verdict::Passed
        );
    }

    #[test]
    fn payload_substring_narrows_matches_and_evaluate_all_reports_each_verdict() {
        let records = seeded();
        let assertions = vec![
            Assertion {
                family: AssertionFamily::Graph,
                contains: Some("ship".into()),
                min_count: 2,
            },
            Assertion {
                family: AssertionFamily::Policy,
                contains: Some("deny".into()),
                min_count: 1,
            }, // no deny events -> fail
        ];
        let verdicts = AssertionEngine::evaluate_all(&records, &assertions);
        assert_eq!(verdicts[0].1, Verdict::Passed);
        assert_eq!(
            verdicts[1].1,
            Verdict::Failed {
                observed: 0
            }
        );
        // Family parser accepts documented aliases and rejects junk.
        assert!(AssertionFamily::parse("capability").is_some());
        assert!(AssertionFamily::parse("nope").is_none());
    }
}
