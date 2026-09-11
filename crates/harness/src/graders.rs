//! P11-014..017: deterministic grader registry, optional JudgeAdapter seam,
//! MetricCollector registry (incl. verified-success-per-token), FailureBundler.

use crate::assertions::{Assertion, LedgerRecord, Verdict};

/// Deterministic verdict derived from assertions only. Same input -> same
/// grade, no model in the loop.
#[derive(Clone, Debug, PartialEq)]
pub struct Grade {
    pub grader: &'static str,
    pub score: f64,
    pub passed: bool,
}

pub struct DeterministicGrader;

impl DeterministicGrader {
    /// Score = passed/total assertions; pass requires every verdict Passed.
    pub fn grade(assertions: &[(Assertion, Verdict)]) -> Grade {
        let total = assertions.len().max(1);
        let passed = assertions
            .iter()
            .filter(|(_, v)| *v == Verdict::Passed)
            .count();
        Grade {
            grader: "deterministic",
            score: passed as f64 / total as f64,
            passed: passed == assertions.len() && !assertions.is_empty(),
        }
    }
}

/// Optional rubric-judge seam for live models. The harness never performs
/// live I/O itself; implementations are provided externally per experiment.
pub trait JudgeAdapter {
    fn judge(&self, transcript: &str) -> Result<(f64, String), JudgeError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JudgeError {
    NotConfigured,
}

/// Collected metric sample.
#[derive(Clone, Debug, PartialEq)]
pub struct Metric {
    pub name: &'static str,
    pub value: f64,
}

/// Metric collectors over one run's records + token usage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricCollector {
    VerifiedSuccessPerToken,
    EventCount,
    ApprovalRate,
}

impl MetricCollector {
    pub const ALL: [Self; 3] = [
        Self::VerifiedSuccessPerToken,
        Self::EventCount,
        Self::ApprovalRate,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::VerifiedSuccessPerToken => "verified_success_per_token",
            Self::EventCount => "event_count",
            Self::ApprovalRate => "approval_rate",
        }
    }

    pub fn collect(self, records: &[LedgerRecord], tokens_used: u64) -> Metric {
        match self {
            Self::EventCount => Metric {
                name: self.name(),
                value: records.len() as f64,
            },
            Self::ApprovalRate => {
                let approvals = records
                    .iter()
                    .filter(|r| r.kind == "approval.resolved")
                    .count();
                Metric {
                    name: self.name(),
                    value: if records.is_empty() {
                        0.0
                    } else {
                        approvals as f64 / records.len() as f64
                    },
                }
            }
            Self::VerifiedSuccessPerToken => {
                let completed = records
                    .iter()
                    .any(|r| r.kind == "goal.completed" || r.kind == "turn.completed");
                let success = u32::from(completed) as f64;
                Metric {
                    name: self.name(),
                    value: if tokens_used == 0 {
                        0.0
                    } else {
                        success / tokens_used as f64
                    },
                }
            }
        }
    }
}

/// Minimal repro bundle for one failure: bounded inputs + verdicts.
#[derive(Clone, Debug, PartialEq)]
pub struct FailureBundle {
    pub scenario: String,
    pub failing: Vec<String>,
    pub metrics: Vec<Metric>,
}

impl FailureBundle {
    pub fn bundle(
        scenario: &str,
        verdicts: &[(Assertion, Verdict)],
        metrics: &[Metric],
    ) -> Option<Self> {
        let failing: Vec<String> = verdicts
            .iter()
            .filter(|(_, v)| *v != Verdict::Passed)
            .map(|(a, _)| {
                format!(
                    "{}:{}",
                    a.family.as_str(),
                    a.contains.clone().unwrap_or_default()
                )
            })
            .collect();
        if failing.is_empty() {
            return None;
        }
        Some(Self {
            scenario: scenario.to_owned(),
            failing,
            metrics: metrics.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assertions::AssertionFamily;
    use serde_json::json;

    #[test]
    fn deterministic_grader_scores_ratio_and_requires_full_pass() {
        let records = vec![
            LedgerRecord::new("goal.completed", json!({})),
            LedgerRecord::new("job.completed", json!({})),
        ];
        let assertions = vec![
            Assertion {
                family: AssertionFamily::Graph,
                contains: None,
                min_count: 1,
            },
            Assertion {
                family: AssertionFamily::Process,
                contains: None,
                min_count: 1,
            },
        ];
        let grade = DeterministicGrader::grade(&AssertionEngineProxy::eval(&records, &assertions));
        assert!(grade.passed);
        assert!((grade.score - 1.0).abs() < f64::EPSILON);
        let failing = vec![(
            Assertion {
                family: AssertionFamily::BrowserComputer,
                contains: None,
                min_count: 5,
            },
            Verdict::Failed { observed: 2 },
        )];
        let grade2 = DeterministicGrader::grade(&failing);
        assert!(!grade2.passed);
        assert!((grade2.score).abs() < f64::EPSILON);
    }

    #[test]
    fn metrics_include_verified_success_per_token_and_approval_rate() {
        let records = vec![
            LedgerRecord::new("approval.requested", json!({})),
            LedgerRecord::new("approval.resolved", json!({})),
            LedgerRecord::new("turn.completed", json!({})),
        ];
        let vspt = MetricCollector::VerifiedSuccessPerToken.collect(&records, 100);
        assert_eq!(vspt.name, "verified_success_per_token");
        assert!((vspt.value - 0.01).abs() < 1e-9);
        let rate = MetricCollector::ApprovalRate.collect(&records, 0);
        assert!((rate.value - (1.0 / 3.0)).abs() < 1e-9);
        assert_eq!(MetricCollector::EventCount.collect(&records, 0).value, 3.0);
    }

    #[test]
    fn failure_bundle_only_bundles_failures() {
        let records: Vec<LedgerRecord> = vec![];
        let verdicts = vec![
            (
                Assertion {
                    family: AssertionFamily::Files,
                    contains: Some("patch".into()),
                    min_count: 1,
                },
                Verdict::Failed { observed: 0 },
            ),
            (
                Assertion {
                    family: AssertionFamily::Policy,
                    contains: None,
                    min_count: 0,
                },
                Verdict::Passed,
            ),
        ];
        let bundle = FailureBundle::bundle("sc-1", &verdicts, &[]).expect("bundle");
        assert_eq!(bundle.scenario, "sc-1");
        assert_eq!(bundle.failing, vec!["files:patch".to_owned()]);
        // All passing -> no bundle.
        let all_pass = vec![(
            Assertion {
                family: AssertionFamily::Policy,
                contains: None,
                min_count: 0,
            },
            Verdict::Passed,
        )];
        assert!(FailureBundle::bundle("sc-1", &all_pass, &[]).is_none());
        let _ = &records;
    }

    /// Local adapter so grader tests do not depend on engine construction order.
    struct AssertionEngineProxy;
    impl AssertionEngineProxy {
        fn eval(records: &[LedgerRecord], assertions: &[Assertion]) -> Vec<(Assertion, Verdict)> {
            crate::assertions::AssertionEngine::evaluate_all(records, assertions)
        }
    }
}
