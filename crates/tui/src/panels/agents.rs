//! Agents panel over the kernel agent projection.
//!
//! [`AgentsViewModel`] is a frontend projection. It never schedules, cancels,
//! inspects, or merges. Cancel/inspect/merge return typed intents for the
//! kernel client. Untrusted action/blocker text is sanitized. Hierarchy is
//! keyed by immutable parent IDs so streaming stats cannot reshuffle the tree.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use agent_runtime::{InspectedResult, MergeStatus, PatchSummary};
use protocol::{AgentId, ArtifactRef, EvidenceId, WorkspaceViewId};

use crate::sanitize::sanitize_untrusted;
use crate::state::{
    AgentLifecycle, AgentProjection, AppState, CancellationToken, ControlHolder,
    MAX_PROJECTED_AGENTS, WorkerClass,
};

/// Maximum projected agent rows retained in one panel.
pub const MAX_AGENTS: usize = MAX_PROJECTED_AGENTS;

/// Render width is clamped to this many columns.
pub const MAX_AGENTS_COLS: u16 = 512;

/// Render height is clamped to this many rows.
pub const MAX_AGENTS_ROWS: u16 = 256;

/// Maximum UTF-8 bytes retained for one action or blocker preview.
pub const MAX_AGENT_TEXT_BYTES: usize = 128;

const CANCEL_STRIDE: usize = 8;
const MAX_PREVIEW_CHARS: usize = 24;
const TAKEOVER_LABEL: &str = "TAKEOVER";

/// Local row cursor. Not domain state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct AgentsSelection {
    row_index: usize,
}

/// Last explicit child merge observation. Never inferred from summary text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AgentMergeState {
    Pending,
    Committed,
    Conflict { conflicts: usize },
}

/// Typed panel failure. Display never echoes IDs, actions, or blockers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AgentsPanelError {
    Cancelled,
    BoundExceeded,
    InvalidSelection,
    InvalidField,
    ActionsBlocked,
    AlreadyTerminal,
    AlreadyMerged,
    MissingParent,
    MissingWriteView,
}

/// Kernel-bound cancel request. The panel does not apply it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct AgentCancelIntent {
    agent_id: AgentId,
}

/// Kernel-bound inspect request. The panel does not load the result.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct AgentInspectIntent {
    agent_id: AgentId,
}

/// Kernel-bound merge request. The panel does not apply a change-set.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct AgentMergeIntent {
    child: AgentId,
    parent: AgentId,
    child_view: WorkspaceViewId,
}

/// Observation copied from a stored child result / merge handoff.
///
/// This is not a store. Patch and merge status are references only.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct AgentHandoffObservation {
    agent_id: AgentId,
    patch_summary: Option<PatchSummary>,
    merge: Option<AgentMergeState>,
}

/// One projected tree row. Secret plaintext is never stored here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRowView {
    id: AgentId,
    parent_id: Option<AgentId>,
    depth: usize,
    display_id: String,
    role: Option<String>,
    worker_class: WorkerClass,
    state: AgentLifecycle,
    action: String,
    elapsed_ms: u64,
    tokens: u64,
    cost: u64,
    workspace_view_id: Option<WorkspaceViewId>,
    display_view: String,
    write_view: bool,
    patch_summary: Option<PatchSummary>,
    merge: Option<AgentMergeState>,
    blocker: String,
    evidence: Option<EvidenceId>,
    display_evidence: String,
    last_artifact: Option<ArtifactRef>,
    takeover: bool,
}

/// Frontend-only projection. Never cancels, inspects, or merges.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentsViewModel {
    rows: Vec<AgentRowView>,
    selection: AgentsSelection,
    control_holder: ControlHolder,
    actions_blocked: bool,
}

/// One painted frame. `golden` omits trailing pad.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentsFrame {
    width: u16,
    height: u16,
    lines: Vec<String>,
}

impl AgentsSelection {
    pub const fn new(row_index: usize) -> Self {
        Self { row_index }
    }

    pub const fn row_index(self) -> usize {
        self.row_index
    }
}

impl AgentMergeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Committed => "committed",
            Self::Conflict { .. } => "conflict",
        }
    }

    pub const fn is_conflict(self) -> bool {
        matches!(self, Self::Conflict { .. })
    }

    pub const fn is_committed(self) -> bool {
        matches!(self, Self::Committed)
    }
}

impl AgentsPanelError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "agents panel cancelled",
            Self::BoundExceeded => "agents panel resource bound exceeded",
            Self::InvalidSelection => "agents selection is out of range",
            Self::InvalidField => "agents observation field is invalid",
            Self::ActionsBlocked => "agents actions are blocked after a protocol error",
            Self::AlreadyTerminal => "agent is already terminal",
            Self::AlreadyMerged => "agent result is already merged",
            Self::MissingParent => "agent has no parent for merge",
            Self::MissingWriteView => "agent has no write workspace view",
        }
    }
}

impl AgentCancelIntent {
    pub const fn agent_id(self) -> AgentId {
        self.agent_id
    }

