//! Isolated subagent spawn: child view, role prompt, scheduler start.
//!
//! `WriteIsolated` always allocates a fresh Git-worktree view and a child
//! principal. Parent capability leases are not a spawn input and cannot be
//! handed to the child. `agent.spawned` is emitted before the child turn runs.

use std::error::Error;
use std::fmt;

use protocol::{AgentId, ArtifactId, ErrorCode, RepoId, SessionId, TurnId, WorkspaceViewId};
use workspace::CancellationToken as WorkspaceCancel;
use workspace::{
    CreateView, GitWorktreeError, GitWorktreeStore, ViewAccess, ViewRegistry, WorkspaceBackend,
    WorkspaceError, WorkspaceView,
};

use super::model::{
    Agent, AgentBudget, AgentModelError, AgentResult, AgentRole, AgentSpec, AgentState,
    AgentTerminalStatus, CancellationToken, MAX_PERMISSIONS_PROFILE_BYTES, MAX_TASK_BYTES,
    ModelPolicyRef,
};
use super::scheduler::{
    AgentHandle, AgentPriority, ProviderKey, ResultSchema, Scheduler, SchedulerError, SpawnAgent,
    WorkspaceAccess,
};
use crate::agent_executor::{
    AgentExecutionError, AgentExecutionRequest, AgentExecutor, TurnAgentExecutor,
};
use crate::prompt::{
    PromptBundle, PromptCompiler, PromptContextBlock, PromptError, PromptInputs, PromptLimits,
};
use crate::turn::{ModelDriver, ToolDriver, TurnError, TurnEventSink};

/// Maximum lifecycle events one spawn may emit through [`SpawnEventSink`].
pub const MAX_SPAWN_EVENTS: usize = 32;

/// Maximum UTF-8 bytes accepted for an explicitly-selected child context excerpt.
pub const MAX_TASK_CONTEXT_BYTES: usize = 16 * 1024;

/// Default child profile when the request does not name one.
const PROFILE_READ_ONLY: &str = "child.read_only";
const PROFILE_WRITE_ISOLATED: &str = "child.write_isolated";

/// Child identity used for later lease issuance. Never a parent principal copy.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ChildPrincipal {
    agent_id: AgentId,
    name: String,
}

/// Typed spawn request. There is no capability-lease field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnRequest {
    id: Option<AgentId>,
    parent: AgentId,
    parent_access: WorkspaceAccess,
    role: AgentRole,
    task: String,
    access: WorkspaceAccess,
    budget: AgentBudget,
    model_policy: ModelPolicyRef,
    expected_result: ResultSchema,
    provider: ProviderKey,
    repo_id: RepoId,
    base_revision: String,
    session_id: SessionId,
    tool_catalog_hash: ArtifactId,
    parent_view: Option<WorkspaceViewId>,
    permissions_profile: String,
    priority: AgentPriority,
    detached: bool,
    task_context: Option<String>,
}

/// Construction handle for [`SpawnRequest`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnRequestBuilder {
    id: Option<AgentId>,
    parent: AgentId,
    parent_access: WorkspaceAccess,
    role: AgentRole,
    task: String,
    access: WorkspaceAccess,
    budget: AgentBudget,
    model_policy: ModelPolicyRef,
    expected_result: ResultSchema,
    provider: ProviderKey,
    repo_id: RepoId,
    base_revision: String,
    session_id: SessionId,
    tool_catalog_hash: ArtifactId,
    parent_view: Option<WorkspaceViewId>,
    permissions_profile: Option<String>,
    priority: AgentPriority,
    detached: bool,
    task_context: Option<String>,
}

/// Collaborators for one [`spawn_agent`] call.
pub struct SpawnEnv<'a, M, T, W, L, S> {
    scheduler: &'a Scheduler,
    views: &'a ViewRegistry,
    worktrees: &'a W,
    model: &'a mut M,
    tools: &'a mut T,
    lifecycle: &'a mut L,
    turn_events: &'a mut S,
}

/// Outcome after the child view exists, the role prompt is compiled, and the
/// first child turn has been started under the scheduler.
#[derive(Clone, Debug)]
pub struct SpawnedAgent {
    agent: Agent,
    handle: AgentHandle,
    view: WorkspaceView,
    principal: ChildPrincipal,
    prompt: PromptBundle,
    result: AgentResult,
}

/// Durable agent lifecycle event emitted by spawn. Wire names match `EventKind`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnEvent {
    Spawned {
        agent_id: AgentId,
        parent: AgentId,
        workspace_view: WorkspaceViewId,
        access: WorkspaceAccess,
    },
    Started {
        agent_id: AgentId,
        turn_id: TurnId,
    },
}

/// Kernel `EventKind` wire form for [`SpawnEvent`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum SpawnEventKind {
    AgentSpawned,
    AgentStarted,
}

/// Bounded sink for durable spawn lifecycle events.
pub trait SpawnEventSink {
    fn emit(&mut self, event: SpawnEvent) -> Result<(), SpawnError>;
}

/// Materialize a Git-worktree checkout for a write-isolated child view.
pub trait IsolatedWorktree {
    fn materialize(
        &self,
        view: &WorkspaceView,
        cancel: &CancellationToken,
    ) -> Result<(), SpawnError>;
}

/// Marker for an attempted parent→child lease transfer. Always denied.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ParentLeaseTransfer;

/// Typed spawn failure. Display never echoes task, revision, or lease bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnError {
    Cancelled,
    Timeout,
    InvalidTask,
    InvalidRole,
    InvalidPermissionsProfile,
    InvalidBaseRevision,
    InvalidView,
    LeaseNotTransferable,
    CapabilityEscalation,
    SharedWriteView { view_id: WorkspaceViewId },
    ParentCancelled { parent: AgentId },
    BoundExceeded,
    EventSink,
    Prompt(PromptError),
    Scheduler(SchedulerError),
    Agent(AgentModelError),
    Turn(TurnError),
    Workspace,
}

