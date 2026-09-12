//! Immutable TUI state reducer driven only by kernel events/snapshots.
//!
//! `reduce` is a pure fold: it reads the prior [`AppState`] and a [`UiEvent`].
//! It does not perform I/O and never issues kernel commands. Domain maps are
//! keyed by stable protocol IDs. Local chrome (route/focus/composer) cannot
//! insert agents, goals, jobs, or approvals.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use event_ledger::event::{ErasedEventEnvelope, EventKind};
use kernel::{
    GoalSnapshot, GoalState, GoalStopReason, ProjectionError, SessionSnapshot, SessionStatus, apply,
};
use protocol::{
    AgentId, ArtifactRef, EvidenceId, GoalId, IdParseError, JobId, RedactionClass, SessionId,
    TurnId, WorkspaceViewId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Architecture name for [`AppState`].
pub type UiState = AppState;

/// Maximum events accepted by [`replay`].
pub const MAX_REPLAY_EVENTS: usize = kernel::MAX_REPLAY_EVENTS;

/// Maximum agents retained in the frontend projection.
pub const MAX_PROJECTED_AGENTS: usize = 1024;

/// Maximum goals retained in the frontend projection.
pub const MAX_PROJECTED_GOALS: usize = 64;

/// Maximum jobs retained in the frontend projection.
pub const MAX_PROJECTED_JOBS: usize = 256;

/// Maximum approvals retained in the frontend projection.
pub const MAX_PROJECTED_APPROVALS: usize = 256;

/// Hard cap on the lines of one job's output held for the `/jobs logs` view.
///
/// The spool a job writes is capped in bytes by the producer, but a byte cap
/// still admits an unbounded *line* count; this is what the panel keeps, and
/// it keeps the tail — the end of a build log is the part a reader wants.
pub const MAX_JOB_LOG_LINES: usize = 512;

/// Hard cap on retrieved blocks listed by the `/context search` view.
pub const MAX_CONTEXT_HITS: usize = 64;

/// Maximum approval/protocol modals on the stack.
pub const MAX_MODALS: usize = 16;

/// Maximum transcript entries retained; the oldest is dropped once exceeded
/// (a live conversation view, not a durable history — the kernel ledger is
/// the durable record).
pub const MAX_TRANSCRIPT_ENTRIES: usize = 4096;

/// Maximum UTF-8 bytes accepted in composer text (local chrome only).
pub const MAX_COMPOSER_BYTES: usize = 32 * 1024;

/// Maximum UTF-8 bytes retained for a display string copied from an event.
pub const MAX_DISPLAY_TEXT_BYTES: usize = 16 * 1024;

/// Maximum UTF-8 bytes accepted in an [`ApprovalKey`].
pub const MAX_APPROVAL_KEY_BYTES: usize = 128;

const CANCEL_CHECK_EVERY: usize = 32;

/// Cooperative cancellation for [`replay`].
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Input to [`reduce`]. Domain mutations come only from kernel events/snapshots.
#[derive(Clone, Debug, PartialEq)]
pub enum UiEvent {
    Kernel(ErasedEventEnvelope),
    Snapshot(SessionSnapshot),
    Local(LocalUiEvent),
}

/// View-only chrome. Cannot create or delete domain rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalUiEvent {
    FocusPanel(PanelId),
    SetRoute(UiRoute),
    SetViewport {
        width: u16,
        height: u16,
    },
    SetComposerText(String),
    CloseModal,
    SelectAgent(Option<AgentId>),
    SelectGoal(Option<GoalId>),
    SelectJob(Option<JobId>),
    SelectApproval(Option<ApprovalKey>),
    /// Project a host-owned goal (e.g. the composition-root persisted goal)
    /// into the frontend without a kernel event. Local chrome, not a mutation
    /// of business authority.
    SyncGoal(GoalProjection),
    /// Project the session's configured models — which one is active, which
    /// are fallbacks — into the frontend without a kernel event, the same way
    /// [`Self::SyncGoal`] projects a host-owned goal. Model configuration is
    /// host state read from files and environment, not session history, so
    /// there is no kernel event to carry it.
    SyncModels(Vec<ModelRow>),
    /// Project the project memory index (`.rapidlm/MEMORY.md`) — the same
    /// bounded text the model is given — into the frontend. Host state read
    /// from a file, like [`Self::SyncModels`], so there is no kernel event
    /// to carry it.
    SyncMemory(Vec<String>),
    /// Output for the selected job, or `None` to leave the logs view.
    SyncJobLogs(Option<JobLogView>),
    /// Results for a `/context search`, or `None` to leave the search view.
    SyncContextSearch(Option<ContextSearchView>),
    /// The agent `/diff --agent` named, recorded so the panel can say it
    /// cannot narrow by one. `None` clears it.
    SelectDiffAgent(Option<AgentId>),
    /// Drop a host-owned goal projection that no longer has a snapshot to
    /// project from — completion/cancel clear the host's own snapshot (see
    /// `agent_runtime::GoalState`'s own doc comment), so without this a
    /// completed/cancelled goal would keep showing as its last-known
    /// (usually `Active`) lifecycle forever. Local chrome, not a mutation of
    /// business authority — the real state change already happened in the
    /// host; this only stops the frontend from displaying a stale row.
    ClearGoal(GoalId),
    /// A local slash-command's own output — help text, a confirmation — that
    /// never touched the model. See [`TranscriptEntry::CommandOutput`].
    AppendCommandOutput(String),
    /// A local slash-command that could not run: unknown command, bad
    /// arguments, or a refused domain mutation. Never a turn/tool failure —
    /// see [`TranscriptEntry::CommandError`].
    AppendCommandError(String),
}

/// Typed fold failure. Display never echoes untrusted payload text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiStateError {
    Cancelled,
    TooManyEvents,
    ActionsBlocked,
    Kernel(ProjectionError),
    SessionMismatch {
        expected: SessionId,
        found: SessionId,
    },
    MissingField {
        field: &'static str,
    },
    InvalidField {
        field: &'static str,
    },
    FieldTooLong {
        field: &'static str,
    },
    AgentLimit,
    GoalLimit,
    JobLimit,
    ApprovalLimit,
    ModalLimit,
}

/// Frontend projection. Business authority stays in the kernel snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    snapshot: Option<SessionSnapshot>,
    cached_projection_version: u64,
    route: UiRoute,
    focused_panel: PanelId,
    viewport: Viewport,
    composer: ComposerState,
    modal_stack: Vec<Modal>,
    agents: BTreeMap<AgentId, AgentProjection>,
    goals: BTreeMap<GoalId, GoalProjection>,
    jobs: BTreeMap<JobId, JobProjection>,
    /// Configured models, projected by the host — see
    /// [`LocalUiEvent::SyncModels`].
    models: Vec<ModelRow>,
    /// The project memory index as the model receives it — see
    /// [`LocalUiEvent::SyncMemory`].
    memory: Vec<String>,
    /// Output of the job `/jobs logs` last asked for — see
    /// [`LocalUiEvent::SyncJobLogs`]. `None` whenever no logs view is open.
    job_logs: Option<JobLogView>,
    /// What `/context search` last retrieved — see
    /// [`LocalUiEvent::SyncContextSearch`]. `None` whenever no search is open.
    context_search: Option<ContextSearchView>,
    /// The agent `/diff --agent` asked to narrow to.
    ///
    /// Deliberately *not* a filter: `workspace.mutation_detected` carries no
    /// agent, so nothing here could narrow by one. It is recorded so the
    /// panel can say that, where the user is looking, instead of painting
    /// every change as though the flag had been applied.
    diff_agent: Option<AgentId>,
    /// Compiled-context usage from the last `context.compiled` event: tokens
    /// included in the packet the model was given, against the hard limit.
    /// `None` until a turn has reported one — there is no honest figure to
    /// show before then.
    context_usage: Option<(u64, u64)>,
    /// Per-class breakdown from the same `context.compiled` event — which
    /// class is consuming the window, which the totals cannot answer.
    context_partitions: Vec<ContextPartition>,
    /// Files this session's turns wrote, keyed by workspace-relative path so
    /// repeated writes to one file collapse into one row.
    changed_files: BTreeMap<String, ChangedFile>,
    approvals: BTreeMap<ApprovalKey, ApprovalProjection>,
    selected_agent: Option<AgentId>,
    selected_goal: Option<GoalId>,
    selected_job: Option<JobId>,
    selected_approval: Option<ApprovalKey>,
    control_holder: ControlHolder,
    protocol_error: Option<String>,
    actions_blocked: bool,
    transcript: Vec<TranscriptEntry>,
}

/// Default interactive route.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiRoute {
    #[default]
    Transcript,
    Agents,
    Diff,
    Context,
    Memory,
    Jobs,
    Approvals,
    Goals,
    Graph,
    Computer,
    Resources,
    Models,
}

/// Focusable layout region. Sizing lives in a later layout task.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelId {
    #[default]
    Transcript,
    Composer,
    Status,
    Sidebar,
    Modal,
}

/// Terminal size/scroll chrome. Not domain state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Viewport {
    width: u16,
    height: u16,
    scroll: u64,
}

/// Local composer buffer. Submitting is a kernel command, not a reduce step.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComposerState {
    text: String,
}

