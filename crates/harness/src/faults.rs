//! P11-006/007: KernelRunner through the production kernel path and the
//! FaultInjector framework (process/provider/network/persistence classes).

/// Fault class injected into a harness operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultClass {
    /// Child process loss / spawn failure.
    Process,
    /// Model/provider failure mid-run.
    Provider,
    /// Transport-level failure.
    Network,
    /// Durable store failure.
    Persistence,
}

impl FaultClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Provider => "provider",
            Self::Network => "network",
            Self::Persistence => "persistence",
        }
    }
}

/// One fault injection point: fires once, then the operation must recover
/// or fail closed — never hang or corrupt state.
#[derive(Clone, Debug, PartialEq)]
pub struct FaultInjection {
    pub class: FaultClass,
    /// Fire on the Nth call (1-based).
    pub fire_on: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultOutcome {
    /// Operation completed despite the injected fault (recovery proven).
    Recovered,
    /// Operation failed closed at the injection point.
    FailedClosed,
}

/// Inject faults into an operation across runs. Each `run` invocation
/// executes `op`; when the configured call count matches, the injected error
/// is returned instead. Recovery is asserted by the caller re-running after
/// the failed attempt.
pub struct FaultInjector;

impl FaultInjector {
    /// Run one operation under an injection schedule.
    pub fn run<T>(
        op: &mut dyn FnMut(u32) -> Result<T, FaultClass>,
        injection: &FaultInjection,
        recovery_runs: u32,
    ) -> FaultOutcome {
        let mut recovered = false;
        for call in 1..=1 + recovery_runs {
            let result = if call == injection.fire_on {
                Err(injection.class)
            } else {
                op(call)
            };
            match result {
                Ok(_) => {
                    if call >= injection.fire_on {
                        recovered = true;
                        break;
                    }
                }
                Err(_) => continue, // fail-closed: skip this run, retry next
            }
        }
        if recovered {
            FaultOutcome::Recovered
        } else {
            FaultOutcome::FailedClosed
        }
    }
}

/// Runner mode for evaluation executions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerMode {
    /// Deterministic scripted driver.
    Scripted,
    /// Replay of recorded outputs.
    Replay,
    /// Live experiment (explicit pinning required upstream).
    LiveExperiment,
}

/// Result summary of one kernel-backed harness run.
#[derive(Clone, Debug, PartialEq)]
pub struct RunSummary {
    pub mode: RunnerMode,
    pub session_created: bool,
    pub first_seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injector_fails_closed_at_fire_point_then_recovers() {
        let mut calls = Vec::new();
        let outcome = FaultInjector::run(
            &mut |call| {
                calls.push(call);
                if calls.len() >= 2 {
                    Ok(7)
                } else {
                    Ok(0) // would succeed anyway; injection overrides below
                }
            },
            &FaultInjection {
                class: FaultClass::Network,
                fire_on: 2,
            },
            3,
        );
        // The injection itself is delivered by run() at call 2 regardless of
        // op success; recovery observed on the following successful run.
        assert_eq!(outcome, FaultOutcome::Recovered);
        assert!(calls.len() >= 2);
    }

    #[test]
    fn every_class_is_labelled_for_reports() {
        assert_eq!(FaultClass::Process.as_str(), "process");
        assert_eq!(FaultClass::Provider.as_str(), "provider");
        assert_eq!(FaultClass::Network.as_str(), "network");
        assert_eq!(FaultClass::Persistence.as_str(), "persistence");
    }

    #[test]
    fn exhausted_recovery_budget_fails_closed() {
        let outcome: FaultOutcome = FaultInjector::run(
            &mut |_call| -> Result<u8, FaultClass> { Err(FaultClass::Persistence) }, // op always fails too
            &FaultInjection {
                class: FaultClass::Persistence,
                fire_on: 1,
            },
            2,
        );
        assert_eq!(outcome, FaultOutcome::FailedClosed);
    }
}
