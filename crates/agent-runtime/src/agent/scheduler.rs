//! Agent scheduler queues: concurrency, priority, cost, and cancellation.
//!
//! Write-capable agents never share a view. Per-provider and global slots are
//! claimed atomically under the scheduler lock. Parent cancel cascades unless
//! the child was enqueued as detached.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use protocol::{AgentId, AgentsConfig, ErrorCode, WorkspaceViewId};

use super::model::{
    AgentBudget, AgentRole, AgentTerminalStatus, CancellationToken, MAX_TASK_BYTES, ModelPolicyRef,
};

/// Maximum live (non-terminal) agents admitted by default.
pub const MAX_SCHEDULED_AGENTS: usize = 256;

/// Maximum UTF-8 bytes accepted in [`ProviderKey`].
pub const MAX_PROVIDER_KEY_BYTES: usize = 64;

/// Maximum UTF-8 bytes accepted in [`ResultSchema`].
pub const MAX_RESULT_SCHEMA_BYTES: usize = 256;

/// Workspace access requested at enqueue. `WriteIsolated` owns its view exclusively.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum WorkspaceAccess {
    ReadOnly,
    WriteIsolated,
}

/// Occupancy class that rejected dispatch or enqueue.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum LimitKind {
    Global,
    Provider,
    Write,
    Cost,
    Queue,
}

/// Scheduler-visible lifecycle. Distinct from full [`super::model::AgentState`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum ScheduleState {
    Queued,
    Running,
    Paused,
    Succeeded,
    Failed,
    Cancelled,
}

/// Higher numeric value is selected first. Ties break by enqueue sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct AgentPriority(u8);

/// Bounded provider identity used for per-provider concurrency.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ProviderKey(String);

/// Expected child result contract name. Empty names are rejected.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ResultSchema(String);

/// Immutable concurrency and cost ceilings. Zero concurrency never silently widens.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SchedulerLimits {
    max_parallel: u32,
    max_write_parallel: u32,
    max_per_provider: u32,
    max_cost: Option<u64>,
    max_queued: usize,
}

/// Typed spawn request admitted by [`Scheduler::enqueue`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnAgent {
    id: Option<AgentId>,
    parent: AgentId,
    role: AgentRole,
    task: String,
    access: WorkspaceAccess,
    budget: AgentBudget,
    model_policy: ModelPolicyRef,
    expected_result: ResultSchema,
    workspace_view: WorkspaceViewId,
    provider: ProviderKey,
    priority: AgentPriority,
    detached: bool,
}

/// Handle returned by enqueue. Cancellation is cooperative via the token.
#[derive(Clone, Debug)]
pub struct AgentHandle {
    id: AgentId,
    cancel: CancellationToken,
}

/// Observational snapshot of one scheduled agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledAgent {
    id: AgentId,
    parent: AgentId,
    role: AgentRole,
    access: WorkspaceAccess,
    workspace_view: WorkspaceViewId,
    provider: ProviderKey,
    priority: AgentPriority,
    state: ScheduleState,
    detached: bool,
    seq: u64,
}

/// Occupied slot counts. Does not include queued or paused agents.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct SchedulerOccupancy {
    global: usize,
    write: usize,
}

/// Typed scheduler failure. Display never echoes task or schema text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerError {
    Cancelled,
    InvalidTask,
    InvalidProvider,
    InvalidResultSchema,
    InvalidLimits,
    QueueFull {
        limit: usize,
    },
    AgentNotFound {
        agent_id: AgentId,
    },
    DuplicateAgent {
        agent_id: AgentId,
    },
    SharedWriteView {
        view_id: WorkspaceViewId,
    },
    ConcurrencyLimit {
        kind: LimitKind,
    },
    CostBudgetExceeded,
    ParentCancelled {
        parent: AgentId,
    },
    InvalidState {
        agent_id: AgentId,
        state: ScheduleState,
    },
}

/// In-process agent queue. Selection is deterministic for a given enqueue order.
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Mutex<Inner>>,
    limits: SchedulerLimits,
}

struct Inner {
    next_seq: u64,
    consumed_cost: u64,
    agents: BTreeMap<AgentId, Record>,
    children: HashMap<AgentId, BTreeSet<AgentId>>,
}

struct Record {
    parent: AgentId,
    role: AgentRole,
    access: WorkspaceAccess,
    workspace_view: WorkspaceViewId,
    provider: ProviderKey,
    priority: AgentPriority,
    state: ScheduleState,
    detached: bool,
    seq: u64,
    reserved_cost: u64,
    cancel: CancellationToken,
}

impl WorkspaceAccess {
    pub const ALL: &'static [Self] = &[Self::ReadOnly, Self::WriteIsolated];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WriteIsolated => "write_isolated",
        }
    }

    pub const fn is_write(self) -> bool {
        matches!(self, Self::WriteIsolated)
    }
}

impl LimitKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Provider => "provider",
            Self::Write => "write",
            Self::Cost => "cost",
            Self::Queue => "queue",
        }
    }
}

impl ScheduleState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    pub const fn occupies_slot(self) -> bool {
        matches!(self, Self::Running)
    }
}