/// Overlay keyed by a stable ID when the overlay is an approval.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modal {
    Approval { id: ApprovalKey },
    ProtocolError,
}

/// Who currently owns input control. Updated from control-transfer events.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlHolder {
    #[default]
    Agent,
    Human,
}

/// Persistent vs managed worker class. Unknown until an event names it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerClass {
    #[default]
    Unspecified,
    Persistent,
    Managed,
}

/// Wire agent lifecycle copied from `agent-runtime` / ledger payloads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLifecycle {
    #[default]
    Queued,
    Starting,
    Running,
    WaitingTool,
    WaitingApproval,
    Paused,
    Blocked,
    Succeeded,
    Failed,
    Cancelled,
}

/// Agent row keyed by [`AgentId`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentProjection {
    id: AgentId,
    parent_id: Option<AgentId>,
    state: AgentLifecycle,
    role: Option<String>,
    worker_class: WorkerClass,
    workspace_view_id: Option<WorkspaceViewId>,
    tokens: u64,
    cost: u64,
    tool_calls: u64,
    active_ms: u64,
    current_operation: Option<String>,
    last_evidence: Option<EvidenceId>,
    last_artifact: Option<ArtifactRef>,
    blocker: Option<String>,
}

/// Goal row keyed by [`GoalId`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GoalProjection {
    id: GoalId,
    statement: Option<String>,
    lifecycle: GoalLifecycle,
    stop_reason: Option<GoalStopReason>,
    max_turns: Option<u64>,
    max_tokens: Option<u64>,
    turns: u64,
    tokens: u64,
}

/// Goal lifecycle including terminal events that clear the kernel snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalLifecycle {
    Active,
    Paused,
    Blocked,
    Completed,
    Cancelled,
}

/// One rendered unit of the live conversation view. A projection of the
/// turn-lifecycle/model/tool kernel events, not the durable record itself
/// (the ledger is) — bounded and droppable, per [`MAX_TRANSCRIPT_ENTRIES`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEntry {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolActivity {
        tool: String,
        status: ToolActivityStatus,
        /// Why, when the producing event carried a reason. Populated today
        /// only for [`ToolActivityStatus::Denied`]: without it the user saw
        /// `⛔ workspace_write` and nothing else, while the reason — which
        /// names the remedy — went only to the model as a tool result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    TurnFailed {
        reason: String,
    },
    TurnInterrupted,
    /// A local slash-command's own output (help text, a confirmation). Never
    /// sent to or produced by the model — kept distinct from [`Self::
    /// Assistant`] so a command result is never misrepresented as model
    /// output.
    CommandOutput {
        text: String,
    },
    /// A local slash-command that could not run (unknown command, bad
    /// arguments, a refused domain mutation). Kept distinct from [`Self::
    /// TurnFailed`] so a parser/dispatch error is never misrepresented as a
    /// turn failure — no turn ever started.
    CommandError {
        text: String,
    },
}

/// One tool call's lifecycle, as reflected into the transcript. Not the
/// tool's own result content (`agent_runtime::TurnEvent` doesn't carry
/// that) — just what stage it reached.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolActivityStatus {
    Started,
    Completed,
    Failed,
    Denied,
    ApprovalRequired,
    ContextRequired,
}

/// Job row keyed by [`JobId`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobProjection {
    id: JobId,
    state: JobLifecycle,
    exit_status: Option<i32>,
    /// What the job is running, for a reader who needs to recognise it.
    ///
    /// A `JobId` is a UUID; a panel listing four of them tells the user
    /// nothing about which is the test run they are waiting on. Carried
    /// through the same bounded, redaction-aware accessor as every other
    /// display string, so it can neither exceed the display bound nor
    /// survive a secret-classified event.
    command: Option<String>,
    /// The `job-N` handle the model was given and the transcript uses.
    ///
    /// `job.started` has carried this since the producer existed — its own
    /// comment says it is there "so a reader can correlate the panel row
    /// with what the transcript said" — and the projection dropped it, so
    /// the correlation never reached a reader. It is also the name a user
    /// can actually type: `/jobs cancel job-3` resolves through it.
    handle: Option<String>,
}

/// One file this session changed, as the `/diff` panel shows it.
///
/// Line counts always — they are true for every write — and the unified
/// hunks of the *latest* write when the producer could compute them.
/// `before` is `None` for a file that did not exist.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub lines_before: Option<u64>,
    pub lines_after: u64,
    /// How many times this session wrote it — a file rewritten repeatedly is
    /// a different situation from one touched once, and the last write's
    /// counts alone cannot say which happened.
    pub writes: u64,
    /// Unified hunks of the most recent write, as the producer computed
    /// them at the write site. `None` when it could not (binary content, a
    /// change past its bounds) or when a write did not carry any.
    ///
    /// The latest write, not a cumulative diff: producing "what changed
    /// since the session began" would need the original content kept
    /// somewhere, and the ledger deliberately carries hunk text rather than
    /// file copies. A file written more than once says so through `writes`.
    pub hunks: Option<String>,
}

/// One hard context partition, as the `/context` panel shows it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextPartition {
    /// Partition class: `system`, `user`, `goal`, `diff`, `retrieved`,
    /// `memory`, `read_set`.
    pub class: String,
    pub used: u64,
    pub cap: u64,
}

/// One configured model, as the `/models` panel shows it.
///
/// Carries no credential and has nowhere to put one: `[model.<id>]` tables
/// hold `api_key`/`env_key`, and the way to guarantee those never reach a
/// rendered frame is for the projected type to be unable to hold them, not
/// for every construction site to remember to leave them out.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelRow {
    /// The `[model.<id>]` table id the user configured it under.
    pub id: String,
    pub provider: String,
    /// Provider-side model id sent on the wire.
    pub model: String,
    /// Whether this is the model the session resolved.
    pub active: bool,
    /// Position in the explicit `[models] fallback` chain, when it is in one.
    pub fallback_rank: Option<usize>,
    pub context_window: Option<u32>,
}

/// Job lifecycle copied from job event kinds / optional payload state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobLifecycle {
    Started,
    Output,
    Completed,
    /// Exited non-zero, or never started.
    Failed,
    /// Stopped on request, or at shutdown.
    Cancelled,
    /// Ran past its deadline and was stopped.
    TimedOut,
    OrphanReconciled,
}

/// Opaque approval identity from `approval_id` / `id` payload fields.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct ApprovalKey(String);

/// Approval row keyed by [`ApprovalKey`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApprovalProjection {
    id: ApprovalKey,
    state: ApprovalLifecycle,
    decision: Option<ApprovalDecisionView>,
}

/// Approval lifecycle copied from approval event kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalLifecycle {
    Requested,
    Resolved,
    Expired,
}

/// Decision recorded on `approval.resolved`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecisionView {
    Approved,
    Denied,
}

/// Pure fold. No I/O and no kernel mutations.
///
/// Protocol/invariant failures are recorded on the returned state and block
/// further side-effecting actions. The bad event is not applied.
pub fn reduce(state: AppState, event: &UiEvent) -> AppState {
    match try_reduce(state.clone(), event) {
        Ok(next) => next,
        Err(err) => state.record_error(err),
    }
}

/// Fold [`reduce`] over `events`. The same list always yields a byte-equal state.
pub fn replay(events: &[UiEvent], cancel: &CancellationToken) -> Result<AppState, UiStateError> {
    if events.len() > MAX_REPLAY_EVENTS {
        return Err(UiStateError::TooManyEvents);
    }
    check_cancel(cancel)?;
    let mut state = AppState::new();
    for (i, event) in events.iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            check_cancel(cancel)?;
        }
        state = try_reduce(state, event)?;
    }
    Ok(state)
}

fn try_reduce(state: AppState, event: &UiEvent) -> Result<AppState, UiStateError> {
    match event {
        UiEvent::Snapshot(snapshot) => apply_snapshot(state, snapshot),
        UiEvent::Kernel(envelope) => apply_kernel(state, envelope),
        UiEvent::Local(local) => apply_local(state, local),
    }
}

fn apply_snapshot(
    mut state: AppState,
    snapshot: &SessionSnapshot,
) -> Result<AppState, UiStateError> {
    if let Some(current) = state.snapshot.as_ref()
        && current.id() != snapshot.id()
    {
        return Err(UiStateError::SessionMismatch {
            expected: current.id(),
            found: snapshot.id(),
        });
    }
    merge_snapshot_rows(&mut state, snapshot)?;
    state.snapshot = Some(snapshot.clone());
    state.cached_projection_version = snapshot.seq();
    state.protocol_error = None;
    state.actions_blocked = false;
    state
        .modal_stack
        .retain(|modal| !matches!(modal, Modal::ProtocolError));
    Ok(state)
}

