//! Session lifecycle command flows: resume, fork, rewind, and compact.
//!
//! [`SessionActionsViewModel`] is a frontend projection. It never forks,
//! rewinds, resumes, or compacts. Apply returns a typed kernel intent. Fork
//! previews never claim an active goal is copied. Rewind apply is refused when
//! workspace conflicts would silently destroy newer user data.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use agent_runtime::CompactionArtifact;
use kernel::{
    GoalSnapshot, GoalState, GoalStopReason, RewindResult, SessionSnapshot, SessionStatus,
};
use protocol::{GoalId, SessionId, WorkspaceViewId};
use workspace::{RewindConflict, RewindOp, RewindOpKind, RewindPreview};

use crate::commands::{KernelAction, UiCommand};
use crate::sanitize::sanitize_untrusted;
use crate::state::{AppState, CancellationToken};

/// Render width is clamped to this many columns.
pub const MAX_SESSION_ACTION_COLS: u16 = 512;

/// Render height is clamped to this many rows.
pub const MAX_SESSION_ACTION_ROWS: u16 = 256;

/// Maximum workspace ops retained in one rewind preview.
pub const MAX_REWIND_OPS: usize = 256;

/// Maximum workspace conflicts retained in one rewind preview.
pub const MAX_REWIND_CONFLICTS: usize = 256;

/// Maximum UTF-8 bytes retained for one goal statement preview.
pub const MAX_GOAL_PREVIEW_BYTES: usize = 128;

const CANCEL_STRIDE: usize = 8;
const MAX_GOAL_PREVIEW_CHARS: usize = 48;
const NEW_SESSION_LABEL: &str = "new-session";

/// Slash-command lifecycle kind. Not a kernel mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SessionLifecycleKind {
    Resume,
    Fork,
    Rewind,
    Compact,
}

/// Typed request derived from [`UiCommand`] / [`KernelAction`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SessionLifecycleRequest {
    Resume { session: Option<SessionId> },
    Fork,
    Rewind { to_seq: Option<u64> },
    Compact,
}

/// Typed planner failure. Display never echoes IDs, paths, or goal text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SessionActionError {
    Cancelled,
    BoundExceeded,
    MissingSession,
    MissingTarget,
    InvalidSeq,
    ClosedSession,
    ActionsBlocked,
    RewindConflict,
    GoalUnsafe,
}

/// Kernel-bound resume. The view does not attach or unpark work.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ResumeSessionIntent {
    session_id: SessionId,
}

/// Kernel-bound fork. The view does not create a child stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ForkSessionIntent {
    source: SessionId,
    at_seq: u64,
}

/// Kernel-bound rewind. The view does not restore files or truncate history.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct RewindSessionIntent {
    session_id: SessionId,
    to_seq: u64,
}

/// Kernel-bound compact. The view does not write a continuation artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct CompactSessionIntent {
    session_id: SessionId,
}

/// Side-effecting intent that must enter [`kernel::KernelClient`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SessionLifecycleIntent {
    Resume(ResumeSessionIntent),
    Fork(ForkSessionIntent),
    Rewind(RewindSessionIntent),
    Compact(CompactSessionIntent),
}

/// Goal fields shown before apply. Not a capability grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalSafetyView {
    id: GoalId,
    state: GoalState,
    stop_reason: Option<GoalStopReason>,
    statement: String,
    max_turns: Option<u64>,
    max_tokens: Option<u64>,
}

/// One projected workspace rewind op. Paths are sanitized.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindOpView {
    path: String,
    kind: RewindOpKind,
}

/// Newer external user modification that apply would destroy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindConflictView {
    path: String,
}

/// Resulting view/session/goal behavior before destructive apply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionLifecyclePreview {
    kind: SessionLifecycleKind,
    source_session: SessionId,
    resulting_session: Option<SessionId>,
    source_seq: u64,
    resulting_seq: u64,
    source_status: SessionStatus,
    resulting_status: SessionStatus,
    source_goal: Option<GoalSafetyView>,
    resulting_goal: Option<GoalSafetyView>,
    copies_active_goal: bool,
    auto_continues: bool,
    workspace_view_id: Option<WorkspaceViewId>,
    workspace_ops: usize,
    workspace_conflicts: usize,
    dropped_events: u64,
    compact_preserves_goal: bool,
    compact_preserves_budget: bool,
    compact_blockers: usize,
    compact_files: usize,
    can_apply: bool,
}

/// Kernel/workspace observations the planner projects. Not a store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionLifecycleObservation {
    current: SessionSnapshot,
    target: Option<SessionSnapshot>,
    rewind: Option<RewindObservation>,
    compact: Option<CompactObservation>,
    recovered: bool,
    actions_blocked: bool,
}

/// Historical session projection plus optional workspace rewind preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindObservation {
    snapshot: SessionSnapshot,
    through_seq: u64,
    current_seq: u64,
    workspace: Option<RewindPreview>,
}

/// Structured compact observation. Goal/budget come from the artifact only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactObservation {
    session_id: SessionId,
    from_seq: u64,
    to_seq: u64,
    goal_id: Option<GoalId>,
    goal_state: Option<GoalState>,
    max_turns: Option<u64>,
    max_tokens: Option<u64>,
    blocker_count: usize,
    file_count: usize,
}

/// Frontend-only projection. Never mutates kernel or workspace state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionActionsViewModel {
    request: SessionLifecycleRequest,
    preview: SessionLifecyclePreview,
    ops: Vec<RewindOpView>,
    conflicts: Vec<RewindConflictView>,
}

/// One painted frame. `golden` omits trailing pad.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionActionFrame {
    width: u16,
    height: u16,
    lines: Vec<String>,
}

impl SessionLifecycleKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Fork => "fork",
            Self::Rewind => "rewind",
            Self::Compact => "compact",
        }
    }
}

impl SessionLifecycleRequest {
    pub const fn kind(self) -> SessionLifecycleKind {
        match self {
            Self::Resume { .. } => SessionLifecycleKind::Resume,
            Self::Fork => SessionLifecycleKind::Fork,
            Self::Rewind { .. } => SessionLifecycleKind::Rewind,
            Self::Compact => SessionLifecycleKind::Compact,
        }
    }