impl AgentPriority {
    pub const MIN: Self = Self(0);
    pub const LOW: Self = Self(64);
    pub const NORMAL: Self = Self(128);
    pub const HIGH: Self = Self(192);
    pub const MAX: Self = Self(255);

    pub const fn new(raw: u8) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for AgentPriority {
    fn default() -> Self {
        Self::NORMAL
    }
}

impl ProviderKey {
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, SchedulerError> {
        let raw = raw.as_ref();
        if raw.is_empty() || raw.len() > MAX_PROVIDER_KEY_BYTES {
            return Err(SchedulerError::InvalidProvider);
        }
        let mut bytes = raw.bytes();
        let Some(first) = bytes.next() else {
            return Err(SchedulerError::InvalidProvider);
        };
        if !first.is_ascii_lowercase() {
            return Err(SchedulerError::InvalidProvider);
        }
        if !raw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return Err(SchedulerError::InvalidProvider);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ResultSchema {
    pub fn new(name: impl Into<String>) -> Result<Self, SchedulerError> {
        let name = name.into();
        if name.is_empty()
            || name.len() > MAX_RESULT_SCHEMA_BYTES
            || name.chars().any(|c| c.is_control())
        {
            return Err(SchedulerError::InvalidResultSchema);
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SchedulerLimits {
    pub const fn new(
        max_parallel: u32,
        max_write_parallel: u32,
        max_per_provider: u32,
        max_cost: Option<u64>,
        max_queued: usize,
    ) -> Result<Self, SchedulerError> {
        if max_queued == 0 {
            return Err(SchedulerError::InvalidLimits);
        }
        Ok(Self {
            max_parallel,
            max_write_parallel,
            max_per_provider,
            max_cost,
            max_queued,
        })
    }

    pub fn from_agents_config(config: &AgentsConfig) -> Result<Self, SchedulerError> {
        Self::new(
            config.max_parallel,
            config.max_write_parallel,
            config.max_parallel,
            None,
            MAX_SCHEDULED_AGENTS,
        )
    }

    pub const fn max_parallel(self) -> u32 {
        self.max_parallel
    }

    pub const fn max_write_parallel(self) -> u32 {
        self.max_write_parallel
    }

    pub const fn max_per_provider(self) -> u32 {
        self.max_per_provider
    }

    pub const fn max_cost(self) -> Option<u64> {
        self.max_cost
    }

    pub const fn max_queued(self) -> usize {
        self.max_queued
    }
}

impl Default for SchedulerLimits {
    fn default() -> Self {
        Self {
            max_parallel: 4,
            max_write_parallel: 2,
            max_per_provider: 4,
            max_cost: None,
            max_queued: MAX_SCHEDULED_AGENTS,
        }
    }
}

/// Construction handle for the contract [`SpawnAgent`] fields plus view/provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnAgentBuilder {
    id: Option<AgentId>,
    parent: AgentId,
    role: AgentRole,
    task: String,
    access: WorkspaceAccess,
    budget: AgentBudget,
    model_policy: ModelPolicyRef,
    expected_result: ResultSchema,
    workspace_view: WorkspaceViewId,
    provider: ProviderKey,
    priority: AgentPriority,
    detached: bool,
}

impl SpawnAgent {
    pub fn builder(
        parent: AgentId,
        role: AgentRole,
        task: impl Into<String>,
        access: WorkspaceAccess,
        expected_result: ResultSchema,
        workspace_view: WorkspaceViewId,
        provider: ProviderKey,
    ) -> SpawnAgentBuilder {
        SpawnAgentBuilder {
            id: None,
            parent,
            role,
            task: task.into(),
            access,
            budget: AgentBudget::unlimited(),
            model_policy: ModelPolicyRef::default(),
            expected_result,
            workspace_view,
            provider,
            priority: AgentPriority::NORMAL,
            detached: false,
        }
    }
}

impl SpawnAgentBuilder {
    pub fn budget(mut self, budget: AgentBudget) -> Self {
        self.budget = budget;
        self
    }

    pub fn model_policy(mut self, model_policy: ModelPolicyRef) -> Self {
        self.model_policy = model_policy;
        self
    }

    pub fn with_id(mut self, id: AgentId) -> Self {
        self.id = Some(id);
        self
    }

    pub fn with_priority(mut self, priority: AgentPriority) -> Self {
        self.priority = priority;
        self
    }

    /// Detached children do not inherit parent cancellation.
    pub fn detached(mut self) -> Self {
        self.detached = true;
        self
    }

    pub fn build(self) -> Result<SpawnAgent, SchedulerError> {
        let spawn = SpawnAgent {
            id: self.id,
            parent: self.parent,
            role: self.role,
            task: self.task,
            access: self.access,
            budget: self.budget,
            model_policy: self.model_policy,
            expected_result: self.expected_result,
            workspace_view: self.workspace_view,
            provider: self.provider,
            priority: self.priority,
            detached: self.detached,
        };
        spawn.validate()?;
        Ok(spawn)
    }
}

impl SpawnAgent {
    pub fn with_id(mut self, id: AgentId) -> Self {
        self.id = Some(id);
        self
    }

    pub fn with_priority(mut self, priority: AgentPriority) -> Self {
        self.priority = priority;
        self
    }

    /// Detached children do not inherit parent cancellation.
    pub fn detached(mut self) -> Self {
        self.detached = true;
        self
    }

    pub fn parent(&self) -> AgentId {
        self.parent
    }

    pub fn role(&self) -> AgentRole {
        self.role
    }

    pub fn task(&self) -> &str {
        &self.task
    }

    pub fn access(&self) -> WorkspaceAccess {
        self.access
    }

    pub fn budget(&self) -> AgentBudget {
        self.budget
    }

    pub fn model_policy(&self) -> &ModelPolicyRef {
        &self.model_policy
    }

    pub fn expected_result(&self) -> &ResultSchema {
        &self.expected_result
    }

    pub fn workspace_view(&self) -> WorkspaceViewId {
        self.workspace_view
    }

    pub fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    pub fn priority(&self) -> AgentPriority {
        self.priority
    }

    pub fn is_detached(&self) -> bool {
        self.detached
    }

    fn validate(&self) -> Result<(), SchedulerError> {
        if self.task.is_empty() || self.task.len() > MAX_TASK_BYTES {
            return Err(SchedulerError::InvalidTask);
        }
        Ok(())
    }

    fn reserved_cost(&self) -> Result<u64, SchedulerError> {
        match self.budget.max_cost() {
            Some(cost) => Ok(cost),
            None => Ok(0),
        }
    }
}

impl AgentHandle {
    pub fn id(&self) -> AgentId {
        self.id
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl ScheduledAgent {
    pub fn id(&self) -> AgentId {
        self.id
    }

    pub fn parent(&self) -> AgentId {
        self.parent
    }

    pub fn role(&self) -> AgentRole {
        self.role
    }

    pub fn access(&self) -> WorkspaceAccess {
        self.access
    }

    pub fn workspace_view(&self) -> WorkspaceViewId {
        self.workspace_view
    }

    pub fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    pub fn priority(&self) -> AgentPriority {
        self.priority
    }

    pub fn state(&self) -> ScheduleState {
        self.state
    }

    pub fn is_detached(&self) -> bool {
        self.detached
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }
}

impl SchedulerOccupancy {
    pub const fn global(self) -> usize {
        self.global
    }

    pub const fn write(self) -> usize {
        self.write
    }
}

impl Scheduler {
    pub fn new(limits: SchedulerLimits) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                next_seq: 1,
                consumed_cost: 0,
                agents: BTreeMap::new(),
                children: HashMap::new(),
            })),
            limits,
        }
    }

    pub fn from_agents_config(config: &AgentsConfig) -> Result<Self, SchedulerError> {
        Ok(Self::new(SchedulerLimits::from_agents_config(config)?))
    }

    pub fn limits(&self) -> SchedulerLimits {
        self.limits
    }

    /// Admit `spawn` in `queued` and return a handle. Selection stays deterministic.
    pub fn enqueue(&self, spawn: SpawnAgent) -> Result<AgentHandle, SchedulerError> {
        spawn.validate()?;
        let reserved_cost = spawn.reserved_cost()?;
        let cancel = CancellationToken::new();
        let mut inner = self.lock();

        if self.live_count(&inner) >= self.limits.max_queued {
            return Err(SchedulerError::QueueFull {
                limit: self.limits.max_queued,
            });
        }

        if let Some(parent) = inner.agents.get(&spawn.parent) {
            if parent.state == ScheduleState::Cancelled && !spawn.detached {
                return Err(SchedulerError::ParentCancelled {
                    parent: spawn.parent,
                });
            }
            if parent.cancel.is_cancelled() && !spawn.detached {
                return Err(SchedulerError::ParentCancelled {
                    parent: spawn.parent,
                });
            }
        }

        if spawn.access.is_write()
            && write_view_owner(&inner, spawn.workspace_view, spawn.id).is_some()
        {
            return Err(SchedulerError::SharedWriteView {
                view_id: spawn.workspace_view,
            });
        }

        if let Some(max_cost) = self.limits.max_cost {
            if spawn.budget.max_cost().is_none() {
                return Err(SchedulerError::CostBudgetExceeded);
            }
            let reserved = reserved_live(&inner);
            let committed = inner
                .consumed_cost
                .saturating_add(reserved)
                .saturating_add(reserved_cost);
            if committed > max_cost {
                return Err(SchedulerError::CostBudgetExceeded);
            }
        }

        let id = spawn.id.unwrap_or_else(AgentId::new);
        if inner.agents.contains_key(&id) {
            return Err(SchedulerError::DuplicateAgent { agent_id: id });
        }

        let seq = inner.next_seq;
        inner.next_seq = inner.next_seq.saturating_add(1);
        let record = Record {
            parent: spawn.parent,
            role: spawn.role,
            access: spawn.access,
            workspace_view: spawn.workspace_view,
            provider: spawn.provider.clone(),
            priority: spawn.priority,
            state: ScheduleState::Queued,
            detached: spawn.detached,
            seq,
            reserved_cost,
            cancel: cancel.clone(),
        };
        inner.agents.insert(id, record);
        inner.children.entry(spawn.parent).or_default().insert(id);

        Ok(AgentHandle { id, cancel })
    }

    /// Queued agents that currently pass every limit, highest priority first.
    pub fn runnable(&self) -> Vec<AgentId> {
        let inner = self.lock();
        self.runnable_locked(&inner)
    }

    /// Claim the next runnable agent. Concurrent callers serialize on the lock.
    pub fn dispatch(&self) -> Result<Option<AgentHandle>, SchedulerError> {
        let mut inner = self.lock();
        let Some(id) = self.runnable_locked(&inner).into_iter().next() else {
            return Ok(None);
        };
        let handle = self.start_locked(&mut inner, id)?;
        Ok(Some(handle))
    }

    /// Start a specific queued agent if it is currently eligible.
    pub fn start(&self, id: AgentId) -> Result<AgentHandle, SchedulerError> {
        let mut inner = self.lock();
        if !self.runnable_locked(&inner).contains(&id) {
            let Some(record) = inner.agents.get(&id) else {
                return Err(SchedulerError::AgentNotFound { agent_id: id });
            };
            if record.state != ScheduleState::Queued {
                return Err(SchedulerError::InvalidState {
                    agent_id: id,
                    state: record.state,
                });
            }
            return Err(self.admission_error(&inner, id, record));
        }
        self.start_locked(&mut inner, id)
    }

    pub fn pause(&self, id: AgentId) -> Result<ScheduleState, SchedulerError> {
        let mut inner = self.lock();
        let record = inner
            .agents
            .get_mut(&id)
            .ok_or(SchedulerError::AgentNotFound { agent_id: id })?;
        if record.cancel.is_cancelled() {
            return Err(SchedulerError::Cancelled);
        }
        match record.state {
            ScheduleState::Queued | ScheduleState::Running => {
                record.state = ScheduleState::Paused;
                Ok(ScheduleState::Paused)
            }
            state => Err(SchedulerError::InvalidState {
                agent_id: id,
                state,
            }),
        }
    }

    pub fn resume(&self, id: AgentId) -> Result<ScheduleState, SchedulerError> {
        let mut inner = self.lock();
        let record = inner
            .agents
            .get_mut(&id)
            .ok_or(SchedulerError::AgentNotFound { agent_id: id })?;
        if record.cancel.is_cancelled() {
            return Err(SchedulerError::Cancelled);
        }
        match record.state {
            ScheduleState::Paused => {
                record.state = ScheduleState::Queued;
                Ok(ScheduleState::Queued)
            }
            state => Err(SchedulerError::InvalidState {
                agent_id: id,
                state,
            }),
        }
    }

    pub fn set_priority(
        &self,
        id: AgentId,
        priority: AgentPriority,
    ) -> Result<AgentPriority, SchedulerError> {
        let mut inner = self.lock();
        let record = inner
            .agents
            .get_mut(&id)
            .ok_or(SchedulerError::AgentNotFound { agent_id: id })?;
        if record.state.is_terminal() {
            return Err(SchedulerError::InvalidState {
                agent_id: id,
                state: record.state,
            });
        }
        record.priority = priority;
        Ok(priority)
    }

    /// Cancel `id` and every non-detached descendant. Idempotent.
    pub fn cancel(&self, id: AgentId) -> Result<(), SchedulerError> {
        let tokens = {
            let mut inner = self.lock();
            if !inner.agents.contains_key(&id) {
                return Err(SchedulerError::AgentNotFound { agent_id: id });
            }
            cancel_tree(&mut inner, id)
        };
        for token in tokens {
            token.cancel();
        }
        Ok(())
    }

    pub fn finish(
        &self,
        id: AgentId,
        status: AgentTerminalStatus,
        spent_cost: u64,
    ) -> Result<ScheduleState, SchedulerError> {
        if status == AgentTerminalStatus::Cancelled {
            self.cancel(id)?;
            return Ok(ScheduleState::Cancelled);
        }
        let mut inner = self.lock();
        let record = inner
            .agents
            .get_mut(&id)
            .ok_or(SchedulerError::AgentNotFound { agent_id: id })?;
        if record.state.is_terminal() {
            return Err(SchedulerError::InvalidState {
                agent_id: id,
                state: record.state,
            });
        }
        if record.cancel.is_cancelled() {
            record.state = ScheduleState::Cancelled;
            return Err(SchedulerError::Cancelled);
        }
        let next = match status {
            AgentTerminalStatus::Succeeded => ScheduleState::Succeeded,
            AgentTerminalStatus::Failed => ScheduleState::Failed,
            AgentTerminalStatus::Cancelled => ScheduleState::Cancelled,
        };
        record.state = next;
        inner.consumed_cost = inner.consumed_cost.saturating_add(spent_cost);
        Ok(next)
    }

    pub fn get(&self, id: AgentId) -> Option<ScheduledAgent> {
        let inner = self.lock();
        inner.agents.get(&id).map(|record| snapshot(id, record))
    }

    pub fn occupancy(&self) -> SchedulerOccupancy {
        let inner = self.lock();
        occupancy_of(&inner)
    }

    pub fn occupancy_for_provider(&self, provider: &ProviderKey) -> usize {
        let inner = self.lock();
        occupancy_for_provider(&inner, provider)
    }

    pub fn reserved_cost(&self) -> u64 {
        let inner = self.lock();
        reserved_live(&inner)
    }

    pub fn consumed_cost(&self) -> u64 {
        self.lock().consumed_cost
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn live_count(&self, inner: &Inner) -> usize {
        inner
            .agents
            .values()
            .filter(|record| !record.state.is_terminal())
            .count()
    }

    fn runnable_locked(&self, inner: &Inner) -> Vec<AgentId> {
        let mut eligible: Vec<(AgentPriority, u64, AgentId)> = inner
            .agents
            .iter()
            .filter_map(|(&id, record)| {
                if record.state != ScheduleState::Queued || record.cancel.is_cancelled() {
                    return None;
                }
                match self.try_admit(inner, id, record) {
                    Ok(()) => Some((record.priority, record.seq, id)),
                    Err(_) => None,
                }
            })
            .collect();
        eligible.sort_by_key(|(priority, seq, id)| (Reverse(*priority), *seq, *id));
        eligible.into_iter().map(|(_, _, id)| id).collect()
    }

    fn start_locked(&self, inner: &mut Inner, id: AgentId) -> Result<AgentHandle, SchedulerError> {
        let record = inner
            .agents
            .get(&id)
            .ok_or(SchedulerError::AgentNotFound { agent_id: id })?;
        if record.cancel.is_cancelled() {
            return Err(SchedulerError::Cancelled);
        }
        self.try_admit(inner, id, record)?;
        let record = inner
            .agents
            .get_mut(&id)
            .ok_or(SchedulerError::AgentNotFound { agent_id: id })?;
        record.state = ScheduleState::Running;
        Ok(AgentHandle {
            id,
            cancel: record.cancel.clone(),
        })
    }

    fn try_admit(&self, inner: &Inner, id: AgentId, record: &Record) -> Result<(), SchedulerError> {
        if record.cancel.is_cancelled() {
            return Err(SchedulerError::Cancelled);
        }
        if let Some(parent) = inner.agents.get(&record.parent)
            && !record.detached
            && (parent.state == ScheduleState::Cancelled || parent.cancel.is_cancelled())
        {
            return Err(SchedulerError::ParentCancelled {
                parent: record.parent,
            });
        }

        let occ = occupancy_of(inner);
        if occ.global >= self.limits.max_parallel as usize {
            return Err(SchedulerError::ConcurrencyLimit {
                kind: LimitKind::Global,
            });
        }
        if record.access.is_write() && occ.write >= self.limits.max_write_parallel as usize {
            return Err(SchedulerError::ConcurrencyLimit {
                kind: LimitKind::Write,
            });
        }
        if occupancy_for_provider(inner, &record.provider) >= self.limits.max_per_provider as usize
        {
            return Err(SchedulerError::ConcurrencyLimit {
                kind: LimitKind::Provider,
            });
        }
        if record.access.is_write()
            && write_view_owner(inner, record.workspace_view, Some(id)).is_some()
        {
            return Err(SchedulerError::SharedWriteView {
                view_id: record.workspace_view,
            });
        }
        if let Some(max_cost) = self.limits.max_cost {
            let running_reserved = reserved_running(inner);
            if inner
                .consumed_cost
                .saturating_add(running_reserved)
                .saturating_add(record.reserved_cost)
                > max_cost
            {
                return Err(SchedulerError::ConcurrencyLimit {
                    kind: LimitKind::Cost,
                });
            }
        }
        Ok(())
    }

    fn admission_error(&self, inner: &Inner, id: AgentId, record: &Record) -> SchedulerError {
        match self.try_admit(inner, id, record) {
            Err(err) => err,
            Ok(()) => SchedulerError::ConcurrencyLimit {
                kind: LimitKind::Global,
            },
        }
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new(SchedulerLimits::default())
    }
}

fn snapshot(id: AgentId, record: &Record) -> ScheduledAgent {
    ScheduledAgent {
        id,
        parent: record.parent,
        role: record.role,
        access: record.access,
        workspace_view: record.workspace_view,
        provider: record.provider.clone(),
        priority: record.priority,
        state: record.state,
        detached: record.detached,
        seq: record.seq,
    }
}

fn occupancy_of(inner: &Inner) -> SchedulerOccupancy {
    let mut global = 0;
    let mut write = 0;
    for record in inner.agents.values() {
        if !record.state.occupies_slot() {
            continue;
        }
        global += 1;
        if record.access.is_write() {
            write += 1;
        }
    }
    SchedulerOccupancy { global, write }
}

fn occupancy_for_provider(inner: &Inner, provider: &ProviderKey) -> usize {
    inner
        .agents
        .values()
        .filter(|record| record.state.occupies_slot() && record.provider == *provider)
        .count()
}

fn write_view_owner(
    inner: &Inner,
    view: WorkspaceViewId,
    except: Option<AgentId>,
) -> Option<AgentId> {
    inner.agents.iter().find_map(|(&id, record)| {
        if except == Some(id) {
            return None;
        }
        if record.access.is_write() && !record.state.is_terminal() && record.workspace_view == view
        {
            Some(id)
        } else {
            None
        }
    })
}

fn reserved_live(inner: &Inner) -> u64 {
    inner
        .agents
        .values()
        .filter(|record| !record.state.is_terminal())
        .map(|record| record.reserved_cost)
        .fold(0, u64::saturating_add)
}

fn reserved_running(inner: &Inner) -> u64 {
    inner
        .agents
        .values()
        .filter(|record| record.state.occupies_slot())
        .map(|record| record.reserved_cost)
        .fold(0, u64::saturating_add)
}

fn cancel_tree(inner: &mut Inner, root: AgentId) -> Vec<CancellationToken> {
    let mut tokens = Vec::new();
    let mut stack = vec![root];
    let mut seen = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(record) = inner.agents.get_mut(&id) else {
            continue;
        };
        if record.state.is_terminal() && id != root {
            continue;
        }
        if !record.state.is_terminal() {
            record.state = ScheduleState::Cancelled;
        }
        tokens.push(record.cancel.clone());
        if let Some(children) = inner.children.get(&id) {
            for child in children {
                if let Some(child_record) = inner.agents.get(child)
                    && !child_record.detached
                {
                    stack.push(*child);
                }
            }
        }
    }
    tokens
}

impl SchedulerError {
    /// Public error code when this failure has a wire mapping.
    ///
    /// [`SchedulerError::Cancelled`] and parent-cancel inheritance have no public code.
    pub const fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled | Self::ParentCancelled { .. } => None,
            Self::QueueFull { .. }
            | Self::ConcurrencyLimit { .. }
            | Self::CostBudgetExceeded
            | Self::SharedWriteView { .. } => Some(ErrorCode::AgentConcurrencyLimit),
            Self::InvalidTask
            | Self::InvalidProvider
            | Self::InvalidResultSchema
            | Self::InvalidLimits => Some(ErrorCode::ConfigInvalid),
            Self::AgentNotFound { .. }
            | Self::DuplicateAgent { .. }
            | Self::InvalidState { .. } => Some(ErrorCode::InternalUnexpected),
        }
    }
}