impl ChildPrincipal {
    fn for_agent(agent_id: AgentId) -> Self {
        let mut name = String::from("agent/");
        name.push_str(&agent_id.to_string());
        Self { agent_id, name }
    }

    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    pub fn as_str(&self) -> &str {
        &self.name
    }
}

impl SpawnRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn builder(
        parent: AgentId,
        role: AgentRole,
        task: impl Into<String>,
        access: WorkspaceAccess,
        expected_result: ResultSchema,
        provider: ProviderKey,
        repo_id: RepoId,
        base_revision: impl Into<String>,
        session_id: SessionId,
        tool_catalog_hash: ArtifactId,
    ) -> SpawnRequestBuilder {
        SpawnRequestBuilder {
            id: None,
            parent,
            parent_access: WorkspaceAccess::ReadOnly,
            role,
            task: task.into(),
            access,
            budget: AgentBudget::unlimited(),
            model_policy: ModelPolicyRef::default(),
            expected_result,
            provider,
            repo_id,
            base_revision: base_revision.into(),
            session_id,
            tool_catalog_hash,
            parent_view: None,
            permissions_profile: None,
            priority: AgentPriority::NORMAL,
            detached: false,
            task_context: None,
        }
    }

    /// Denied: capability leases are not transferable to children.
    pub fn with_parent_lease<L>(self, lease: L) -> Result<Self, SpawnError> {
        let _ = std::mem::size_of_val(&lease);
        Err(SpawnError::LeaseNotTransferable)
    }

    pub fn parent(&self) -> AgentId {
        self.parent
    }

    pub fn parent_access(&self) -> WorkspaceAccess {
        self.parent_access
    }

    pub fn role(&self) -> AgentRole {
        self.role
    }

    pub fn task(&self) -> &str {
        &self.task
    }

    pub fn task_context(&self) -> Option<&str> {
        self.task_context.as_deref()
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

    pub fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn base_revision(&self) -> &str {
        &self.base_revision
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn tool_catalog_hash(&self) -> ArtifactId {
        self.tool_catalog_hash
    }

    pub fn parent_view(&self) -> Option<WorkspaceViewId> {
        self.parent_view
    }

    pub fn permissions_profile(&self) -> &str {
        &self.permissions_profile
    }

    pub fn priority(&self) -> AgentPriority {
        self.priority
    }

    pub fn is_detached(&self) -> bool {
        self.detached
    }

    fn validate(&self) -> Result<(), SpawnError> {
        if self.role == AgentRole::Main {
            return Err(SpawnError::InvalidRole);
        }
        if self.task.is_empty() || self.task.len() > MAX_TASK_BYTES {
            return Err(SpawnError::InvalidTask);
        }
        if let Some(context) = &self.task_context
            && (context.is_empty() || context.len() > MAX_TASK_CONTEXT_BYTES)
        {
            return Err(SpawnError::BoundExceeded);
        }
        if self.permissions_profile.is_empty()
            || self.permissions_profile.len() > MAX_PERMISSIONS_PROFILE_BYTES
        {
            return Err(SpawnError::InvalidPermissionsProfile);
        }
        validate_base_revision(&self.base_revision)?;
        if self.access.is_write() && !self.parent_access.is_write() {
            return Err(SpawnError::CapabilityEscalation);
        }
        if let Some(id) = self.id
            && id == self.parent
        {
            return Err(SpawnError::InvalidRole);
        }
        Ok(())
    }
}

impl SpawnRequestBuilder {
    /// Explicitly selected context excerpt passed to the child. Never the full
    /// parent transcript; the caller must supply only what the child needs.
    pub fn with_task_context(mut self, text: impl Into<String>) -> Self {
        self.task_context = Some(text.into());
        self
    }

    pub fn with_id(mut self, id: AgentId) -> Self {
        self.id = Some(id);
        self
    }

    pub fn parent_access(mut self, access: WorkspaceAccess) -> Self {
        self.parent_access = access;
        self
    }

    pub fn budget(mut self, budget: AgentBudget) -> Self {
        self.budget = budget;
        self
    }

    pub fn model_policy(mut self, model_policy: ModelPolicyRef) -> Self {
        self.model_policy = model_policy;
        self
    }

    pub fn parent_view(mut self, view: WorkspaceViewId) -> Self {
        self.parent_view = Some(view);
        self
    }

    pub fn permissions_profile(mut self, profile: impl Into<String>) -> Self {
        self.permissions_profile = Some(profile.into());
        self
    }

    pub fn with_priority(mut self, priority: AgentPriority) -> Self {
        self.priority = priority;
        self
    }

    pub fn detached(mut self) -> Self {
        self.detached = true;
        self
    }

    /// Denied: capability leases are not transferable to children.
    pub fn with_parent_lease<L>(self, lease: L) -> Result<SpawnRequest, SpawnError> {
        let _ = std::mem::size_of_val(&lease);
        Err(SpawnError::LeaseNotTransferable)
    }

    pub fn build(self) -> Result<SpawnRequest, SpawnError> {
        let profile = match self.permissions_profile {
            Some(profile) => profile,
            None => default_profile(self.access).to_owned(),
        };
        let request = SpawnRequest {
            id: self.id,
            parent: self.parent,
            parent_access: self.parent_access,
            role: self.role,
            task: self.task,
            access: self.access,
            budget: self.budget,
            model_policy: self.model_policy,
            expected_result: self.expected_result,
            provider: self.provider,
            repo_id: self.repo_id,
            base_revision: self.base_revision,
            session_id: self.session_id,
            tool_catalog_hash: self.tool_catalog_hash,
            parent_view: self.parent_view,
            permissions_profile: profile,
            priority: self.priority,
            detached: self.detached,
            task_context: self.task_context,
        };
        request.validate()?;
        Ok(request)
    }
}

impl<'a, M, T, W, L, S> SpawnEnv<'a, M, T, W, L, S> {
    pub fn new(
        scheduler: &'a Scheduler,
        views: &'a ViewRegistry,
        worktrees: &'a W,
        model: &'a mut M,
        tools: &'a mut T,
        lifecycle: &'a mut L,
        turn_events: &'a mut S,
    ) -> Self {
        Self {
            scheduler,
            views,
            worktrees,
            model,
            tools,
            lifecycle,
            turn_events,
        }
    }
}

impl SpawnedAgent {
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    pub fn handle(&self) -> &AgentHandle {
        &self.handle
    }

    pub fn id(&self) -> AgentId {
        self.handle.id()
    }

    pub fn view(&self) -> &WorkspaceView {
        &self.view
    }

    pub fn principal(&self) -> &ChildPrincipal {
        &self.principal
    }

    pub fn prompt(&self) -> &PromptBundle {
        &self.prompt
    }

    /// Canonical `AgentResult` assembled by the host-owned `AgentExecutor`.
    pub fn result(&self) -> &AgentResult {
        &self.result
    }
}

impl SpawnEvent {
    pub const fn kind(&self) -> SpawnEventKind {
        match self {
            Self::Spawned { .. } => SpawnEventKind::AgentSpawned,
            Self::Started { .. } => SpawnEventKind::AgentStarted,
        }
    }

    pub const fn agent_id(&self) -> AgentId {
        match self {
            Self::Spawned { agent_id, .. } | Self::Started { agent_id, .. } => *agent_id,
        }
    }
}

impl SpawnEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentSpawned => "agent.spawned",
            Self::AgentStarted => "agent.started",
        }
    }
}

