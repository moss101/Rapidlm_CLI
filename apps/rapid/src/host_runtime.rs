//! Canonical production host runtime (P7/P8 composition closure).
//!
//! The single production authority that owns lifecycle and composition for
//! P7 subsystems (resource pool, monitors, triggers) and coordinates them
//! toward wake/event paths. Built in the composition root so it can reach
//! agent-runtime, agent-pool, process-supervisor, and protocol.
//!
//! This is not a facade — it IS the runtime. The CLI constructs it, drives
//! operations through it, and its methods are the production call path.

use agent_pool::{PoolBackend, PoolError, Provisioner, ResourcePool};
use process_supervisor::monitor::{MonitorError, MonitorObservation, MonitorSpec, MonitorVerdict};
use process_supervisor::trigger::{FireDecision, TriggerCursor, TriggerError, TriggerSpec};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

/// Canonical host-runtime identity. Bounded, stable per session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeId(String);

impl RuntimeId {
    pub fn new(raw: impl Into<String>) -> Result<Self, HostRuntimeError> {
        let raw = raw.into();
        if raw.is_empty() || raw.len() > 128 || raw.chars().any(char::is_control) {
            return Err(HostRuntimeError::InvalidIdentity);
        }
        Ok(Self(raw))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Typed host-runtime failure. Display never echoes untrusted content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostRuntimeError {
    InvalidIdentity,
    ResourceAcquisition(PoolError),
    MonitorRegistration(MonitorError),
    TriggerRegistration(TriggerError),
}

impl fmt::Display for HostRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidIdentity => "host runtime identity is invalid",
            Self::ResourceAcquisition(err) => return write!(f, "resource acquisition: {err}"),
            Self::MonitorRegistration(err) => return write!(f, "monitor registration: {err}"),
            Self::TriggerRegistration(err) => return write!(f, "trigger registration: {err}"),
        })
    }
}

impl Error for HostRuntimeError {}

/// A registered monitor attached to a supervised target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredMonitor {
    spec: MonitorSpec,
    last_verdict: Option<MonitorVerdict>,
}

/// A registered trigger with its durable cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredTrigger {
    spec: TriggerSpec,
    cursor: TriggerCursor,
}

/// Lifecycle event emitted by the host runtime for the ledger/wake path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostEvent {
    ResourceAcquired { env_id: String, owner: String },
    ResourceReleased { env_id: String },
    MonitorRegistered { monitor_id: String },
    MonitorFired { monitor_id: String },
    TriggerFired { trigger_id: String },
    WakeEmitted { reason: String },
}

/// Canonical production host runtime. Composes P7 subsystems into one authority.
pub struct HostRuntime {
    identity: RuntimeId,
    pool: ResourcePool,
    monitors: BTreeMap<String, RegisteredMonitor>,
    triggers: BTreeMap<String, RegisteredTrigger>,
    events: Vec<HostEvent>,
}

impl HostRuntime {
    /// Construct the canonical host runtime for one session.
    pub fn new(identity: RuntimeId) -> Self {
        Self {
            identity,
            pool: ResourcePool::new(8),
            monitors: BTreeMap::new(),
            triggers: BTreeMap::new(),
            events: Vec::new(),
        }
    }

    pub fn identity(&self) -> &str {
        self.identity.as_str()
    }

    /// Acquire a warm environment lease (P7-025/P7-028 miss-fallback provisioning).
    pub fn acquire_environment<P: agent_pool::Provisioner>(
        &mut self,
        backend: PoolBackend,
        owner: &str,
        now: u64,
        provisioner: &mut P,
    ) -> Result<agent_pool::EnvironmentLease, HostRuntimeError> {
        let lease = self
            .pool
            .acquire(backend, owner, now, provisioner)
            .map_err(HostRuntimeError::ResourceAcquisition)?;
        self.events.push(HostEvent::ResourceAcquired {
            env_id: lease.id().to_owned(),
            owner: owner.to_owned(),
        });
        Ok(lease)
    }

    /// Release an environment back to the pool (P7-027).
    pub fn release_environment<P: agent_pool::Provisioner>(
        &mut self,
        id: &str,
        clean: bool,
        now: u64,
        provisioner: &mut P,
    ) -> Result<(), HostRuntimeError> {
        self.pool
            .release(id, clean, now, provisioner)
            .map_err(HostRuntimeError::ResourceAcquisition)?;
        self.events.push(HostEvent::ResourceReleased {
            env_id: id.to_owned(),
        });
        Ok(())
    }