impl fmt::Display for WorkspaceAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ScheduleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for AgentPriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for ProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ResultSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderKey {
    type Err = SchedulerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("scheduler operation cancelled"),
            Self::InvalidTask => f.write_str("agent task is empty or exceeds the size bound"),
            Self::InvalidProvider => f.write_str("provider key is empty, too long, or malformed"),
            Self::InvalidResultSchema => {
                f.write_str("result schema is empty, too long, or contains control characters")
            }
            Self::InvalidLimits => f.write_str("scheduler queue bound must be non-zero"),
            Self::QueueFull { limit } => {
                write!(f, "scheduler live-agent bound {limit} exceeded")
            }
            Self::AgentNotFound { .. } => f.write_str("scheduled agent was not found"),
            Self::DuplicateAgent { .. } => f.write_str("agent id is already scheduled"),
            Self::SharedWriteView { .. } => {
                f.write_str("write-capable agents cannot share a workspace view")
            }
            Self::ConcurrencyLimit { kind } => {
                write!(f, "scheduler {kind} concurrency limit reached")
            }
            Self::CostBudgetExceeded => f.write_str("scheduler global cost budget exceeded"),
            Self::ParentCancelled { .. } => {
                f.write_str("parent agent is cancelled; child inherit is denied")
            }
            Self::InvalidState { state, .. } => {
                write!(f, "illegal scheduler state {state}")
            }
        }
    }
}