impl SpawnEventSink for Vec<SpawnEvent> {
    fn emit(&mut self, event: SpawnEvent) -> Result<(), SpawnError> {
        if self.len() >= MAX_SPAWN_EVENTS {
            return Err(SpawnError::BoundExceeded);
        }
        self.push(event);
        Ok(())
    }
}

impl IsolatedWorktree for () {
    fn materialize(
        &self,
        view: &WorkspaceView,
        cancel: &CancellationToken,
    ) -> Result<(), SpawnError> {
        check_cancel(cancel)?;
        if view.backend() != WorkspaceBackend::GitWorktree || !view.access().is_writable() {
            return Err(SpawnError::InvalidView);
        }
        Ok(())
    }
}

impl IsolatedWorktree for GitWorktreeStore {
    fn materialize(
        &self,
        view: &WorkspaceView,
        cancel: &CancellationToken,
    ) -> Result<(), SpawnError> {
        check_cancel(cancel)?;
        let ws_cancel = workspace_cancel(cancel);
        self.create_view(view, &ws_cancel)
            .map(|_| ())
            .map_err(SpawnError::from)
    }
}

impl<T: IsolatedWorktree + ?Sized> IsolatedWorktree for &T {
    fn materialize(
        &self,
        view: &WorkspaceView,
        cancel: &CancellationToken,
    ) -> Result<(), SpawnError> {
        (**self).materialize(view, cancel)
    }
}

/// Create an isolated child view, compile the role prompt, and start a turn.
///
/// `WriteIsolated` maps to a fresh Git-worktree view and child principal.
/// A parent lease cannot be supplied; [`SpawnRequest::with_parent_lease`]
/// always fails. `agent.spawned` is appended before `run_turn`.
pub fn spawn_agent<M, T, W, L, S>(
    request: SpawnRequest,
    env: &mut SpawnEnv<'_, M, T, W, L, S>,
    cancel: &CancellationToken,
) -> Result<SpawnedAgent, SpawnError>
where
    M: ModelDriver,
    T: ToolDriver,
    W: IsolatedWorktree,
    L: SpawnEventSink,
    S: TurnEventSink,
{
    check_cancel(cancel)?;
    request.validate()?;
    reject_scheduled_parent_escalation(env.scheduler, &request)?;

    let child_id = request.id.unwrap_or_else(AgentId::new);
    if child_id == request.parent {
        return Err(SpawnError::InvalidRole);
    }
    let principal = ChildPrincipal::for_agent(child_id);
    let view = allocate_child_view(&request, child_id, env.views, env.worktrees, cancel)?;
    if request.access.is_write() && request.parent_view == Some(view.id()) {
        return Err(SpawnError::SharedWriteView { view_id: view.id() });
    }

    check_cancel(cancel)?;
    let prompt = compile_role_prompt(&request, child_id, &view, &principal, cancel)?;

    let spec = AgentSpec::builder(child_id, request.role, request.task.clone(), view.id())
        .parent_id(Some(request.parent))
        .model_policy(request.model_policy.clone())
        .budget(request.budget)
        .permissions_profile(request.permissions_profile.clone())
        .build()
        .map_err(SpawnError::from)?;
    // Clone the spec for the executor so the canonical `AgentResult` can read
    // the task/role/budget/workspace identity; the original retains spawn-time
    // ownership of the `Agent`'s immutable spec.
    let mut agent = Agent::spawn(spec.clone(), cancel).map_err(SpawnError::from)?;

    let mut enqueue = SpawnAgent::builder(
        request.parent,
        request.role,
        request.task.clone(),
        request.access,
        request.expected_result.clone(),
        view.id(),
        request.provider.clone(),
    )
    .budget(request.budget)
    .model_policy(request.model_policy.clone())
    .with_id(child_id)
    .with_priority(request.priority);
    if request.detached {
        enqueue = enqueue.detached();
    }
    let queued = enqueue.build().map_err(SpawnError::from)?;
    let handle = env.scheduler.enqueue(queued).map_err(SpawnError::from)?;

    if let Err(err) = env.lifecycle.emit(SpawnEvent::Spawned {
        agent_id: child_id,
        parent: request.parent,
        workspace_view: view.id(),
        access: request.access,
    }) {
        let _ = env.scheduler.cancel(child_id);
        return Err(err);
    }

    check_cancel(cancel)?;
    let handle = env.scheduler.start(handle.id()).map_err(SpawnError::from)?;
    agent
        .transition(AgentState::Starting, cancel)
        .map_err(SpawnError::from)?;
    agent
        .transition(AgentState::Running, cancel)
        .map_err(SpawnError::from)?;

    let turn_id = TurnId::new();
    env.lifecycle.emit(SpawnEvent::Started {
        agent_id: child_id,
        turn_id,
    })?;

    // The authoritative subagent execution path runs through the canonical
    // host-owned `AgentExecutor`. The child turn's terminal output and host
    // provenance are assembled into a canonical `AgentResult`.
    let exec_request = {
        let mut req = AgentExecutionRequest::new(spec, request.session_id);
        if let Some(selected) = request.task_context() {
            req = req.with_selected_context(selected);
        }
        req
    };
    let result = match TurnAgentExecutor.execute(
        &exec_request,
        env.model,
        env.tools,
        env.turn_events,
        cancel,
    ) {
        Ok(result) => result,
        Err(AgentExecutionError::Cancelled) => {
            let _ = env.scheduler.cancel(child_id);
            let _ = agent.transition(AgentState::Cancelled, &CancellationToken::new());
            return Err(SpawnError::Cancelled);
        }
        Err(err) => {
            let _ = env
                .scheduler
                .finish(child_id, AgentTerminalStatus::Failed, 0);
            return Err(SpawnError::from(err));
        }
    };
    let status = result.status();
    let _ = env.scheduler.finish(child_id, status, 0);
    let _ = agent.transition(status.as_state(), &CancellationToken::new());

    Ok(SpawnedAgent {
        agent,
        handle,
        view,
        principal,
        prompt,
        result,
    })
}

