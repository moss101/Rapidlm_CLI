//! Subagent result persist, parent inspect, and explicit merge handoff.
//!
//! `complete_agent` records the typed summary, evidence, and view. The
//! child's natural-language summary is never treated as a patch. Merge goes
//! through the workspace preview and transaction API. A parent can inspect
//! the stored result after a merge conflict. There is no shared chat.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use protocol::{AgentId, ArtifactId, ErrorCode, WorkspaceViewId};
use workspace::CancellationToken as WorkspaceCancel;
use workspace::{
    CommitReceipt, DirectBackend, MergeError, MergePreview, MergeView, SemanticPatch,
    TransactionError, TransactionManager, VerificationHook, WorkspaceView,
};

use super::model::{Agent, AgentModelError, AgentResult, AgentTerminalStatus, CancellationToken};

/// Maximum typed child results retained by one [`ResultStore`].
pub const MAX_STORED_RESULTS: usize = 256;

/// Maximum lifecycle events one complete/merge may emit through [`ResultEventSink`].
pub const MAX_RESULT_EVENTS: usize = 32;

/// In-process store of completed child results for parent inspect/merge.
pub struct ResultStore {
    inner: Mutex<Inner>,
    max_results: usize,
}

struct Inner {
    records: BTreeMap<AgentId, StoredHandoff>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredHandoff {
    parent: AgentId,
    result: AgentResult,
    merge: Option<MergeStatus>,
}

/// Last explicit merge attempt. Never inferred from summary text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeStatus {
    Committed {
        receipt: CommitReceipt,
    },
    Conflict {
        preview_hash: ArtifactId,
        conflicts: usize,
    },
}

/// Parent-visible child result. Contains no transcript or hidden chat.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectedResult {
    parent: AgentId,
    result: AgentResult,
    merge: Option<MergeStatus>,
}

/// Explicit child→parent merge inputs. Summary text is not an input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeRequest {
    parent: AgentId,
    child: AgentId,
    child_view: WorkspaceView,
    parent_view: WorkspaceView,
    child_patch: SemanticPatch,
    parent_patch: SemanticPatch,
}

/// Collaborators for one [`merge_result`] call.
pub struct MergeEnv<'a, S> {
    transactions: &'a TransactionManager,
    source: &'a DirectBackend,
    hooks: &'a [&'a dyn VerificationHook],
    events: &'a mut S,
}

/// Outcome of an explicit merge. Conflicts still carry the typed result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeHandoff {
    result: AgentResult,
    preview: MergePreview,
    receipt: Option<CommitReceipt>,
}

/// Durable result/merge event. Payloads never include summary or patch bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResultEvent {
    Completed {
        agent_id: AgentId,
        parent: AgentId,
        status: AgentTerminalStatus,
        workspace_view: Option<WorkspaceViewId>,
        evidence: usize,
    },
    MergeCommitted {
        agent_id: AgentId,
        parent: AgentId,
        parent_view: WorkspaceViewId,
        preview_hash: ArtifactId,
        change_count: usize,
    },
}

/// Kernel `EventKind` wire form for [`ResultEvent`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum ResultEventKind {
    AgentResult,
    WorkspaceTransactionCommitted,
}

/// Bounded sink for durable result lifecycle events.
pub trait ResultEventSink {
    fn emit(&mut self, event: ResultEvent) -> Result<(), ResultError>;
}

/// Typed result/merge failure. Display never echoes summary or patch text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResultError {
    Cancelled,
    MissingParent,
    AgentNotFound { agent_id: AgentId },
    ParentMismatch { expected: AgentId, found: AgentId },
    AlreadyCompleted { agent_id: AgentId },
    AlreadyMerged { agent_id: AgentId },
    SharedWriteView { view_id: WorkspaceViewId },
    ViewMismatch,
    BoundExceeded,
    EventSink,
    LockPoisoned,
    Agent(AgentModelError),
    Merge(MergeError),
    Transaction(TransactionError),
}

impl ResultStore {
    pub fn new() -> Self {
        Self::with_limit(MAX_STORED_RESULTS)
    }

