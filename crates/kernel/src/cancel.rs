//! Hierarchical cancellation: session → turn → model/tool/job.
//!
//! Client-lifetime children inherit parent cancel. A detached daemon job
//! requires an explicit [`CancelLifetime::Daemon`] and does not inherit
//! client/parent cancellation.

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use protocol::{ApiError, ErrorCode, JobId, SessionId, TraceId, TurnId, UNKNOWN_INTERNAL_MESSAGE};

/// Maximum live children retained on one cancellation node.
pub const MAX_CANCEL_CHILDREN: usize = 256;

/// Maximum depth from a root (`0`) through session → turn → leaf.
pub const MAX_CANCEL_DEPTH: u8 = 3;

/// Lifetime that decides whether a node inherits parent/client cancel.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CancelLifetime {
    Client,
    Daemon,
}

/// Structured owner of one node in the session → turn → child tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CancelOwner {
    Client,
    Session(SessionId),
    Turn(TurnId),
    Model,
    Tool,
    Job { id: JobId, lifetime: CancelLifetime },
}

/// Cooperative hierarchical cancellation token.
#[derive(Clone)]
pub struct CancelToken {
    node: Arc<Node>,
}

/// Factory for hierarchical cancellation tokens.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CancellationTree;

/// Typed cancellation-tree failure. Public mapping uses [`CancelError::code`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelError {
    Cancelled,
    InvalidHierarchy,
    InvalidLifetime,
    TooManyChildren,
    DepthExceeded,
}

struct Node {
    owner: CancelOwner,
    cancelled: AtomicBool,
    depth: u8,
    state: Mutex<NodeState>,
    cv: Condvar,
}

struct NodeState {
    children: Vec<Weak<Node>>,
}

impl CancelLifetime {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Daemon => "daemon",
        }
    }
}

impl CancelOwner {
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Session(_) => "session",
            Self::Turn(_) => "turn",
            Self::Model => "model",
            Self::Tool => "tool",
            Self::Job { .. } => "job",
        }
    }

    pub const fn lifetime(self) -> CancelLifetime {
        match self {
            Self::Job { lifetime, .. } => lifetime,
            _ => CancelLifetime::Client,
        }
    }

    /// Whether this owner inherits parent cancellation.
    pub const fn inherits_parent(self) -> bool {
        !matches!(
            self,
            Self::Job {
                lifetime: CancelLifetime::Daemon,
                ..
            }
        )
    }
}

impl CancellationTree {
    /// Create a client or session root. Other owners must be children.
    pub fn root(owner: CancelOwner) -> Result<CancelToken, CancelError> {
        match owner {
            CancelOwner::Client | CancelOwner::Session(_) => Ok(CancelToken {
                node: Arc::new(Node {
                    owner,
                    cancelled: AtomicBool::new(false),
                    depth: 0,
                    state: Mutex::new(NodeState {
                        children: Vec::new(),
                    }),
                    cv: Condvar::new(),
                }),
            }),
            CancelOwner::Job {
                lifetime: CancelLifetime::Daemon,
                ..
            } => Err(CancelError::InvalidLifetime),
            _ => Err(CancelError::InvalidHierarchy),
        }
    }

    /// Attach `owner` under `parent`.
    ///
    /// Client-lifetime owners inherit parent cancel. A daemon job must set
    /// [`CancelLifetime::Daemon`] and does not inherit client cancellation.
    pub fn child(parent: &CancelToken, owner: CancelOwner) -> Result<CancelToken, CancelError> {
        validate_child(parent.owner(), owner)?;
        let depth = parent
            .node
            .depth
            .checked_add(1)
            .filter(|next| *next <= MAX_CANCEL_DEPTH)
            .ok_or(CancelError::DepthExceeded)?;
        if owner.inherits_parent() && parent.is_cancelled() {
            return Err(CancelError::Cancelled);
        }

        let child = Arc::new(Node {
            owner,
            cancelled: AtomicBool::new(false),
            depth,
            state: Mutex::new(NodeState {
                children: Vec::new(),
            }),
            cv: Condvar::new(),
        });

        {
            let mut state = lock_state(&parent.node.state);
            state.children.retain(|child| child.strong_count() > 0);
            if state.children.len() >= MAX_CANCEL_CHILDREN {
                return Err(CancelError::TooManyChildren);
            }
            if owner.inherits_parent() && parent.is_cancelled() {
                return Err(CancelError::Cancelled);
            }
            state.children.push(Arc::downgrade(&child));
        }

        if owner.inherits_parent() && parent.is_cancelled() {
            child.cancel();
        }

        Ok(CancelToken { node: child })
    }
}

