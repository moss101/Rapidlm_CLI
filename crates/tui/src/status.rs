//! Status bar projection over [`AppState`].
//!
//! Items read only from a [`StatusSnapshot`]. They never query kernel or
//! other services. Untrusted model/provider labels are sanitized before
//! they become visible text.
//!
//! Narrow terminals drop items in a fixed priority order after compacting
//! labels. The same snapshot and width always produce the same line.

use crate::sanitize::sanitize_untrusted;
use crate::state::{AgentLifecycle, AppState, GoalLifecycle, GoalProjection};

/// Maximum UTF-8 bytes retained for a model or provider display label.
pub const MAX_MODEL_LABEL_BYTES: usize = 64;

/// Status line width is clamped to this many columns.
pub const MAX_STATUS_LINE_COLS: u16 = 512;

/// Space between adjacent status items.
const ITEM_SEP: &str = "  ";

const DISPLAY_ORDER: [StatusItemKind; 8] = [
    StatusItemKind::Model,
    StatusItemKind::Sandbox,
    StatusItemKind::Policy,
    StatusItemKind::Context,
    StatusItemKind::GoalBudget,
    StatusItemKind::Agents,
    StatusItemKind::Cost,
    StatusItemKind::Connectivity,
];

/// Lowest priority first. Connectivity is the last item retained.
const DROP_ORDER: [StatusItemKind; 8] = [
    StatusItemKind::Cost,
    StatusItemKind::Context,
    StatusItemKind::Agents,
    StatusItemKind::GoalBudget,
    StatusItemKind::Model,
    StatusItemKind::Sandbox,
    StatusItemKind::Policy,
    StatusItemKind::Connectivity,
];

/// One status-bar field. Display order and drop order are independent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum StatusItemKind {
    Model,
    Sandbox,
    Policy,
    Context,
    GoalBudget,
    Agents,
    Cost,
    Connectivity,
}

/// Kernel/session reachability derived from the frontend projection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum Connectivity {
    #[default]
    Disconnected,
    Connected,
    Reconnecting,
    ReadOnly,
}

/// Sandbox tier shown on the bar. Unknown until chrome supplies it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum SandboxMode {
    #[default]
    Unknown,
    HostRestricted,
    Container,
    Gvisor,
    RemoteWorker,
}

/// Policy decision mode shown on the bar. Unknown until chrome supplies it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum PolicyMode {
    #[default]
    Unknown,
    Allow,
    Ask,
    Deny,
}

/// Compiled-context usage. Absent limit means unbounded/unknown ceiling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct ContextUsage {
    used: u64,
    limit: Option<u64>,
}

/// Local chrome that is not yet a domain map on [`AppState`].
///
/// Held beside the reducer. Status items still do not query services.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatusChrome {
    model: Option<String>,
    provider: Option<String>,
    sandbox: SandboxMode,
    policy: PolicyMode,
    context: ContextUsage,
}

/// Frontend-only status projection. Constructed from [`AppState`] plus chrome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusSnapshot {
    model: Option<String>,
    sandbox: SandboxMode,
    policy: PolicyMode,
    context: ContextUsage,
    goal: GoalBudgetView,
    agent_count: u32,
    cost: u64,
    connectivity: Connectivity,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
struct GoalBudgetView {
    present: bool,
    turns: u64,
    max_turns: Option<u64>,
    tokens: u64,
    max_tokens: Option<u64>,
}

/// One rendered status row. `text` is padded or truncated to `width`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusLine {
    width: u16,
    content: String,
    visible: Vec<StatusItemKind>,
    collapsed: Vec<StatusItemKind>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum LabelStyle {
    Full,
    Compact,
}

impl StatusItemKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Sandbox => "sandbox",
            Self::Policy => "policy",
            Self::Context => "context",
            Self::GoalBudget => "goal",
            Self::Agents => "agents",
            Self::Cost => "cost",
            Self::Connectivity => "connectivity",
        }
    }
}