fn apply_kernel(
    mut state: AppState,
    event: &ErasedEventEnvelope,
) -> Result<AppState, UiStateError> {
    if state.actions_blocked {
        return Err(UiStateError::ActionsBlocked);
    }
    if let Some(current) = state.snapshot.as_ref()
        && current.id() != event.session_id()
    {
        return Err(UiStateError::SessionMismatch {
            expected: current.id(),
            found: event.session_id(),
        });
    }

    let snapshot = apply(state.snapshot.clone(), event).map_err(UiStateError::Kernel)?;
    state.snapshot = Some(snapshot);
    state.cached_projection_version = event.seq();

    match event.kind() {
        EventKind::AgentSpawned | EventKind::AgentStarted | EventKind::AgentStateChanged => {
            upsert_agent(&mut state, event, default_lifecycle(event.kind()))?;
        }
        EventKind::AgentPoolBackgroundStarted => {
            let agent = upsert_agent(&mut state, event, AgentLifecycle::Starting)?;
            agent.worker_class = WorkerClass::Persistent;
        }
        EventKind::AgentPoolBackgroundParked => {
            let agent = upsert_agent(&mut state, event, AgentLifecycle::Paused)?;
            agent.worker_class = WorkerClass::Persistent;
        }
        EventKind::AgentTaskEnvelopeCreated => {
            let agent = upsert_agent(&mut state, event, AgentLifecycle::Queued)?;
            agent.worker_class = WorkerClass::Managed;
        }
        EventKind::AgentResult => {
            upsert_agent(&mut state, event, AgentLifecycle::Succeeded)?;
        }
        EventKind::AgentCancelled => {
            upsert_agent(&mut state, event, AgentLifecycle::Cancelled)?;
        }
        EventKind::GoalCreated => {
            upsert_goal(&mut state, event, GoalLifecycle::Active)?;
        }
        EventKind::GoalUpdated | EventKind::GoalBudgetUpdated => {
            let id = parse_id::<GoalId>(event.payload(), "goal_id")?;
            let lifecycle = state
                .goals
                .get(&id)
                .map(GoalProjection::lifecycle)
                .unwrap_or(GoalLifecycle::Active);
            upsert_goal(&mut state, event, lifecycle)?;
        }
        EventKind::GoalResumed => {
            upsert_goal(&mut state, event, GoalLifecycle::Active)?;
        }
        EventKind::GoalPaused => {
            upsert_goal(&mut state, event, GoalLifecycle::Paused)?;
        }
        EventKind::GoalBlocked => {
            upsert_goal(&mut state, event, GoalLifecycle::Blocked)?;
        }
        EventKind::GoalCompleted => {
            upsert_goal(&mut state, event, GoalLifecycle::Completed)?;
        }
        EventKind::GoalCancelled => {
            upsert_goal(&mut state, event, GoalLifecycle::Cancelled)?;
        }
        EventKind::JobStarted => {
            upsert_job(&mut state, event, JobLifecycle::Started)?;
        }
        EventKind::WorkspaceMutationDetected => {
            // The path is workspace-relative text from a tool call, so it
            // goes through the same bounded, redaction-aware accessor as
            // every other display string.
            if let (Some(path), Some(after)) = (
                optional_display(event, event.payload(), "path")?,
                optional_u64(event.payload(), "lines_after")?,
            ) {
                let before = optional_u64(event.payload(), "lines_before")?;
                // Bounded and redaction-aware like `path`: this is file
                // content, and a secret-classified write must not leave its
                // hunks in the panel.
                let hunks = optional_display(event, event.payload(), "hunks")?;
                let entry = state
                    .changed_files
                    .entry(path.clone())
                    .or_insert(ChangedFile {
                        path,
                        // Only the *first* write's "before" describes what
                        // the session started from; a later write's before is
                        // this session's own earlier output.
                        lines_before: before,
                        lines_after: after,
                        writes: 0,
                        hunks: None,
                    });
                entry.lines_after = after;
                entry.writes = entry.writes.saturating_add(1);
                // The latest write's hunks replace the previous write's —
                // and a write that carried none clears them, since the row
                // would otherwise describe an older state than its counts.
                entry.hunks = hunks;
            }
        }
        EventKind::ContextCompiled => {
            // Both fields or neither: a limit without a usage (or the
            // reverse) would render as a ratio with an invented half.
            if let (Some(used), Some(limit)) = (
                optional_u64(event.payload(), "included_tokens")?,
                optional_u64(event.payload(), "context_limit")?,
            ) {
                state.context_usage = Some((used, limit));
            }
            // Rows are optional: an emitter that reports only the totals
            // leaves the panel showing those, rather than failing the event.
            if let Some(rows) = event.payload().get("partitions").and_then(|v| v.as_array()) {
                let parsed: Vec<ContextPartition> = rows
                    .iter()
                    .filter_map(|row| {
                        Some(ContextPartition {
                            class: row.get("class")?.as_str()?.to_owned(),
                            used: row.get("used")?.as_u64()?,
                            cap: row.get("cap")?.as_u64()?,
                        })
                    })
                    .collect();
                if !parsed.is_empty() {
                    state.context_partitions = parsed;
                }
            }
        }
        EventKind::JobOutput => {
            upsert_job(&mut state, event, JobLifecycle::Output)?;
        }
        EventKind::JobCompleted => {
            upsert_job(&mut state, event, JobLifecycle::Completed)?;
        }
        EventKind::JobOrphanReconciled => {
            upsert_job(&mut state, event, JobLifecycle::OrphanReconciled)?;
        }
        EventKind::ApprovalRequested => {
            let id = upsert_approval(&mut state, event, ApprovalLifecycle::Requested)?;
            push_approval_modal(&mut state, id)?;
        }
        EventKind::ToolApprovalRequired => {
            let id = upsert_approval(&mut state, event, ApprovalLifecycle::Requested)?;
            push_approval_modal(&mut state, id)?;
            if let Some(tool) = optional_display(event, event.payload(), "tool")? {
                push_transcript(
                    &mut state,
                    TranscriptEntry::ToolActivity {
                        tool,
                        status: ToolActivityStatus::ApprovalRequired,
                        detail: None,
                    },
                );
            }
        }
        EventKind::ApprovalResolved => {
            let id = upsert_approval(&mut state, event, ApprovalLifecycle::Resolved)?;
            pop_approval_modal(&mut state, &id);
        }
        EventKind::ApprovalExpired => {
            let id = upsert_approval(&mut state, event, ApprovalLifecycle::Expired)?;
            pop_approval_modal(&mut state, &id);
        }
        EventKind::ControlTransferredToHuman => {
            state.control_holder = ControlHolder::Human;
        }
        EventKind::ControlTransferredToAgent => {
            state.control_holder = ControlHolder::Agent;
        }
        EventKind::TurnStarted => {
            if let Some(text) = optional_display(event, event.payload(), "text")?
                && !text.is_empty()
            {
                push_transcript(&mut state, TranscriptEntry::User { text });
            }
        }
        EventKind::ToolStarted => {
            push_tool_activity(&mut state, event, ToolActivityStatus::Started)?;
        }
        EventKind::ToolCompleted => {
            push_tool_activity(&mut state, event, ToolActivityStatus::Completed)?;
        }
        EventKind::ToolFailed => {
            push_tool_activity(&mut state, event, ToolActivityStatus::Failed)?;
        }
        EventKind::ToolContextRequired => {
            push_tool_activity(&mut state, event, ToolActivityStatus::ContextRequired)?;
        }
        EventKind::ToolDenied => {
            push_tool_activity(&mut state, event, ToolActivityStatus::Denied)?;
        }
        EventKind::TurnCompleted => {
            if let Some(text) = optional_display(event, event.payload(), "text")? {
                push_transcript(&mut state, TranscriptEntry::Assistant { text });
            }
        }
        EventKind::TurnFailed => {
            if let Some(reason) = optional_display(event, event.payload(), "reason")? {
                push_transcript(&mut state, TranscriptEntry::TurnFailed { reason });
            }
        }
        EventKind::TurnInterrupted => {
            push_transcript(&mut state, TranscriptEntry::TurnInterrupted);
        }
        _ => {}
    }
    Ok(state)
}

/// Push a bounded transcript entry, dropping the oldest once
/// [`MAX_TRANSCRIPT_ENTRIES`] is exceeded — a live view, not the durable
/// record (the kernel ledger is that).
fn push_transcript(state: &mut AppState, entry: TranscriptEntry) {
    state.transcript.push(entry);
    if state.transcript.len() > MAX_TRANSCRIPT_ENTRIES {
        state.transcript.remove(0);
    }
}

fn push_tool_activity(
    state: &mut AppState,
    event: &ErasedEventEnvelope,
    status: ToolActivityStatus,
) -> Result<(), UiStateError> {
    if let Some(tool) = optional_display(event, event.payload(), "tool")? {
        // Through the same bounded, redaction-aware accessor as `tool`, so a
        // reason can neither exceed the display bound nor survive a
        // secret-classified event.
        let detail = optional_display(event, event.payload(), "detail")?;
        push_transcript(
            state,
            TranscriptEntry::ToolActivity {
                tool,
                status,
                detail,
            },
        );
    }
    Ok(())
}

