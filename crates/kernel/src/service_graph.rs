//! Dependency-ordered kernel service graph.
//!
//! [`ServiceGraph::start_all`] starts services in topological order. A later
//! start failure reverse-quiesces and stops already-started services. Shutdown
//! is terminal and never starts remaining dependents.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

use crate::CancellationToken;
use crate::service::{
    HealthSnapshot, LifecycleService, ServiceContext, ServiceDecl, ServiceError, ServiceId,
    validate_dependency_graph,
};

/// Bound applied to reverse quiesce when [`ServiceGraph::start_all`] rolls back.
pub const DEFAULT_ROLLBACK_QUIESCE: Duration = Duration::from_secs(5);

const CANCEL_CHECK_EVERY: usize = 16;

/// Observable graph lifecycle. Shutdown is terminal for this instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GraphPhase {
    Idle,
    Starting,
    Running,
    ShuttingDown,
    Stopped,
    Failed,
}

/// Registered services plus the start/stop cursor over them.
pub struct ServiceGraph<S> {
    nodes: Vec<S>,
    /// Dependency-first start indices.
    start_order: Vec<usize>,
    /// Completed starts, in start order. Popped from the back on reverse stop.
    started: Vec<usize>,
    phase: GraphPhase,
    shutting_down: bool,
}

impl GraphPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::ShuttingDown => "shutting_down",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for GraphPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<S: LifecycleService> ServiceGraph<S> {
    /// Validate declarations and freeze a deterministic topological start order.
    pub fn new(
        services: impl IntoIterator<Item = S>,
        cancel: &CancellationToken,
    ) -> Result<Self, ServiceError> {
        cancel_check(cancel)?;
        let nodes: Vec<S> = services.into_iter().collect();
        let decls = nodes
            .iter()
            .map(ServiceDecl::from_service)
            .collect::<Result<Vec<_>, _>>()?;
        let order_ids = validate_dependency_graph(&decls, cancel)?;

        let mut index_by_id: BTreeMap<&ServiceId, usize> = BTreeMap::new();
        for (i, svc) in nodes.iter().enumerate() {
            if i.is_multiple_of(CANCEL_CHECK_EVERY) {
                cancel_check(cancel)?;
            }
            index_by_id.insert(svc.id(), i);
        }

        let mut start_order = Vec::with_capacity(order_ids.len());
        for (i, id) in order_ids.iter().enumerate() {
            if i.is_multiple_of(CANCEL_CHECK_EVERY) {
                cancel_check(cancel)?;
            }
            let idx = index_by_id.get(id).copied().expect(
                "validate_dependency_graph returns only ids present in the declaration set",
            );
            start_order.push(idx);
        }

        Ok(Self {
            nodes,
            start_order,
            started: Vec::new(),
            phase: GraphPhase::Idle,
            shutting_down: false,
        })
    }