    pub fn with_limit(max_results: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                records: BTreeMap::new(),
            }),
            max_results,
        }
    }

    /// Persist a typed child result. Does not merge or apply code.
    pub fn complete(
        &self,
        agent: &mut Agent,
        result: AgentResult,
        events: &mut impl ResultEventSink,
        cancel: &CancellationToken,
    ) -> Result<AgentResult, ResultError> {
        check_cancel(cancel)?;
        let parent = agent.spec().parent_id().ok_or(ResultError::MissingParent)?;
        if result.agent_id() != agent.spec().id() {
            return Err(ResultError::from(AgentModelError::ResultAgentMismatch));
        }
        if let Some(view) = result.workspace_view()
            && view != agent.spec().workspace_view_id() {
                return Err(ResultError::from(AgentModelError::ResultViewMismatch));
            }

        {
            let inner = self.lock()?;
            check_cancel(cancel)?;
            if inner.records.contains_key(&agent.spec().id()) {
                return Err(ResultError::AlreadyCompleted {
                    agent_id: agent.spec().id(),
                });
            }
            if inner.records.len() >= self.max_results {
                return Err(ResultError::BoundExceeded);
            }
        }

        agent.complete(result.clone(), cancel)?;
        let stored = StoredHandoff {
            parent,
            result: result.clone(),
            merge: None,
        };
        {
            let mut inner = self.lock()?;
            if inner.records.contains_key(&stored.result.agent_id()) {
                return Err(ResultError::AlreadyCompleted {
                    agent_id: stored.result.agent_id(),
                });
            }
            if inner.records.len() >= self.max_results {
                return Err(ResultError::BoundExceeded);
            }
            inner.records.insert(stored.result.agent_id(), stored);
        }

        events.emit(ResultEvent::Completed {
            agent_id: result.agent_id(),
            parent,
            status: result.status(),
            workspace_view: result.workspace_view(),
            evidence: result.evidence().len(),
        })?;
        Ok(result)
    }

    /// Return the typed child result. Works after a merge conflict.
    pub fn inspect(
        &self,
        parent: AgentId,
        child: AgentId,
        cancel: &CancellationToken,
    ) -> Result<InspectedResult, ResultError> {
        check_cancel(cancel)?;
        let inner = self.lock()?;
        check_cancel(cancel)?;
        let stored = inner
            .records
            .get(&child)
            .ok_or(ResultError::AgentNotFound { agent_id: child })?;
        if stored.parent != parent {
            return Err(ResultError::ParentMismatch {
                expected: stored.parent,
                found: parent,
            });
        }
        Ok(InspectedResult {
            parent: stored.parent,
            result: stored.result.clone(),
            merge: stored.merge.clone(),
        })
    }

    /// Preview and optionally commit through the workspace transaction API.
    ///
    /// Child summary text is not read. Conflicts do not hide the stored result.
    pub fn merge<S: ResultEventSink>(
        &self,
        request: MergeRequest,
        env: &mut MergeEnv<'_, S>,
        cancel: &CancellationToken,
    ) -> Result<MergeHandoff, ResultError> {
        check_cancel(cancel)?;
        request.validate()?;
        let stored = {
            let inner = self.lock()?;
            check_cancel(cancel)?;
            authorize_parent(&inner, request.parent, request.child)?
        };
        if matches!(stored.merge, Some(MergeStatus::Committed { .. })) {
            return Err(ResultError::AlreadyMerged {
                agent_id: request.child,
            });
        }
        if stored.result.workspace_view() != Some(request.child_view.id()) {
            return Err(ResultError::ViewMismatch);
        }

        let ws_cancel = workspace_cancel(cancel);
        check_cancel(cancel)?;
        let preview = MergeView::new(
            request.child_view.clone(),
            request.parent_view.clone(),
            request.child_patch.clone(),
            request.parent_patch.clone(),
        )
        .preview(&ws_cancel)?;

        if !preview.is_conflict_free() {
            self.record_merge(
                request.child,
                MergeStatus::Conflict {
                    preview_hash: preview.preview_hash(),
                    conflicts: preview.conflicts().len(),
                },
            )?;
            return Ok(MergeHandoff {
                result: stored.result,
                preview,
                receipt: None,
            });
        }

        check_cancel(cancel)?;
        let ws_cancel = workspace_cancel(cancel);
        let tx = env.transactions.begin_transaction(
            &preview,
            &request.parent_view,
            env.source,
            request.child,
            &ws_cancel,
        )?;
        let ws_cancel = workspace_cancel(cancel);
        let receipt = match env.transactions.commit_transaction(
            tx,
            &request.parent_view,
            env.hooks,
            &ws_cancel,
        ) {
            Ok(receipt) => receipt,
            Err(err) => {
                if cancel.is_cancelled() {
                    return Err(ResultError::Cancelled);
                }
                return Err(ResultError::from(err));
            }
        };

        self.record_merge(
            request.child,
            MergeStatus::Committed {
                receipt: receipt.clone(),
            },
        )?;
        env.events.emit(ResultEvent::MergeCommitted {
            agent_id: request.child,
            parent: request.parent,
            parent_view: receipt.parent_view_id(),
            preview_hash: receipt.preview_hash(),
            change_count: receipt.change_count(),
        })?;
        Ok(MergeHandoff {
            result: stored.result,
            preview,
            receipt: Some(receipt),
        })
    }

    fn record_merge(&self, child: AgentId, merge: MergeStatus) -> Result<(), ResultError> {
        let mut inner = self.lock()?;
        let stored = inner
            .records
            .get_mut(&child)
            .ok_or(ResultError::AgentNotFound { agent_id: child })?;
        stored.merge = Some(merge);
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, ResultError> {
        self.inner.lock().map_err(|_| ResultError::LockPoisoned)
    }
}