fn apply_local(mut state: AppState, event: &LocalUiEvent) -> Result<AppState, UiStateError> {
    match event {
        LocalUiEvent::FocusPanel(panel) => state.focused_panel = *panel,
        LocalUiEvent::SetRoute(route) => state.route = *route,
        LocalUiEvent::SetViewport { width, height } => {
            state.viewport.width = *width;
            state.viewport.height = *height;
        }
        LocalUiEvent::SetComposerText(text) => {
            if text.len() > MAX_COMPOSER_BYTES {
                return Err(UiStateError::FieldTooLong { field: "composer" });
            }
            state.composer.text = text.clone();
        }
        LocalUiEvent::CloseModal => {
            state.modal_stack.pop();
        }
        LocalUiEvent::SelectAgent(id) => state.selected_agent = *id,
        LocalUiEvent::SelectGoal(id) => state.selected_goal = *id,
        LocalUiEvent::SelectJob(id) => state.selected_job = *id,
        LocalUiEvent::SelectApproval(id) => state.selected_approval = id.clone(),
        LocalUiEvent::SyncModels(rows) => {
            state.models = rows.clone();
        }
        LocalUiEvent::SyncMemory(lines) => {
            state.memory = lines.clone();
        }
        LocalUiEvent::SyncJobLogs(page) => {
            state.job_logs = page.clone();
        }
        LocalUiEvent::SyncContextSearch(found) => {
            state.context_search = found.clone();
        }
        LocalUiEvent::SelectDiffAgent(agent) => {
            state.diff_agent = *agent;
        }
        LocalUiEvent::SyncGoal(goal) => {
            insert_goal(&mut state, goal.clone())?;
            state.selected_goal = Some(goal.id);
        }
        LocalUiEvent::ClearGoal(id) => {
            state.goals.remove(id);
            if state.selected_goal == Some(*id) {
                state.selected_goal = None;
            }
        }
        LocalUiEvent::AppendCommandOutput(text) => {
            push_transcript(
                &mut state,
                TranscriptEntry::CommandOutput { text: text.clone() },
            );
        }
        LocalUiEvent::AppendCommandError(text) => {
            push_transcript(
                &mut state,
                TranscriptEntry::CommandError { text: text.clone() },
            );
        }
    }
    Ok(state)
}

fn merge_snapshot_rows(
    state: &mut AppState,
    snapshot: &SessionSnapshot,
) -> Result<(), UiStateError> {
    if let Some(goal) = snapshot.top_level_goal() {
        insert_goal(state, GoalProjection::from_kernel(goal))?;
    }
    for agent_id in snapshot.active_agents() {
        if !state.agents.contains_key(agent_id) {
            insert_agent(state, AgentProjection::new(*agent_id))?;
        }
    }
    Ok(())
}

fn upsert_agent<'a>(
    state: &'a mut AppState,
    event: &ErasedEventEnvelope,
    default_state: AgentLifecycle,
) -> Result<&'a mut AgentProjection, UiStateError> {
    let payload = event.payload();
    let id = parse_id::<AgentId>(payload, "agent_id")?;
    if !state.agents.contains_key(&id) {
        insert_agent(state, AgentProjection::new(id))?;
    }
    let agent = state
        .agents
        .get_mut(&id)
        .ok_or(UiStateError::InvalidField { field: "agent_id" })?;
    if let Some(parent) = optional_id::<AgentId>(payload, "parent_id")? {
        if let Some(existing) = agent.parent_id
            && existing != parent
        {
            return Err(UiStateError::InvalidField { field: "parent_id" });
        }
        agent.parent_id = Some(parent);
    }
    if let Some(view) = optional_id::<WorkspaceViewId>(payload, "workspace_view_id")? {
        if let Some(existing) = agent.workspace_view_id
            && existing != view
        {
            return Err(UiStateError::InvalidField {
                field: "workspace_view_id",
            });
        }
        agent.workspace_view_id = Some(view);
    }
    if let Some(state_name) = optional_str(payload, "state")? {
        agent.state = AgentLifecycle::parse(state_name)
            .ok_or(UiStateError::InvalidField { field: "state" })?;
    } else {
        agent.state = default_state;
    }
    if let Some(role) = optional_display(event, payload, "role")? {
        agent.role = Some(role);
    }
    if let Some(op) = optional_display(event, payload, "current_operation")? {
        agent.current_operation = Some(op);
    }
    if let Some(blocker) = optional_display(event, payload, "blocker")? {
        agent.blocker = Some(blocker);
    }
    if let Some(class) = optional_str(payload, "worker_class")? {
        agent.worker_class = WorkerClass::parse(class).ok_or(UiStateError::InvalidField {
            field: "worker_class",
        })?;
    }
    apply_agent_stats(agent, payload)?;
    if let Some(evidence) = optional_id::<EvidenceId>(payload, "evidence_id")? {
        agent.last_evidence = Some(evidence);
    }
    if let Some(artifact) = optional_artifact(payload, "artifact")? {
        agent.last_artifact = Some(artifact);
    }
    Ok(agent)
}

fn upsert_goal(
    state: &mut AppState,
    event: &ErasedEventEnvelope,
    lifecycle: GoalLifecycle,
) -> Result<(), UiStateError> {
    let payload = event.payload();
    let id = parse_id::<GoalId>(payload, "goal_id")?;
    if !state.goals.contains_key(&id) {
        insert_goal(
            state,
            GoalProjection {
                id,
                statement: None,
                lifecycle,
                stop_reason: None,
                max_turns: None,
                max_tokens: None,
                turns: 0,
                tokens: 0,
            },
        )?;
    }
    let goal = state
        .goals
        .get_mut(&id)
        .ok_or(UiStateError::InvalidField { field: "goal_id" })?;
    goal.lifecycle = lifecycle;
    if let Some(statement) = optional_display(event, payload, "statement")? {
        goal.statement = Some(statement);
    }
    goal.stop_reason = match lifecycle {
        GoalLifecycle::Completed => Some(GoalStopReason::Completed),
        GoalLifecycle::Cancelled => Some(GoalStopReason::Cancelled),
        GoalLifecycle::Paused if payload_flag(payload, "process_recovered") => {
            Some(GoalStopReason::ProcessRecovered)
        }
        GoalLifecycle::Blocked if payload_flag(payload, "budget_exhausted") => {
            Some(GoalStopReason::BudgetExhausted)
        }
        GoalLifecycle::Active => None,
        _ => goal.stop_reason,
    };
    let budget = payload.get("budget").unwrap_or(payload);
    if let Some(max_turns) = optional_u64(budget, "max_turns")? {
        goal.max_turns = Some(max_turns);
    }
    if let Some(max_tokens) = optional_u64(budget, "max_tokens")? {
        goal.max_tokens = Some(max_tokens);
    }
    let usage = payload.get("usage").unwrap_or(payload);
    if let Some(turns) = optional_u64(usage, "turns")? {
        goal.turns = turns;
    }
    if let Some(tokens) = optional_u64(usage, "tokens")? {
        goal.tokens = tokens;
    }
    Ok(())
}

fn upsert_job(
    state: &mut AppState,
    event: &ErasedEventEnvelope,
    lifecycle: JobLifecycle,
) -> Result<(), UiStateError> {
    let payload = event.payload();
    let id = parse_id::<JobId>(payload, "job_id")?;
    if !state.jobs.contains_key(&id) {
        insert_job(
            state,
            JobProjection {
                id,
                state: lifecycle,
                exit_status: None,
                command: None,
                handle: None,
            },
        )?;
    }
    let job = state
        .jobs
        .get_mut(&id)
        .ok_or(UiStateError::InvalidField { field: "job_id" })?;
    if let Some(named) = optional_str(payload, "state")? {
        job.state =
            JobLifecycle::parse(named).ok_or(UiStateError::InvalidField { field: "state" })?;
    } else {
        job.state = lifecycle;
    }
    if let Some(status) = optional_i32(payload, "exit_status")? {
        job.exit_status = Some(status);
    }
    // Only `job.started` carries these; a later event for the same job must
    // not blank out what the panel is already showing.
    if let Some(command) = optional_display(event, payload, "command")? {
        job.command = Some(command);
    }
    if let Some(handle) = optional_display(event, payload, "handle")? {
        job.handle = Some(handle);
    }
    Ok(())
}

/// One job's captured output, as the `/jobs logs` view shows it.
///
/// Tagged with the job it came from so a page synced for one job can never
/// be painted under a different selection: the panel renders these lines
/// only when `job` matches the selected job.
///
/// Host-synced rather than ledger-projected, like [`LocalUiEvent::SyncModels`]
/// and [`LocalUiEvent::SyncMemory`]: a job's bytes are spooled in this
/// process by the job registry and are not in the event ledger at all.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobLogView {
    job: JobId,
    lines: Vec<String>,
    /// The producer cut the output short of what the process actually wrote.
    truncated: bool,
    /// This page was read after the job had already stopped, so it is the
    /// whole log and need not be re-read. A page read while the job ran is
    /// re-read on every tick — including the one tick after the job stops,
    /// which is how the last lines it wrote get onto the screen.
    #[serde(default)]
    complete: bool,
}

impl JobLogView {
    pub fn new(job: JobId, lines: Vec<String>, truncated: bool) -> Self {
        let mut lines = lines;
        // Keep the tail: the end of a log is what a reader opened it for.
        if lines.len() > MAX_JOB_LOG_LINES {
            lines.drain(..lines.len() - MAX_JOB_LOG_LINES);
        }
        Self {
            job,
            lines,
            truncated,
            complete: false,
        }
    }