    /// Cancel is keyed by the durable agent identity, never a live task.
    pub const fn targets_stable_agent_id(self) -> bool {
        true
    }

    /// Cancel must enter the kernel interrupt path, not a local kill.
    pub const fn requires_kernel_interrupt(self) -> bool {
        true
    }
}

impl AgentInspectIntent {
    pub const fn agent_id(self) -> AgentId {
        self.agent_id
    }

    /// Inspect loads the stored typed result; it is not a transcript clone.
    pub const fn uses_stored_result(self) -> bool {
        true
    }
}

impl AgentMergeIntent {
    pub const fn child(self) -> AgentId {
        self.child
    }

    pub const fn parent(self) -> AgentId {
        self.parent
    }

    pub const fn child_view(self) -> WorkspaceViewId {
        self.child_view
    }

    /// Merge must enter the workspace transaction path, not this view.
    pub const fn requires_workspace_transaction(self) -> bool {
        true
    }
}

impl AgentHandoffObservation {
    pub const fn new(
        agent_id: AgentId,
        patch_summary: Option<PatchSummary>,
        merge: Option<AgentMergeState>,
    ) -> Self {
        Self {
            agent_id,
            patch_summary,
            merge,
        }
    }

    /// Copy a parent-visible stored result. Summary text is ignored.
    pub fn from_inspected(inspected: &InspectedResult) -> Self {
        let merge = match inspected.merge() {
            None => Some(AgentMergeState::Pending),
            Some(MergeStatus::Committed { .. }) => Some(AgentMergeState::Committed),
            Some(MergeStatus::Conflict { conflicts, .. }) => Some(AgentMergeState::Conflict {
                conflicts: *conflicts,
            }),
        };
        Self {
            agent_id: inspected.result().agent_id(),
            patch_summary: inspected.result().patch_summary(),
            merge,
        }
    }

    pub const fn agent_id(self) -> AgentId {
        self.agent_id
    }

    pub const fn patch_summary(self) -> Option<PatchSummary> {
        self.patch_summary
    }

    pub const fn merge(self) -> Option<AgentMergeState> {
        self.merge
    }
}

impl AgentRowView {
    pub const fn id(&self) -> AgentId {
        self.id
    }

    pub const fn parent_id(&self) -> Option<AgentId> {
        self.parent_id
    }

    pub const fn depth(&self) -> usize {
        self.depth
    }

    pub fn display_id(&self) -> &str {
        &self.display_id
    }

    pub fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }

    pub const fn worker_class(&self) -> WorkerClass {
        self.worker_class
    }

    pub const fn state(&self) -> AgentLifecycle {
        self.state
    }

    pub fn action(&self) -> &str {
        &self.action
    }

    pub const fn elapsed_ms(&self) -> u64 {
        self.elapsed_ms
    }

    pub const fn tokens(&self) -> u64 {
        self.tokens
    }

    pub const fn cost(&self) -> u64 {
        self.cost
    }

    pub const fn workspace_view_id(&self) -> Option<WorkspaceViewId> {
        self.workspace_view_id
    }

    pub fn display_view(&self) -> &str {
        &self.display_view
    }

    pub const fn has_write_view(&self) -> bool {
        self.write_view
    }

    pub const fn patch_summary(&self) -> Option<PatchSummary> {
        self.patch_summary
    }

    pub const fn merge(&self) -> Option<AgentMergeState> {
        self.merge
    }

    pub fn blocker(&self) -> &str {
        &self.blocker
    }

    pub const fn evidence(&self) -> Option<EvidenceId> {
        self.evidence
    }

    pub fn display_evidence(&self) -> &str {
        &self.display_evidence
    }

    pub fn last_artifact(&self) -> Option<&ArtifactRef> {
        self.last_artifact.as_ref()
    }

    pub const fn is_takeover(&self) -> bool {
        self.takeover
    }

    pub const fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            AgentLifecycle::Succeeded | AgentLifecycle::Failed | AgentLifecycle::Cancelled
        )
    }
}

impl AgentsViewModel {
    /// Project `AppState.agents` plus optional merge/diff observations.
    pub fn from_state(
        state: &AppState,
        handoffs: &[AgentHandoffObservation],
        selection: AgentsSelection,
        cancel: &CancellationToken,
    ) -> Result<Self, AgentsPanelError> {
        check_cancel(cancel)?;
        if state.agents().len() > MAX_AGENTS || handoffs.len() > MAX_AGENTS {
            return Err(AgentsPanelError::BoundExceeded);
        }

        let mut handoff_by_id = BTreeMap::new();
        for (index, handoff) in handoffs.iter().enumerate() {
            if index.is_multiple_of(CANCEL_STRIDE) {
                check_cancel(cancel)?;
            }
            handoff_by_id.insert(handoff.agent_id, *handoff);
        }

        let order = walk_tree(state.agents())?;
        let mut rows = Vec::with_capacity(order.len());
        for (index, (id, depth)) in order.iter().copied().enumerate() {
            if index.is_multiple_of(CANCEL_STRIDE) {
                check_cancel(cancel)?;
            }
            let agent = state
                .agents()
                .get(&id)
                .ok_or(AgentsPanelError::InvalidField)?;
            rows.push(project_row(
                agent,
                depth,
                handoff_by_id.get(&id).copied(),
                state.control_holder(),
            ));
        }

        let row_index = resolve_selection(&rows, state.selected_agent(), selection);
        Ok(Self {
            rows,
            selection: AgentsSelection { row_index },
            control_holder: state.control_holder(),
            actions_blocked: state.actions_blocked(),
        })
    }