impl Connectivity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "offline",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
            Self::ReadOnly => "readonly",
        }
    }

    const fn compact(self) -> &'static str {
        match self {
            Self::Disconnected => "off",
            Self::Connected => "on",
            Self::Reconnecting => "reconn",
            Self::ReadOnly => "ro",
        }
    }
}

impl SandboxMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "-",
            Self::HostRestricted => "host-restricted",
            Self::Container => "container",
            Self::Gvisor => "gvisor",
            Self::RemoteWorker => "remote-worker",
        }
    }

    const fn compact(self) -> &'static str {
        match self {
            Self::Unknown => "-",
            Self::HostRestricted => "host",
            Self::Container => "ctr",
            Self::Gvisor => "gvisor",
            Self::RemoteWorker => "remote",
        }
    }
}

impl PolicyMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "-",
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

impl ContextUsage {
    pub const fn new(used: u64, limit: Option<u64>) -> Self {
        Self { used, limit }
    }

    pub const fn unknown() -> Self {
        Self {
            used: 0,
            limit: None,
        }
    }

    pub const fn used(self) -> u64 {
        self.used
    }

    pub const fn limit(self) -> Option<u64> {
        self.limit
    }

    const fn is_unknown(self) -> bool {
        self.used == 0 && self.limit.is_none()
    }
}

impl StatusChrome {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_model(mut self, model: &str) -> Self {
        self.model = sanitize_model_label(model);
        self
    }

    pub fn with_provider(mut self, provider: &str) -> Self {
        self.provider = sanitize_provider_label(provider);
        self
    }

    pub fn with_sandbox(mut self, sandbox: SandboxMode) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn with_policy(mut self, policy: PolicyMode) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_context(mut self, context: ContextUsage) -> Self {
        self.context = context;
        self
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub fn sandbox(&self) -> SandboxMode {
        self.sandbox
    }

    pub fn policy(&self) -> PolicyMode {
        self.policy
    }

    pub fn context(&self) -> ContextUsage {
        self.context
    }
}

impl StatusSnapshot {
    /// Project status items from reducer state. No service calls.
    pub fn from_state(state: &AppState) -> Self {
        Self::from_parts(state, &StatusChrome::default())
    }

    /// Same as [`from_state`] with local model/sandbox/policy/context chrome.
    pub fn from_parts(state: &AppState, chrome: &StatusChrome) -> Self {
        Self {
            model: compose_model_label(chrome.model.as_deref(), chrome.provider.as_deref()),
            sandbox: chrome.sandbox,
            policy: chrome.policy,
            context: chrome.context,
            goal: goal_from_state(state),
            agent_count: live_agent_count(state),
            cost: total_cost(state),
            connectivity: connectivity_from_state(state),
        }
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn sandbox(&self) -> SandboxMode {
        self.sandbox
    }

    pub fn policy(&self) -> PolicyMode {
        self.policy
    }

    pub fn context(&self) -> ContextUsage {
        self.context
    }

    pub fn agent_count(&self) -> u32 {
        self.agent_count
    }

    pub fn cost(&self) -> u64 {
        self.cost
    }

    pub fn connectivity(&self) -> Connectivity {
        self.connectivity
    }

    pub fn turns(&self) -> Option<u64> {
        self.goal.present.then_some(self.goal.turns)
    }

    pub fn max_turns(&self) -> Option<u64> {
        self.goal.max_turns
    }

    pub fn tokens(&self) -> Option<u64> {
        self.goal.present.then_some(self.goal.tokens)
    }

    pub fn max_tokens(&self) -> Option<u64> {
        self.goal.max_tokens
    }

    pub fn with_model(mut self, model: &str) -> Self {
        self.model = sanitize_model_label(model);
        self
    }

    pub fn with_sandbox(mut self, sandbox: SandboxMode) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn with_policy(mut self, policy: PolicyMode) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_context(mut self, context: ContextUsage) -> Self {
        self.context = context;
        self
    }

    pub fn with_connectivity(mut self, connectivity: Connectivity) -> Self {
        self.connectivity = connectivity;
        self
    }

    pub fn render(&self, width: u16) -> StatusLine {
        render_snapshot(self, width)
    }
}

impl StatusLine {
    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    /// Exact-width row written into the status rect.
    pub fn text(&self) -> String {
        fit_width(&self.content, usize::from(self.width))
    }