fn allocate_child_view<W: IsolatedWorktree>(
    request: &SpawnRequest,
    child: AgentId,
    views: &ViewRegistry,
    worktrees: &W,
    cancel: &CancellationToken,
) -> Result<WorkspaceView, SpawnError> {
    check_cancel(cancel)?;
    let ws_cancel = workspace_cancel(cancel);
    let view = match request.access {
        WorkspaceAccess::ReadOnly => {
            let spec = CreateView::new(
                request.repo_id,
                WorkspaceBackend::Overlay,
                request.base_revision.clone(),
                ViewAccess::ReadOnly,
            );
            let view = views.create(spec, &ws_cancel).map_err(SpawnError::from)?;
            views
                .attach_reader(view.id(), child, &ws_cancel)
                .map_err(SpawnError::from)?;
            view
        }
        WorkspaceAccess::WriteIsolated => {
            let spec = CreateView::new(
                request.repo_id,
                WorkspaceBackend::GitWorktree,
                request.base_revision.clone(),
                ViewAccess::ReadWrite,
            )
            .with_write_owner(child);
            let view = views.create(spec, &ws_cancel).map_err(SpawnError::from)?;
            worktrees.materialize(&view, cancel)?;
            view
        }
    };
    if request.parent_view == Some(view.id()) {
        return Err(SpawnError::SharedWriteView { view_id: view.id() });
    }
    Ok(view)
}

fn compile_role_prompt(
    request: &SpawnRequest,
    child: AgentId,
    view: &WorkspaceView,
    principal: &ChildPrincipal,
    cancel: &CancellationToken,
) -> Result<PromptBundle, SpawnError> {
    let mut facts = String::from("child_id=");
    facts.push_str(&child.to_string());
    facts.push_str(" parent_id=");
    facts.push_str(&request.parent.to_string());
    facts.push_str(" access=");
    facts.push_str(request.access.as_str());
    facts.push_str(" view=");
    facts.push_str(&view.id().to_string());
    facts.push_str(" principal=");
    facts.push_str(principal.as_str());
    facts.push_str(" backend=");
    facts.push_str(view.backend().as_str());

    let limits = PromptLimits::new().cancellation(cancel.clone());
    let mut inputs = PromptInputs::new(request.role, request.tool_catalog_hash)
        .runtime_facts(facts)
        .user_message(request.task.clone())
        .limits(limits);
    // The child receives only the parent-explicitly-selected context excerpt.
    // It is delimited as untrusted data (never higher-priority instructions),
    // and the full parent transcript is never threaded into the child prompt.
    if let Some(selected) = request.task_context() {
        inputs = inputs.context(PromptContextBlock::new("parent-selected", selected));
    }
    PromptCompiler::compile(&inputs).map_err(SpawnError::from)
}

fn reject_scheduled_parent_escalation(
    scheduler: &Scheduler,
    request: &SpawnRequest,
) -> Result<(), SpawnError> {
    if !request.access.is_write() {
        return Ok(());
    }
    if let Some(parent) = scheduler.get(request.parent)
        && !parent.access().is_write()
    {
        return Err(SpawnError::CapabilityEscalation);
    }
    Ok(())
}

fn default_profile(access: WorkspaceAccess) -> &'static str {
    match access {
        WorkspaceAccess::ReadOnly => PROFILE_READ_ONLY,
        WorkspaceAccess::WriteIsolated => PROFILE_WRITE_ISOLATED,
    }
}

fn validate_base_revision(raw: &str) -> Result<(), SpawnError> {
    if raw.is_empty() || raw.len() > workspace::MAX_BASE_REVISION_BYTES {
        return Err(SpawnError::InvalidBaseRevision);
    }
    if raw.contains('\0')
        || raw.chars().any(char::is_control)
        || raw.chars().any(char::is_whitespace)
    {
        return Err(SpawnError::InvalidBaseRevision);
    }
    Ok(())
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), SpawnError> {
    if cancel.is_cancelled() {
        Err(SpawnError::Cancelled)
    } else {
        Ok(())
    }
}