    /// Map a parsed slash command. Unknown commands are `None`.
    pub const fn from_command(command: &UiCommand) -> Option<Self> {
        match command {
            UiCommand::Resume { session } => Some(Self::Resume { session: *session }),
            UiCommand::Fork => Some(Self::Fork),
            UiCommand::Rewind { to_seq } => Some(Self::Rewind { to_seq: *to_seq }),
            UiCommand::Compact => Some(Self::Compact),
            _ => None,
        }
    }

    /// Map a kernel-bound action. Other actions are `None`.
    pub const fn from_action(action: &KernelAction) -> Option<Self> {
        match action {
            KernelAction::ResumeSession { session } => Some(Self::Resume { session: *session }),
            KernelAction::ForkSession => Some(Self::Fork),
            KernelAction::RewindSession { to_seq } => Some(Self::Rewind { to_seq: *to_seq }),
            KernelAction::CompactSession => Some(Self::Compact),
            _ => None,
        }
    }
}

impl SessionActionError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "session action cancelled",
            Self::BoundExceeded => "session action resource bound exceeded",
            Self::MissingSession => "session snapshot is required",
            Self::MissingTarget => "target session snapshot is required",
            Self::InvalidSeq => "session sequence is invalid",
            Self::ClosedSession => "closed session cannot be mutated",
            Self::ActionsBlocked => "session actions are blocked",
            Self::RewindConflict => "rewind refused because it would lose data",
            Self::GoalUnsafe => "session action would violate goal safety",
        }
    }
}

impl ResumeSessionIntent {
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }
}

impl ForkSessionIntent {
    pub const fn source(self) -> SessionId {
        self.source
    }

    pub const fn at_seq(self) -> u64 {
        self.at_seq
    }

    /// Fork never inherits an active top-level goal.
    pub const fn copies_active_goal(self) -> bool {
        false
    }
}

impl RewindSessionIntent {
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    pub const fn to_seq(self) -> u64 {
        self.to_seq
    }
}

impl CompactSessionIntent {
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }
}

impl SessionLifecycleIntent {
    pub const fn kind(self) -> SessionLifecycleKind {
        match self {
            Self::Resume(_) => SessionLifecycleKind::Resume,
            Self::Fork(_) => SessionLifecycleKind::Fork,
            Self::Rewind(_) => SessionLifecycleKind::Rewind,
            Self::Compact(_) => SessionLifecycleKind::Compact,
        }
    }

    pub fn kernel_action(self) -> KernelAction {
        match self {
            Self::Resume(intent) => KernelAction::ResumeSession {
                session: Some(intent.session_id),
            },
            Self::Fork(_) => KernelAction::ForkSession,
            Self::Rewind(intent) => KernelAction::RewindSession {
                to_seq: Some(intent.to_seq),
            },
            Self::Compact(_) => KernelAction::CompactSession,
        }
    }
}

impl GoalSafetyView {
    pub const fn id(&self) -> GoalId {
        self.id
    }

    pub const fn state(&self) -> GoalState {
        self.state
    }

    pub const fn stop_reason(&self) -> Option<GoalStopReason> {
        self.stop_reason
    }

    pub fn statement(&self) -> &str {
        &self.statement
    }

    pub const fn max_turns(&self) -> Option<u64> {
        self.max_turns
    }

    pub const fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    pub const fn is_active(&self) -> bool {
        matches!(self.state, GoalState::Active)
    }
}

impl RewindOpView {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub const fn kind(&self) -> RewindOpKind {
        self.kind
    }
}

impl RewindConflictView {
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl SessionLifecyclePreview {
    pub const fn kind(&self) -> SessionLifecycleKind {
        self.kind
    }

    pub const fn source_session(&self) -> SessionId {
        self.source_session
    }

    pub const fn resulting_session(&self) -> Option<SessionId> {
        self.resulting_session
    }

    pub const fn source_seq(&self) -> u64 {
        self.source_seq
    }

    pub const fn resulting_seq(&self) -> u64 {
        self.resulting_seq
    }

    pub const fn source_status(&self) -> SessionStatus {
        self.source_status
    }

    pub const fn resulting_status(&self) -> SessionStatus {
        self.resulting_status
    }

    pub fn source_goal(&self) -> Option<&GoalSafetyView> {
        self.source_goal.as_ref()
    }

    pub fn resulting_goal(&self) -> Option<&GoalSafetyView> {
        self.resulting_goal.as_ref()
    }

    /// Fork always returns false. Other kinds report whether the result stays active.
    pub const fn copies_active_goal(&self) -> bool {
        self.copies_active_goal
    }

    pub const fn auto_continues(&self) -> bool {
        self.auto_continues
    }

    pub const fn workspace_view_id(&self) -> Option<WorkspaceViewId> {
        self.workspace_view_id
    }

    pub const fn workspace_ops(&self) -> usize {
        self.workspace_ops
    }

    pub const fn workspace_conflicts(&self) -> usize {
        self.workspace_conflicts
    }

    pub const fn dropped_events(&self) -> u64 {
        self.dropped_events
    }

    pub const fn compact_preserves_goal(&self) -> bool {
        self.compact_preserves_goal
    }

    pub const fn compact_preserves_budget(&self) -> bool {
        self.compact_preserves_budget
    }

    pub const fn compact_blockers(&self) -> usize {
        self.compact_blockers
    }

    pub const fn compact_files(&self) -> usize {
        self.compact_files
    }

    pub const fn can_apply(&self) -> bool {
        self.can_apply
    }
}

impl SessionLifecycleObservation {
    /// Current session is required. Other fields are optional observations.
    pub fn new(current: SessionSnapshot) -> Self {
        Self {
            current,
            target: None,
            rewind: None,
            compact: None,
            recovered: false,
            actions_blocked: false,
        }
    }

    pub fn from_app_state(state: &AppState) -> Result<Self, SessionActionError> {
        let current = state
            .snapshot()
            .cloned()
            .ok_or(SessionActionError::MissingSession)?;
        Ok(Self {
            current,
            target: None,
            rewind: None,
            compact: None,
            recovered: false,
            actions_blocked: state.actions_blocked(),
        })
    }

    pub fn with_target(mut self, target: SessionSnapshot) -> Self {
        self.target = Some(target);
        self
    }

    pub fn with_rewind(mut self, rewind: RewindObservation) -> Self {
        self.rewind = Some(rewind);
        self
    }

    pub fn with_compact(mut self, compact: CompactObservation) -> Self {
        self.compact = Some(compact);
        self
    }

    /// Marks a crash/restart resume so an active goal is previewed as paused.
    pub fn with_recovered(mut self, recovered: bool) -> Self {
        self.recovered = recovered;
        self
    }