    /// Mark this page as read after the job stopped — the final one.
    #[must_use]
    pub fn completed(mut self) -> Self {
        self.complete = true;
        self
    }

    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    pub const fn job(&self) -> JobId {
        self.job
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// What a `/context search` retrieved, as the panel shows it.
///
/// Host-synced like [`JobLogView`]: the retrieval subsystem walks the
/// project on disk, so there is no kernel event carrying this. `outcome`
/// exists because "no blocks" has several honest causes — an untrusted
/// project is never walked at all — and a bare empty list would report a
/// refusal as though the query simply matched nothing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextSearchView {
    query: String,
    hits: Vec<ContextHit>,
    outcome: ContextSearchOutcome,
}

/// Why a search has the results it has.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ContextSearchOutcome {
    /// The retrieval pass ran.
    Searched,
    /// The project is not trusted, so nothing was indexed or walked.
    Untrusted,
}

/// One block the retrieval pass would put in front of the model.
///
/// Bytes, not tokens: `context_retrieval::retrieve` builds its inputs with
/// `CompileInput::new(locator, text)` and sets no token estimate, so a
/// token figure here would be a number this tree never computed. The byte
/// length of the retrieved text is true and answers the same question.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextHit {
    pub locator: String,
    pub bytes: u64,
}

impl ContextSearchView {
    pub fn new(query: String, hits: Vec<ContextHit>, outcome: ContextSearchOutcome) -> Self {
        let mut hits = hits;
        hits.truncate(MAX_CONTEXT_HITS);
        Self {
            query,
            hits,
            outcome,
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn hits(&self) -> &[ContextHit] {
        &self.hits
    }

    pub const fn outcome(&self) -> ContextSearchOutcome {
        self.outcome
    }
}

fn upsert_approval(
    state: &mut AppState,
    event: &ErasedEventEnvelope,
    lifecycle: ApprovalLifecycle,
) -> Result<ApprovalKey, UiStateError> {
    let payload = event.payload();
    let id = parse_approval_key(payload)?;
    if !state.approvals.contains_key(&id) {
        insert_approval(
            state,
            ApprovalProjection {
                id: id.clone(),
                state: lifecycle,
                decision: None,
            },
        )?;
    }
    let approval = state
        .approvals
        .get_mut(&id)
        .ok_or(UiStateError::InvalidField {
            field: "approval_id",
        })?;
    approval.state = lifecycle;
    if let Some(decision) = optional_str(payload, "decision")? {
        approval.decision = Some(
            ApprovalDecisionView::parse(decision)
                .ok_or(UiStateError::InvalidField { field: "decision" })?,
        );
    }
    Ok(id)
}

fn insert_agent(state: &mut AppState, agent: AgentProjection) -> Result<(), UiStateError> {
    if state.agents.len() >= MAX_PROJECTED_AGENTS && !state.agents.contains_key(&agent.id) {
        return Err(UiStateError::AgentLimit);
    }
    state.agents.insert(agent.id, agent);
    Ok(())
}

fn insert_goal(state: &mut AppState, goal: GoalProjection) -> Result<(), UiStateError> {
    if state.goals.len() >= MAX_PROJECTED_GOALS && !state.goals.contains_key(&goal.id) {
        return Err(UiStateError::GoalLimit);
    }
    state.goals.insert(goal.id, goal);
    Ok(())
}

fn insert_job(state: &mut AppState, job: JobProjection) -> Result<(), UiStateError> {
    if state.jobs.len() >= MAX_PROJECTED_JOBS && !state.jobs.contains_key(&job.id) {
        return Err(UiStateError::JobLimit);
    }
    state.jobs.insert(job.id, job);
    Ok(())
}

fn insert_approval(state: &mut AppState, approval: ApprovalProjection) -> Result<(), UiStateError> {
    if state.approvals.len() >= MAX_PROJECTED_APPROVALS
        && !state.approvals.contains_key(&approval.id)
    {
        return Err(UiStateError::ApprovalLimit);
    }
    state.approvals.insert(approval.id.clone(), approval);
    Ok(())
}

fn push_approval_modal(state: &mut AppState, id: ApprovalKey) -> Result<(), UiStateError> {
    if state
        .modal_stack
        .iter()
        .any(|modal| matches!(modal, Modal::Approval { id: existing } if *existing == id))
    {
        return Ok(());
    }
    if state.modal_stack.len() >= MAX_MODALS {
        return Err(UiStateError::ModalLimit);
    }
    state.modal_stack.push(Modal::Approval { id });
    Ok(())
}

fn pop_approval_modal(state: &mut AppState, id: &ApprovalKey) {
    state.modal_stack.retain(|modal| match modal {
        Modal::Approval { id: existing } => existing != id,
        Modal::ProtocolError => true,
    });
}

fn apply_agent_stats(agent: &mut AgentProjection, payload: &Value) -> Result<(), UiStateError> {
    let stats = payload.get("stats").unwrap_or(payload);
    if let Some(tokens) = optional_u64(stats, "tokens")? {
        agent.tokens = tokens;
    }
    if let Some(cost) = optional_u64(stats, "cost")? {
        agent.cost = cost;
    }
    if let Some(tool_calls) = optional_u64(stats, "tool_calls")? {
        agent.tool_calls = tool_calls;
    }
    if let Some(active_ms) = optional_u64(stats, "active_ms")? {
        agent.active_ms = active_ms;
    }
    Ok(())
}

fn default_lifecycle(kind: EventKind) -> AgentLifecycle {
    match kind {
        EventKind::AgentSpawned => AgentLifecycle::Queued,
        EventKind::AgentStarted => AgentLifecycle::Starting,
        EventKind::AgentStateChanged => AgentLifecycle::Running,
        _ => AgentLifecycle::Queued,
    }
}

fn parse_id<T: FromStr<Err = IdParseError>>(
    payload: &Value,
    field: &'static str,
) -> Result<T, UiStateError> {
    let Some(raw) = payload.get(field).and_then(Value::as_str) else {
        return Err(UiStateError::MissingField { field });
    };
    raw.parse()
        .map_err(|_| UiStateError::InvalidField { field })
}

fn optional_id<T: FromStr<Err = IdParseError>>(
    payload: &Value,
    field: &'static str,
) -> Result<Option<T>, UiStateError> {
    match payload.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(raw)) => raw
            .parse()
            .map(Some)
            .map_err(|_| UiStateError::InvalidField { field }),
        Some(_) => Err(UiStateError::InvalidField { field }),
    }
}

fn optional_str<'a>(
    payload: &'a Value,
    field: &'static str,
) -> Result<Option<&'a str>, UiStateError> {
    match payload.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(raw)) => Ok(Some(raw.as_str())),
        Some(_) => Err(UiStateError::InvalidField { field }),
    }
}

fn optional_display(
    event: &ErasedEventEnvelope,
    payload: &Value,
    field: &'static str,
) -> Result<Option<String>, UiStateError> {
    let Some(raw) = optional_str(payload, field)? else {
        return Ok(None);
    };
    if event.redaction() == RedactionClass::Secret {
        return Ok(None);
    }
    if raw.len() > MAX_DISPLAY_TEXT_BYTES {
        return Err(UiStateError::FieldTooLong { field });
    }
    Ok(Some(raw.to_owned()))
}

fn optional_u64(source: &Value, field: &'static str) -> Result<Option<u64>, UiStateError> {
    match source.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .ok_or(UiStateError::InvalidField { field })
            .map(Some),
    }
}

fn optional_i32(source: &Value, field: &'static str) -> Result<Option<i32>, UiStateError> {
    match source.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let Some(n) = value.as_i64() else {
                return Err(UiStateError::InvalidField { field });
            };
            i32::try_from(n)
                .map(Some)
                .map_err(|_| UiStateError::InvalidField { field })
        }
    }
}

fn optional_artifact(
    payload: &Value,
    field: &'static str,
) -> Result<Option<ArtifactRef>, UiStateError> {
    match payload.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|_| UiStateError::InvalidField { field }),
    }
}

fn parse_approval_key(payload: &Value) -> Result<ApprovalKey, UiStateError> {
    if let Some(raw) = payload.get("approval_id").and_then(Value::as_str) {
        return ApprovalKey::parse(raw);
    }
    if let Some(raw) = payload.get("id").and_then(Value::as_str) {
        return ApprovalKey::parse(raw);
    }
    Err(UiStateError::MissingField {
        field: "approval_id",
    })
}