fn workspace_cancel(cancel: &CancellationToken) -> WorkspaceCancel {
    let token = WorkspaceCancel::new();
    if cancel.is_cancelled() {
        token.cancel();
    }
    token
}

impl SpawnError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "spawn cancelled",
            Self::Timeout => "spawn timed out",
            Self::InvalidTask => "agent task is empty or exceeds the size bound",
            Self::InvalidRole => "child spawn role is not allowed",
            Self::InvalidPermissionsProfile => {
                "permissions profile is empty or exceeds the size bound"
            }
            Self::InvalidBaseRevision => "workspace view base revision is invalid",
            Self::InvalidView => "child workspace view is invalid",
            Self::LeaseNotTransferable => "parent capability lease cannot be passed to a child",
            Self::CapabilityEscalation => "child write access exceeds the parent workspace ceiling",
            Self::SharedWriteView { .. } => "write-capable agents cannot share a workspace view",
            Self::ParentCancelled { .. } => "parent agent is cancelled; child inherit is denied",
            Self::BoundExceeded => "spawn resource bound exceeded",
            Self::EventSink => "spawn event sink rejected an event",
            Self::Prompt(_) => "role prompt compile failed",
            Self::Scheduler(_) => "scheduler rejected child spawn",
            Self::Agent(_) => "agent model rejected child spawn",
            Self::Turn(_) => "child turn failed to start",
            Self::Workspace => "child workspace view allocation failed",
        }
    }

    pub const fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled | Self::ParentCancelled { .. } => None,
            Self::Timeout | Self::BoundExceeded => Some(ErrorCode::ProcessTimeout),
            Self::InvalidTask
            | Self::InvalidRole
            | Self::InvalidPermissionsProfile
            | Self::InvalidBaseRevision
            | Self::InvalidView => Some(ErrorCode::ConfigInvalid),
            Self::LeaseNotTransferable => Some(ErrorCode::PolicyLeaseInvalid),
            Self::CapabilityEscalation => Some(ErrorCode::PolicyDenied),
            Self::SharedWriteView { .. } => Some(ErrorCode::AgentConcurrencyLimit),
            Self::EventSink | Self::Workspace | Self::Agent(_) => {
                Some(ErrorCode::InternalUnexpected)
            }
            Self::Prompt(err) => err.code(),
            Self::Scheduler(err) => err.code(),
            Self::Turn(err) => err.code(),
        }
    }
}

impl fmt::Display for ChildPrincipal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for SpawnEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for SpawnError {}

impl From<PromptError> for SpawnError {
    fn from(err: PromptError) -> Self {
        match err {
            PromptError::Cancelled => Self::Cancelled,
            PromptError::Timeout => Self::Timeout,
            other => Self::Prompt(other),
        }
    }
}

impl From<SchedulerError> for SpawnError {
    fn from(err: SchedulerError) -> Self {
        match err {
            SchedulerError::Cancelled => Self::Cancelled,
            SchedulerError::InvalidTask => Self::InvalidTask,
            SchedulerError::ParentCancelled { parent } => Self::ParentCancelled { parent },
            SchedulerError::SharedWriteView { view_id } => Self::SharedWriteView { view_id },
            other => Self::Scheduler(other),
        }
    }
}

impl From<AgentModelError> for SpawnError {
    fn from(err: AgentModelError) -> Self {
        match err {
            AgentModelError::Cancelled => Self::Cancelled,
            AgentModelError::InvalidTask => Self::InvalidTask,
            AgentModelError::InvalidPermissionsProfile => Self::InvalidPermissionsProfile,
            other => Self::Agent(other),
        }
    }
}

impl From<TurnError> for SpawnError {
    fn from(err: TurnError) -> Self {
        match err {
            TurnError::Cancelled => Self::Cancelled,
            TurnError::BoundExceeded => Self::BoundExceeded,
            TurnError::EventSink => Self::EventSink,
            other => Self::Turn(other),
        }
    }
}

impl From<AgentExecutionError> for SpawnError {
    fn from(err: AgentExecutionError) -> Self {
        match err {
            AgentExecutionError::Cancelled => Self::Cancelled,
            AgentExecutionError::Turn(err) => Self::from(err),
            AgentExecutionError::AgentResult => Self::Agent(AgentModelError::InvalidSummary),
            AgentExecutionError::InvalidRequest => Self::InvalidTask,
            AgentExecutionError::ContextOverflowAfterEffects
            | AgentExecutionError::ContextRetryExceeded
            | AgentExecutionError::ContextRecovery(_) => Self::Turn(TurnError::BoundExceeded),
        }
    }
}

impl From<WorkspaceError> for SpawnError {
    fn from(err: WorkspaceError) -> Self {
        match err {
            WorkspaceError::Cancelled => Self::Cancelled,
            WorkspaceError::InvalidBaseRevision => Self::InvalidBaseRevision,
            WorkspaceError::ReadOnlyView { view_id }
            | WorkspaceError::WriteOwnerConflict { view_id, .. } => {
                Self::SharedWriteView { view_id }
            }
            WorkspaceError::TooManyViews { .. } => Self::BoundExceeded,
            WorkspaceError::InvalidAccess | WorkspaceError::InvalidState { .. } => {
                Self::InvalidView
            }
            _ => Self::Workspace,
        }
    }
}