    pub fn with_actions_blocked(mut self, blocked: bool) -> Self {
        self.actions_blocked = blocked;
        self
    }

    pub fn current(&self) -> &SessionSnapshot {
        &self.current
    }

    pub fn target(&self) -> Option<&SessionSnapshot> {
        self.target.as_ref()
    }

    pub fn rewind(&self) -> Option<&RewindObservation> {
        self.rewind.as_ref()
    }

    pub fn compact(&self) -> Option<&CompactObservation> {
        self.compact.as_ref()
    }

    pub const fn recovered(&self) -> bool {
        self.recovered
    }

    pub const fn actions_blocked(&self) -> bool {
        self.actions_blocked
    }
}

impl RewindObservation {
    pub fn new(
        snapshot: SessionSnapshot,
        through_seq: u64,
        current_seq: u64,
    ) -> Result<Self, SessionActionError> {
        if through_seq == 0 || through_seq > current_seq || snapshot.seq() != through_seq {
            return Err(SessionActionError::InvalidSeq);
        }
        Ok(Self {
            snapshot,
            through_seq,
            current_seq,
            workspace: None,
        })
    }

    pub fn from_kernel(result: &RewindResult) -> Result<Self, SessionActionError> {
        Self::new(
            result.snapshot().clone(),
            result.through_seq(),
            result.current_seq(),
        )
    }

    pub fn with_workspace(mut self, preview: RewindPreview) -> Self {
        self.workspace = Some(preview);
        self
    }

    pub fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }

    pub const fn through_seq(&self) -> u64 {
        self.through_seq
    }

    pub const fn current_seq(&self) -> u64 {
        self.current_seq
    }

    pub fn workspace(&self) -> Option<&RewindPreview> {
        self.workspace.as_ref()
    }
}

impl CompactObservation {
    pub fn new(
        session_id: SessionId,
        from_seq: u64,
        to_seq: u64,
    ) -> Result<Self, SessionActionError> {
        if from_seq == 0 || from_seq > to_seq {
            return Err(SessionActionError::InvalidSeq);
        }
        Ok(Self {
            session_id,
            from_seq,
            to_seq,
            goal_id: None,
            goal_state: None,
            max_turns: None,
            max_tokens: None,
            blocker_count: 0,
            file_count: 0,
        })
    }

    pub fn from_artifact(artifact: &CompactionArtifact) -> Result<Self, SessionActionError> {
        let range = artifact.source_range();
        let mut observation = Self::new(artifact.session_id(), range.from_seq(), range.to_seq())?;
        if let Some(goal) = artifact.goal() {
            observation.goal_id = Some(goal.id());
            observation.goal_state = map_runtime_goal_state(goal.state());
            let budget = goal.budget();
            observation.max_turns = budget.max_turns();
            observation.max_tokens = budget.max_tokens();
        }
        observation.blocker_count = artifact.blockers().len();
        observation.file_count = artifact.files().len();
        Ok(observation)
    }