    pub fn rows(&self) -> &[AgentRowView] {
        &self.rows
    }

    pub fn selected_row(&self) -> Option<&AgentRowView> {
        self.rows.get(self.selection.row_index)
    }

    pub fn selection(&self) -> AgentsSelection {
        self.selection
    }

    pub const fn control_holder(&self) -> ControlHolder {
        self.control_holder
    }

    pub const fn actions_blocked(&self) -> bool {
        self.actions_blocked
    }

    /// This panel never schedules or mutates agent lifecycle.
    pub const fn mutates_runtime(&self) -> bool {
        false
    }

    pub fn select_row(&self, index: usize) -> Result<Self, AgentsPanelError> {
        if self.rows.is_empty() {
            if index == 0 {
                return Ok(self.clone());
            }
            return Err(AgentsPanelError::InvalidSelection);
        }
        if index >= self.rows.len() {
            return Err(AgentsPanelError::InvalidSelection);
        }
        let mut next = self.clone();
        next.selection.row_index = index;
        Ok(next)
    }

    pub fn select_agent(&self, id: AgentId) -> Result<Self, AgentsPanelError> {
        let index = self
            .rows
            .iter()
            .position(|row| row.id == id)
            .ok_or(AgentsPanelError::InvalidSelection)?;
        self.select_row(index)
    }

    pub fn select_next(&self) -> Self {
        let mut next = self.clone();
        if !self.rows.is_empty() && self.selection.row_index + 1 < self.rows.len() {
            next.selection.row_index += 1;
        }
        next
    }

    pub fn select_prev(&self) -> Self {
        let mut next = self.clone();
        next.selection.row_index = self.selection.row_index.saturating_sub(1);
        next
    }

    /// Request cancel. The view is not mutated.
    pub fn cancel(
        &self,
        cancel: &CancellationToken,
    ) -> Result<AgentCancelIntent, AgentsPanelError> {
        check_cancel(cancel)?;
        self.guard_actions()?;
        let row = self
            .selected_row()
            .ok_or(AgentsPanelError::InvalidSelection)?;
        if row.is_terminal() {
            return Err(AgentsPanelError::AlreadyTerminal);
        }
        Ok(AgentCancelIntent { agent_id: row.id })
    }

    pub fn cancel_agent(
        &self,
        id: AgentId,
        cancel: &CancellationToken,
    ) -> Result<AgentCancelIntent, AgentsPanelError> {
        self.select_agent(id)?.cancel(cancel)
    }

    /// Request inspect of the stored typed result. The view is not mutated.
    pub fn inspect(
        &self,
        cancel: &CancellationToken,
    ) -> Result<AgentInspectIntent, AgentsPanelError> {
        check_cancel(cancel)?;
        let row = self
            .selected_row()
            .ok_or(AgentsPanelError::InvalidSelection)?;
        Ok(AgentInspectIntent { agent_id: row.id })
    }

    pub fn inspect_agent(
        &self,
        id: AgentId,
        cancel: &CancellationToken,
    ) -> Result<AgentInspectIntent, AgentsPanelError> {
        self.select_agent(id)?.inspect(cancel)
    }

    /// Request an explicit child→parent merge. The view is not mutated.
    pub fn merge(&self, cancel: &CancellationToken) -> Result<AgentMergeIntent, AgentsPanelError> {
        check_cancel(cancel)?;
        self.guard_actions()?;
        let row = self
            .selected_row()
            .ok_or(AgentsPanelError::InvalidSelection)?;
        merge_intent(row)
    }

    pub fn merge_agent(
        &self,
        id: AgentId,
        cancel: &CancellationToken,
    ) -> Result<AgentMergeIntent, AgentsPanelError> {
        self.select_agent(id)?.merge(cancel)
    }

    pub fn render(&self, width: u16, height: u16) -> AgentsFrame {
        let width = width.min(MAX_AGENTS_COLS);
        let height = height.min(MAX_AGENTS_ROWS);
        if width == 0 || height == 0 {
            return AgentsFrame {
                width,
                height,
                lines: Vec::new(),
            };
        }
        let mut header = format!("agents control:{}", control_label(self.control_holder));
        if self.actions_blocked {
            header.push_str(" blocked");
        }
        let mut lines = vec![header];
        if self.rows.is_empty() {
            lines.push("  (empty)".to_owned());
        } else {
            for (index, row) in self.rows.iter().enumerate() {
                let marker = if index == self.selection.row_index {
                    '>'
                } else {
                    ' '
                };
                lines.push(format_tree_row(row, marker));
            }
            if let Some(row) = self.selected_row() {
                lines.extend(format_selected(row, self.control_holder));
            }
        }
        if lines.len() > usize::from(height) {
            lines.truncate(usize::from(height));
        }
        AgentsFrame {
            width,
            height,
            lines,
        }
    }