impl From<GitWorktreeError> for SpawnError {
    fn from(err: GitWorktreeError) -> Self {
        match err {
            GitWorktreeError::Cancelled => Self::Cancelled,
            GitWorktreeError::Timeout => Self::Timeout,
            GitWorktreeError::ReadOnlyView
            | GitWorktreeError::WrongBackend
            | GitWorktreeError::InvalidState
            | GitWorktreeError::InvalidRevision => Self::InvalidView,
            GitWorktreeError::WorktreeLimit | GitWorktreeError::BoundExceeded => {
                Self::BoundExceeded
            }
            _ => Self::Workspace,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::scheduler::ScheduleState;
    use crate::prompt::PromptSection;
    use crate::turn::{ModelStepError, ModelStepInput, ModelStepOutput, ToolStepError};
    use std::cell::Cell;
    use std::str::FromStr;

    const CATALOG: &str = "sha256:c2127ba39ec170e735fce76c2bebbb430086f98e7dc0c36900dd78dd17b9b7a8";

    struct ImmediateModel {
        steps: Cell<u32>,
    }

    struct CountingModel {
        steps: Cell<u32>,
    }

    struct NoTools;

    struct RejectingSink;

    struct RecordingWorktree {
        calls: Cell<u32>,
        last: Cell<Option<WorkspaceViewId>>,
    }

    impl ImmediateModel {
        fn new() -> Self {
            Self {
                steps: Cell::new(0),
            }
        }
    }

    impl CountingModel {
        fn new() -> Self {
            Self {
                steps: Cell::new(0),
            }
        }
    }

    impl ModelDriver for ImmediateModel {
        fn step(
            &mut self,
            _input: &ModelStepInput<'_>,
            cancel: &CancellationToken,
        ) -> Result<ModelStepOutput, ModelStepError> {
            if cancel.is_cancelled() {
                return Err(ModelStepError::Cancelled);
            }
            self.steps.set(self.steps.get().saturating_add(1));
            Ok(ModelStepOutput::Terminal {
                text: "ok".to_owned(),
                tokens: 1,
            })
        }
    }

    impl ModelDriver for CountingModel {
        fn step(
            &mut self,
            _input: &ModelStepInput<'_>,
            cancel: &CancellationToken,
        ) -> Result<ModelStepOutput, ModelStepError> {
            if cancel.is_cancelled() {
                return Err(ModelStepError::Cancelled);
            }
            self.steps.set(self.steps.get().saturating_add(1));
            Ok(ModelStepOutput::Terminal {
                text: "ok".to_owned(),
                tokens: 1,
            })
        }
    }

    impl ToolDriver for NoTools {
        fn validate(
            &mut self,
            _call: &crate::turn::ProposedToolCall,
            cancel: &CancellationToken,
        ) -> Result<crate::turn::ValidatedToolCall, ToolStepError> {
            if cancel.is_cancelled() {
                return Err(ToolStepError::Cancelled);
            }
            Err(ToolStepError::Invalid)
        }

        fn execute(
            &mut self,
            _call: &crate::turn::ValidatedToolCall,
            cancel: &CancellationToken,
        ) -> Result<crate::turn::ToolStepResult, ToolStepError> {
            if cancel.is_cancelled() {
                return Err(ToolStepError::Cancelled);
            }
            Err(ToolStepError::Failed)
        }
    }

    impl SpawnEventSink for RejectingSink {
        fn emit(&mut self, _event: SpawnEvent) -> Result<(), SpawnError> {
            Err(SpawnError::EventSink)
        }
    }

    impl IsolatedWorktree for RecordingWorktree {
        fn materialize(
            &self,
            view: &WorkspaceView,
            cancel: &CancellationToken,
        ) -> Result<(), SpawnError> {
            check_cancel(cancel)?;
            if view.backend() != WorkspaceBackend::GitWorktree || !view.access().is_writable() {
                return Err(SpawnError::InvalidView);
            }
            self.calls.set(self.calls.get().saturating_add(1));
            self.last.set(Some(view.id()));
            Ok(())
        }
    }

    fn catalog() -> ArtifactId {
        ArtifactId::from_str(CATALOG).expect("catalog")
    }

    fn provider() -> ProviderKey {
        ProviderKey::parse("openai").expect("provider")
    }

    fn schema() -> ResultSchema {
        ResultSchema::new("rapidlm.agent_result").expect("schema")
    }

    fn policy() -> ModelPolicyRef {
        ModelPolicyRef::new("balanced").expect("policy")
    }

    fn builder(parent: AgentId, access: WorkspaceAccess) -> SpawnRequestBuilder {
        SpawnRequest::builder(
            parent,
            AgentRole::Explorer,
            "inspect crate",
            access,
            schema(),
            provider(),
            RepoId::new(),
            "base-rev",
            SessionId::new(),
            catalog(),
        )
        .model_policy(policy())
    }

    fn request(access: WorkspaceAccess) -> SpawnRequest {
        let parent_access = if access.is_write() {
            WorkspaceAccess::WriteIsolated
        } else {
            WorkspaceAccess::ReadOnly
        };
        builder(AgentId::new(), access)
            .parent_access(parent_access)
            .build()
            .expect("request")
    }

    fn scheduler() -> Scheduler {
        Scheduler::default()
    }

    #[test]
    fn write_isolated_maps_to_fresh_git_worktree_view_and_principal() {
        let views = ViewRegistry::new();
        let worktrees = RecordingWorktree {
            calls: Cell::new(0),
            last: Cell::new(None),
        };
        let parent_view = WorkspaceViewId::new();
        let parent = AgentId::new();
        let req = builder(parent, WorkspaceAccess::WriteIsolated)
            .parent_access(WorkspaceAccess::WriteIsolated)
            .parent_view(parent_view)
            .build()
            .expect("request");
        let scheduler = scheduler();
        let mut model = ImmediateModel::new();
        let mut tools = NoTools;
        let mut lifecycle = Vec::new();
        let mut turn_events = Vec::new();
        let mut env = SpawnEnv::new(
            &scheduler,
            &views,
            &worktrees,
            &mut model,
            &mut tools,
            &mut lifecycle,
            &mut turn_events,
        );
        let spawned = spawn_agent(req, &mut env, &CancellationToken::new()).expect("spawn");

        assert_ne!(spawned.view().id(), parent_view);
        assert_eq!(spawned.view().backend(), WorkspaceBackend::GitWorktree);
        assert_eq!(spawned.view().access(), ViewAccess::ReadWrite);
        assert_eq!(spawned.view().write_owner(), Some(spawned.id()));
        assert_ne!(spawned.id(), parent);
        assert_eq!(
            spawned.principal().as_str(),
            format!("agent/{}", spawned.id())
        );
        assert_ne!(spawned.principal().as_str(), format!("agent/{parent}"));
        assert_eq!(worktrees.calls.get(), 1);
        assert_eq!(worktrees.last.get(), Some(spawned.view().id()));
        assert_eq!(
            spawned.agent().spec().permissions_profile(),
            PROFILE_WRITE_ISOLATED
        );
    }

    #[test]
    fn read_only_creates_overlay_view_without_worktree() {
        let views = ViewRegistry::new();
        let worktrees = RecordingWorktree {
            calls: Cell::new(0),
            last: Cell::new(None),
        };
        let scheduler = scheduler();
        let mut model = ImmediateModel::new();
        let mut tools = NoTools;
        let mut lifecycle = Vec::new();
        let mut turn_events = Vec::new();
        let req = request(WorkspaceAccess::ReadOnly);
        let mut env = SpawnEnv::new(
            &scheduler,
            &views,
            &worktrees,
            &mut model,
            &mut tools,
            &mut lifecycle,
            &mut turn_events,
        );
        let spawned = spawn_agent(req, &mut env, &CancellationToken::new()).expect("spawn");
        assert_eq!(spawned.view().backend(), WorkspaceBackend::Overlay);
        assert_eq!(spawned.view().access(), ViewAccess::ReadOnly);
        assert_eq!(spawned.view().write_owner(), None);
        assert_eq!(worktrees.calls.get(), 0);
        assert_eq!(
            spawned.agent().spec().permissions_profile(),
            PROFILE_READ_ONLY
        );
    }

    #[test]
    fn compile_role_prompt_and_start_turn_under_scheduler() {
        let views = ViewRegistry::new();
        let scheduler = scheduler();
        let mut model = ImmediateModel::new();
        let mut tools = NoTools;
        let mut lifecycle = Vec::new();
        let mut turn_events = Vec::new();
        let req = request(WorkspaceAccess::ReadOnly);
        let mut env = SpawnEnv::new(
            &scheduler,
            &views,
            &(),
            &mut model,
            &mut tools,
            &mut lifecycle,
            &mut turn_events,
        );
        let spawned = spawn_agent(req, &mut env, &CancellationToken::new()).expect("spawn");
        let sections: Vec<_> = spawned
            .prompt()
            .messages()
            .iter()
            .map(|message| message.section())
            .collect();
        assert!(sections.contains(&PromptSection::Role));
        assert!(sections.contains(&PromptSection::User));
        let role = spawned
            .prompt()
            .messages()
            .iter()
            .find(|message| message.section() == PromptSection::Role)
            .expect("role");
        assert!(role.content().contains("explorer"));
        // The authoritative subagent path produces a canonical AgentResult whose
        // terminal output matches the real final assistant-visible result.
        assert_eq!(spawned.result().status(), AgentTerminalStatus::Succeeded);
        assert_eq!(spawned.result().summary(), "ok");
        assert_eq!(model.steps.get(), 1);
        let scheduled = scheduler.get(spawned.id()).expect("scheduled");
        assert!(scheduled.state().is_terminal());
        assert_eq!(scheduled.state(), ScheduleState::Succeeded);
    }

    #[test]
    fn child_prompt_carries_only_explicitly_selected_state() {
        let views = ViewRegistry::new();
        let scheduler = scheduler();
        let mut model = ImmediateModel::new();
        let mut tools = NoTools;
        let mut lifecycle = Vec::new();
        let mut turn_events = Vec::new();
        let req = builder(AgentId::new(), WorkspaceAccess::ReadOnly)
            .parent_access(WorkspaceAccess::ReadOnly)
            .with_task_context("SELECTED-CONTEXT-123")
            .build()
            .expect("request");
        let mut env = SpawnEnv::new(
            &scheduler,
            &views,
            &(),
            &mut model,
            &mut tools,
            &mut lifecycle,
            &mut turn_events,
        );
        let spawned = spawn_agent(req, &mut env, &CancellationToken::new()).expect("spawn");
        let text: String = spawned
            .prompt()
            .messages()
            .iter()
            .map(|message| message.content())
            .collect::<Vec<_>>()
            .join("\n");
        // The child receives its own task contract and only the parent-selected
        // context excerpt.
        assert!(
            text.contains("inspect crate"),
            "child task contract present"
        );
        assert!(
            text.contains("SELECTED-CONTEXT-123"),
            "selected context present"
        );
        // The full parent transcript / unrelated history is never threaded in.
        assert!(
            !text.contains("PARENT-ONLY-UNRELATED"),
            "parent transcript/history must not leak into the child"
        );
    }

    #[test]
    fn selected_context_is_delimited_untrusted_data_not_instructions() {
        let views = ViewRegistry::new();
        let scheduler = scheduler();
        let mut model = ImmediateModel::new();
        let mut tools = NoTools;
        let mut lifecycle = Vec::new();
        let mut turn_events = Vec::new();
        let req = builder(AgentId::new(), WorkspaceAccess::ReadOnly)
            .parent_access(WorkspaceAccess::ReadOnly)
            .with_task_context("CONTEXT-456")
            .build()
            .expect("request");
        let mut env = SpawnEnv::new(
            &scheduler,
            &views,
            &(),
            &mut model,
            &mut tools,
            &mut lifecycle,
            &mut turn_events,
        );
        let spawned = spawn_agent(req, &mut env, &CancellationToken::new()).expect("spawn");
        let context = spawned
            .prompt()
            .messages()
            .iter()
            .find(|message| message.section() == PromptSection::Context)
            .expect("context section");
        let content = context.content();
        assert!(content.contains("<untrusted-data"));
        assert!(content.contains("parent-selected"));
        assert!(content.contains("CONTEXT-456"));
    }

    #[test]
    fn spawned_event_emitted_before_child_turn_executes() {
        let views = ViewRegistry::new();
        let scheduler = scheduler();
        let mut model = ImmediateModel::new();
        let mut tools = NoTools;
        let mut lifecycle = Vec::new();
        let mut turn_events = Vec::new();
        let req = request(WorkspaceAccess::ReadOnly);
        let mut env = SpawnEnv::new(
            &scheduler,
            &views,
            &(),
            &mut model,
            &mut tools,
            &mut lifecycle,
            &mut turn_events,
        );
        spawn_agent(req, &mut env, &CancellationToken::new()).expect("spawn");
        assert_eq!(lifecycle[0].kind(), SpawnEventKind::AgentSpawned);
        assert_eq!(lifecycle[0].kind().as_str(), "agent.spawned");
        assert_eq!(turn_events[0].kind().as_str(), "turn.started");
        assert_eq!(lifecycle[1].kind(), SpawnEventKind::AgentStarted);
    }

    #[test]
    fn lifecycle_sink_failure_does_not_execute_turn() {
        let views = ViewRegistry::new();
        let scheduler = scheduler();
        let mut model = CountingModel::new();
        let mut tools = NoTools;
        let mut lifecycle = RejectingSink;
        let mut turn_events = Vec::new();
        let req = request(WorkspaceAccess::ReadOnly);
        let mut env = SpawnEnv::new(
            &scheduler,
            &views,
            &(),
            &mut model,
            &mut tools,
            &mut lifecycle,
            &mut turn_events,
        );
        let err = spawn_agent(req, &mut env, &CancellationToken::new()).expect_err("sink");
        assert_eq!(err, SpawnError::EventSink);
        assert_eq!(model.steps.get(), 0);
        assert!(turn_events.is_empty());
    }

    #[test]
    fn parent_cannot_pass_raw_capability_lease() {
        let lease = FakeLeaseToken([0x51; 32]);
        assert_eq!(lease.marker_byte(), 0x51);
        let err = builder(AgentId::new(), WorkspaceAccess::ReadOnly)
            .with_parent_lease(lease)
            .expect_err("denied");
        assert_eq!(err, SpawnError::LeaseNotTransferable);
        assert_eq!(err.code(), Some(ErrorCode::PolicyLeaseInvalid));
        assert!(!err.as_str().contains("51"));

        let built = request(WorkspaceAccess::ReadOnly);
        let err = built
            .with_parent_lease(FakeLeaseToken([0x42; 32]))
            .expect_err("denied built");
        assert_eq!(err, SpawnError::LeaseNotTransferable);
        assert!(!format!("{err}").contains("lease token"));
    }

    #[test]
    fn read_only_parent_cannot_escalate_to_write_isolated() {
        let err = builder(AgentId::new(), WorkspaceAccess::WriteIsolated)
            .parent_access(WorkspaceAccess::ReadOnly)
            .build()
            .expect_err("escalation");
        assert_eq!(err, SpawnError::CapabilityEscalation);
        assert_eq!(err.code(), Some(ErrorCode::PolicyDenied));
    }

    #[test]
    fn write_siblings_receive_distinct_views() {
        let views = ViewRegistry::new();
        let scheduler = scheduler();
        let parent = AgentId::new();
        let mut first_id = None;
        let mut second_id = None;
        for slot in [&mut first_id, &mut second_id] {
            let req = builder(parent, WorkspaceAccess::WriteIsolated)
                .parent_access(WorkspaceAccess::WriteIsolated)
                .build()
                .expect("request");
            let mut model = ImmediateModel::new();
            let mut tools = NoTools;
            let mut lifecycle = Vec::new();
            let mut turn_events = Vec::new();
            let mut env = SpawnEnv::new(
                &scheduler,
                &views,
                &(),
                &mut model,
                &mut tools,
                &mut lifecycle,
                &mut turn_events,
            );
            let spawned = spawn_agent(req, &mut env, &CancellationToken::new()).expect("spawn");
            *slot = Some(spawned.view().id());
        }
        assert_ne!(first_id, second_id);
    }

    #[test]
    fn cancelled_spawn_does_not_emit_or_run() {
        let views = ViewRegistry::new();
        let scheduler = scheduler();
        let mut model = CountingModel::new();
        let mut tools = NoTools;
        let mut lifecycle = Vec::new();
        let mut turn_events = Vec::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let req = request(WorkspaceAccess::ReadOnly);
        let mut env = SpawnEnv::new(
            &scheduler,
            &views,
            &(),
            &mut model,
            &mut tools,
            &mut lifecycle,
            &mut turn_events,
        );
        let err = spawn_agent(req, &mut env, &cancel).expect_err("cancelled");
        assert_eq!(err, SpawnError::Cancelled);
        assert!(err.code().is_none());
        assert!(lifecycle.is_empty());
        assert_eq!(model.steps.get(), 0);
    }

    #[test]
    fn main_role_and_empty_task_are_rejected() {
        let err = SpawnRequest::builder(
            AgentId::new(),
            AgentRole::Main,
            "inspect crate",
            WorkspaceAccess::ReadOnly,
            schema(),
            provider(),
            RepoId::new(),
            "base-rev",
            SessionId::new(),
            catalog(),
        )
        .build()
        .expect_err("main");
        assert_eq!(err, SpawnError::InvalidRole);

        let err = SpawnRequest::builder(
            AgentId::new(),
            AgentRole::Explorer,
            "",
            WorkspaceAccess::ReadOnly,
            schema(),
            provider(),
            RepoId::new(),
            "base-rev",
            SessionId::new(),
            catalog(),
        )
        .build()
        .expect_err("empty");
        assert_eq!(err, SpawnError::InvalidTask);
    }

    #[derive(Clone, Copy)]
    struct FakeLeaseToken([u8; 32]);

    impl FakeLeaseToken {
        fn marker_byte(self) -> u8 {
            self.0[0]
        }
    }
}