    pub fn with_goal(
        mut self,
        id: GoalId,
        state: GoalState,
        max_turns: Option<u64>,
        max_tokens: Option<u64>,
    ) -> Self {
        self.goal_id = Some(id);
        self.goal_state = Some(state);
        self.max_turns = max_turns;
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_counts(mut self, blockers: usize, files: usize) -> Self {
        self.blocker_count = blockers;
        self.file_count = files;
        self
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn from_seq(&self) -> u64 {
        self.from_seq
    }

    pub const fn to_seq(&self) -> u64 {
        self.to_seq
    }

    pub const fn goal_id(&self) -> Option<GoalId> {
        self.goal_id
    }

    pub const fn goal_state(&self) -> Option<GoalState> {
        self.goal_state
    }

    pub const fn blocker_count(&self) -> usize {
        self.blocker_count
    }

    pub const fn file_count(&self) -> usize {
        self.file_count
    }
}

impl SessionActionsViewModel {
    /// Project a lifecycle command. The view is not mutated and nothing is applied.
    pub fn plan(
        request: SessionLifecycleRequest,
        observation: &SessionLifecycleObservation,
        cancel: &CancellationToken,
    ) -> Result<Self, SessionActionError> {
        check_cancel(cancel)?;
        if observation.actions_blocked {
            return Err(SessionActionError::ActionsBlocked);
        }
        match request {
            SessionLifecycleRequest::Resume { session } => {
                plan_resume(session, observation, cancel)
            }
            SessionLifecycleRequest::Fork => plan_fork(observation, cancel),
            SessionLifecycleRequest::Rewind { to_seq } => plan_rewind(to_seq, observation, cancel),
            SessionLifecycleRequest::Compact => plan_compact(observation, cancel),
        }
    }

    pub fn from_command(
        command: &UiCommand,
        observation: &SessionLifecycleObservation,
        cancel: &CancellationToken,
    ) -> Result<Self, SessionActionError> {
        let request =
            SessionLifecycleRequest::from_command(command).ok_or(SessionActionError::GoalUnsafe)?;
        Self::plan(request, observation, cancel)
    }

    pub fn from_action(
        action: &KernelAction,
        observation: &SessionLifecycleObservation,
        cancel: &CancellationToken,
    ) -> Result<Self, SessionActionError> {
        let request =
            SessionLifecycleRequest::from_action(action).ok_or(SessionActionError::GoalUnsafe)?;
        Self::plan(request, observation, cancel)
    }

    pub const fn request(&self) -> SessionLifecycleRequest {
        self.request
    }

    pub fn preview(&self) -> &SessionLifecyclePreview {
        &self.preview
    }

    pub fn ops(&self) -> &[RewindOpView] {
        &self.ops
    }

    pub fn conflicts(&self) -> &[RewindConflictView] {
        &self.conflicts
    }

    /// This planner never persists session, workspace, or compaction state.
    pub const fn mutates_store(&self) -> bool {
        false
    }

    /// Request the kernel mutation. Conflicts and goal-unsafe plans refuse.
    pub fn apply(
        &self,
        cancel: &CancellationToken,
    ) -> Result<SessionLifecycleIntent, SessionActionError> {
        check_cancel(cancel)?;
        if !self.preview.can_apply {
            return Err(if self.preview.kind == SessionLifecycleKind::Rewind {
                SessionActionError::RewindConflict
            } else {
                SessionActionError::GoalUnsafe
            });
        }
        if !self.conflicts.is_empty() {
            return Err(SessionActionError::RewindConflict);
        }
        if self.preview.kind == SessionLifecycleKind::Fork && self.preview.copies_active_goal {
            return Err(SessionActionError::GoalUnsafe);
        }
        match self.request {
            SessionLifecycleRequest::Resume { session } => {
                Ok(SessionLifecycleIntent::Resume(ResumeSessionIntent {
                    session_id: session.unwrap_or(self.preview.source_session),
                }))
            }
            SessionLifecycleRequest::Fork => Ok(SessionLifecycleIntent::Fork(ForkSessionIntent {
                source: self.preview.source_session,
                at_seq: self.preview.source_seq,
            })),
            SessionLifecycleRequest::Rewind { to_seq } => {
                let to_seq = to_seq.unwrap_or(self.preview.resulting_seq);
                if to_seq == 0 {
                    return Err(SessionActionError::InvalidSeq);
                }
                Ok(SessionLifecycleIntent::Rewind(RewindSessionIntent {
                    session_id: self.preview.source_session,
                    to_seq,
                }))
            }
            SessionLifecycleRequest::Compact => {
                Ok(SessionLifecycleIntent::Compact(CompactSessionIntent {
                    session_id: self.preview.source_session,
                }))
            }
        }
    }

    pub fn render(&self, width: u16, height: u16) -> SessionActionFrame {
        let width = width.min(MAX_SESSION_ACTION_COLS);
        let height = height.min(MAX_SESSION_ACTION_ROWS);
        if width == 0 || height == 0 {
            return SessionActionFrame {
                width,
                height,
                lines: Vec::new(),
            };
        }
        let mut lines = Vec::new();
        lines.push(format!("{} preview", self.preview.kind.as_str()));
        lines.push(format_source_line(&self.preview));
        lines.push(format_result_line(&self.preview));
        lines.push(format_goal_line(&self.preview));
        match self.preview.kind {
            SessionLifecycleKind::Fork => {
                lines.push("workspace:pointer-only leases:none".to_owned());
            }
            SessionLifecycleKind::Resume => {
                let auto = if self.preview.auto_continues {
                    "yes"
                } else {
                    "no"
                };
                lines.push(format!("auto-continue:{auto}"));
            }
            SessionLifecycleKind::Rewind => {
                lines.push(format!(
                    "workspace:ops:{} conflicts:{} dropped:{}",
                    self.preview.workspace_ops,
                    self.preview.workspace_conflicts,
                    self.preview.dropped_events
                ));
                for conflict in &self.conflicts {
                    lines.push(format!("conflict:{} data-loss", conflict.path));
                }
                for op in &self.ops {
                    lines.push(format!("op:{} {}", op.kind.as_str(), op.path));
                }
            }
            SessionLifecycleKind::Compact => {
                let goal = if self.preview.compact_preserves_goal {
                    "preserved"
                } else {
                    "none"
                };
                let budget = if self.preview.compact_preserves_budget {
                    "preserved"
                } else {
                    "none"
                };
                lines.push(format!("goal:{goal} budget:{budget}"));
                lines.push(format!(
                    "blockers:{} files:{} leases:omitted",
                    self.preview.compact_blockers, self.preview.compact_files
                ));
            }
        }
        let apply = if self.preview.can_apply {
            "allowed"
        } else {
            "refused"
        };
        lines.push(format!("apply:{apply}"));
        if lines.len() > usize::from(height) {
            lines.truncate(usize::from(height));
        }
        SessionActionFrame {
            width,
            height,
            lines,
        }
    }
}

impl SessionActionFrame {
    pub const fn width(&self) -> u16 {
        self.width
    }

    pub const fn height(&self) -> u16 {
        self.height
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Stable dump used by golden tests. Trailing pad is omitted.
    pub fn golden(&self) -> String {
        self.lines.join("\n")
    }

    /// Exact-width rows written into the pane, padded/truncated to `height`.
    pub fn text(&self) -> String {
        let width = usize::from(self.width);
        let height = usize::from(self.height);
        let mut rows = Vec::with_capacity(height);
        for i in 0..height {
            let src = self.lines.get(i).map(String::as_str).unwrap_or("");
            rows.push(fit_width(src, width));
        }
        rows.join("\n")
    }
}

impl Display for SessionActionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for SessionActionError {}

impl Display for SessionLifecycleKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn plan_resume(
    requested: Option<SessionId>,
    observation: &SessionLifecycleObservation,
    cancel: &CancellationToken,
) -> Result<SessionActionsViewModel, SessionActionError> {
    check_cancel(cancel)?;
    let source = match requested {
        Some(id) if id != observation.current.id() => observation
            .target
            .as_ref()
            .filter(|target| target.id() == id)
            .ok_or(SessionActionError::MissingTarget)?,
        _ => &observation.current,
    };
    reject_closed(source)?;
    let source_goal = source.top_level_goal().map(project_goal);
    let recovered = observation.recovered
        || source.status() == SessionStatus::Recovering
        || source_goal
            .as_ref()
            .is_some_and(|goal| goal.stop_reason == Some(GoalStopReason::ProcessRecovered));
    let resulting_goal = source_goal.as_ref().map(|goal| {
        if recovered && goal.state == GoalState::Active {
            GoalSafetyView {
                state: GoalState::Paused,
                stop_reason: Some(GoalStopReason::ProcessRecovered),
                ..goal.clone()
            }
        } else {
            goal.clone()
        }
    });
    if resulting_goal
        .as_ref()
        .is_some_and(|goal| recovered && goal.state == GoalState::Active)
    {
        return Err(SessionActionError::GoalUnsafe);
    }
    let resulting_status = if recovered {
        SessionStatus::Paused
    } else {
        source.status()
    };
    let preview = SessionLifecyclePreview {
        kind: SessionLifecycleKind::Resume,
        source_session: source.id(),
        resulting_session: Some(source.id()),
        source_seq: source.seq(),
        resulting_seq: source.seq(),
        source_status: source.status(),
        resulting_status,
        source_goal,
        resulting_goal,
        copies_active_goal: false,
        auto_continues: false,
        workspace_view_id: None,
        workspace_ops: 0,
        workspace_conflicts: 0,
        dropped_events: 0,
        compact_preserves_goal: false,
        compact_preserves_budget: false,
        compact_blockers: 0,
        compact_files: 0,
        can_apply: true,
    };
    Ok(SessionActionsViewModel {
        request: SessionLifecycleRequest::Resume { session: requested },
        preview,
        ops: Vec::new(),
        conflicts: Vec::new(),
    })
}

fn plan_fork(
    observation: &SessionLifecycleObservation,
    cancel: &CancellationToken,
) -> Result<SessionActionsViewModel, SessionActionError> {
    check_cancel(cancel)?;
    let source = &observation.current;
    reject_closed(source)?;
    if source.seq() == 0 {
        return Err(SessionActionError::InvalidSeq);
    }
    let source_goal = source.top_level_goal().map(project_goal);
    let preview = SessionLifecyclePreview {
        kind: SessionLifecycleKind::Fork,
        source_session: source.id(),
        resulting_session: None,
        source_seq: source.seq(),
        resulting_seq: 1,
        source_status: source.status(),
        resulting_status: SessionStatus::Ready,
        source_goal,
        resulting_goal: None,
        copies_active_goal: false,
        auto_continues: false,
        workspace_view_id: None,
        workspace_ops: 0,
        workspace_conflicts: 0,
        dropped_events: 0,
        compact_preserves_goal: false,
        compact_preserves_budget: false,
        compact_blockers: 0,
        compact_files: 0,
        can_apply: true,
    };
    Ok(SessionActionsViewModel {
        request: SessionLifecycleRequest::Fork,
        preview,
        ops: Vec::new(),
        conflicts: Vec::new(),
    })
}

fn plan_rewind(
    requested_seq: Option<u64>,
    observation: &SessionLifecycleObservation,
    cancel: &CancellationToken,
) -> Result<SessionActionsViewModel, SessionActionError> {
    check_cancel(cancel)?;
    let source = &observation.current;
    reject_closed(source)?;
    let rewind = observation
        .rewind
        .as_ref()
        .ok_or(SessionActionError::MissingTarget)?;
    if rewind.snapshot.id() != source.id() {
        return Err(SessionActionError::MissingTarget);
    }
    let to_seq = requested_seq.unwrap_or(rewind.through_seq);
    if to_seq == 0 || to_seq != rewind.through_seq || to_seq > rewind.current_seq {
        return Err(SessionActionError::InvalidSeq);
    }
    let workspace = rewind.workspace.as_ref();
    let (ops, conflicts) = project_workspace(workspace, cancel)?;
    let conflict_count = conflicts.len();
    let can_apply = conflict_count == 0 && workspace.is_none_or(RewindPreview::is_safe);
    let source_goal = source.top_level_goal().map(project_goal);
    let resulting_goal = rewind.snapshot.top_level_goal().map(project_goal);
    let dropped = rewind.current_seq.saturating_sub(to_seq);
    let preview = SessionLifecyclePreview {
        kind: SessionLifecycleKind::Rewind,
        source_session: source.id(),
        resulting_session: Some(source.id()),
        source_seq: source.seq(),
        resulting_seq: to_seq,
        source_status: source.status(),
        resulting_status: rewind.snapshot.status(),
        source_goal,
        resulting_goal,
        copies_active_goal: false,
        auto_continues: false,
        workspace_view_id: workspace.map(RewindPreview::view_id),
        workspace_ops: ops.len(),
        workspace_conflicts: conflict_count,
        dropped_events: dropped,
        compact_preserves_goal: false,
        compact_preserves_budget: false,
        compact_blockers: 0,
        compact_files: 0,
        can_apply,
    };
    Ok(SessionActionsViewModel {
        request: SessionLifecycleRequest::Rewind {
            to_seq: requested_seq,
        },
        preview,
        ops,
        conflicts,
    })
}

fn plan_compact(
    observation: &SessionLifecycleObservation,
    cancel: &CancellationToken,
) -> Result<SessionActionsViewModel, SessionActionError> {
    check_cancel(cancel)?;
    let source = &observation.current;
    reject_closed(source)?;
    if source.seq() == 0 {
        return Err(SessionActionError::InvalidSeq);
    }
    let source_goal = source.top_level_goal().map(project_goal);
    let (resulting_goal, blockers, files, from_seq) = if let Some(compact) = &observation.compact {
        if compact.session_id != source.id() {
            return Err(SessionActionError::MissingTarget);
        }
        if compact.to_seq > source.seq() {
            return Err(SessionActionError::InvalidSeq);
        }
        if source_goal.is_some() && compact.goal_id.is_none() {
            return Err(SessionActionError::GoalUnsafe);
        }
        let resulting = match (source_goal.as_ref(), compact.goal_id, compact.goal_state) {
            (Some(source_goal), Some(id), Some(state)) => Some(GoalSafetyView {
                id,
                state,
                stop_reason: source_goal.stop_reason,
                statement: source_goal.statement.clone(),
                max_turns: compact.max_turns.or(source_goal.max_turns),
                max_tokens: compact.max_tokens.or(source_goal.max_tokens),
            }),
            (None, None, _) => None,
            (None, Some(id), Some(state)) => Some(GoalSafetyView {
                id,
                state,
                stop_reason: None,
                statement: String::new(),
                max_turns: compact.max_turns,
                max_tokens: compact.max_tokens,
            }),
            _ => return Err(SessionActionError::GoalUnsafe),
        };
        (
            resulting,
            compact.blocker_count,
            compact.file_count,
            compact.from_seq,
        )
    } else {
        (source_goal.clone(), 0, 0, 1)
    };
    let preserves_goal = source_goal.is_none() || resulting_goal.is_some();
    let preserves_budget = match (source_goal.as_ref(), resulting_goal.as_ref()) {
        (None, _) => true,
        (Some(source), Some(result)) => {
            result.max_turns == source.max_turns && result.max_tokens == source.max_tokens
        }
        (Some(_), None) => false,
    };
    let can_apply = preserves_goal && preserves_budget;
    let preview = SessionLifecyclePreview {
        kind: SessionLifecycleKind::Compact,
        source_session: source.id(),
        resulting_session: Some(source.id()),
        source_seq: source.seq(),
        resulting_seq: source.seq(),
        source_status: source.status(),
        resulting_status: source.status(),
        source_goal,
        resulting_goal,
        copies_active_goal: false,
        auto_continues: false,
        workspace_view_id: None,
        workspace_ops: 0,
        workspace_conflicts: 0,
        dropped_events: 0,
        compact_preserves_goal: preserves_goal,
        compact_preserves_budget: preserves_budget,
        compact_blockers: blockers,
        compact_files: files,
        can_apply,
    };
    let _ = from_seq;
    Ok(SessionActionsViewModel {
        request: SessionLifecycleRequest::Compact,
        preview,
        ops: Vec::new(),
        conflicts: Vec::new(),
    })
}

fn project_workspace(
    workspace: Option<&RewindPreview>,
    cancel: &CancellationToken,
) -> Result<(Vec<RewindOpView>, Vec<RewindConflictView>), SessionActionError> {
    check_cancel(cancel)?;
    let Some(preview) = workspace else {
        return Ok((Vec::new(), Vec::new()));
    };
    if preview.ops().len() > MAX_REWIND_OPS || preview.conflicts().len() > MAX_REWIND_CONFLICTS {
        return Err(SessionActionError::BoundExceeded);
    }
    let mut ops = Vec::with_capacity(preview.ops().len());
    for (index, op) in preview.ops().iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        ops.push(project_op(op));
    }
    let mut conflicts = Vec::with_capacity(preview.conflicts().len());
    for (index, conflict) in preview.conflicts().iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        conflicts.push(project_conflict(conflict));
    }
    Ok((ops, conflicts))
}

fn project_op(op: &RewindOp) -> RewindOpView {
    RewindOpView {
        path: sanitize_untrusted(op.path().as_str()).into_owned(),
        kind: op.kind(),
    }
}

fn project_conflict(conflict: &RewindConflict) -> RewindConflictView {
    RewindConflictView {
        path: sanitize_untrusted(conflict.path().as_str()).into_owned(),
    }
}

fn project_goal(goal: &GoalSnapshot) -> GoalSafetyView {
    let sanitized = sanitize_untrusted(goal.statement());
    let statement = preview_text(sanitized.as_ref());
    GoalSafetyView {
        id: goal.id(),
        state: goal.state(),
        stop_reason: goal.stop_reason(),
        statement,
        max_turns: goal.budget().max_turns(),
        max_tokens: goal.budget().max_tokens(),
    }
}

fn map_runtime_goal_state(state: agent_runtime::GoalState) -> Option<GoalState> {
    match state {
        agent_runtime::GoalState::Active => Some(GoalState::Active),
        agent_runtime::GoalState::Paused => Some(GoalState::Paused),
        agent_runtime::GoalState::Blocked => Some(GoalState::Blocked),
        _ => None,
    }
}

fn reject_closed(snapshot: &SessionSnapshot) -> Result<(), SessionActionError> {
    if snapshot.status() == SessionStatus::Closed {
        Err(SessionActionError::ClosedSession)
    } else {
        Ok(())
    }
}

fn format_source_line(preview: &SessionLifecyclePreview) -> String {
    format!(
        "source:{} seq:{} status:{}",
        preview.source_session,
        preview.source_seq,
        preview.source_status.as_str()
    )
}

fn format_result_line(preview: &SessionLifecyclePreview) -> String {
    let session = match preview.resulting_session {
        Some(id) => id.to_string(),
        None => NEW_SESSION_LABEL.to_owned(),
    };
    format!(
        "result:{} seq:{} status:{}",
        session,
        preview.resulting_seq,
        preview.resulting_status.as_str()
    )
}

fn format_goal_line(preview: &SessionLifecyclePreview) -> String {
    match preview.kind {
        SessionLifecycleKind::Fork => {
            let source = preview
                .source_goal
                .as_ref()
                .map(|goal| goal.state.as_str())
                .unwrap_or("none");
            format!("goal:none (not copied) source:{source}")
        }
        _ => {
            let source = preview
                .source_goal
                .as_ref()
                .map(|goal| goal.state.as_str())
                .unwrap_or("none");
            let result = preview
                .resulting_goal
                .as_ref()
                .map(|goal| goal.state.as_str())
                .unwrap_or("none");
            let extra = preview
                .resulting_goal
                .as_ref()
                .and_then(|goal| goal.stop_reason)
                .map(|reason| format!(" ({})", reason.as_str()))
                .unwrap_or_default();
            format!("goal:{source} -> {result}{extra}")
        }
    }
}

fn preview_text(text: &str) -> String {
    let mut out: String = text.chars().take(MAX_GOAL_PREVIEW_CHARS).collect();
    if out.len() > MAX_GOAL_PREVIEW_BYTES {
        // Find the last valid char boundary at or before the cap *before*
        // truncating: `String::truncate` itself panics on a non-boundary
        // index, so fixing the boundary after calling it (the previous
        // shape here) never actually ran — `MAX_GOAL_PREVIEW_CHARS` (48)
        // multi-byte characters can reach up to 192 bytes, comfortably over
        // `MAX_GOAL_PREVIEW_BYTES` (128).
        let mut end = MAX_GOAL_PREVIEW_BYTES;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out.retain(|c| c != '\n' && c != '\t');
    out
}

fn fit_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cols = text.chars().count();
    if cols == width {
        return text.to_owned();
    }
    if cols < width {
        let mut out = text.to_owned();
        out.extend(std::iter::repeat_n(' ', width - cols));
        return out;
    }
    text.chars().take(width).collect()
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), SessionActionError> {
    if cancel.is_cancelled() {
        Err(SessionActionError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_text_truncates_multibyte_chars_without_panicking() {
        // 48 (MAX_GOAL_PREVIEW_CHARS) CJK characters at 3 bytes each is 144
        // bytes, comfortably over MAX_GOAL_PREVIEW_BYTES (128) -- and 128
        // falls strictly between two of those characters' byte boundaries
        // (126 and 129), so a naive `truncate(128)` panics.
        let text: String = std::iter::repeat_n('字', 48).collect();
        let preview = preview_text(&text);
        assert!(preview.len() <= MAX_GOAL_PREVIEW_BYTES, "{}", preview.len());
        assert!(!preview.is_empty());
    }
    use crate::commands::{dispatch, parse_command};
    use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, EventKind, RecordedAt};
    use kernel::{apply, replay};
    use protocol::{EventId, RedactionClass, TraceId, WorkspaceViewId};
    use serde_json::{Value, json};

    const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000010";
    const PROJECT_ID: &str = "019c0000-0000-7000-8000-000000000011";
    const GOAL_ID: &str = "019c0000-0000-7000-8000-000000000014";
    const VIEW_ID: &str = "019c0000-0000-7000-8000-000000000021";
    const ACTOR_ID: &str = "019c0000-0000-7000-8000-000000000016";
    const TRACE_ID: &str = "8f000000-0000-7000-8000-000000000017";
    const CREATED_AT: &str = "2026-08-14T15:20:04.123Z";
    const UPDATED_AT: &str = "2026-08-14T15:21:00.000Z";
    const ARTIFACT: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    const GOLDEN_FORK: &str = "\
fork preview
source:019c0000-0000-7000-8000-000000000010 seq:2 status:ready
result:new-session seq:1 status:ready
goal:none (not copied) source:active
workspace:pointer-only leases:none
apply:allowed";

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn envelope(
        seq: u64,
        kind: EventKind,
        at: &str,
        payload: Value,
    ) -> event_ledger::event::ErasedEventEnvelope {
        EventEnvelope::new(
            event_id_for_seq(seq),
            SESSION_ID.parse().expect("session"),
            seq,
            at.parse::<RecordedAt>().expect("recorded_at"),
            ActorRef::new(ActorKind::System, ACTOR_ID).expect("actor"),
            TRACE_ID.parse::<TraceId>().expect("trace"),
            kind,
            RedactionClass::Project,
            payload,
        )
    }

    fn event_id_for_seq(seq: u64) -> EventId {
        format!("019c0000-0000-7000-8000-{seq:012x}")
            .parse()
            .expect("event id")
    }

    fn created() -> event_ledger::event::ErasedEventEnvelope {
        envelope(
            1,
            EventKind::SessionCreated,
            CREATED_AT,
            json!({"project_id": PROJECT_ID}),
        )
    }

    fn active_goal_snapshot() -> SessionSnapshot {
        let events = [
            created(),
            envelope(
                2,
                EventKind::GoalCreated,
                UPDATED_AT,
                json!({
                    "goal_id": GOAL_ID,
                    "statement": "ship the projection",
                    "budget": {"max_turns": 8, "max_tokens": 1000}
                }),
            ),
        ];
        replay(&events, &kernel::CancellationToken::new()).expect("replay")
    }

    fn paused_after_goal() -> SessionSnapshot {
        let events = [
            created(),
            envelope(
                2,
                EventKind::GoalCreated,
                UPDATED_AT,
                json!({
                    "goal_id": GOAL_ID,
                    "statement": "ship the projection",
                    "budget": {"max_turns": 8, "max_tokens": 1000}
                }),
            ),
            envelope(
                3,
                EventKind::GoalPaused,
                UPDATED_AT,
                json!({"goal_id": GOAL_ID}),
            ),
        ];
        replay(&events, &kernel::CancellationToken::new()).expect("replay")
    }

    fn created_only() -> SessionSnapshot {
        apply(None, &created()).expect("created")
    }

    fn rewind_preview(conflicts: bool) -> RewindPreview {
        let view: WorkspaceViewId = VIEW_ID.parse().expect("view");
        let mut value = json!({
            "schema": "rapidlm.rewind_preview",
            "schema_version": 1,
            "checkpoint_id": 1,
            "view_id": view.to_string(),
            "mode": "preview",
            "applied": false,
            "ops": [{
                "path": "src/main.rs",
                "kind": "restore",
                "before": ARTIFACT,
                "after": ARTIFACT
            }],
            "conflicts": []
        });
        if conflicts {
            value["ops"] = json!([]);
            value["conflicts"] = json!([{
                "path": "src/main.rs",
                "current": ARTIFACT,
                "expected": ARTIFACT,
                "target": ARTIFACT
            }]);
        }
        serde_json::from_value(value).expect("rewind preview")
    }

    #[test]
    fn fork_never_claims_active_goal_copied() {
        let snapshot = active_goal_snapshot();
        assert_eq!(
            snapshot.top_level_goal().expect("goal").state(),
            GoalState::Active
        );
        let observation = SessionLifecycleObservation::new(snapshot);
        let model = SessionActionsViewModel::from_command(
            &parse_command("/fork").expect("parse"),
            &observation,
            &cancel(),
        )
        .expect("plan");
        let preview = model.preview();
        assert_eq!(preview.kind(), SessionLifecycleKind::Fork);
        assert!(!preview.copies_active_goal());
        assert!(preview.resulting_goal().is_none());
        assert!(preview.resulting_session().is_none());
        assert_eq!(preview.resulting_status(), SessionStatus::Ready);
        assert_eq!(preview.resulting_seq(), 1);
        assert!(!preview.auto_continues());
        let intent = model.apply(&cancel()).expect("apply");
        match intent {
            SessionLifecycleIntent::Fork(fork) => {
                assert!(!fork.copies_active_goal());
                assert_eq!(fork.at_seq(), 2);
            }
            other => panic!("expected fork intent, got {other:?}"),
        }
        assert_eq!(intent.kernel_action(), KernelAction::ForkSession);
        assert!(!model.mutates_store());
        let golden = model.render(80, 24).golden();
        assert_eq!(golden, GOLDEN_FORK);
        assert!(golden.contains("goal:none (not copied)"));
        assert!(!golden.contains("goal:active -> active"));
        assert!(!golden.contains("goal copied"));
    }

    #[test]
    fn rewind_conflict_refuses_silent_data_loss() {
        let current = paused_after_goal();
        let historical = created_only();
        let rewind = RewindObservation::new(historical, 1, current.seq())
            .expect("rewind")
            .with_workspace(rewind_preview(true));
        let observation = SessionLifecycleObservation::new(current).with_rewind(rewind);
        let model = SessionActionsViewModel::plan(
            SessionLifecycleRequest::Rewind { to_seq: Some(1) },
            &observation,
            &cancel(),
        )
        .expect("preview");
        let preview = model.preview();
        assert_eq!(preview.kind(), SessionLifecycleKind::Rewind);
        assert!(!preview.can_apply());
        assert_eq!(preview.workspace_conflicts(), 1);
        assert_eq!(preview.dropped_events(), 2);
        assert!(preview.resulting_goal().is_none());
        assert_eq!(preview.resulting_status(), SessionStatus::Ready);
        assert_eq!(
            model.apply(&cancel()),
            Err(SessionActionError::RewindConflict)
        );
        let golden = model.render(80, 24).golden();
        assert!(golden.contains("conflicts:1"));
        assert!(golden.contains("conflict:src/main.rs data-loss"));
        assert!(golden.contains("apply:refused"));
        assert!(!model.mutates_store());
    }

    #[test]
    fn rewind_without_conflicts_shows_resulting_goal_and_allows_intent() {
        let current = paused_after_goal();
        let historical = created_only();
        let rewind = RewindObservation::new(historical, 1, current.seq())
            .expect("rewind")
            .with_workspace(rewind_preview(false));
        let observation = SessionLifecycleObservation::new(current).with_rewind(rewind);
        let model = SessionActionsViewModel::from_action(
            &KernelAction::RewindSession { to_seq: Some(1) },
            &observation,
            &cancel(),
        )
        .expect("plan");
        assert!(model.preview().can_apply());
        assert_eq!(model.preview().workspace_ops(), 1);
        assert_eq!(model.conflicts().len(), 0);
        let intent = model.apply(&cancel()).expect("apply");
        assert_eq!(
            intent.kernel_action(),
            KernelAction::RewindSession { to_seq: Some(1) }
        );
    }

    #[test]
    fn resume_parks_active_goal_and_never_auto_continues() {
        let snapshot = active_goal_snapshot();
        let observation = SessionLifecycleObservation::new(snapshot).with_recovered(true);
        let model = SessionActionsViewModel::plan(
            SessionLifecycleRequest::Resume { session: None },
            &observation,
            &cancel(),
        )
        .expect("plan");
        let preview = model.preview();
        assert_eq!(preview.kind(), SessionLifecycleKind::Resume);
        assert_eq!(
            preview.source_goal().expect("source").state(),
            GoalState::Active
        );
        let result = preview.resulting_goal().expect("result");
        assert_eq!(result.state(), GoalState::Paused);
        assert_eq!(result.stop_reason(), Some(GoalStopReason::ProcessRecovered));
        assert!(!preview.auto_continues());
        assert!(!preview.copies_active_goal());
        assert_eq!(preview.resulting_status(), SessionStatus::Paused);
        let intent = model.apply(&cancel()).expect("apply");
        assert!(matches!(intent, SessionLifecycleIntent::Resume(_)));
        let golden = model.render(80, 24).golden();
        assert!(golden.contains("goal:active -> paused (process_recovered)"));
        assert!(golden.contains("auto-continue:no"));
    }

    #[test]
    fn compact_preview_preserves_goal_and_budget() {
        let snapshot = active_goal_snapshot();
        let goal_id = snapshot.top_level_goal().expect("goal").id();
        let compact = CompactObservation::new(snapshot.id(), 1, snapshot.seq())
            .expect("compact")
            .with_goal(goal_id, GoalState::Active, Some(8), Some(1000))
            .with_counts(1, 2);
        let observation = SessionLifecycleObservation::new(snapshot).with_compact(compact);
        let model =
            SessionActionsViewModel::from_command(&UiCommand::Compact, &observation, &cancel())
                .expect("plan");
        let preview = model.preview();
        assert!(preview.compact_preserves_goal());
        assert!(preview.compact_preserves_budget());
        assert_eq!(preview.compact_blockers(), 1);
        assert_eq!(preview.compact_files(), 2);
        assert_eq!(
            preview.resulting_goal().expect("goal").state(),
            GoalState::Active
        );
        assert_eq!(
            model.apply(&cancel()).expect("apply").kernel_action(),
            KernelAction::CompactSession
        );
        let golden = model.render(80, 24).golden();
        assert!(golden.contains("goal:preserved"));
        assert!(golden.contains("budget:preserved"));
        assert!(golden.contains("leases:omitted"));
    }

    #[test]
    fn compact_that_drops_goal_is_refused() {
        let snapshot = active_goal_snapshot();
        let compact = CompactObservation::new(snapshot.id(), 1, snapshot.seq()).expect("compact");
        let observation = SessionLifecycleObservation::new(snapshot).with_compact(compact);
        let err = SessionActionsViewModel::plan(
            SessionLifecycleRequest::Compact,
            &observation,
            &cancel(),
        )
        .expect_err("dropped goal");
        assert_eq!(err, SessionActionError::GoalUnsafe);
        assert_eq!(err.to_string(), "session action would violate goal safety");
    }

    #[test]
    fn closed_and_blocked_and_cancelled_fail_closed() {
        let closed = serde_json::from_value::<SessionSnapshot>(json!({
            "schema": 1,
            "id": SESSION_ID,
            "project_id": PROJECT_ID,
            "status": "closed",
            "active_turn": null,
            "top_level_goal": null,
            "active_agents": [],
            "seq": 1,
            "created_at": CREATED_AT,
            "updated_at": UPDATED_AT
        }))
        .expect("closed");
        let err = SessionActionsViewModel::plan(
            SessionLifecycleRequest::Fork,
            &SessionLifecycleObservation::new(closed),
            &cancel(),
        )
        .expect_err("closed");
        assert_eq!(err, SessionActionError::ClosedSession);

        let ready = created_only();
        let blocked = SessionLifecycleObservation::new(ready.clone()).with_actions_blocked(true);
        assert_eq!(
            SessionActionsViewModel::plan(SessionLifecycleRequest::Compact, &blocked, &cancel()),
            Err(SessionActionError::ActionsBlocked)
        );

        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            SessionActionsViewModel::plan(
                SessionLifecycleRequest::Fork,
                &SessionLifecycleObservation::new(ready),
                &token
            ),
            Err(SessionActionError::Cancelled)
        );
    }

    #[test]
    fn command_router_feeds_the_planner() {
        assert_eq!(
            SessionLifecycleRequest::from_command(&parse_command("/resume").expect("resume")),
            Some(SessionLifecycleRequest::Resume { session: None })
        );
        assert_eq!(
            dispatch(parse_command("/fork").expect("fork")),
            crate::commands::FrontendAction::Kernel(KernelAction::ForkSession)
        );
        assert_eq!(
            SessionLifecycleRequest::from_action(&KernelAction::CompactSession),
            Some(SessionLifecycleRequest::Compact)
        );
    }

    #[test]
    fn render_pads_to_requested_geometry() {
        let model = SessionActionsViewModel::plan(
            SessionLifecycleRequest::Fork,
            &SessionLifecycleObservation::new(created_only()),
            &cancel(),
        )
        .expect("plan");
        let frame = model.render(80, 24);
        assert_eq!(frame.text().lines().count(), 24);
        assert!(frame.text().lines().all(|line| line.chars().count() == 80));
        assert_eq!(
            model.render(120, 24).golden(),
            model.render(80, 24).golden()
        );
    }
}