    /// Register a monitor against a supervised target (P7-009..013).
    pub fn register_monitor(&mut self, spec: MonitorSpec) -> Result<(), HostRuntimeError> {
        if self.monitors.len() >= 256 {
            return Err(HostRuntimeError::MonitorRegistration(
                MonitorError::BoundExceeded,
            ));
        }
        let monitor_id = spec.id().to_owned();
        self.monitors.insert(
            monitor_id.clone(),
            RegisteredMonitor {
                spec,
                last_verdict: None,
            },
        );
        self.events.push(HostEvent::MonitorRegistered {
            monitor_id,
        });
        Ok(())
    }

    /// Feed an observation to a registered monitor; returns the verdict.
    pub fn observe_monitor(
        &mut self,
        monitor_id: &str,
        observation: &MonitorObservation,
    ) -> Result<MonitorVerdict, HostRuntimeError> {
        let monitor = self.monitors.get_mut(monitor_id).ok_or_else(|| {
            HostRuntimeError::MonitorRegistration(MonitorError::InvalidId)
        })?;
        let verdict = monitor.spec.observe(observation);
        monitor.last_verdict = Some(verdict);
        if verdict.matched_flag() {
            self.events.push(HostEvent::MonitorFired {
                monitor_id: monitor_id.to_owned(),
            });
        }
        Ok(verdict)
    }

    /// Register a trigger (P7-015..019).
    pub fn register_trigger(&mut self, spec: TriggerSpec) -> Result<(), HostRuntimeError> {
        if self.triggers.len() >= 256 {
            return Err(HostRuntimeError::TriggerRegistration(
                TriggerError::InvalidExpression,
            ));
        }
        self.triggers.insert(
            spec.id().to_owned(),
            RegisteredTrigger {
                spec,
                cursor: process_supervisor::trigger::TriggerCursor::new(),
            },
        );
        Ok(())
    }

    /// Evaluate a trigger; returns the fire decision (P7-016..019).
    pub fn evaluate_trigger(
        &mut self,
        trigger_id: &str,
        now: u64,
    ) -> Result<FireDecision, HostRuntimeError> {
        let trigger = self.triggers.get_mut(trigger_id).ok_or_else(|| {
            HostRuntimeError::TriggerRegistration(TriggerError::InvalidId)
        })?;
        use process_supervisor::trigger::UnitTime;
        let decision =
            trigger.spec.kind().fires_at(UnitTime::new(now), &trigger.cursor);
        if matches!(
            decision,
            FireDecision::Fire | FireDecision::MissedBackfill
        ) {
            trigger.cursor.advance(decision, now);
            self.events.push(HostEvent::TriggerFired {
                trigger_id: trigger_id.to_owned(),
            });
            self.events.push(HostEvent::WakeEmitted {
                reason: format!("trigger:{trigger_id}"),
            });
        }
        Ok(decision)
    }

    /// Lifecycle event log for ledger persistence / wake propagation.
    pub fn events(&self) -> &[HostEvent] {
        &self.events
    }