fn payload_flag(payload: &Value, field: &str) -> bool {
    payload.get(field).and_then(Value::as_bool).unwrap_or(false)
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), UiStateError> {
    if cancel.is_cancelled() {
        Err(UiStateError::Cancelled)
    } else {
        Ok(())
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            snapshot: None,
            cached_projection_version: 0,
            route: UiRoute::Transcript,
            focused_panel: PanelId::Transcript,
            viewport: Viewport::default(),
            composer: ComposerState::default(),
            modal_stack: Vec::new(),
            agents: BTreeMap::new(),
            goals: BTreeMap::new(),
            jobs: BTreeMap::new(),
            models: Vec::new(),
            memory: Vec::new(),
            context_usage: None,
            context_partitions: Vec::new(),
            changed_files: BTreeMap::new(),
            approvals: BTreeMap::new(),
            selected_agent: None,
            selected_goal: None,
            job_logs: None,
            context_search: None,
            diff_agent: None,
            selected_job: None,
            selected_approval: None,
            control_holder: ControlHolder::Agent,
            protocol_error: None,
            actions_blocked: false,
            transcript: Vec::new(),
        }
    }

    pub fn snapshot(&self) -> Option<&SessionSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn cached_projection_version(&self) -> u64 {
        self.cached_projection_version
    }

    pub fn session_status(&self) -> Option<SessionStatus> {
        self.snapshot.as_ref().map(SessionSnapshot::status)
    }

    pub fn active_turn(&self) -> Option<TurnId> {
        self.snapshot
            .as_ref()
            .and_then(SessionSnapshot::active_turn)
    }

    pub fn route(&self) -> UiRoute {
        self.route
    }

    pub fn focused_panel(&self) -> PanelId {
        self.focused_panel
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub fn composer(&self) -> &ComposerState {
        &self.composer
    }

    pub fn modal_stack(&self) -> &[Modal] {
        &self.modal_stack
    }

    pub fn agents(&self) -> &BTreeMap<AgentId, AgentProjection> {
        &self.agents
    }

    pub fn goals(&self) -> &BTreeMap<GoalId, GoalProjection> {
        &self.goals
    }

    pub fn models(&self) -> &[ModelRow] {
        &self.models
    }

    pub fn memory(&self) -> &[String] {
        &self.memory
    }

    /// `(used, limit)` for the last turn that reported it.
    pub fn context_usage(&self) -> Option<(u64, u64)> {
        self.context_usage
    }

    pub fn context_partitions(&self) -> &[ContextPartition] {
        &self.context_partitions
    }

    pub fn changed_files(&self) -> &BTreeMap<String, ChangedFile> {
        &self.changed_files
    }

    pub fn jobs(&self) -> &BTreeMap<JobId, JobProjection> {
        &self.jobs
    }

    pub fn approvals(&self) -> &BTreeMap<ApprovalKey, ApprovalProjection> {
        &self.approvals
    }

    pub fn transcript(&self) -> &[TranscriptEntry] {
        &self.transcript
    }

    pub fn selected_agent(&self) -> Option<AgentId> {
        self.selected_agent
    }

    pub fn selected_goal(&self) -> Option<GoalId> {
        self.selected_goal
    }

    pub fn selected_job(&self) -> Option<JobId> {
        self.selected_job
    }

    /// Output synced for the selected job, if a logs view is open.
    pub fn job_logs(&self) -> Option<&JobLogView> {
        self.job_logs.as_ref()
    }

    /// Results synced for an open `/context search`.
    pub fn context_search(&self) -> Option<&ContextSearchView> {
        self.context_search.as_ref()
    }

    /// The agent `/diff --agent` named, if any.
    pub fn diff_agent(&self) -> Option<AgentId> {
        self.diff_agent
    }

    pub fn selected_approval(&self) -> Option<&ApprovalKey> {
        self.selected_approval.as_ref()
    }

    pub fn control_holder(&self) -> ControlHolder {
        self.control_holder
    }

    pub fn protocol_error(&self) -> Option<&str> {
        self.protocol_error.as_deref()
    }

    pub fn actions_blocked(&self) -> bool {
        self.actions_blocked
    }

    fn record_error(mut self, err: UiStateError) -> Self {
        if self.protocol_error.is_none() {
            self.protocol_error = Some(err.to_string());
        }
        self.actions_blocked = true;
        if !self
            .modal_stack
            .iter()
            .any(|modal| matches!(modal, Modal::ProtocolError))
            && self.modal_stack.len() < MAX_MODALS
        {
            self.modal_stack.push(Modal::ProtocolError);
        }
        self
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl Viewport {
    pub const fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            scroll: 0,
        }
    }

    pub const fn width(self) -> u16 {
        self.width
    }

    pub const fn height(self) -> u16 {
        self.height
    }

    pub const fn scroll(self) -> u64 {
        self.scroll
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Self::new(80, 24)
    }
}

impl ComposerState {
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl AgentProjection {
    fn new(id: AgentId) -> Self {
        Self {
            id,
            parent_id: None,
            state: AgentLifecycle::Queued,
            role: None,
            worker_class: WorkerClass::Unspecified,
            workspace_view_id: None,
            tokens: 0,
            cost: 0,
            tool_calls: 0,
            active_ms: 0,
            current_operation: None,
            last_evidence: None,
            last_artifact: None,
            blocker: None,
        }
    }

    pub fn id(&self) -> AgentId {
        self.id
    }

    pub fn parent_id(&self) -> Option<AgentId> {
        self.parent_id
    }

    pub fn state(&self) -> AgentLifecycle {
        self.state
    }

    pub fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }

    pub fn worker_class(&self) -> WorkerClass {
        self.worker_class
    }

    pub fn workspace_view_id(&self) -> Option<WorkspaceViewId> {
        self.workspace_view_id
    }

    pub fn tokens(&self) -> u64 {
        self.tokens
    }

    pub fn cost(&self) -> u64 {
        self.cost
    }

    pub fn tool_calls(&self) -> u64 {
        self.tool_calls
    }

    pub fn active_ms(&self) -> u64 {
        self.active_ms
    }

    pub fn current_operation(&self) -> Option<&str> {
        self.current_operation.as_deref()
    }

    pub fn last_evidence(&self) -> Option<EvidenceId> {
        self.last_evidence
    }

    pub fn last_artifact(&self) -> Option<&ArtifactRef> {
        self.last_artifact.as_ref()
    }

    pub fn blocker(&self) -> Option<&str> {
        self.blocker.as_deref()
    }
}

impl GoalProjection {
    /// Construct a goal row from host state (used by `SyncGoal`).
    pub fn new(
        id: GoalId,
        statement: Option<String>,
        lifecycle: GoalLifecycle,
        max_turns: Option<u64>,
        max_tokens: Option<u64>,
        turns: u64,
        tokens: u64,
    ) -> Self {
        Self {
            id,
            statement,
            lifecycle,
            stop_reason: None,
            max_turns,
            max_tokens,
            turns,
            tokens,
        }
    }

    fn from_kernel(goal: &GoalSnapshot) -> Self {
        Self {
            id: goal.id(),
            statement: Some(goal.statement().to_owned()),
            lifecycle: GoalLifecycle::from_kernel(goal.state()),
            stop_reason: goal.stop_reason(),
            max_turns: goal.budget().max_turns(),
            max_tokens: goal.budget().max_tokens(),
            turns: goal.usage().turns(),
            tokens: goal.usage().tokens(),
        }
    }

    pub fn id(&self) -> GoalId {
        self.id
    }

    pub fn statement(&self) -> Option<&str> {
        self.statement.as_deref()
    }

    pub fn lifecycle(&self) -> GoalLifecycle {
        self.lifecycle
    }

    pub fn stop_reason(&self) -> Option<GoalStopReason> {
        self.stop_reason
    }

    pub fn max_turns(&self) -> Option<u64> {
        self.max_turns
    }

    pub fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    pub fn turns(&self) -> u64 {
        self.turns
    }

    pub fn tokens(&self) -> u64 {
        self.tokens
    }
}

impl GoalLifecycle {
    fn from_kernel(state: GoalState) -> Self {
        match state {
            GoalState::Active => Self::Active,
            GoalState::Paused => Self::Paused,
            GoalState::Blocked => Self::Blocked,
        }
    }
}

impl JobProjection {
    pub fn id(&self) -> JobId {
        self.id
    }

    pub fn state(&self) -> JobLifecycle {
        self.state
    }

    pub fn exit_status(&self) -> Option<i32> {
        self.exit_status
    }

    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    /// The `job-N` handle, when the producer recorded one.
    pub fn handle(&self) -> Option<&str> {
        self.handle.as_deref()
    }
}

impl ApprovalProjection {
    pub fn id(&self) -> &ApprovalKey {
        &self.id
    }

    pub fn state(&self) -> ApprovalLifecycle {
        self.state
    }

    pub fn decision(&self) -> Option<ApprovalDecisionView> {
        self.decision
    }
}

impl ApprovalKey {
    pub fn parse(raw: &str) -> Result<Self, UiStateError> {
        if raw.is_empty() {
            return Err(UiStateError::InvalidField {
                field: "approval_id",
            });
        }
        if raw.len() > MAX_APPROVAL_KEY_BYTES {
            return Err(UiStateError::FieldTooLong {
                field: "approval_id",
            });
        }
        if !raw.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            return Err(UiStateError::InvalidField {
                field: "approval_id",
            });
        }
        Ok(Self(raw.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ApprovalKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AgentLifecycle {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "queued" => Self::Queued,
            "starting" => Self::Starting,
            "running" => Self::Running,
            "waiting_tool" => Self::WaitingTool,
            "waiting_approval" => Self::WaitingApproval,
            "paused" => Self::Paused,
            "blocked" => Self::Blocked,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

impl WorkerClass {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "unspecified" => Self::Unspecified,
            "persistent" => Self::Persistent,
            "managed" => Self::Managed,
            _ => return None,
        })
    }
}

impl JobLifecycle {
    /// Whether the job has stopped for good.
    ///
    /// A terminal job's captured output never changes again, so a reader of
    /// that spool — the `/jobs logs` view — knows it has nothing to re-read.
    /// Classified here rather than at the reader so the two cannot drift.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::TimedOut
                | Self::OrphanReconciled
        )
    }

    /// The wire vocabulary is `process_supervisor::JobState::as_str`'s, so
    /// one payload shape serves every producer of `job.*` events.
    ///
    /// A state this cannot parse is an `InvalidField` error, and `reduce`
    /// answers those by blocking every later action and raising a
    /// protocol-error modal — so a job merely *timing out* used to be enough
    /// to freeze the session, once anything actually reported one.
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "started" | "queued" | "running" | "sleeping" => Self::Started,
            "output" => Self::Output,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "timed_out" => Self::TimedOut,
            "orphaned" | "orphan_reconciled" => Self::OrphanReconciled,
            _ => return None,
        })
    }
}