    fn guard_actions(&self) -> Result<(), AgentsPanelError> {
        if self.actions_blocked {
            Err(AgentsPanelError::ActionsBlocked)
        } else {
            Ok(())
        }
    }
}

impl AgentsFrame {
    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
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

impl Display for AgentsPanelError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for AgentsPanelError {}

fn project_row(
    agent: &AgentProjection,
    depth: usize,
    handoff: Option<AgentHandoffObservation>,
    control: ControlHolder,
) -> AgentRowView {
    let write_view = agent.workspace_view_id().is_some();
    let takeover = is_takeover(agent.state(), control);
    let display_view = match agent.workspace_view_id() {
        Some(id) => short_id(&id.to_string()),
        None => "-".to_owned(),
    };
    let display_evidence = match agent.last_evidence() {
        Some(id) => short_id(&id.to_string()),
        None => "-".to_owned(),
    };
    AgentRowView {
        id: agent.id(),
        parent_id: agent.parent_id(),
        depth,
        display_id: short_id(&agent.id().to_string()),
        role: agent.role().map(sanitize_preview),
        worker_class: agent.worker_class(),
        state: agent.state(),
        action: match agent.current_operation() {
            Some(op) => sanitize_preview(op),
            None => "-".to_owned(),
        },
        elapsed_ms: agent.active_ms(),
        tokens: agent.tokens(),
        cost: agent.cost(),
        workspace_view_id: agent.workspace_view_id(),
        display_view,
        write_view,
        patch_summary: handoff.and_then(AgentHandoffObservation::patch_summary),
        merge: handoff.and_then(AgentHandoffObservation::merge),
        blocker: match agent.blocker() {
            Some(blocker) => sanitize_preview(blocker),
            None => "-".to_owned(),
        },
        evidence: agent.last_evidence(),
        display_evidence,
        last_artifact: agent.last_artifact().cloned(),
        takeover,
    }
}

fn walk_tree(
    agents: &BTreeMap<AgentId, AgentProjection>,
) -> Result<Vec<(AgentId, usize)>, AgentsPanelError> {
    let mut children: BTreeMap<AgentId, Vec<AgentId>> = BTreeMap::new();
    let mut roots = Vec::new();
    for (id, agent) in agents {
        match agent.parent_id() {
            Some(parent) if agents.contains_key(&parent) => {
                children.entry(parent).or_default().push(*id);
            }
            _ => roots.push(*id),
        }
    }

    let mut out = Vec::with_capacity(agents.len());
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for root in roots {
        walk_node(root, 0, &children, &mut visiting, &mut visited, &mut out)?;
    }
    if visited.len() != agents.len() {
        return Err(AgentsPanelError::InvalidField);
    }
    Ok(out)
}

fn walk_node(
    id: AgentId,
    depth: usize,
    children: &BTreeMap<AgentId, Vec<AgentId>>,
    visiting: &mut BTreeSet<AgentId>,
    visited: &mut BTreeSet<AgentId>,
    out: &mut Vec<(AgentId, usize)>,
) -> Result<(), AgentsPanelError> {
    if visited.contains(&id) {
        return Ok(());
    }
    if !visiting.insert(id) {
        return Err(AgentsPanelError::InvalidField);
    }
    out.push((id, depth));
    if let Some(kids) = children.get(&id) {
        for child in kids {
            walk_node(*child, depth + 1, children, visiting, visited, out)?;
        }
    }
    visiting.remove(&id);
    visited.insert(id);
    Ok(())
}

fn resolve_selection(
    rows: &[AgentRowView],
    selected: Option<AgentId>,
    selection: AgentsSelection,
) -> usize {
    if rows.is_empty() {
        return 0;
    }
    if let Some(id) = selected
        && let Some(index) = rows.iter().position(|row| row.id == id)
    {
        return index;
    }
    selection.row_index.min(rows.len() - 1)
}

fn merge_intent(row: &AgentRowView) -> Result<AgentMergeIntent, AgentsPanelError> {
    if matches!(row.merge, Some(AgentMergeState::Committed)) {
        return Err(AgentsPanelError::AlreadyMerged);
    }
    let parent = row.parent_id.ok_or(AgentsPanelError::MissingParent)?;
    let child_view = row
        .workspace_view_id
        .ok_or(AgentsPanelError::MissingWriteView)?;
    Ok(AgentMergeIntent {
        child: row.id,
        parent,
        child_view,
    })
}

fn format_tree_row(row: &AgentRowView, marker: char) -> String {
    let indent = "  ".repeat(row.depth);
    let mut line = format!(
        "{indent}{marker} {} {} {}",
        row.display_id,
        worker_label(row.worker_class),
        lifecycle_label(row.state)
    );
    if row.write_view {
        line.push_str(" write");
    }
    if let Some(merge) = row.merge {
        line.push(' ');
        line.push_str(&format_merge(merge));
    }
    if row.takeover {
        line.push(' ');
        line.push_str(TAKEOVER_LABEL);
    }
    line
}

fn format_selected(row: &AgentRowView, control: ControlHolder) -> Vec<String> {
    let mut lines = vec![
        format!("selected:{}", row.display_id),
        format!(
            "  state:{} action:{} elapsed:{}ms tokens:{} cost:{}",
            lifecycle_label(row.state),
            row.action,
            row.elapsed_ms,
            row.tokens,
            row.cost
        ),
        format!(
            "  view:{}{}",
            row.display_view,
            if row.write_view { " write" } else { "" }
        ),
        format!("  diff:{}", format_diff(row.patch_summary)),
        format!("  blocker:{}", row.blocker),
        format!("  evidence:{}", row.display_evidence),
        format!(
            "  {}",
            row.merge
                .map(format_merge)
                .unwrap_or_else(|| "merge:-".to_owned())
        ),
    ];
    if row.takeover {
        lines.push(format!(
            "  control:{} {TAKEOVER_LABEL}",
            control_label(control)
        ));
    }
    lines
}

fn format_merge(merge: AgentMergeState) -> String {
    match merge {
        AgentMergeState::Pending => "merge:pending".to_owned(),
        AgentMergeState::Committed => "merge:committed".to_owned(),
        AgentMergeState::Conflict { conflicts } => format!("merge:conflict:{conflicts}"),
    }
}

fn format_diff(patch: Option<PatchSummary>) -> String {
    match patch {
        None => "-".to_owned(),
        Some(patch) => format!(
            "+{}/-{} files:{}",
            patch.additions(),
            patch.deletions(),
            patch.files_changed()
        ),
    }
}

fn lifecycle_label(state: AgentLifecycle) -> &'static str {
    match state {
        AgentLifecycle::Queued => "queued",
        AgentLifecycle::Starting => "starting",
        AgentLifecycle::Running => "running",
        AgentLifecycle::WaitingTool => "waiting_tool",
        AgentLifecycle::WaitingApproval => "waiting_approval",
        AgentLifecycle::Paused => "paused",
        AgentLifecycle::Blocked => "blocked",
        AgentLifecycle::Succeeded => "succeeded",
        AgentLifecycle::Failed => "failed",
        AgentLifecycle::Cancelled => "cancelled",
    }
}

