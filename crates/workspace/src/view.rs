//! Isolated workspace view lifecycle and exclusive write-owner enforcement.
//!
//! Write acquisition is a single check-and-set under the registry lock. A
//! read-only view or an agent recorded as a read-only viewer can never become
//! the write owner. `base_revision` is immutable after create.

use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use protocol::{AgentId, RepoId, RepoPath, WorkspaceViewId};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

/// Wire schema name for [`WorkspaceView`].
pub const WORKSPACE_VIEW_SCHEMA: &str = "rapidlm.workspace_view";

/// v2 schema version for workspace-view objects (adds scope + generation).
pub const WORKSPACE_VIEW_SCHEMA_VERSION: u16 = 2;

/// Maximum UTF-8 bytes accepted in a base revision token.
pub const MAX_BASE_REVISION_BYTES: usize = 256;

/// Maximum live views in one [`ViewRegistry`].
pub const MAX_WORKSPACE_VIEWS: usize = 4096;

/// Maximum path prefixes on one view scope.
pub const MAX_VIEW_SCOPE_PREFIXES: usize = 64;

const VIEW_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "id",
    "repo_id",
    "backend",
    "base_revision",
    "write_owner",
    "state",
    "access",
    "generation",
    "scope",
];

/// Isolated logical filesystem backend. Wire form is snake_case.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum WorkspaceBackend {
    Direct,
    GitWorktree,
    Overlay,
    Remote,
}

/// Lifecycle state of a view. Write acquisition is allowed only while `Active`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum WorkspaceState {
    Active,
    Quiescent,
    Closed,
}

/// View-level write ceiling. `ReadOnly` cannot be upgraded after create.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ViewAccess {
    ReadOnly,
    ReadWrite,
}

/// Path scope for one view. Empty prefixes mean the whole repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewScope {
    prefixes: Vec<RepoPath>,
}

/// Isolated logical filesystem version. Fields are observational; the registry
/// is the authority for write ownership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceView {
    id: WorkspaceViewId,
    repo_id: RepoId,
    backend: WorkspaceBackend,
    base_revision: String,
    write_owner: Option<AgentId>,
    state: WorkspaceState,
    access: ViewAccess,
    generation: u64,
    scope: ViewScope,
}

/// Request to create a view. `write_owner` is accepted only for `ReadWrite`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateView {
    repo_id: RepoId,
    backend: WorkspaceBackend,
    base_revision: String,
    access: ViewAccess,
    write_owner: Option<AgentId>,
    scope: ViewScope,
}

/// In-process view lifecycle store. Write acquisition is atomic.
pub struct ViewRegistry {
    inner: Mutex<Inner>,
    max_views: usize,
}

/// Cooperative cancellation for view operations.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Typed view-lifecycle failure. Display never echoes revision text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceError {
    Cancelled,
    ViewNotFound {
        view_id: WorkspaceViewId,
    },
    WriteOwnerConflict {
        view_id: WorkspaceViewId,
        current: AgentId,
    },
    ReadOnlyView {
        view_id: WorkspaceViewId,
    },
    ReadOnlyViewer {
        view_id: WorkspaceViewId,
        agent_id: AgentId,
    },
    NotWriteOwner {
        view_id: WorkspaceViewId,
        agent_id: AgentId,
    },
    InvalidState {
        view_id: WorkspaceViewId,
        state: WorkspaceState,
    },
    InvalidBaseRevision,
    InvalidAccess,
    InvalidScope,
    UnknownVariant,
    TooManyViews {
        limit: usize,
    },
    LockPoisoned,
}

struct Inner {
    views: HashMap<WorkspaceViewId, ViewRecord>,
}

struct ViewRecord {
    view: WorkspaceView,
    readers: BTreeSet<AgentId>,
}

impl WorkspaceBackend {
    pub const ALL: &'static [Self] =
        &[Self::Direct, Self::GitWorktree, Self::Overlay, Self::Remote];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::GitWorktree => "git_worktree",
            Self::Overlay => "overlay",
            Self::Remote => "remote",
        }
    }
}

impl WorkspaceState {
    pub const ALL: &'static [Self] = &[Self::Active, Self::Quiescent, Self::Closed];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Quiescent => "quiescent",
            Self::Closed => "closed",
        }
    }
}