impl Default for ResultStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MergeStatus {
    pub const fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }

    pub fn receipt(&self) -> Option<&CommitReceipt> {
        match self {
            Self::Committed { receipt } => Some(receipt),
            Self::Conflict { .. } => None,
        }
    }
}

impl InspectedResult {
    pub fn parent(&self) -> AgentId {
        self.parent
    }

    pub fn result(&self) -> &AgentResult {
        &self.result
    }

    pub fn merge(&self) -> Option<&MergeStatus> {
        self.merge.as_ref()
    }
}

impl MergeRequest {
    pub fn new(
        parent: AgentId,
        child: AgentId,
        child_view: WorkspaceView,
        parent_view: WorkspaceView,
        child_patch: SemanticPatch,
        parent_patch: SemanticPatch,
    ) -> Self {
        Self {
            parent,
            child,
            child_view,
            parent_view,
            child_patch,
            parent_patch,
        }
    }

    pub fn parent(&self) -> AgentId {
        self.parent
    }

    pub fn child(&self) -> AgentId {
        self.child
    }

    pub fn child_view(&self) -> &WorkspaceView {
        &self.child_view
    }

    pub fn parent_view(&self) -> &WorkspaceView {
        &self.parent_view
    }

    pub fn child_patch(&self) -> &SemanticPatch {
        &self.child_patch
    }

    pub fn parent_patch(&self) -> &SemanticPatch {
        &self.parent_patch
    }

    fn validate(&self) -> Result<(), ResultError> {
        if self.child_view.id() == self.parent_view.id() {
            return Err(ResultError::SharedWriteView {
                view_id: self.child_view.id(),
            });
        }
        if self.child_patch.author() != self.child {
            return Err(ResultError::ViewMismatch);
        }
        Ok(())
    }
}

impl<'a, S> MergeEnv<'a, S> {
    pub fn new(
        transactions: &'a TransactionManager,
        source: &'a DirectBackend,
        hooks: &'a [&'a dyn VerificationHook],
        events: &'a mut S,
    ) -> Self {
        Self {
            transactions,
            source,
            hooks,
            events,
        }
    }
}

impl MergeHandoff {
    pub fn result(&self) -> &AgentResult {
        &self.result
    }

    pub fn preview(&self) -> &MergePreview {
        &self.preview
    }

    pub fn receipt(&self) -> Option<&CommitReceipt> {
        self.receipt.as_ref()
    }