    pub fn visible(&self) -> &[StatusItemKind] {
        &self.visible
    }

    pub fn collapsed(&self) -> &[StatusItemKind] {
        &self.collapsed
    }

    /// Stable dump used by golden tests. Trailing pad is omitted.
    pub fn golden(&self) -> String {
        self.content.clone()
    }
}

/// Render the status bar from reducer state at `width` columns.
pub fn render_status(state: &AppState, width: u16) -> StatusLine {
    StatusSnapshot::from_state(state).render(width)
}

/// Render with explicit model/sandbox/policy/context chrome.
pub fn render_status_with(state: &AppState, chrome: &StatusChrome, width: u16) -> StatusLine {
    StatusSnapshot::from_parts(state, chrome).render(width)
}

fn render_snapshot(snapshot: &StatusSnapshot, width: u16) -> StatusLine {
    let width = width.min(MAX_STATUS_LINE_COLS);
    if width == 0 {
        return StatusLine {
            width,
            content: String::new(),
            visible: Vec::new(),
            collapsed: DISPLAY_ORDER.to_vec(),
        };
    }

    let mut visible = DISPLAY_ORDER.to_vec();
    if let Some(content) = join_items(snapshot, &visible, LabelStyle::Full, width) {
        return StatusLine {
            width,
            content,
            visible,
            collapsed: Vec::new(),
        };
    }
    if let Some(content) = join_items(snapshot, &visible, LabelStyle::Compact, width) {
        return StatusLine {
            width,
            content,
            visible,
            collapsed: Vec::new(),
        };
    }

    let mut collapsed = Vec::new();
    for drop in DROP_ORDER {
        if visible.len() <= 1 {
            break;
        }
        if let Some(idx) = visible.iter().position(|kind| *kind == drop) {
            visible.remove(idx);
            collapsed.push(drop);
            if let Some(content) = join_items(snapshot, &visible, LabelStyle::Compact, width) {
                return StatusLine {
                    width,
                    content,
                    visible,
                    collapsed,
                };
            }
        }
    }

    let content = force_fit(snapshot, &visible, width);
    StatusLine {
        width,
        content,
        visible,
        collapsed,
    }
}

fn join_items(
    snapshot: &StatusSnapshot,
    visible: &[StatusItemKind],
    style: LabelStyle,
    width: u16,
) -> Option<String> {
    if visible.is_empty() {
        return Some(String::new());
    }
    let mut content = String::new();
    for (i, kind) in visible.iter().enumerate() {
        if i > 0 {
            content.push_str(ITEM_SEP);
        }
        content.push_str(&format_item(snapshot, *kind, style));
    }
    if display_width(&content) <= usize::from(width) {
        Some(content)
    } else {
        None
    }
}

fn force_fit(snapshot: &StatusSnapshot, visible: &[StatusItemKind], width: u16) -> String {
    let raw = visible
        .iter()
        .map(|kind| format_item(snapshot, *kind, LabelStyle::Compact))
        .collect::<Vec<_>>()
        .join(ITEM_SEP);
    fit_width(&raw, usize::from(width)).trim_end().to_owned()
}

fn format_item(snapshot: &StatusSnapshot, kind: StatusItemKind, style: LabelStyle) -> String {
    match (kind, style) {
        (StatusItemKind::Model, LabelStyle::Full) => {
            format!("model:{}", snapshot.model.as_deref().unwrap_or("-"))
        }
        (StatusItemKind::Model, LabelStyle::Compact) => {
            snapshot.model.clone().unwrap_or_else(|| "-".to_owned())
        }
        (StatusItemKind::Sandbox, LabelStyle::Full) => {
            format!("sandbox:{}", snapshot.sandbox.as_str())
        }
        (StatusItemKind::Sandbox, LabelStyle::Compact) => {
            format!("sbx:{}", snapshot.sandbox.compact())
        }
        (StatusItemKind::Policy, LabelStyle::Full) => {
            format!("policy:{}", snapshot.policy.as_str())
        }
        (StatusItemKind::Policy, LabelStyle::Compact) => {
            format!("pol:{}", snapshot.policy.as_str())
        }
        (StatusItemKind::Context, LabelStyle::Full) => {
            format!("ctx:{}", format_usage(snapshot.context))
        }
        (StatusItemKind::Context, LabelStyle::Compact) => {
            format!("c:{}", format_usage(snapshot.context))
        }
        (StatusItemKind::GoalBudget, LabelStyle::Full) => {
            format!("goal:{}", format_goal(snapshot.goal, false))
        }
        (StatusItemKind::GoalBudget, LabelStyle::Compact) => {
            format!("g:{}", format_goal(snapshot.goal, true))
        }
        (StatusItemKind::Agents, LabelStyle::Full) => {
            format!("agents:{}", snapshot.agent_count)
        }
        (StatusItemKind::Agents, LabelStyle::Compact) => {
            format!("a:{}", snapshot.agent_count)
        }
        (StatusItemKind::Cost, LabelStyle::Full) => {
            format!("cost:{}", format_number(snapshot.cost))
        }
        (StatusItemKind::Cost, LabelStyle::Compact) => {
            format!("${}", format_number(snapshot.cost))
        }
        (StatusItemKind::Connectivity, LabelStyle::Full) => {
            snapshot.connectivity.as_str().to_owned()
        }
        (StatusItemKind::Connectivity, LabelStyle::Compact) => {
            snapshot.connectivity.compact().to_owned()
        }
    }
}

fn format_usage(usage: ContextUsage) -> String {
    if usage.is_unknown() {
        return "-".to_owned();
    }
    match usage.limit {
        Some(limit) => format!("{}/{}", format_number(usage.used), format_number(limit)),
        None => format_number(usage.used),
    }
}

fn format_goal(goal: GoalBudgetView, compact: bool) -> String {
    if !goal.present {
        return "-".to_owned();
    }
    let turns = match goal.max_turns {
        Some(max) => Some(format!("{}/{}t", goal.turns, max)),
        None if goal.turns > 0 || goal.max_tokens.is_none() => Some(format!("{}t", goal.turns)),
        None => None,
    };
    let tokens = match goal.max_tokens {
        Some(max) => Some(format!(
            "{}/{}",
            format_number(goal.tokens),
            format_number(max)
        )),
        None if goal.tokens > 0 => Some(format_number(goal.tokens)),
        None => None,
    };
    if compact {
        return turns.or(tokens).unwrap_or_else(|| "0t".to_owned());
    }
    match (turns, tokens) {
        (Some(t), Some(k)) => format!("{t} {k}"),
        (Some(t), None) => t,
        (None, Some(k)) => k,
        (None, None) => "0t".to_owned(),
    }
}

fn format_number(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        let whole = n / 1_000;
        let frac = (n % 1_000) / 100;
        if frac == 0 {
            format!("{whole}k")
        } else {
            format!("{whole}.{frac}k")
        }
    } else {
        let whole = n / 1_000_000;
        let frac = (n % 1_000_000) / 100_000;
        if frac == 0 {
            format!("{whole}m")
        } else {
            format!("{whole}.{frac}m")
        }
    }
}

fn live_agent_count(state: &AppState) -> u32 {
    let count = state
        .agents()
        .values()
        .filter(|agent| !is_terminal(agent.state()))
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn is_terminal(state: AgentLifecycle) -> bool {
    matches!(
        state,
        AgentLifecycle::Succeeded | AgentLifecycle::Failed | AgentLifecycle::Cancelled
    )
}

fn total_cost(state: &AppState) -> u64 {
    state
        .agents()
        .values()
        .map(|agent| agent.cost())
        .fold(0, u64::saturating_add)
}

fn goal_from_state(state: &AppState) -> GoalBudgetView {
    let selected = state.selected_goal().and_then(|id| state.goals().get(&id));
    let active = state.goals().values().find(|goal| {
        matches!(
            goal.lifecycle(),
            GoalLifecycle::Active | GoalLifecycle::Paused | GoalLifecycle::Blocked
        )
    });
    let goal = selected
        .or(active)
        .or_else(|| state.goals().values().next());
    match goal {
        Some(goal) => GoalBudgetView::from_projection(goal),
        None => GoalBudgetView::default(),
    }
}

impl GoalBudgetView {
    fn from_projection(goal: &GoalProjection) -> Self {
        Self {
            present: true,
            turns: goal.turns(),
            max_turns: goal.max_turns(),
            tokens: goal.tokens(),
            max_tokens: goal.max_tokens(),
        }
    }
}

fn connectivity_from_state(state: &AppState) -> Connectivity {
    if state.actions_blocked() {
        return Connectivity::ReadOnly;
    }
    match state.session_status() {
        None => Connectivity::Disconnected,
        Some(kernel::SessionStatus::Closed) => Connectivity::Disconnected,
        Some(kernel::SessionStatus::Recovering) => Connectivity::Reconnecting,
        Some(
            kernel::SessionStatus::Ready
            | kernel::SessionStatus::Busy
            | kernel::SessionStatus::Paused,
        ) => Connectivity::Connected,
    }
}

fn compose_model_label(model: Option<&str>, provider: Option<&str>) -> Option<String> {
    match (model, provider) {
        (None, None) => None,
        (Some(model), None) => Some(model.to_owned()),
        (None, Some(provider)) => Some(provider.to_owned()),
        (Some(model), Some(provider)) => {
            if provider == "[redacted]" {
                Some(model.to_owned())
            } else {
                Some(format!("{model}@{provider}"))
            }
        }
    }
}

/// Neutralize escapes and redact key-like model/provider metadata.
fn sanitize_model_label(raw: &str) -> Option<String> {
    let cleaned = single_line(sanitize_untrusted(raw).as_ref());
    if cleaned.is_empty() {
        return None;
    }
    if looks_sensitive(&cleaned) {
        return Some("[redacted]".to_owned());
    }
    Some(truncate_bytes(&cleaned, MAX_MODEL_LABEL_BYTES))
}

fn sanitize_provider_label(raw: &str) -> Option<String> {
    let label = sanitize_model_label(raw)?;
    if label == "[redacted]" {
        return Some(label);
    }
    if is_display_safe_ident(&label) {
        Some(label)
    } else {
        Some("[redacted]".to_owned())
    }
}

fn is_display_safe_ident(label: &str) -> bool {
    !label.is_empty()
        && !label.contains("://")
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':'))
}