impl CancelToken {
    pub fn owner(&self) -> CancelOwner {
        self.node.owner
    }

    pub fn lifetime(&self) -> CancelLifetime {
        self.node.owner.lifetime()
    }

    pub fn is_cancelled(&self) -> bool {
        self.node.is_cancelled()
    }

    pub fn check(&self) -> Result<(), CancelError> {
        if self.is_cancelled() {
            Err(CancelError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Cancel this node and every inheriting descendant. Idempotent.
    pub fn cancel(&self) {
        self.node.cancel();
    }

    /// Block until this node is cancelled. Returns immediately if already set.
    pub fn wait(&self) {
        let mut state = lock_state(&self.node.state);
        while !self.node.is_cancelled() {
            state = wait_state(&self.node.cv, state);
        }
    }

    /// Block until cancelled or `timeout` elapses.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = lock_state(&self.node.state);
        while !self.node.is_cancelled() {
            let Some(deadline) = deadline else {
                state = wait_state(&self.node.cv, state);
                continue;
            };
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return self.node.is_cancelled();
            };
            let (next, result) = wait_timeout_state(&self.node.cv, state, remaining);
            state = next;
            if result.timed_out() {
                return self.node.is_cancelled();
            }
        }
        true
    }
}

impl Node {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        let inheriting = {
            let mut state = lock_state(&self.state);
            self.cv.notify_all();
            let mut live = Vec::with_capacity(state.children.len());
            state.children.retain(|child| {
                if let Some(child) = child.upgrade() {
                    if child.owner.inherits_parent() {
                        live.push(child);
                    }
                    true
                } else {
                    false
                }
            });
            live
        };
        for child in inheriting {
            child.cancel();
        }
    }
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelToken")
            .field("owner", &self.node.owner)
            .field("lifetime", &self.lifetime())
            .field("cancelled", &self.is_cancelled())
            .field("depth", &self.node.depth)
            .finish()
    }
}

impl CancelError {
    /// Public error code when this failure has a wire mapping.
    ///
    /// [`CancelError::Cancelled`] has no public code.
    pub fn code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::InvalidHierarchy
            | Self::InvalidLifetime
            | Self::TooManyChildren
            | Self::DepthExceeded => Some(ErrorCode::InternalUnexpected),
        }
    }

    /// Convert to the public envelope. Cancellation is not an API error.
    pub fn into_api_error(self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = match self {
            Self::Cancelled => return None,
            Self::InvalidHierarchy => "Invalid cancellation hierarchy",
            Self::InvalidLifetime => "Invalid cancellation lifetime",
            Self::TooManyChildren | Self::DepthExceeded => UNKNOWN_INTERNAL_MESSAGE,
        };
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }
}

impl fmt::Display for CancelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancellation tree cancelled"),
            Self::InvalidHierarchy => f.write_str("invalid cancellation hierarchy"),
            Self::InvalidLifetime => f.write_str("invalid cancellation lifetime"),
            Self::TooManyChildren => f.write_str("cancellation node child bound exceeded"),
            Self::DepthExceeded => f.write_str("cancellation tree depth bound exceeded"),
        }
    }
}

impl Error for CancelError {}

impl fmt::Display for CancelLifetime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn validate_child(parent: CancelOwner, child: CancelOwner) -> Result<(), CancelError> {
    if matches!(child, CancelOwner::Client) {
        return Err(CancelError::InvalidHierarchy);
    }
    if matches!(
        child,
        CancelOwner::Job {
            lifetime: CancelLifetime::Daemon,
            ..
        }
    ) && !matches!(parent, CancelOwner::Session(_) | CancelOwner::Turn(_))
    {
        return Err(CancelError::InvalidLifetime);
    }
    match (parent, child) {
        (CancelOwner::Client, CancelOwner::Session(_))
        | (CancelOwner::Session(_), CancelOwner::Turn(_))
        | (
            CancelOwner::Session(_),
            CancelOwner::Job {
                lifetime: CancelLifetime::Daemon,
                ..
            },
        )
        | (
            CancelOwner::Turn(_),
            CancelOwner::Model | CancelOwner::Tool | CancelOwner::Job { .. },
        ) => Ok(()),
        _ => Err(CancelError::InvalidHierarchy),
    }
}

