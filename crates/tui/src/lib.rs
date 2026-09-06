#![forbid(unsafe_code)]

pub mod color;
pub mod commands;
pub mod compositor;
pub mod composer;
pub mod layout;
pub mod panels {
    pub mod agents;
    pub mod goals;
    pub mod approval;
    pub mod context;
    pub mod control_room;
    pub mod diff;
    pub mod memory;
    pub mod model;
    pub mod runtime_views;
    pub mod trace_jobs;
}

pub use panels::runtime_views::{
    CheckpointMarker, ComputerActionState, ComputerViewModel, ControlOwner, GraphNodeRow,
    GraphViewModel, MAX_ROW_CHARS, MAX_VIEW_ROWS, OwnershipIndicator, ResourceRow,
    ResourcesViewModel, TimelineViewModel,
};
pub mod sanitize;
pub mod session_actions;
pub mod state;
pub mod status;
pub mod terminal;
pub mod transcript;

pub use commands::{
    CommandError, FrontendAction, HandoffDest, InlineHelp, Inspector, KernelAction, KernelApi,
    LocalAction, PaletteEntry, TakeoverSurface, UiCommand, dispatch, parse_command, suggest,
};
pub use composer::{
    ComposerCommand, ComposerError, ComposerEvent, ComposerModel, ComposerView, Motion,
};
pub use compositor::{Screen, compute_screen_layout, paint_screen, sidebar_lines, ui_mode_for};
pub use layout::{LayoutRects, Rect, UiMode, compute_layout, compute_layout_with_composer};
pub use panels::agents::{
    AgentCancelIntent, AgentHandoffObservation, AgentInspectIntent, AgentMergeIntent,
    AgentMergeState, AgentRowView, AgentsFrame, AgentsPanelError, AgentsSelection, AgentsViewModel,
    MAX_AGENT_TEXT_BYTES, MAX_AGENTS, MAX_AGENTS_COLS, MAX_AGENTS_ROWS,
};
pub use panels::control_room::{
    ControlRoomError, ControlRoomFrame, ControlRoomViewModel, MAX_PHASES, MAX_ROOM_COLS,
    MAX_ROOM_ROWS, MAX_STREAMS, RoomGoal, RoomPhase, RoomPhaseState, RoomStream, goal_row,
    phase_row, stream_row,
};
pub use panels::approval::{
    ApprovalActionSpec, ApprovalClock, ApprovalFrame, ApprovalLeaseFields, ApprovalModalChoice,
    ApprovalPolicySpec, ApprovalPrompt, ApprovalRiskSpec, ApprovalScopeId, ApprovalScopeSpec,
    ApprovalSubmitIntent, ApprovalUiError, ApprovalViewModel, CapabilityView, CommandModeView,
    CommandPreviewInput, ListedScope, MAX_ACTION_TEXT_BYTES, MAX_APPROVAL_COLS, MAX_APPROVAL_ROWS,
    MAX_ARGV, MAX_ENV_NAMES, MAX_REASON_BYTES, MAX_SCOPES, PolicyLayerView, RiskClassView,
    ScopeKindView,
};
pub use panels::context::{
    ContextBlockView, ContextFrame, ContextGroup, ContextInspectError, ContextPinIntent,
    ContextSelection, ContextUnpinIntent, ContextViewModel, MAX_CONTEXT_BLOCKS, MAX_CONTEXT_COLS,
    MAX_CONTEXT_ROWS,
};
pub use panels::diff::{
    DegradeReason, DiffAttribution, DiffBodyKind, DiffError, DiffFile, DiffFileKind, DiffFrame,
    DiffLineKind, DiffRenderMode, DiffSelection, DiffViewModel, DiffWarningKind, MAX_DIFF_FILES,
    MAX_INLINE_DIFF_BYTES, MAX_INLINE_DIFF_LINES, SemanticOpKind,
};
pub use panels::memory::{
    MAX_MEMORY_COLS, MAX_MEMORY_ID_BYTES, MAX_MEMORY_ITEMS, MAX_MEMORY_ROWS, MemoryDeleteIntent,
    MemoryDisableWritesIntent, MemoryFrame, MemoryInspectError, MemoryObservation, MemoryRowView,
    MemorySelection, MemoryViewModel,
};
pub use panels::model::{
    MAX_MODEL_COLS, MAX_MODEL_ITEMS, MAX_MODEL_ROWS, ModelAvailability, ModelFrame, ModelPinIntent,
    ModelRowView, ModelSelectError, ModelSelection, ModelUnpinIntent, ModelViewModel,
};
pub use panels::trace_jobs::{
    ArtifactCursor, JobCancelIntent, JobClass, JobObservation, JobRowView, LogPage, LogViewIntent,
    MAX_INLINE_LOG_BYTES, MAX_LOG_PAGE_LINES, MAX_TRACE_JOBS, MAX_TRACE_JOBS_COLS,
    MAX_TRACE_JOBS_ROWS, MAX_TRACE_SPANS, SpanObservation, SpanRowView, SpanStatus, TraceJobsFrame,
    TraceJobsInspectError, TraceJobsSelection, TraceJobsTab, TraceJobsViewModel,
};
pub use sanitize::sanitize_untrusted;
pub use session_actions::{
    CompactObservation, CompactSessionIntent, ForkSessionIntent, GoalSafetyView,
    MAX_GOAL_PREVIEW_BYTES, MAX_REWIND_CONFLICTS, MAX_REWIND_OPS, MAX_SESSION_ACTION_COLS,
    MAX_SESSION_ACTION_ROWS, ResumeSessionIntent, RewindConflictView, RewindObservation,
    RewindOpView, RewindSessionIntent, SessionActionError, SessionActionFrame,
    SessionActionsViewModel, SessionLifecycleIntent, SessionLifecycleKind,
    SessionLifecycleObservation, SessionLifecyclePreview, SessionLifecycleRequest,
};
pub use state::{
    AppState, CancellationToken, LocalUiEvent, UiEvent, UiState, UiStateError, reduce, replay,
};
pub use status::{
    Connectivity, ContextUsage, PolicyMode, SandboxMode, StatusChrome, StatusItemKind, StatusLine,
    StatusSnapshot, render_status, render_status_with,
};
pub use terminal::{
    CrosstermBackend, FrontendKind, RecordingBackend, TerminalBackend, TerminalError,
    TerminalGuard, TerminalOp, restore_if_armed,
};
pub use transcript::{
    BlockId, FrameWork, RenderBlock, RenderBlockKind, ScrollAnchor, Transcript, TranscriptError,
    TranscriptViewport, VisibleRow, VisibleWindow, render_block_parts,
};