fn looks_sensitive(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("sk-")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("bearer ")
        || lower.contains("-----begin")
        || lower.contains("secret=")
        || lower.contains("token=")
    {
        return true;
    }
    let body: String = text.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    body.len() >= 32 && body.chars().all(|c| c.is_ascii_hexdigit())
}

fn single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c == '\n' || c == '\t' || c == '\r' {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space && c != ' ' {
            out.push(' ');
        }
        pending_space = false;
        out.push(c);
    }
    out.trim().to_owned()
}

fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].trim_end().to_owned()
}

fn fit_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cols = display_width(text);
    if cols == width {
        return text.to_owned();
    }
    if cols < width {
        let mut out = text.to_owned();
        out.extend(std::iter::repeat_n(' ', width - cols));
        return out;
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let keep = width - 3;
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = display_cols(c);
        if used + w > keep {
            break;
        }
        out.push(c);
        used = used.saturating_add(w);
    }
    out.push_str("...");
    out
}

fn display_width(text: &str) -> usize {
    text.chars().map(display_cols).sum()
}

fn display_cols(c: char) -> usize {
    match c {
        '\t' => 1,
        c if is_wide(c) => 2,
        _ => 1,
    }
}

fn is_wide(c: char) -> bool {
    matches!(
        c,
        '\u{1100}'..='\u{115F}'
            | '\u{2329}'..='\u{232A}'
            | '\u{2E80}'..='\u{A4CF}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE10}'..='\u{FE19}'
            | '\u{FE30}'..='\u{FE6F}'
            | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}'
            | '\u{1F300}'..='\u{1F64F}'
            | '\u{1F900}'..='\u{1F9FF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CancellationToken, LocalUiEvent, UiEvent, reduce, replay};
    use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, EventKind, RecordedAt};
    use protocol::{EventId, RedactionClass, TraceId};
    use serde_json::Value;

    const GOLDEN_80: &str = "claude-sonnet  sbx:ctr  pol:ask  c:12k/200k  g:3/20t  a:2  $42  on";
    const GOLDEN_120: &str = "model:claude-sonnet  sandbox:container  policy:ask  ctx:12k/200k  goal:3/20t 1.2k/50k  agents:2  cost:42  connected";
    const GOLDEN_200: &str = GOLDEN_120;
    const GOLDEN_EMPTY_80: &str =
        "model:-  sandbox:-  policy:-  ctx:-  goal:-  agents:0  cost:0  offline";

    const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000010";
    const PROJECT_ID: &str = "019c0000-0000-7000-8000-000000000011";
    const GOAL_ID: &str = "019c0000-0000-7000-8000-000000000014";
    const AGENT_ID: &str = "019c0000-0000-7000-8000-000000000015";
    const CHILD_ID: &str = "019c0000-0000-7000-8000-000000000018";
    const ACTOR_ID: &str = "019c0000-0000-7000-8000-000000000016";
    const TRACE_ID: &str = "8f000000-0000-7000-8000-000000000017";
    const CREATED_AT: &str = "2026-08-14T15:20:04.123Z";
    const UPDATED_AT: &str = "2026-08-14T15:21:00.000Z";

    fn fixture_snapshot() -> StatusSnapshot {
        StatusSnapshot::from_state(&AppState::new())
            .with_model("claude-sonnet")
            .with_sandbox(SandboxMode::Container)
            .with_policy(PolicyMode::Ask)
            .with_context(ContextUsage::new(12_000, Some(200_000)))
            .with_connectivity(Connectivity::Connected)
            .with_goal(3, Some(20), 1_200, Some(50_000))
            .with_counts(2, 42)
    }

    impl StatusSnapshot {
        fn with_goal(
            mut self,
            turns: u64,
            max_turns: Option<u64>,
            tokens: u64,
            max_tokens: Option<u64>,
        ) -> Self {
            self.goal = GoalBudgetView {
                present: true,
                turns,
                max_turns,
                tokens,
                max_tokens,
            };
            self
        }

        fn with_counts(mut self, agent_count: u32, cost: u64) -> Self {
            self.agent_count = agent_count;
            self.cost = cost;
            self
        }
    }

    fn envelope(
        seq: u64,
        kind: EventKind,
        payload: Value,
    ) -> event_ledger::event::ErasedEventEnvelope {
        EventEnvelope::new(
            format!("019c0000-0000-7000-8000-{seq:012x}")
                .parse::<EventId>()
                .expect("event id"),
            SESSION_ID.parse().expect("session"),
            seq,
            UPDATED_AT
                .parse::<RecordedAt>()
                .unwrap_or_else(|_| CREATED_AT.parse::<RecordedAt>().expect("recorded_at")),
            ActorRef::new(ActorKind::System, ACTOR_ID).expect("actor"),
            TRACE_ID.parse::<TraceId>().expect("trace"),
            kind,
            RedactionClass::Project,
            payload,
        )
    }

    fn projected_state() -> AppState {
        let events = vec![
            UiEvent::Kernel(envelope(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": PROJECT_ID}),
            )),
            UiEvent::Kernel(envelope(
                2,
                EventKind::GoalCreated,
                serde_json::json!({
                    "goal_id": GOAL_ID,
                    "statement": "ship the bar",
                    "budget": {"max_turns": 8, "max_tokens": 1000}
                }),
            )),
            UiEvent::Kernel(envelope(
                3,
                EventKind::AgentSpawned,
                serde_json::json!({
                    "agent_id": AGENT_ID,
                    "role": "coder",
                    "stats": {"tokens": 12, "cost": 3}
                }),
            )),
            UiEvent::Kernel(envelope(
                4,
                EventKind::AgentSpawned,
                serde_json::json!({
                    "agent_id": CHILD_ID,
                    "parent_id": AGENT_ID,
                    "state": "running",
                    "stats": {"cost": 4}
                }),
            )),
        ];
        replay(&events, &CancellationToken::new()).expect("replay")
    }

    #[test]
    fn golden_80_120_200() {
        let snap = fixture_snapshot();
        assert_eq!(snap.render(80).golden(), GOLDEN_80);
        assert_eq!(snap.render(120).golden(), GOLDEN_120);
        assert_eq!(snap.render(200).golden(), GOLDEN_200);
        assert_eq!(snap.render(80).text().chars().count(), 80);
        assert_eq!(snap.render(120).text().chars().count(), 120);
        assert_eq!(display_width(&snap.render(200).text()), 200);
    }

    #[test]
    fn empty_state_renders_placeholders() {
        let line = render_status(&AppState::new(), 80);
        assert_eq!(line.golden(), GOLDEN_EMPTY_80);
        assert!(line.collapsed().is_empty());
        assert_eq!(line.visible().len(), 8);
    }

    #[test]
    fn collapse_is_deterministic_and_priority_ordered() {
        let snap = fixture_snapshot();
        let narrow = snap.render(40);
        assert_eq!(narrow.golden(), "claude-sonnet  sbx:ctr  pol:ask  on");
        assert_eq!(
            narrow.collapsed(),
            &[
                StatusItemKind::Cost,
                StatusItemKind::Context,
                StatusItemKind::Agents,
                StatusItemKind::GoalBudget,
            ]
        );

        let tighter = snap.render(20);
        assert_eq!(tighter.golden(), "sbx:ctr  pol:ask  on");
        assert!(tighter.collapsed().contains(&StatusItemKind::Model));

        let last = snap.render(8);
        assert_eq!(last.golden(), "on");
        assert_eq!(last.visible(), &[StatusItemKind::Connectivity]);

        let mut prev: Option<StatusLine> = None;
        for width in 0..=220 {
            let line = snap.render(width);
            let again = snap.render(width);
            assert_eq!(line, again, "width {width}");
            if let Some(prev) = prev.as_ref() {
                assert!(
                    line.collapsed().len() <= prev.collapsed().len() || width < prev.width(),
                    "collapse must not grow as width increases: {width}"
                );
            }
            prev = Some(line);
        }
    }

    #[test]
    fn from_state_reads_agents_goal_cost_and_connectivity() {
        let state = projected_state();
        let snap = StatusSnapshot::from_state(&state);
        assert_eq!(snap.agent_count(), 2);
        assert_eq!(snap.cost(), 7);
        assert_eq!(snap.turns(), Some(0));
        assert_eq!(snap.max_turns(), Some(8));
        assert_eq!(snap.max_tokens(), Some(1000));
        assert_eq!(snap.connectivity(), Connectivity::Connected);
        assert_eq!(snap.model(), None);
        assert_eq!(snap.sandbox(), SandboxMode::Unknown);
        assert_eq!(snap.policy(), PolicyMode::Unknown);
        assert!(snap.context().is_unknown());

        let chrome = StatusChrome::new()
            .with_model("opus")
            .with_sandbox(SandboxMode::Gvisor)
            .with_policy(PolicyMode::Deny)
            .with_context(ContextUsage::new(10, Some(100)));
        let with_chrome = StatusSnapshot::from_parts(&state, &chrome);
        assert_eq!(with_chrome.model(), Some("opus"));
        assert_eq!(with_chrome.sandbox(), SandboxMode::Gvisor);
        assert_eq!(with_chrome.policy(), PolicyMode::Deny);
        assert_eq!(with_chrome.context().used(), 10);
        assert_eq!(with_chrome.agent_count(), 2);
        assert_eq!(with_chrome.cost(), 7);
    }

    #[test]
    fn terminal_agents_are_excluded_from_count_but_keep_cost() {
        let state = projected_state();
        let child: protocol::AgentId = CHILD_ID.parse().expect("child");
        // Cancelled via a follow-up event so cost from both agents remains.
        let cancelled = reduce(
            state,
            &UiEvent::Kernel(envelope(
                5,
                EventKind::AgentCancelled,
                serde_json::json!({"agent_id": child.to_string()}),
            )),
        );
        let snap = StatusSnapshot::from_state(&cancelled);
        assert_eq!(snap.agent_count(), 1);
        assert_eq!(snap.cost(), 7);
    }

    #[test]
    fn protocol_error_is_readonly_and_missing_session_is_offline() {
        let empty = StatusSnapshot::from_state(&AppState::new());
        assert_eq!(empty.connectivity(), Connectivity::Disconnected);

        let blocked = reduce(
            AppState::new(),
            &UiEvent::Local(LocalUiEvent::SetComposerText(
                "x".repeat(crate::state::MAX_COMPOSER_BYTES + 1),
            )),
        );
        assert!(blocked.actions_blocked());
        assert_eq!(
            StatusSnapshot::from_state(&blocked).connectivity(),
            Connectivity::ReadOnly
        );
    }

    #[test]
    fn model_and_provider_metadata_is_display_safe() {
        let osc = StatusChrome::new().with_model("gpt-4\u{1b}]52;c;c2VjcmV0\u{07}-turbo");
        assert_eq!(osc.model(), Some("gpt-4-turbo"));

        let key = StatusChrome::new().with_model("sk-secret-abcdefghijklmnopqrstuvwxyz012345");
        assert_eq!(key.model(), Some("[redacted]"));

        let hex = StatusChrome::new()
            .with_model("aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899");
        assert_eq!(hex.model(), Some("[redacted]"));

        let provider = StatusChrome::new()
            .with_model("opus")
            .with_provider("https://evil.example/v1?key=1");
        let snap = StatusSnapshot::from_parts(&AppState::new(), &provider);
        assert_eq!(snap.model(), Some("opus"));

        let ok_provider = StatusChrome::new()
            .with_model("opus")
            .with_provider("anthropic");
        let snap = StatusSnapshot::from_parts(&AppState::new(), &ok_provider);
        assert_eq!(snap.model(), Some("opus@anthropic"));

        let multiline = StatusChrome::new().with_model("gpt\n4\tsk-abc");
        assert_eq!(multiline.model(), Some("[redacted]"));

        let line = StatusSnapshot::from_state(&AppState::new())
            .with_model("safe\u{1b}[31mname")
            .render(80);
        assert!(!line.golden().contains('\u{1b}'));
        assert!(line.golden().contains("model:safename"));
    }

    #[test]
    fn zero_width_collapses_everything() {
        let line = fixture_snapshot().render(0);
        assert!(line.golden().is_empty());
        assert!(line.text().is_empty());
        assert_eq!(line.visible().len(), 0);
        assert_eq!(line.collapsed().len(), 8);
    }
}