    pub fn phase(&self) -> GraphPhase {
        self.phase
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// Dependency-first start order.
    pub fn start_order(&self) -> impl Iterator<Item = &ServiceId> {
        self.start_order.iter().map(|&i| self.nodes[i].id())
    }

    /// Services that completed `start`, in start order.
    pub fn started(&self) -> impl Iterator<Item = &ServiceId> {
        self.started.iter().map(|&i| self.nodes[i].id())
    }

    pub fn services(&self) -> &[S] {
        &self.nodes
    }

    /// Non-blocking health snapshots in registration order.
    pub fn health(&self) -> Vec<HealthSnapshot> {
        self.nodes.iter().map(LifecycleService::health).collect()
    }

    /// Start every service in topological order.
    ///
    /// If a later start fails or cancellation/shutdown is observed, already-started
    /// services are reverse-quiesced and stopped. Remaining dependents are not started.
    pub async fn start_all(&mut self, ctx: ServiceContext) -> Result<(), ServiceError> {
        self.start_all_until(ctx, Instant::now() + DEFAULT_ROLLBACK_QUIESCE)
            .await
    }

    /// [`start_all`] with an explicit rollback quiesce deadline.
    pub async fn start_all_until(
        &mut self,
        ctx: ServiceContext,
        rollback_deadline: Instant,
    ) -> Result<(), ServiceError> {
        ctx.check()?;
        if self.shutting_down
            || matches!(self.phase, GraphPhase::ShuttingDown | GraphPhase::Stopped)
        {
            return Err(ServiceError::Cancelled);
        }
        if self.phase == GraphPhase::Running && !self.started.is_empty() {
            return Ok(());
        }
        if self.phase == GraphPhase::Starting {
            return Err(ServiceError::Failed);
        }

        self.phase = GraphPhase::Starting;
        self.started.clear();

        let order = self.start_order.clone();
        for idx in order {
            if let Err(err) = self.ensure_may_start(&ctx) {
                return self.abort_start(err, rollback_deadline, ctx.cancel()).await;
            }
            match self.nodes[idx].start(ctx.clone()).await {
                Ok(()) => self.started.push(idx),
                Err(err) => {
                    return self.abort_start(err, rollback_deadline, ctx.cancel()).await;
                }
            }
        }

        self.phase = GraphPhase::Running;
        Ok(())
    }

    /// Reverse-quiesce then stop started services. Never starts dependents.
    ///
    /// Deadline expiry force-cancels `cancel` and still calls `stop` on each
    /// started service. Shutdown is terminal for this graph.
    pub async fn shutdown(
        &mut self,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<(), ServiceError> {
        self.shutting_down = true;
        if matches!(self.phase, GraphPhase::Stopped) && self.started.is_empty() {
            return Ok(());
        }
        if matches!(self.phase, GraphPhase::Idle) && self.started.is_empty() {
            self.phase = GraphPhase::Stopped;
            return Ok(());
        }

        self.phase = GraphPhase::ShuttingDown;
        let result = self.reverse_quiesce_stop(deadline, cancel).await;
        self.phase = if result.is_ok() {
            GraphPhase::Stopped
        } else {
            GraphPhase::Failed
        };
        result
    }

    fn ensure_may_start(&self, ctx: &ServiceContext) -> Result<(), ServiceError> {
        if self.shutting_down {
            return Err(ServiceError::Cancelled);
        }
        ctx.check()
    }

    async fn abort_start(
        &mut self,
        err: ServiceError,
        rollback_deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<(), ServiceError> {
        let _ = self.reverse_quiesce_stop(rollback_deadline, cancel).await;
        self.phase = if self.started.is_empty() && !self.shutting_down {
            GraphPhase::Idle
        } else {
            GraphPhase::Failed
        };
        Err(err)
    }

    async fn reverse_quiesce_stop(
        &mut self,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<(), ServiceError> {
        let mut first_err = None;
        while let Some(idx) = self.started.pop() {
            if Instant::now() >= deadline {
                cancel.cancel();
                if first_err.is_none() {
                    first_err = Some(ServiceError::DeadlineExceeded);
                }
            }
            if let Err(err) = self.nodes[idx].quiesce(deadline).await {
                if Instant::now() >= deadline || matches!(err, ServiceError::DeadlineExceeded) {
                    cancel.cancel();
                }
                if first_err.is_none() {
                    first_err = Some(err);
                }
            }
            if let Err(err) = self.nodes[idx].stop().await
                && first_err.is_none()
            {
                first_err = Some(err);
            }
        }
        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

fn cancel_check(cancel: &CancellationToken) -> Result<(), ServiceError> {
    if cancel.is_cancelled() {
        Err(ServiceError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{HealthState, ServiceFailureKind, ServiceStatus};
    use protocol::TraceContext;
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::thread;
    use std::time::SystemTime;

    struct Probe {
        id: ServiceId,
        deps: Vec<ServiceId>,
        health: HealthState,
        log: Arc<Mutex<Vec<String>>>,
        fail_start: bool,
        cancel_on_start: Option<CancellationToken>,
    }

    impl Probe {
        fn new(id: &str, deps: &[&str], log: &Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                id: ServiceId::parse(id).expect("id"),
                deps: deps
                    .iter()
                    .map(|d| ServiceId::parse(*d).expect("dep"))
                    .collect(),
                health: HealthState::new(ServiceStatus::Stopped, 0),
                log: Arc::clone(log),
                fail_start: false,
                cancel_on_start: None,
            }
        }

        fn fail_start(mut self) -> Self {
            self.fail_start = true;
            self
        }

        fn cancel_on_start(mut self, cancel: CancellationToken) -> Self {
            self.cancel_on_start = Some(cancel);
            self
        }

        fn record(&self, op: &str) {
            self.log
                .lock()
                .expect("log mutex")
                .push(format!("{op}:{}", self.id.as_str()));
        }
    }

    impl LifecycleService for Probe {
        fn id(&self) -> &ServiceId {
            &self.id
        }

        fn dependencies(&self) -> &[ServiceId] {
            &self.deps
        }

        async fn start(&self, ctx: ServiceContext) -> Result<(), ServiceError> {
            self.record("start");
            ctx.check()?;
            if let Some(cancel) = &self.cancel_on_start {
                cancel.cancel();
            }
            if self.fail_start {
                self.health.record(
                    ServiceStatus::Failed,
                    Some(ServiceFailureKind::Failed),
                    unix_now_ms(),
                );
                return Err(ServiceError::Failed);
            }
            self.health
                .record(ServiceStatus::Running, None, unix_now_ms());
            Ok(())
        }

        async fn quiesce(&self, deadline: Instant) -> Result<(), ServiceError> {
            self.record("quiesce");
            if Instant::now() >= deadline {
                self.health.record(
                    ServiceStatus::Failed,
                    Some(ServiceFailureKind::DeadlineExceeded),
                    unix_now_ms(),
                );
                return Err(ServiceError::DeadlineExceeded);
            }
            self.health
                .record(ServiceStatus::Quiescing, None, unix_now_ms());
            Ok(())
        }

        async fn stop(&self) -> Result<(), ServiceError> {
            self.record("stop");
            self.health
                .record(ServiceStatus::Stopped, None, unix_now_ms());
            Ok(())
        }

        fn health(&self) -> HealthSnapshot {
            HealthSnapshot::new(self.id.clone(), self.health.snapshot())
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn ctx(cancel: &CancellationToken) -> ServiceContext {
        ServiceContext::new(cancel.clone(), TraceContext::root())
    }

    fn log_of(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        log.lock().expect("log mutex").clone()
    }

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn unix_now_ms() -> u64 {
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }

    fn block_on<F: Future>(fut: F) -> F::Output {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(out) => return out,
                Poll::Pending => thread::yield_now(),
            }
        }
    }

    #[test]
    fn start_all_uses_dependency_first_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut graph = ServiceGraph::new(
            [
                Probe::new("policy", &["event-ledger", "config"], &log),
                Probe::new("config", &[], &log),
                Probe::new("event-ledger", &["config"], &log),
            ],
            &live(),
        )
        .expect("graph");
        let order: Vec<_> = graph.start_order().map(ServiceId::as_str).collect();
        let pos = |name: &str| order.iter().position(|id| *id == name).expect("present");
        assert!(pos("config") < pos("event-ledger"));
        assert!(pos("event-ledger") < pos("policy"));

        block_on(graph.start_all(ctx(&live()))).expect("start");
        assert_eq!(graph.phase(), GraphPhase::Running);
        assert_eq!(
            log_of(&log),
            vec!["start:config", "start:event-ledger", "start:policy",]
        );
        let started: Vec<_> = graph.started().map(ServiceId::as_str).collect();
        assert_eq!(started, vec!["config", "event-ledger", "policy"]);
    }

    #[test]
    fn start_failure_reverse_quiesces_and_stops_already_started() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut graph = ServiceGraph::new(
            [
                Probe::new("config", &[], &log),
                Probe::new("event-ledger", &["config"], &log),
                Probe::new("policy", &["event-ledger"], &log).fail_start(),
            ],
            &live(),
        )
        .expect("graph");

        let err = block_on(graph.start_all(ctx(&live()))).expect_err("injected fail");
        assert_eq!(err, ServiceError::Failed);
        assert!(graph.started().next().is_none());
        assert_eq!(
            log_of(&log),
            vec![
                "start:config",
                "start:event-ledger",
                "start:policy",
                "quiesce:event-ledger",
                "stop:event-ledger",
                "quiesce:config",
                "stop:config",
            ]
        );

        let by_id = |name: &str| {
            graph
                .services()
                .iter()
                .find(|s| s.id().as_str() == name)
                .expect("service")
                .health()
                .status()
        };
        assert_eq!(by_id("config"), ServiceStatus::Stopped);
        assert_eq!(by_id("event-ledger"), ServiceStatus::Stopped);
        assert_eq!(by_id("policy"), ServiceStatus::Failed);
        assert_ne!(graph.phase(), GraphPhase::Running);
    }

    #[test]
    fn shutdown_does_not_start_new_dependent_services() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancellationToken::new();
        let mut graph = ServiceGraph::new(
            [
                Probe::new("config", &[], &log).cancel_on_start(cancel.clone()),
                Probe::new("event-ledger", &["config"], &log),
                Probe::new("policy", &["event-ledger"], &log),
            ],
            &cancel,
        )
        .expect("graph");

        let err = block_on(graph.start_all(ctx(&cancel))).expect_err("cancelled");
        assert_eq!(err, ServiceError::Cancelled);
        let events = log_of(&log);
        assert_eq!(
            events,
            vec!["start:config", "quiesce:config", "stop:config"]
        );
        assert!(!events.iter().any(|e| e.starts_with("start:event-ledger")));
        assert!(!events.iter().any(|e| e.starts_with("start:policy")));
        assert!(graph.started().next().is_none());

        let log = Arc::new(Mutex::new(Vec::new()));
        let mut graph = ServiceGraph::new(
            [
                Probe::new("config", &[], &log),
                Probe::new("event-ledger", &["config"], &log),
            ],
            &live(),
        )
        .expect("graph");
        block_on(graph.shutdown(far_deadline(), &live())).expect("idle shutdown");
        assert_eq!(graph.phase(), GraphPhase::Stopped);
        let err = block_on(graph.start_all(ctx(&live()))).expect_err("terminal");
        assert_eq!(err, ServiceError::Cancelled);
        assert!(log_of(&log).is_empty());
    }

    #[test]
    fn shutdown_reverse_quiesces_running_graph() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut graph = ServiceGraph::new(
            [
                Probe::new("config", &[], &log),
                Probe::new("event-ledger", &["config"], &log),
            ],
            &live(),
        )
        .expect("graph");
        block_on(graph.start_all(ctx(&live()))).expect("start");
        block_on(graph.shutdown(far_deadline(), &live())).expect("shutdown");
        assert_eq!(graph.phase(), GraphPhase::Stopped);
        assert!(graph.is_shutting_down());
        assert_eq!(
            log_of(&log),
            vec![
                "start:config",
                "start:event-ledger",
                "quiesce:event-ledger",
                "stop:event-ledger",
                "quiesce:config",
                "stop:config",
            ]
        );
        let err = block_on(graph.start_all(ctx(&live()))).expect_err("terminal");
        assert_eq!(err, ServiceError::Cancelled);
        assert_eq!(log_of(&log).len(), 6);
    }

    #[test]
    fn shutdown_deadline_force_cancels_then_stops() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut graph = ServiceGraph::new(
            [
                Probe::new("config", &[], &log),
                Probe::new("event-ledger", &["config"], &log),
            ],
            &live(),
        )
        .expect("graph");
        block_on(graph.start_all(ctx(&live()))).expect("start");

        let cancel = CancellationToken::new();
        let err = block_on(graph.shutdown(Instant::now(), &cancel)).expect_err("deadline");
        assert_eq!(err, ServiceError::DeadlineExceeded);
        assert!(cancel.is_cancelled());
        assert_eq!(
            log_of(&log),
            vec![
                "start:config",
                "start:event-ledger",
                "quiesce:event-ledger",
                "stop:event-ledger",
                "quiesce:config",
                "stop:config",
            ]
        );
        assert!(graph.started().next().is_none());
    }

    #[test]
    fn cancelled_construction_fails_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let log = Arc::new(Mutex::new(Vec::new()));
        match ServiceGraph::new([Probe::new("config", &[], &log)], &cancel) {
            Err(err) => assert_eq!(err, ServiceError::Cancelled),
            Ok(_) => panic!("expected cancelled construction"),
        }
    }

    #[test]
    fn cycle_is_rejected_before_start() {
        let log = Arc::new(Mutex::new(Vec::new()));
        match ServiceGraph::new(
            [
                Probe::new("alpha", &["beta"], &log),
                Probe::new("beta", &["alpha"], &log),
            ],
            &live(),
        ) {
            Err(ServiceError::DependencyCycle { .. }) => {}
            Err(err) => panic!("expected cycle, got {err:?}"),
            Ok(_) => panic!("expected cycle"),
        }
    }
}