    pub fn is_conflict(&self) -> bool {
        !self.preview.is_conflict_free()
    }
}

impl ResultEvent {
    pub const fn kind(&self) -> ResultEventKind {
        match self {
            Self::Completed { .. } => ResultEventKind::AgentResult,
            Self::MergeCommitted { .. } => ResultEventKind::WorkspaceTransactionCommitted,
        }
    }

    pub const fn agent_id(&self) -> AgentId {
        match self {
            Self::Completed { agent_id, .. } | Self::MergeCommitted { agent_id, .. } => *agent_id,
        }
    }
}

impl ResultEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentResult => "agent.result",
            Self::WorkspaceTransactionCommitted => "workspace.transaction_committed",
        }
    }
}

impl ResultEventSink for Vec<ResultEvent> {
    fn emit(&mut self, event: ResultEvent) -> Result<(), ResultError> {
        if self.len() >= MAX_RESULT_EVENTS {
            return Err(ResultError::BoundExceeded);
        }
        self.push(event);
        Ok(())
    }
}

/// Persist a typed child result. Never applies workspace changes.
pub fn complete_agent<S: ResultEventSink>(
    store: &ResultStore,
    agent: &mut Agent,
    result: AgentResult,
    events: &mut S,
    cancel: &CancellationToken,
) -> Result<AgentResult, ResultError> {
    store.complete(agent, result, events, cancel)
}

/// Inspect a stored child result. Conflicts do not hide it.
pub fn inspect_result(
    store: &ResultStore,
    parent: AgentId,
    child: AgentId,
    cancel: &CancellationToken,
) -> Result<InspectedResult, ResultError> {
    store.inspect(parent, child, cancel)
}

/// Merge an explicit child patch through the workspace transaction API.
pub fn merge_result<S: ResultEventSink>(
    store: &ResultStore,
    request: MergeRequest,
    env: &mut MergeEnv<'_, S>,
    cancel: &CancellationToken,
) -> Result<MergeHandoff, ResultError> {
    store.merge(request, env, cancel)
}

fn authorize_parent(
    inner: &Inner,
    parent: AgentId,
    child: AgentId,
) -> Result<StoredHandoff, ResultError> {
    let stored = inner
        .records
        .get(&child)
        .ok_or(ResultError::AgentNotFound { agent_id: child })?;
    if stored.parent != parent {
        return Err(ResultError::ParentMismatch {
            expected: stored.parent,
            found: parent,
        });
    }
    Ok(stored.clone())
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), ResultError> {
    if cancel.is_cancelled() {
        Err(ResultError::Cancelled)
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

impl ResultError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "agent result operation cancelled",
            Self::MissingParent => "only a child agent result can be persisted for parent inspect",
            Self::AgentNotFound { .. } => "child agent result is not stored",
            Self::ParentMismatch { .. } => "only the parent may inspect or merge a child result",
            Self::AlreadyCompleted { .. } => "child agent result is already stored",
            Self::AlreadyMerged { .. } => "child result is already merged into the parent view",
            Self::SharedWriteView { .. } => "write-capable agents cannot share a workspace view",
            Self::ViewMismatch => "child result view does not match the merge request",
            Self::BoundExceeded => "agent result resource bound exceeded",
            Self::EventSink => "agent result event sink rejected an event",
            Self::LockPoisoned => "agent result store lock poisoned",
            Self::Agent(_) => "agent model rejected result persist",
            Self::Merge(_) => "workspace merge preview failed",
            Self::Transaction(_) => "workspace merge transaction failed",
        }
    }

    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::MissingParent
            | Self::AlreadyCompleted { .. }
            | Self::AlreadyMerged { .. }
            | Self::ViewMismatch => Some(ErrorCode::ConfigInvalid),
            Self::AgentNotFound { .. } => Some(ErrorCode::InternalUnexpected),
            Self::ParentMismatch { .. } => Some(ErrorCode::PolicyDenied),
            Self::SharedWriteView { .. } => Some(ErrorCode::AgentConcurrencyLimit),
            Self::BoundExceeded => Some(ErrorCode::ProcessTimeout),
            Self::EventSink | Self::LockPoisoned | Self::Agent(_) => {
                Some(ErrorCode::InternalUnexpected)
            }
            Self::Merge(MergeError::Cancelled) => None,
            Self::Merge(MergeError::BoundExceeded) => Some(ErrorCode::ProcessTimeout),
            Self::Merge(_) => Some(ErrorCode::ConfigInvalid),
            Self::Transaction(err) => err.code().or(match err {
                TransactionError::Cancelled => None,
                TransactionError::PreviewConflict => Some(ErrorCode::WorkspaceMergeConflict),
                TransactionError::BoundExceeded | TransactionError::TransactionLimit { .. } => {
                    Some(ErrorCode::ProcessTimeout)
                }
                _ => Some(ErrorCode::InternalUnexpected),
            }),
        }
    }
}