impl Error for SchedulerError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn provider(name: &str) -> ProviderKey {
        ProviderKey::parse(name).expect("provider")
    }

    fn schema() -> ResultSchema {
        ResultSchema::new("rapidlm.agent_result").expect("schema")
    }

    fn policy() -> ModelPolicyRef {
        ModelPolicyRef::new("balanced").expect("policy")
    }

    fn budget(max_cost: Option<u64>) -> AgentBudget {
        AgentBudget::new(None, max_cost, None, None)
    }

    fn limits(
        max_parallel: u32,
        max_write: u32,
        max_provider: u32,
        max_cost: Option<u64>,
    ) -> SchedulerLimits {
        SchedulerLimits::new(max_parallel, max_write, max_provider, max_cost, 64).expect("limits")
    }

    fn spawn(
        parent: AgentId,
        access: WorkspaceAccess,
        view: WorkspaceViewId,
        provider_name: &str,
        priority: AgentPriority,
        max_cost: Option<u64>,
    ) -> SpawnAgent {
        SpawnAgent::builder(
            parent,
            AgentRole::Explorer,
            "inspect crate",
            access,
            schema(),
            view,
            provider(provider_name),
        )
        .budget(budget(max_cost))
        .model_policy(policy())
        .with_priority(priority)
        .build()
        .expect("spawn")
    }

    #[test]
    fn enqueue_returns_handle_and_queues() {
        let scheduler = Scheduler::new(limits(2, 1, 2, None));
        let parent = AgentId::new();
        let handle = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::NORMAL,
                None,
            ))
            .expect("enqueue");
        let queued = scheduler.get(handle.id()).expect("queued");
        assert_eq!(queued.state(), ScheduleState::Queued);
        assert_eq!(queued.parent(), parent);
        assert!(!handle.cancel_token().is_cancelled());
    }

    #[test]
    fn runnable_selection_is_deterministic_within_priority() {
        let scheduler = Scheduler::new(limits(4, 4, 4, None));
        let parent = AgentId::new();
        let low = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::LOW,
                None,
            ))
            .expect("low");
        let high_a = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::HIGH,
                None,
            ))
            .expect("high a");
        let high_b = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::HIGH,
                None,
            ))
            .expect("high b");

        assert_eq!(
            scheduler.runnable(),
            vec![high_a.id(), high_b.id(), low.id()]
        );
        assert_eq!(
            scheduler.dispatch().expect("first").map(|h| h.id()),
            Some(high_a.id())
        );
        assert_eq!(
            scheduler.dispatch().expect("second").map(|h| h.id()),
            Some(high_b.id())
        );
        assert_eq!(
            scheduler.dispatch().expect("third").map(|h| h.id()),
            Some(low.id())
        );
        assert!(scheduler.dispatch().expect("empty").is_none());
    }

    #[test]
    fn never_schedules_two_write_agents_into_same_view() {
        let scheduler = Scheduler::new(limits(4, 2, 4, None));
        let parent = AgentId::new();
        let view = WorkspaceViewId::new();
        let first = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::WriteIsolated,
                view,
                "openai",
                AgentPriority::NORMAL,
                None,
            ))
            .expect("first write");
        let err = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::WriteIsolated,
                view,
                "anthropic",
                AgentPriority::HIGH,
                None,
            ))
            .expect_err("shared write view");
        assert_eq!(err, SchedulerError::SharedWriteView { view_id: view });
        assert_eq!(err.code(), Some(ErrorCode::AgentConcurrencyLimit));

        let other_view = WorkspaceViewId::new();
        let second = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::WriteIsolated,
                other_view,
                "openai",
                AgentPriority::NORMAL,
                None,
            ))
            .expect("isolated write");
        scheduler.dispatch().expect("dispatch first");
        scheduler.dispatch().expect("dispatch second");
        assert_eq!(
            scheduler.get(first.id()).expect("a").state(),
            ScheduleState::Running
        );
        assert_eq!(
            scheduler.get(second.id()).expect("b").state(),
            ScheduleState::Running
        );
        assert_ne!(
            scheduler.get(first.id()).expect("a").workspace_view(),
            scheduler.get(second.id()).expect("b").workspace_view()
        );

        let reader = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                view,
                "openai",
                AgentPriority::NORMAL,
                None,
            ))
            .expect("reader may share");
        assert_eq!(
            scheduler.get(reader.id()).expect("r").workspace_view(),
            view
        );
    }

    #[test]
    fn per_provider_limit_enforced_under_concurrent_dispatch() {
        let scheduler = Scheduler::new(limits(8, 8, 1, None));
        let parent = AgentId::new();
        let openai = provider("openai");
        for _ in 0..16 {
            scheduler
                .enqueue(spawn(
                    parent,
                    WorkspaceAccess::ReadOnly,
                    WorkspaceViewId::new(),
                    "openai",
                    AgentPriority::NORMAL,
                    None,
                ))
                .expect("enqueue");
        }

        let observed = AtomicUsize::new(0);
        thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    while let Ok(Some(_)) = scheduler.dispatch() {
                        let n = scheduler.occupancy_for_provider(&openai);
                        let mut current = observed.load(Ordering::SeqCst);
                        while n > current {
                            match observed.compare_exchange(
                                current,
                                n,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            ) {
                                Ok(_) => break,
                                Err(actual) => current = actual,
                            }
                        }
                    }
                });
            }
        });

        assert!(observed.load(Ordering::SeqCst) <= 1);
        assert_eq!(scheduler.occupancy_for_provider(&openai), 1);
        assert_eq!(scheduler.occupancy().global(), 1);
        assert!(scheduler.dispatch().expect("saturated").is_none());
    }

    #[test]
    fn global_and_write_concurrency_limits() {
        let scheduler = Scheduler::new(limits(4, 1, 4, None));
        let parent = AgentId::new();
        let w1 = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::WriteIsolated,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::HIGH,
                None,
            ))
            .expect("w1");
        let w2 = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::WriteIsolated,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::NORMAL,
                None,
            ))
            .expect("w2");
        let r1 = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "anthropic",
                AgentPriority::LOW,
                None,
            ))
            .expect("r1");

        assert_eq!(
            scheduler.dispatch().expect("w1").map(|h| h.id()),
            Some(w1.id())
        );
        assert!(scheduler.runnable().contains(&r1.id()));
        assert!(!scheduler.runnable().contains(&w2.id()));
        assert_eq!(
            scheduler.dispatch().expect("r1").map(|h| h.id()),
            Some(r1.id())
        );
        assert!(scheduler.runnable().is_empty());
        assert_eq!(
            scheduler.start(w2.id()).expect_err("write limit"),
            SchedulerError::ConcurrencyLimit {
                kind: LimitKind::Write
            }
        );

        scheduler
            .finish(w1.id(), AgentTerminalStatus::Succeeded, 0)
            .expect("finish w1");
        assert_eq!(
            scheduler.dispatch().expect("w2").map(|h| h.id()),
            Some(w2.id())
        );
    }

    #[test]
    fn global_cost_budget_is_enforced() {
        let scheduler = Scheduler::new(limits(4, 2, 4, Some(100)));
        let parent = AgentId::new();
        scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::NORMAL,
                Some(60),
            ))
            .expect("60");
        let err = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::NORMAL,
                Some(50),
            ))
            .expect_err("over budget");
        assert_eq!(err, SchedulerError::CostBudgetExceeded);
        let unbounded = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::NORMAL,
                None,
            ))
            .expect_err("unbounded under budget");
        assert_eq!(unbounded, SchedulerError::CostBudgetExceeded);
        assert_eq!(scheduler.reserved_cost(), 60);
    }

    #[test]
    fn parent_cancel_cascades_except_detached() {
        let scheduler = Scheduler::new(limits(4, 2, 4, None));
        let root = AgentId::new();
        let parent = scheduler
            .enqueue(spawn(
                root,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::NORMAL,
                None,
            ))
            .expect("parent");
        let child = scheduler
            .enqueue(spawn(
                parent.id(),
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::NORMAL,
                None,
            ))
            .expect("child");
        let daemon = scheduler
            .enqueue(
                spawn(
                    parent.id(),
                    WorkspaceAccess::ReadOnly,
                    WorkspaceViewId::new(),
                    "openai",
                    AgentPriority::NORMAL,
                    None,
                )
                .detached(),
            )
            .expect("daemon");

        scheduler.cancel(parent.id()).expect("cancel parent");
        assert_eq!(
            scheduler.get(parent.id()).expect("p").state(),
            ScheduleState::Cancelled
        );
        assert_eq!(
            scheduler.get(child.id()).expect("c").state(),
            ScheduleState::Cancelled
        );
        assert!(child.cancel_token().is_cancelled());
        assert_eq!(
            scheduler.get(daemon.id()).expect("d").state(),
            ScheduleState::Queued
        );
        assert!(!daemon.cancel_token().is_cancelled());

        let rejected = scheduler
            .enqueue(spawn(
                parent.id(),
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::NORMAL,
                None,
            ))
            .expect_err("inherit cancelled parent");
        assert_eq!(
            rejected,
            SchedulerError::ParentCancelled {
                parent: parent.id()
            }
        );
        scheduler
            .enqueue(
                spawn(
                    parent.id(),
                    WorkspaceAccess::ReadOnly,
                    WorkspaceViewId::new(),
                    "openai",
                    AgentPriority::NORMAL,
                    None,
                )
                .detached(),
            )
            .expect("detached after parent cancel");
    }

    #[test]
    fn pause_resume_and_priority_update() {
        let scheduler = Scheduler::new(limits(1, 1, 1, None));
        let parent = AgentId::new();
        let first = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::LOW,
                None,
            ))
            .expect("first");
        let second = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::NORMAL,
                None,
            ))
            .expect("second");
        scheduler.pause(second.id()).expect("pause second");
        assert_eq!(scheduler.runnable(), vec![first.id()]);
        scheduler
            .set_priority(first.id(), AgentPriority::MIN)
            .expect("lower first");
        scheduler.resume(second.id()).expect("resume");
        assert_eq!(scheduler.runnable(), vec![second.id(), first.id()]);
    }

    #[test]
    fn queue_bound_and_invalid_inputs() {
        let scheduler = Scheduler::new(SchedulerLimits::new(1, 1, 1, None, 1).expect("tiny queue"));
        let parent = AgentId::new();
        scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::NORMAL,
                None,
            ))
            .expect("one");
        let err = scheduler
            .enqueue(spawn(
                parent,
                WorkspaceAccess::ReadOnly,
                WorkspaceViewId::new(),
                "openai",
                AgentPriority::NORMAL,
                None,
            ))
            .expect_err("full");
        assert_eq!(err, SchedulerError::QueueFull { limit: 1 });
        assert!(SchedulerLimits::new(1, 1, 1, None, 0).is_err());
        assert!(ProviderKey::parse("").is_err());
        assert!(ProviderKey::parse("OpenAI").is_err());
        assert!(ResultSchema::new("").is_err());
        assert!(
            SpawnAgent::builder(
                parent,
                AgentRole::Coder,
                "",
                WorkspaceAccess::ReadOnly,
                schema(),
                WorkspaceViewId::new(),
                provider("openai"),
            )
            .build()
            .is_err()
        );
    }

    #[test]
    fn from_agents_config_uses_documented_defaults() {
        let config = AgentsConfig::default();
        let scheduler = Scheduler::from_agents_config(&config).expect("config");
        assert_eq!(scheduler.limits().max_parallel(), 4);
        assert_eq!(scheduler.limits().max_write_parallel(), 2);
        assert_eq!(scheduler.limits().max_per_provider(), 4);
    }
}