impl ApprovalDecisionView {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "approved" => Self::Approved,
            "denied" => Self::Denied,
            _ => return None,
        })
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
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for UiStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("ui projection cancelled"),
            Self::TooManyEvents => f.write_str("ui projection exceeds the event bound"),
            Self::ActionsBlocked => f.write_str("ui actions are blocked after a protocol error"),
            Self::Kernel(err) => write!(f, "kernel projection: {err}"),
            Self::SessionMismatch { expected, found } => {
                write!(
                    f,
                    "event session {found} does not match projection {expected}"
                )
            }
            Self::MissingField { field } => write!(f, "event payload missing field {field}"),
            Self::InvalidField { field } => write!(f, "event payload has invalid field {field}"),
            Self::FieldTooLong { field } => write!(f, "event payload field {field} exceeds bound"),
            Self::AgentLimit => f.write_str("ui agent projection bound exceeded"),
            Self::GoalLimit => f.write_str("ui goal projection bound exceeded"),
            Self::JobLimit => f.write_str("ui job projection bound exceeded"),
            Self::ApprovalLimit => f.write_str("ui approval projection bound exceeded"),
            Self::ModalLimit => f.write_str("ui modal stack bound exceeded"),
        }
    }
}

impl Error for UiStateError {}

#[cfg(test)]
mod tests {
    use super::*;
    use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, RecordedAt};
    use kernel::ProjectionInvariant;
    use protocol::{EventId, ProjectId, TraceId};

    const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000010";
    const PROJECT_ID: &str = "019c0000-0000-7000-8000-000000000011";
    const TURN_ID: &str = "019c0000-0000-7000-8000-000000000012";
    const GOAL_ID: &str = "019c0000-0000-7000-8000-000000000014";
    const AGENT_ID: &str = "019c0000-0000-7000-8000-000000000015";
    const CHILD_ID: &str = "019c0000-0000-7000-8000-000000000018";
    const JOB_ID: &str = "019c0000-0000-7000-8000-000000000019";
    const APPROVAL_ID: &str = "019c0000-0000-7000-8000-00000000001a";
    const ACTOR_ID: &str = "019c0000-0000-7000-8000-000000000016";
    const TRACE_ID: &str = "8f000000-0000-7000-8000-000000000017";
    const CREATED_AT: &str = "2026-08-14T15:20:04.123Z";
    const UPDATED_AT: &str = "2026-08-14T15:21:00.000Z";

    fn envelope(seq: u64, kind: EventKind, at: &str, payload: Value) -> ErasedEventEnvelope {
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

    fn secret_envelope(seq: u64, kind: EventKind, payload: Value) -> ErasedEventEnvelope {
        EventEnvelope::new(
            event_id_for_seq(seq),
            SESSION_ID.parse().expect("session"),
            seq,
            UPDATED_AT.parse::<RecordedAt>().expect("recorded_at"),
            ActorRef::new(ActorKind::System, ACTOR_ID).expect("actor"),
            TRACE_ID.parse::<TraceId>().expect("trace"),
            kind,
            RedactionClass::Secret,
            payload,
        )
    }

    fn event_id_for_seq(seq: u64) -> EventId {
        format!("019c0000-0000-7000-8000-{seq:012x}")
            .parse()
            .expect("event id")
    }

    fn created() -> UiEvent {
        UiEvent::Kernel(envelope(
            1,
            EventKind::SessionCreated,
            CREATED_AT,
            serde_json::json!({"project_id": PROJECT_ID}),
        ))
    }

    fn fixture() -> Vec<UiEvent> {
        vec![
            created(),
            UiEvent::Kernel(envelope(
                2,
                EventKind::GoalCreated,
                UPDATED_AT,
                serde_json::json!({
                    "goal_id": GOAL_ID,
                    "statement": "ship the projection",
                    "budget": {"max_turns": 8, "max_tokens": 1000}
                }),
            )),
            UiEvent::Kernel(envelope(
                3,
                EventKind::TurnStarted,
                UPDATED_AT,
                serde_json::json!({"turn_id": TURN_ID}),
            )),
            UiEvent::Kernel(envelope(
                4,
                EventKind::AgentSpawned,
                UPDATED_AT,
                serde_json::json!({
                    "agent_id": AGENT_ID,
                    "role": "coder",
                    "stats": {"tokens": 12, "cost": 3}
                }),
            )),
            UiEvent::Kernel(envelope(
                5,
                EventKind::AgentSpawned,
                UPDATED_AT,
                serde_json::json!({
                    "agent_id": CHILD_ID,
                    "parent_id": AGENT_ID,
                    "state": "running"
                }),
            )),
            UiEvent::Kernel(envelope(
                6,
                EventKind::JobStarted,
                UPDATED_AT,
                serde_json::json!({"job_id": JOB_ID}),
            )),
            UiEvent::Kernel(envelope(
                7,
                EventKind::ApprovalRequested,
                UPDATED_AT,
                serde_json::json!({"approval_id": APPROVAL_ID}),
            )),
            UiEvent::Kernel(envelope(
                8,
                EventKind::JobCompleted,
                UPDATED_AT,
                serde_json::json!({"job_id": JOB_ID, "exit_status": 0}),
            )),
            UiEvent::Kernel(envelope(
                9,
                EventKind::ApprovalResolved,
                UPDATED_AT,
                serde_json::json!({"approval_id": APPROVAL_ID, "decision": "approved"}),
            )),
            UiEvent::Kernel(envelope(
                10,
                EventKind::TurnCompleted,
                UPDATED_AT,
                serde_json::json!({"turn_id": TURN_ID}),
            )),
            UiEvent::Kernel(envelope(
                11,
                EventKind::GoalPaused,
                UPDATED_AT,
                serde_json::json!({"goal_id": GOAL_ID}),
            )),
            UiEvent::Local(LocalUiEvent::SetRoute(UiRoute::Agents)),
            UiEvent::Local(LocalUiEvent::SelectAgent(Some(
                AGENT_ID.parse().expect("agent"),
            ))),
        ]
    }

    fn replay_ok(events: &[UiEvent]) -> AppState {
        replay(events, &CancellationToken::new()).expect("replay")
    }

    #[test]
    fn sync_goal_projects_host_goal_and_selects_it() {
        let projection = GoalProjection {
            id: GOAL_ID.parse().expect("goal"),
            statement: Some("ship feature X".to_owned()),
            lifecycle: GoalLifecycle::Active,
            stop_reason: None,
            max_turns: Some(10),
            max_tokens: Some(100_000),
            turns: 0,
            tokens: 0,
        };
        let state = reduce(
            AppState::new(),
            &UiEvent::Local(LocalUiEvent::SyncGoal(projection)),
        );
        assert!(state.goals().contains_key(&GOAL_ID.parse().expect("g")));
        assert_eq!(state.selected_goal(), Some(GOAL_ID.parse().expect("g")));
        assert_eq!(
            state.goals()[&GOAL_ID.parse().expect("g")].statement(),
            Some("ship feature X")
        );
    }

    #[test]
    fn clear_goal_drops_the_projection_and_deselects_it() {
        let projection = GoalProjection {
            id: GOAL_ID.parse().expect("goal"),
            statement: Some("cancel me".to_owned()),
            lifecycle: GoalLifecycle::Active,
            stop_reason: None,
            max_turns: None,
            max_tokens: None,
            turns: 0,
            tokens: 0,
        };
        let state = reduce(
            AppState::new(),
            &UiEvent::Local(LocalUiEvent::SyncGoal(projection)),
        );
        let goal_id: GoalId = GOAL_ID.parse().expect("g");
        assert!(state.goals().contains_key(&goal_id));

        let state = reduce(state, &UiEvent::Local(LocalUiEvent::ClearGoal(goal_id)));
        assert!(!state.goals().contains_key(&goal_id));
        assert_eq!(state.selected_goal(), None);
    }

    #[test]
    fn clear_goal_for_an_unknown_id_is_a_harmless_no_op() {
        let goal_id: GoalId = GOAL_ID.parse().expect("g");
        let state = reduce(
            AppState::new(),
            &UiEvent::Local(LocalUiEvent::ClearGoal(goal_id)),
        );
        assert!(state.goals().is_empty());
        assert_eq!(state.selected_goal(), None);
    }

    #[test]
    fn append_command_output_and_error_push_distinct_transcript_kinds() {
        let state = reduce(
            AppState::new(),
            &UiEvent::Local(LocalUiEvent::AppendCommandOutput(
                "goal started: x".to_owned(),
            )),
        );
        let state = reduce(
            state,
            &UiEvent::Local(LocalUiEvent::AppendCommandError(
                "unknown command".to_owned(),
            )),
        );
        assert_eq!(
            state.transcript(),
            &[
                TranscriptEntry::CommandOutput {
                    text: "goal started: x".to_owned()
                },
                TranscriptEntry::CommandError {
                    text: "unknown command".to_owned()
                },
            ]
        );
    }

    #[test]
    fn replay_fixture_is_deterministic() {
        let events = fixture();
        let first = replay_ok(&events);
        let second = replay_ok(&events);
        let folded = events.iter().fold(AppState::new(), reduce);
        let a = serde_json::to_vec(&first).expect("bytes a");
        let b = serde_json::to_vec(&second).expect("bytes b");
        let c = serde_json::to_vec(&folded).expect("bytes c");
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(first, second);
        assert_eq!(first, folded);
        assert_eq!(first.cached_projection_version(), 11);
        assert_eq!(first.session_status(), Some(SessionStatus::Paused));
    }

    #[test]
    fn domain_rows_are_keyed_by_stable_ids() {
        let state = replay_ok(&fixture());
        let agent: AgentId = AGENT_ID.parse().expect("agent");
        let child: AgentId = CHILD_ID.parse().expect("child");
        let goal: GoalId = GOAL_ID.parse().expect("goal");
        let job: JobId = JOB_ID.parse().expect("job");
        let approval = ApprovalKey::parse(APPROVAL_ID).expect("approval");

        assert!(state.agents().contains_key(&agent));
        assert_eq!(state.agents()[&child].parent_id(), Some(agent));
        assert_eq!(state.agents()[&child].state(), AgentLifecycle::Running);
        assert_eq!(state.agents()[&agent].tokens(), 12);
        assert_eq!(state.goals()[&goal].lifecycle(), GoalLifecycle::Paused);
        assert_eq!(state.jobs()[&job].state(), JobLifecycle::Completed);
        assert_eq!(state.jobs()[&job].exit_status(), Some(0));
        assert_eq!(
            state.approvals()[&approval].state(),
            ApprovalLifecycle::Resolved
        );
        assert_eq!(
            state.approvals()[&approval].decision(),
            Some(ApprovalDecisionView::Approved)
        );
        assert!(state.modal_stack().is_empty());
        assert_eq!(state.route(), UiRoute::Agents);
        assert_eq!(state.selected_agent(), Some(agent));
    }

    #[test]
    fn local_events_cannot_insert_domain_rows() {
        let before = replay_ok(&[created()]);
        let after = reduce(
            before.clone(),
            &UiEvent::Local(LocalUiEvent::SelectAgent(Some(
                AGENT_ID.parse().expect("agent"),
            ))),
        );
        assert!(after.agents().is_empty());
        assert!(after.goals().is_empty());
        assert!(after.jobs().is_empty());
        assert!(after.approvals().is_empty());
        assert_eq!(after.selected_agent().unwrap().to_string(), AGENT_ID);
        assert_eq!(after.snapshot(), before.snapshot());
    }

    #[test]
    fn snapshot_then_later_events_match_full_replay() {
        let events = fixture();
        let through_turn = replay_ok(&events[..3]);
        let snapshot = through_turn.snapshot().expect("snapshot").clone();
        let mut resumed = vec![UiEvent::Snapshot(snapshot)];
        resumed.extend(events[3..].iter().cloned());
        let from_snapshot = replay_ok(&resumed);
        let full = replay_ok(&events);
        assert_eq!(from_snapshot.agents(), full.agents());
        assert_eq!(from_snapshot.goals(), full.goals());
        assert_eq!(from_snapshot.jobs(), full.jobs());
        assert_eq!(from_snapshot.approvals(), full.approvals());
        assert_eq!(
            from_snapshot.cached_projection_version(),
            full.cached_projection_version()
        );
    }

    #[test]
    fn missing_stable_id_is_typed_and_blocks_actions() {
        let events = vec![
            created(),
            UiEvent::Kernel(envelope(
                2,
                EventKind::ApprovalRequested,
                UPDATED_AT,
                serde_json::json!({}),
            )),
        ];
        assert!(matches!(
            replay(&events, &CancellationToken::new()),
            Err(UiStateError::MissingField {
                field: "approval_id"
            })
        ));
        let blocked = events.iter().fold(AppState::new(), reduce);
        assert!(blocked.actions_blocked());
        assert!(blocked.protocol_error().is_some());
        assert!(
            blocked
                .modal_stack()
                .iter()
                .any(|modal| matches!(modal, Modal::ProtocolError))
        );
        let ignored = reduce(
            blocked.clone(),
            &UiEvent::Kernel(envelope(
                2,
                EventKind::JobStarted,
                UPDATED_AT,
                serde_json::json!({"job_id": JOB_ID}),
            )),
        );
        assert!(ignored.jobs().is_empty());
        assert_eq!(ignored.protocol_error(), blocked.protocol_error());
    }

    #[test]
    fn kernel_seq_gap_is_not_applied() {
        let state = replay_ok(&[created()]);
        let gapped = reduce(
            state.clone(),
            &UiEvent::Kernel(envelope(
                3,
                EventKind::TurnStarted,
                UPDATED_AT,
                serde_json::json!({"turn_id": TURN_ID}),
            )),
        );
        assert!(gapped.actions_blocked());
        assert_eq!(gapped.cached_projection_version(), 1);
        assert!(gapped.active_turn().is_none());
        assert!(matches!(
            replay(
                &[
                    created(),
                    UiEvent::Kernel(envelope(
                        3,
                        EventKind::TurnStarted,
                        UPDATED_AT,
                        serde_json::json!({"turn_id": TURN_ID}),
                    )),
                ],
                &CancellationToken::new()
            ),
            Err(UiStateError::Kernel(ProjectionError::Invariant(
                ProjectionInvariant::SeqGap {
                    expected: 2,
                    found: 3
                }
            )))
        ));
    }

    #[test]
    fn secret_payload_text_is_not_copied() {
        let state = replay_ok(&[
            created(),
            UiEvent::Kernel(secret_envelope(
                2,
                EventKind::GoalCreated,
                serde_json::json!({
                    "goal_id": GOAL_ID,
                    "statement": "do not project this secret"
                }),
            )),
        ]);
        let goal: GoalId = GOAL_ID.parse().expect("goal");
        assert!(state.goals()[&goal].statement().is_none());
        assert_eq!(state.goals()[&goal].lifecycle(), GoalLifecycle::Active);
    }

    #[test]
    fn replay_honours_cancellation_and_event_bound() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            replay(&fixture(), &cancel),
            Err(UiStateError::Cancelled)
        ));
    }

    #[test]
    fn completed_goal_stays_keyed_after_kernel_clears_snapshot() {
        let state = replay_ok(&[
            created(),
            UiEvent::Kernel(envelope(
                2,
                EventKind::GoalCreated,
                UPDATED_AT,
                serde_json::json!({"goal_id": GOAL_ID, "statement": "done"}),
            )),
            UiEvent::Kernel(envelope(
                3,
                EventKind::GoalCompleted,
                UPDATED_AT,
                serde_json::json!({"goal_id": GOAL_ID}),
            )),
        ]);
        let goal: GoalId = GOAL_ID.parse().expect("goal");
        assert!(state.snapshot().unwrap().top_level_goal().is_none());
        assert_eq!(state.goals()[&goal].lifecycle(), GoalLifecycle::Completed);
        assert_eq!(
            state.goals()[&goal].stop_reason(),
            Some(GoalStopReason::Completed)
        );
    }

    #[test]
    fn approval_modal_is_keyed_and_removed_on_resolve() {
        let requested = replay_ok(&[
            created(),
            UiEvent::Kernel(envelope(
                2,
                EventKind::ApprovalRequested,
                UPDATED_AT,
                serde_json::json!({"approval_id": APPROVAL_ID}),
            )),
        ]);
        let key = ApprovalKey::parse(APPROVAL_ID).expect("approval");
        assert_eq!(
            requested.modal_stack(),
            &[Modal::Approval { id: key.clone() }]
        );
        let resolved = reduce(
            requested,
            &UiEvent::Kernel(envelope(
                3,
                EventKind::ApprovalResolved,
                UPDATED_AT,
                serde_json::json!({"approval_id": APPROVAL_ID, "decision": "denied"}),
            )),
        );
        assert!(resolved.modal_stack().is_empty());
        assert_eq!(
            resolved.approvals()[&key].decision(),
            Some(ApprovalDecisionView::Denied)
        );
    }

    #[test]
    fn composer_bound_is_typed() {
        let too_long = "x".repeat(MAX_COMPOSER_BYTES + 1);
        assert!(matches!(
            replay(
                &[UiEvent::Local(LocalUiEvent::SetComposerText(too_long))],
                &CancellationToken::new()
            ),
            Err(UiStateError::FieldTooLong { field: "composer" })
        ));
    }

    #[test]
    fn snapshot_project_id_round_trips() {
        let state = replay_ok(&[created()]);
        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.id().to_string(), SESSION_ID);
        assert_eq!(
            snapshot.project_id(),
            PROJECT_ID.parse::<ProjectId>().expect("project")
        );
        assert_eq!(state.cached_projection_version(), 1);
    }
}