impl ViewAccess {
    pub const ALL: &'static [Self] = &[Self::ReadOnly, Self::ReadWrite];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::ReadWrite => "read_write",
        }
    }

    pub const fn is_writable(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

impl ViewScope {
    /// Whole-repository scope.
    pub fn repo() -> Self {
        Self {
            prefixes: Vec::new(),
        }
    }

    pub fn prefixes(prefixes: Vec<RepoPath>) -> Result<Self, WorkspaceError> {
        if prefixes.len() > MAX_VIEW_SCOPE_PREFIXES {
            return Err(WorkspaceError::InvalidScope);
        }
        Ok(Self { prefixes })
    }

    pub fn is_repo_wide(&self) -> bool {
        self.prefixes.is_empty()
    }

    pub fn prefixes_value(&self) -> &[RepoPath] {
        &self.prefixes
    }

    /// Prefix match: `src` includes `src/lib.rs` and not `srcfoo`.
    pub fn contains(&self, path: &RepoPath) -> bool {
        if self.prefixes.is_empty() {
            return true;
        }
        let raw = path.as_str();
        self.prefixes.iter().any(|prefix| {
            let prefix = prefix.as_str();
            raw == prefix
                || (raw.starts_with(prefix) && raw.as_bytes().get(prefix.len()) == Some(&b'/'))
        })
    }
}

impl WorkspaceView {
    pub fn id(&self) -> WorkspaceViewId {
        self.id
    }

    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn backend(&self) -> WorkspaceBackend {
        self.backend
    }

    pub fn base_revision(&self) -> &str {
        &self.base_revision
    }

    pub fn write_owner(&self) -> Option<AgentId> {
        self.write_owner
    }

    pub fn state(&self) -> WorkspaceState {
        self.state
    }

    pub fn access(&self) -> ViewAccess {
        self.access
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn scope(&self) -> &ViewScope {
        &self.scope
    }

    pub fn is_write_owner(&self, agent: AgentId) -> bool {
        self.write_owner == Some(agent)
    }
}

impl CreateView {
    pub fn new(
        repo_id: RepoId,
        backend: WorkspaceBackend,
        base_revision: impl Into<String>,
        access: ViewAccess,
    ) -> Self {
        Self {
            repo_id,
            backend,
            base_revision: base_revision.into(),
            access,
            write_owner: None,
            scope: ViewScope::repo(),
        }
    }

    pub fn with_write_owner(mut self, agent: AgentId) -> Self {
        self.write_owner = Some(agent);
        self
    }

    pub fn with_scope(mut self, scope: ViewScope) -> Self {
        self.scope = scope;
        self
    }
}

impl ViewRegistry {
    pub fn new() -> Self {
        Self::with_limit(MAX_WORKSPACE_VIEWS)
    }

    pub fn with_limit(max_views: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                views: HashMap::new(),
            }),
            max_views,
        }
    }

    /// Create a view. Optional initial write owner is installed atomically.
    pub fn create(
        &self,
        spec: CreateView,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceView, WorkspaceError> {
        cancel.check()?;
        let base_revision = parse_base_revision(&spec.base_revision)?;
        if spec.write_owner.is_some() && !spec.access.is_writable() {
            return Err(WorkspaceError::InvalidAccess);
        }

        let mut inner = self.lock()?;
        cancel.check()?;
        if inner.views.len() >= self.max_views {
            return Err(WorkspaceError::TooManyViews {
                limit: self.max_views,
            });
        }

        let id = WorkspaceViewId::new();
        let view = WorkspaceView {
            id,
            repo_id: spec.repo_id,
            backend: spec.backend,
            base_revision,
            write_owner: spec.write_owner,
            state: WorkspaceState::Active,
            access: spec.access,
            generation: 1,
            scope: spec.scope,
        };
        inner.views.insert(
            id,
            ViewRecord {
                view: view.clone(),
                readers: BTreeSet::new(),
            },
        );
        Ok(view)
    }

    pub fn get(
        &self,
        view_id: WorkspaceViewId,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceView, WorkspaceError> {
        cancel.check()?;
        let inner = self.lock()?;
        cancel.check()?;
        inner
            .views
            .get(&view_id)
            .map(|record| record.view.clone())
            .ok_or(WorkspaceError::ViewNotFound { view_id })
    }

    /// Atomically acquire exclusive write ownership. Already-owning is a no-op.
    pub fn acquire_write(
        &self,
        view_id: WorkspaceViewId,
        agent: AgentId,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceView, WorkspaceError> {
        cancel.check()?;
        let mut inner = self.lock()?;
        cancel.check()?;
        let record = inner
            .views
            .get_mut(&view_id)
            .ok_or(WorkspaceError::ViewNotFound { view_id })?;

        if record.view.state != WorkspaceState::Active {
            return Err(WorkspaceError::InvalidState {
                view_id,
                state: record.view.state,
            });
        }
        if !record.view.access.is_writable() {
            return Err(WorkspaceError::ReadOnlyView { view_id });
        }
        if record.readers.contains(&agent) {
            return Err(WorkspaceError::ReadOnlyViewer {
                view_id,
                agent_id: agent,
            });
        }
        match record.view.write_owner {
            Some(current) if current != agent => {
                return Err(WorkspaceError::WriteOwnerConflict { view_id, current });
            }
            Some(_) => {}
            None => {
                record.view.write_owner = Some(agent);
                bump_generation(&mut record.view);
            }
        }
        Ok(record.view.clone())
    }

    pub fn release_write(
        &self,
        view_id: WorkspaceViewId,
        agent: AgentId,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceView, WorkspaceError> {
        cancel.check()?;
        let mut inner = self.lock()?;
        cancel.check()?;
        let record = inner
            .views
            .get_mut(&view_id)
            .ok_or(WorkspaceError::ViewNotFound { view_id })?;

        match record.view.write_owner {
            Some(current) if current == agent => {
                record.view.write_owner = None;
                bump_generation(&mut record.view);
                Ok(record.view.clone())
            }
            Some(_) | None => Err(WorkspaceError::NotWriteOwner {
                view_id,
                agent_id: agent,
            }),
        }
    }

    /// Register a read-only viewer. That agent cannot later acquire write.
    pub fn attach_reader(
        &self,
        view_id: WorkspaceViewId,
        agent: AgentId,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceView, WorkspaceError> {
        cancel.check()?;
        let mut inner = self.lock()?;
        cancel.check()?;
        let record = inner
            .views
            .get_mut(&view_id)
            .ok_or(WorkspaceError::ViewNotFound { view_id })?;

        if record.view.state == WorkspaceState::Closed {
            return Err(WorkspaceError::InvalidState {
                view_id,
                state: record.view.state,
            });
        }
        if record.view.write_owner == Some(agent) {
            return Err(WorkspaceError::WriteOwnerConflict {
                view_id,
                current: agent,
            });
        }
        record.readers.insert(agent);
        Ok(record.view.clone())
    }

    /// Freeze an active view with no write owner so it can be merged.
    pub fn quiesce(
        &self,
        view_id: WorkspaceViewId,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceView, WorkspaceError> {
        self.transition(view_id, WorkspaceState::Quiescent, cancel)
    }

    pub fn close(
        &self,
        view_id: WorkspaceViewId,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceView, WorkspaceError> {
        self.transition(view_id, WorkspaceState::Closed, cancel)
    }

    fn transition(
        &self,
        view_id: WorkspaceViewId,
        next: WorkspaceState,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceView, WorkspaceError> {
        cancel.check()?;
        let mut inner = self.lock()?;
        cancel.check()?;
        let record = inner
            .views
            .get_mut(&view_id)
            .ok_or(WorkspaceError::ViewNotFound { view_id })?;

        if record.view.state == WorkspaceState::Closed {
            return Err(WorkspaceError::InvalidState {
                view_id,
                state: record.view.state,
            });
        }
        if next == WorkspaceState::Quiescent && record.view.state != WorkspaceState::Active {
            return Err(WorkspaceError::InvalidState {
                view_id,
                state: record.view.state,
            });
        }
        if let Some(current) = record.view.write_owner {
            return Err(WorkspaceError::WriteOwnerConflict { view_id, current });
        }
        record.view.state = next;
        bump_generation(&mut record.view);
        Ok(record.view.clone())
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, WorkspaceError> {
        self.inner.lock().map_err(|_| WorkspaceError::LockPoisoned)
    }
}

impl Default for ViewRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<(), WorkspaceError> {
        if self.is_cancelled() {
            Err(WorkspaceError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for WorkspaceBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for WorkspaceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ViewAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WorkspaceBackend {
    type Err = WorkspaceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl FromStr for WorkspaceState {
    type Err = WorkspaceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl FromStr for ViewAccess {
    type Err = WorkspaceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl Serialize for WorkspaceBackend {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Serialize for WorkspaceState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Serialize for ViewAccess {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for WorkspaceBackend {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl<'de> Deserialize<'de> for WorkspaceState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl<'de> Deserialize<'de> for ViewAccess {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl Serialize for WorkspaceView {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("WorkspaceView", VIEW_FIELDS.len())?;
        state.serialize_field("schema", WORKSPACE_VIEW_SCHEMA)?;
        state.serialize_field("schema_version", &WORKSPACE_VIEW_SCHEMA_VERSION)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("repo_id", &self.repo_id)?;
        state.serialize_field("backend", &self.backend)?;
        state.serialize_field("base_revision", &self.base_revision)?;
        state.serialize_field("write_owner", &self.write_owner)?;
        state.serialize_field("state", &self.state)?;
        state.serialize_field("access", &self.access)?;
        state.serialize_field("generation", &self.generation)?;
        state.serialize_field("scope", &self.scope.prefixes)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for WorkspaceView {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawWorkspaceView::deserialize(deserializer)?;
        if raw.schema != WORKSPACE_VIEW_SCHEMA {
            return Err(de::Error::custom("unsupported workspace view schema"));
        }
        if raw.schema_version != WORKSPACE_VIEW_SCHEMA_VERSION {
            return Err(de::Error::custom(
                "unsupported workspace view schema version",
            ));
        }
        let base_revision = parse_base_revision(&raw.base_revision).map_err(de::Error::custom)?;
        let view = WorkspaceView {
            id: raw.id,
            repo_id: raw.repo_id,
            backend: raw.backend,
            base_revision,
            write_owner: raw.write_owner,
            state: raw.state,
            access: raw.access,
            generation: raw.generation,
            scope: ViewScope::prefixes(raw.scope).map_err(de::Error::custom)?,
        };
        validate_view_invariants(&view).map_err(de::Error::custom)?;
        Ok(view)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspaceView {
    schema: String,
    schema_version: u16,
    id: WorkspaceViewId,
    repo_id: RepoId,
    backend: WorkspaceBackend,
    base_revision: String,
    write_owner: Option<AgentId>,
    state: WorkspaceState,
    access: ViewAccess,
    generation: u64,
    scope: Vec<RepoPath>,
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("workspace view operation cancelled"),
            Self::ViewNotFound { .. } => f.write_str("workspace view not found"),
            Self::WriteOwnerConflict { .. } => {
                f.write_str("workspace view already has a write owner")
            }
            Self::ReadOnlyView { .. } => f.write_str("workspace view is read-only"),
            Self::ReadOnlyViewer { .. } => {
                f.write_str("read-only viewer cannot become a write owner")
            }
            Self::NotWriteOwner { .. } => f.write_str("agent is not the write owner"),
            Self::InvalidState { .. } => f.write_str("workspace view is not in a valid state"),
            Self::InvalidBaseRevision => f.write_str("workspace view base revision is invalid"),
            Self::InvalidAccess => {
                f.write_str("read-only view cannot be created with a write owner")
            }
            Self::InvalidScope => f.write_str("workspace view scope is invalid"),
            Self::UnknownVariant => f.write_str("unknown workspace view enumeration value"),
            Self::TooManyViews { .. } => f.write_str("workspace view limit reached"),
            Self::LockPoisoned => f.write_str("workspace view registry lock poisoned"),
        }
    }
}

impl Error for WorkspaceError {}

fn parse_base_revision(raw: &str) -> Result<String, WorkspaceError> {
    if raw.is_empty() || raw.len() > MAX_BASE_REVISION_BYTES {
        return Err(WorkspaceError::InvalidBaseRevision);
    }
    if raw.contains('\0')
        || raw.chars().any(char::is_control)
        || raw.chars().any(char::is_whitespace)
    {
        return Err(WorkspaceError::InvalidBaseRevision);
    }
    Ok(raw.to_owned())
}

fn validate_view_invariants(view: &WorkspaceView) -> Result<(), WorkspaceError> {
    if view.write_owner.is_some() && !view.access.is_writable() {
        return Err(WorkspaceError::InvalidAccess);
    }
    if view.write_owner.is_some() && view.state != WorkspaceState::Active {
        return Err(WorkspaceError::InvalidState {
            view_id: view.id,
            state: view.state,
        });
    }
    if view.generation == 0 {
        return Err(WorkspaceError::InvalidState {
            view_id: view.id,
            state: view.state,
        });
    }
    if view.scope.prefixes.len() > MAX_VIEW_SCOPE_PREFIXES {
        return Err(WorkspaceError::InvalidScope);
    }
    Ok(())
}

fn bump_generation(view: &mut WorkspaceView) {
    view.generation = view.generation.saturating_add(1);
}

fn parse_closed<T: Copy>(
    raw: &str,
    all: &[T],
    as_str: fn(T) -> &'static str,
) -> Result<T, WorkspaceError> {
    for item in all {
        if as_str(*item) == raw {
            return Ok(*item);
        }
    }
    Err(WorkspaceError::UnknownVariant)
}

fn deserialize_closed<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: FromStr<Err = WorkspaceError>,
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    raw.parse()
        .map_err(|_| de::Error::unknown_variant(&raw, &[]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::thread;

    const GOLDEN_VIEW: &str = r#"{"schema":"rapidlm.workspace_view","schema_version":2,"id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","repo_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ac","backend":"git_worktree","base_revision":"deadbeef","write_owner":null,"state":"active","access":"read_only","generation":1,"scope":[]}"#;
    const GOLDEN_OWNED: &str = r#"{"schema":"rapidlm.workspace_view","schema_version":2,"id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","repo_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ac","backend":"direct","base_revision":"abc123","write_owner":"018f3c8a-7e2b-7a10-8c4d-0123456789ad","state":"active","access":"read_write","generation":1,"scope":["src"]}"#;

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn create_rw(registry: &ViewRegistry) -> WorkspaceView {
        registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::GitWorktree,
                    "base-rev",
                    ViewAccess::ReadWrite,
                ),
                &cancel(),
            )
            .expect("create write-capable view")
    }

    fn create_ro(registry: &ViewRegistry) -> WorkspaceView {
        registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::Overlay,
                    "base-rev",
                    ViewAccess::ReadOnly,
                ),
                &cancel(),
            )
            .expect("create read-only view")
    }

    #[test]
    fn create_records_backend_revision_and_empty_owner() {
        let registry = ViewRegistry::new();
        let view = create_rw(&registry);
        assert_eq!(view.backend(), WorkspaceBackend::GitWorktree);
        assert_eq!(view.base_revision(), "base-rev");
        assert_eq!(view.write_owner(), None);
        assert_eq!(view.state(), WorkspaceState::Active);
        assert_eq!(view.access(), ViewAccess::ReadWrite);
        assert_eq!(view.generation(), 1);
        assert!(view.scope().is_repo_wide());
        let loaded = registry.get(view.id(), &cancel()).expect("get");
        assert_eq!(loaded, view);
    }

    #[test]
    fn create_with_write_owner_is_atomic() {
        let registry = ViewRegistry::new();
        let agent = AgentId::new();
        let view = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::Direct,
                    "rev",
                    ViewAccess::ReadWrite,
                )
                .with_write_owner(agent),
                &cancel(),
            )
            .expect("create owned");
        assert_eq!(view.write_owner(), Some(agent));
        let other = AgentId::new();
        assert!(matches!(
            registry.acquire_write(view.id(), other, &cancel()),
            Err(WorkspaceError::WriteOwnerConflict { current, .. }) if current == agent
        ));
    }

    #[test]
    fn two_write_agents_cannot_own_same_view() {
        let registry = ViewRegistry::new();
        let view = create_rw(&registry);
        let first = AgentId::new();
        let second = AgentId::new();
        let owned = registry
            .acquire_write(view.id(), first, &cancel())
            .expect("first writer");
        assert_eq!(owned.write_owner(), Some(first));
        assert!(matches!(
            registry.acquire_write(view.id(), second, &cancel()),
            Err(WorkspaceError::WriteOwnerConflict { current, .. }) if current == first
        ));
        assert_eq!(
            registry
                .get(view.id(), &cancel())
                .expect("get")
                .write_owner(),
            Some(first)
        );
    }

    #[test]
    fn concurrent_write_acquisition_admits_exactly_one_owner() {
        let registry = Arc::new(ViewRegistry::new());
        for _ in 0..32 {
            let view = create_rw(&registry);
            let first = AgentId::new();
            let second = AgentId::new();
            let barrier = Arc::new(Barrier::new(2));
            let spawn = |agent: AgentId| {
                let registry = Arc::clone(&registry);
                let barrier = Arc::clone(&barrier);
                let view_id = view.id();
                thread::spawn(move || {
                    barrier.wait();
                    registry.acquire_write(view_id, agent, &CancellationToken::new())
                })
            };
            let left = spawn(first);
            let right = spawn(second);
            let left = left.join().expect("left thread");
            let right = right.join().expect("right thread");
            let wins = u8::from(left.is_ok()) + u8::from(right.is_ok());
            assert_eq!(wins, 1, "exactly one writer must win");
            let owner = registry
                .get(view.id(), &cancel())
                .expect("get")
                .write_owner()
                .expect("owner");
            assert!(owner == first || owner == second);
            if let Some(err) = left.as_ref().err().or(right.as_ref().err()) {
                assert!(matches!(err, WorkspaceError::WriteOwnerConflict { .. }));
            }
        }
    }

    #[test]
    fn read_only_view_cannot_gain_a_write_owner() {
        let registry = ViewRegistry::new();
        let view = create_ro(&registry);
        let agent = AgentId::new();
        assert!(matches!(
            registry.acquire_write(view.id(), agent, &cancel()),
            Err(WorkspaceError::ReadOnlyView { view_id }) if view_id == view.id()
        ));
        assert_eq!(
            registry
                .get(view.id(), &cancel())
                .expect("get")
                .write_owner(),
            None
        );
    }

    #[test]
    fn read_only_viewers_do_not_become_write_owners() {
        let registry = ViewRegistry::new();
        let view = create_rw(&registry);
        let reader = AgentId::new();
        registry
            .attach_reader(view.id(), reader, &cancel())
            .expect("attach reader");
        assert!(matches!(
            registry.acquire_write(view.id(), reader, &cancel()),
            Err(WorkspaceError::ReadOnlyViewer { agent_id, .. }) if agent_id == reader
        ));
        let writer = AgentId::new();
        let owned = registry
            .acquire_write(view.id(), writer, &cancel())
            .expect("unrelated writer");
        assert_eq!(owned.write_owner(), Some(writer));
        assert!(!owned.is_write_owner(reader));
    }

    #[test]
    fn create_rejects_write_owner_on_read_only_view() {
        let registry = ViewRegistry::new();
        let err = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::Remote,
                    "rev",
                    ViewAccess::ReadOnly,
                )
                .with_write_owner(AgentId::new()),
                &cancel(),
            )
            .expect_err("read-only + owner");
        assert_eq!(err, WorkspaceError::InvalidAccess);
    }

    #[test]
    fn release_allows_a_later_writer() {
        let registry = ViewRegistry::new();
        let view = create_rw(&registry);
        let first = AgentId::new();
        let second = AgentId::new();
        registry
            .acquire_write(view.id(), first, &cancel())
            .expect("first");
        let released = registry
            .release_write(view.id(), first, &cancel())
            .expect("release");
        assert_eq!(released.write_owner(), None);
        let owned = registry
            .acquire_write(view.id(), second, &cancel())
            .expect("second");
        assert_eq!(owned.write_owner(), Some(second));
    }

    #[test]
    fn non_owner_cannot_release_or_quiesce() {
        let registry = ViewRegistry::new();
        let view = create_rw(&registry);
        let owner = AgentId::new();
        registry
            .acquire_write(view.id(), owner, &cancel())
            .expect("owner");
        let other = AgentId::new();
        assert!(matches!(
            registry.release_write(view.id(), other, &cancel()),
            Err(WorkspaceError::NotWriteOwner { agent_id, .. }) if agent_id == other
        ));
        assert!(matches!(
            registry.quiesce(view.id(), &cancel()),
            Err(WorkspaceError::WriteOwnerConflict { current, .. }) if current == owner
        ));
    }

    #[test]
    fn base_revision_is_immutable_across_lifecycle() {
        let registry = ViewRegistry::new();
        let view = create_rw(&registry);
        let original = view.base_revision().to_owned();
        let agent = AgentId::new();
        registry
            .acquire_write(view.id(), agent, &cancel())
            .expect("acquire");
        registry
            .release_write(view.id(), agent, &cancel())
            .expect("release");
        let quiescent = registry.quiesce(view.id(), &cancel()).expect("quiesce");
        let closed = registry.close(view.id(), &cancel()).expect("close");
        assert_eq!(quiescent.base_revision(), original);
        assert_eq!(closed.base_revision(), original);
        assert_eq!(
            registry
                .get(view.id(), &cancel())
                .expect("get")
                .base_revision(),
            original
        );
    }

    #[test]
    fn cannot_acquire_write_after_quiesce_or_close() {
        let registry = ViewRegistry::new();
        let view = create_rw(&registry);
        registry.quiesce(view.id(), &cancel()).expect("quiesce");
        assert!(matches!(
            registry.acquire_write(view.id(), AgentId::new(), &cancel()),
            Err(WorkspaceError::InvalidState {
                state: WorkspaceState::Quiescent,
                ..
            })
        ));
        registry.close(view.id(), &cancel()).expect("close");
        assert!(matches!(
            registry.acquire_write(view.id(), AgentId::new(), &cancel()),
            Err(WorkspaceError::InvalidState {
                state: WorkspaceState::Closed,
                ..
            })
        ));
    }

    #[test]
    fn cancelled_operations_fail_closed() {
        let registry = ViewRegistry::new();
        let token = CancellationToken::new();
        token.cancel();
        let err = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::Direct,
                    "rev",
                    ViewAccess::ReadWrite,
                ),
                &token,
            )
            .expect_err("cancelled create");
        assert_eq!(err, WorkspaceError::Cancelled);
    }

    #[test]
    fn invalid_base_revisions_are_rejected() {
        let registry = ViewRegistry::new();
        for sample in [
            "",
            "has space",
            "rev\0x",
            "rev\n",
            &"a".repeat(MAX_BASE_REVISION_BYTES + 1),
        ] {
            let err = registry
                .create(
                    CreateView::new(
                        RepoId::new(),
                        WorkspaceBackend::Direct,
                        sample,
                        ViewAccess::ReadWrite,
                    ),
                    &cancel(),
                )
                .expect_err("invalid revision");
            assert_eq!(err, WorkspaceError::InvalidBaseRevision, "{sample:?}");
            if !sample.is_empty() {
                assert!(
                    !err.to_string().contains(sample),
                    "error echoed revision {sample:?}"
                );
            }
        }
    }

    #[test]
    fn view_limit_is_enforced() {
        let registry = ViewRegistry::with_limit(1);
        create_rw(&registry);
        let err = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::Overlay,
                    "rev",
                    ViewAccess::ReadOnly,
                ),
                &cancel(),
            )
            .expect_err("over limit");
        assert_eq!(err, WorkspaceError::TooManyViews { limit: 1 });
    }

    #[test]
    fn errors_do_not_echo_revision_or_secret_like_text() {
        let err = WorkspaceError::InvalidBaseRevision;
        let text = err.to_string();
        for leaked in ["password", "hunter2", "Authorization", "secret"] {
            assert!(!text.contains(leaked), "{text}");
        }
    }

    #[test]
    fn golden_json_round_trips() {
        let decoded: WorkspaceView = serde_json::from_str(GOLDEN_VIEW).expect("decode");
        assert_eq!(decoded.backend(), WorkspaceBackend::GitWorktree);
        assert_eq!(decoded.base_revision(), "deadbeef");
        assert_eq!(decoded.write_owner(), None);
        assert_eq!(decoded.state(), WorkspaceState::Active);
        assert_eq!(decoded.access(), ViewAccess::ReadOnly);
        assert_eq!(decoded.generation(), 1);
        assert!(decoded.scope().is_repo_wide());
        assert_eq!(
            serde_json::to_string(&decoded).expect("encode"),
            GOLDEN_VIEW
        );

        let owned: WorkspaceView = serde_json::from_str(GOLDEN_OWNED).expect("decode owned");
        assert_eq!(owned.backend(), WorkspaceBackend::Direct);
        assert!(owned.write_owner().is_some());
        assert_eq!(owned.access(), ViewAccess::ReadWrite);
        assert_eq!(owned.scope().prefixes_value().len(), 1);
        assert_eq!(owned.scope().prefixes_value()[0].as_str(), "src");
        assert_eq!(
            serde_json::to_string(&owned).expect("encode owned"),
            GOLDEN_OWNED
        );
    }

    #[test]
    fn deserialize_rejects_unknown_fields_and_escalation() {
        assert!(serde_json::from_str::<WorkspaceView>(r#"{"schema":"rapidlm.workspace_view","schema_version":2,"id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","repo_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ac","backend":"git_worktree","base_revision":"deadbeef","write_owner":null,"state":"active","access":"read_only","generation":1,"scope":[],"extra":true}"#).is_err());
        assert!(serde_json::from_str::<WorkspaceView>(r#"{"schema":"rapidlm.workspace_view","schema_version":2,"id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","repo_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ac","backend":"direct","base_revision":"abc123","write_owner":"018f3c8a-7e2b-7a10-8c4d-0123456789ad","state":"active","access":"read_only","generation":1,"scope":[]}"#).is_err());
        assert!(
            serde_json::from_str::<WorkspaceView>(
                GOLDEN_VIEW
                    .replace("schema_version\":2", "schema_version\":1")
                    .as_str()
            )
            .is_err()
        );
        assert!(serde_json::from_str::<WorkspaceBackend>("\"GIT_WORKTREE\"").is_err());
        assert!(serde_json::from_str::<WorkspaceBackend>("\"git-worktree\"").is_err());
        assert!(serde_json::from_str::<ViewAccess>("\"rw\"").is_err());
    }

    #[test]
    fn acquire_is_idempotent_for_current_owner() {
        let registry = ViewRegistry::new();
        let view = create_rw(&registry);
        let agent = AgentId::new();
        registry
            .acquire_write(view.id(), agent, &cancel())
            .expect("first");
        let again = registry
            .acquire_write(view.id(), agent, &cancel())
            .expect("again");
        assert_eq!(again.write_owner(), Some(agent));
        assert_eq!(again.generation(), 2);
    }

    #[test]
    fn generation_bumps_on_write_lifecycle_not_idempotent_reacquire() {
        let registry = ViewRegistry::new();
        let view = create_rw(&registry);
        assert_eq!(view.generation(), 1);
        let agent = AgentId::new();
        let owned = registry
            .acquire_write(view.id(), agent, &cancel())
            .expect("acquire");
        assert_eq!(owned.generation(), 2);
        let again = registry
            .acquire_write(view.id(), agent, &cancel())
            .expect("idempotent");
        assert_eq!(again.generation(), 2);
        let released = registry
            .release_write(view.id(), agent, &cancel())
            .expect("release");
        assert_eq!(released.generation(), 3);
        let quiesced = registry.quiesce(view.id(), &cancel()).expect("quiesce");
        assert_eq!(quiesced.generation(), 4);
    }

    #[test]
    fn view_scope_contains_prefixes_not_siblings() {
        let scope = ViewScope::prefixes(vec![
            RepoPath::parse("src").expect("src"),
            RepoPath::parse("crates/workspace").expect("ws"),
        ])
        .expect("scope");
        assert!(scope.contains(&RepoPath::parse("src/lib.rs").expect("lib")));
        assert!(scope.contains(&RepoPath::parse("src").expect("src eq")));
        assert!(!scope.contains(&RepoPath::parse("srcfoo/lib.rs").expect("sib")));
        assert!(scope.contains(&RepoPath::parse("crates/workspace/src/view.rs").expect("view")));
        assert!(!scope.contains(&RepoPath::parse("crates/kernel/src/lib.rs").expect("other")));
        assert!(ViewScope::repo().contains(&RepoPath::parse("anywhere.rs").expect("any")));
    }

    #[test]
    fn create_records_explicit_scope() {
        let registry = ViewRegistry::new();
        let scope =
            ViewScope::prefixes(vec![RepoPath::parse("apps").expect("apps")]).expect("scope");
        let view = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::Direct,
                    "rev",
                    ViewAccess::ReadWrite,
                )
                .with_scope(scope.clone()),
                &cancel(),
            )
            .expect("create scoped");
        assert_eq!(view.scope(), &scope);
        assert!(!view.scope().is_repo_wide());
    }
}