fn worker_label(class: WorkerClass) -> &'static str {
    match class {
        WorkerClass::Unspecified => "unspecified",
        WorkerClass::Persistent => "persistent",
        WorkerClass::Managed => "managed",
    }
}

fn control_label(holder: ControlHolder) -> &'static str {
    match holder {
        ControlHolder::Agent => "agent",
        ControlHolder::Human => "human",
    }
}

fn is_takeover(state: AgentLifecycle, control: ControlHolder) -> bool {
    control == ControlHolder::Human
        && matches!(
            state,
            AgentLifecycle::Starting
                | AgentLifecycle::Running
                | AgentLifecycle::WaitingTool
                | AgentLifecycle::WaitingApproval
        )
}

fn short_id(raw: &str) -> String {
    let tail = raw.rsplit('-').next().unwrap_or(raw);
    sanitize_untrusted(tail).into_owned()
}

fn sanitize_preview(raw: &str) -> String {
    let cleaned = sanitize_untrusted(raw);
    let mut out: String = cleaned.chars().take(MAX_PREVIEW_CHARS).collect();
    out.retain(|c| c != '\n' && c != '\t');
    if out.len() > MAX_AGENT_TEXT_BYTES {
        out.truncate(MAX_AGENT_TEXT_BYTES);
    }
    if out.is_empty() { "-".to_owned() } else { out }
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

fn check_cancel(cancel: &CancellationToken) -> Result<(), AgentsPanelError> {
    if cancel.is_cancelled() {
        Err(AgentsPanelError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{LocalUiEvent, UiEvent, reduce, replay};
    use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, EventKind, RecordedAt};
    use protocol::{EventId, RedactionClass, TraceId};
    use serde_json::Value;

    const GOLDEN_80: &str = "\
agents control:human
  000000000015 persistent paused
  > 000000000018 managed running write merge:conflict:1 TAKEOVER
    00000000001b unspecified paused
selected:000000000018
  state:running action:edit src elapsed:1500ms tokens:4 cost:1
  view:000000000021 write
  diff:+2/-1 files:1
  blocker:-
  evidence:000000000030
  merge:conflict:1
  control:human TAKEOVER";

    const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000010";
    const PROJECT_ID: &str = "019c0000-0000-7000-8000-000000000011";
    const AGENT_ID: &str = "019c0000-0000-7000-8000-000000000015";
    const CHILD_ID: &str = "019c0000-0000-7000-8000-000000000018";
    const SIBLING_ID: &str = "019c0000-0000-7000-8000-00000000001b";
    const VIEW_ID: &str = "019c0000-0000-7000-8000-000000000021";
    const EVIDENCE_ID: &str = "019c0000-0000-7000-8000-000000000030";
    const ACTOR_ID: &str = "019c0000-0000-7000-8000-000000000016";
    const TRACE_ID: &str = "8f000000-0000-7000-8000-000000000017";
    const CREATED_AT: &str = "2026-08-14T15:20:04.123Z";
    const UPDATED_AT: &str = "2026-08-14T15:21:00.000Z";

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn agent_id() -> AgentId {
        AGENT_ID.parse().expect("agent")
    }

    fn child_id() -> AgentId {
        CHILD_ID.parse().expect("child")
    }

    fn sibling_id() -> AgentId {
        SIBLING_ID.parse().expect("sibling")
    }

    fn view_id() -> WorkspaceViewId {
        VIEW_ID.parse().expect("view")
    }

    fn envelope(seq: u64, kind: EventKind, payload: Value) -> EventEnvelope<Value> {
        EventEnvelope::new(
            event_id_for_seq(seq),
            SESSION_ID.parse().expect("session"),
            seq,
            UPDATED_AT.parse::<RecordedAt>().expect("recorded_at"),
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

    fn created() -> UiEvent {
        UiEvent::Kernel(EventEnvelope::new(
            event_id_for_seq(1),
            SESSION_ID.parse().expect("session"),
            1,
            CREATED_AT.parse::<RecordedAt>().expect("recorded_at"),
            ActorRef::new(ActorKind::System, ACTOR_ID).expect("actor"),
            TRACE_ID.parse::<TraceId>().expect("trace"),
            EventKind::SessionCreated,
            RedactionClass::Project,
            serde_json::json!({"project_id": PROJECT_ID}),
        ))
    }

    fn kernel(seq: u64, kind: EventKind, payload: Value) -> UiEvent {
        UiEvent::Kernel(envelope(seq, kind, payload))
    }

    fn fixture_events() -> Vec<UiEvent> {
        vec![
            created(),
            kernel(
                2,
                EventKind::AgentSpawned,
                serde_json::json!({
                    "agent_id": AGENT_ID,
                    "role": "coder",
                    "worker_class": "persistent",
                    "state": "paused",
                    "stats": {"tokens": 12, "cost": 3, "active_ms": 40}
                }),
            ),
            kernel(
                3,
                EventKind::AgentSpawned,
                serde_json::json!({
                    "agent_id": CHILD_ID,
                    "parent_id": AGENT_ID,
                    "worker_class": "managed",
                    "workspace_view_id": VIEW_ID
                }),
            ),
            kernel(
                4,
                EventKind::AgentStateChanged,
                serde_json::json!({
                    "agent_id": CHILD_ID,
                    "state": "running",
                    "current_operation": "edit src",
                    "evidence_id": EVIDENCE_ID,
                    "stats": {"tokens": 4, "cost": 1, "active_ms": 1500}
                }),
            ),
            kernel(
                5,
                EventKind::AgentSpawned,
                serde_json::json!({
                    "agent_id": SIBLING_ID,
                    "parent_id": AGENT_ID,
                    "state": "paused"
                }),
            ),
            kernel(
                6,
                EventKind::ControlTransferredToHuman,
                serde_json::json!({}),
            ),
            UiEvent::Local(LocalUiEvent::SelectAgent(Some(child_id()))),
        ]
    }

    fn fixture_state() -> AppState {
        replay(&fixture_events(), &cancel()).expect("replay")
    }

    fn child_handoff() -> AgentHandoffObservation {
        AgentHandoffObservation::new(
            child_id(),
            Some(PatchSummary::new(1, 2, 1)),
            Some(AgentMergeState::Conflict { conflicts: 1 }),
        )
    }

    fn fixture_model() -> AgentsViewModel {
        AgentsViewModel::from_state(
            &fixture_state(),
            &[child_handoff()],
            AgentsSelection::default(),
            &cancel(),
        )
        .expect("model")
    }

    #[test]
    fn golden_80_120_200() {
        let model = fixture_model();
        assert_eq!(model.render(80, 24).golden(), GOLDEN_80);
        assert_eq!(model.render(120, 24).golden(), GOLDEN_80);
        assert_eq!(model.render(200, 24).golden(), GOLDEN_80);
        assert_eq!(model.render(80, 24).text().lines().count(), 24);
        assert!(
            model
                .render(80, 24)
                .text()
                .lines()
                .all(|line| line.chars().count() == 80)
        );
    }

    #[test]
    fn parent_child_hierarchy_is_stable_during_streaming() {
        let before = fixture_model();
        let ids: Vec<(AgentId, usize)> = before
            .rows()
            .iter()
            .map(|row| (row.id(), row.depth()))
            .collect();
        assert_eq!(
            ids,
            vec![(agent_id(), 0), (child_id(), 1), (sibling_id(), 1)]
        );
        assert_eq!(before.rows()[1].parent_id(), Some(agent_id()));
        assert_eq!(before.rows()[2].parent_id(), Some(agent_id()));

        let streamed = reduce(
            fixture_state(),
            &kernel(
                7,
                EventKind::AgentStateChanged,
                serde_json::json!({
                    "agent_id": CHILD_ID,
                    "state": "waiting_tool",
                    "current_operation": "run tests",
                    "stats": {"tokens": 40, "cost": 9, "active_ms": 9000}
                }),
            ),
        );
        let after = AgentsViewModel::from_state(
            &streamed,
            &[child_handoff()],
            AgentsSelection::default(),
            &cancel(),
        )
        .expect("after");
        let after_ids: Vec<(AgentId, usize)> = after
            .rows()
            .iter()
            .map(|row| (row.id(), row.depth()))
            .collect();
        assert_eq!(ids, after_ids);
        assert_eq!(after.rows()[1].state(), AgentLifecycle::WaitingTool);
        assert_eq!(after.rows()[1].tokens(), 40);
    }

    #[test]
    fn write_view_and_pending_merge_conflict_are_visible() {
        let model = fixture_model();
        let child = &model.rows()[1];
        assert!(child.has_write_view());
        assert_eq!(child.workspace_view_id(), Some(view_id()));
        assert_eq!(
            child.merge(),
            Some(AgentMergeState::Conflict { conflicts: 1 })
        );
        let golden = model.render(80, 24).golden();
        assert!(golden.contains("view:000000000021 write"));
        assert!(golden.contains("merge:conflict:1"));
        assert!(golden.contains("diff:+2/-1 files:1"));

        let pending = AgentsViewModel::from_state(
            &fixture_state(),
            &[AgentHandoffObservation::new(
                child_id(),
                Some(PatchSummary::new(2, 4, 0)),
                Some(AgentMergeState::Pending),
            )],
            AgentsSelection::default(),
            &cancel(),
        )
        .expect("pending");
        let pending_golden = pending.render(80, 16).golden();
        assert!(pending_golden.contains("merge:pending"));
        assert!(pending_golden.contains("write"));
    }

    #[test]
    fn cancel_inspect_merge_emit_typed_intents() {
        let model = fixture_model();
        assert!(!model.mutates_runtime());
        let cancel_intent = model.cancel(&cancel()).expect("cancel");
        assert_eq!(cancel_intent.agent_id(), child_id());
        assert!(cancel_intent.requires_kernel_interrupt());
        assert_eq!(model.selected_row().map(AgentRowView::id), Some(child_id()));

        let inspect = model.inspect(&cancel()).expect("inspect");
        assert_eq!(inspect.agent_id(), child_id());
        assert!(inspect.uses_stored_result());

        let merge = model.merge(&cancel()).expect("merge");
        assert_eq!(merge.child(), child_id());
        assert_eq!(merge.parent(), agent_id());
        assert_eq!(merge.child_view(), view_id());
        assert!(merge.requires_workspace_transaction());
    }

    #[test]
    fn takeover_is_distinct_from_pause() {
        let model = fixture_model();
        assert_eq!(model.control_holder(), ControlHolder::Human);
        assert!(model.rows()[1].is_takeover());
        assert!(!model.rows()[0].is_takeover());
        assert!(!model.rows()[2].is_takeover());
        let golden = model.render(80, 24).golden();
        assert!(golden.contains("TAKEOVER"));
        assert!(golden.contains("000000000015 persistent paused"));
        assert!(
            !golden
                .lines()
                .any(|line| { line.contains("000000000015") && line.contains(TAKEOVER_LABEL) })
        );
    }

    #[test]
    fn terminal_cancel_and_committed_merge_fail_closed() {
        let cancelled = reduce(
            fixture_state(),
            &kernel(
                7,
                EventKind::AgentCancelled,
                serde_json::json!({"agent_id": CHILD_ID}),
            ),
        );
        let model = AgentsViewModel::from_state(
            &cancelled,
            &[child_handoff()],
            AgentsSelection::default(),
            &cancel(),
        )
        .expect("cancelled model");
        assert_eq!(
            model.cancel(&cancel()),
            Err(AgentsPanelError::AlreadyTerminal)
        );
        assert!(model.inspect(&cancel()).is_ok());

        let committed = AgentsViewModel::from_state(
            &fixture_state(),
            &[AgentHandoffObservation::new(
                child_id(),
                Some(PatchSummary::new(1, 1, 0)),
                Some(AgentMergeState::Committed),
            )],
            AgentsSelection::default(),
            &cancel(),
        )
        .expect("committed");
        assert_eq!(
            committed.merge(&cancel()),
            Err(AgentsPanelError::AlreadyMerged)
        );
        assert!(committed.inspect(&cancel()).is_ok());
    }

    #[test]
    fn merge_requires_parent_and_write_view() {
        let model = fixture_model();
        assert_eq!(
            model.merge_agent(agent_id(), &cancel()),
            Err(AgentsPanelError::MissingParent)
        );
        assert_eq!(
            model.merge_agent(sibling_id(), &cancel()),
            Err(AgentsPanelError::MissingWriteView)
        );
    }

    #[test]
    fn sanitizes_untrusted_action_and_blocker() {
        let dirty = reduce(
            fixture_state(),
            &kernel(
                7,
                EventKind::AgentStateChanged,
                serde_json::json!({
                    "agent_id": CHILD_ID,
                    "state": "blocked",
                    "current_operation": "run\u{001B}]8;;https://evil.example\u{0007}tool",
                    "blocker": "wait\u{001B}]52;c;c2VjcmV0\u{0007}lease"
                }),
            ),
        );
        let model = AgentsViewModel::from_state(
            &dirty,
            &[child_handoff()],
            AgentsSelection::default(),
            &cancel(),
        )
        .expect("dirty");
        let child = &model.rows()[1];
        assert!(!child.action().contains('\u{001B}'));
        assert!(!child.action().contains("https://evil.example"));
        assert!(!child.blocker().contains('\u{001B}'));
        let golden = model.render(80, 16).golden();
        assert!(!golden.contains('\u{001B}'));
        assert!(!golden.contains("https://evil.example"));
    }

    #[test]
    fn construct_and_actions_honor_cancellation() {
        let token = cancel();
        token.cancel();
        assert_eq!(
            AgentsViewModel::from_state(
                &fixture_state(),
                &[child_handoff()],
                AgentsSelection::default(),
                &token
            ),
            Err(AgentsPanelError::Cancelled)
        );
        let model = fixture_model();
        assert_eq!(model.cancel(&token), Err(AgentsPanelError::Cancelled));
        assert_eq!(model.inspect(&token), Err(AgentsPanelError::Cancelled));
        assert_eq!(model.merge(&token), Err(AgentsPanelError::Cancelled));
    }

    #[test]
    fn handoff_bound_exceeded_fails_closed() {
        let extra = AgentHandoffObservation::new(child_id(), None, Some(AgentMergeState::Pending));
        let too_many = vec![extra; MAX_AGENTS + 1];
        assert_eq!(
            AgentsViewModel::from_state(
                &fixture_state(),
                &too_many,
                AgentsSelection::default(),
                &cancel()
            ),
            Err(AgentsPanelError::BoundExceeded)
        );
    }

    #[test]
    fn cycle_fails_closed() {
        let cyclic = replay(
            &[
                created(),
                kernel(
                    2,
                    EventKind::AgentSpawned,
                    serde_json::json!({
                        "agent_id": AGENT_ID,
                        "parent_id": CHILD_ID
                    }),
                ),
                kernel(
                    3,
                    EventKind::AgentSpawned,
                    serde_json::json!({
                        "agent_id": CHILD_ID,
                        "parent_id": AGENT_ID
                    }),
                ),
            ],
            &cancel(),
        )
        .expect("cycle state");
        assert_eq!(
            AgentsViewModel::from_state(&cyclic, &[], AgentsSelection::default(), &cancel()),
            Err(AgentsPanelError::InvalidField)
        );
    }

    #[test]
    fn navigation_moves_selection() {
        let model = fixture_model();
        assert_eq!(model.selected_row().map(AgentRowView::id), Some(child_id()));
        let next = model.select_next();
        assert_eq!(
            next.selected_row().map(AgentRowView::id),
            Some(sibling_id())
        );
        assert!(next.render(80, 12).golden().contains("> 00000000001b"));
        let prev = next.select_prev();
        assert_eq!(prev.selected_row().map(AgentRowView::id), Some(child_id()));
    }

    #[test]
    fn blocked_actions_cannot_cancel_or_merge() {
        let mut events = fixture_events();
        events.push(kernel(
            7,
            EventKind::ApprovalRequested,
            serde_json::json!({}),
        ));
        let blocked = events
            .iter()
            .fold(AppState::new(), reduce);
        assert!(blocked.actions_blocked());
        let model = AgentsViewModel::from_state(
            &blocked,
            &[child_handoff()],
            AgentsSelection::default(),
            &cancel(),
        )
        .expect("blocked model");
        assert!(model.render(80, 4).golden().contains("blocked"));
        assert_eq!(
            model.cancel(&cancel()),
            Err(AgentsPanelError::ActionsBlocked)
        );
        assert_eq!(
            model.merge(&cancel()),
            Err(AgentsPanelError::ActionsBlocked)
        );
        assert!(model.inspect(&cancel()).is_ok());
    }

    #[test]
    fn empty_panel_renders_placeholder() {
        let state = replay(&[created()], &cancel()).expect("empty");
        let model = AgentsViewModel::from_state(&state, &[], AgentsSelection::default(), &cancel())
            .expect("empty model");
        assert!(model.rows().is_empty());
        assert_eq!(
            model.render(80, 4).golden(),
            "agents control:agent\n  (empty)"
        );
        assert_eq!(
            model.cancel(&cancel()),
            Err(AgentsPanelError::InvalidSelection)
        );
    }
}