fn lock_state(mutex: &Mutex<NodeState>) -> MutexGuard<'_, NodeState> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_state<'a>(cv: &'a Condvar, state: MutexGuard<'a, NodeState>) -> MutexGuard<'a, NodeState> {
    cv.wait(state)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_timeout_state<'a>(
    cv: &'a Condvar,
    state: MutexGuard<'a, NodeState>,
    timeout: Duration,
) -> (MutexGuard<'a, NodeState>, std::sync::WaitTimeoutResult) {
    cv.wait_timeout(state, timeout)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    fn client() -> CancelToken {
        CancellationTree::root(CancelOwner::Client).expect("client root")
    }

    fn session_under(parent: &CancelToken) -> CancelToken {
        CancellationTree::child(parent, CancelOwner::Session(SessionId::new())).expect("session")
    }

    fn turn_under(parent: &CancelToken) -> CancelToken {
        CancellationTree::child(parent, CancelOwner::Turn(TurnId::new())).expect("turn")
    }

    fn job(lifetime: CancelLifetime) -> CancelOwner {
        CancelOwner::Job {
            id: JobId::new(),
            lifetime,
        }
    }

    #[test]
    fn parent_cancel_wakes_descendants_once() {
        let client = client();
        let session = session_under(&client);
        let turn = turn_under(&session);
        let model = CancellationTree::child(&turn, CancelOwner::Model).expect("model");
        let tool = CancellationTree::child(&turn, CancelOwner::Tool).expect("tool");
        let fg_job = CancellationTree::child(&turn, job(CancelLifetime::Client)).expect("job");

        let wakes = Arc::new(AtomicUsize::new(0));
        let (ready_tx, ready_rx) = mpsc::channel();
        let waiters = [model.clone(), tool.clone(), fg_job.clone()];
        let handles: Vec<_> = waiters
            .into_iter()
            .map(|token| {
                let wakes = Arc::clone(&wakes);
                let ready_tx = ready_tx.clone();
                thread::spawn(move || {
                    ready_tx.send(()).expect("ready");
                    token.wait();
                    wakes.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();
        drop(ready_tx);
        for _ in 0..3 {
            ready_rx.recv().expect("waiter started");
        }
        thread::sleep(Duration::from_millis(20));

        session.cancel();
        session.cancel();
        for handle in handles {
            handle.join().expect("waiter");
        }

        assert_eq!(wakes.load(Ordering::SeqCst), 3);
        assert!(session.is_cancelled());
        assert!(turn.is_cancelled());
        assert!(model.is_cancelled());
        assert!(tool.is_cancelled());
        assert!(fg_job.is_cancelled());
        assert!(!client.is_cancelled());
        assert_eq!(session.check(), Err(CancelError::Cancelled));
    }

    #[test]
    fn cancel_is_idempotent_and_does_not_climb() {
        let session =
            CancellationTree::root(CancelOwner::Session(SessionId::new())).expect("session");
        let turn = turn_under(&session);
        turn.cancel();
        turn.cancel();
        assert!(turn.is_cancelled());
        assert!(!session.is_cancelled());
        session.cancel();
        session.cancel();
        assert!(session.is_cancelled());
    }

    #[test]
    fn detached_daemon_job_requires_explicit_lifetime_and_skips_client_cancel() {
        let client = client();
        let session = session_under(&client);
        let turn = turn_under(&session);
        let fg = CancellationTree::child(&turn, job(CancelLifetime::Client)).expect("client job");
        let daemon =
            CancellationTree::child(&session, job(CancelLifetime::Daemon)).expect("daemon job");

        assert_eq!(fg.lifetime(), CancelLifetime::Client);
        assert_eq!(daemon.lifetime(), CancelLifetime::Daemon);
        assert!(fg.owner().inherits_parent());
        assert!(!daemon.owner().inherits_parent());

        client.cancel();
        assert!(session.is_cancelled());
        assert!(turn.is_cancelled());
        assert!(fg.is_cancelled());
        assert!(!daemon.is_cancelled());
        assert_eq!(daemon.check(), Ok(()));

        daemon.cancel();
        assert!(daemon.is_cancelled());
    }

    #[test]
    fn implicit_job_lifetime_is_client_and_inherits() {
        let session =
            CancellationTree::root(CancelOwner::Session(SessionId::new())).expect("session");
        let turn = turn_under(&session);
        let implicit = CancellationTree::child(&turn, job(CancelLifetime::Client)).expect("job");
        session.cancel();
        assert!(implicit.is_cancelled());
    }

    #[test]
    fn hierarchy_and_lifetime_are_typed() {
        let client = client();
        let session = session_under(&client);
        assert_eq!(
            CancellationTree::child(&client, CancelOwner::Turn(TurnId::new()))
                .expect_err("turn under client"),
            CancelError::InvalidHierarchy
        );
        assert_eq!(
            CancellationTree::child(&session, CancelOwner::Model).expect_err("model under session"),
            CancelError::InvalidHierarchy
        );
        assert_eq!(
            CancellationTree::child(&session, job(CancelLifetime::Client))
                .expect_err("client job under session"),
            CancelError::InvalidHierarchy
        );
        assert_eq!(
            CancellationTree::root(CancelOwner::Model).expect_err("model root"),
            CancelError::InvalidHierarchy
        );
        assert_eq!(
            CancellationTree::root(job(CancelLifetime::Daemon)).expect_err("daemon root"),
            CancelError::InvalidLifetime
        );
        assert_eq!(
            CancellationTree::child(&client, job(CancelLifetime::Daemon))
                .expect_err("daemon under client"),
            CancelError::InvalidLifetime
        );
        let turn = turn_under(&session);
        assert!(CancellationTree::child(&turn, job(CancelLifetime::Daemon)).is_ok());
    }

    #[test]
    fn cancelled_parent_rejects_inheriting_child_not_daemon() {
        let session =
            CancellationTree::root(CancelOwner::Session(SessionId::new())).expect("session");
        session.cancel();
        assert_eq!(
            CancellationTree::child(&session, CancelOwner::Turn(TurnId::new()))
                .expect_err("cancelled parent"),
            CancelError::Cancelled
        );
        let daemon =
            CancellationTree::child(&session, job(CancelLifetime::Daemon)).expect("daemon");
        assert!(!daemon.is_cancelled());
    }

    #[test]
    fn child_bound_and_depth_are_enforced() {
        let session =
            CancellationTree::root(CancelOwner::Session(SessionId::new())).expect("session");
        let turn = turn_under(&session);
        let mut children = Vec::with_capacity(MAX_CANCEL_CHILDREN);
        for _ in 0..MAX_CANCEL_CHILDREN {
            children.push(CancellationTree::child(&turn, CancelOwner::Model).expect("child"));
        }
        assert_eq!(
            CancellationTree::child(&turn, CancelOwner::Tool).expect_err("bound"),
            CancelError::TooManyChildren
        );
        drop(children);
        assert!(CancellationTree::child(&turn, CancelOwner::Tool).is_ok());

        let model = CancellationTree::child(&turn, CancelOwner::Model).expect("model");
        assert_eq!(
            CancellationTree::child(&model, job(CancelLifetime::Client)).expect_err("depth"),
            CancelError::InvalidHierarchy
        );
    }

    #[test]
    fn wait_timeout_observes_cancel_and_deadline() {
        let token = client();
        let started = Instant::now();
        assert!(!token.wait_timeout(Duration::from_millis(15)));
        assert!(started.elapsed() >= Duration::from_millis(10));
        token.cancel();
        assert!(token.wait_timeout(Duration::from_millis(15)));
    }

    #[test]
    fn concurrent_cancel_is_idempotent() {
        let session =
            CancellationTree::root(CancelOwner::Session(SessionId::new())).expect("session");
        let turn = turn_under(&session);
        let model = CancellationTree::child(&turn, CancelOwner::Model).expect("model");
        thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| session.cancel());
            }
        });
        assert!(model.is_cancelled());
        assert!(turn.is_cancelled());
    }

    #[test]
    fn cancelled_is_not_an_api_error() {
        let trace = TraceId::new();
        assert_eq!(CancelError::Cancelled.code(), None);
        assert!(CancelError::Cancelled.into_api_error(trace).is_none());
        let err = CancelError::InvalidHierarchy
            .into_api_error(trace)
            .expect("mapped");
        assert_eq!(err.code(), ErrorCode::InternalUnexpected);
    }
}