impl fmt::Display for MergeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Committed { .. } => f.write_str("committed"),
            Self::Conflict { .. } => f.write_str("conflict"),
        }
    }
}

impl fmt::Display for ResultEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ResultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ResultError {}

impl From<AgentModelError> for ResultError {
    fn from(err: AgentModelError) -> Self {
        match err {
            AgentModelError::Cancelled => Self::Cancelled,
            other => Self::Agent(other),
        }
    }
}

impl From<MergeError> for ResultError {
    fn from(err: MergeError) -> Self {
        match err {
            MergeError::Cancelled => Self::Cancelled,
            other => Self::Merge(other),
        }
    }
}

impl From<TransactionError> for ResultError {
    fn from(err: TransactionError) -> Self {
        match err {
            TransactionError::Cancelled => Self::Cancelled,
            other => Self::Transaction(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::model::{AgentRole, AgentSpec, AgentState, PatchSummary};
    use protocol::{ArtifactId, RepoId, RepoPath};
    use std::fs;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use workspace::{
        AcceptHook, CreateView, DirectOptions, PatchOp, ViewAccess, ViewRegistry, WorkspaceBackend,
    };

    const PARENT_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ac";
    const EVIDENCE_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ae";
    static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    struct Fixture {
        dir: PathBuf,
        backend: Option<DirectBackend>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.backend.take();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    struct RejectingSink;

    impl ResultEventSink for RejectingSink {
        fn emit(&mut self, _event: ResultEvent) -> Result<(), ResultError> {
            Err(ResultError::EventSink)
        }
    }

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn parse_id<T: FromStr>(raw: &str) -> T
    where
        T::Err: std::fmt::Debug,
    {
        raw.parse().expect("id")
    }

    fn parent() -> AgentId {
        parse_id(PARENT_ID)
    }

    fn repo_path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn hello_preimage() -> ArtifactId {
        ArtifactId::from_bytes(b"hello")
    }

    fn replace(path: &str, start: u64, end: u64, content: &str) -> PatchOp {
        PatchOp::replace_range(
            repo_path(path),
            hello_preimage(),
            start,
            end,
            content.to_owned(),
        )
        .expect("replace")
    }

    fn patch(author: AgentId, ops: Vec<PatchOp>) -> SemanticPatch {
        SemanticPatch::new(ops, author, "deadbeef", &workspace_cancel(&cancel())).expect("patch")
    }

    fn running_child(parent_id: AgentId, view: WorkspaceViewId) -> Agent {
        let spec = AgentSpec::builder(AgentId::new(), AgentRole::Coder, "review auth crate", view)
            .parent_id(Some(parent_id))
            .permissions_profile("child.write_isolated")
            .build()
            .expect("spec");
        let mut agent = Agent::spawn(spec, &cancel()).expect("spawn");
        agent
            .transition(AgentState::Starting, &cancel())
            .expect("starting");
        agent
            .transition(AgentState::Running, &cancel())
            .expect("running");
        agent
    }

    fn child_result(agent: &Agent, summary: &str) -> AgentResult {
        AgentResult::new(
            agent.spec().id(),
            AgentTerminalStatus::Succeeded,
            summary,
            vec![parse_id(EVIDENCE_ID)],
            Some(agent.spec().workspace_view_id()),
            Some(PatchSummary::new(1, 4, 0)),
            Vec::new(),
        )
        .expect("result")
    }

    fn complete_child(
        store: &ResultStore,
        parent_id: AgentId,
        view: WorkspaceViewId,
        summary: &str,
    ) -> (Agent, AgentResult, Vec<ResultEvent>) {
        let mut agent = running_child(parent_id, view);
        let result = child_result(&agent, summary);
        let mut events = Vec::new();
        let stored = complete_agent(store, &mut agent, result.clone(), &mut events, &cancel())
            .expect("complete");
        (agent, stored, events)
    }

    fn live_pair() -> (ViewRegistry, WorkspaceView, WorkspaceView) {
        let registry = ViewRegistry::new();
        let repo = RepoId::new();
        let child = registry
            .create(
                CreateView::new(
                    repo,
                    WorkspaceBackend::GitWorktree,
                    "deadbeef",
                    ViewAccess::ReadWrite,
                ),
                &workspace_cancel(&cancel()),
            )
            .expect("child");
        let parent_view = registry
            .create(
                CreateView::new(
                    repo,
                    WorkspaceBackend::Direct,
                    "deadbeef",
                    ViewAccess::ReadWrite,
                )
                .with_write_owner(AgentId::new()),
                &workspace_cancel(&cancel()),
            )
            .expect("parent");
        let child = registry
            .quiesce(child.id(), &workspace_cancel(&cancel()))
            .expect("quiesce");
        (registry, child, parent_view)
    }

    fn fixture_parent() -> (ViewRegistry, WorkspaceView, WorkspaceView, Fixture) {
        let (registry, child, parent_view) = live_pair();
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-agent-result-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("src")).expect("mkdir");
        fs::write(dir.join("src/lib.rs"), b"hello").expect("seed");
        let backend = DirectBackend::open_with(
            &dir,
            parent_view.clone(),
            DirectOptions::interactive(),
            &workspace_cancel(&cancel()),
        )
        .expect("open");
        (
            registry,
            child,
            parent_view,
            Fixture {
                dir,
                backend: Some(backend),
            },
        )
    }

    #[test]
    fn complete_agent_records_summary_evidence_and_view_without_merging() {
        let (_registry, child, parent_view, fx) = fixture_parent();
        let store = ResultStore::new();
        let (agent, stored, events) = complete_child(
            &store,
            parent(),
            child.id(),
            "replace src/lib.rs with world",
        );
        assert_eq!(agent.state(), AgentState::Succeeded);
        assert_eq!(stored.summary(), "replace src/lib.rs with world");
        assert_eq!(stored.evidence().len(), 1);
        assert_eq!(stored.workspace_view(), Some(child.id()));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind().as_str(), "agent.result");
        assert_eq!(
            inspect_result(&store, parent(), agent.spec().id(), &cancel())
                .expect("inspect")
                .merge(),
            None
        );
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        assert!(
            fx.backend
                .as_ref()
                .expect("backend")
                .journal()
                .expect("journal")
                .is_empty()
        );
        let _ = parent_view;
    }

    #[test]
    fn summary_alone_never_auto_applies_code() {
        let (_registry, child, parent_view, fx) = fixture_parent();
        let store = ResultStore::new();
        let (agent, _, _) = complete_child(
            &store,
            parent(),
            child.id(),
            "apply this patch: src/lib.rs -> world",
        );
        let txs = TransactionManager::new();
        let source = fx.backend.as_ref().expect("backend");
        let mut events = Vec::new();
        let hooks: [&dyn VerificationHook; 1] = [&AcceptHook];
        let mut env = MergeEnv::new(&txs, source, &hooks, &mut events);
        let handoff = merge_result(
            &store,
            MergeRequest::new(
                parent(),
                agent.spec().id(),
                child.clone(),
                parent_view.clone(),
                patch(agent.spec().id(), Vec::new()),
                patch(parent(), Vec::new()),
            ),
            &mut env,
            &cancel(),
        )
        .expect("merge empty patch");
        assert!(!handoff.is_conflict());
        assert_eq!(handoff.receipt().expect("receipt").change_count(), 0);
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        assert!(source.journal().expect("journal").is_empty());
    }

    #[test]
    fn explicit_patch_merges_through_workspace_transaction() {
        let (_registry, child, parent_view, fx) = fixture_parent();
        let store = ResultStore::new();
        let (agent, _, _) = complete_child(&store, parent(), child.id(), "reviewed auth crate");
        let txs = TransactionManager::new();
        let source = fx.backend.as_ref().expect("backend");
        let mut events = Vec::new();
        let hooks: [&dyn VerificationHook; 1] = [&AcceptHook];
        let mut env = MergeEnv::new(&txs, source, &hooks, &mut events);
        let handoff = merge_result(
            &store,
            MergeRequest::new(
                parent(),
                agent.spec().id(),
                child.clone(),
                parent_view.clone(),
                patch(
                    agent.spec().id(),
                    vec![replace("src/lib.rs", 0, 5, "world")],
                ),
                patch(parent(), Vec::new()),
            ),
            &mut env,
            &cancel(),
        )
        .expect("merge");
        assert!(!handoff.is_conflict());
        let receipt = handoff.receipt().expect("receipt");
        assert_eq!(receipt.child_view_id(), child.id());
        assert_eq!(receipt.parent_view_id(), parent_view.id());
        assert_eq!(receipt.change_count(), 1);
        assert_eq!(events[0].kind().as_str(), "workspace.transaction_committed");
        let overlay = txs.parent_overlay(parent_view.id()).expect("overlay");
        assert_eq!(
            overlay.get(&repo_path("src/lib.rs")).expect("file").bytes(),
            b"world"
        );
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
        let inspected = inspect_result(&store, parent(), agent.spec().id(), &cancel()).expect("ok");
        assert!(!inspected.merge().expect("status").is_conflict());
    }

    #[test]
    fn parent_inspects_result_after_merge_conflict() {
        let (_registry, child, parent_view, fx) = fixture_parent();
        let store = ResultStore::new();
        let (agent, stored, _) =
            complete_child(&store, parent(), child.id(), "reviewed auth crate");
        let txs = TransactionManager::new();
        let source = fx.backend.as_ref().expect("backend");
        let mut events = Vec::new();
        let hooks: [&dyn VerificationHook; 0] = [];
        let mut env = MergeEnv::new(&txs, source, &hooks, &mut events);
        let handoff = merge_result(
            &store,
            MergeRequest::new(
                parent(),
                agent.spec().id(),
                child.clone(),
                parent_view.clone(),
                patch(
                    agent.spec().id(),
                    vec![replace("src/lib.rs", 0, 5, "child")],
                ),
                patch(parent(), vec![replace("src/lib.rs", 0, 5, "sib")]),
            ),
            &mut env,
            &cancel(),
        )
        .expect("conflict is inspectable");
        assert!(handoff.is_conflict());
        assert!(handoff.receipt().is_none());
        assert_eq!(handoff.result(), &stored);
        assert_eq!(
            handoff.preview().code(),
            Some(ErrorCode::WorkspaceMergeConflict)
        );
        assert!(events.is_empty());
        assert!(txs.parent_visibility(parent_view.id()).is_none());
        let inspected =
            inspect_result(&store, parent(), agent.spec().id(), &cancel()).expect("inspect");
        assert_eq!(inspected.result().summary(), "reviewed auth crate");
        assert_eq!(inspected.result().evidence().len(), 1);
        assert_eq!(inspected.result().workspace_view(), Some(child.id()));
        assert!(inspected.merge().expect("status").is_conflict());
        assert_eq!(fs::read(fx.dir.join("src/lib.rs")).expect("disk"), b"hello");
    }

    #[test]
    fn foreign_parent_cannot_inspect_or_merge() {
        let (_registry, child, parent_view, fx) = fixture_parent();
        let store = ResultStore::new();
        let (agent, _, _) = complete_child(&store, parent(), child.id(), "reviewed auth crate");
        let stranger = AgentId::new();
        assert_eq!(
            inspect_result(&store, stranger, agent.spec().id(), &cancel()).expect_err("denied"),
            ResultError::ParentMismatch {
                expected: parent(),
                found: stranger,
            }
        );
        let txs = TransactionManager::new();
        let source = fx.backend.as_ref().expect("backend");
        let mut events = Vec::new();
        let hooks: [&dyn VerificationHook; 0] = [];
        let mut env = MergeEnv::new(&txs, source, &hooks, &mut events);
        let err = merge_result(
            &store,
            MergeRequest::new(
                stranger,
                agent.spec().id(),
                child,
                parent_view,
                patch(agent.spec().id(), Vec::new()),
                patch(stranger, Vec::new()),
            ),
            &mut env,
            &cancel(),
        )
        .expect_err("denied");
        assert_eq!(
            err,
            ResultError::ParentMismatch {
                expected: parent(),
                found: stranger,
            }
        );
        assert_eq!(err.code(), Some(ErrorCode::PolicyDenied));
    }

    #[test]
    fn main_agent_result_is_not_a_child_handoff() {
        let spec = AgentSpec::builder(
            AgentId::new(),
            AgentRole::Main,
            "top-level work",
            WorkspaceViewId::new(),
        )
        .permissions_profile("main")
        .build()
        .expect("spec");
        let mut agent = Agent::spawn(spec, &cancel()).expect("spawn");
        agent
            .transition(AgentState::Starting, &cancel())
            .expect("starting");
        agent
            .transition(AgentState::Running, &cancel())
            .expect("running");
        let result = AgentResult::new(
            agent.spec().id(),
            AgentTerminalStatus::Succeeded,
            "done",
            Vec::new(),
            Some(agent.spec().workspace_view_id()),
            None,
            Vec::new(),
        )
        .expect("result");
        let store = ResultStore::new();
        let mut events = Vec::new();
        assert_eq!(
            complete_agent(&store, &mut agent, result, &mut events, &cancel()).expect_err("main"),
            ResultError::MissingParent
        );
        assert_eq!(agent.state(), AgentState::Running);
        assert!(agent.result().is_none());
        assert!(events.is_empty());
    }

    #[test]
    fn cancellation_is_typed_and_does_not_persist() {
        let store = ResultStore::new();
        let mut agent = running_child(parent(), WorkspaceViewId::new());
        let result = child_result(&agent, "reviewed auth crate");
        let token = cancel();
        token.cancel();
        let mut events = Vec::new();
        assert_eq!(
            complete_agent(&store, &mut agent, result, &mut events, &token).expect_err("cancel"),
            ResultError::Cancelled
        );
        assert_eq!(agent.state(), AgentState::Running);
        assert!(events.is_empty());
    }

    #[test]
    fn store_bound_and_shared_view_fail_closed() {
        let (_registry, child, parent_view) = live_pair();
        let store = ResultStore::with_limit(1);
        let (first, _, _) = complete_child(&store, parent(), child.id(), "first child done");
        let mut second = running_child(parent(), WorkspaceViewId::new());
        let result = child_result(&second, "second child done");
        let mut events = Vec::new();
        assert_eq!(
            complete_agent(&store, &mut second, result, &mut events, &cancel()).expect_err("bound"),
            ResultError::BoundExceeded
        );
        let err = MergeRequest::new(
            parent(),
            first.spec().id(),
            child.clone(),
            child,
            patch(first.spec().id(), Vec::new()),
            patch(parent(), Vec::new()),
        )
        .validate()
        .expect_err("shared");
        assert!(matches!(err, ResultError::SharedWriteView { .. }));
        let _ = parent_view;
    }

    #[test]
    fn event_sink_failure_keeps_inspectable_result() {
        let store = ResultStore::new();
        let mut agent = running_child(parent(), WorkspaceViewId::new());
        let result = child_result(&agent, "reviewed auth crate");
        let mut sink = RejectingSink;
        assert_eq!(
            complete_agent(&store, &mut agent, result, &mut sink, &cancel()).expect_err("sink"),
            ResultError::EventSink
        );
        let inspected =
            inspect_result(&store, parent(), agent.spec().id(), &cancel()).expect("inspect");
        assert_eq!(inspected.result().summary(), "reviewed auth crate");
        assert_eq!(agent.state(), AgentState::Succeeded);
    }
}