    /// Extract wake signals from lifecycle events (P7-014).
    ///
    /// Returns `(trigger_id_or_reason, kind)` pairs that the composition-root
    /// can route to the Runtime Graph scheduler as runnable-work signals.
    /// This is the bridge between process-supervisor monitors/triggers and
    /// the graph scheduler without creating a direct cross-crate dependency.
    pub fn drain_wake_signals(&self) -> Vec<(String, &'static str)> {
        self.events
            .iter()
            .filter_map(|event| match event {
                HostEvent::MonitorFired { monitor_id } => {
                    Some((monitor_id.clone(), "monitor"))
                }
                HostEvent::TriggerFired { trigger_id } => {
                    Some((trigger_id.clone(), "trigger"))
                }
                HostEvent::WakeEmitted { reason } => Some((reason.clone(), "wake")),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use process_supervisor::monitor::{FileEventKind, MonitorKind};
    use process_supervisor::trigger::TriggerKind;

    struct FakeProvisioner;
    impl agent_pool::Provisioner for FakeProvisioner {
        fn provision(&mut self, backend: PoolBackend) -> Result<String, PoolError> {
            Ok(format!("env-{}", backend.as_str()))
        }
        fn sanitize(&mut self, _id: &str) -> Result<(), PoolError> {
            Ok(())
        }
        fn destroy(&mut self, _id: &str) -> Result<(), PoolError> {
            Ok(())
        }
    }

    #[test]
    fn host_runtime_full_p7_production_path() {
        // This is the production-path integration test (§7 of the objective):
        // HostRuntime → acquire resource → register monitor → observe monitor
        // → register trigger → evaluate trigger → wake event emitted.
        let mut runtime = HostRuntime::new(
            RuntimeId::new("test-session").unwrap(),
        );
        let mut provisioner = FakeProvisioner;

        // Acquire a resource lease through the pool.
        let lease = runtime
            .acquire_environment(PoolBackend::Container, "test-owner", 1000, &mut provisioner)
            .expect("acquire");
        assert_eq!(lease.state(), agent_pool::EnvironmentState::InUse);

        // Register a file-event monitor on the acquired environment's output.
        let monitor_spec = MonitorSpec::new(
            "output-ready",
            MonitorKind::FileEvent {
                path: "/tmp/output.json".to_owned(),
                event: FileEventKind::Created,
            },
        )
        .expect("monitor spec");
        runtime.register_monitor(monitor_spec).expect("register");

        // Observe the monitor — the output file was created.
        let observation =
            MonitorObservation::file("/tmp/output.json", FileEventKind::Created).expect("obs");
        let verdict = runtime.observe_monitor("output-ready", &observation).expect("observe");
        assert!(verdict.matched_flag(), "monitor should fire on matching observation");

        // Register a cron trigger that wakes graph work.
        let trigger_spec = TriggerSpec::new(
            "wake-trigger",
            TriggerKind::Interval { seconds: 60 },
        )
        .expect("trigger spec");
        runtime.register_trigger(trigger_spec).expect("register");

        // Evaluate the trigger at a time past its interval → fires.
        let decision = runtime.evaluate_trigger("wake-trigger", 120).expect("evaluate");
        assert_eq!(decision, FireDecision::Fire);

        // Release the environment back to warm.
        runtime
            .release_environment(lease.id(), true, 200, &mut provisioner)
            .expect("release");

        // Verify the event log records every lifecycle transition.
        assert!(runtime.events().iter().any(|e| matches!(
            e,
            HostEvent::ResourceAcquired { .. }
        )));
        assert!(runtime.events().iter().any(|e| matches!(
            e,
            HostEvent::MonitorRegistered { .. }
        )));
        assert!(runtime.events().iter().any(|e| matches!(
            e,
            HostEvent::MonitorFired { .. }
        )));
        assert!(runtime.events().iter().any(|e| matches!(
            e,
            HostEvent::TriggerFired { .. }
        )));
        assert!(runtime.events().iter().any(|e| matches!(
            e,
            HostEvent::WakeEmitted { .. }
        )));
        assert!(runtime.events().iter().any(|e| matches!(
            e,
            HostEvent::ResourceReleased { .. }
        )));
    }

    #[test]
    fn host_runtime_rejects_invalid_identity() {
        let result = RuntimeId::new("");
        assert!(matches!(result, Err(HostRuntimeError::InvalidIdentity)));
    }

    #[test]
    fn drain_wake_signals_extracts_monitor_and_trigger_fires() {
        let mut runtime =
            HostRuntime::new(RuntimeId::new("wake-test").unwrap());
        let mut provisioner = FakeProvisioner;

        runtime
            .acquire_environment(PoolBackend::Container, "o", 1, &mut provisioner)
            .expect("acquire");

        runtime
            .register_monitor(
                MonitorSpec::new(
                    "exit-monitor",
                    process_supervisor::monitor::MonitorKind::ExitCode { code: 0 },
                )
                .unwrap(),
            )
            .expect("register monitor");
        let obs = MonitorObservation::ExitCode { code: 0 };
        let verdict = runtime.observe_monitor("exit-monitor", &obs).expect("observe");
        assert!(verdict.matched_flag());

        runtime
            .register_trigger(
                TriggerSpec::new("interval-t", TriggerKind::Interval { seconds: 60 }).unwrap(),
            )
            .expect("register trigger");
        let decision = runtime.evaluate_trigger("interval-t", 120).expect("evaluate");
        assert_eq!(decision, FireDecision::Fire);

        let signals = runtime.drain_wake_signals();
        assert!(
            signals.iter().any(|(id, kind)| id == "exit-monitor" && *kind == "monitor"),
            "monitor wake signal must be present"
        );
        assert!(
            signals.iter().any(|(id, kind)| id == "interval-t" && *kind == "trigger"),
            "trigger wake signal must be present"
        );
    }
}
